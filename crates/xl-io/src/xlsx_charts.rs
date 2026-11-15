//! Reading charts from xlsx: worksheet → drawing (anchors) → chart parts
//! (kind, series references, title).

use std::sync::Arc;

use quick_xml::events::Event;
use quick_xml::XmlVersion;
use xl_engine::chart::{Axis, BinMode, Chart, ChartKind, ChartOptions, ChartSeries, DataLabels, Dash, ErrorBars, ErrorDir, ErrorKind, LegendPos, MarkerShape, Paint, SeriesFormat, TrendKind, Trendline};
use xl_engine::formula::parse_formula;
use xl_engine::{CellRef, SheetId, Workbook};

use crate::xlsx_read::{Zip, attr, local, push_entity, read_entry, read_rels, xml};
use crate::{from_excel_height, from_excel_width_px};

/// Read the tables (xl/tables/*.xml) a worksheet points to.
pub(crate) fn read_sheet_tables(zip: &mut Zip, sheet_part: &str, wb: &mut Workbook, sheet: SheetId) -> Result<(), String> {
    let rels = read_rels(zip, &rels_of(sheet_part))?;
    let parts: Vec<String> = rels.values().filter(|t| t.contains("tables/")).map(|t| resolve(sheet_part, t)).collect();
    for part in parts {
        let Some(bytes) = read_entry(zip, &part)? else { continue };
        let mut r = xml(&bytes);
        let mut buf = Vec::new();
        let mut table: Option<xl_engine::Table> = None;
        loop {
            match r.read_event_into(&mut buf).map_err(|e| e.to_string())? {
                Event::Start(ref e) | Event::Empty(ref e) => match local(e.name().as_ref()) {
                    "table" => {
                        let range = attr(e, "ref").and_then(|s| {
                            let (a, b) = s.split_once(':').unwrap_or((&s, &s));
                            Some(xl_engine::Range::new(CellRef::parse(a)?, CellRef::parse(b)?))
                        });
                        let header = attr(e, "headerRowCount").is_none_or(|v| v != "0");
                        let name = attr(e, "displayName").or_else(|| attr(e, "name"));
                        if let (Some(range), Some(name), true) = (range, name, header) {
                            let totals = attr(e, "totalsRowCount").is_some_and(|v| v != "0");
                            table = Some(xl_engine::Table { name, range, totals, style: xl_engine::TableStyle::Blue, banded: true });
                        }
                    }
                    "tableStyleInfo" => {
                        if let Some(t) = table.as_mut() {
                            t.banded = attr(e, "showRowStripes").is_none_or(|v| v != "0");
                            // Excel's built-in styles cycle through accent colors by number.
                            let n: u32 = attr(e, "name").and_then(|s| s.trim_start_matches(|c: char| !c.is_ascii_digit()).parse().ok()).unwrap_or(2);
                            t.style = match (n + 5) % 7 {
                                0 => xl_engine::TableStyle::Blue,
                                1 => xl_engine::TableStyle::Orange,
                                2 | 3 => xl_engine::TableStyle::Gray,
                                5 => xl_engine::TableStyle::Green,
                                _ => xl_engine::TableStyle::Blue,
                            };
                        }
                    }
                    _ => {}
                },
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        if let Some(t) = table
            && let Some(l) = wb.layout_mut(sheet)
        {
            l.tables.push(t);
        }
    }
    Ok(())
}

/// Read a worksheet's hyperlinks (addresses live in its relationships).
pub(crate) fn read_sheet_links(zip: &mut Zip, sheet_part: &str, wb: &mut Workbook, sheet: SheetId) -> Result<(), String> {
    let Some(bytes) = read_entry(zip, sheet_part)? else { return Ok(()) };
    if !bytes.windows(10).any(|w| w == b"<hyperlink" || w.ends_with(b":hyperlink")) {
        return Ok(());
    }
    let rels = read_rels(zip, &rels_of(sheet_part))?;
    let mut r = xml(&bytes);
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf).map_err(|e| e.to_string())? {
            Event::Start(ref e) | Event::Empty(ref e) if local(e.name().as_ref()) == "hyperlink" => {
                let Some(range) = attr(e, "ref") else { continue };
                let tooltip = attr(e, "tooltip");
                let link = match (attr(e, "id").and_then(|id| rels.get(&id).cloned()), attr(e, "location")) {
                    (Some(url), loc) => xl_engine::Link::Url { url: loc.map_or(url.clone(), |l| format!("{url}#{l}")), tooltip },
                    (None, Some(location)) => xl_engine::Link::Place { location, tooltip },
                    (None, None) => continue,
                };
                // A link over a range applies to each of its cells.
                let (a, b) = range.split_once(':').unwrap_or((&range, &range));
                if let (Some(a), Some(b)) = (CellRef::parse(a), CellRef::parse(b))
                    && let Some(l) = wb.layout_mut(sheet)
                {
                    for c in xl_engine::Range::new(a, b).iter().take(10_000) {
                        l.links.insert(c, link.clone());
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// Read a worksheet's notes (legacy comments parts).
pub(crate) fn read_sheet_notes(zip: &mut Zip, sheet_part: &str, wb: &mut Workbook, sheet: SheetId) -> Result<(), String> {
    let rels = read_rels(zip, &rels_of(sheet_part))?;
    let parts: Vec<String> = rels.values().filter(|t| t.rsplit('/').next().is_some_and(|f| f.starts_with("comments"))).map(|t| resolve(sheet_part, t)).collect();
    for part in parts {
        let Some(bytes) = read_entry(zip, &part)? else { continue };
        let mut r = xml(&bytes);
        let mut buf = Vec::new();
        let mut authors: Vec<String> = Vec::new();
        let (mut in_author, mut in_text, mut in_t) = (false, false, false);
        let mut cur: Option<(CellRef, usize)> = None;
        let mut text = String::new();
        loop {
            match r.read_event_into(&mut buf).map_err(|e| e.to_string())? {
                Event::Start(ref e) => match local(e.name().as_ref()) {
                    "author" => {
                        in_author = true;
                        authors.push(String::new());
                    }
                    "comment" => {
                        cur = attr(e, "ref").and_then(|r| CellRef::parse(r.split(':').next().unwrap_or(&r))).map(|c| (c, attr(e, "authorId").and_then(|v| v.parse().ok()).unwrap_or(0)));
                        text.clear();
                    }
                    "text" => in_text = true,
                    "t" if in_text => in_t = true,
                    // Phonetic runs aren't part of the note.
                    "rPh" => in_text = false,
                    _ => {}
                },
                Event::Text(ref t) => {
                    let s = t.xml_content(XmlVersion::Implicit1_0);
                    if in_author {
                        if let Some(a) = authors.last_mut() {
                            a.push_str(&s);
                        }
                    } else if in_t {
                        text.push_str(&s);
                    }
                }
                Event::GeneralRef(ref g) => {
                    if in_author {
                        if let Some(a) = authors.last_mut() {
                            push_entity(a, g);
                        }
                    } else if in_t {
                        push_entity(&mut text, g);
                    }
                }
                Event::End(ref e) => match local(e.name().as_ref()) {
                    "author" => in_author = false,
                    "t" => in_t = false,
                    "rPh" => in_text = true,
                    "text" => in_text = false,
                    "comment" => {
                        if let Some((cell, author_id)) = cur.take() {
                            let author = authors.get(author_id).cloned().unwrap_or_default();
                            // Older Excel starts the text with a bold "Author:" line.
                            let body = text.strip_prefix(&format!("{author}:")).map_or(text.as_str(), |t| t.strip_prefix('\n').unwrap_or(t)).to_string();
                            if let Some(l) = wb.layout_mut(sheet) {
                                l.notes.insert(cell, xl_engine::Note { author, text: body });
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
    }
    Ok(())
}

/// What reading a sheet's drawing found besides the charts it loaded.
#[derive(Default)]
pub(crate) struct DrawingNotes {
    pub charts: usize,
    /// Shapes, text boxes, grouped or linked pictures and the like (kept
    /// when xl saves into the original, but not shown).
    pub other_drawings: bool,
    /// Pictures xl shows, with the media part each one's image is in.
    pub pictures: Vec<FoundPicture>,
    /// Pictures in a format xl can't draw (EMF, WMF…).
    pub undrawable_pictures: bool,
    /// Charts of kinds xl doesn't draw.
    pub unsupported_charts: bool,
    /// Charts with formatting xl doesn't model (fonts, data tables, 3-D…),
    /// which is dropped if the chart is edited.
    pub styled_charts: bool,
}

/// A picture found in a sheet's drawing; its image is read separately.
pub(crate) struct FoundPicture {
    pub anchor: CellRef,
    pub offset: (f64, f64),
    pub end: Option<(CellRef, (f64, f64))>,
    pub size: (f64, f64),
    /// The media part (e.g. "xl/media/image1.png").
    pub part: String,
    pub name: String,
    pub alt: Option<String>,
}

/// `target` relative to the folder of `part` (or absolute with a leading /).
pub(crate) fn resolve(part: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut segs: Vec<&str> = part.split('/').collect();
    segs.pop();
    for s in target.split('/') {
        match s {
            ".." => {
                segs.pop();
            }
            "." | "" => {}
            s => segs.push(s),
        }
    }
    segs.join("/")
}

pub(crate) fn rels_of(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

const EMU_PER_PX: f64 = 9525.0;

#[derive(Default, Clone, Copy)]
struct Marker {
    col: u32,
    col_off: f64,
    row: u32,
    row_off: f64,
}

pub(crate) fn read_sheet_charts(zip: &mut Zip, sheet_part: &str, wb: &mut Workbook, sheet: SheetId) -> Result<DrawingNotes, String> {
    let (charts, mut notes) = scan_charts(zip, sheet_part, wb, sheet)?;
    // Pictures showing the same image share its bytes.
    let mut media: std::collections::HashMap<String, Option<Arc<[u8]>>> = std::collections::HashMap::new();
    let mut pictures = Vec::new();
    for p in std::mem::take(&mut notes.pictures) {
        if !media.contains_key(&p.part) {
            let bytes = read_entry(zip, &p.part)?.map(Arc::<[u8]>::from);
            media.insert(p.part.clone(), bytes);
        }
        let Some(Some(data)) = media.get(&p.part).cloned() else {
            notes.other_drawings = true;
            continue;
        };
        let format = xl_engine::picture::PictureFormat::sniff(&data);
        notes.undrawable_pictures |= format == xl_engine::picture::PictureFormat::Other;
        pictures.push(xl_engine::picture::Picture { anchor: p.anchor, offset: p.offset, end: p.end, size: p.size, data, format, name: p.name.clone(), alt: p.alt.clone() });
        notes.pictures.push(p);
    }
    if let Some(l) = wb.layout_mut(sheet) {
        l.charts.extend(charts.into_iter().map(|c| c.0));
        l.pictures.extend(pictures);
    }
    Ok(notes)
}

/// A chart found in a sheet's drawings, with the drawing part and the
/// index of its anchor there (so a moved chart can be re-anchored).
pub(crate) type FoundChart = (Chart, String, usize);

/// The charts xl can read in a sheet's drawings, in a stable order.
pub(crate) fn scan_charts(zip: &mut Zip, sheet_part: &str, wb: &Workbook, sheet: SheetId) -> Result<(Vec<FoundChart>, DrawingNotes), String> {
    let mut notes = DrawingNotes::default();
    let mut found = Vec::new();
    let rels = read_rels(zip, &rels_of(sheet_part))?;
    let mut drawings: Vec<String> = rels.values().filter(|t| t.contains("drawings/") && t.ends_with(".xml")).map(|t| resolve(sheet_part, t)).collect();
    drawings.sort();
    let mut theme: Option<Vec<xl_engine::Rgb>> = None;
    for drawing in drawings {
        let mut anchor_index = 0usize;
        let Some(bytes) = read_entry(zip, &drawing)? else { continue };
        let drels = read_rels(zip, &rels_of(&drawing))?;
        let mut r = xml(&bytes);
        let (mut from, mut to) = (Marker::default(), Marker::default());
        let mut ext: Option<(f64, f64)> = None;
        let mut in_marker: Option<bool> = None; // Some(true) = from
        let mut field = String::new();
        let mut text = String::new();
        let mut chart_rid: Option<String> = None;
        let mut in_frame = false;
        let mut anchor_kind = "";
        let mut pos: Option<(f64, f64)> = None;
        // The picture directly in this anchor: (blip relationship, name, alt text, hidden).
        let mut pic: Option<(Option<String>, String, Option<String>, bool)> = None;
        let mut group_depth = 0usize;
        let mut buf = Vec::new();
        let mut skip = Vec::new();
        loop {
            match r.read_event_into(&mut buf).map_err(|e| e.to_string())? {
                // Fallback content (the "chart isn't available" shape of chartex
                // frames) stands in for what the choice holds: skip it.
                Event::Start(ref e) if local(e.name().as_ref()) == "Fallback" => {
                    let end = e.name().as_ref().to_string();
                    r.read_to_end_into(quick_xml::name::QName(&end), &mut skip).map_err(|e| e.to_string())?;
                }
                Event::Start(ref e) | Event::Empty(ref e) => match local(e.name().as_ref()) {
                    k @ ("twoCellAnchor" | "oneCellAnchor" | "absoluteAnchor") => {
                        (from, to, ext, chart_rid, in_frame) = (Marker::default(), Marker::default(), None, None, false);
                        anchor_kind = match k {
                            "twoCellAnchor" => "two",
                            "oneCellAnchor" => "one",
                            _ => "abs",
                        };
                        (pos, pic, group_depth) = (None, None, 0);
                    }
                    // Past here, extents belong to the drawn object, not the anchor.
                    "graphicFrame" => in_frame = true,
                    "pic" if group_depth == 0 => {
                        in_frame = true;
                        pic = Some((None, String::new(), None, false));
                    }
                    "cNvPr" if group_depth == 0 => {
                        if let Some(p) = pic.as_mut() {
                            p.1 = attr(e, "name").unwrap_or_default();
                            p.2 = attr(e, "descr").filter(|d| !d.trim().is_empty());
                            p.3 = attr(e, "hidden").is_some_and(|v| v == "1" || v == "true");
                        }
                    }
                    "blip" if group_depth == 0 => {
                        if let Some(p) = pic.as_mut() {
                            // Linked (external) images have r:link instead.
                            p.0 = attr(e, "embed");
                        }
                    }
                    "pos" if anchor_kind == "abs" && !in_frame => {
                        let n = |k: &str| attr(e, k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                        pos = Some((n("x") / EMU_PER_PX, n("y") / EMU_PER_PX));
                    }
                    "from" => in_marker = Some(true),
                    "to" => in_marker = Some(false),
                    f @ ("col" | "colOff" | "row" | "rowOff") if in_marker.is_some() => {
                        field = f.to_string();
                        text.clear();
                    }
                    // The anchor's own extent (one-cell anchors), not the frame's.
                    "ext" if in_marker.is_none() && !in_frame => {
                        let n = |k: &str| attr(e, k).and_then(|v| v.parse::<f64>().ok());
                        if let (Some(cx), Some(cy)) = (n("cx"), n("cy"))
                            && cx > 0.0
                            && cy > 0.0
                        {
                            ext = Some((cx / EMU_PER_PX, cy / EMU_PER_PX));
                        }
                    }
                    "chart" => chart_rid = attr(e, "id"),
                    "grpSp" => {
                        in_frame = true;
                        notes.other_drawings = true;
                        group_depth += 1;
                    }
                    "pic" | "sp" | "cxnSp" => {
                        in_frame = true;
                        notes.other_drawings = true;
                    }
                    _ => {}
                },
                Event::Text(ref t) if !field.is_empty() => text.push_str(&t.xml_content(XmlVersion::Implicit1_0)),
                Event::End(ref e) => match local(e.name().as_ref()) {
                    "from" | "to" => in_marker = None,
                    "col" | "colOff" | "row" | "rowOff" if !field.is_empty() => {
                        let m = if in_marker == Some(true) { &mut from } else { &mut to };
                        let v = text.trim().parse::<f64>().unwrap_or(0.0);
                        match field.as_str() {
                            "col" => m.col = v as u32,
                            "colOff" => m.col_off = v / EMU_PER_PX,
                            "row" => m.row = v as u32,
                            _ => m.row_off = v / EMU_PER_PX,
                        }
                        field.clear();
                    }
                    "grpSp" => group_depth = group_depth.saturating_sub(1),
                    "twoCellAnchor" | "oneCellAnchor" | "absoluteAnchor" => {
                        anchor_index += 1;
                        let this_anchor = anchor_index - 1;
                        // Size in Excel pixels: from the two-cell span, or the extent.
                        let (w, h) = match ext {
                            Some(e) => e,
                            None => {
                                let col_px = |c: u32| wb.col_width(sheet, c).map_or(64.0, |px| crate::to_excel_width_px(px) as f64);
                                let row_px = |r: u32| wb.row_height(sheet, r).map_or(20.0, |px| crate::to_excel_height(px) * 4.0 / 3.0);
                                let w: f64 = (from.col..to.col).map(col_px).sum::<f64>() - from.col_off + to.col_off;
                                let h: f64 = (from.row..to.row).map(row_px).sum::<f64>() - from.row_off + to.row_off;
                                (w, h)
                            }
                        };
                        // Excel pixels to xl's.
                        let x_px = from_excel_width_px;
                        let y_px = |excel: f64| from_excel_height(excel * 0.75);
                        if let Some((rid, name, alt, hidden)) = pic.take() {
                            match rid.and_then(|id| drels.get(&id).cloned()) {
                                Some(target) if !hidden && w > 0.0 && h > 0.0 => {
                                    // Absolute anchors: a position from the sheet's corner.
                                    let (off_x, off_y) = match pos {
                                        Some(p) if anchor_kind == "abs" => p,
                                        _ => (from.col_off, from.row_off),
                                    };
                                    notes.pictures.push(FoundPicture {
                                        anchor: CellRef::new(from.row, from.col),
                                        offset: (x_px(off_x), y_px(off_y)),
                                        end: (anchor_kind == "two").then(|| (CellRef::new(to.row, to.col), (x_px(to.col_off), y_px(to.row_off)))),
                                        size: (x_px(w), y_px(h)),
                                        part: resolve(&drawing, &target),
                                        name,
                                        alt,
                                    });
                                }
                                _ => notes.other_drawings = true,
                            }
                            continue;
                        }
                        let Some(target) = chart_rid.take().and_then(|id| drels.get(&id).cloned()) else { continue };
                        let part = resolve(&drawing, &target);
                        let Some(chart_xml) = read_entry(zip, &part)? else { continue };
                        if theme.is_none() {
                            theme = Some(crate::xlsx_read::read_theme(zip)?);
                        }
                        match parse_chart(&chart_xml, wb, theme.as_deref().unwrap_or(&[])) {
                            Some(p) if !p.series.is_empty() => {
                                notes.styled_charts |= p.lossy;
                                let chart = Chart {
                                    kind: p.kind,
                                    title: p.title,
                                    series: p.series,
                                    anchor: CellRef::new(from.row, from.col),
                                    offset: (from_excel_width_px(from.col_off), from_excel_height(from.row_off * 0.75)),
                                    size: (from_excel_width_px(w).max(80.0), from_excel_height(h * 0.75).max(60.0)),
                                    stacked: p.stacked,
                                    options: p.options,
                                    pivot: None,
                                };
                                found.push((chart, drawing.clone(), this_anchor));
                                notes.charts += 1;
                            }
                            _ => notes.unsupported_charts = true,
                        }
                    }
                    _ => {}
                },
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
    }
    Ok((found, notes))
}

// ------------------------------------------------------------ chart parts

/// An element of a chart part, as a small tree (chart parts are small, and
/// a tree is far easier to read correctly than a stream).
#[derive(Default, Debug)]
struct Node {
    prefix: String,
    name: String,
    attrs: Vec<(String, String)>,
    kids: Vec<Node>,
    text: String,
}

impl Node {
    fn child(&self, name: &str) -> Option<&Node> {
        self.kids.iter().find(|k| k.name == name)
    }

    fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.kids.iter().filter(move |k| k.name == name)
    }

    fn path(&self, names: &[&str]) -> Option<&Node> {
        names.iter().try_fold(self, |n, name| n.child(name))
    }

    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn val(&self) -> Option<&str> {
        self.attr("val")
    }

    fn num(&self) -> Option<f64> {
        self.val().and_then(|v| v.trim().parse().ok())
    }

    /// A CT_Boolean: no `val` means true.
    fn flag(&self) -> bool {
        self.val().is_none_or(|v| v == "1" || v == "true")
    }

    /// The text of every `<a:t>` run below, in order.
    fn runs(&self) -> String {
        let mut out = String::new();
        self.walk(&mut |n| {
            if n.name == "t" {
                out.push_str(&n.text);
            }
        });
        out
    }

    fn walk(&self, f: &mut impl FnMut(&Node)) {
        f(self);
        for k in &self.kids {
            k.walk(f);
        }
    }

    fn any(&self, pred: &impl Fn(&Node) -> bool) -> bool {
        pred(self) || self.kids.iter().any(|k| k.any(pred))
    }
}

fn dom(bytes: &[u8]) -> Option<Node> {
    let node_of = |e: &quick_xml::events::BytesStart| {
        let full = e.name().as_ref().to_string();
        let (prefix, name) = match full.split_once(':') {
            Some((p, n)) => (p.to_string(), n.to_string()),
            None => (String::new(), full.clone()),
        };
        let attrs = e
            .attributes()
            .with_checks(false)
            .flatten()
            .map(|a| (local(a.key.as_ref()).to_string(), a.normalized_value(XmlVersion::Implicit1_0).map(|v| v.into_owned()).unwrap_or_default()))
            .collect();
        Node { prefix, name, attrs, kids: Vec::new(), text: String::new() }
    };
    let mut r = xml(bytes);
    let mut stack: Vec<Node> = vec![Node::default()];
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf).ok()? {
            Event::Start(ref e) => stack.push(node_of(e)),
            Event::Empty(ref e) => {
                let n = node_of(e);
                stack.last_mut()?.kids.push(n);
            }
            Event::Text(ref t) => stack.last_mut()?.text.push_str(&t.xml_content(XmlVersion::Implicit1_0)),
            Event::GeneralRef(ref g) => push_entity(&mut stack.last_mut()?.text, g),
            Event::End(_) => {
                let n = stack.pop()?;
                stack.last_mut()?.kids.push(n);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    let mut root = stack.pop()?;
    root.kids.pop()
}

/// A chart part as xl models it.
pub(crate) struct ParsedChart {
    pub kind: ChartKind,
    pub stacked: bool,
    pub title: Option<String>,
    pub series: Vec<ChartSeries>,
    pub options: ChartOptions,
    /// It has formatting xl doesn't model.
    pub lossy: bool,
}

/// A DrawingML color element (srgbClr, schemeClr with luminance changes…).
fn color(n: &Node, theme: &[xl_engine::Rgb]) -> Option<xl_engine::Rgb> {
    let base = match n.name.as_str() {
        "srgbClr" => u32::from_str_radix(n.val()?, 16).ok()?,
        "sysClr" => u32::from_str_radix(n.attr("lastClr")?, 16).ok()?,
        "schemeClr" => {
            // Theme colors are in theme-index order: lt1, dk1, lt2, dk2, accent1–6, hlink, folHlink.
            let i = match n.val()? {
                "bg1" | "lt1" => 0,
                "tx1" | "dk1" => 1,
                "bg2" | "lt2" => 2,
                "tx2" | "dk2" => 3,
                "hlink" => 10,
                "folHlink" => 11,
                a => 3 + a.strip_prefix("accent")?.parse::<usize>().ok().filter(|k| (1..=6).contains(k))?,
            };
            *theme.get(i)?
        }
        "prstClr" => match n.val()? {
            "black" => 0x000000,
            "white" => 0xFFFFFF,
            "red" => 0xFF0000,
            "green" => 0x008000,
            "blue" => 0x0000FF,
            "yellow" => 0xFFFF00,
            "gray" => 0x808080,
            _ => return None,
        },
        _ => return None,
    };
    let pct = |name: &str| n.child(name).and_then(Node::num).map(|v| v / 100_000.0);
    let (mut r, mut g, mut b) = (((base >> 16) & 255) as f64 / 255.0, ((base >> 8) & 255) as f64 / 255.0, (base & 255) as f64 / 255.0);
    if pct("lumMod").is_some() || pct("lumOff").is_some() {
        let (h, s, l) = rgb_to_hsl(r, g, b);
        let l = (l * pct("lumMod").unwrap_or(1.0) + pct("lumOff").unwrap_or(0.0)).clamp(0.0, 1.0);
        (r, g, b) = hsl_to_rgb(h, s, l);
    }
    if let Some(s) = pct("shade") {
        (r, g, b) = (r * s, g * s, b * s);
    }
    if let Some(t) = pct("tint") {
        (r, g, b) = (1.0 - (1.0 - r) * t, 1.0 - (1.0 - g) * t, 1.0 - (1.0 - b) * t);
    }
    let byte = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    Some((byte(r) << 16) | (byte(g) << 8) | byte(b))
}

fn rgb_to_hsl(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    let l = (mx + mn) / 2.0;
    if mx == mn {
        return (0.0, 0.0, l);
    }
    let d = mx - mn;
    let s = if l > 0.5 { d / (2.0 - mx - mn) } else { d / (mx + mn) };
    let h = if mx == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if mx == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h / 6.0, s, l)
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (f64, f64, f64) {
    if s == 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let hue = |mut t: f64| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (hue(h + 1.0 / 3.0), hue(h), hue(h - 1.0 / 3.0))
}

/// A fill (`solidFill` / `noFill` child) as a paint; Auto if neither.
fn paint_in(n: &Node, theme: &[xl_engine::Rgb]) -> Paint {
    if n.child("noFill").is_some() {
        return Paint::None;
    }
    match n.child("solidFill").and_then(|f| f.kids.iter().find_map(|c| color(c, theme))) {
        Some(c) => Paint::Solid(c),
        None => Paint::Auto,
    }
}

/// Fill, line, width and dash of a shape-properties element.
fn shape_props(sp: &Node, f: &mut SeriesFormat, theme: &[xl_engine::Rgb]) {
    f.fill = paint_in(sp, theme);
    if let Some(ln) = sp.child("ln") {
        f.line = paint_in(ln, theme);
        f.line_width = ln.attr("w").and_then(|w| w.parse::<f64>().ok()).map(|w| w / 12700.0);
        f.dash = match ln.child("prstDash").and_then(Node::val) {
            Some("dash" | "sysDash") => Dash::Dash,
            Some("dot" | "sysDot") => Dash::Dot,
            Some("dashDot" | "sysDashDot" | "lgDashDot" | "lgDashDotDot" | "sysDashDotDot") => Dash::DashDot,
            Some("lgDash") => Dash::LongDash,
            _ => Dash::Solid,
        };
    }
}

fn labels_of(d: &Node) -> DataLabels {
    if d.child("delete").is_some_and(Node::flag) {
        return DataLabels::default();
    }
    let on = |k: &str| d.child(k).is_some_and(Node::flag);
    DataLabels { value: on("showVal"), category: on("showCatName"), series: on("showSerName"), percent: on("showPercent") }
}

fn axis_of(n: &Node) -> Axis {
    let scaling = n.child("scaling");
    let sc = |k: &str| scaling.and_then(|s| s.child(k)).and_then(Node::num);
    let fmt = n.child("numFmt").filter(|f| f.attr("sourceLinked") != Some("1")).and_then(|f| f.attr("formatCode")).map(str::to_string);
    Axis {
        title: n.child("title").map(Node::runs).map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
        min: sc("min"),
        max: sc("max"),
        major: n.child("majorUnit").and_then(Node::num),
        log_base: sc("logBase"),
        num_fmt: fmt,
        gridlines: n.child("majorGridlines").is_some(),
        minor_gridlines: n.child("minorGridlines").is_some(),
        hidden: n.child("delete").is_some_and(Node::flag),
    }
}

fn group_kind(name: &str) -> Option<Result<ChartKind, ()>> {
    Some(Ok(match name {
        "barChart" | "bar3DChart" => ChartKind::Column,
        "lineChart" | "line3DChart" => ChartKind::Line,
        "areaChart" | "area3DChart" => ChartKind::Area,
        "pieChart" | "pie3DChart" => ChartKind::Pie,
        "doughnutChart" => ChartKind::Doughnut,
        "scatterChart" => ChartKind::Scatter,
        "radarChart" => ChartKind::Radar,
        "bubbleChart" => ChartKind::Bubble,
        "stockChart" | "surfaceChart" | "surface3DChart" | "ofPieChart" => return Some(Err(())),
        _ => return None,
    }))
}

/// Kind, stacking, title, series and formatting of a chart part. None if it's
/// a kind xl doesn't draw.
pub(crate) fn parse_chart(bytes: &[u8], wb: &Workbook, theme: &[xl_engine::Rgb]) -> Option<ParsedChart> {
    let root = dom(bytes)?;
    if root.child("chartData").is_some() {
        return parse_chartex(&root, wb, theme);
    }
    let chart = root.child("chart")?;
    let plot = chart.child("plotArea")?;
    let expr = |src: &str| parse_formula(src.trim().trim_start_matches('='), wb.sheets()).ok().map(Arc::new);
    let mut options = ChartOptions::blank();
    let mut lossy = false;
    // The chart style: plain c:style, or the fallback of Excel's c14 choice.
    let mut style = None;
    root.walk(&mut |n| {
        if n.name == "style" && n.prefix != "c14" && style.is_none() {
            style = n.num().filter(|v| (1.0..=48.0).contains(v)).map(|v| v as u8);
        }
    });
    options.style = style;
    options.auto_title_deleted = chart.child("autoTitleDeleted").is_some_and(Node::flag);
    let title = chart.child("title").map(Node::runs).map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    options.legend = chart.child("legend").map(|l| match l.child("legendPos").and_then(Node::val) {
        Some("b") => LegendPos::Bottom,
        Some("t") => LegendPos::Top,
        Some("l") => LegendPos::Left,
        Some("tr") => LegendPos::TopRight,
        _ => LegendPos::Right,
    });
    lossy |= chart.child("view3D").is_some() || plot.child("dTable").is_some();
    let axes: Vec<(&str, &Node)> = plot.kids.iter().filter(|k| matches!(k.name.as_str(), "catAx" | "valAx" | "dateAx" | "serAx")).filter_map(|k| Some((k.child("axId")?.val()?, k))).collect();
    lossy |= axes.iter().any(|(_, a)| a.name == "serAx");
    let ax_ids = |g: &Node| -> Vec<String> { g.children("axId").filter_map(|a| a.val().map(str::to_string)).collect() };
    let mut kind: Option<ChartKind> = None;
    let mut primary: Vec<String> = Vec::new();
    let mut secondary_ids: Option<Vec<String>> = None;
    let mut stacked = false;
    let mut series: Vec<(f64, ChartSeries)> = Vec::new();
    for g in &plot.kids {
        let Some(gk) = group_kind(&g.name) else { continue };
        let mut gk = gk.ok()?;
        lossy |= g.name.contains("3D") || ["dropLines", "hiLowLines", "upDownBars", "serLines"].iter().any(|k| g.child(k).is_some());
        if gk == ChartKind::Column && g.child("barDir").and_then(Node::val) == Some("bar") {
            gk = ChartKind::Bar;
        }
        let ids = ax_ids(g);
        let first = kind.is_none();
        if first {
            kind = Some(gk);
            primary = ids.clone();
            stacked = matches!(g.child("grouping").and_then(Node::val), Some("stacked" | "percentStacked"));
            if gk == ChartKind::Doughnut {
                options.hole_size = g.child("holeSize").and_then(Node::num).map_or(50, |v| v.clamp(1.0, 99.0) as u8);
            }
            options.radar_filled = g.child("radarStyle").and_then(Node::val) == Some("filled");
        }
        let main = kind?;
        // Combo charts: a second group of the same kind can't be told apart from the first.
        if !first && (!main.combinable() || !gk.combinable()) && gk != main {
            return None;
        }
        let secondary = !ids.is_empty() && ids != primary;
        if secondary && secondary_ids.is_none() {
            secondary_ids = Some(ids.clone());
        }
        let group_labels = g.child("dLbls").map(labels_of);
        for (n, ser) in g.children("ser").enumerate() {
            let f = |part: &str| ser.child(part).and_then(|p| p.kids.iter().find_map(|r| r.child("f"))).map(|f| f.text.clone());
            let Some(values) = f("val").or_else(|| f("yVal")).as_deref().and_then(expr) else { continue };
            let order = ser.child("order").and_then(Node::num).unwrap_or((series.len() + n) as f64);
            let name = ser
                .child("tx")
                .and_then(|tx| match tx.path(&["strRef", "f"]) {
                    Some(f) => expr(&f.text),
                    None => tx.child("v").map(|v| Arc::new(xl_engine::formula::Expr::Text(v.text.clone().into()))),
                });
            let categories = f("cat").or_else(|| f("xVal")).as_deref().and_then(expr);
            let sizes = f("bubbleSize").as_deref().and_then(expr);
            let mut format = SeriesFormat { kind: (gk != main).then_some(gk), secondary, ..SeriesFormat::default() };
            if let Some(sp) = ser.child("spPr") {
                shape_props(sp, &mut format, theme);
            }
            if let Some(m) = ser.child("marker") {
                format.marker = xl_engine::chart::Marker {
                    shape: m.child("symbol").and_then(Node::val).and_then(MarkerShape::from_name),
                    size: m.child("size").and_then(Node::num).map(|v| v.clamp(2.0, 72.0) as u8),
                };
            }
            format.smooth = ser.child("smooth").is_some_and(Node::flag);
            format.points = ser
                .children("dPt")
                .filter_map(|p| {
                    let i = p.child("idx")?.num()? as u32;
                    match paint_in(p.child("spPr")?, theme) {
                        Paint::Solid(c) => Some((i, c)),
                        _ => None,
                    }
                })
                .collect();
            format.labels = ser.child("dLbls").map(labels_of).or(group_labels).unwrap_or_default();
            lossy |= ser.child("dLbls").is_some_and(|d| d.child("dLbl").is_some());
            let trends: Vec<&Node> = ser.children("trendline").collect();
            lossy |= trends.len() > 1;
            format.trendline = trends.first().and_then(|t| {
                let kind = match t.child("trendlineType").and_then(Node::val)? {
                    "linear" => TrendKind::Linear,
                    "exp" => TrendKind::Exponential,
                    "log" => TrendKind::Logarithmic,
                    "power" => TrendKind::Power,
                    "poly" => TrendKind::Polynomial(t.child("order").and_then(Node::num).map_or(2, |v| v.clamp(2.0, 6.0) as u8)),
                    "movingAvg" => TrendKind::MovingAverage(t.child("period").and_then(Node::num).map_or(2, |v| v.clamp(2.0, 255.0) as u8)),
                    _ => return None,
                };
                Some(Trendline { kind, equation: t.child("dispEq").is_some_and(Node::flag), r_squared: t.child("dispRSqr").is_some_and(Node::flag), name: t.child("name").map(|n| n.text.clone()) })
            });
            let bars: Vec<&Node> = ser.children("errBars").filter(|e| e.child("errDir").and_then(Node::val) != Some("x")).collect();
            lossy |= ser.children("errBars").count() > bars.len();
            format.error_bars = bars.first().and_then(|e| {
                let kind = match e.child("errValType").and_then(Node::val)? {
                    "fixedVal" => ErrorKind::Fixed,
                    "percentage" => ErrorKind::Percent,
                    "stdDev" => ErrorKind::StdDev,
                    "stdErr" => ErrorKind::StdErr,
                    _ => {
                        lossy = true;
                        return None;
                    }
                };
                let dir = match e.child("errBarType").and_then(Node::val) {
                    Some("plus") => ErrorDir::Plus,
                    Some("minus") => ErrorDir::Minus,
                    _ => ErrorDir::Both,
                };
                Some(ErrorBars { kind, value: e.child("val").and_then(Node::num).unwrap_or(0.0), dir })
            });
            series.push((order, ChartSeries { name, categories, values, sizes, format }));
        }
    }
    let kind = kind?;
    // Legend order is the series' order, whatever group they're in.
    series.sort_by(|a, b| a.0.total_cmp(&b.0));
    let axis = |id: Option<&String>| id.and_then(|id| axes.iter().find(|(a, _)| a == id)).map(|(_, n)| axis_of(n)).unwrap_or_default();
    if kind.is_round() {
        // No axes: they keep what a new chart has (shown if the kind changes).
        let d = ChartOptions::default();
        (options.x_axis, options.y_axis) = (d.x_axis, d.y_axis);
    } else {
        options.x_axis = axis(primary.first());
        options.y_axis = axis(primary.get(1));
        options.y2_axis = secondary_ids.as_ref().map_or_else(Axis::default, |ids| axis(ids.get(1)));
    }
    Some(ParsedChart { kind, stacked, title, series: series.into_iter().map(|s| s.1).collect(), options, lossy })
}

/// Histogram and waterfall charts (Office 2016 chartex parts).
fn parse_chartex(root: &Node, wb: &Workbook, theme: &[xl_engine::Rgb]) -> Option<ParsedChart> {
    let expr = |src: &str| parse_formula(src.trim().trim_start_matches('='), wb.sheets()).ok().map(Arc::new);
    // Data blocks by id: (categories, values).
    let data: Vec<(&str, Option<String>, Option<String>)> = root
        .child("chartData")?
        .children("data")
        .map(|d| {
            let dim = |tag: &str, ty: &str| d.children(tag).find(|n| n.attr("type") == Some(ty)).and_then(|n| n.child("f")).map(|f| f.text.clone());
            (d.attr("id").unwrap_or("0"), dim("strDim", "cat").or_else(|| dim("numDim", "cat")), dim("numDim", "val"))
        })
        .collect();
    let chart = root.child("chart")?;
    let text_of = |n: &Node| -> Option<String> {
        let t = match n.path(&["tx", "txData", "v"]) {
            Some(v) => v.text.clone(),
            None => n.runs(),
        };
        let t = t.trim().to_string();
        (!t.is_empty()).then_some(t)
    };
    let title = chart.child("title").and_then(text_of);
    let mut options = ChartOptions::blank();
    let mut lossy = false;
    options.legend = chart.child("legend").map(|l| match (l.attr("pos"), l.attr("align")) {
        (Some("b"), _) => LegendPos::Bottom,
        (Some("t"), Some("max")) => LegendPos::TopRight,
        (Some("t"), _) => LegendPos::Top,
        (Some("l"), _) => LegendPos::Left,
        _ => LegendPos::Right,
    });
    let plot = chart.child("plotArea")?;
    let mut kind = None;
    let mut series = Vec::new();
    for s in plot.child("plotAreaRegion")?.children("series") {
        let layout = s.child("layoutPr");
        let k = match s.attr("layoutId")? {
            "clusteredColumn" if layout.is_none_or(|l| l.child("aggregation").is_none()) => ChartKind::Histogram,
            "waterfall" => ChartKind::Waterfall,
            _ => return None,
        };
        if kind.is_some_and(|prev| prev != k) {
            return None;
        }
        kind = Some(k);
        let id = s.child("dataId").and_then(Node::val).unwrap_or("0");
        let (_, cats, vals) = data.iter().find(|d| d.0 == id)?;
        let Some(values) = vals.as_deref().and_then(expr) else { continue };
        let name = s.path(&["tx", "txData"]).and_then(|t| match t.child("f") {
            Some(f) => expr(&f.text),
            None => t.child("v").map(|v| Arc::new(xl_engine::formula::Expr::Text(v.text.clone().into()))),
        });
        let mut format = SeriesFormat::default();
        if let Some(sp) = s.child("spPr") {
            shape_props(sp, &mut format, theme);
        }
        format.points = s
            .children("dataPt")
            .filter_map(|p| {
                let i = p.attr("idx")?.parse().ok()?;
                match paint_in(p.child("spPr")?, theme) {
                    Paint::Solid(c) => Some((i, c)),
                    _ => None,
                }
            })
            .collect();
        if let Some(v) = s.path(&["dataLabels", "visibility"]) {
            let on = |k: &str| v.attr(k).is_some_and(|x| x == "1" || x == "true");
            format.labels = DataLabels { value: on("value"), category: on("categoryName"), series: on("seriesName"), percent: false };
        } else if s.child("dataLabels").is_some() {
            format.labels.value = true;
        }
        if let Some(l) = layout {
            if let Some(b) = l.child("binning") {
                let edge = |k: &str| b.attr(k).and_then(|v| v.parse::<f64>().ok());
                options.bins.underflow = edge("underflow");
                options.bins.overflow = edge("overflow");
                options.bins.mode = match (b.child("binSize").and_then(Node::num), b.child("binCount").and_then(Node::num)) {
                    (Some(w), _) if w > 0.0 => BinMode::Width(w),
                    (_, Some(c)) if c >= 1.0 => BinMode::Count(c as u32),
                    _ => BinMode::Auto,
                };
            }
            if let Some(t) = l.child("subtotals") {
                options.subtotals = t.children("idx").filter_map(|i| i.num()).map(|v| v as u32).collect();
            }
            lossy |= l.child("statistics").is_some();
        }
        series.push(ChartSeries { name, categories: cats.as_deref().and_then(expr), values, sizes: None, format });
    }
    for a in plot.children("axis") {
        let mut ax = Axis {
            title: a.child("title").and_then(text_of),
            gridlines: a.child("majorGridlines").is_some(),
            minor_gridlines: a.child("minorGridlines").is_some(),
            hidden: a.attr("hidden").is_some_and(|v| v == "1" || v == "true"),
            num_fmt: a.child("numFmt").filter(|f| f.attr("sourceLinked") != Some("1")).and_then(|f| f.attr("formatCode")).map(str::to_string),
            ..Axis::default()
        };
        if let Some(v) = a.child("valScaling") {
            let n = |k: &str| v.attr(k).and_then(|x| x.parse::<f64>().ok());
            (ax.min, ax.max, ax.major) = (n("min"), n("max"), n("majorUnit"));
            options.y_axis = ax;
        } else {
            options.x_axis = ax;
        }
    }
    lossy |= root.any(&|n| n.name == "txPr");
    Some(ParsedChart { kind: kind?, stacked: false, title, series, options, lossy })
}

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn resolves_relative_part_paths() {
        assert_eq!(resolve("xl/worksheets/sheet1.xml", "../drawings/drawing1.xml"), "xl/drawings/drawing1.xml");
        assert_eq!(resolve("xl/drawings/drawing1.xml", "../charts/chart3.xml"), "xl/charts/chart3.xml");
        assert_eq!(resolve("xl/worksheets/sheet1.xml", "/xl/drawings/d.xml"), "xl/drawings/d.xml");
    }

    #[test]
    fn scheme_colors_with_luminance() {
        let theme = [0xFFFFFF, 0x000000, 0xE7E6E6, 0x44546A, 0x4472C4, 0xED7D31, 0xA5A5A5, 0xFFC000, 0x5B9BD5, 0x70AD47, 0x0563C1, 0x954F72];
        let n = super::dom(b"<a:schemeClr xmlns:a=\"x\" val=\"accent2\"/>").unwrap();
        assert_eq!(super::color(&n, &theme), Some(0xED7D31));
        // 75% luminance of white is Excel's "darker 25%" gray.
        let n = super::dom(b"<a:schemeClr xmlns:a=\"x\" val=\"bg1\"><a:lumMod val=\"75000\"/></a:schemeClr>").unwrap();
        assert_eq!(super::color(&n, &theme), Some(0xBFBFBF));
    }
}
