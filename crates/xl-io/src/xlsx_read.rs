//! Streaming xlsx reader: workbook structure, shared strings, number formats,
//! column widths, values, formulas (including shared formulas) and cached
//! results.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;
use std::sync::Arc;

use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesRef, BytesStart, Event};
use xl_engine::format::builtin_format;
use xl_engine::formula::parse_formula;
use xl_engine::input::classify_literal;
use xl_engine::{Addr, CellRef, ErrorKind, LoadedCell, MAX_COLS, SheetId, Value, Workbook, validate_sheet_name};
use zip::ZipArchive;

use crate::{Loaded, from_excel_width};

type Zip = ZipArchive<BufReader<File>>;

fn local(name: &str) -> &str {
    match name.rfind(':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

fn attr(e: &BytesStart, key: &str) -> Option<String> {
    e.attributes().with_checks(false).flatten().find(|a| local(a.key.as_ref()) == key).and_then(|a| a.normalized_value(XmlVersion::Implicit1_0).ok().map(|v| v.into_owned()))
}

fn push_entity(out: &mut String, r: &BytesRef) {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        out.push(c);
        return;
    }
    out.push_str(match r.xml_content(XmlVersion::Implicit1_0).as_ref() {
        "lt" => "<",
        "gt" => ">",
        "amp" => "&",
        "quot" => "\"",
        "apos" => "'",
        _ => "",
    });
}

fn read_entry<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Result<Option<Vec<u8>>, String> {
    let mut f = match zip.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(format!("{name}: {e}")),
    };
    let mut buf = Vec::with_capacity(f.size() as usize);
    f.read_to_end(&mut buf).map_err(|e| format!("{name}: {e}"))?;
    Ok(Some(buf))
}

fn xml(bytes: &[u8]) -> Reader<&[u8]> {
    Reader::from_reader(bytes)
}

