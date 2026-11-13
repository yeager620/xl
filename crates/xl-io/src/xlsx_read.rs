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
use xl_engine::style::{INDEXED_COLORS, apply_tint};
use xl_engine::{Addr, Border, CellRef, ErrorKind, HAlign, LineStyle, LoadedCell, MAX_COLS, Rgb, SheetId, Style, Value, Workbook, validate_sheet_name};
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

/// Theme colors in Excel's theme-index order (lt1, dk1, lt2, dk2, accent1..6, hlink, folHlink).
fn read_theme(zip: &mut Zip) -> Result<Vec<Rgb>, String> {
    // Office defaults, used when the file has no theme.
    let mut scheme: Vec<Rgb> = vec![0x000000, 0xFFFFFF, 0x44546A, 0xE7E6E6, 0x4472C4, 0xED7D31, 0xA5A5A5, 0xFFC000, 0x5B9BD5, 0x70AD47, 0x0563C1, 0x954F72];
    if let Some(bytes) = read_entry(zip, "xl/theme/theme1.xml")? {
        let names = ["dk1", "lt1", "dk2", "lt2", "accent1", "accent2", "accent3", "accent4", "accent5", "accent6", "hlink", "folHlink"];
        let mut r = xml(&bytes);
        let mut slot: Option<usize> = None;
        loop {
            match r.read_event().map_err(|e| e.to_string())? {
                Event::Start(e) | Event::Empty(e) => {
                    let n = local(e.name().as_ref()).to_string();
                    if let Some(i) = names.iter().position(|x| *x == n) {
                        slot = Some(i);
                    } else if let Some(i) = slot {
                        let v = match n.as_str() {
                            "srgbClr" => attr(&e, "val"),
                            "sysClr" => attr(&e, "lastClr"),
                            _ => None,
                        };
                        if let Some(rgb) = v.and_then(|v| u32::from_str_radix(&v, 16).ok()) {
                            scheme[i] = rgb;
                            slot = None;
                        }
                    }
                }
                Event::End(e) if local(e.name().as_ref()) == "clrScheme" => break,
                Event::Eof => break,
                _ => {}
            }
        }
    }
    // dk1/lt1 and dk2/lt2 swap places in theme-index order.
    scheme.swap(0, 1);
    scheme.swap(2, 3);
    Ok(scheme)
}

/// Resolve a <color>/<fgColor> element. Returns None for automatic.
fn color_of(e: &BytesStart, theme: &[Rgb]) -> Option<Rgb> {
    if attr(e, "auto").is_some_and(|v| v == "1" || v == "true") {
        return None;
    }
    let base = if let Some(rgb) = attr(e, "rgb") {
        let hex = if rgb.len() == 8 { &rgb[2..] } else { rgb.as_str() };
        u32::from_str_radix(hex, 16).ok()?
    } else if let Some(t) = attr(e, "theme").and_then(|v| v.parse::<usize>().ok()) {
        *theme.get(t)?
    } else {
        let i = attr(e, "indexed").and_then(|v| v.parse::<usize>().ok())?;
        match i {
            64 => return None, // system foreground
            65 => 0xFFFFFF,    // system background
            _ => *INDEXED_COLORS.get(i)?,
        }
    };
    let tint: f64 = attr(e, "tint").and_then(|v| v.parse().ok()).unwrap_or(0.0);
    Some(apply_tint(base, tint))
}

fn line_style(s: &str) -> LineStyle {
    match s {
        "thin" | "hair" => LineStyle::Thin,
        "medium" | "mediumDashDot" | "mediumDashDotDot" => LineStyle::Medium,
        "thick" => LineStyle::Thick,
        "dashed" | "mediumDashed" | "dashDot" | "dashDotDot" | "slantDashDot" => LineStyle::Dashed,
        "dotted" => LineStyle::Dotted,
        "double" => LineStyle::Double,
        _ => LineStyle::None,
    }
}

fn flag(e: &BytesStart) -> bool {
    attr(e, "val").is_none_or(|v| v != "0" && v != "false" && v != "none")
}

/// What one cellXfs entry contributes to a cell.
#[derive(Clone, Default)]
pub(crate) struct Xf {
    num_fmt: Option<String>,
    style: Style,
}

