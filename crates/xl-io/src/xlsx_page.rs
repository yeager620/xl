//! Page setup in xlsx files: reading `<pageSetup>`, `<pageMargins>`,
//! `<printOptions>`, `<headerFooter>`, `<rowBreaks>`/`<colBreaks>`,
//! `<sheetPr><pageSetUpPr fitToPage>` and the `_xlnm.Print_Area` /
//! `_xlnm.Print_Titles` names; writing them in fresh saves; and in patched
//! saves regenerating them only when they changed (attributes xl doesn't
//! model, like the printer-settings relationship, are kept).

use std::collections::{BTreeSet, HashMap};

use quick_xml::XmlVersion;
use quick_xml::events::Event;
use xl_engine::page::{Orientation, PageSetup};
use xl_engine::{CellRef, Range, SheetId, StructOp, Workbook, col_name, parse_col, quote_sheet_name};

use crate::package::Package;
use crate::xlsx_patch::{SHEET_ORDER, WORKBOOK_ORDER, children, esc, prefixed, set_attrs, splice_keeping_raw, split_start_tag, strip_prefix_tag};
use crate::xlsx_read::{attr, local, push_entity, xml};

// ------------------------------------------------------------------ read

fn truthy(v: Option<String>) -> Option<bool> {
    v.map(|v| v == "1" || v == "true")
}

/// The byte ranges of a worksheet worth scanning for page setup: before
/// `<sheetData>` (for `<sheetPr>`) and after it (everything else), so huge
/// sheets aren't parsed twice.
fn outside_sheet_data(bytes: &[u8]) -> (&[u8], &[u8]) {
    let find = |needle: &[u8]| bytes.windows(needle.len()).position(|w| w == needle);
    let rfind = |needle: &[u8]| bytes.windows(needle.len()).rposition(|w| w == needle);
    match (find(b"sheetData"), rfind(b"sheetData>")) {
        (Some(a), Some(b)) if b >= a => (&bytes[..a.saturating_sub(1)], &bytes[b + "sheetData>".len()..]),
        _ => (bytes, &[]),
    }
}

