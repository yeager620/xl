//! Saving into the opened xlsx package instead of writing a new file.
//!
//! Parts xl didn't change are copied untouched. In a changed worksheet only
//! the elements xl models (and that changed) are regenerated — cells, column
//! widths, merges, conditional formats, the filter, frozen panes — and
//! spliced between the original's other elements, which pass through as
//! they were (validation, hyperlinks, drawings, page setup, extensions…).
//! Styles and shared strings are append-only, so indexes other parts use
//! stay valid.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use quick_xml::events::Event;
use xl_engine::cond::{CfOp, CfRule, CfStyle, CfValue, CondFormat, TextMatch};
use xl_engine::format::builtin_format;
use xl_engine::{Addr, CellInput, CellRef, Range, SheetId, Style, VAlign, Value, Workbook, precise};

use crate::package::{Package, SheetDigest, names_digest, original_tables, sheet_digest};
use crate::xlsx_read::{Xf, attr, local, read_rels, xml};
use crate::xlsx_write::{expand_this_row, prefix_future_functions};
use crate::{to_excel_height, to_excel_width_px};

// ------------------------------------------------------------ XML toolkit

pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // Control characters aren't allowed in XML 1.0; Excel escapes them like this.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => out.push_str(&format!("_x{:04X}_", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// A direct child of an XML document's root element, as byte ranges.
#[derive(Clone, Debug)]
struct Child {
    local: String,
    start: usize,
    end: usize,
}

/// The root's start tag end, its end tag start, the namespace prefix it
/// uses ("x:" or ""), and its direct children.
fn children(full: &[u8]) -> Result<(usize, usize, String, Vec<Child>), String> {
    // The reader skips a byte-order mark and counts positions after it.
    let bom = if full.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
    let (open, close, prefix, kids) = children_after_bom(&full[bom..])?;
    let kids = kids.into_iter().map(|k| Child { start: k.start + bom, end: k.end + bom, ..k }).collect();
    Ok((open + bom, close + bom, prefix, kids))
}

/// A root written as `<sst .../>` can't take children; spell it out as
/// `<sst ...></sst>` first.
fn expand_root(doc: &[u8]) -> Vec<u8> {
    let bom = if doc.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
    let mut r = xml(&doc[bom..]);
    let mut buf = Vec::new();
    loop {
        let before = r.buffer_position() as usize;
        match r.read_event_into(&mut buf) {
            Ok(Event::Start(_)) | Ok(Event::Eof) | Err(_) => return doc.to_vec(),
            Ok(Event::Empty(e)) => {
                let after = r.buffer_position() as usize;
                let name = e.name().as_ref().to_string();
                let tag = String::from_utf8_lossy(&doc[bom + before..bom + after]).to_string();
                let open = format!("{}>", tag.trim_end_matches("/>").trim_end());
                return apply_edits(doc, vec![(bom + before, bom + after, format!("{open}</{name}>"))]);
            }
            _ => {}
        }
        buf.clear();
    }
}

fn children_after_bom(doc: &[u8]) -> Result<(usize, usize, String, Vec<Child>), String> {
    let mut r = xml(doc);
    let mut buf = Vec::new();
    let mut depth = 0usize;
    let (mut root_open, mut root_close, mut prefix) = (0, doc.len(), String::new());
    let mut out: Vec<Child> = Vec::new();
    let mut child_start = 0;
    let mut child_name = String::new();
    loop {
        let before = r.buffer_position() as usize;
        let ev = r.read_event_into(&mut buf).map_err(|e| e.to_string())?;
        let after = r.buffer_position() as usize;
        match ev {
            Event::Start(ref e) => {
                if depth == 0 {
                    root_open = after;
                    let qname = e.name().as_ref().to_string();
                    prefix = qname.rfind(':').map_or(String::new(), |i| qname[..=i].to_string());
                } else if depth == 1 {
                    child_start = before;
                    child_name = local(e.name().as_ref()).to_string();
                }
                depth += 1;
            }
            Event::Empty(ref e) if depth == 1 => {
                out.push(Child { local: local(e.name().as_ref()).to_string(), start: before, end: after });
            }
            Event::End(_) => {
                depth -= 1;
                if depth == 1 {
                    out.push(Child { local: std::mem::take(&mut child_name), start: child_start, end: after });
                } else if depth == 0 {
                    root_close = before;
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok((root_open, root_close, prefix, out))
}

/// Give every tag in a generated snippet the document's namespace prefix.
fn prefixed(snippet: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        return snippet.to_string();
    }
    let mut out = String::with_capacity(snippet.len() + 64);
    let b = snippet.as_bytes();
    let mut i = 0;
    let mut in_attr = false;
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            in_attr = !in_attr;
        }
        out.push(c as char);
        if c == b'<' && !in_attr {
            if b.get(i + 1) == Some(&b'/') {
                out.push('/');
                i += 1;
            }
            out.push_str(prefix);
        }
        i += 1;
    }
    // Multi-byte characters were pushed byte-wise above; redo safely.
    if snippet.is_ascii() { out } else { prefixed_utf8(snippet, prefix) }
}

fn prefixed_utf8(snippet: &str, prefix: &str) -> String {
    let mut out = String::new();
    let mut in_attr = false;
    let mut chars = snippet.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            in_attr = !in_attr;
        }
        out.push(c);
        if c == '<' && !in_attr {
            if chars.peek() == Some(&'/') {
                out.push('/');
                chars.next();
            }
            out.push_str(prefix);
        }
    }
    out
}

/// Set (Some) or remove (None) attributes in a start tag's text.
fn set_attrs(tag: &str, changes: &[(&str, Option<String>)]) -> String {
    let mut t = tag.to_string();
    for (name, value) in changes {
        let pat = format!(" {name}=\"");
        if let Some(i) = t.find(&pat) {
            let vstart = i + pat.len();
            let vend = vstart + t[vstart..].find('"').unwrap_or(0);
            t = match value {
                Some(v) => format!("{}{}{}", &t[..vstart], esc(v), &t[vend..]),
                None => format!("{}{}", &t[..i], &t[vend + 1..]),
            };
        } else if let Some(v) = value {
            let at = if t.ends_with("/>") { t.len() - 2 } else { t.len() - 1 };
            t = format!("{} {name}=\"{}\"{}", &t[..at], esc(v), &t[at..]);
        }
    }
    t
}

/// The start tag of an element snippet, and the rest.
fn split_start_tag(el: &str) -> (&str, &str) {
    // The first '>' outside quotes ends the start tag.
    let mut q = false;
    for (i, c) in el.char_indices() {
        match c {
            '"' => q = !q,
            '>' if !q => return (&el[..=i], &el[i + 1..]),
            _ => {}
        }
    }
    (el, "")
}

/// The XML between an element's start and end tags ("" if self-closing).
fn inner_of(el: &str) -> &str {
    let (tag, rest) = split_start_tag(el);
    if tag.ends_with("/>") {
        return "";
    }
    rest.rfind("</").map_or(rest, |i| &rest[..i])
}

/// Number of direct child elements of an element snippet.
fn child_count(el: &str) -> usize {
    children(format!("<r>{}</r>", inner_of(el)).as_bytes()).map(|(_, _, _, c)| c.len()).unwrap_or(0)
}

/// Apply byte-range replacements to a document.
fn apply_edits(doc: &[u8], mut edits: Vec<(usize, usize, String)>) -> Vec<u8> {
    edits.sort_by_key(|e| (e.0, e.1));
    let mut out = Vec::with_capacity(doc.len() + 1024);
    let mut pos = 0;
    for (a, b, text) in edits {
        if a < pos {
            continue;
        }
        out.extend_from_slice(&doc[pos..a]);
        out.extend_from_slice(text.as_bytes());
        pos = b;
    }
    out.extend_from_slice(&doc[pos..]);
    out
}

/// Replace, remove or insert root children by local name, keeping the
/// schema's element order (`order` lists the names in sequence).
fn splice(doc: &[u8], order: &[&str], mut replace: HashMap<&str, Option<String>>) -> Result<Vec<u8>, String> {
    let doc = &expand_root(doc);
    let (_, root_close, prefix, kids) = children(doc)?;
    let mut edits = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for k in &kids {
        let first = seen.insert(k.local.clone());
        if let Some(rep) = replace.get(k.local.as_str()) {
            let text = if first { rep.as_ref().map(|r| prefixed(r, &prefix)).unwrap_or_default() } else { String::new() };
            edits.push((k.start, k.end, text));
        }
    }
    // Elements that weren't there go before the first later-ranked one.
    let rank = |n: &str| order.iter().position(|o| *o == n).unwrap_or(order.len());
    for (name, rep) in replace.drain() {
        let Some(rep) = rep else { continue };
        if seen.contains(name) {
            continue;
        }
        let at = kids.iter().find(|k| rank(&k.local) > rank(name)).map_or(root_close, |k| k.start);
        edits.push((at, at, prefixed(&rep, &prefix)));
    }
    Ok(apply_edits(doc, edits))
}

const SHEET_ORDER: &[&str] = &[
    "sheetPr", "dimension", "sheetViews", "sheetFormatPr", "cols", "sheetData", "sheetCalcPr", "sheetProtection", "protectedRanges", "scenarios", "autoFilter", "sortState",
    "dataConsolidate", "customSheetViews", "mergeCells", "phoneticPr", "conditionalFormatting", "dataValidations", "hyperlinks", "printOptions", "pageMargins", "pageSetup",
    "headerFooter", "rowBreaks", "colBreaks", "customProperties", "cellWatches", "ignoredErrors", "smartTags", "drawing", "legacyDrawing", "legacyDrawingHF", "picture",
    "oleObjects", "controls", "webPublishItems", "tableParts", "extLst",
];

const WORKBOOK_ORDER: &[&str] = &[
    "fileVersion", "fileSharing", "workbookPr", "workbookProtection", "bookViews", "sheets", "functionGroups", "externalReferences", "definedNames", "calcPr", "oleSize",
    "customWorkbookViews", "pivotCaches", "smartTagPr", "smartTagTypes", "webPublishing", "fileRecoveryPr", "webPublishObjects", "extLst",
];

const STYLE_ORDER: &[&str] = &["numFmts", "fonts", "fills", "borders", "cellStyleXfs", "cellXfs", "cellStyles", "dxfs", "tableStyles", "colors", "extLst"];

// ------------------------------------------------------------ styles

fn rgb_attr(c: u32) -> String {
    format!("FF{c:06X}")
}

/// The style table, extended (never renumbered) with what changed cells need.
struct Styles {
    xfs: Vec<Xf>,
    lookup: HashMap<(Option<String>, Style), u32>,
    added: Vec<(Option<String>, Style)>,
    dxfs: Vec<CfStyle>,
    dxf_added: Vec<CfStyle>,
    /// Custom number format codes by id, and the next free id.
    num_fmts: HashMap<String, u32>,
    next_fmt: u32,
    added_fmts: Vec<(u32, String)>,
    default_font: (String, String),
}

