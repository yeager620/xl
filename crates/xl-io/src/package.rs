//! The original xlsx package, kept so saving can patch it instead of
//! writing a new file: every part's bytes, which sheet is which part, the
//! style index each cell had, and fingerprints of what xl modeled at load
//! (to tell what changed since).

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{Cursor, Read};
use std::sync::Arc;

use quick_xml::events::Event;
use xl_engine::{Addr, CellRef, SheetId, Workbook};
use zip::ZipArchive;

use crate::xlsx_read::{attr, local, read_rels, xml};

/// What xl modeled of a sheet at load, in parts that map to separate XML
/// elements, so saving rewrites only the elements whose part changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SheetDigest {
    /// Cell inputs, formats and styles, and row heights / hidden / outline
    /// levels (all of which live in `<sheetData>`).
    pub data: u64,
    pub cols: u64,
    pub merges: u64,
    pub cond: u64,
    pub filter: u64,
    pub freeze: u64,
    pub tables: u64,
    /// Charts and sparklines apart from where they are (xl can't patch
    /// new or edited ones into an existing file yet).
    pub objects: u64,
    /// Where charts and sparklines are, and the cells they read.
    pub object_places: u64,
}

#[derive(Clone, Debug)]
pub struct PackageSheet {
    pub id: SheetId,
    pub name: String,
    /// Zip path of the worksheet part ("xl/worksheets/sheet1.xml").
    pub part: String,
    pub digest: SheetDigest,
    pub structure: u64,
}

/// An opened xlsx file, for saving back into.
#[derive(Clone)]
pub struct Package {
    pub(crate) bytes: Arc<Vec<u8>>,
    pub(crate) sheets: Vec<PackageSheet>,
    /// Style index (`s`) of every styled cell, per sheet.
    pub(crate) cell_xfs: HashMap<SheetId, HashMap<CellRef, u32>>,
    pub(crate) names: u64,
    /// Structural edits the workbook had when this package was read.
    pub(crate) log_len: usize,
}

impl std::fmt::Debug for Package {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Package").field("bytes", &self.bytes.len()).field("sheets", &self.sheets).finish()
    }
}

fn hash_of(parts: &[&dyn std::fmt::Debug]) -> u64 {
    let mut h = DefaultHasher::new();
    for p in parts {
        format!("{p:?}").hash(&mut h);
    }
    h.finish()
}

pub(crate) fn sheet_digest(wb: &Workbook, id: SheetId) -> SheetDigest {
    let l = wb.layout(id);
    let mut h = DefaultHasher::new();
    for (c, _) in wb.store(id).iter() {
        (c, wb.input_text(Addr::new(id, c))).hash(&mut h);
    }
    let mut formatted: Vec<(CellRef, String)> = wb.formatted_cells(id).map(|(c, f)| (c, f.to_string())).collect();
    formatted.sort();
    formatted.hash(&mut h);
    let mut styled: Vec<(CellRef, String)> = wb.styled_cells(id).map(|(c, s)| (c, format!("{s:?}"))).collect();
    styled.sort();
    styled.hash(&mut h);
    format!("{:?}{:?}{:?}{:?}", l.row_heights, l.hidden_rows, l.filtered_rows, l.row_levels).hash(&mut h);
    SheetDigest {
        data: h.finish(),
        cols: hash_of(&[&l.col_widths, &l.hidden_cols, &l.col_levels]),
        merges: hash_of(&[&l.merges]),
        cond: hash_of(&[&l.cond]),
        filter: hash_of(&[&l.filter]),
        freeze: hash_of(&[&l.freeze]),
        tables: hash_of(&[&l.tables]),
        objects: {
            let charts: Vec<_> = l.charts.iter().map(|c| (c.kind, c.title.clone(), c.stacked, c.series.len())).collect();
            let sparks: Vec<_> = l.sparklines.iter().map(|s| (s.kind, s.color)).collect();
            hash_of(&[&charts, &sparks])
        },
        object_places: hash_of(&[&l.charts, &l.sparklines]),
    }
}

pub(crate) fn names_digest(wb: &Workbook) -> u64 {
    let names: Vec<(String, Option<SheetId>, String)> = wb.names().iter().map(|d| (d.name.clone(), d.scope, wb.name_formula(d))).collect();
    hash_of(&[&names])
}

