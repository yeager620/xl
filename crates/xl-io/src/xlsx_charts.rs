//! Reading charts from xlsx: worksheet → drawing (anchors) → chart parts
//! (kind, series references, title).

use std::sync::Arc;

use quick_xml::events::Event;
use quick_xml::XmlVersion;
use xl_engine::chart::{Chart, ChartKind, ChartSeries};
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
    /// Charts with colors, labels, trendlines and the like, which xl drops.
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

/// Chart XML that carries formatting beyond kind, series and title.
const STYLING: &[&str] = &["<c:spPr", "<c:txPr", "<c:dLbls", "<c:trendline", "<c:errBars", "<c:dTable", "<c:dPt", "<c:style ", "<a:solidFill"];

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
        loop {
            match r.read_event_into(&mut buf).map_err(|e| e.to_string())? {
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
                        if STYLING.iter().any(|m| chart_xml.windows(m.len()).any(|w| w == m.as_bytes())) {
                            notes.styled_charts = true;
                        }
                        match parse_chart(&chart_xml, wb) {
                            Some((kind, stacked, title, series)) if !series.is_empty() => {
                                let chart = Chart {
                                    kind,
                                    title,
                                    series,
                                    anchor: CellRef::new(from.row, from.col),
                                    offset: (from_excel_width_px(from.col_off), from_excel_height(from.row_off * 0.75)),
                                    size: (from_excel_width_px(w).max(80.0), from_excel_height(h * 0.75).max(60.0)),
                                    stacked,
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

type ParsedChart = (ChartKind, bool, Option<String>, Vec<ChartSeries>);

/// Kind, stacking, title and series of a chart part. None if it's a kind xl
/// doesn't draw (or mixes several).
fn parse_chart(bytes: &[u8], wb: &Workbook) -> Option<ParsedChart> {
    let mut r = xml(bytes);
    let mut kind: Option<ChartKind> = None;
    let mut kinds = 0;
    let mut stacked = false;
    let mut bar_dir_col = true;
    let mut series: Vec<ChartSeries> = Vec::new();
    // Inside a <c:ser>: which part (tx, cat, val, xVal, yVal) and its formula / literal.
    let mut part = String::new();
    let mut in_ser = false;
    let (mut name, mut cats, mut vals): (Option<String>, Option<String>, Option<String>) = (None, None, None);
    let mut literal_name: Option<String> = None;
    let mut capture: Option<&'static str> = None;
    let mut text = String::new();
    // Title: only the chart's own (not an axis title), from <a:t> runs.
    let mut depth_title: Option<usize> = None;
    let mut depth = 0usize;
    let mut stack: Vec<String> = Vec::new();
    let mut title = String::new();
    let mut auto_title_deleted = false;
    let mut buf = Vec::new();
    loop {
        let ev = r.read_event_into(&mut buf).ok()?;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(ev, Event::Empty(_));
                let n = local(e.name().as_ref()).to_string();
                match n.as_str() {
                    "barChart" | "bar3DChart" | "lineChart" | "line3DChart" | "areaChart" | "area3DChart" | "pieChart" | "pie3DChart" | "doughnutChart" | "scatterChart" => {
                        kinds += 1;
                        kind = Some(match n.as_str() {
                            "barChart" | "bar3DChart" => ChartKind::Column,
                            "lineChart" | "line3DChart" => ChartKind::Line,
                            "areaChart" | "area3DChart" => ChartKind::Area,
                            "scatterChart" => ChartKind::Scatter,
                            _ => ChartKind::Pie,
                        });
                    }
                    "radarChart" | "stockChart" | "bubbleChart" | "surfaceChart" | "surface3DChart" | "ofPieChart" => return None,
                    "barDir" => bar_dir_col = attr(e, "val").as_deref() != Some("bar"),
                    "grouping" => stacked |= matches!(attr(e, "val").as_deref(), Some("stacked" | "percentStacked")),
                    "autoTitleDeleted" => auto_title_deleted = attr(e, "val").as_deref() != Some("0"),
                    "title" if stack.last().is_some_and(|p| p == "chart") => depth_title = Some(depth),
                    "ser" => {
                        in_ser = true;
                        (name, cats, vals, literal_name) = (None, None, None, None);
                    }
                    p @ ("tx" | "cat" | "val" | "xVal" | "yVal") if in_ser => part = p.to_string(),
                    "f" if in_ser => {
                        capture = Some("f");
                        text.clear();
                    }
                    "v" if in_ser && part == "tx" => {
                        capture = Some("v");
                        text.clear();
                    }
                    "t" if depth_title.is_some() => {
                        capture = Some("t");
                        text.clear();
                    }
                    _ => {}
                }
                if !empty {
                    depth += 1;
                    stack.push(n);
                }
            }
            Event::Text(ref t) if capture.is_some() => text.push_str(&t.xml_content(XmlVersion::Implicit1_0)),
            Event::GeneralRef(ref g) if capture.is_some() => push_entity(&mut text, g),
            Event::End(ref e) => {
                let n = local(e.name().as_ref()).to_string();
                depth = depth.saturating_sub(1);
                stack.pop();
                match (n.as_str(), capture) {
                    ("f", Some("f")) => {
                        let f = std::mem::take(&mut text);
                        match part.as_str() {
                            "tx" => name = Some(f),
                            "cat" | "xVal" => cats = Some(f),
                            _ => vals = Some(f),
                        }
                        capture = None;
                    }
                    ("v", Some("v")) => {
                        literal_name = Some(std::mem::take(&mut text));
                        capture = None;
                    }
                    ("t", Some("t")) => {
                        title.push_str(&text);
                        capture = None;
                    }
                    _ => {}
                }
                match n.as_str() {
                    "title" if depth_title == Some(depth) => depth_title = None,
                    "tx" | "cat" | "val" | "xVal" | "yVal" if in_ser => part.clear(),
                    "ser" => {
                        in_ser = false;
                        let expr = |src: &str| parse_formula(src.trim_start_matches('='), wb.sheets()).ok().map(Arc::new);
                        if let Some(values) = vals.as_deref().and_then(expr) {
                            let name = name.as_deref().and_then(expr).or_else(|| literal_name.take().map(|t| Arc::new(xl_engine::formula::Expr::Text(t.into()))));
                            series.push(ChartSeries { name, categories: cats.as_deref().and_then(expr), values });
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if kinds != 1 {
        return None;
    }
    let mut kind = kind?;
    if kind == ChartKind::Column && !bar_dir_col {
        kind = ChartKind::Bar;
    }
    let _ = auto_title_deleted;
    let title = (!title.trim().is_empty()).then(|| title.trim().to_string());
    Some((kind, stacked, title, series))
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
}