fn read_rels(zip: &mut Zip, name: &str) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    let Some(bytes) = read_entry(zip, name)? else { return Ok(out) };
    let mut r = xml(&bytes);
    loop {
        match r.read_event().map_err(|e| e.to_string())? {
            Event::Start(e) | Event::Empty(e) if local(e.name().as_ref()) == "Relationship" => {
                if let (Some(id), Some(target)) = (attr(&e, "Id"), attr(&e, "Target")) {
                    out.insert(id, target);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn read_sheet_list(zip: &mut Zip) -> Result<Vec<(String, String)>, String> {
    let bytes = read_entry(zip, "xl/workbook.xml")?.ok_or("not an xlsx file (missing xl/workbook.xml)")?;
    let mut r = xml(&bytes);
    let mut out = Vec::new();
    loop {
        match r.read_event().map_err(|e| e.to_string())? {
            Event::Start(e) | Event::Empty(e) if local(e.name().as_ref()) == "sheet" => {
                if let (Some(name), Some(rid)) = (attr(&e, "name"), attr(&e, "id")) {
                    out.push((name, rid));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn read_shared_strings(zip: &mut Zip) -> Result<Vec<Arc<str>>, String> {
    let mut out = Vec::new();
    let Some(bytes) = read_entry(zip, "xl/sharedStrings.xml")? else { return Ok(out) };
    let mut r = xml(&bytes);
    let mut cur = String::new();
    let (mut in_t, mut in_phonetic) = (false, false);
    loop {
        match r.read_event().map_err(|e| e.to_string())? {
            Event::Start(e) => match local(e.name().as_ref()) {
                "si" => cur.clear(),
                "t" => in_t = true,
                "rPh" => in_phonetic = true,
                _ => {}
            },
            Event::End(e) => match local(e.name().as_ref()) {
                "si" => out.push(Arc::from(cur.as_str())),
                "t" => in_t = false,
                "rPh" => in_phonetic = false,
                _ => {}
            },
            Event::Text(t) if in_t && !in_phonetic => cur.push_str(&t.xml_content(XmlVersion::Implicit1_0)),
            Event::GeneralRef(g) if in_t && !in_phonetic => push_entity(&mut cur, &g),
            Event::CData(c) if in_t && !in_phonetic => cur.push_str(&c.into_inner()),
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// Number format code for each cell style index (None = General).
fn read_styles(zip: &mut Zip) -> Result<Vec<Option<String>>, String> {
    let mut out = Vec::new();
    let Some(bytes) = read_entry(zip, "xl/styles.xml")? else { return Ok(out) };
    let mut r = xml(&bytes);
    let mut custom: HashMap<u32, String> = HashMap::new();
    let mut in_cell_xfs = false;
    loop {
        match r.read_event().map_err(|e| e.to_string())? {
            Event::Start(e) | Event::Empty(e) => match local(e.name().as_ref()) {
                "numFmt" => {
                    if let (Some(id), Some(code)) = (attr(&e, "numFmtId"), attr(&e, "formatCode")) {
                        custom.insert(id.parse().unwrap_or(0), code);
                    }
                }
                "cellXfs" => in_cell_xfs = true,
                "xf" if in_cell_xfs => {
                    let id: u32 = attr(&e, "numFmtId").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let code = custom.get(&id).cloned().or_else(|| builtin_format(id).map(str::to_string));
                    out.push(code.filter(|c| !c.eq_ignore_ascii_case("General")));
                }
                _ => {}
            },
            Event::End(e) if local(e.name().as_ref()) == "cellXfs" => in_cell_xfs = false,
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

#[derive(Default, PartialEq)]
enum Capture {
    #[default]
    None,
    Value,
    Formula,
    Inline,
}

struct SharedFormula {
    origin: CellRef,
    expr: Option<xl_engine::formula::Expr>,
}

struct SheetCtx<'a> {
    id: SheetId,
    shared: &'a [Arc<str>],
    styles: &'a [Option<String>],
}

fn read_sheet(wb: &mut Workbook, bytes: &[u8], cx: &SheetCtx, cells: &mut usize, warnings: &mut Vec<String>) -> Result<(), String> {
    let mut r = xml(bytes);
    let mut buf = Vec::new();
    let mut row: u32 = 0;
    let mut col: u32 = 0;
    // Current cell state.
    let mut cell = CellRef::new(0, 0);
    let mut ty = String::new();
    let mut style: Option<usize> = None;
    let mut v = String::new();
    let mut f = String::new();
    let mut f_shared: Option<(u32, bool)> = None; // (si, is master with text)
    let mut cap = Capture::None;
    let mut shared_formulas: HashMap<u32, SharedFormula> = HashMap::new();
    let mut unresolved_shared = 0usize;

    loop {
        let ev = r.read_event_into(&mut buf).map_err(|e| e.to_string())?;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(ev, Event::Empty(_));
                match local(e.name().as_ref()) {
                    "row" => {
                        if let Some(n) = attr(e, "r").and_then(|s| s.parse::<u32>().ok()) {
                            row = n.saturating_sub(1);
                        }
                        col = 0;
                    }
                    "c" => {
                        cell = attr(e, "r").and_then(|s| CellRef::parse(&s)).unwrap_or(CellRef::new(row, col));
                        row = cell.row;
                        col = cell.col + 1;
                        ty = attr(e, "t").unwrap_or_default();
                        style = attr(e, "s").and_then(|s| s.parse().ok());
                        v.clear();
                        f.clear();
                        f_shared = None;
                        if empty {
                            // Formatted but empty cell.
                            if let Some(code) = style.and_then(|s| cx.styles.get(s)).and_then(|c| c.as_deref()) {
                                wb.set_format(Addr::new(cx.id, cell), Some(code));
                            }
                        }
                    }
                    "v" if !empty => cap = Capture::Value,
                    "f" => {
                        if attr(e, "t").as_deref() == Some("shared")
                            && let Some(si) = attr(e, "si").and_then(|s| s.parse().ok())
                        {
                            f_shared = Some((si, !empty));
                        }
                        if !empty {
                            cap = Capture::Formula;
                        }
                    }
                    "t" if !empty && ty == "inlineStr" => cap = Capture::Inline,
                    "col" => {
                        let min: u32 = attr(e, "min").and_then(|s| s.parse().ok()).unwrap_or(1);
                        let max: u32 = attr(e, "max").and_then(|s| s.parse().ok()).unwrap_or(min);
                        let custom = attr(e, "customWidth").is_some_and(|s| s == "1" || s == "true");
                        if let Some(w) = attr(e, "width").and_then(|s| s.parse::<f64>().ok())
                            && custom
                            && max >= min
                            && max - min < 1000
                        {
                            for c in min..=max.min(MAX_COLS) {
                                wb.set_col_width(cx.id, c - 1, Some(from_excel_width(w)));
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(ref t) => match cap {
                Capture::Value => v.push_str(&t.xml_content(XmlVersion::Implicit1_0)),
                Capture::Formula => f.push_str(&t.xml_content(XmlVersion::Implicit1_0)),
                Capture::Inline => v.push_str(&t.xml_content(XmlVersion::Implicit1_0)),
                Capture::None => {}
            },
            Event::GeneralRef(ref g) => match cap {
                Capture::Value | Capture::Inline => push_entity(&mut v, g),
                Capture::Formula => push_entity(&mut f, g),
                Capture::None => {}
            },
            Event::End(ref e) => match local(e.name().as_ref()) {
                "v" | "f" | "t" => cap = Capture::None,
                "c" => {
                    let addr = Addr::new(cx.id, cell);
                    let value = match ty.as_str() {
                        "s" => v.trim().parse::<usize>().ok().and_then(|i| cx.shared.get(i)).map_or(Value::Empty, |s| Value::Text(s.clone())),
                        "str" | "inlineStr" => Value::text(v.as_str()),
                        "b" => Value::Bool(v.trim() == "1"),
                        "e" => Value::Error(ErrorKind::parse(v.trim()).unwrap_or(ErrorKind::NA)),
                        "d" => classify_literal(&v.replace('T', " ")).map_or(Value::text(v.as_str()), |l| l.value),
                        _ if v.trim().is_empty() => Value::Empty,
                        _ => v.trim().parse::<f64>().map_or(Value::text(v.as_str()), Value::Number),
                    };
                    // Resolve shared formulas: the master carries the text,
                    // the rest are offsets from it.
                    let mut formula = (!f.is_empty()).then(|| f.clone());
                    if let Some((si, is_master)) = f_shared {
                        if is_master && !f.is_empty() {
                            let expr = parse_formula(&f, wb.sheets()).ok();
                            shared_formulas.insert(si, SharedFormula { origin: cell, expr });
                        } else if let Some(sf) = shared_formulas.get(&si) {
                            formula = match &sf.expr {
                                Some(expr) => Some(expr.to_source(cell.row as i64 - sf.origin.row as i64, cell.col as i64 - sf.origin.col as i64, wb.sheets())),
                                None => {
                                    unresolved_shared += 1;
                                    None
                                }
                            };
                        }
                    }
                    if let Some(code) = style.and_then(|s| cx.styles.get(s)).and_then(|c| c.as_deref()) {
                        wb.set_format(addr, Some(code));
                    }
                    match formula {
                        Some(src) => {
                            wb.load_cell(addr, LoadedCell::Formula { src, cached: value });
                            *cells += 1;
                        }
                        None if !value.is_empty() => {
                            wb.load_cell(addr, LoadedCell::Value(value));
                            *cells += 1;
                        }
                        None => {}
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if unresolved_shared > 0 {
        warnings.push(format!("{}: {unresolved_shared} shared formulas couldn't be expanded and were kept as values", wb.sheet_name(cx.id)));
    }
    Ok(())
}

fn sheet_path(target: &str) -> String {
    match target.strip_prefix('/') {
        Some(abs) => abs.to_string(),
        None => format!("xl/{}", target.trim_start_matches("./")),
    }
}

pub fn read_xlsx(path: &Path) -> Result<Loaded, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut zip = ZipArchive::new(BufReader::new(file)).map_err(|e| format!("not an xlsx file: {e}"))?;
    let rels = read_rels(&mut zip, "xl/_rels/workbook.xml.rels")?;
    let list = read_sheet_list(&mut zip)?;
    let shared = read_shared_strings(&mut zip)?;
    let styles = read_styles(&mut zip)?;

    let mut wb = Workbook::empty();
    let mut warnings = Vec::new();
    // Create every sheet first so cross-sheet references resolve.
    let mut ids = Vec::new();
    for (i, (name, _)) in list.iter().enumerate() {
        let id = match validate_sheet_name(name).ok().and_then(|_| wb.add_sheet(Some(name)).ok()) {
            Some(id) => id,
            None => {
                warnings.push(format!("Sheet '{name}' was renamed (invalid or duplicate name)"));
                wb.add_sheet(Some(&format!("Sheet{}", i + 1))).or_else(|_| wb.add_sheet(None))?
            }
        };
        ids.push(id);
    }
    if ids.is_empty() {
        wb.add_sheet(None)?;
    }
    let mut cells = 0;
    for ((name, rid), id) in list.iter().zip(&ids) {
        let Some(target) = rels.get(rid) else {
            warnings.push(format!("Sheet '{name}' has no data part"));
            continue;
        };
        let part = sheet_path(target);
        let Some(bytes) = read_entry(&mut zip, &part)? else {
            // Chart sheets and dialog sheets have other part types.
            warnings.push(format!("Sheet '{name}' isn't a worksheet and was left empty"));
            continue;
        };
        read_sheet(&mut wb, &bytes, &SheetCtx { id: *id, shared: &shared, styles: &styles }, &mut cells, &mut warnings)?;
    }
    let stats = wb.recalc_all();
    Ok(Loaded { workbook: wb, cells, warnings, recalc: stats })
}