impl Styles {
    fn new(xfs: Vec<Xf>, dxfs: Vec<CfStyle>, styles_xml: Option<&[u8]>) -> Styles {
        let mut lookup = HashMap::new();
        for (i, xf) in xfs.iter().enumerate() {
            lookup.entry((xf.num_fmt.clone(), xf.style.clone())).or_insert(i as u32);
        }
        let mut num_fmts = HashMap::new();
        let mut next_fmt = 164;
        let mut default_font = ("Calibri".to_string(), "11".to_string());
        if let Some(bytes) = styles_xml {
            let mut r = xml(bytes);
            let mut buf = Vec::new();
            let mut fonts_seen = 0;
            let mut in_fonts = false;
            while let Ok(ev) = r.read_event_into(&mut buf) {
                match ev {
                    Event::Start(ref e) | Event::Empty(ref e) => match local(e.name().as_ref()) {
                        "numFmt" => {
                            if let (Some(id), Some(code)) = (attr(e, "numFmtId").and_then(|v| v.parse::<u32>().ok()), attr(e, "formatCode")) {
                                next_fmt = next_fmt.max(id + 1);
                                num_fmts.entry(code).or_insert(id);
                            }
                        }
                        "fonts" => in_fonts = true,
                        "font" if in_fonts => fonts_seen += 1,
                        "sz" if in_fonts && fonts_seen == 1 => {
                            if let Some(v) = attr(e, "val") {
                                default_font.1 = v;
                            }
                        }
                        "name" if in_fonts && fonts_seen == 1 => {
                            if let Some(v) = attr(e, "val") {
                                default_font.0 = v;
                            }
                        }
                        _ => {}
                    },
                    Event::End(ref e) if local(e.name().as_ref()) == "fonts" => in_fonts = false,
                    Event::Eof => break,
                    _ => {}
                }
                buf.clear();
            }
        }
        Styles { xfs, lookup, added: Vec::new(), dxfs, dxf_added: Vec::new(), num_fmts, next_fmt, added_fmts: Vec::new(), default_font }
    }

    /// The xf index for a cell: its original one if the cell still looks the
    /// same (keeping anything xl doesn't model, like rotation), else an equal
    /// existing one, else a new one.
    fn xf(&mut self, code: Option<&str>, st: &Style, original: Option<u32>) -> u32 {
        let key = (code.map(str::to_string), st.clone());
        if let Some(o) = original
            && self.xfs.get(o as usize).is_some_and(|x| x.num_fmt == key.0 && x.style == key.1)
        {
            return o;
        }
        if key.0.is_none() && st.is_default() {
            return 0;
        }
        if let Some(&i) = self.lookup.get(&key) {
            return i;
        }
        let i = (self.xfs.len() + self.added.len()) as u32;
        self.lookup.insert(key.clone(), i);
        self.added.push(key);
        i
    }

    fn dxf(&mut self, st: &CfStyle) -> u32 {
        if let Some(i) = self.dxfs.iter().chain(&self.dxf_added).position(|d| d == st) {
            return i as u32;
        }
        self.dxf_added.push(st.clone());
        (self.dxfs.len() + self.dxf_added.len() - 1) as u32
    }

    fn fmt_id(&mut self, code: Option<&str>) -> u32 {
        let Some(code) = code else { return 0 };
        if let Some(id) = (0..50).find(|&i| builtin_format(i) == Some(code)) {
            return id;
        }
        if let Some(&id) = self.num_fmts.get(code) {
            return id;
        }
        let id = self.next_fmt;
        self.next_fmt += 1;
        self.num_fmts.insert(code.to_string(), id);
        self.added_fmts.push((id, code.to_string()));
        id
    }

    /// Patch styles.xml (None if nothing was added).
    fn patch(&mut self, doc: &[u8]) -> Result<Option<Vec<u8>>, String> {
        if self.added.is_empty() && self.dxf_added.is_empty() {
            return Ok(None);
        }
        let doc = &expand_root(doc);
        let (_, _, _, kids) = children(doc)?;
        let text = String::from_utf8_lossy(doc).to_string();
        let count_in = |name: &str| -> usize {
            kids.iter().find(|k| k.local == name).map_or(0, |k| child_count(&text[k.start..k.end]))
        };
        let (n_fonts, n_fills, n_borders) = (count_in("fonts"), count_in("fills"), count_in("borders"));
        let (mut fonts, mut fills, mut borders, mut xfs) = (String::new(), String::new(), String::new(), String::new());
        let (mut nf, mut nfi, mut nb) = (n_fonts, n_fills, n_borders);
        let added = std::mem::take(&mut self.added);
        for (code, st) in &added {
            let num = self.fmt_id(code.as_deref());
            let font_id = if st.bold || st.italic || st.underline || st.strike || st.color.is_some() || st.font_size.is_some() || st.font_name.is_some() {
                let size = st.font_size.map_or(self.default_font.1.clone(), |t| precise(t as f64 / 10.0));
                let name = st.font_name.as_deref().map_or(self.default_font.0.clone(), str::to_string);
                fonts.push_str(&format!(
                    "<font>{}{}{}{}<sz val=\"{size}\"/>{}<name val=\"{}\"/></font>",
                    if st.bold { "<b/>" } else { "" },
                    if st.italic { "<i/>" } else { "" },
                    if st.strike { "<strike/>" } else { "" },
                    if st.underline { "<u/>" } else { "" },
                    st.color.map_or(String::new(), |c| format!("<color rgb=\"{}\"/>", rgb_attr(c))),
                    esc(&name)
                ));
                nf += 1;
                nf - 1
            } else {
                0
            };
            let fill_id = match st.fill {
                Some(c) => {
                    fills.push_str(&format!("<fill><patternFill patternType=\"solid\"><fgColor rgb=\"{}\"/><bgColor indexed=\"64\"/></patternFill></fill>", rgb_attr(c)));
                    nfi += 1;
                    nfi - 1
                }
                None => 0,
            };
            let border_id = if st.has_borders() {
                let side = |name: &str, b: &xl_engine::Border| {
                    if b.is_none() {
                        return format!("<{name}/>");
                    }
                    let style = match b.style {
                        xl_engine::LineStyle::None => "",
                        xl_engine::LineStyle::Thin => "thin",
                        xl_engine::LineStyle::Medium => "medium",
                        xl_engine::LineStyle::Thick => "thick",
                        xl_engine::LineStyle::Dashed => "dashed",
                        xl_engine::LineStyle::Dotted => "dotted",
                        xl_engine::LineStyle::Double => "double",
                    };
                    let color = b.color.map_or("<color auto=\"1\"/>".to_string(), |c| format!("<color rgb=\"{}\"/>", rgb_attr(c)));
                    format!("<{name} style=\"{style}\">{color}</{name}>")
                };
                borders.push_str(&format!("<border>{}{}{}{}<diagonal/></border>", side("left", &st.left), side("right", &st.right), side("top", &st.top), side("bottom", &st.bottom)));
                nb += 1;
                nb - 1
            } else {
                0
            };
            let h = match st.align {
                xl_engine::HAlign::General => None,
                xl_engine::HAlign::Left => Some("left"),
                xl_engine::HAlign::Center => Some("center"),
                xl_engine::HAlign::Right => Some("right"),
            };
            let v = match st.valign {
                VAlign::Bottom => None,
                VAlign::Center => Some("center"),
                VAlign::Top => Some("top"),
            };
            let align = if h.is_some() || v.is_some() || st.wrap {
                format!(
                    "<alignment{}{}{}/>",
                    h.map_or(String::new(), |h| format!(" horizontal=\"{h}\"")),
                    v.map_or(String::new(), |v| format!(" vertical=\"{v}\"")),
                    if st.wrap { " wrapText=\"1\"" } else { "" }
                )
            } else {
                String::new()
            };
            xfs.push_str(&format!(
                "<xf numFmtId=\"{num}\" fontId=\"{font_id}\" fillId=\"{fill_id}\" borderId=\"{border_id}\" xfId=\"0\"{}{}{}{}{}>{align}</xf>",
                if num != 0 { " applyNumberFormat=\"1\"" } else { "" },
                if font_id != 0 { " applyFont=\"1\"" } else { "" },
                if fill_id != 0 { " applyFill=\"1\"" } else { "" },
                if border_id != 0 { " applyBorder=\"1\"" } else { "" },
                if align.is_empty() { "" } else { " applyAlignment=\"1\"" }
            ));
        }
        let mut dxfs = String::new();
        for d in std::mem::take(&mut self.dxf_added) {
            let font = if d.bold || d.italic || d.color.is_some() {
                format!("<font>{}{}{}</font>", if d.bold { "<b/>" } else { "" }, if d.italic { "<i/>" } else { "" }, d.color.map_or(String::new(), |c| format!("<color rgb=\"{}\"/>", rgb_attr(c))))
            } else {
                String::new()
            };
            let fill = d.fill.map_or(String::new(), |c| format!("<fill><patternFill patternType=\"solid\"><bgColor rgb=\"{}\"/></patternFill></fill>", rgb_attr(c)));
            dxfs.push_str(&format!("<dxf>{font}{fill}</dxf>"));
        }
        let mut num_fmts = String::new();
        for (id, code) in std::mem::take(&mut self.added_fmts) {
            num_fmts.push_str(&format!("<numFmt numFmtId=\"{id}\" formatCode=\"{}\"/>", esc(&code)));
        }
        // Extend each list (or create it).
        let mut replace: HashMap<&str, Option<String>> = HashMap::new();
        let extend = |name: &str, extra: &str, kids: &[Child]| -> Option<String> {
            if extra.is_empty() {
                return None;
            }
            let extra_count = children(format!("<r>{extra}</r>").as_bytes()).map(|(_, _, _, c)| c.len()).unwrap_or(0);
            Some(match kids.iter().find(|k| k.local == name) {
                Some(k) => {
                    let el = &text[k.start..k.end];
                    let (tag, _) = split_start_tag(el);
                    let existing = child_count(el);
                    let tag_unprefixed = strip_prefix_tag(tag);
                    if tag.ends_with("/>") {
                        let open = set_attrs(&tag_unprefixed, &[("count", Some(extra_count.to_string()))]).replace("/>", ">");
                        format!("{open}{extra}</{name}>")
                    } else {
                        // The original inner XML keeps its prefixes; ours get added by splice.
                        let open = set_attrs(&tag_unprefixed, &[("count", Some((existing + extra_count).to_string()))]);
                        format!("{open}\u{1}{}\u{1}{extra}</{name}>", inner_of(el))
                    }
                }
                None => format!("<{name} count=\"{extra_count}\">{extra}</{name}>"),
            })
        };
        for (name, extra) in [("numFmts", &num_fmts), ("fonts", &fonts), ("fills", &fills), ("borders", &borders), ("cellXfs", &xfs), ("dxfs", &dxfs)] {
            if let Some(el) = extend(name, extra, &kids) {
                replace.insert(name, Some(el));
            }
        }
        splice_keeping_raw(doc, STYLE_ORDER, replace).map(Some)
    }
}

fn strip_prefix_tag(tag: &str) -> String {
    // "<x:fonts count=..>" → "<fonts count=..>"
    let after_lt = &tag[1..];
    let name_end = after_lt.find(|c: char| c.is_whitespace() || c == '>' || c == '/').unwrap_or(after_lt.len());
    let name = &after_lt[..name_end];
    match name.rfind(':') {
        Some(i) => format!("<{}{}", &name[i + 1..], &after_lt[name_end..]),
        None => tag.to_string(),
    }
}