/// A sheet's page setup from its worksheet XML (all but print area and titles).
pub(crate) fn read_page(bytes: &[u8]) -> PageSetup {
    let mut p = PageSetup::default();
    let (head, tail) = outside_sheet_data(bytes);
    let mut fit_to_page = false;
    let mut fit = (1u16, 1u16);
    for part in [head, tail] {
        let mut r = xml(part);
        let mut buf = Vec::new();
        let mut text: Option<(String, String)> = None;
        let mut breaks: Option<bool> = None;
        while let Ok(ev) = r.read_event_into(&mut buf) {
            match ev {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let empty = matches!(ev, Event::Empty(_));
                    match local(e.name().as_ref()) {
                        "pageSetUpPr" => fit_to_page = truthy(attr(e, "fitToPage")).unwrap_or(false),
                        "printOptions" => {
                            p.gridlines = truthy(attr(e, "gridLines")).unwrap_or(false);
                            p.headings = truthy(attr(e, "headings")).unwrap_or(false);
                            p.center_h = truthy(attr(e, "horizontalCentered")).unwrap_or(false);
                            p.center_v = truthy(attr(e, "verticalCentered")).unwrap_or(false);
                        }
                        "pageMargins" => {
                            let m = &mut p.margins;
                            for (k, slot) in [("left", &mut m.left), ("right", &mut m.right), ("top", &mut m.top), ("bottom", &mut m.bottom), ("header", &mut m.header), ("footer", &mut m.footer)] {
                                if let Some(v) = attr(e, k).and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite() && *v >= 0.0) {
                                    *slot = v;
                                }
                            }
                        }
                        "pageSetup" => {
                            if let Some(v) = attr(e, "paperSize").and_then(|v| v.parse::<u16>().ok()).filter(|v| *v > 0) {
                                p.paper = v;
                            }
                            if let Some(v) = attr(e, "scale").and_then(|v| v.parse::<u16>().ok()) {
                                p.scale = v.clamp(10, 400);
                            }
                            p.orientation = if attr(e, "orientation").as_deref() == Some("landscape") { Orientation::Landscape } else { Orientation::Portrait };
                            p.over_then_down = attr(e, "pageOrder").as_deref() == Some("overThenDown");
                            p.black_and_white = truthy(attr(e, "blackAndWhite")).unwrap_or(false);
                            fit.0 = attr(e, "fitToWidth").and_then(|v| v.parse().ok()).unwrap_or(1);
                            fit.1 = attr(e, "fitToHeight").and_then(|v| v.parse().ok()).unwrap_or(1);
                            if truthy(attr(e, "useFirstPageNumber")).unwrap_or(false) {
                                p.first_page = attr(e, "firstPageNumber").and_then(|v| v.parse::<i64>().ok()).map(|v| v.clamp(0, u32::MAX as i64) as u32);
                            }
                        }
                        name @ ("oddHeader" | "oddFooter") if !empty => text = Some((name.to_string(), String::new())),
                        "rowBreaks" => breaks = Some(true),
                        "colBreaks" => breaks = Some(false),
                        "brk" => {
                            if let (Some(rows), Some(id)) = (breaks, attr(e, "id").and_then(|v| v.parse::<u32>().ok()))
                                && truthy(attr(e, "man")).unwrap_or(false)
                                && id > 0
                            {
                                if rows { p.row_breaks.insert(id) } else { p.col_breaks.insert(id) };
                            }
                        }
                        _ => {}
                    }
                }
                Event::Text(t) => {
                    if let Some((_, s)) = text.as_mut() {
                        s.push_str(&t.xml_content(XmlVersion::Implicit1_0));
                    }
                }
                Event::GeneralRef(g) => {
                    if let Some((_, s)) = text.as_mut() {
                        push_entity(s, &g);
                    }
                }
                Event::End(ref e) => match local(e.name().as_ref()) {
                    "oddHeader" | "oddFooter" => {
                        if let Some((name, s)) = text.take() {
                            if name == "oddHeader" { p.header = s } else { p.footer = s }
                        }
                    }
                    "rowBreaks" | "colBreaks" => breaks = None,
                    _ => {}
                },
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
    }
    if fit_to_page {
        p.fit = Some(fit);
    }
    p
}

/// Strip a sheet qualifier ("Sheet1!" or "'My sheet'!") from a reference.
fn unqualified(s: &str) -> &str {
    let s = s.trim();
    match s.rfind('!') {
        Some(i) => &s[i + 1..],
        None => s,
    }
}

/// The parts of one area reference: "$A$1:$D$9", "$1:$2", "$A:$B", "$C$3".
enum Area {
    Cells(Range),
    Rows(u32, u32),
    Cols(u32, u32),
}

fn parse_area(s: &str) -> Option<Area> {
    let s = unqualified(s).replace('$', "");
    let (a, b) = s.split_once(':').unwrap_or((&s, &s));
    let (a, b) = (a.trim(), b.trim());
    if let (Ok(x), Ok(y)) = (a.parse::<u32>(), b.parse::<u32>()) {
        return (x > 0 && y > 0).then(|| Area::Rows(x.min(y) - 1, x.max(y) - 1));
    }
    if a.chars().all(|c| c.is_ascii_alphabetic()) && b.chars().all(|c| c.is_ascii_alphabetic()) {
        let (x, y) = (parse_col(a)?, parse_col(b)?);
        return Some(Area::Cols(x.min(y), x.max(y)));
    }
    Some(Area::Cells(Range::new(CellRef::parse(a)?, CellRef::parse(b)?)))
}