fn read_styles(zip: &mut Zip) -> Result<Vec<Xf>, String> {
    let theme = read_theme(zip)?;
    let mut out = Vec::new();
    let Some(bytes) = read_entry(zip, "xl/styles.xml")? else { return Ok(out) };
    let mut r = xml(&bytes);
    let mut custom: HashMap<u32, String> = HashMap::new();
    let mut fonts: Vec<Style> = Vec::new();
    let mut fills: Vec<Option<Rgb>> = Vec::new();
    let mut borders: Vec<[Border; 4]> = Vec::new();
    let mut section = String::new();
    let mut cur_style = Style::default();
    let mut cur_fill: Option<Rgb> = None;
    let mut solid = false;
    let mut cur_border = [Border::default(); 4];
    let mut border_side: Option<usize> = None;
    let mut cur_xf: Option<Xf> = None;
    loop {
        let ev = r.read_event().map_err(|e| e.to_string())?;
        let (e, empty) = match &ev {
            Event::Start(e) => (Some(e.clone()), false),
            Event::Empty(e) => (Some(e.clone()), true),
            _ => (None, false),
        };
        if let Some(e) = e {
            let name = local(e.name().as_ref()).to_string();
            match (section.as_str(), name.as_str()) {
                (_, "numFmt") => {
                    if let (Some(id), Some(code)) = (attr(&e, "numFmtId"), attr(&e, "formatCode")) {
                        custom.insert(id.parse().unwrap_or(0), code);
                    }
                }
                (_, "fonts" | "fills" | "borders" | "cellXfs" | "cellStyleXfs" | "dxfs") if !empty => section = name.clone(),
                ("fonts", "font") => cur_style = Style::default(),
                ("fonts", "b") => cur_style.bold = flag(&e),
                ("fonts", "i") => cur_style.italic = flag(&e),
                ("fonts", "u") => cur_style.underline = flag(&e),
                ("fonts", "strike") => cur_style.strike = flag(&e),
                ("fonts", "color") => {
                    // The default text color (black / dark 1) stays automatic so dark mode works.
                    cur_style.color = color_of(&e, &theme).filter(|&c| c != 0x000000 && c != theme[1]);
                }
                ("fills", "fill") => {
                    cur_fill = None;
                    solid = false;
                }
                ("fills", "patternFill") => solid = attr(&e, "patternType").as_deref() == Some("solid"),
                ("fills", "fgColor") if solid => cur_fill = color_of(&e, &theme),
                ("borders", "border") => cur_border = [Border::default(); 4],
                ("borders", side @ ("top" | "bottom" | "left" | "right" | "start" | "end")) => {
                    let i = match side {
                        "top" => 0,
                        "bottom" => 1,
                        "left" | "start" => 2,
                        _ => 3,
                    };
                    cur_border[i].style = attr(&e, "style").map_or(LineStyle::None, |s| line_style(&s));
                    border_side = (!empty).then_some(i);
                }
                ("borders", "color") => {
                    if let Some(i) = border_side {
                        cur_border[i].color = color_of(&e, &theme).filter(|&c| c != 0x000000 && c != theme[1]);
                    }
                }
                ("cellXfs", "xf") => {
                    let num: u32 = attr(&e, "numFmtId").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let code = custom.get(&num).cloned().or_else(|| builtin_format(num).map(str::to_string));
                    let mut style = attr(&e, "fontId").and_then(|v| v.parse::<usize>().ok()).and_then(|i| fonts.get(i)).cloned().unwrap_or_default();
                    style.fill = attr(&e, "fillId").and_then(|v| v.parse::<usize>().ok()).and_then(|i| fills.get(i).copied()).flatten();
                    if let Some(b) = attr(&e, "borderId").and_then(|v| v.parse::<usize>().ok()).and_then(|i| borders.get(i)) {
                        [style.top, style.bottom, style.left, style.right] = *b;
                    }
                    let xf = Xf { num_fmt: code.filter(|c| !c.eq_ignore_ascii_case("General")), style };
                    if empty {
                        out.push(xf);
                    } else {
                        cur_xf = Some(xf);
                    }
                }
                ("cellXfs", "alignment") => {
                    if let Some(xf) = cur_xf.as_mut() {
                        xf.style.align = match attr(&e, "horizontal").as_deref() {
                            Some("left") => HAlign::Left,
                            Some("center" | "centerContinuous") => HAlign::Center,
                            Some("right") => HAlign::Right,
                            _ => HAlign::General,
                        };
                    }
                }
                _ => {}
            }
            // Self-closing elements also end themselves.
            if empty {
                end_element(&name, &section, &mut fonts, &cur_style, &mut fills, cur_fill, &mut borders, &cur_border, &mut border_side);
            }
            continue;
        }
        match ev {
            Event::End(e) => {
                let name = local(e.name().as_ref()).to_string();
                if name == section {
                    section.clear();
                } else if name == "xf" && section == "cellXfs" {
                    if let Some(xf) = cur_xf.take() {
                        out.push(xf);
                    }
                } else {
                    end_element(&name, &section, &mut fonts, &cur_style, &mut fills, cur_fill, &mut borders, &cur_border, &mut border_side);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn end_element(
    name: &str,
    section: &str,
    fonts: &mut Vec<Style>,
    cur_style: &Style,
    fills: &mut Vec<Option<Rgb>>,
    cur_fill: Option<Rgb>,
    borders: &mut Vec<[Border; 4]>,
    cur_border: &[Border; 4],
    border_side: &mut Option<usize>,
) {
    match (section, name) {
        ("fonts", "font") => fonts.push(cur_style.clone()),
        ("fills", "fill") => fills.push(cur_fill),
        ("borders", "border") => borders.push(*cur_border),
        ("borders", "top" | "bottom" | "left" | "right" | "start" | "end") => *border_side = None,
        _ => {}
    }
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
    styles: &'a [Xf],
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
                            if let Some(xf) = style.and_then(|s| cx.styles.get(s)) {
                                wb.set_format(Addr::new(cx.id, cell), xf.num_fmt.as_deref());
                                wb.set_style(Addr::new(cx.id, cell), &xf.style);
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
                    if let Some(xf) = style.and_then(|s| cx.styles.get(s)) {
                        wb.set_format(addr, xf.num_fmt.as_deref());
                        wb.set_style(addr, &xf.style);
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
