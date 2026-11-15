//! The original xlsx package, kept so saving can patch it instead of
//! writing a new file: every part's bytes, which sheet is which part, the
//! style index each cell had, and fingerprints of what xl modeled at load
//! (to tell what changed since).

use xl_engine::chart::Chart;
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
    pub notes: u64,
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
    /// Extra cell/formula attributes, with the cell's input when read.
    pub(crate) cell_extras: HashMap<SheetId, HashMap<CellRef, (CellExtra, String)>>,
    pub(crate) names: u64,
    /// Structural edits the workbook had when this package was read.
    pub(crate) log_len: usize,
    /// Each sheet's notes and hyperlinks when read.
    pub(crate) notes: HashMap<SheetId, std::collections::BTreeMap<CellRef, xl_engine::Note>>,
    pub(crate) links: HashMap<SheetId, std::collections::BTreeMap<CellRef, xl_engine::Link>>,
    pub(crate) validations: HashMap<SheetId, Vec<xl_engine::validation::Validation>>,
    pub(crate) sparklines: HashMap<SheetId, Vec<xl_engine::chart::Sparkline>>,
    /// Per sheet, each chart as read with its drawing part and anchor index
    /// (only when they line up one-to-one with the workbook's charts).
    pub(crate) chart_places: HashMap<SheetId, Vec<crate::xlsx_charts::FoundChart>>,
}

/// A per-cell map after the structural edits since the package was read.
fn shifted<T: Clone>(before: &std::collections::BTreeMap<CellRef, T>, wb: &Workbook, pkg: &Package, sheet: SheetId) -> std::collections::BTreeMap<CellRef, T> {
    let mut out = before.clone();
    for op in wb.structure_log().iter().skip(pkg.log_len) {
        if let xl_engine::StructOp::Shift(s) = op
            && s.sheet() == sheet
        {
            let rows = matches!(s, xl_engine::Shift::InsertRows { .. } | xl_engine::Shift::DeleteRows { .. });
            out = out.into_iter().filter_map(|(c, n)| Some((if rows { CellRef::new(s.map_pos(c.row)?, c.col) } else { CellRef::new(c.row, s.map_pos(c.col)?) }, n))).collect();
        }
    }
    out
}

impl Package {
    /// Did a sheet's notes change other than by moving with inserted or
    /// deleted lines (which rewriting the comments part handles)?
    pub(crate) fn notes_changed(&self, wb: &Workbook, sheet: SheetId) -> bool {
        let Some(before) = self.notes.get(&sheet) else { return !wb.layout(sheet).notes.is_empty() };
        // Notes deleted with their rows need their note boxes rebuilt too.
        let expected = shifted(before, wb, self, sheet);
        expected.len() != before.len() || expected != wb.layout(sheet).notes
    }

    /// How a sheet's charts map onto the file's: charts read from the file
    /// and still there unedited are kept (re-anchored if moved by hand), the
    /// others' anchors are removed, and charts made or edited in xl added.
    /// None if the sheet's drawings couldn't be read.
    pub(crate) fn chart_plan(&self, wb: &Workbook, sheet: SheetId) -> Option<ChartPlan> {
        let now = &wb.layout(sheet).charts;
        let places = self.chart_places.get(&sheet)?;
        let same = |a: &Chart, b: &Chart| (a.kind, &a.title, &a.series, a.stacked) == (b.kind, &b.title, &b.series, b.stacked);
        let expected: Vec<Chart> = places
            .iter()
            .map(|(orig, _, _)| {
                let mut c = orig.clone();
                for op in wb.structure_log().iter().skip(self.log_len) {
                    match op {
                        xl_engine::StructOp::Shift(s) => c.apply_shift(*s, sheet),
                        xl_engine::StructOp::Move(m) => c.apply_move(m, sheet),
                    }
                }
                c
            })
            .collect();
        let mut used = vec![false; places.len()];
        let mut plan = ChartPlan::default();
        for (i, current) in now.iter().enumerate() {
            match (0..places.len()).find(|&j| !used[j] && same(&expected[j], current)) {
                Some(j) => {
                    used[j] = true;
                    let e = &expected[j];
                    if (e.anchor, e.offset, e.size) != (current.anchor, current.offset, current.size) {
                        plan.moved.push((i, places[j].1.clone(), places[j].2));
                    }
                }
                None => plan.added.push(i),
            }
        }
        plan.removed = places.iter().zip(&used).filter(|(_, u)| !**u).map(|((_, d, a), _)| (d.clone(), *a)).collect();
        Some(plan)
    }

