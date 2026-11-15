//! Streaming xlsx reader: workbook structure, shared strings, number formats,
//! column widths, values, formulas (including shared formulas) and cached
//! results.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Cursor, Read, Seek};
use std::path::Path;
use std::sync::Arc;

use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesRef, BytesStart, Event};
use xl_engine::format::builtin_format;
use xl_engine::formula::parse_formula;
use xl_engine::input::classify_literal;
use xl_engine::cond::{CfOp, CfRule, CfStyle, CfValue, TextMatch};
use xl_engine::style::{INDEXED_COLORS, apply_tint};
use xl_engine::{Addr, Border, CellRef, ErrorKind, HAlign, LineStyle, LoadedCell, MAX_COLS, MAX_ROWS, Range, Rgb, SheetId, Style, VAlign, Value, Workbook, validate_sheet_name};
use zip::ZipArchive;

use crate::{Loaded, from_excel_height, from_excel_width};

pub(crate) type Zip = ZipArchive<Cursor<Vec<u8>>>;

pub(crate) fn local(name: &str) -> &str {
    match name.rfind(':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

pub(crate) fn attr(e: &BytesStart, key: &str) -> Option<String> {
    e.attributes().with_checks(false).flatten().find(|a| local(a.key.as_ref()) == key).and_then(|a| a.normalized_value(XmlVersion::Implicit1_0).ok().map(|v| v.into_owned()))
}

pub(crate) fn push_entity(out: &mut String, r: &BytesRef) {
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

pub(crate) fn read_entry<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Result<Option<Vec<u8>>, String> {
    let mut f = match zip.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(format!("{name}: {e}")),
    };
    let mut buf = Vec::with_capacity(f.size() as usize);
    f.read_to_end(&mut buf).map_err(|e| format!("{name}: {e}"))?;
    Ok(Some(buf))
}

pub(crate) fn xml(bytes: &[u8]) -> Reader<&[u8]> {
    Reader::from_reader(bytes)
}

pub(crate) fn read_rels(zip: &mut Zip, name: &str) -> Result<HashMap<String, String>, String> {
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

/// A `<definedName>`: (name, index of the sheet it's local to, formula).
type DefinedNameXml = (String, Option<usize>, String);
/// Sheets as (name, relationship id), and defined names.
type WorkbookXml = (Vec<(String, String)>, Vec<DefinedNameXml>);

fn read_sheet_list(zip: &mut Zip) -> Result<WorkbookXml, String> {
    let bytes = read_entry(zip, "xl/workbook.xml")?.ok_or("not an xlsx file (missing xl/workbook.xml)")?;
    let mut r = xml(&bytes);
    let mut out = Vec::new();
    let mut names = Vec::new();
    let mut cur: Option<(String, Option<usize>, String)> = None;
    loop {
        match r.read_event().map_err(|e| e.to_string())? {
            Event::Start(e) | Event::Empty(e) if local(e.name().as_ref()) == "sheet" => {
                if let (Some(name), Some(rid)) = (attr(&e, "name"), attr(&e, "id")) {
                    out.push((name, rid));
                }
            }
            Event::Start(e) if local(e.name().as_ref()) == "definedName" => {
                if let Some(name) = attr(&e, "name") {
                    cur = Some((name, attr(&e, "localSheetId").and_then(|v| v.parse().ok()), String::new()));
                }
            }
            Event::Text(t) => {
                if let Some(c) = cur.as_mut() {
                    c.2.push_str(&t.xml_content(XmlVersion::Implicit1_0));
                }
            }
            Event::GeneralRef(g) => {
                if let Some(c) = cur.as_mut() {
                    push_entity(&mut c.2, &g);
                }
            }
            Event::End(e) if local(e.name().as_ref()) == "definedName" => names.extend(cur.take()),
            Event::Eof => break,
            _ => {}
        }
    }
    Ok((out, names))
}

pub(crate) fn read_shared_strings(zip: &mut Zip) -> Result<Vec<Arc<str>>, String> {
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
    pub(crate) num_fmt: Option<String>,
    pub(crate) style: Style,
}

/// Cell formats, conditional-format styles (dxfs) and theme colors.
type StylesXml = (Vec<Xf>, Vec<CfStyle>, Vec<Rgb>);

pub(crate) fn read_styles(zip: &mut Zip) -> Result<StylesXml, String> {
    let theme = read_theme(zip)?;
    let mut out = Vec::new();
    let mut dxfs: Vec<CfStyle> = Vec::new();
    let mut dxf_part = "";
    let Some(bytes) = read_entry(zip, "xl/styles.xml")? else { return Ok((out, dxfs, theme)) };
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
    // The workbook's default font (the first one) is xl's default font too.
    let mut default_font: Option<(Option<Arc<str>>, Option<u16>)> = None;
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
                ("fonts", "sz") => {
                    cur_style.font_size = attr(&e, "val").and_then(|v| v.parse::<f64>().ok()).map(|pt| (pt * 10.0).round() as u16);
                    if default_font.as_ref().is_some_and(|d| d.1 == cur_style.font_size) {
                        cur_style.font_size = None;
                    }
                }
                ("fonts", "name") => {
                    cur_style.font_name = attr(&e, "val").map(Arc::from);
                    if default_font.as_ref().is_some_and(|d| d.0 == cur_style.font_name) {
                        cur_style.font_name = None;
                    }
                }
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
                ("dxfs", "dxf") => {
                    dxfs.push(CfStyle::default());
                    dxf_part = "";
                }
                ("dxfs", "font") => dxf_part = "font",
                ("dxfs", "fill") => dxf_part = "fill",
                ("dxfs", "b") => {
                    if let Some(d) = dxfs.last_mut() {
                        d.bold = flag(&e);
                    }
                }
                ("dxfs", "i") => {
                    if let Some(d) = dxfs.last_mut() {
                        d.italic = flag(&e);
                    }
                }
                ("dxfs", "color") if dxf_part == "font" => {
                    if let Some(d) = dxfs.last_mut() {
                        d.color = color_of(&e, &theme);
                    }
                }
                ("dxfs", "bgColor") if dxf_part == "fill" => {
                    if let Some(d) = dxfs.last_mut() {
                        d.fill = color_of(&e, &theme).or(d.fill);
                    }
                }
                ("dxfs", "fgColor") if dxf_part == "fill" => {
                    if let Some(d) = dxfs.last_mut()
                        && d.fill.is_none()
                    {
                        d.fill = color_of(&e, &theme);
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
                        xf.style.valign = match attr(&e, "vertical").as_deref() {
                            Some("top") => VAlign::Top,
                            Some("center" | "justify" | "distributed") => VAlign::Center,
                            _ => VAlign::Bottom,
                        };
                        xf.style.wrap = attr(&e, "wrapText").is_some_and(|v| v == "1" || v == "true");
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
        if let Event::End(e) = &ev
            && section == "fonts"
            && local(e.name().as_ref()) == "font"
            && default_font.is_none()
        {
            // The first font defines the defaults; it is itself plain.
            default_font = Some((cur_style.font_name.take(), cur_style.font_size.take()));
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
    Ok((out, dxfs, theme))
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
    CfFormula,
    Sqref,
    DvFormula,
}

struct SharedFormula {
    origin: CellRef,
    expr: Option<xl_engine::formula::Expr>,
}

struct SheetCtx<'a> {
    id: SheetId,
    shared: &'a [Arc<str>],
    styles: &'a [Xf],
    dxfs: &'a [CfStyle],
    theme: &'a [Rgb],
}

/// A <cfRule> being read.
#[derive(Default)]
struct CfRuleXml {
    ty: String,
    dxf: Option<usize>,
    priority: i64,
    operator: String,
    stop: bool,
    text: String,
    rank: u32,
    percent: bool,
    bottom: bool,
    below: bool,
    formulas: Vec<String>,
    cfvos: Vec<(String, String)>,
    colors: Vec<Rgb>,
}

fn cf_value(ty: &str, val: &str) -> CfValue {
    let v = val.parse::<f64>().unwrap_or(0.0);
    match ty {
        "min" => CfValue::Min,
        "max" => CfValue::Max,
        "percent" => CfValue::Percent(v),
        "percentile" => CfValue::Percentile(v),
        _ => CfValue::Number(v),
    }
}

impl CfRuleXml {
    /// The rule in xl's terms; None for kinds xl doesn't support.
    fn to_rule(&self, wb: &Workbook, dxfs: &[CfStyle]) -> Option<CfRule> {
        let style = self.dxf.and_then(|i| dxfs.get(i).cloned()).unwrap_or_default();
        let expr = |i: usize| -> Option<Arc<xl_engine::formula::Expr>> { parse_formula(self.formulas.get(i)?, wb.sheets()).ok().map(Arc::new) };
        Some(match self.ty.as_str() {
            "cellIs" => {
                let op = match self.operator.as_str() {
                    "between" => CfOp::Between,
                    "notBetween" => CfOp::NotBetween,
                    "equal" => CfOp::Equal,
                    "notEqual" => CfOp::NotEqual,
                    "greaterThan" => CfOp::Greater,
                    "lessThan" => CfOp::Less,
                    "greaterThanOrEqual" => CfOp::GreaterEq,
                    "lessThanOrEqual" => CfOp::LessEq,
                    _ => return None,
                };
                let b = if matches!(op, CfOp::Between | CfOp::NotBetween) { Some(expr(1)?) } else { None };
                CfRule::Cell { op, a: expr(0)?, b, style }
            }
            "containsText" => CfRule::Text { kind: TextMatch::Contains, text: self.text.clone(), style },
            "notContainsText" => CfRule::Text { kind: TextMatch::NotContains, text: self.text.clone(), style },
            "beginsWith" => CfRule::Text { kind: TextMatch::BeginsWith, text: self.text.clone(), style },
            "endsWith" => CfRule::Text { kind: TextMatch::EndsWith, text: self.text.clone(), style },
            "top10" => CfRule::Top { n: self.rank.max(1), percent: self.percent, bottom: self.bottom, style },
            "aboveAverage" => CfRule::Average { above: !self.below, style },
            "duplicateValues" => CfRule::Duplicates { unique: false, style },
            "uniqueValues" => CfRule::Duplicates { unique: true, style },
            "containsBlanks" => CfRule::Blanks { blanks: true, style },
            "notContainsBlanks" => CfRule::Blanks { blanks: false, style },
            "expression" => CfRule::Formula { expr: expr(0)?, style },
            "colorScale" if (2..=3).contains(&self.colors.len()) && self.cfvos.len() == self.colors.len() => {
                CfRule::ColorScale { stops: self.cfvos.iter().zip(&self.colors).map(|((t, v), c)| (cf_value(t, v), *c)).collect() }
            }
            "dataBar" if self.cfvos.len() >= 2 => CfRule::DataBar {
                color: self.colors.first().copied().unwrap_or(0x638EC6),
                min: cf_value(&self.cfvos[0].0, &self.cfvos[0].1),
                max: cf_value(&self.cfvos[1].0, &self.cfvos[1].1),
            },
            _ => return None,
        })
    }
}

/// Read one worksheet. Returns true if it had conditional formats xl can't keep.
fn read_sheet(wb: &mut Workbook, bytes: &[u8], cx: &SheetCtx, cells: &mut usize, warnings: &mut Vec<String>) -> Result<(bool, bool), String> {
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
    // AutoFilter: range, then per column the listed values.
    let mut filter: Option<xl_engine::AutoFilter> = None;
    let mut filter_col: Option<u32> = None;
    // Conditional formatting: (priority, rule) pairs, and the one being read.
    let mut cf_rules: Vec<(i64, xl_engine::cond::CondFormat)> = Vec::new();
    let mut cf_ranges: Vec<Range> = Vec::new();
    let mut cf_rule: Option<CfRuleXml> = None;
    let mut cf_formula = String::new();
    let mut cf_skipped = false;
    // Sparklines (an x14 extension): group kind/color, then each one's data and cell.
    let mut spark_kind = xl_engine::chart::SparkKind::Line;
    let mut spark_color: Option<Rgb> = None;
    let mut sqref = String::new();
    let mut sparklines: Vec<xl_engine::chart::Sparkline> = Vec::new();
    // Data validation: the rule being read (attributes, formulas) and the results.
    let mut dv: Option<DvXml> = None;
    let mut dv_formula = String::new();
    let mut validations: Vec<xl_engine::validation::Validation> = Vec::new();
    let mut dv_skipped = false;

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
                        let truthy = |k: &str| attr(e, k).is_some_and(|s| s == "1" || s == "true");
                        if truthy("customHeight")
                            && let Some(ht) = attr(e, "ht").and_then(|s| s.parse::<f64>().ok())
                        {
                            wb.set_row_height(cx.id, row, Some(from_excel_height(ht)));
                        }
                        if truthy("hidden")
                            && let Some(l) = wb.layout_mut(cx.id)
                        {
                            l.hidden_rows.insert(row);
                        }
                        if let Some(level) = attr(e, "outlineLevel").and_then(|s| s.parse::<u8>().ok()).filter(|&l| l > 0)
                            && let Some(l) = wb.layout_mut(cx.id)
                        {
                            l.row_levels.insert(row, level.min(7));
                        }
                    }
                    // Standard rules only; extension ones (x14) stay preserved as they are.
                    "dataValidation" if e.name().as_ref() == "dataValidation" || !e.name().as_ref().starts_with("x14:") => {
                        let attrs: Vec<(String, String)> = e
                            .attributes()
                            .with_checks(false)
                            .flatten()
                            .map(|a| (local(a.key.as_ref()).to_string(), a.normalized_value(XmlVersion::Implicit1_0).map(|v| v.into_owned()).unwrap_or_default()))
                            .collect();
                        dv = Some((attrs, Vec::new()));
                        if empty {
                            match dv.take().and_then(|(a, f)| validation_from_xml(&a, &f, wb)) {
                                Some(v) => validations.push(v),
                                None => dv_skipped = true,
                            }
                        }
                    }
                    "formula1" | "formula2" if dv.is_some() && !empty => {
                        dv_formula.clear();
                        cap = Capture::DvFormula;
                    }
                    "conditionalFormatting" => {
                        cf_ranges = attr(e, "sqref").map(|s| s.split_whitespace().filter_map(parse_range).collect()).unwrap_or_default();
                    }
                    // Extension copies (x14:cfRule) duplicate the main rule.
                    "cfRule" if e.name().as_ref() == "cfRule" => {
                        let truthy = |k: &str| attr(e, k).is_some_and(|s| s == "1" || s == "true");
                        cf_rule = Some(CfRuleXml {
                            ty: attr(e, "type").unwrap_or_default(),
                            dxf: attr(e, "dxfId").and_then(|s| s.parse().ok()),
                            priority: attr(e, "priority").and_then(|s| s.parse().ok()).unwrap_or(i64::MAX),
                            operator: attr(e, "operator").unwrap_or_default(),
                            stop: truthy("stopIfTrue"),
                            text: attr(e, "text").unwrap_or_default(),
                            rank: attr(e, "rank").and_then(|s| s.parse().ok()).unwrap_or(10),
                            percent: truthy("percent"),
                            bottom: truthy("bottom"),
                            below: attr(e, "aboveAverage").is_some_and(|s| s == "0" || s == "false"),
                            ..Default::default()
                        });
                        if empty {
                            if let Some(rule) = cf_rule.take().and_then(|x| x.to_rule(wb, cx.dxfs).map(|r| (x.priority, x.stop, r))) {
                                cf_rules.push((rule.0, xl_engine::cond::CondFormat { ranges: cf_ranges.clone(), rule: rule.2, stop_if_true: rule.1 }));
                            } else {
                                cf_skipped = true;
                            }
                        }
                    }
                    "formula" if cf_rule.is_some() && !empty => {
                        cf_formula.clear();
                        cap = Capture::CfFormula;
                    }
                    "cfvo" => {
                        if let Some(rule) = cf_rule.as_mut() {
                            rule.cfvos.push((attr(e, "type").unwrap_or_default(), attr(e, "val").unwrap_or_default()));
                        }
                    }
                    "color" if cf_rule.is_some() => {
                        if let (Some(rule), Some(c)) = (cf_rule.as_mut(), color_of(e, cx.theme)) {
                            rule.colors.push(c);
                        }
                    }
                    "sparklineGroup" => {
                        spark_kind = match attr(e, "type").as_deref() {
                            Some("column") => xl_engine::chart::SparkKind::Column,
                            Some("stacked") => xl_engine::chart::SparkKind::WinLoss,
                            _ => xl_engine::chart::SparkKind::Line,
                        };
                        spark_color = None;
                    }
                    // Excel's and rust_xlsxwriter's default series colors mean "automatic".
                    "colorSeries" => spark_color = color_of(e, cx.theme).filter(|c| !matches!(c, 0x376092 | 0x254061)),
                    "sparkline" => {
                        f.clear();
                        sqref.clear();
                    }
                    "sqref" if !empty => {
                        sqref.clear();
                        cap = Capture::Sqref;
                    }
                    "autoFilter" => {
                        if let Some(r) = attr(e, "ref").and_then(|s| parse_range(&s)) {
                            filter = Some(xl_engine::AutoFilter { range: r, columns: Default::default() });
                        }
                    }
                    "filterColumn" => filter_col = attr(e, "colId").and_then(|s| s.parse::<u32>().ok()),
                    "filters" => {
                        if let (Some(f), Some(c)) = (filter.as_mut(), filter_col) {
                            let set = f.columns.entry(f.range.start.col + c).or_default();
                            if attr(e, "blank").is_some_and(|v| v == "1" || v == "true") {
                                set.insert(String::new());
                            }
                        }
                    }
                    "filter" => {
                        if let (Some(f), Some(c), Some(v)) = (filter.as_mut(), filter_col, attr(e, "val")) {
                            f.columns.entry(f.range.start.col + c).or_default().insert(v);
                        }
                    }
                    "mergeCell" => {
                        if let Some(r) = attr(e, "ref").and_then(|s| parse_range(&s))
                            && r.cell_count() > 1
                            && let Some(l) = wb.layout_mut(cx.id)
                        {
                            l.merges.push(r);
                        }
                    }
                    "pane" => {
                        let frozen = matches!(attr(e, "state").as_deref(), Some("frozen" | "frozenSplit"));
                        let split = |k: &str| attr(e, k).and_then(|s| s.parse::<f64>().ok()).map_or(0, |v| v as u32);
                        if frozen && let Some(l) = wb.layout_mut(cx.id) {
                            l.freeze = (split("ySplit").min(MAX_ROWS - 1), split("xSplit").min(MAX_COLS - 1));
                        }
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
                        if let Some(level) = attr(e, "outlineLevel").and_then(|s| s.parse::<u8>().ok()).filter(|&l| l > 0)
                            && max >= min
                            && max - min < 16_384
                            && let Some(l) = wb.layout_mut(cx.id)
                        {
                            for c in min - 1..max.min(MAX_COLS) {
                                l.col_levels.insert(c, level.min(7));
                            }
                        }
                        if attr(e, "hidden").is_some_and(|s| s == "1" || s == "true")
                            && max >= min
                            && let Some(l) = wb.layout_mut(cx.id)
                        {
                            l.hidden_cols.extend(min - 1..max.min(MAX_COLS));
                        }
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
                Capture::CfFormula => cf_formula.push_str(&t.xml_content(XmlVersion::Implicit1_0)),
                Capture::Sqref => sqref.push_str(&t.xml_content(XmlVersion::Implicit1_0)),
                Capture::DvFormula => dv_formula.push_str(&t.xml_content(XmlVersion::Implicit1_0)),
                Capture::None => {}
            },
            Event::GeneralRef(ref g) => match cap {
                Capture::Value | Capture::Inline => push_entity(&mut v, g),
                Capture::Formula => push_entity(&mut f, g),
                Capture::CfFormula => push_entity(&mut cf_formula, g),
                Capture::Sqref => push_entity(&mut sqref, g),
                Capture::DvFormula => push_entity(&mut dv_formula, g),
                Capture::None => {}
            },
            Event::End(ref e) => match local(e.name().as_ref()) {
                "v" | "f" | "t" | "sqref" => cap = Capture::None,
                "formula1" | "formula2" if cap == Capture::DvFormula => {
                    cap = Capture::None;
                    if let Some((_, formulas)) = dv.as_mut() {
                        formulas.push(std::mem::take(&mut dv_formula));
                    }
                }
                "dataValidation" => {
                    if let Some((attrs, formulas)) = dv.take() {
                        match validation_from_xml(&attrs, &formulas, wb) {
                            Some(v) => validations.push(v),
                            None => dv_skipped = true,
                        }
                    }
                }
                "sparkline" => {
                    // The data reference was captured like a cell formula.
                    if let (Some(cell), Ok(data)) = (CellRef::parse(sqref.trim()), parse_formula(f.trim(), wb.sheets())) {
                        sparklines.push(xl_engine::chart::Sparkline { cell, kind: spark_kind, data: Arc::new(data), color: spark_color });
                    }
                    f.clear();
                }
                "formula" if cap == Capture::CfFormula => {
                    cap = Capture::None;
                    if let Some(rule) = cf_rule.as_mut() {
                        rule.formulas.push(std::mem::take(&mut cf_formula));
                    }
                }
                "cfRule" if e.name().as_ref() == "cfRule" => {
                    if let Some(x) = cf_rule.take() {
                        match x.to_rule(wb, cx.dxfs) {
                            Some(rule) => cf_rules.push((x.priority, xl_engine::cond::CondFormat { ranges: cf_ranges.clone(), rule, stop_if_true: x.stop })),
                            None => cf_skipped = true,
                        }
                    }
                }
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
    if !validations.is_empty()
        && let Some(l) = wb.layout_mut(cx.id)
    {
        l.validations = validations;
    }
    if !sparklines.is_empty()
        && let Some(l) = wb.layout_mut(cx.id)
    {
        l.sparklines = sparklines;
    }
    if !cf_rules.is_empty()
        && let Some(l) = wb.layout_mut(cx.id)
    {
        cf_rules.sort_by_key(|(p, _)| *p);
        l.cond = cf_rules.into_iter().map(|(_, c)| c).collect();
    }
    if let Some(f) = filter
        && let Some(l) = wb.layout_mut(cx.id)
    {
        // Rows the filter hid aren't hidden by hand.
        let rows: Vec<u32> = l.hidden_rows.range(f.range.start.row + 1..=f.range.end.row).copied().collect();
        for r in rows {
            l.hidden_rows.remove(&r);
            l.filtered_rows.insert(r);
        }
        l.filter = Some(f);
    }
    if unresolved_shared > 0 {
        warnings.push(format!("{}: {unresolved_shared} shared formulas couldn't be expanded and were kept as values", wb.sheet_name(cx.id)));
    }
    Ok((cf_skipped, dv_skipped))
}

/// Package parts xl doesn't model, by path prefix.
const UNSUPPORTED_PARTS: &[(&str, &str)] = &[
    ("xl/media/", "drawings and images"),
    ("xl/threadedComments/", "comments"),
    ("xl/pivotTables/", "pivot tables"),
    ("xl/pivotCache/", "pivot tables"),
    ("xl/vbaProject.bin", "macros"),
    ("xl/externalLinks/", "links to other workbooks"),
    ("xl/slicers/", "slicers"),
    ("xl/timelines/", "timelines"),
    ("customXml/", "custom XML"),
];

/// Worksheet elements xl doesn't model.
const UNSUPPORTED_SHEET_ELEMENTS: &[(&str, &str)] = &[
    ("<x14:dataValidations", "data validation on other sheets"),
    ("<sheetProtection", "sheet protection"),
    ("<customFilters", "advanced filters"),
    ("<top10", "advanced filters"),
    ("<dynamicFilter", "advanced filters"),
    ("<colorFilter", "advanced filters"),
    ("<iconFilter", "advanced filters"),
];

/// A `<dataValidation>` being read: its attributes and formulas.
type DvXml = (Vec<(String, String)>, Vec<String>);

/// A `<dataValidation>` in xl's terms; None for rules xl can't represent.
fn validation_from_xml(attrs: &[(String, String)], formulas: &[String], wb: &Workbook) -> Option<xl_engine::validation::Validation> {
    use xl_engine::validation::{ErrorStyle, ListSource, Validation, ValidationKind, ValidationOp, ValidationRule};
    let get = |k: &str| attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
    let flag = |k: &str| get(k).is_some_and(|v| v == "1" || v == "true");
    let ranges: Vec<Range> = get("sqref")?.split_whitespace().filter_map(parse_range).collect();
    if ranges.is_empty() {
        return None;
    }
    let expr = |i: usize| -> Option<Arc<xl_engine::formula::Expr>> { parse_formula(formulas.get(i)?.trim().trim_start_matches('='), wb.sheets()).ok().map(Arc::new) };
    let op = match get("operator").unwrap_or("between") {
        "between" => ValidationOp::Between,
        "notBetween" => ValidationOp::NotBetween,
        "equal" => ValidationOp::Equal,
        "notEqual" => ValidationOp::NotEqual,
        "greaterThan" => ValidationOp::Greater,
        "lessThan" => ValidationOp::Less,
        "greaterThanOrEqual" => ValidationOp::GreaterEq,
        "lessThanOrEqual" => ValidationOp::LessEq,
        _ => return None,
    };
    let compare = |kind| -> Option<ValidationRule> { Some(ValidationRule::Compare { kind, op, a: expr(0)?, b: if op.two_values() { Some(expr(1)?) } else { None } }) };
    let rule = match get("type").unwrap_or("none") {
        "list" => {
            let f = formulas.first()?.trim();
            match f.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
                Some(items) => ValidationRule::List(ListSource::Items(items.replace("\"\"", "\"").split(',').map(|i| i.trim().to_string()).collect())),
                None => ValidationRule::List(ListSource::Cells(expr(0)?)),
            }
        }
        "whole" => compare(ValidationKind::Whole)?,
        "decimal" => compare(ValidationKind::Decimal)?,
        "date" => compare(ValidationKind::Date)?,
        "time" => compare(ValidationKind::Time)?,
        "textLength" => compare(ValidationKind::TextLength)?,
        "custom" => ValidationRule::Custom(expr(0)?),
        // "none" (any value, used just for an input message) and others.
        _ => return None,
    };
    let message = |title: &str, body: &str| -> Option<(String, String)> {
        let (t, b) = (get(title).unwrap_or(""), get(body).unwrap_or(""));
        (!t.is_empty() || !b.is_empty()).then(|| (t.to_string(), b.to_string()))
    };
    Some(Validation {
        ranges,
        rule,
        allow_blank: flag("allowBlank"),
        // The attribute is named backwards: showDropDown="1" hides it.
        dropdown: !flag("showDropDown"),
        prompt: if get("showInputMessage").is_none_or(|v| v == "1" || v == "true") { message("promptTitle", "prompt") } else { None },
        error: message("errorTitle", "error"),
        error_style: match get("errorStyle") {
            Some("warning") => ErrorStyle::Warning,
            Some("information") => ErrorStyle::Information,
            _ => ErrorStyle::Stop,
        },
    })
}

/// "A1:B2" (or a single cell) to a range.
fn parse_range(s: &str) -> Option<Range> {
    let (a, b) = s.split_once(':').unwrap_or((s, s));
    Some(Range::new(CellRef::parse(a)?, CellRef::parse(b)?))
}

pub(crate) fn sheet_path(target: &str) -> String {
    match target.strip_prefix('/') {
        Some(abs) => abs.to_string(),
        None => format!("xl/{}", target.trim_start_matches("./")),
    }
}

pub fn read_xlsx(path: &Path) -> Result<Loaded, String> {
    let mut bytes = Vec::new();
    File::open(path).and_then(|mut f| f.read_to_end(&mut bytes)).map_err(|e| e.to_string())?;
    let mut zip = ZipArchive::new(Cursor::new(bytes.clone())).map_err(|e| format!("not an xlsx file: {e}"))?;
    let rels = read_rels(&mut zip, "xl/_rels/workbook.xml.rels")?;
    let (list, defined_names) = read_sheet_list(&mut zip)?;
    let shared = read_shared_strings(&mut zip)?;
    let (styles, dxfs, theme) = read_styles(&mut zip)?;

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
    // Names before cells, so formulas that use them get the right dependencies.
    for (name, local, formula) in defined_names {
        // Built-ins (print areas, filter ranges) aren't user names.
        if name.starts_with("_xlnm.") {
            continue;
        }
        let scope = local.and_then(|i| ids.get(i).copied());
        if !wb.load_name(&name, scope, formula.trim_start_matches('=')) {
            warnings.push(format!("Name '{name}' couldn't be read and was dropped"));
        }
    }
    let mut cells = 0;
    let mut unsupported: Vec<&'static str> = Vec::new();
    let mut note = |label: &'static str| {
        if !unsupported.contains(&label) {
            unsupported.push(label);
        }
    };
    let names: Vec<String> = zip.file_names().flatten().map(|n| n.into_owned()).collect();
    for name in names {
        if let Some(&(_, label)) = UNSUPPORTED_PARTS.iter().find(|(prefix, _)| name.starts_with(prefix)) {
            note(label);
        }
    }
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
        for &(tag, label) in UNSUPPORTED_SHEET_ELEMENTS {
            if bytes.windows(tag.len()).any(|w| w == tag.as_bytes()) {
                note(label);
            }
        }
        let skipped = read_sheet(&mut wb, &bytes, &SheetCtx { id: *id, shared: &shared, styles: &styles, dxfs: &dxfs, theme: &theme }, &mut cells, &mut warnings)?;
        if skipped.0 {
            note("some kinds of conditional formatting");
        }
        if skipped.1 {
            note("some data validation rules");
        }
        crate::xlsx_charts::read_sheet_tables(&mut zip, &part, &mut wb, *id)?;
        crate::xlsx_charts::read_sheet_notes(&mut zip, &part, &mut wb, *id)?;
        crate::xlsx_charts::read_sheet_links(&mut zip, &part, &mut wb, *id)?;
        let drawn = crate::xlsx_charts::read_sheet_charts(&mut zip, &part, &mut wb, *id)?;
        if drawn.other_drawings {
            note("drawings and images");
        }
        if drawn.unsupported_charts {
            note("some kinds of charts");
        }
        if drawn.charts > 0 && drawn.styled_charts {
            note("chart styling (the charts themselves are kept)");
        }
    }
    if list.iter().any(|(_, rid)| rels.get(rid).is_none_or(|t| !t.contains("worksheets/"))) {
        note("chart sheets");
    }
    // Old Mac workbooks count dates from 1904.
    if read_entry(&mut zip, "xl/workbook.xml")?.is_some_and(|b| {
        let t = String::from_utf8_lossy(&b);
        t.contains("date1904=\"1\"") || t.contains("date1904=\"true\"")
    }) {
        wb.convert_from_1904();
    }
    // Formulas using tables get their dependencies once the tables are known.
    let has_tables = wb.sheet_ids().iter().any(|&s| !wb.layout(s).tables.is_empty());
    let stats = if has_tables { wb.rebuild() } else { wb.recalc_all() };
    // Keep the original to save back into (if it can be indexed).
    let package = crate::package::Package::new(bytes, &wb).ok();
    Ok(Loaded { workbook: wb, cells, warnings, recalc: stats, unsupported, package })
}