/// Every `<c r=".." s="..">` style index in a worksheet part.
fn scan_cell_xfs(bytes: &[u8]) -> HashMap<CellRef, u32> {
    let mut out = HashMap::new();
    let mut r = xml(bytes);
    let mut buf = Vec::new();
    while let Ok(ev) = r.read_event_into(&mut buf) {
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) if local(e.name().as_ref()) == "c" => {
                if let (Some(cell), Some(s)) = (attr(e, "r").and_then(|r| CellRef::parse(&r)), attr(e, "s").and_then(|s| s.parse::<u32>().ok())) {
                    out.insert(cell, s);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

impl Package {
    /// Index an xlsx file's bytes against the workbook loaded from it (sheets
    /// in the same order).
    pub(crate) fn new(bytes: Vec<u8>, wb: &Workbook) -> Result<Package, String> {
        let bytes = Arc::new(bytes);
        let mut zip = ZipArchive::new(Cursor::new(bytes.as_ref().clone())).map_err(|e| e.to_string())?;
        let rels = read_rels(&mut zip, "xl/_rels/workbook.xml.rels")?;
        let mut workbook_xml = Vec::new();
        zip.by_name("xl/workbook.xml").map_err(|e| e.to_string())?.read_to_end(&mut workbook_xml).map_err(|e| e.to_string())?;
        // (name, relationship id) per sheet, in order.
        let mut listed: Vec<(String, String)> = Vec::new();
        let mut r = xml(&workbook_xml);
        let mut buf = Vec::new();
        loop {
            match r.read_event_into(&mut buf).map_err(|e| e.to_string())? {
                Event::Start(ref e) | Event::Empty(ref e) if local(e.name().as_ref()) == "sheet" => {
                    if let (Some(n), Some(id)) = (attr(e, "name"), attr(e, "id")) {
                        listed.push((n, id));
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        let mut sheets = Vec::new();
        let mut cell_xfs = HashMap::new();
        for ((name, rid), &id) in listed.iter().zip(wb.sheet_ids()) {
            let Some(target) = rels.get(rid) else { continue };
            let part = crate::xlsx_read::sheet_path(target);
            let mut data = Vec::new();
            if let Ok(mut f) = zip.by_name(&part) {
                f.read_to_end(&mut data).map_err(|e| e.to_string())?;
            } else {
                continue;
            }
            cell_xfs.insert(id, scan_cell_xfs(&data));
            sheets.push(PackageSheet { id, name: name.clone(), part, digest: sheet_digest(wb, id), structure: wb.structure_version(id) });
        }
        Ok(Package { bytes, sheets, cell_xfs, names: names_digest(wb), log_len: wb.structure_log().len() })
    }

    /// Zip paths of the worksheet parts, in sheet order.
    pub fn sheets_parts(&self) -> Vec<String> {
        self.sheets.iter().map(|s| s.part.clone()).collect()
    }

    pub(crate) fn zip(&self) -> Result<ZipArchive<Cursor<Vec<u8>>>, String> {
        ZipArchive::new(Cursor::new(self.bytes.as_ref().clone())).map_err(|e| e.to_string())
    }

    pub(crate) fn part(&self, name: &str) -> Option<Vec<u8>> {
        let mut zip = self.zip().ok()?;
        let mut f = zip.by_name(name).ok()?;
        let mut out = Vec::new();
        f.read_to_end(&mut out).ok()?;
        Some(out)
    }

}

/// How saving to the opened file will go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SavePlan {
    /// Patch the original: everything xl doesn't model is kept.
    Patch,
    /// Write a new file (parts xl doesn't model are lost), and why.
    Rewrite(Vec<String>),
}

pub fn save_plan(wb: &Workbook, pkg: &Package) -> SavePlan {
    let mut why = Vec::new();
    let structural = wb.structure_log().len() > pkg.log_len;
    for s in &pkg.sheets {
        if !wb.sheet_ids().contains(&s.id) {
            why.push(format!("the sheet “{}” was deleted", s.name));
            continue;
        }
        let now = sheet_digest(wb, s.id);
        if now.objects != s.digest.objects {
            why.push(format!("charts or sparklines on “{}” were added, removed or edited", wb.sheet_name(s.id)));
        }
        // Positions follow inserted/deleted lines (rewritten in the drawing);
        // a move by hand isn't patched yet.
        if now.object_places != s.digest.object_places && !structural {
            why.push(format!("a chart on “{}” was moved or resized", wb.sheet_name(s.id)));
        }
        if now.tables != s.digest.tables && !tables_patchable(wb, pkg, s) {
            why.push(format!("tables on “{}” were added, removed or changed columns", wb.sheet_name(s.id)));
        }
    }
    // Addresses in preserved parts after inserted/deleted lines and renames.
    if let Err(e) = crate::xlsx_refs::rewrite_preserved(wb, pkg) {
        why.push(e);
    }
    if why.is_empty() { SavePlan::Patch } else { SavePlan::Rewrite(why) }
}

/// Did the sheet's tables only change their row extent (same names, same
/// first row, same columns)? Those are patched in their table parts.
pub(crate) fn tables_patchable(wb: &Workbook, pkg: &Package, s: &PackageSheet) -> bool {
    let now = &wb.layout(s.id).tables;
    let Some(before) = original_tables(pkg, s) else { return false };
    // Same tables and the same number of columns: only the range moved or grew.
    now.len() == before.len() && now.iter().all(|t| before.iter().any(|(name, start_col, end_col)| name.eq_ignore_ascii_case(&t.name) && end_col - start_col == t.range.end.col - t.range.start.col))
}

/// (name, first column, last column) of the tables in the original sheet.
pub(crate) fn original_tables(pkg: &Package, s: &PackageSheet) -> Option<Vec<(String, u32, u32)>> {
    let mut zip = pkg.zip().ok()?;
    let rels = read_rels(&mut zip, &crate::xlsx_charts::rels_of(&s.part)).ok()?;
    let mut out = Vec::new();
    for target in rels.values().filter(|t| t.contains("tables/")) {
        let part = crate::xlsx_charts::resolve(&s.part, target);
        let bytes = pkg.part(&part)?;
        let mut r = xml(&bytes);
        let mut buf = Vec::new();
        loop {
            match r.read_event_into(&mut buf).ok()? {
                Event::Start(ref e) | Event::Empty(ref e) if local(e.name().as_ref()) == "table" => {
                    let name = attr(e, "displayName").or_else(|| attr(e, "name"))?;
                    let reference = attr(e, "ref")?;
                    let (a, b) = reference.split_once(':').unwrap_or((&reference, &reference));
                    out.push((name, CellRef::parse(a)?.col, CellRef::parse(b)?.col));
                    break;
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
    }
    Some(out)
}