/// Like `splice`, but a replacement may carry a raw (already prefixed)
/// middle section between \u{1} markers that must not be re-prefixed.
fn splice_keeping_raw(doc: &[u8], order: &[&str], replace: HashMap<&str, Option<String>>) -> Result<Vec<u8>, String> {
    let doc = &expand_root(doc);
    let (_, root_close, prefix, kids) = children(doc)?;
    let fix = |s: &str| -> String {
        let parts: Vec<&str> = s.split('\u{1}').collect();
        match parts.as_slice() {
            [a, raw, b] => format!("{}{}{}", prefixed(a, &prefix), raw, prefixed(b, &prefix)),
            _ => prefixed(s, &prefix),
        }
    };
    let mut edits = Vec::new();
    let mut seen = BTreeSet::new();
    for k in &kids {
        if seen.insert(k.local.clone())
            && let Some(rep) = replace.get(k.local.as_str())
        {
            edits.push((k.start, k.end, rep.as_deref().map(fix).unwrap_or_default()));
        }
    }
    let rank = |n: &str| order.iter().position(|o| *o == n).unwrap_or(order.len());
    for (name, rep) in &replace {
        let Some(rep) = rep else { continue };
        if seen.contains(*name) {
            continue;
        }
        let at = kids.iter().find(|k| rank(&k.local) > rank(name)).map_or(root_close, |k| k.start);
        edits.push((at, at, fix(rep)));
    }
    Ok(apply_edits(doc, edits))
}

// ------------------------------------------------------------ shared strings

struct Strings {
    lookup: HashMap<String, u32>,
    count: usize,
    added: Vec<String>,
}

impl Strings {
    fn new(existing: &[std::sync::Arc<str>]) -> Strings {
        let mut lookup = HashMap::new();
        for (i, s) in existing.iter().enumerate() {
            lookup.entry(s.to_string()).or_insert(i as u32);
        }
        Strings { lookup, count: existing.len(), added: Vec::new() }
    }

    fn index(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.lookup.get(s) {
            return i;
        }
        let i = (self.count + self.added.len()) as u32;
        self.lookup.insert(s.to_string(), i);
        self.added.push(s.to_string());
        i
    }

    fn items(&self) -> String {
        self.added.iter().map(|s| format!("<si><t xml:space=\"preserve\">{}</t></si>", esc(s))).collect()
    }
}

// ------------------------------------------------------------ worksheet pieces

/// Excel's column width in characters for our pixel width (7px digits, 5px padding).
fn excel_col_width(px: f64) -> f64 {
    let epx = to_excel_width_px(px) as f64;
    (((epx - 5.0) / 7.0 * 100.0 + 0.5).trunc() / 100.0).max(0.0)
}

fn range_ref(r: Range) -> String {
    if r.start == r.end { r.start.to_string() } else { format!("{}:{}", r.start, r.end) }
}

struct SheetCtx<'a> {
    wb: &'a Workbook,
    id: SheetId,
    styles: &'a mut Styles,
    strings: &'a mut Strings,
    original_xfs: Option<&'a HashMap<CellRef, u32>>,
    /// Extra attributes per cell, and the input they belong with.
    extras: Option<&'a HashMap<CellRef, (crate::package::CellExtra, String)>>,
    /// Attributes of the original `<row>` elements xl doesn't own.
    row_attrs: RowAttrs,
    /// The original `<col>` style indexes, per column.
    col_styles: HashMap<u32, String>,
    validations_changed: bool,
}

fn formula_text(wb: &Workbook, sheet: SheetId, cell: CellRef) -> String {
    let a = Addr::new(sheet, cell);
    let mut src = prefix_future_functions(&wb.formula_source(a).unwrap_or_default());
    if src.contains("[@") {
        src = expand_this_row(&src, wb.table_at(sheet, cell).map_or("", |t| t.name.as_str()));
    }
    src
}

impl SheetCtx<'_> {
    fn sheet_data(&mut self) -> String {
        let wb = self.wb;
        let id = self.id;
        let l = wb.layout(id);
        let mut rows: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        for (c, _) in wb.store(id).iter() {
            rows.entry(c.row).or_default().insert(c.col);
        }
        for (c, _) in wb.formatted_cells(id).chain(wb.styled_cells(id).map(|(c, _)| (c, ""))) {
            rows.entry(c.row).or_default().insert(c.col);
        }
        for r in l.row_heights.keys().chain(&l.hidden_rows).chain(&l.filtered_rows).chain(l.row_levels.keys()).chain(self.row_attrs.keys()) {
            rows.entry(*r).or_default();
        }
        let mut out = String::from("<sheetData>");
        for (r, cols) in rows {
            let mut attrs: Vec<(String, String)> = vec![("r".into(), (r + 1).to_string())];
            if let Some(orig) = self.row_attrs.get(&r) {
                attrs.extend(orig.iter().filter(|(k, _)| !matches!(k.as_str(), "r" | "spans" | "ht" | "customHeight" | "hidden" | "outlineLevel")).cloned());
            }
            if let Some(px) = l.row_heights.get(&r) {
                attrs.push(("ht".into(), precise(to_excel_height(*px))));
                attrs.push(("customHeight".into(), "1".into()));
            }
            if l.hidden_rows.contains(&r) || l.filtered_rows.contains(&r) {
                attrs.push(("hidden".into(), "1".into()));
            }
            if let Some(level) = l.row_levels.get(&r) {
                attrs.push(("outlineLevel".into(), level.to_string()));
            }
            out.push_str("<row");
            for (k, v) in &attrs {
                out.push_str(&format!(" {k}=\"{}\"", esc(v)));
            }
            if cols.is_empty() {
                out.push_str("/>");
                continue;
            }
            out.push('>');
            for c in cols {
                out.push_str(&self.cell(CellRef::new(r, c)));
            }
            out.push_str("</row>");
        }
        out.push_str("</sheetData>");
        out
    }

    fn cell(&mut self, cell: CellRef) -> String {
        let wb = self.wb;
        let a = Addr::new(self.id, cell);
        let original = self.original_xfs.and_then(|m| m.get(&cell).copied());
        let s = self.styles.xf(wb.format_code(a), wb.style(a), original);
        let s_attr = if s != 0 { format!(" s=\"{s}\"") } else { String::new() };
        let r = cell.to_string();
        // Attributes xl doesn't model come back when the cell is unchanged.
        let extra = self.extras.and_then(|m| m.get(&cell)).filter(|(_, input)| *input == wb.input_text(a)).map(|(e, _)| e);
        let mut s_attr = s_attr;
        if let Some(e) = extra {
            for (k, v) in &e.c_attrs {
                s_attr.push_str(&format!(" {k}=\"{}\"", esc(v)));
            }
        }
        let f_attrs: String = extra.map_or(String::new(), |e| e.f_attrs.iter().map(|(k, v)| format!(" {k}=\"{}\"", esc(v))).collect());
        let Some(c) = wb.cell(a) else { return format!("<c r=\"{r}\"{s_attr}/>") };
        // A 1904-based file gets its dates back in its own system.
        let to_file = |n: f64| if wb.date1904() && n >= 1.0 + xl_engine::DATE1904_OFFSET && wb.format(a).is_some_and(|f| f.has_date()) { n - xl_engine::DATE1904_OFFSET } else { n };
        match &c.input {
            CellInput::Formula(_) | CellInput::Opaque(_) => {
                let f = format!("<f{f_attrs}>{}</f>", esc(&formula_text(wb, self.id, cell)));
                match &c.value {
                    Value::Number(n) => format!("<c r=\"{r}\"{s_attr}>{f}<v>{}</v></c>", precise(to_file(*n))),
                    Value::Text(t) => format!("<c r=\"{r}\"{s_attr} t=\"str\">{f}<v>{}</v></c>", esc(t)),
                    Value::Bool(b) => format!("<c r=\"{r}\"{s_attr} t=\"b\">{f}<v>{}</v></c>", *b as u8),
                    Value::Error(e) => format!("<c r=\"{r}\"{s_attr} t=\"e\">{f}<v>{}</v></c>", esc(e.as_str())),
                    Value::Empty => format!("<c r=\"{r}\"{s_attr}>{f}</c>"),
                }
            }
            CellInput::Value(v) => match v {
                Value::Number(n) => format!("<c r=\"{r}\"{s_attr}><v>{}</v></c>", precise(to_file(*n))),
                Value::Text(t) => format!("<c r=\"{r}\"{s_attr} t=\"s\"><v>{}</v></c>", self.strings.index(t)),
                Value::Bool(b) => format!("<c r=\"{r}\"{s_attr} t=\"b\"><v>{}</v></c>", *b as u8),
                Value::Error(e) => format!("<c r=\"{r}\"{s_attr} t=\"e\"><v>{}</v></c>", esc(e.as_str())),
                Value::Empty => format!("<c r=\"{r}\"{s_attr}/>"),
            },
        }
    }

    fn cols(&self) -> Option<String> {
        let l = self.wb.layout(self.id);
        let mut cols: BTreeSet<u32> = l.col_widths.keys().copied().collect();
        cols.extend(&l.hidden_cols);
        cols.extend(l.col_levels.keys());
        cols.extend(self.col_styles.keys());
        if cols.is_empty() {
            return None;
        }
        let attrs = |c: u32| -> String {
            let mut s = String::new();
            match l.col_widths.get(&c) {
                Some(px) => s.push_str(&format!(" width=\"{}\" customWidth=\"1\"", precise(excel_col_width(*px)))),
                // Excel requires a width; the default column is 8.43 characters.
                None => s.push_str(" width=\"8.43\""),
            }
            if let Some(st) = self.col_styles.get(&c) {
                s.push_str(&format!(" style=\"{st}\""));
            }
            if l.hidden_cols.contains(&c) {
                s.push_str(" hidden=\"1\"");
            }
            if let Some(level) = l.col_levels.get(&c) {
                s.push_str(&format!(" outlineLevel=\"{level}\""));
            }
            s
        };
        let mut out = String::from("<cols>");
        let list: Vec<u32> = cols.into_iter().collect();
        let mut i = 0;
        while i < list.len() {
            let a = list[i];
            let mut j = i;
            while j + 1 < list.len() && list[j + 1] == list[j] + 1 && attrs(list[j + 1]) == attrs(a) {
                j += 1;
            }
            out.push_str(&format!("<col min=\"{}\" max=\"{}\"{}/>", a + 1, list[j] + 1, attrs(a)));
            i = j + 1;
        }
        out.push_str("</cols>");
        Some(out)
    }

    fn dimension(&self) -> String {
        let r = self.wb.store(self.id).extent().map_or("A1".to_string(), |e| range_ref(Range::new(CellRef::new(0, 0), e)));
        format!("<dimension ref=\"{r}\"/>")
    }

    fn merges(&self) -> Option<String> {
        let m = &self.wb.layout(self.id).merges;
        if m.is_empty() {
            return None;
        }
        Some(format!("<mergeCells count=\"{}\">{}</mergeCells>", m.len(), m.iter().map(|r| format!("<mergeCell ref=\"{}\"/>", range_ref(*r))).collect::<String>()))
    }

    fn filter(&self) -> Option<String> {
        let l = self.wb.layout(self.id);
        let f = l.filter.as_ref()?;
        // A table's own filter lives in the table part.
        if l.tables.iter().any(|t| t.range == f.range) {
            return None;
        }
        let mut out = format!("<autoFilter ref=\"{}\"", range_ref(f.range));
        if f.columns.is_empty() {
            out.push_str("/>");
            return Some(out);
        }
        out.push('>');
        for (col, allowed) in &f.columns {
            let blank = allowed.contains("");
            out.push_str(&format!("<filterColumn colId=\"{}\"><filters{}>", col - f.range.start.col, if blank { " blank=\"1\"" } else { "" }));
            for v in allowed.iter().filter(|v| !v.is_empty()) {
                out.push_str(&format!("<filter val=\"{}\"/>", esc(v)));
            }
            out.push_str("</filters></filterColumn>");
        }
        out.push_str("</autoFilter>");
        Some(out)
    }

    fn cond(&mut self) -> Vec<String> {
        let wb = self.wb;
        let mut out = Vec::new();
        for (priority, cf) in wb.layout(self.id).cond.iter().enumerate() {
            if let Some(x) = cond_xml(wb, self.styles, cf, priority + 1) {
                out.push(x);
            }
        }
        out
    }
}