    /// Did a sheet's sparklines change other than by moving with lines?
    pub(crate) fn sparklines_changed(&self, wb: &Workbook, sheet: SheetId) -> bool {
        let mut expected = self.sparklines.get(&sheet).cloned().unwrap_or_default();
        for op in wb.structure_log().iter().skip(self.log_len) {
            if let xl_engine::StructOp::Shift(s) = op {
                expected.retain_mut(|sp| sp.apply_shift(*s, sheet));
            }
        }
        expected != wb.layout(sheet).sparklines
    }

    /// Did a sheet's validation rules change other than by moving with lines?
    pub(crate) fn validations_changed(&self, wb: &Workbook, sheet: SheetId) -> bool {
        let mut expected = self.validations.get(&sheet).cloned().unwrap_or_default();
        for op in wb.structure_log().iter().skip(self.log_len) {
            if let xl_engine::StructOp::Shift(s) = op {
                expected = expected.iter().filter_map(|v| v.shifted(*s, sheet)).collect();
            }
        }
        expected != wb.layout(sheet).validations
    }

    /// Did a sheet's hyperlinks change other than by moving with lines?
    pub(crate) fn links_changed(&self, wb: &Workbook, sheet: SheetId) -> bool {
        let Some(before) = self.links.get(&sheet) else { return !wb.layout(sheet).links.is_empty() };
        shifted(before, wb, self, sheet) != wb.layout(sheet).links
    }
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
            hash_of(&[&charts])
        },
        object_places: hash_of(&[&l.charts]),
        notes: hash_of(&[&l.notes]),
    }
}

pub(crate) fn names_digest(wb: &Workbook) -> u64 {
    let names: Vec<(String, Option<SheetId>, String)> = wb.names().iter().map(|d| (d.name.clone(), d.scope, wb.name_formula(d))).collect();
    hash_of(&[&names])
}

/// See [`Package::chart_plan`].
#[derive(Debug, Default)]
pub(crate) struct ChartPlan {
    /// (chart index, drawing part, anchor index) of kept charts moved by hand.
    pub moved: Vec<(usize, String, usize)>,
    /// (drawing part, anchor index) of charts no longer there (or edited).
    pub removed: Vec<(String, usize)>,
    /// Indexes of charts to write as new.
    pub added: Vec<usize>,
}

/// What a cell's XML carried beyond xl's model, put back when the cell is
/// rewritten unchanged: `<c>` attributes like `cm`/`vm` (dynamic arrays,
/// rich values) and `<f>` attributes of array and data-table formulas.
#[derive(Clone, Debug, Default)]
pub(crate) struct CellExtra {
    pub c_attrs: Vec<(String, String)>,
    pub f_attrs: Vec<(String, String)>,
}

fn all_attrs(e: &quick_xml::events::BytesStart) -> Vec<(String, String)> {
    e.attributes()
        .with_checks(false)
        .flatten()
        .map(|a| (a.key.as_ref().to_string(), a.normalized_value(quick_xml::XmlVersion::Implicit1_0).map(|v| v.into_owned()).unwrap_or_default()))
        .collect()
}

/// Per-cell style index and extra attributes of a worksheet part.
type CellScan = (HashMap<CellRef, u32>, HashMap<CellRef, CellExtra>);