/// Apply a `_xlnm.Print_Area` or `_xlnm.Print_Titles` defined name.
/// Several print areas: xl keeps the first.
pub(crate) fn apply_print_name(p: &mut PageSetup, name: &str, formula: &str) {
    let formula = formula.trim().trim_start_matches('=');
    // Split at commas outside quotes (sheet names may hold commas).
    let mut parts = Vec::new();
    let (mut cur, mut quoted) = (String::new(), false);
    for c in formula.chars() {
        match c {
            '\'' => {
                quoted = !quoted;
                cur.push(c);
            }
            ',' if !quoted => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    if name.eq_ignore_ascii_case("_xlnm.Print_Area") {
        p.print_area = parts.iter().find_map(|s| match parse_area(s)? {
            Area::Cells(r) => Some(r),
            Area::Rows(a, b) => Some(Range::new(CellRef::new(a, 0), CellRef::new(b, xl_engine::MAX_COLS - 1))),
            Area::Cols(a, b) => Some(Range::new(CellRef::new(0, a), CellRef::new(xl_engine::MAX_ROWS - 1, b))),
        });
    } else if name.eq_ignore_ascii_case("_xlnm.Print_Titles") {
        for s in &parts {
            match parse_area(s) {
                Some(Area::Rows(a, b)) => p.title_rows = Some((a, b)),
                Some(Area::Cols(a, b)) => p.title_cols = Some((a, b)),
                // Whole-width / whole-height cell ranges mean the same.
                Some(Area::Cells(r)) if r.start.col == 0 && r.end.col >= xl_engine::MAX_COLS - 1 => p.title_rows = Some((r.start.row, r.end.row)),
                Some(Area::Cells(r)) if r.start.row == 0 && r.end.row >= xl_engine::MAX_ROWS - 1 => p.title_cols = Some((r.start.col, r.end.col)),
                _ => {}
            }
        }
    }
}

// ------------------------------------------------------------------ names

fn area_text(r: Range) -> String {
    if r.start.col == 0 && r.end.col >= xl_engine::MAX_COLS - 1 {
        return format!("${}:${}", r.start.row + 1, r.end.row + 1);
    }
    if r.start.row == 0 && r.end.row >= xl_engine::MAX_ROWS - 1 {
        return format!("${}:${}", col_name(r.start.col), col_name(r.end.col));
    }
    let cell = |c: CellRef| format!("${}${}", col_name(c.col), c.row + 1);
    if r.start == r.end { cell(r.start) } else { format!("{}:{}", cell(r.start), cell(r.end)) }
}

/// The print area and print titles formulas of a sheet (without '=').
pub(crate) fn print_names(p: &PageSetup, sheet_name: &str) -> (Option<String>, Option<String>) {
    let q = quote_sheet_name(sheet_name);
    let area = p.print_area.map(|r| format!("{q}!{}", area_text(r)));
    let mut titles = Vec::new();
    if let Some((a, b)) = p.title_cols {
        titles.push(format!("{q}!${}:${}", col_name(a), col_name(b)));
    }
    if let Some((a, b)) = p.title_rows {
        titles.push(format!("{q}!${}:${}", a + 1, b + 1));
    }
    (area, (!titles.is_empty()).then(|| titles.join(",")))
}

// ------------------------------------------------------------------ fresh

/// Page setup for a sheet written by rust_xlsxwriter.
pub(crate) fn write_fresh(ws: &mut rust_xlsxwriter::Worksheet, p: &PageSetup) -> Result<(), rust_xlsxwriter::XlsxError> {
    if p.is_default() {
        return Ok(());
    }
    if p.orientation == Orientation::Landscape {
        ws.set_landscape();
    }
    if p.paper != 1 && p.paper <= u8::MAX as u16 {
        ws.set_paper_size(p.paper as u8);
    }
    let m = &p.margins;
    ws.set_margins(m.left, m.right, m.top, m.bottom, m.header, m.footer);
    match p.fit {
        Some((w, h)) => {
            ws.set_print_fit_to_pages(w, h);
        }
        None if p.scale != 100 => {
            ws.set_print_scale(p.scale);
        }
        None => {}
    }
    if let Some(n) = p.first_page {
        ws.set_print_first_page_number(n.min(u16::MAX as u32) as u16);
    }
    ws.set_print_gridlines(p.gridlines);
    ws.set_print_headings(p.headings);
    ws.set_print_center_horizontally(p.center_h);
    ws.set_print_center_vertically(p.center_v);
    ws.set_print_black_and_white(p.black_and_white);
    if p.over_then_down {
        ws.set_page_order(false);
    }
    if !p.header.is_empty() {
        ws.set_header(&p.header);
    }
    if !p.footer.is_empty() {
        ws.set_footer(&p.footer);
    }
    let rows: Vec<u32> = p.row_breaks.iter().copied().take(1023).collect();
    if !rows.is_empty() {
        ws.set_page_breaks(&rows)?;
    }
    let cols: Vec<u32> = p.col_breaks.iter().copied().take(1023).collect();
    if !cols.is_empty() {
        ws.set_vertical_page_breaks(&cols)?;
    }
    if let Some(r) = p.print_area {
        ws.set_print_area(r.start.row, r.start.col as u16, r.end.row, r.end.col as u16)?;
    }
    if let Some((a, b)) = p.title_rows {
        ws.set_repeat_rows(a, b)?;
    }
    if let Some((a, b)) = p.title_cols {
        ws.set_repeat_columns(a as u16, b as u16)?;
    }
    Ok(())
}

// ------------------------------------------------------------------ patch

/// A sheet's page setup as it should be now if only the structural edits
/// since loading touched it.
fn expected(wb: &Workbook, pkg: &Package, sheet: SheetId) -> PageSetup {
    let mut p = pkg.pages.get(&sheet).cloned().unwrap_or_default();
    for op in wb.structure_log().iter().skip(pkg.log_len) {
        if let StructOp::Shift(s) = op
            && s.sheet() == sheet
        {
            p.apply_shift(*s);
        }
    }
    p
}

/// Did the page setup kept in the worksheet part change (beyond moving with lines)?
pub(crate) fn elements_changed(wb: &Workbook, pkg: &Package, sheet: SheetId) -> bool {
    expected(wb, pkg, sheet).without_names() != wb.layout(sheet).page.without_names()
}

/// Did the print area or titles change (beyond moving with lines)?
pub(crate) fn names_changed(wb: &Workbook, pkg: &Package, sheet: SheetId) -> bool {
    expected(wb, pkg, sheet).names_part() != wb.layout(sheet).page.names_part()
}

fn bool_attr(on: bool) -> Option<String> {
    on.then(|| "1".to_string())
}

fn num(v: f64) -> String {
    xl_engine::precise((v * 1e6).round() / 1e6)
}

/// The original element (prefix stripped from its start tag) or a fresh one.
fn start_tag(original: Option<&str>, name: &str) -> String {
    match original {
        Some(el) => {
            let (tag, _) = split_start_tag(el);
            let t = strip_prefix_tag(tag);
            // Always open (children may follow); callers close it.
            let t = t.trim_end_matches("/>").trim_end_matches('>').to_string();
            format!("{t}>")
        }
        None => format!("<{name}>"),
    }
}

/// Self-close a start tag built with `start_tag`.
fn closed(tag: String) -> String {
    format!("{}/>", tag.trim_end_matches('>'))
}

/// The page-setup elements of a worksheet, keyed by name (None removes),
/// generated from `p` with the original elements' other attributes kept.
fn page_elements(p: &PageSetup, original: &dyn Fn(&str) -> Option<String>) -> Vec<(&'static str, Option<String>)> {
    let mut out = Vec::new();
    let opts = [("horizontalCentered", p.center_h), ("verticalCentered", p.center_v), ("headings", p.headings), ("gridLines", p.gridlines)];
    let po = if opts.iter().any(|o| o.1) {
        let changes: Vec<(&str, Option<String>)> = opts.iter().map(|(k, v)| (*k, bool_attr(*v))).chain([("gridLinesSet", None)]).collect();
        Some(closed(set_attrs(&start_tag(original("printOptions").as_deref(), "printOptions"), &changes)))
    } else {
        None
    };
    out.push(("printOptions", po));
    let m = &p.margins;
    let margins: Vec<(&str, Option<String>)> =
        [("left", m.left), ("right", m.right), ("top", m.top), ("bottom", m.bottom), ("header", m.header), ("footer", m.footer)].iter().map(|(k, v)| (*k, Some(num(*v)))).collect();
    out.push(("pageMargins", Some(closed(set_attrs(&start_tag(original("pageMargins").as_deref(), "pageMargins"), &margins)))));
    let (fw, fh) = p.fit.unwrap_or((1, 1));
    let setup: Vec<(&str, Option<String>)> = vec![
        ("paperSize", (p.paper != 1).then(|| p.paper.to_string())),
        ("scale", (p.scale != 100).then(|| p.scale.to_string())),
        ("firstPageNumber", p.first_page.map(|n| n.to_string())),
        ("fitToWidth", (fw != 1).then(|| fw.to_string())),
        ("fitToHeight", (fh != 1).then(|| fh.to_string())),
        ("pageOrder", p.over_then_down.then(|| "overThenDown".to_string())),
        ("orientation", Some(if p.orientation == Orientation::Landscape { "landscape" } else { "portrait" }.to_string())),
        ("blackAndWhite", bool_attr(p.black_and_white)),
        ("useFirstPageNumber", p.first_page.map(|_| "1".to_string())),
    ];
    out.push(("pageSetup", Some(closed(set_attrs(&start_tag(original("pageSetup").as_deref(), "pageSetup"), &setup)))));
    // Header/footer: odd (all-pages) texts regenerated; first/even ones and
    // the element's options kept.
    let hf_orig = original("headerFooter");
    let mut kept = String::new();
    if let Some(el) = &hf_orig
        && let Ok((_, _, _, kids)) = children(format!("<r>{}</r>", crate::xlsx_patch::inner_of(el)).as_bytes())
    {
        let inner = format!("<r>{}</r>", crate::xlsx_patch::inner_of(el));
        for k in kids.iter().filter(|k| !matches!(k.local.as_str(), "oddHeader" | "oddFooter")) {
            kept.push_str(&inner[k.start..k.end]);
        }
    }
    let hf = if p.header.is_empty() && p.footer.is_empty() && kept.is_empty() {
        None
    } else {
        let mut body = String::new();
        if !p.header.is_empty() {
            body.push_str(&format!("<oddHeader>{}</oddHeader>", esc(&p.header)));
        }
        if !p.footer.is_empty() {
            body.push_str(&format!("<oddFooter>{}</oddFooter>", esc(&p.footer)));
        }
        let tag = start_tag(hf_orig.as_deref(), "headerFooter");
        // Kept children carry the document's prefix already: mark them raw.
        Some(format!("{tag}{body}\u{1}{kept}\u{1}</headerFooter>"))
    };
    out.push(("headerFooter", hf));
    let brks = |set: &BTreeSet<u32>, el: &str, max: u32| {
        (!set.is_empty()).then(|| {
            let items: String = set.iter().map(|b| format!("<brk id=\"{b}\" max=\"{max}\" man=\"1\"/>")).collect();
            format!("<{el} count=\"{n}\" manualBreakCount=\"{n}\">{items}</{el}>", n = set.len())
        })
    };
    out.push(("rowBreaks", brks(&p.row_breaks, "rowBreaks", xl_engine::MAX_COLS - 1)));
    out.push(("colBreaks", brks(&p.col_breaks, "colBreaks", xl_engine::MAX_ROWS - 1)));
    out
}

/// `<sheetPr>` with `<pageSetUpPr fitToPage>` set as wanted (raw, with the
/// document's prefix), or None to leave it alone.
fn sheet_pr(original: Option<&str>, prefix: &str, fit: bool) -> Option<String> {
    let current = original.is_some_and(|el| {
        el.split("pageSetUpPr").nth(1).and_then(|rest| rest.split('>').next()).is_some_and(|attrs| attrs.contains("fitToPage=\"1\"") || attrs.contains("fitToPage=\"true\""))
    });
    if current == fit {
        return None;
    }
    let pr_tag = format!("<{prefix}pageSetUpPr");
    let el = original.map(str::to_string).unwrap_or_else(|| format!("<{prefix}sheetPr/>"));
    if let Some(i) = el.find(&pr_tag) {
        let end = i + el[i..].find('>').unwrap_or(el.len() - i);
        let tag = &el[i..=end];
        let new = set_attrs(tag, &[("fitToPage", fit.then(|| "1".to_string()))]);
        return Some(format!("{}{}{}", &el[..i], new, &el[end + 1..]));
    }
    let child = format!("<{prefix}pageSetUpPr fitToPage=\"1\"/>");
    let (tag, rest) = split_start_tag(&el);
    if tag.ends_with("/>") {
        let name = tag[1..].split([' ', '/', '>']).next().unwrap_or("sheetPr");
        return Some(format!("{}>{child}</{name}>", tag.trim_end_matches("/>").trim_end()));
    }
    // pageSetUpPr is the last child in sheetPr's schema order.
    let close = rest.rfind("</").map_or(el.len(), |i| tag.len() + i);
    Some(format!("{}{child}{}", &el[..close], &el[close..]))
}

/// Rewrite a worksheet's page-setup elements from `p`.
pub(crate) fn patch_sheet_xml(doc: &[u8], p: &PageSetup) -> Result<Vec<u8>, String> {
    let (_, _, prefix, kids) = children(doc)?;
    let text = String::from_utf8_lossy(doc).to_string();
    let original = |name: &str| kids.iter().find(|k| k.local == name).map(|k| text[k.start..k.end].to_string());
    let mut replace: HashMap<&str, Option<String>> = HashMap::new();
    for (name, el) in page_elements(p, &original) {
        // Generated snippets get the document's prefix here; raw parts stay.
        let el = el.map(|e| {
            let parts: Vec<&str> = e.split('\u{1}').collect();
            match parts.as_slice() {
                [a, raw, b] => format!("\u{1}{}{raw}{}\u{1}", prefixed(a, &prefix), prefixed(b, &prefix)),
                _ => format!("\u{1}{}\u{1}", prefixed(&e, &prefix)),
            }
        });
        replace.insert(name, el);
    }
    if let Some(pr) = sheet_pr(original("sheetPr").as_deref(), &prefix, p.fit.is_some()) {
        replace.insert("sheetPr", Some(format!("\u{1}{pr}\u{1}")));
    }
    splice_keeping_raw(doc, SHEET_ORDER, replace)
}

/// Replace the print area / titles names of `sheets` in workbook.xml.
pub(crate) fn patch_workbook_names(doc: &[u8], wb: &Workbook, sheets: &[SheetId]) -> Result<Vec<u8>, String> {
    let (_, _, prefix, kids) = children(doc)?;
    let text = String::from_utf8_lossy(doc).to_string();
    let index: HashMap<SheetId, usize> = wb.sheet_ids().iter().enumerate().map(|(i, s)| (*s, i)).collect();
    let dirty: BTreeSet<usize> = sheets.iter().filter_map(|s| index.get(s).copied()).collect();
    let mut kept = String::new();
    if let Some(k) = kids.iter().find(|k| k.local == "definedNames") {
        let el = &text[k.start..k.end];
        let inner = format!("<r>{}</r>", crate::xlsx_patch::inner_of(el));
        if let Ok((_, _, _, list)) = children(inner.as_bytes()) {
            for d in list {
                let item = &inner[d.start..d.end];
                let (tag, _) = split_start_tag(item);
                let name = tag.split("name=\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("");
                let local_id = tag.split("localSheetId=\"").nth(1).and_then(|s| s.split('"').next()).and_then(|v| v.parse::<usize>().ok());
                let ours = name.eq_ignore_ascii_case("_xlnm.Print_Area") || name.eq_ignore_ascii_case("_xlnm.Print_Titles");
                if ours && local_id.is_some_and(|i| dirty.contains(&i)) {
                    continue;
                }
                kept.push_str(item);
            }
        }
    }
    let mut added = String::new();
    for &s in sheets {
        let Some(&i) = index.get(&s) else { continue };
        let (area, titles) = print_names(&wb.layout(s).page, wb.sheet_name(s));
        for (name, f) in [("_xlnm.Print_Area", area), ("_xlnm.Print_Titles", titles)] {
            if let Some(f) = f {
                added.push_str(&format!("<definedName name=\"{name}\" localSheetId=\"{i}\">{}</definedName>", esc(&f)));
            }
        }
    }
    let mut replace: HashMap<&str, Option<String>> = HashMap::new();
    let body = format!("{kept}{}", prefixed(&added, &prefix));
    let el = (!body.is_empty()).then(|| format!("\u{1}{}{body}{}\u{1}", prefixed("<definedNames>", &prefix), prefixed("</definedNames>", &prefix)));
    replace.insert("definedNames", el);
    splice_keeping_raw(doc, WORKBOOK_ORDER, replace)
}

/// The patched save's page-setup step: regenerate the elements and names
/// of sheets whose page setup changed (and of new sheets that have one).
/// `doc_of` gives a part's current bytes (already patched or rewritten);
/// returns the parts to replace.
pub(crate) fn patch_pages(wb: &Workbook, pkg: &Package, new_sheets: &[(SheetId, String)], doc_of: &dyn Fn(&str) -> Option<Vec<u8>>) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    let mut name_sheets = Vec::new();
    let mut sheets: Vec<(SheetId, String, bool)> = pkg.sheets.iter().filter(|s| wb.sheet_ids().contains(&s.id)).map(|s| (s.id, s.part.clone(), false)).collect();
    sheets.extend(new_sheets.iter().map(|(id, part)| (*id, part.clone(), true)));
    for (id, part, is_new) in sheets {
        let p = &wb.layout(id).page;
        let (elements, names) = if is_new { (!p.without_names().is_default(), p.names_part() != (None, None, None)) } else { (elements_changed(wb, pkg, id), names_changed(wb, pkg, id)) };
        if elements {
            let doc = doc_of(&part).ok_or_else(|| format!("missing {part}"))?;
            out.push((part, patch_sheet_xml(&doc, p)?));
        }
        if names {
            name_sheets.push(id);
        }
    }
    if !name_sheets.is_empty() {
        let doc = doc_of("xl/workbook.xml").ok_or("missing xl/workbook.xml")?;
        out.push(("xl/workbook.xml".into(), patch_workbook_names(&doc, wb, &name_sheets)?));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_page_elements() {
        let doc = br#"<worksheet><sheetPr><tabColor rgb="FF00FF00"/><pageSetUpPr fitToPage="1"/></sheetPr><sheetData><row r="1"/></sheetData>
<printOptions horizontalCentered="1" gridLines="1"/><pageMargins left="0.5" right="0.5" top="1" bottom="1" header="0.4" footer="0.4"/>
<pageSetup paperSize="9" orientation="landscape" fitToHeight="0" pageOrder="overThenDown" r:id="rId1"/>
<headerFooter><oddHeader>&amp;CReport &amp;A</oddHeader><oddFooter>&amp;RPage &amp;P of &amp;N</oddFooter></headerFooter>
<rowBreaks count="2" manualBreakCount="2"><brk id="10" max="16383" man="1"/><brk id="20" max="16383"/></rowBreaks><colBreaks count="1"><brk id="4" max="1048575" man="1"/></colBreaks></worksheet>"#;
        let p = read_page(doc);
        assert_eq!(p.fit, Some((1, 0)));
        assert!(p.center_h && p.gridlines && !p.headings && !p.center_v);
        assert_eq!((p.margins.left, p.margins.top, p.margins.header), (0.5, 1.0, 0.4));
        assert_eq!((p.paper, p.orientation, p.over_then_down), (9, Orientation::Landscape, true));
        assert_eq!(p.header, "&CReport &A");
        assert_eq!(p.footer, "&RPage &P of &N");
        assert_eq!(p.row_breaks.iter().copied().collect::<Vec<_>>(), vec![10]);
        assert_eq!(p.col_breaks.iter().copied().collect::<Vec<_>>(), vec![4]);
    }

    #[test]
    fn parses_print_names() {
        let mut p = PageSetup::default();
        apply_print_name(&mut p, "_xlnm.Print_Area", "'My, sheet'!$B$2:$F$40");
        apply_print_name(&mut p, "_xlnm.Print_Titles", "'My, sheet'!$A:$A,'My, sheet'!$1:$2");
        assert_eq!(p.print_area, Some(Range::new(CellRef::parse("B2").unwrap(), CellRef::parse("F40").unwrap())));
        assert_eq!((p.title_rows, p.title_cols), (Some((0, 1)), Some((0, 0))));
        assert_eq!(print_names(&p, "My, sheet"), (Some("'My, sheet'!$B$2:$F$40".into()), Some("'My, sheet'!$A:$A,'My, sheet'!$1:$2".into())));
    }

    #[test]
    fn patching_keeps_unmodeled_attributes_and_order() {
        let doc = br#"<worksheet xmlns:r="r"><sheetPr codeName="S"/><sheetData/><pageMargins left="0.7" right="0.7" top="0.75" bottom="0.75" header="0.3" footer="0.3"/><pageSetup paperSize="9" horizontalDpi="600" r:id="rId3"/><drawing r:id="rId1"/></worksheet>"#;
        let mut p = read_page(doc);
        p.orientation = Orientation::Landscape;
        p.fit = Some((1, 0));
        p.footer = "&CPage &P".into();
        p.row_breaks.insert(30);
        let out = String::from_utf8(patch_sheet_xml(doc, &p).unwrap()).unwrap();
        assert!(out.contains(r#"<sheetPr codeName="S"><pageSetUpPr fitToPage="1"/></sheetPr>"#), "{out}");
        assert!(out.contains(r#"horizontalDpi="600""#) && out.contains(r#"r:id="rId3""#) && out.contains(r#"orientation="landscape""#) && out.contains(r#"fitToHeight="0""#), "{out}");
        let order = ["<pageMargins", "<pageSetup", "<headerFooter", "<rowBreaks", "<drawing"];
        let pos: Vec<usize> = order.iter().map(|t| out.find(t).unwrap()).collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]), "{out}");
        assert_eq!(read_page(out.as_bytes()), p);
    }

    #[test]
    fn patching_prefixed_documents() {
        let doc = br#"<x:worksheet xmlns:x="m"><x:sheetData/><x:headerFooter differentFirst="1"><x:oddHeader>old</x:oddHeader><x:firstHeader>first</x:firstHeader></x:headerFooter></x:worksheet>"#;
        let p = PageSetup { header: "&Cnew".into(), gridlines: true, ..Default::default() };
        let out = String::from_utf8(patch_sheet_xml(doc, &p).unwrap()).unwrap();
        assert!(out.contains(r#"<x:headerFooter differentFirst="1"><x:oddHeader>&amp;Cnew</x:oddHeader><x:firstHeader>first</x:firstHeader></x:headerFooter>"#), "{out}");
        assert!(out.contains(r#"<x:printOptions gridLines="1"/>"#), "{out}");
        assert_eq!(read_page(out.as_bytes()).header, "&Cnew");
    }
}