fn cfvo(v: CfValue) -> String {
    match v {
        CfValue::Min => "<cfvo type=\"min\"/>".into(),
        CfValue::Max => "<cfvo type=\"max\"/>".into(),
        CfValue::Number(x) => format!("<cfvo type=\"num\" val=\"{}\"/>", precise(x)),
        CfValue::Percent(x) => format!("<cfvo type=\"percent\" val=\"{}\"/>", precise(x)),
        CfValue::Percentile(x) => format!("<cfvo type=\"percentile\" val=\"{}\"/>", precise(x)),
    }
}

fn cond_xml(wb: &Workbook, styles: &mut Styles, cf: &CondFormat, priority: usize) -> Option<String> {
    let first = cf.ranges.first()?;
    let sqref = cf.ranges.iter().map(|r| range_ref(*r)).collect::<Vec<_>>().join(" ");
    let top_left = first.start.to_string();
    let f = |e: &xl_engine::formula::Expr| esc(&prefix_future_functions(&e.to_source(0, 0, wb.sheets())));
    let stop = if cf.stop_if_true { " stopIfTrue=\"1\"" } else { "" };
    let quoted = |t: &str| format!("\"{}\"", t.replace('"', "\"\""));
    let rule = match &cf.rule {
        CfRule::Cell { op, a, b, style } => {
            let operator = match op {
                CfOp::Between => "between",
                CfOp::NotBetween => "notBetween",
                CfOp::Equal => "equal",
                CfOp::NotEqual => "notEqual",
                CfOp::Greater => "greaterThan",
                CfOp::Less => "lessThan",
                CfOp::GreaterEq => "greaterThanOrEqual",
                CfOp::LessEq => "lessThanOrEqual",
            };
            let second = b.as_ref().map_or(String::new(), |b| format!("<formula>{}</formula>", f(b)));
            format!("<cfRule type=\"cellIs\" dxfId=\"{}\" priority=\"{priority}\"{stop} operator=\"{operator}\"><formula>{}</formula>{second}</cfRule>", styles.dxf(style), f(a))
        }
        CfRule::Text { kind, text, style } => {
            let q = quoted(text);
            let (ty, op, formula) = match kind {
                TextMatch::Contains => ("containsText", "containsText", format!("NOT(ISERROR(SEARCH({q},{top_left})))")),
                TextMatch::NotContains => ("notContainsText", "notContains", format!("ISERROR(SEARCH({q},{top_left}))")),
                TextMatch::BeginsWith => ("beginsWith", "beginsWith", format!("LEFT({top_left},LEN({q}))={q}")),
                TextMatch::EndsWith => ("endsWith", "endsWith", format!("RIGHT({top_left},LEN({q}))={q}")),
            };
            format!("<cfRule type=\"{ty}\" dxfId=\"{}\" priority=\"{priority}\"{stop} operator=\"{op}\" text=\"{}\"><formula>{}</formula></cfRule>", styles.dxf(style), esc(text), esc(&formula))
        }
        CfRule::Top { n, percent, bottom, style } => format!(
            "<cfRule type=\"top10\" dxfId=\"{}\" priority=\"{priority}\"{stop} rank=\"{n}\"{}{}/>",
            styles.dxf(style),
            if *percent { " percent=\"1\"" } else { "" },
            if *bottom { " bottom=\"1\"" } else { "" }
        ),
        CfRule::Average { above, style } => format!("<cfRule type=\"aboveAverage\" dxfId=\"{}\" priority=\"{priority}\"{stop}{}/>", styles.dxf(style), if *above { "" } else { " aboveAverage=\"0\"" }),
        CfRule::Duplicates { unique, style } => format!("<cfRule type=\"{}\" dxfId=\"{}\" priority=\"{priority}\"{stop}/>", if *unique { "uniqueValues" } else { "duplicateValues" }, styles.dxf(style)),
        CfRule::Blanks { blanks, style } => format!(
            "<cfRule type=\"{}\" dxfId=\"{}\" priority=\"{priority}\"{stop}><formula>LEN(TRIM({top_left})){}</formula></cfRule>",
            if *blanks { "containsBlanks" } else { "notContainsBlanks" },
            styles.dxf(style),
            if *blanks { "=0" } else { "&gt;0" }
        ),
        CfRule::Formula { expr, style } => format!("<cfRule type=\"expression\" dxfId=\"{}\" priority=\"{priority}\"{stop}><formula>{}</formula></cfRule>", styles.dxf(style), f(expr)),
        CfRule::ColorScale { stops } => {
            let vos: String = stops.iter().map(|(v, _)| cfvo(*v)).collect();
            let colors: String = stops.iter().map(|(_, c)| format!("<color rgb=\"{}\"/>", rgb_attr(*c))).collect();
            format!("<cfRule type=\"colorScale\" priority=\"{priority}\"{stop}><colorScale>{vos}{colors}</colorScale></cfRule>")
        }
        CfRule::DataBar { color, min, max } => {
            format!("<cfRule type=\"dataBar\" priority=\"{priority}\"{stop}><dataBar>{}{}<color rgb=\"{}\"/></dataBar></cfRule>", cfvo(*min), cfvo(*max), rgb_attr(*color))
        }
    };
    Some(format!("<conditionalFormatting sqref=\"{sqref}\">{rule}</conditionalFormatting>"))
}

/// A sheet's `<dataValidations>` element.
fn validations_xml(wb: &Workbook, sheet: SheetId) -> Option<String> {
    use xl_engine::validation::{ErrorStyle, ListSource, ValidationKind, ValidationOp, ValidationRule};
    let list = &wb.layout(sheet).validations;
    if list.is_empty() {
        return None;
    }
    let f = |e: &xl_engine::formula::Expr| esc(&prefix_future_functions(&e.to_source(0, 0, wb.sheets())));
    let mut out = format!("<dataValidations count=\"{}\">", list.len());
    for v in list {
        let sqref = v.ranges.iter().map(|r| range_ref(*r)).collect::<Vec<_>>().join(" ");
        let (ty, op, formulas) = match &v.rule {
            ValidationRule::List(ListSource::Items(items)) => ("list", None, vec![format!("\"{}\"", esc(&items.join(",").replace('"', "\"\"")))]),
            ValidationRule::List(ListSource::Cells(e)) => ("list", None, vec![f(e)]),
            ValidationRule::Custom(e) => ("custom", None, vec![f(e)]),
            ValidationRule::Compare { kind, op, a, b } => {
                let ty = match kind {
                    ValidationKind::Whole => "whole",
                    ValidationKind::Decimal => "decimal",
                    ValidationKind::Date => "date",
                    ValidationKind::Time => "time",
                    ValidationKind::TextLength => "textLength",
                };
                let op = match op {
                    ValidationOp::Between => "between",
                    ValidationOp::NotBetween => "notBetween",
                    ValidationOp::Equal => "equal",
                    ValidationOp::NotEqual => "notEqual",
                    ValidationOp::Greater => "greaterThan",
                    ValidationOp::Less => "lessThan",
                    ValidationOp::GreaterEq => "greaterThanOrEqual",
                    ValidationOp::LessEq => "lessThanOrEqual",
                };
                (ty, Some(op), std::iter::once(f(a)).chain(b.as_deref().map(f)).collect())
            }
        };
        out.push_str(&format!("<dataValidation type=\"{ty}\""));
        if let Some(op) = op.filter(|o| *o != "between") {
            out.push_str(&format!(" operator=\"{op}\""));
        }
        match v.error_style {
            ErrorStyle::Stop => {}
            ErrorStyle::Warning => out.push_str(" errorStyle=\"warning\""),
            ErrorStyle::Information => out.push_str(" errorStyle=\"information\""),
        }
        if v.allow_blank {
            out.push_str(" allowBlank=\"1\"");
        }
        if ty == "list" && !v.dropdown {
            out.push_str(" showDropDown=\"1\"");
        }
        if let Some((t, b)) = &v.prompt {
            out.push_str(&format!(" showInputMessage=\"1\" promptTitle=\"{}\" prompt=\"{}\"", esc(t), esc(b)));
        }
        out.push_str(" showErrorMessage=\"1\"");
        if let Some((t, b)) = &v.error {
            out.push_str(&format!(" errorTitle=\"{}\" error=\"{}\"", esc(t), esc(b)));
        }
        out.push_str(&format!(" sqref=\"{sqref}\">"));
        for (i, formula) in formulas.iter().enumerate() {
            out.push_str(&format!("<formula{n}>{formula}</formula{n}>", n = i + 1));
        }
        out.push_str("</dataValidation>");
    }
    out.push_str("</dataValidations>");
    Some(out)
}

/// Frozen panes inside `<sheetViews>`: drop the old pane and its
/// selections, add ours to the first view.
fn patch_sheet_views(el: &str, freeze: (u32, u32)) -> Result<String, String> {
    use quick_xml::Writer;
    let mut r = xml(el.as_bytes());
    let mut w = Writer::new(Vec::new());
    let mut buf = Vec::new();
    let mut first_view = true;
    let (rows, cols) = freeze;
    let pane = if freeze == (0, 0) {
        String::new()
    } else {
        let top_left = CellRef::new(rows, cols).to_string();
        let active = match (rows > 0, cols > 0) {
            (true, true) => "bottomRight",
            (true, false) => "bottomLeft",
            _ => "topRight",
        };
        format!(
            "<pane{}{} topLeftCell=\"{top_left}\" activePane=\"{active}\" state=\"frozen\"/><selection pane=\"{active}\" activeCell=\"{top_left}\" sqref=\"{top_left}\"/>",
            if cols > 0 { format!(" xSplit=\"{cols}\"") } else { String::new() },
            if rows > 0 { format!(" ySplit=\"{rows}\"") } else { String::new() }
        )
    };
    let mut skip_depth = 0usize;
    loop {
        let ev = r.read_event_into(&mut buf).map_err(|e| e.to_string())?;
        let name = match &ev {
            Event::Start(e) | Event::Empty(e) => local(e.name().as_ref()).to_string(),
            _ => String::new(),
        };
        let has_pane_attr = matches!(&ev, Event::Start(e) | Event::Empty(e) if attr(e, "pane").is_some());
        match ev {
            Event::Eof => break,
            _ if skip_depth > 0 => {
                if matches!(ev, Event::Start(_)) {
                    skip_depth += 1;
                } else if matches!(ev, Event::End(_)) {
                    skip_depth -= 1;
                }
            }
            Event::Empty(_) if name == "pane" || (name == "selection" && has_pane_attr) => {}
            Event::Start(_) if name == "pane" || (name == "selection" && has_pane_attr) => skip_depth = 1,
            Event::Start(ref e) if name == "sheetView" && first_view => {
                first_view = false;
                w.write_event(Event::Start(e.clone())).map_err(|e| e.to_string())?;
                w.get_mut().extend_from_slice(pane.as_bytes());
            }
            Event::Empty(ref e) if name == "sheetView" && first_view => {
                first_view = false;
                let qname = e.name().as_ref().to_string();
                w.write_event(Event::Start(e.clone())).map_err(|e| e.to_string())?;
                w.get_mut().extend_from_slice(pane.as_bytes());
                w.get_mut().extend_from_slice(format!("</{qname}>").as_bytes());
            }
            other => w.write_event(other).map_err(|e| e.to_string())?,
        }
        buf.clear();
    }
    String::from_utf8(w.into_inner()).map_err(|e| e.to_string())
}