/// Every cell's style index (`s`) and extra attributes in a worksheet part.
fn scan_cells(bytes: &[u8]) -> CellScan {
    let (mut xfs, mut extras) = (HashMap::new(), HashMap::<CellRef, CellExtra>::new());
    let mut r = xml(bytes);
    let mut buf = Vec::new();
    let mut cur: Option<CellRef> = None;
    while let Ok(ev) = r.read_event_into(&mut buf) {
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => match local(e.name().as_ref()) {
                "c" => {
                    cur = attr(e, "r").and_then(|r| CellRef::parse(&r));
                    if let Some(cell) = cur {
                        if let Some(s) = attr(e, "s").and_then(|s| s.parse::<u32>().ok()) {
                            xfs.insert(cell, s);
                        }
                        let extra: Vec<_> = all_attrs(e).into_iter().filter(|(k, _)| !matches!(k.as_str(), "r" | "s" | "t")).collect();
                        if !extra.is_empty() {
                            extras.entry(cell).or_default().c_attrs = extra;
                        }
                    }
                }
                "f" => {
                    // Shared formulas are written out in full; arrays and data tables keep their kind.
                    let fa = all_attrs(e);
                    if let Some(cell) = cur
                        && fa.iter().any(|(k, v)| k == "t" && (v == "array" || v == "dataTable"))
                    {
                        extras.entry(cell).or_default().f_attrs = fa;
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    (xfs, extras)
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
        let mut cell_extras = HashMap::new();
        for ((name, rid), &id) in listed.iter().zip(wb.sheet_ids()) {
            let Some(target) = rels.get(rid) else { continue };
            let part = crate::xlsx_read::sheet_path(target);
            let mut data = Vec::new();
            if let Ok(mut f) = zip.by_name(&part) {
                f.read_to_end(&mut data).map_err(|e| e.to_string())?;
            } else {
                continue;
            }
            let (xfs, extras) = scan_cells(&data);
            cell_xfs.insert(id, xfs);
            // Remember each extra's input, so it's only reapplied to the same formula.
            let with_input: HashMap<CellRef, (CellExtra, String)> = extras.into_iter().map(|(c, e)| (c, (e, wb.input_text(Addr::new(id, c))))).collect();
            cell_extras.insert(id, with_input);
            sheets.push(PackageSheet { id, name: name.clone(), part, digest: sheet_digest(wb, id), structure: wb.structure_version(id) });
        }
        let mut chart_places = HashMap::new();
        for s in &sheets {
            if let Ok((found, _)) = crate::xlsx_charts::scan_charts(&mut zip, &s.part, wb, s.id)
                && found.len() == wb.layout(s.id).charts.len()
            {
                chart_places.insert(s.id, found);
            }
        }
        Ok(Package { bytes, sheets, cell_xfs, cell_extras, chart_places, names: names_digest(wb), log_len: wb.structure_log().len(), notes: wb.sheet_ids().iter().map(|&id| (id, wb.layout(id).notes.clone())).collect(), links: wb.sheet_ids().iter().map(|&id| (id, wb.layout(id).links.clone())).collect(), validations: wb.sheet_ids().iter().map(|&id| (id, wb.layout(id).validations.clone())).collect(), sparklines: wb.sheet_ids().iter().map(|&id| (id, wb.layout(id).sparklines.clone())).collect() })
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
        for s in &pkg.sheets {
        if !wb.sheet_ids().contains(&s.id) {
            why.push(format!("the sheet “{}” was deleted", s.name));
            continue;
        }
        let now = sheet_digest(wb, s.id);
        // Chart changes need the charts matched to the file's anchors.
        if (now.objects != s.digest.objects || now.object_places != s.digest.object_places) && pkg.chart_plan(wb, s.id).is_none() {
            why.push(format!("charts on “{}” changed, and its drawings couldn't be read", wb.sheet_name(s.id)));
        }
        if pkg.validations_changed(wb, s.id)
            && let Some(xml) = pkg.part(&s.part)
        {
            // Rules xl couldn't read would be lost when the list is rewritten.
            let in_file = String::from_utf8_lossy(&xml).matches("<dataValidation ").count();
            if in_file > pkg.validations.get(&s.id).map_or(0, Vec::len) {
                why.push(format!("validation on “{}” changed, and it has rules xl can't edit", wb.sheet_name(s.id)));
            }
        }
        if pkg.notes_changed(wb, s.id)
            && let Some(problem) = notes_unpatchable(pkg, s)
        {
            why.push(format!("notes on “{}” changed, and {problem}", wb.sheet_name(s.id)));
        }
    }
    // Addresses in preserved parts after inserted/deleted lines and renames.
    if let Err(e) = crate::xlsx_refs::rewrite_preserved(wb, pkg) {
        why.push(e);
    }
    if why.is_empty() { SavePlan::Patch } else { SavePlan::Rewrite(why) }
}

/// Why a sheet's notes can't be rewritten in place, if they can't: modern
/// threaded comments would fall out of step, and other shapes in the note
/// drawing (form controls) would be lost.
fn notes_unpatchable(pkg: &Package, s: &PackageSheet) -> Option<&'static str> {
    let mut zip = pkg.zip().ok()?;
    let rels = read_rels(&mut zip, &crate::xlsx_charts::rels_of(&s.part)).ok()?;
    if rels.values().any(|t| t.contains("threadedComment")) {
        return Some("the sheet also has threaded comments");
    }
    for t in rels.values().filter(|t| t.ends_with(".vml")) {
        let vml = pkg.part(&crate::xlsx_charts::resolve(&s.part, t))?;
        let text = String::from_utf8_lossy(&vml);
        if text.split("ObjectType=\"").skip(1).any(|rest| !rest.starts_with("Note\"")) {
            return Some("its note drawing also holds form controls");
        }
    }
    None
}