/// Per row, the original `<row>` element's attributes.
type RowAttrs = HashMap<u32, Vec<(String, String)>>;

/// `<row>` attributes and `<col>` styles of the original sheet.
fn original_rows_and_cols(doc: &[u8]) -> (RowAttrs, HashMap<u32, String>) {
    let (mut rows, mut cols) = (HashMap::new(), HashMap::new());
    let mut r = xml(doc);
    let mut buf = Vec::new();
    while let Ok(ev) = r.read_event_into(&mut buf) {
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => match local(e.name().as_ref()) {
                "row" => {
                    let attrs: Vec<(String, String)> = e
                        .attributes()
                        .with_checks(false)
                        .flatten()
                        .map(|a| (a.key.as_ref().to_string(), a.normalized_value(quick_xml::XmlVersion::Implicit1_0).map(|v| v.into_owned()).unwrap_or_default()))
                        .collect();
                    if let Some(n) = attrs.iter().find(|(k, _)| k == "r").and_then(|(_, v)| v.parse::<u32>().ok()) {
                        // Only rows carrying more than xl regenerates are worth keeping.
                        if attrs.iter().any(|(k, _)| !matches!(k.as_str(), "r" | "spans" | "ht" | "customHeight" | "hidden" | "outlineLevel" | "x14ac:dyDescent")) {
                            rows.insert(n - 1, attrs);
                        }
                    }
                }
                "col" => {
                    if let (Some(min), Some(max), Some(style)) = (attr(e, "min").and_then(|v| v.parse::<u32>().ok()), attr(e, "max").and_then(|v| v.parse::<u32>().ok()), attr(e, "style"))
                        && style != "0"
                        && max >= min
                        && max - min < 16_384
                    {
                        for c in min..=max {
                            cols.insert(c - 1, style.clone());
                        }
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    (rows, cols)
}

const X14_COND_EXT: &str = "{78C0D931-6437-407d-A8EE-F0AAD7539E65}";

/// Remove one `<ext uri="…">` from an `<extLst>` element.
fn drop_ext(el: &str, uri: &str) -> String {
    let Some(i) = el.find(uri) else { return el.to_string() };
    let start = el[..i].rfind("<").and_then(|s| el[..s + 1].rfind('<').map(|_| s)).unwrap_or(i);
    let start = el[..start + 1].rfind('<').unwrap_or(start);
    // Find this ext's end: the matching </…ext>.
    let mut depth = 0;
    let mut j = start;
    while j < el.len() {
        if el[j..].starts_with("</") {
            depth -= 1;
            if depth == 0 {
                let close = el[j..].find('>').map_or(el.len(), |k| j + k + 1);
                return format!("{}{}", &el[..start], &el[close..]);
            }
            j += 2;
            continue;
        }
        if el[j..].starts_with('<') && !el[j..].starts_with("<?") {
            let tag_end = el[j..].find('>').map_or(el.len(), |k| j + k);
            if !el[..tag_end].ends_with('/') {
                depth += 1;
            } else if depth == 0 {
                return format!("{}{}", &el[..start], &el[tag_end + 1..]);
            }
            j = tag_end + 1;
            continue;
        }
        j += 1;
    }
    el.to_string()
}

/// Rewrite the parts of a worksheet that changed.
fn patch_sheet(doc: &[u8], ctx: &mut SheetCtx, before: &SheetDigest, now: &SheetDigest) -> Result<Vec<u8>, String> {
    let mut replace: HashMap<&str, Option<String>> = HashMap::new();
    let text = String::from_utf8_lossy(doc).to_string();
    let (_, _, _, kids) = children(doc)?;
    let original = |name: &str| kids.iter().find(|k| k.local == name).map(|k| text[k.start..k.end].to_string());
    if now.data != before.data {
        replace.insert("sheetData", Some(ctx.sheet_data()));
        replace.insert("dimension", Some(ctx.dimension()));
    }
    if now.cols != before.cols {
        replace.insert("cols", ctx.cols());
    }
    if now.merges != before.merges {
        replace.insert("mergeCells", ctx.merges());
    }
    if now.filter != before.filter {
        replace.insert("autoFilter", ctx.filter());
    }
    if now.cond != before.cond {
        let all = ctx.cond().concat();
        replace.insert("conditionalFormatting", (!all.is_empty()).then_some(all));
        // Extension rules (data bar details, icon sets) belong to the old rules.
        if let Some(ext) = original("extLst")
            && ext.contains(X14_COND_EXT)
        {
            let cleaned = drop_ext(&ext, X14_COND_EXT);
            let empty = !cleaned.contains("<ext") && !cleaned.contains(":ext");
            replace.insert("extLst", if empty { None } else { Some(cleaned) });
        }
    }
    if ctx.validations_changed {
        replace.insert("dataValidations", validations_xml(ctx.wb, ctx.id));
    }
    if now.freeze != before.freeze {
        let freeze = ctx.wb.layout(ctx.id).freeze;
        let views = match original("sheetViews") {
            Some(v) => patch_sheet_views(&v, freeze)?,
            None => patch_sheet_views("<sheetViews><sheetView workbookViewId=\"0\"/></sheetViews>", freeze)?,
        };
        replace.insert("sheetViews", Some(views));
    }
    // Originals we re-insert (extLst, sheetViews) already carry their prefixes:
    // mark them raw so splice doesn't prefix them again.
    let raw: Vec<&str> = ["extLst", "sheetViews"].into_iter().filter(|n| replace.get(n).is_some_and(|v| v.is_some())).collect();
    let replace: HashMap<&str, Option<String>> = replace.into_iter().map(|(k, v)| (k, v.map(|s| if raw.contains(&k) { format!("\u{1}{s}\u{1}") } else { s }))).collect();
    splice_keeping_raw(doc, SHEET_ORDER, replace)
}

/// A whole worksheet part for a sheet that's new in xl.
fn new_sheet(ctx: &mut SheetCtx) -> String {
    let mut body = String::new();
    body.push_str(&ctx.dimension());
    let freeze = ctx.wb.layout(ctx.id).freeze;
    if freeze != (0, 0) {
        body.push_str(&patch_sheet_views("<sheetViews><sheetView workbookViewId=\"0\"/></sheetViews>", freeze).unwrap_or_default());
    }
    body.push_str("<sheetFormatPr defaultRowHeight=\"15\"/>");
    if let Some(c) = ctx.cols() {
        body.push_str(&c);
    }
    body.push_str(&ctx.sheet_data());
    if let Some(f) = ctx.filter() {
        body.push_str(&f);
    }
    if let Some(m) = ctx.merges() {
        body.push_str(&m);
    }
    body.push_str(&ctx.cond().concat());
    if let Some(v) = validations_xml(ctx.wb, ctx.id) {
        body.push_str(&v);
    }
    body.push_str("<pageMargins left=\"0.7\" right=\"0.7\" top=\"0.75\" bottom=\"0.75\" header=\"0.3\" footer=\"0.3\"/>");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">{body}</worksheet>"
    )
}

// ------------------------------------------------------------ package-level parts

const REL_COMMENTS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments";
const REL_VML: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/vmlDrawing";
const CT_COMMENTS: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.comments+xml";
const CT_VML: &str = "application/vnd.openxmlformats-officedocument.vmlDrawing";

/// A comments part for a sheet's notes.
fn comments_xml(notes: &BTreeMap<CellRef, xl_engine::Note>) -> String {
    let mut authors: Vec<&str> = Vec::new();
    for n in notes.values() {
        if !authors.contains(&n.author.as_str()) {
            authors.push(&n.author);
        }
    }
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<comments xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><authors>");
    for a in &authors {
        out.push_str(&format!("<author>{}</author>", esc(a)));
    }
    out.push_str("</authors><commentList>");
    for (cell, n) in notes {
        let id = authors.iter().position(|a| *a == n.author).unwrap_or(0);
        out.push_str(&format!("<comment ref=\"{cell}\" authorId=\"{id}\"><text><r><rPr><sz val=\"9\"/><color indexed=\"81\"/><rFont val=\"Tahoma\"/><family val=\"2\"/></rPr><t xml:space=\"preserve\">{}</t></r></text></comment>", esc(&n.text)));
    }
    out.push_str("</commentList></comments>");
    out
}

/// The legacy drawing (VML) Excel uses for note boxes.
fn notes_vml(notes: &BTreeMap<CellRef, xl_engine::Note>, idmap: u32) -> String {
    let mut out = format!(
        "<xml xmlns:v=\"urn:schemas-microsoft-com:vml\" xmlns:o=\"urn:schemas-microsoft-com:office:office\" xmlns:x=\"urn:schemas-microsoft-com:office:excel\"><o:shapelayout v:ext=\"edit\"><o:idmap v:ext=\"edit\" data=\"{idmap}\"/></o:shapelayout><v:shapetype id=\"_x0000_t202\" coordsize=\"21600,21600\" o:spt=\"202\" path=\"m,l,21600r21600,l21600,xe\"><v:stroke joinstyle=\"miter\"/><v:path gradientshapeok=\"t\" o:connecttype=\"rect\"/></v:shapetype>"
    );
    for (i, cell) in notes.keys().enumerate() {
        let (r, c) = (cell.row, cell.col);
        out.push_str(&format!(
            "<v:shape id=\"_x0000_s{}\" type=\"#_x0000_t202\" style=\"position:absolute;margin-left:59.25pt;margin-top:1.5pt;width:108pt;height:59.25pt;z-index:{};visibility:hidden\" fillcolor=\"#ffffe1\" o:insetmode=\"auto\"><v:fill color2=\"#ffffe1\"/><v:shadow on=\"t\" color=\"black\" obscured=\"t\"/><v:path o:connecttype=\"none\"/><v:textbox style=\"mso-direction-alt:auto\"><div style=\"text-align:left\"></div></v:textbox><x:ClientData ObjectType=\"Note\"><x:MoveWithCells/><x:SizeWithCells/><x:Anchor>{}, 15, {}, 2, {}, 15, {}, 16</x:Anchor><x:AutoFill>False</x:AutoFill><x:Row>{r}</x:Row><x:Column>{c}</x:Column></x:ClientData></v:shape>",
            idmap * 1024 + 1 + i as u32,
            i + 1,
            c + 1,
            r.saturating_sub(1),
            c + 3,
            r + 3
        ));
    }
    out.push_str("</xml>");
    out
}

const REL_HYPERLINK: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";

/// Rewrite a sheet's `<hyperlinks>` and its hyperlink relationships (all
/// replaced: they're only used by that element). Returns the new sheet XML.
fn write_links(wb: &Workbook, pkg: &Package, sheet: SheetId, sheet_part: &str, sheet_doc: Vec<u8>, replaced: &mut HashMap<String, Vec<u8>>, added_parts: &mut Vec<(String, Vec<u8>)>) -> Result<Vec<u8>, String> {
    let links = &wb.layout(sheet).links;
    let rels_path = crate::xlsx_charts::rels_of(sheet_part);
    let rels_doc = replaced.get(&rels_path).cloned().or_else(|| pkg.part(&rels_path));
    let mut el = String::new();
    let mut rels: Vec<(String, String)> = Vec::new();
    for (cell, link) in links {
        let tip = link.tooltip().map_or(String::new(), |t| format!(" tooltip=\"{}\"", esc(t)));
        match link {
            xl_engine::Link::Url { url, .. } => {
                let id = format!("rIdXlLink{}", rels.len() + 1);
                el.push_str(&format!("<hyperlink ref=\"{cell}\" r:id=\"{id}\"{tip}/>"));
                rels.push((id, url.clone()));
            }
            xl_engine::Link::Place { location, .. } => el.push_str(&format!("<hyperlink ref=\"{cell}\" location=\"{}\"{tip}/>", esc(location))),
        }
    }
    let new_rels: String = rels.iter().map(|(id, url)| format!("<Relationship Id=\"{id}\" Type=\"{REL_HYPERLINK}\" Target=\"{}\" TargetMode=\"External\"/>", esc(url))).collect();
    match rels_doc {
        Some(doc) => {
            let doc = expand_root(&doc);
            let (_, root_close, prefix, kids) = children(&doc)?;
            let text = String::from_utf8_lossy(&doc).to_string();
            let mut edits: Vec<(usize, usize, String)> = kids.iter().filter(|k| text[k.start..k.end].contains("/relationships/hyperlink\"")).map(|k| (k.start, k.end, String::new())).collect();
            edits.push((root_close, root_close, prefixed(&new_rels, &prefix)));
            replaced.insert(rels_path, apply_edits(&doc, edits));
        }
        None if !rels.is_empty() => added_parts.push((
            rels_path,
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{new_rels}</Relationships>").into_bytes(),
        )),
        None => {}
    }
    let doc = ensure_r_namespace(sheet_doc)?;
    let mut rep: HashMap<&str, Option<String>> = HashMap::new();
    rep.insert("hyperlinks", (!el.is_empty()).then(|| format!("<hyperlinks>{el}</hyperlinks>")));
    splice(&doc, SHEET_ORDER, rep)
}

/// Make sure a worksheet's root declares the relationships namespace (for r:id).
fn ensure_r_namespace(doc: Vec<u8>) -> Result<Vec<u8>, String> {
    let text = String::from_utf8_lossy(&doc).to_string();
    let (open_end, _, _, _) = children(&doc)?;
    let root_start = text[..open_end].rfind("<worksheet").or_else(|| text[..open_end].rfind(":worksheet").and_then(|i| text[..i].rfind('<'))).unwrap_or(0);
    if text[root_start..open_end].contains("xmlns:r=") {
        return Ok(doc);
    }
    let fixed = set_attrs(&text[root_start..open_end], &[("xmlns:r", Some("http://schemas.openxmlformats.org/officeDocument/2006/relationships".into()))]);
    Ok(apply_edits(&doc, vec![(root_start, open_end, fixed)]))
}

/// Put a sheet's notes into the package: rewrite its comments and note
/// drawing, or create them (with relationships and a `<legacyDrawing>`
/// link in the sheet). Returns the sheet XML if it had to change.
#[allow(clippy::too_many_arguments)]
fn write_notes(
    wb: &Workbook,
    pkg: &Package,
    sheet: SheetId,
    sheet_part: &str,
    sheet_doc: Vec<u8>,
    taken: &mut BTreeSet<String>,
    next_idmap: &mut u32,
    replaced: &mut HashMap<String, Vec<u8>>,
    added_parts: &mut Vec<(String, Vec<u8>)>,
    ct_adds: &mut Vec<(String, &'static str)>,
) -> Result<Option<Vec<u8>>, String> {
    let notes = &wb.layout(sheet).notes;
    let rels_path = crate::xlsx_charts::rels_of(sheet_part);
    let rels = match pkg.zip() {
        Ok(mut z) => read_rels(&mut z, &rels_path).unwrap_or_default(),
        Err(_) => HashMap::new(),
    };
    let comments = rels.values().find(|t| t.rsplit('/').next().is_some_and(|f| f.starts_with("comments"))).map(|t| crate::xlsx_charts::resolve(sheet_part, t));
    let vml = rels.values().find(|t| t.ends_with(".vml")).map(|t| crate::xlsx_charts::resolve(sheet_part, t));
    if let (Some(c), Some(v)) = (&comments, &vml) {
        let idmap = pkg.part(v).and_then(|b| String::from_utf8_lossy(&b).split("data=\"").nth(1).and_then(|s| s.split(['"', ',']).next()).and_then(|n| n.parse().ok())).unwrap_or(*next_idmap);
        replaced.insert(c.clone(), comments_xml(notes).into_bytes());
        replaced.insert(v.clone(), notes_vml(notes, idmap).into_bytes());
        return Ok(None);
    }
    if notes.is_empty() {
        return Ok(None);
    }
    let mut n = 1;
    while taken.contains(&format!("xl/comments{n}.xml")) || taken.contains(&format!("xl/drawings/vmlDrawing{n}.vml")) {
        n += 1;
    }
    let (cpart, vpart) = (format!("xl/comments{n}.xml"), format!("xl/drawings/vmlDrawing{n}.vml"));
    taken.insert(cpart.clone());
    taken.insert(vpart.clone());
    added_parts.push((cpart.clone(), comments_xml(notes).into_bytes()));
    added_parts.push((vpart.clone(), notes_vml(notes, *next_idmap).into_bytes()));
    *next_idmap += 1;
    ct_adds.push((format!("/{cpart}"), CT_COMMENTS));
    ct_adds.push((format!("/{vpart}"), CT_VML));
    // Relationships from the sheet (targets relative to xl/worksheets/).
    let adds = vec![("rIdXlNotes".to_string(), REL_COMMENTS, format!("../comments{n}.xml")), ("rIdXlVml".to_string(), REL_VML, format!("../drawings/vmlDrawing{n}.vml"))];
    if let Some(slot) = added_parts.iter_mut().find(|(p, _)| *p == rels_path) {
        // Created earlier in this save (by hyperlinks): extend it.
        slot.1 = patch_rels(&slot.1, &adds, &[])?;
    } else {
        match replaced.get(&rels_path).cloned().or_else(|| pkg.part(&rels_path)) {
        Some(doc) => {
            replaced.insert(rels_path.clone(), patch_rels(&doc, &adds, &[])?);
        }
        None => {
            let body: String = adds.iter().map(|(id, ty, t)| format!("<Relationship Id=\"{id}\" Type=\"{ty}\" Target=\"{t}\"/>")).collect();
            added_parts.push((rels_path.clone(), format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{body}</Relationships>").into_bytes()));
        }
        }
    }
    // Link the note drawing from the sheet; r:id needs the relationships namespace.
    let doc = ensure_r_namespace(sheet_doc)?;
    let mut rep: HashMap<&str, Option<String>> = HashMap::new();
    rep.insert("legacyDrawing", Some("<legacyDrawing r:id=\"rIdXlVml\"/>".into()));
    Ok(Some(splice(&doc, SHEET_ORDER, rep)?))
}

const REL_STYLES: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";
const CT_STYLES: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml";
/// What a package without a styles part gets before xl's formats are added.
const MINIMAL_STYLES: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><fonts count=\"1\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts><fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills><borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs><cellXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/></cellXfs><cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles></styleSheet>";

const REL_WORKSHEET: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet";
const REL_SST: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings";
const CT_WORKSHEET: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml";
const CT_SST: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml";

/// Add and remove relationships in a .rels part (by target).
fn patch_rels(doc: &[u8], add: &[(String, &str, String)], remove_targets: &[String]) -> Result<Vec<u8>, String> {
    let doc = &expand_root(doc);
    let (_, root_close, prefix, kids) = children(doc)?;
    let text = String::from_utf8_lossy(doc).to_string();
    let mut edits = Vec::new();
    for k in &kids {
        let el = &text[k.start..k.end];
        if remove_targets.iter().any(|t| el.contains(&format!("Target=\"{t}\"")) || el.contains(&format!("Target=\"/xl/{t}\""))) {
            edits.push((k.start, k.end, String::new()));
        }
    }
    let new: String = add.iter().map(|(id, ty, target)| format!("<Relationship Id=\"{id}\" Type=\"{ty}\" Target=\"{target}\"/>")).collect();
    edits.push((root_close, root_close, prefixed(&new, &prefix)));
    Ok(apply_edits(doc, edits))
}

fn patch_content_types(doc: &[u8], add: &[(String, &str)], remove: &[String]) -> Result<Vec<u8>, String> {
    let doc = &expand_root(doc);
    let (_, root_close, prefix, kids) = children(doc)?;
    let text = String::from_utf8_lossy(doc).to_string();
    let mut edits = Vec::new();
    for k in &kids {
        let el = &text[k.start..k.end];
        if remove.iter().any(|p| el.contains(&format!("PartName=\"{p}\""))) {
            edits.push((k.start, k.end, String::new()));
        }
    }
    let new: String = add.iter().filter(|(p, _)| !text.contains(&format!("PartName=\"{p}\""))).map(|(p, ct)| format!("<Override PartName=\"{p}\" ContentType=\"{ct}\"/>")).collect();
    edits.push((root_close, root_close, prefixed(&new, &prefix)));
    Ok(apply_edits(doc, edits))
}

/// Rebuild `<sheets>` (names, order, new sheets) and `<definedNames>`.
fn patch_workbook(doc: &[u8], wb: &Workbook, pkg: &Package, new_sheets: &[(SheetId, String)], names_changed: bool) -> Result<Vec<u8>, String> {
    let text = String::from_utf8_lossy(doc).to_string();
    let (_, _, _, kids) = children(doc)?;
    let sheets_el = kids.iter().find(|k| k.local == "sheets").map(|k| text[k.start..k.end].to_string()).unwrap_or_default();
    // Original <sheet> elements by relationship id, and the old sheet order.
    let mut by_rid: HashMap<String, String> = HashMap::new();
    let mut max_sheet_id = 0u32;
    let inner = format!("<r>{}</r>", inner_of(&sheets_el));
    if let Ok((_, _, _, list)) = children(inner.as_bytes()) {
        for k in list {
            let el = &inner[k.start..k.end];
            if let Ok((_, _, _, _)) = children(format!("<w>{el}</w>").as_bytes()) {
                let rid = el.split("r:id=\"").nth(1).or_else(|| el.split(":id=\"").nth(1)).and_then(|s| s.split('"').next()).unwrap_or("").to_string();
                if let Some(n) = el.split("sheetId=\"").nth(1).and_then(|s| s.split('"').next()).and_then(|v| v.parse::<u32>().ok()) {
                    max_sheet_id = max_sheet_id.max(n);
                }
                by_rid.insert(rid, el.to_string());
            }
        }
    }
    // Which rid each package sheet had (from the workbook rels).
    let mut zip = pkg.zip()?;
    let rels = read_rels(&mut zip, "xl/_rels/workbook.xml.rels")?;
    let rid_of_part: HashMap<String, String> = rels.iter().map(|(id, t)| (crate::xlsx_read::sheet_path(t), id.clone())).collect();
    let mut out = String::from("<sheets>");
    let mut new_iter = new_sheets.iter();
    for &id in wb.sheet_ids() {
        let name = wb.sheet_name(id);
        if let Some(ps) = pkg.sheets.iter().find(|s| s.id == id)
            && let Some(rid) = rid_of_part.get(&ps.part)
            && let Some(el) = by_rid.get(rid)
        {
            let (tag, _) = split_start_tag(el);
            out.push_str(&set_attrs(&strip_prefix_tag(tag), &[("name", Some(name.to_string()))]));
            continue;
        }
        if let Some((_, rid)) = new_iter.next() {
            max_sheet_id += 1;
            // Declared here too: some files only declare the namespace per element.
            out.push_str(&format!(
                "<sheet name=\"{}\" sheetId=\"{max_sheet_id}\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" r:id=\"{rid}\"/>",
                esc(name)
            ));
        }
    }
    out.push_str("</sheets>");
    let mut replace: HashMap<&str, Option<String>> = HashMap::new();
    replace.insert("sheets", Some(out));
    if names_changed || !new_sheets.is_empty() || wb.sheet_ids().iter().zip(&pkg.sheets).any(|(a, b)| *a != b.id) {
        // Built-in names (print areas…) stay, with their sheet index remapped.
        let old_order: Vec<SheetId> = pkg.sheets.iter().map(|s| s.id).collect();
        let mut names = String::new();
        if let Some(k) = kids.iter().find(|k| k.local == "definedNames") {
            let el = &text[k.start..k.end];
            let inner = format!("<r>{}</r>", inner_of(el));
            if let Ok((_, _, _, list)) = children(inner.as_bytes()) {
                for k in list {
                    let el = &inner[k.start..k.end];
                    if !el.contains("name=\"_xlnm.") {
                        continue;
                    }
                    let local_id = el.split("localSheetId=\"").nth(1).and_then(|s| s.split('"').next()).and_then(|v| v.parse::<usize>().ok());
                    let remapped = match local_id.map(|i| old_order.get(i).and_then(|sid| wb.sheet_ids().iter().position(|x| x == sid))) {
                        Some(Some(n)) => {
                            let (tag, rest) = split_start_tag(el);
                            format!("{}{}", set_attrs(&strip_prefix_tag(tag), &[("localSheetId", Some(n.to_string()))]), strip_prefix_close(rest))
                        }
                        Some(None) => continue,
                        None => {
                            let (tag, rest) = split_start_tag(el);
                            format!("{}{}", strip_prefix_tag(tag), strip_prefix_close(rest))
                        }
                    };
                    names.push_str(&remapped);
                }
            }
        }
        for d in wb.names().iter() {
            let local = d.scope.and_then(|s| wb.sheet_ids().iter().position(|x| *x == s)).map_or(String::new(), |i| format!(" localSheetId=\"{i}\""));
            names.push_str(&format!("<definedName name=\"{}\"{local}>{}</definedName>", esc(&d.name), esc(&prefix_future_functions(&wb.name_formula(d)))));
        }
        replace.insert("definedNames", (!names.is_empty()).then(|| format!("<definedNames>{names}</definedNames>")));
    }
    splice(doc, WORKBOOK_ORDER, replace)
}

fn strip_prefix_close(rest: &str) -> String {
    // Turn "...</x:definedName>" into "...</definedName>" (our splice re-prefixes).
    match rest.rfind("</") {
        Some(i) => {
            let tag = &rest[i + 2..];
            let name = tag.trim_end_matches('>');
            let name = name.rsplit(':').next().unwrap_or(name);
            format!("{}</{name}>", &rest[..i])
        }
        None => rest.to_string(),
    }
}

/// Re-anchor the `index`-th anchor of a drawing at a chart's place and size.
fn reanchor(doc: &[u8], index: usize, chart: &xl_engine::chart::Chart, wb: &Workbook, sheet: SheetId) -> Result<Vec<u8>, String> {
    const EMU: f64 = 9525.0;
    let (_, _, _, kids) = children(doc)?;
    let anchors: Vec<&Child> = kids.iter().filter(|k| matches!(k.local.as_str(), "twoCellAnchor" | "oneCellAnchor" | "absoluteAnchor")).collect();
    let Some(k) = anchors.get(index) else { return Err("a chart's anchor wasn't found".into()) };
    let text = String::from_utf8_lossy(doc).to_string();
    let el = &text[k.start..k.end];
    let prefix = el[1..].split([' ', '>']).next().and_then(|n| n.rfind(':').map(|i| n[..=i].to_string())).unwrap_or_default();
    // Excel pixels per column/row (hidden ones are zero wide).
    let l = wb.layout(sheet);
    let col_px = |c: u32| if l.hidden_cols.contains(&c) { 0.0 } else { wb.col_width(sheet, c).map_or(64.0, |px| to_excel_width_px(px) as f64) };
    let row_px = |r: u32| if l.hidden_rows.contains(&r) { 0.0 } else { wb.row_height(sheet, r).map_or(20.0, |px| to_excel_height(px) * 4.0 / 3.0) };
    let (c0, r0) = (chart.anchor.col, chart.anchor.row);
    let (x_off, y_off) = (to_excel_width_px(chart.offset.0) as f64, to_excel_height(chart.offset.1) * 4.0 / 3.0);
    let (w, h) = (to_excel_width_px(chart.size.0) as f64, to_excel_height(chart.size.1) * 4.0 / 3.0);
    let (mut c1, mut x_rest) = (c0, x_off + w);
    while c1 + 1 < xl_engine::MAX_COLS && x_rest >= col_px(c1) && col_px(c1) > 0.0 {
        x_rest -= col_px(c1);
        c1 += 1;
    }
    let (mut r1, mut y_rest) = (r0, y_off + h);
    while r1 + 1 < xl_engine::MAX_ROWS && y_rest >= row_px(r1) && row_px(r1) > 0.0 {
        y_rest -= row_px(r1);
        r1 += 1;
    }
    let marker = |name: &str, c: u32, cx: f64, r: u32, ry: f64| {
        format!("<{prefix}{name}><{prefix}col>{c}</{prefix}col><{prefix}colOff>{}</{prefix}colOff><{prefix}row>{r}</{prefix}row><{prefix}rowOff>{}</{prefix}rowOff></{prefix}{name}>", (cx * EMU).round() as i64, (ry * EMU).round() as i64)
    };
    let replace_el = |src: &str, name: &str, with: &str| -> String {
        let (open, close) = (format!("<{prefix}{name}"), format!("</{prefix}{name}>"));
        match (src.find(&open), src.find(&close)) {
            (Some(a), Some(b)) if b > a => format!("{}{}{}", &src[..a], with, &src[b + close.len()..]),
            _ => src.to_string(),
        }
    };
    let mut new = replace_el(el, "from", &marker("from", c0, x_off, r0, y_off));
    if k.local == "twoCellAnchor" {
        new = replace_el(&new, "to", &marker("to", c1, x_rest, r1, y_rest));
    } else {
        // One-cell anchors carry their size as an extent after the marker.
        let ext = format!("<{prefix}ext cx=\"{}\" cy=\"{}\"/>", (w * EMU).round() as i64, (h * EMU).round() as i64);
        if let Some(a) = new.find(&format!("<{prefix}ext "))
            && let Some(len) = new[a..].find("/>")
        {
            new = format!("{}{}{}", &new[..a], ext, &new[a + len + 2..]);
        }
    }
    Ok(apply_edits(doc, vec![(k.start, k.end, new)]))
}

/// Point a table part's range (and its filter's) at the table's new rows.
fn patch_table_part(doc: &[u8], range: Range) -> Vec<u8> {
    let text = String::from_utf8_lossy(doc).to_string();
    let r = range_ref(range);
    let mut out = text.clone();
    for tag in ["table", "autoFilter"] {
        for q in [format!("<{tag} "), format!(":{tag} ")] {
            if let Some(i) = out.find(&q) {
                let end = out[i..].find('>').map_or(out.len(), |k| i + k + 1);
                let el = out[i..end].to_string();
                let fixed = set_attrs(&el, &[("ref", Some(r.clone()))]);
                out = format!("{}{}{}", &out[..i], fixed, &out[end..]);
                break;
            }
        }
    }
    out.into_bytes()
}

// ------------------------------------------------------------ the save

pub(crate) fn write_patched(wb: &Workbook, pkg: &Package, path: &Path) -> Result<(), String> {
    let mut zip = pkg.zip()?;
    // Addresses in preserved parts, adjusted for inserted/deleted lines and renames.
    let rewritten = crate::xlsx_refs::rewrite_preserved(wb, pkg)?;
    let part = |name: &str| rewritten.get(name).cloned().or_else(|| pkg.part(name));
    let (xfs, dxfs, _) = crate::xlsx_read::read_styles(&mut zip)?;
    let styles_xml = pkg.part("xl/styles.xml");
    // Index 0 is the default format; a package without styles still gets one.
    // An empty stylesheet is treated like a missing one (rebuilt from the minimal one).
    let has_formats = !xfs.is_empty();
    let xfs = if xfs.is_empty() { vec![Xf::default()] } else { xfs };
    let mut styles = Styles::new(xfs, dxfs, styles_xml.as_deref());
    let shared = crate::xlsx_read::read_shared_strings(&mut zip)?;
    let mut strings = Strings::new(&shared);
    let mut replaced: HashMap<String, Vec<u8>> = HashMap::new();
    let mut added_parts: Vec<(String, Vec<u8>)> = Vec::new();
    let mut removed: BTreeSet<String> = BTreeSet::new();
    let mut any_data = false;

    for s in &pkg.sheets {
        if !wb.sheet_ids().contains(&s.id) {
            continue;
        }
        let now = sheet_digest(wb, s.id);
        let before = s.digest;
        let validations_changed = pkg.validations_changed(wb, s.id);
        if (now.data, now.cols, now.merges, now.cond, now.filter, now.freeze) == (before.data, before.cols, before.merges, before.cond, before.filter, before.freeze) && !validations_changed {
            continue;
        }
        any_data |= now.data != before.data;
        let doc = part(&s.part).ok_or_else(|| format!("missing part {}", s.part))?;
        let same_structure = wb.structure_version(s.id) == s.structure;
        let (row_attrs, col_styles) = if same_structure { original_rows_and_cols(&doc) } else { Default::default() };
        let mut ctx = SheetCtx {
            wb,
            id: s.id,
            styles: &mut styles,
            strings: &mut strings,
            original_xfs: pkg.cell_xfs.get(&s.id).filter(|_| same_structure),
            extras: pkg.cell_extras.get(&s.id).filter(|_| same_structure),
            row_attrs,
            col_styles,
            validations_changed,
        };
        let patched = patch_sheet(&doc, &mut ctx, &before, &now)?;
        replaced.insert(s.part.clone(), patched);
    }
    // Tables whose row extent changed.
    for s in pkg.sheets.iter().filter(|s| wb.sheet_ids().contains(&s.id)) {
        if sheet_digest(wb, s.id).tables == s.digest.tables {
            continue;
        }
        let rels = read_rels(&mut zip, &crate::xlsx_charts::rels_of(&s.part))?;
        for target in rels.values().filter(|t| t.contains("tables/")) {
            let part = crate::xlsx_charts::resolve(&s.part, target);
            let Some(doc) = pkg.part(&part) else { continue };
            let name = original_tables(pkg, s).and_then(|ts| {
                let text = String::from_utf8_lossy(&doc).to_string();
                ts.into_iter().map(|t| t.0).find(|n| text.contains(&format!("displayName=\"{n}\"")) || text.contains(&format!("name=\"{n}\"")))
            });
            if let Some(t) = name.and_then(|n| wb.layout(s.id).tables.iter().find(|t| t.name.eq_ignore_ascii_case(&n))) {
                replaced.insert(part, patch_table_part(&doc, t.range));
            }
        }
    }
    // Sheets new in xl.
    let existing: BTreeSet<String> = zip.file_names().flatten().map(|n| n.into_owned()).collect();
    let mut new_sheets: Vec<(SheetId, String)> = Vec::new();
    let mut new_sheet_parts: Vec<(SheetId, String)> = Vec::new();
    let mut rel_adds: Vec<(String, &str, String)> = Vec::new();
    let mut ct_adds: Vec<(String, &'static str)> = Vec::new();
    let mut n = 1;
    for &id in wb.sheet_ids() {
        if pkg.sheets.iter().any(|s| s.id == id) {
            continue;
        }
        while existing.contains(&format!("xl/worksheets/sheet{n}.xml")) || added_parts.iter().any(|(p, _)| *p == format!("xl/worksheets/sheet{n}.xml")) {
            n += 1;
        }
        let part = format!("xl/worksheets/sheet{n}.xml");
        let mut ctx = SheetCtx { wb, id, styles: &mut styles, strings: &mut strings, original_xfs: None, extras: None, row_attrs: HashMap::new(), col_styles: HashMap::new(), validations_changed: true };
        added_parts.push((part.clone(), new_sheet(&mut ctx).into_bytes()));
        new_sheet_parts.push((id, part.clone()));
        let rid = format!("rIdXl{n}");
        rel_adds.push((rid.clone(), REL_WORKSHEET, format!("worksheets/sheet{n}.xml")));
        ct_adds.push((format!("/{part}"), CT_WORKSHEET));
        new_sheets.push((id, rid));
        any_data = true;
    }
    // Shared strings: append, or create the part.
    // Hyperlinks that changed (or appeared on new sheets).
    let link_sheets: Vec<(SheetId, String)> = pkg
        .sheets
        .iter()
        .filter(|s| wb.sheet_ids().contains(&s.id) && pkg.links_changed(wb, s.id))
        .map(|s| (s.id, s.part.clone()))
        .chain(new_sheet_parts.iter().filter(|(id, _)| !wb.layout(*id).links.is_empty()).cloned())
        .collect();
    for (id, sheet_part) in link_sheets {
        let doc = replaced
            .get(&sheet_part)
            .cloned()
            .or_else(|| added_parts.iter().find(|(p, _)| *p == sheet_part).map(|(_, b)| b.clone()))
            .or_else(|| part(&sheet_part))
            .ok_or_else(|| format!("missing {sheet_part}"))?;
        let new_doc = write_links(wb, pkg, id, &sheet_part, doc, &mut replaced, &mut added_parts)?;
        if let Some(slot) = added_parts.iter_mut().find(|(p, _)| *p == sheet_part) {
            slot.1 = new_doc;
        } else {
            replaced.insert(sheet_part, new_doc);
        }
    }
    // Notes that changed (or appeared on new sheets).
    let mut taken: BTreeSet<String> = existing.clone();
    taken.extend(added_parts.iter().map(|(p, _)| p.clone()));
    let mut next_idmap = existing
        .iter()
        .filter(|n| n.ends_with(".vml"))
        .filter_map(|n| pkg.part(n))
        .filter_map(|b| String::from_utf8_lossy(&b).split("data=\"").nth(1).and_then(|s| s.split(['"', ',']).next()).and_then(|n| n.parse::<u32>().ok()))
        .max()
        .unwrap_or(0)
        + 1;
    let note_sheets: Vec<(SheetId, String)> = pkg
        .sheets
        .iter()
        .filter(|s| wb.sheet_ids().contains(&s.id) && pkg.notes_changed(wb, s.id))
        .map(|s| (s.id, s.part.clone()))
        .chain(new_sheet_parts.iter().filter(|(id, _)| !wb.layout(*id).notes.is_empty()).cloned())
        .collect();
    for (id, sheet_part) in note_sheets {
        let doc = replaced
            .get(&sheet_part)
            .cloned()
            .or_else(|| added_parts.iter().find(|(p, _)| *p == sheet_part).map(|(_, b)| b.clone()))
            .or_else(|| part(&sheet_part))
            .ok_or_else(|| format!("missing {sheet_part}"))?;
        if let Some(new_doc) = write_notes(wb, pkg, id, &sheet_part, doc, &mut taken, &mut next_idmap, &mut replaced, &mut added_parts, &mut ct_adds)? {
            if let Some(slot) = added_parts.iter_mut().find(|(p, _)| *p == sheet_part) {
                slot.1 = new_doc;
            } else {
                replaced.insert(sheet_part, new_doc);
            }
        }
    }
    if !strings.added.is_empty() {
        match pkg.part("xl/sharedStrings.xml") {
            Some(doc) => {
                let doc = expand_root(&doc);
                let (_, root_close, prefix, _) = children(&doc)?;
                let text = String::from_utf8_lossy(&doc).to_string();
                let (tag_end, _, _, _) = children(&doc)?;
                let root_start = text[..tag_end].rfind('<').unwrap_or(0);
                let tag = &text[root_start..tag_end];
                let total = strings.count + strings.added.len();
                let new_tag = set_attrs(tag, &[("uniqueCount", Some(total.to_string())), ("count", None)]);
                let edits = vec![(root_start, tag_end, new_tag), (root_close, root_close, prefixed(&strings.items(), &prefix))];
                replaced.insert("xl/sharedStrings.xml".into(), apply_edits(&doc, edits));
            }
            None => {
                let body = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<sst xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" uniqueCount=\"{}\">{}</sst>",
                    strings.added.len(),
                    strings.items()
                );
                added_parts.push(("xl/sharedStrings.xml".into(), body.into_bytes()));
                rel_adds.push(("rIdXlSst".into(), REL_SST, "sharedStrings.xml".into()));
                ct_adds.push(("/xl/sharedStrings.xml".into(), CT_SST));
            }
        }
    }
    match styles_xml.as_deref().filter(|_| has_formats) {
        Some(doc) => {
            if let Some(patched) = styles.patch(doc)? {
                replaced.insert("xl/styles.xml".into(), patched);
            }
        }
        None => {
            if let Some(created) = styles.patch(MINIMAL_STYLES.as_bytes())? {
                if styles_xml.is_some() {
                    replaced.insert("xl/styles.xml".into(), created);
                } else {
                    added_parts.push(("xl/styles.xml".into(), created));
                    rel_adds.push(("rIdXlStyles".into(), REL_STYLES, "styles.xml".into()));
                    ct_adds.push(("/xl/styles.xml".into(), CT_STYLES));
                }
            }
        }
    }
    // Excel rebuilds the calculation chain; a stale one makes it repair the file.
    let mut rel_removes = Vec::new();
    let mut ct_removes = Vec::new();
    if any_data && existing.contains("xl/calcChain.xml") {
        removed.insert("xl/calcChain.xml".into());
        rel_removes.push("calcChain.xml".to_string());
        ct_removes.push("/xl/calcChain.xml".to_string());
    }
    let names_changed = names_digest(wb) != pkg.names;
    let renamed = pkg.sheets.iter().any(|s| wb.sheet_ids().contains(&s.id) && wb.sheet_name(s.id) != s.name);
    let reordered = wb.sheet_ids().iter().filter(|id| pkg.sheets.iter().any(|s| s.id == **id)).zip(pkg.sheets.iter().map(|s| &s.id)).any(|(a, b)| a != b);
    if names_changed || renamed || reordered || !new_sheets.is_empty() {
        let doc = part("xl/workbook.xml").ok_or("missing xl/workbook.xml")?;
        replaced.insert("xl/workbook.xml".into(), patch_workbook(&doc, wb, pkg, &new_sheets, names_changed)?);
    }
    // Charts moved or resized by hand.
    for s in pkg.sheets.iter().filter(|s| wb.sheet_ids().contains(&s.id)) {
        for (i, drawing, anchor) in pkg.moved_charts(wb, s.id).unwrap_or_default() {
            let Some(doc) = replaced.get(&drawing).cloned().or_else(|| part(&drawing)) else { continue };
            replaced.insert(drawing, reanchor(&doc, anchor, &wb.layout(s.id).charts[i], wb, s.id)?);
        }
    }
    // Rewritten parts not otherwise changed.
    for (name, bytes) in rewritten {
        replaced.entry(name).or_insert(bytes);
    }
    if !rel_adds.is_empty() || !rel_removes.is_empty() {
        let doc = pkg.part("xl/_rels/workbook.xml.rels").ok_or("missing workbook relationships")?;
        replaced.insert("xl/_rels/workbook.xml.rels".into(), patch_rels(&doc, &rel_adds, &rel_removes)?);
    }
    if !ct_adds.is_empty() || !ct_removes.is_empty() {
        let doc = pkg.part("[Content_Types].xml").ok_or("missing [Content_Types].xml")?;
        replaced.insert("[Content_Types].xml".into(), patch_content_types(&doc, &ct_adds, &ct_removes)?);
    }

    // Write: original entries in order (raw-copied when untouched), then new parts.
    let tmp = path.with_extension("xl-saving");
    {
        let file = File::create(&tmp).map_err(|e| e.to_string())?;
        let mut out = zip::ZipWriter::new(BufWriter::new(file));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for i in 0..zip.len() {
            let name = zip.by_index_raw(i).map_err(|e| e.to_string())?.name().map_err(|e| e.to_string())?.into_owned();
            if removed.contains(&name) {
                continue;
            }
            match replaced.get(&name) {
                Some(bytes) => {
                    out.start_file(name.as_str(), opts).map_err(|e| e.to_string())?;
                    out.write_all(bytes).map_err(|e| e.to_string())?;
                }
                None => out.raw_copy_file(zip.by_index_raw(i).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
            }
        }
        for (name, bytes) in &added_parts {
            out.start_file(name.as_str(), opts).map_err(|e| e.to_string())?;
            out.write_all(bytes).map_err(|e| e.to_string())?;
        }
        out.finish().map_err(|e| e.to_string())?.flush().map_err(|e| e.to_string())?;
    }
    // Replace the file only once the new one is complete.
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splices_children_in_schema_order() {
        let doc = br#"<worksheet a="1"><sheetData><row r="1"/></sheetData><pageMargins left="1"/><extLst><ext/></extLst></worksheet>"#;
        let mut rep = HashMap::new();
        rep.insert("sheetData", Some("<sheetData/>".to_string()));
        rep.insert("mergeCells", Some("<mergeCells count=\"1\"><mergeCell ref=\"A1:B1\"/></mergeCells>".to_string()));
        rep.insert("dimension", Some("<dimension ref=\"A1\"/>".to_string()));
        let out = String::from_utf8(splice(doc, SHEET_ORDER, rep).unwrap()).unwrap();
        assert_eq!(out, r#"<worksheet a="1"><dimension ref="A1"/><sheetData/><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells><pageMargins left="1"/><extLst><ext/></extLst></worksheet>"#);
    }

    #[test]
    fn prefixes_generated_xml() {
        assert_eq!(prefixed("<a b=\"<x>\"><c/></a>", "x:"), "<x:a b=\"<x>\"><x:c/></x:a>");
        assert_eq!(set_attrs("<sst count=\"3\" uniqueCount=\"2\">", &[("uniqueCount", Some("5".into())), ("count", None)]), "<sst uniqueCount=\"5\">");
        assert_eq!(strip_prefix_tag("<x:fonts count=\"1\">"), "<fonts count=\"1\">");
    }

    #[test]
    fn frozen_panes_replace_old_ones() {
        let v = r#"<sheetViews><sheetView tabSelected="1" workbookViewId="0"><pane ySplit="1" topLeftCell="A2" activePane="bottomLeft" state="frozen"/><selection pane="bottomLeft" activeCell="A2" sqref="A2"/></sheetView></sheetViews>"#;
        let out = patch_sheet_views(v, (0, 2)).unwrap();
        assert!(out.contains(r#"<pane xSplit="2" topLeftCell="C1" activePane="topRight" state="frozen"/>"#), "{out}");
        assert!(!out.contains("bottomLeft"));
        let none = patch_sheet_views(v, (0, 0)).unwrap();
        assert_eq!(none, r#"<sheetViews><sheetView tabSelected="1" workbookViewId="0"></sheetView></sheetViews>"#);
    }
}
