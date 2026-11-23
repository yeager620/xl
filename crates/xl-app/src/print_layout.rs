//! Printed pages as drawing lists: each sheet's print range is paginated
//! (xl_engine::page) and every page becomes a list of rectangles, lines,
//! text runs, paths and images in points, with the origin at the page's
//! top-left. The PDF writer (pdf.rs) and the print preview both draw these
//! lists, so what's previewed is what's exported.
//!
//! Text is measured with the bundled Source Sans 3 faces (the ones embedded
//! in the PDF), so wrapping and overflow match the output.

use std::sync::{Arc, OnceLock};

use xl_engine::chart::{ChartKind, ChartData};
use xl_engine::cond::{CfResult, CondView};
use xl_engine::page::{COL_PX_TO_PT, HfContext, PT_PER_INCH, Page, PageSetup, Pagination, ROW_PX_TO_PT, paginate, render_header};
use xl_engine::picture::PictureFormat;
use xl_engine::style::luminance;
use xl_engine::{Addr, CellRef, FmtColor, HAlign, LineStyle, Range, Rgb, SheetId, Style, VAlign, Value, Workbook, col_name};

use crate::app::{DEF_COL_W, ROW_H};

// ------------------------------------------------------------------ fonts

pub const FONT_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/SourceSans3-Regular.ttf");
pub const FONT_ITALIC: &[u8] = include_bytes!("../../../assets/fonts/SourceSans3-It.ttf");
pub const FONT_BOLD: &[u8] = include_bytes!("../../../assets/fonts/SourceSans3-Bold.ttf");
pub const FONT_BOLD_ITALIC: &[u8] = include_bytes!("../../../assets/fonts/SourceSans3-BoldIt.ttf");

/// The four faces: regular, italic, bold, bold italic.
pub const FACES: [&[u8]; 4] = [FONT_REGULAR, FONT_ITALIC, FONT_BOLD, FONT_BOLD_ITALIC];

pub fn face_of(bold: bool, italic: bool) -> usize {
    (bold as usize) * 2 + italic as usize
}

struct Metrics {
    upem: f64,
    ascent: f64,
    descent: f64,
    /// Advance per char for the common range, in font units (0 = missing).
    ascii: Vec<u16>,
    face: ttf_parser::Face<'static>,
}

fn metrics(face: usize) -> &'static Metrics {
    // Faces are measured once and kept (system faces are added by
    // crate::fonts as print jobs use them).
    static M: OnceLock<std::sync::RwLock<Vec<Option<&'static Metrics>>>> = OnceLock::new();
    let cache = M.get_or_init(Default::default);
    if let Some(Some(m)) = cache.read().unwrap().get(face) {
        return m;
    }
    let (data, index) = crate::fonts::face_data(face);
    let face_ = ttf_parser::Face::parse(data, index).or_else(|_| ttf_parser::Face::parse(FACES[0], 0)).expect("bundled font parses");
    let adv = |c: char| face_.glyph_index(c).and_then(|g| face_.glyph_hor_advance(g)).unwrap_or(0);
    let ascii = (0u8..=255).map(|b| adv(b as char)).collect();
    let m: &'static Metrics = Box::leak(Box::new(Metrics { upem: face_.units_per_em() as f64, ascent: face_.ascender() as f64, descent: -(face_.descender() as f64), ascii, face: face_ }));
    let mut w = cache.write().unwrap();
    if w.len() <= face {
        w.resize(face + 1, None);
    }
    w[face].get_or_insert(m)
}

/// Width of `s` in points at `size` points.
pub fn text_width(s: &str, face: usize, size: f64) -> f64 {
    let m = metrics(face);
    let mut units = 0.0;
    for c in s.chars() {
        let a = if (c as u32) < 256 { m.ascii[c as usize] } else { m.face.glyph_index(c).and_then(|g| m.face.glyph_hor_advance(g)).unwrap_or(0) };
        units += if a == 0 { m.upem * 0.5 } else { a as f64 };
    }
    units / m.upem * size
}

/// Ascent and descent (both positive) of a face at `size` points.
pub fn ascent_descent(face: usize, size: f64) -> (f64, f64) {
    let m = metrics(face);
    (m.ascent / m.upem * size, m.descent / m.upem * size)
}

/// Break text into lines no wider than `width` (explicit breaks, then
/// words, then characters for words wider than a line).
pub fn wrap_text(text: &str, face: usize, size: f64, width: f64) -> Vec<String> {
    let w = |s: &str| text_width(s, face, size);
    let mut out = Vec::new();
    for para in text.split(['\n', '\u{2028}']) {
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if line.is_empty() || w(&candidate) <= width {
                line = candidate;
            } else {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
            }
            while line.chars().count() > 1 && w(&line) > width {
                let mut cut = line.len();
                loop {
                    cut = line[..cut].char_indices().last().map_or(0, |(i, _)| i);
                    if cut == 0 || w(&line[..cut]) <= width {
                        break;
                    }
                }
                if cut == 0 {
                    cut = line.char_indices().nth(1).map_or(line.len(), |(i, _)| i);
                }
                out.push(line[..cut].to_string());
                line = line[cut..].to_string();
            }
        }
        out.push(line);
    }
    out
}

// ------------------------------------------------------------------ items

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w: w.max(0.0), h: h.max(0.0) }
    }

    fn intersect(self, o: Rect) -> Rect {
        let (x0, y0) = (self.x.max(o.x), self.y.max(o.y));
        let (x1, y1) = ((self.x + self.w).min(o.x + o.w), (self.y + self.h).min(o.y + o.h));
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
}

#[derive(Clone, Debug)]
pub enum Item {
    Rect { r: Rect, color: Rgb },
    Line { a: (f64, f64), b: (f64, f64), width: f64, color: Rgb, dash: Option<f64> },
    /// One line of text; `y` is the baseline.
    Text { x: f64, y: f64, text: String, face: usize, size: f64, color: Rgb, underline: bool, strike: bool, clip: Option<Rect> },
    Path { points: Vec<(f64, f64)>, closed: bool, fill: Option<Rgb>, stroke: Option<(Rgb, f64)>, clip: Option<Rect> },
    Image { r: Rect, data: Arc<[u8]>, format: PictureFormat, clip: Option<Rect> },
}

#[derive(Clone, Debug)]
pub struct PageDraw {
    pub width: f64,
    pub height: f64,
    pub items: Vec<Item>,
}

/// Every page of a print job.
#[derive(Clone, Debug, Default)]
pub struct PrintDoc {
    pub pages: Vec<PageDraw>,
    /// The scale each sheet printed at (1.0 = 100%), by sheet.
    pub scales: Vec<(SheetId, f64)>,
    /// Pages left out over the limit.
    pub truncated: bool,
}

/// File-level fields for headers and footers.
#[derive(Clone, Debug, Default)]
pub struct DocInfo {
    pub file: String,
    pub path: String,
    pub date: String,
    pub time: String,
}

/// Most pages laid out at once (beyond this the job is cut short).
pub const MAX_PAGES: usize = 5000;

// ------------------------------------------------------------------ sizes

pub fn col_pt(wb: &Workbook, sheet: SheetId, c: u32) -> f64 {
    let l = wb.layout(sheet);
    if l.hidden_cols.contains(&c) { 0.0 } else { l.col_widths.get(&c).copied().unwrap_or(DEF_COL_W) * COL_PX_TO_PT }
}

pub fn row_pt(wb: &Workbook, sheet: SheetId, r: u32) -> f64 {
    let l = wb.layout(sheet);
    if l.hidden_rows.contains(&r) || l.filtered_rows.contains(&r) { 0.0 } else { l.row_heights.get(&r).copied().unwrap_or(ROW_H) * ROW_PX_TO_PT }
}

/// The column (row) a distance in points from the left (top) edge falls
/// in, and the offset within it.
fn line_at(dist: f64, size: &dyn Fn(u32) -> f64, limit: u32) -> (u32, f64) {
    let mut pos = 0.0;
    let mut i = 0;
    while i + 1 < limit {
        let s = size(i);
        if pos + s > dist {
            return (i, dist - pos);
        }
        pos += s;
        i += 1;
    }
    (i, dist - pos)
}

/// A chart's or picture's box in sheet points: (left, top, width, height).
fn object_box(wb: &Workbook, sheet: SheetId, anchor: CellRef, offset: (f64, f64), size: (f64, f64), end: Option<(CellRef, (f64, f64))>) -> (f64, f64, f64, f64) {
    let x_of = |c: u32| (0..c).map(|i| col_pt(wb, sheet, i)).sum::<f64>();
    let y_of = |r: u32| (0..r).map(|i| row_pt(wb, sheet, i)).sum::<f64>();
    let x = x_of(anchor.col) + offset.0 * COL_PX_TO_PT;
    let y = y_of(anchor.row) + offset.1 * ROW_PX_TO_PT;
    match end {
        Some((e, (dx, dy))) => {
            let x2 = x_of(e.col) + dx * COL_PX_TO_PT;
            let y2 = y_of(e.row) + dy * ROW_PX_TO_PT;
            (x, y, (x2 - x).max(1.0), (y2 - y).max(1.0))
        }
        None => (x, y, size.0 * COL_PX_TO_PT, size.1 * ROW_PX_TO_PT),
    }
}

/// What prints when there's no print area: from A1 to the last cell with a
/// value, fill, border or merge, or covered by a chart or picture.
pub fn print_range(wb: &Workbook, sheet: SheetId) -> Option<Range> {
    if let Some(a) = wb.layout(sheet).page.print_area {
        return Some(a);
    }
    let mut end: Option<CellRef> = wb.store(sheet).extent();
    let mut grow = |c: CellRef| {
        end = Some(match end {
            Some(e) => CellRef::new(e.row.max(c.row), e.col.max(c.col)),
            None => c,
        })
    };
    for (c, st) in wb.styled_cells(sheet) {
        if st.fill.is_some() || st.has_borders() {
            grow(c);
        }
    }
    let l = wb.layout(sheet);
    for m in &l.merges {
        grow(m.end);
    }
    let boxes: Vec<(f64, f64, f64, f64)> = l
        .charts
        .iter()
        .map(|c| object_box(wb, sheet, c.anchor, c.offset, c.size, None))
        .chain(l.pictures.iter().map(|p| object_box(wb, sheet, p.anchor, p.offset, p.size, p.end)))
        .collect();
    for (x, y, w, h) in boxes {
        let (c, _) = line_at(x + w - 0.01, &|i| col_pt(wb, sheet, i), xl_engine::MAX_COLS);
        let (r, _) = line_at(y + h - 0.01, &|i| row_pt(wb, sheet, i), xl_engine::MAX_ROWS);
        grow(CellRef::new(r, c));
    }
    end.map(|e| Range::new(CellRef::new(0, 0), e))
}

/// Size of the printed row-number column and column-letter row (points,
/// unscaled) when headings print.
fn heading_size(area: Range) -> (f64, f64) {
    let digits = (area.end.row + 1).to_string();
    (text_width(&digits, 0, 9.0) + 8.0, 13.0)
}

/// Paginate a sheet's print range (None if there's nothing to print).
pub fn paginate_sheet(wb: &Workbook, sheet: SheetId) -> Option<Pagination> {
    let setup = &wb.layout(sheet).page;
    let area = print_range(wb, sheet)?;
    let extra = if setup.headings { heading_size(area) } else { (0.0, 0.0) };
    Some(paginate(setup, area, &|c| col_pt(wb, sheet, c), &|r| row_pt(wb, sheet, r), extra))
}

// ------------------------------------------------------------------ layout

/// Lay out the given sheets' pages, numbered continuously.
pub fn layout(wb: &Workbook, sheets: &[SheetId], info: &DocInfo) -> PrintDoc {
    let mut doc = PrintDoc::default();
    let mut plans: Vec<(SheetId, Pagination)> = Vec::new();
    for &s in sheets {
        if let Some(p) = paginate_sheet(wb, s) {
            doc.scales.push((s, p.scale));
            plans.push((s, p));
        }
    }
    let total: usize = plans.iter().map(|(_, p)| p.pages.len()).sum();
    let mut number = plans.first().and_then(|(s, _)| wb.layout(*s).page.first_page).unwrap_or(1);
    'outer: for (s, plan) in &plans {
        let mut sheet = SheetPainter::new(wb, *s, plan);
        for page in &plan.pages {
            if doc.pages.len() >= MAX_PAGES {
                doc.truncated = true;
                break 'outer;
            }
            let cx = HfContext {
                page: number,
                pages: total as u32,
                date: info.date.clone(),
                time: info.time.clone(),
                file: info.file.clone(),
                path: info.path.clone(),
                sheet: wb.sheet_name(*s).to_string(),
            };
            doc.pages.push(sheet.page(page, &cx));
            number += 1;
        }
    }
    doc
}

/// Excel's automatic text color on a fill, else black.
fn text_color(st: &Style) -> Rgb {
    match (st.color, st.fill) {
        (Some(c), _) => c,
        (None, Some(f)) if luminance(f) <= 0.35 => 0xFFFFFF,
        _ => 0x000000,
    }
}

fn fmt_color(c: FmtColor) -> Rgb {
    match c {
        FmtColor::Red => 0xFF0000,
        FmtColor::Black => 0x000000,
        FmtColor::White => 0xFFFFFF,
        FmtColor::Green => 0x008000,
        FmtColor::Blue => 0x0000FF,
        FmtColor::Yellow => 0xB8860B,
        FmtColor::Magenta => 0xFF00FF,
        FmtColor::Cyan => 0x008B8B,
    }
}

const GRID: Rgb = 0xB0B0B0;
const HEADING_FILL: Rgb = 0xF0F0F0;
const SERIES: [Rgb; 6] = [0x4472C4, 0xED7D31, 0xA5A5A5, 0xFFC000, 0x5B9BD5, 0x70AD47];
/// Cell padding: about Excel's 2px.
const PAD: f64 = 1.5;

/// One sheet being laid out page by page.
struct SheetPainter<'a> {
    wb: &'a Workbook,
    sheet: SheetId,
    setup: &'a PageSetup,
    scale: f64,
    heading: (f64, f64),
    cond: Option<CondView<'a>>,
}

/// A run of lines on a page: (line index, position, size) in unscaled
/// points from the grid's top-left.
type Lines = Vec<(u32, f64, f64)>;

impl<'a> SheetPainter<'a> {
    fn new(wb: &'a Workbook, sheet: SheetId, plan: &Pagination) -> Self {
        let setup = &wb.layout(sheet).page;
        let heading = if setup.headings { heading_size(plan.area) } else { (0.0, 0.0) };
        let cond = (!wb.layout(sheet).cond.is_empty()).then(|| CondView::new(wb, sheet));
        SheetPainter { wb, sheet, setup, scale: plan.scale, heading, cond }
    }

    fn lines(spans: &[(u32, u32)], size: &dyn Fn(u32) -> f64) -> Lines {
        let mut out = Vec::new();
        let mut pos = 0.0;
        for &(a, b) in spans {
            for i in a..=b {
                let s = size(i);
                if s > 0.0 {
                    out.push((i, pos, s));
                    pos += s;
                }
            }
        }
        out
    }

    /// A cell's style with its table look and conditional formatting.
    fn styled(&self, cell: CellRef, cf: &CfResult) -> Style {
        let mut st = self.wb.style(Addr::new(self.sheet, cell)).clone();
        if let Some(tb) = self.wb.layout(self.sheet).tables.iter().find(|t| t.range.contains(cell)) {
            let (header, band) = crate::data::table_colors(tb.style, false);
            if cell.row == tb.range.start.row {
                st.fill = st.fill.or(Some(header));
                st.color = st.color.or(Some(0xFFFFFF));
                st.bold = true;
            } else if tb.banded && (cell.row - tb.range.start.row).is_multiple_of(2) && st.fill.is_none() {
                st.fill = Some(band);
            }
        }
        st.fill = cf.fill.or(st.fill);
        st.color = cf.color.or(st.color);
        st.bold |= cf.bold;
        st.italic |= cf.italic;
        if self.setup.black_and_white {
            st.fill = None;
            st.color = None;
        }
        st
    }

    fn page(&mut self, page: &Page, cx: &HfContext) -> PageDraw {
        let (pw, ph) = self.setup.page_size();
        let (px0, py0, avail_w, avail_h) = self.setup.printable();
        let s = self.scale;
        let wb = self.wb;
        let sheet = self.sheet;
        let col_size = |c: u32| col_pt(wb, sheet, c);
        let row_size = |r: u32| row_pt(wb, sheet, r);
        let mut col_spans = Vec::new();
        col_spans.extend(page.title_cols);
        col_spans.push(page.cols);
        let mut row_spans = Vec::new();
        row_spans.extend(page.title_rows);
        row_spans.push(page.rows);
        let cols = Self::lines(&col_spans, &col_size);
        let rows = Self::lines(&row_spans, &row_size);
        let grid_w: f64 = cols.iter().map(|c| c.2).sum();
        let grid_h: f64 = rows.iter().map(|r| r.2).sum();
        let (hw, hh) = self.heading;
        let mut ox = px0;
        let mut oy = py0;
        if self.setup.center_h {
            ox += ((avail_w - (grid_w + hw) * s) / 2.0).max(0.0);
        }
        if self.setup.center_v {
            oy += ((avail_h - (grid_h + hh) * s) / 2.0).max(0.0);
        }
        // Grid origin on the page, after the headings.
        let gx = ox + hw * s;
        let gy = oy + hh * s;
        let x = |u: f64| gx + u * s;
        let y = |u: f64| gy + u * s;
        let mut items = Vec::new();

        // Headings.
        if self.setup.headings {
            let face = 0;
            let size = 9.0 * s;
            items.push(Item::Rect { r: Rect::new(ox, oy, (hw + grid_w) * s, hh * s), color: HEADING_FILL });
            items.push(Item::Rect { r: Rect::new(ox, oy, hw * s, (hh + grid_h) * s), color: HEADING_FILL });
            let (asc, desc) = ascent_descent(face, size);
            for &(c, cxp, w) in &cols {
                let label = col_name(c);
                let tw = text_width(&label, face, size);
                items.push(Item::Text { x: x(cxp) + (w * s - tw) / 2.0, y: oy + (hh * s + asc - desc) / 2.0, text: label, face, size, color: 0x404040, underline: false, strike: false, clip: None });
                items.push(Item::Line { a: (x(cxp + w), oy), b: (x(cxp + w), gy), width: 0.5 * s, color: GRID, dash: None });
            }
            for &(r, ryp, h) in &rows {
                let label = (r + 1).to_string();
                let tw = text_width(&label, face, size);
                items.push(Item::Text { x: ox + (hw * s - tw) / 2.0, y: y(ryp) + (h * s + asc - desc) / 2.0, text: label, face, size, color: 0x404040, underline: false, strike: false, clip: None });
                items.push(Item::Line { a: (ox, y(ryp + h)), b: (gx, y(ryp + h)), width: 0.5 * s, color: GRID, dash: None });
            }
            items.push(Item::Line { a: (ox, gy), b: (x(grid_w), gy), width: 0.5 * s, color: GRID, dash: None });
            items.push(Item::Line { a: (gx, oy), b: (gx, y(grid_h)), width: 0.5 * s, color: GRID, dash: None });
        }

        // Gridlines.
        if self.setup.gridlines {
            let w = (0.5 * s).max(0.25);
            items.push(Item::Line { a: (gx, gy), b: (x(grid_w), gy), width: w, color: GRID, dash: None });
            items.push(Item::Line { a: (gx, gy), b: (gx, y(grid_h)), width: w, color: GRID, dash: None });
            for &(_, cxp, cw) in &cols {
                items.push(Item::Line { a: (x(cxp + cw), gy), b: (x(cxp + cw), y(grid_h)), width: w, color: GRID, dash: None });
            }
            for &(_, ryp, rh) in &rows {
                items.push(Item::Line { a: (gx, y(ryp + rh)), b: (x(grid_w), y(ryp + rh)), width: w, color: GRID, dash: None });
            }
        }

        // Merged ranges on this page: their visible span on the page.
        let layout = wb.layout(sheet);
        let mut merges: Vec<(Range, Rect)> = Vec::new();
        for m in &layout.merges {
            let ci: Vec<usize> = cols.iter().enumerate().filter(|(_, e)| (m.start.col..=m.end.col).contains(&e.0)).map(|(i, _)| i).collect();
            let ri: Vec<usize> = rows.iter().enumerate().filter(|(_, e)| (m.start.row..=m.end.row).contains(&e.0)).map(|(i, _)| i).collect();
            let (Some(&c0), Some(&c1), Some(&r0), Some(&r1)) = (ci.first(), ci.last(), ri.first(), ri.last()) else { continue };
            let (cx0, cx1) = (cols[c0].1, cols[c1].1 + cols[c1].2);
            let (ry0, ry1) = (rows[r0].1, rows[r1].1 + rows[r1].2);
            merges.push((*m, Rect::new(x(cx0), y(ry0), (cx1 - cx0) * s, (ry1 - ry0) * s)));
        }
        let in_merge = |c: CellRef| merges.iter().any(|(m, _)| m.contains(c));

        // Conditional formatting results for the cells on the page.
        let mut cf: std::collections::HashMap<CellRef, CfResult> = std::collections::HashMap::new();
        if let Some(view) = &self.cond {
            for &(r, _, _) in &rows {
                for &(c, _, _) in &cols {
                    let res = view.cell(CellRef::new(r, c));
                    if !res.is_empty() {
                        cf.insert(CellRef::new(r, c), res);
                    }
                }
            }
        }
        let none = CfResult::default();
        let style_at = |c: CellRef| self.styled(c, cf.get(&c).unwrap_or(&none));

        // Fills, then borders (so borders sit on neighbouring fills).
        let mut borders = Vec::new();
        let mut boxed = |items: &mut Vec<Item>, st: &Style, r: Rect, merged: bool| {
            if let Some(f) = st.fill {
                items.push(Item::Rect { r, color: f });
            } else if merged && self.setup.gridlines {
                // Gridlines don't cross merged cells.
                items.push(Item::Rect { r: Rect::new(r.x + 0.3, r.y + 0.3, r.w - 0.6, r.h - 0.6), color: 0xFFFFFF });
            }
            if st.has_borders() {
                edge_lines(&mut borders, st, r, s);
            }
        };
        for &(rr, ryp, rh) in &rows {
            for &(c, cxp, cw) in &cols {
                let cell = CellRef::new(rr, c);
                if in_merge(cell) {
                    continue;
                }
                let st = style_at(cell);
                if st.fill.is_some() || st.has_borders() {
                    boxed(&mut items, &st, Rect::new(x(cxp), y(ryp), cw * s, rh * s), false);
                }
            }
        }
        for (m, r) in &merges {
            let st = style_at(m.start);
            boxed(&mut items, &st, *r, true);
        }
        items.extend(borders);

        // Data bars.
        for &(rr, ryp, rh) in &rows {
            for &(c, cxp, cw) in &cols {
                if let Some((frac, color)) = cf.get(&CellRef::new(rr, c)).and_then(|res| res.bar) {
                    let len = ((cw - 4.0) * frac).max(1.0);
                    items.push(Item::Rect { r: Rect::new(x(cxp + 1.5), y(ryp + 2.0), len * s, (rh - 4.0).max(1.0) * s), color: lighten(color) });
                }
            }
        }

        // Text.
        let store = wb.store(sheet);
        let empty_at = |c: CellRef| store.value(c).is_empty() && !in_merge(c);
        for &(rr, ryp, rh) in &rows {
            for (ci, &(c, cxp, cw)) in cols.iter().enumerate() {
                let cell = CellRef::new(rr, c);
                if in_merge(cell) {
                    continue;
                }
                let st = style_at(cell);
                // Room to overflow into empty neighbours (left-aligned text).
                let mut extra = 0.0;
                for &(c2, _, w2) in &cols[ci + 1..] {
                    if !empty_at(CellRef::new(rr, c2)) {
                        break;
                    }
                    extra += w2;
                    if extra > 2000.0 {
                        break;
                    }
                }
                self.cell_text(&mut items, cell, &st, Rect::new(x(cxp), y(ryp), cw * s, rh * s), extra * s);
            }
        }
        for (m, r) in &merges {
            let st = style_at(m.start);
            self.cell_text(&mut items, m.start, &st, *r, 0.0);
        }

        // Charts and pictures over the cells, clipped to the page's band
        // (they don't repeat in the title area).
        let band_cols: Vec<&(u32, f64, f64)> = cols.iter().filter(|e| (page.cols.0..=page.cols.1).contains(&e.0)).collect();
        let band_rows: Vec<&(u32, f64, f64)> = rows.iter().filter(|e| (page.rows.0..=page.rows.1).contains(&e.0)).collect();
        if let (Some(fc), Some(lc), Some(fr), Some(lr)) = (band_cols.first(), band_cols.last(), band_rows.first(), band_rows.last()) {
            let sheet_x0: f64 = (0..fc.0).map(col_size).sum();
            let sheet_y0: f64 = (0..fr.0).map(row_size).sum();
            let clip = Rect::new(x(fc.1), y(fr.1), (lc.1 + lc.2 - fc.1) * s, (lr.1 + lr.2 - fr.1) * s);
            // Sheet points to page points.
            let to_page = |bx: f64, by: f64, bw: f64, bh: f64| Rect::new(x(fc.1 + bx - sheet_x0), y(fr.1 + by - sheet_y0), bw * s, bh * s);
            for p in &layout.pictures {
                if p.format == PictureFormat::Other {
                    continue;
                }
                let (bx, by, bw, bh) = object_box(wb, sheet, p.anchor, p.offset, p.size, p.end);
                let r = to_page(bx, by, bw, bh);
                if r.intersect(clip).w > 0.0 && r.intersect(clip).h > 0.0 {
                    items.push(Item::Image { r, data: p.data.clone(), format: p.format, clip: Some(clip) });
                }
            }
            for chart in &layout.charts {
                let (bx, by, bw, bh) = object_box(wb, sheet, chart.anchor, chart.offset, chart.size, None);
                let r = to_page(bx, by, bw, bh);
                if r.intersect(clip).w > 0.0 && r.intersect(clip).h > 0.0 {
                    let data = wb.chart_data(sheet, chart);
                    chart_items(&mut items, chart.kind, chart.stacked, chart.title.as_deref(), &data, r, s, clip);
                }
            }
        }

        // Header and footer (not scaled, like Excel's default).
        let m = &self.setup.margins;
        let hf_size = 10.0;
        let (asc, desc) = ascent_descent(0, hf_size);
        let line_h = hf_size * 1.25;
        let (left, right) = (m.left * PT_PER_INCH, pw - m.right * PT_PER_INCH);
        for (src, top) in [(&self.setup.header, true), (&self.setup.footer, false)] {
            if src.is_empty() {
                continue;
            }
            for (i, text) in render_header(src, cx).iter().enumerate() {
                let lines: Vec<&str> = text.split(['\n', '\r']).filter(|l| !l.is_empty()).collect();
                let n = lines.len() as f64;
                for (k, line) in lines.iter().enumerate() {
                    let tw = text_width(line, 0, hf_size);
                    let tx = match i {
                        0 => left,
                        1 => (pw - tw) / 2.0,
                        _ => right - tw,
                    };
                    let ty = if top { m.header * PT_PER_INCH + asc + k as f64 * line_h } else { ph - m.footer * PT_PER_INCH - desc - (n - 1.0 - k as f64) * line_h };
                    items.push(Item::Text { x: tx, y: ty, text: line.to_string(), face: 0, size: hf_size, color: 0x000000, underline: false, strike: false, clip: None });
                }
            }
        }
        PageDraw { width: pw, height: ph, items }
    }

    /// A cell's displayed text, aligned and wrapped like on screen.
    fn cell_text(&self, items: &mut Vec<Item>, cell: CellRef, st: &Style, r: Rect, overflow: f64) {
        let a = Addr::new(self.sheet, cell);
        let v = self.wb.value(a);
        if v.is_empty() {
            return;
        }
        let shown = self.wb.display(a);
        if shown.text.is_empty() {
            return;
        }
        let s = self.scale;
        let color = match (v, shown.color) {
            _ if self.setup.black_and_white => 0x000000,
            (Value::Error(_), _) => 0xC00000,
            (_, Some(fc)) => fmt_color(fc),
            _ => text_color(st),
        };
        let size = st.font_size.map_or(11.0, |t| t as f64 / 10.0) * s;
        let face = crate::fonts::face_for(st.font_name.as_deref(), st.bold, st.italic);
        let line_h = size * 15.0 / 11.0;
        let pad = PAD * s;
        let avail = (r.w - 2.0 * pad).max(1.0);
        let number = matches!(v, Value::Number(_));
        let align = match (st.align, v) {
            (HAlign::General, Value::Number(_)) => HAlign::Right,
            (HAlign::General, Value::Bool(_) | Value::Error(_)) => HAlign::Center,
            (HAlign::General, _) => HAlign::Left,
            (al, _) => al,
        };
        let wrapped = st.wrap && !number;
        let mut texts: Vec<String> = if wrapped { wrap_text(&shown.text, face, size, avail) } else { vec![shown.text.replace(['\n', '\u{2028}'], " ")] };
        if number && text_width(&texts[0], face, size) > avail {
            let hash = text_width("#", face, size).max(0.1);
            texts = vec!["#".repeat(((avail / hash).floor() as usize).max(1))];
        }
        let total = texts.len() as f64 * line_h;
        let top = match st.valign {
            VAlign::Top => r.y,
            VAlign::Center => r.y + (r.h - total) / 2.0,
            VAlign::Bottom => r.y + r.h - total,
        };
        let (asc, desc) = ascent_descent(face, size);
        let cell_clip = Rect::new(r.x, r.y, r.w, r.h);
        for (i, text) in texts.into_iter().enumerate() {
            let tw = text_width(&text, face, size);
            let (tx, clip) = match align {
                HAlign::Right => (r.x + r.w - pad - tw, cell_clip),
                HAlign::Center => (r.x + (r.w - tw) / 2.0, cell_clip),
                _ if number || wrapped => (r.x + pad, cell_clip),
                _ => (r.x + pad, Rect::new(r.x, r.y, r.w + if tw > avail { overflow } else { 0.0 }, r.h)),
            };
            let baseline = top + i as f64 * line_h + (line_h + asc - desc) / 2.0;
            items.push(Item::Text { x: tx, y: baseline, text, face, size, color, underline: st.underline, strike: st.strike, clip: Some(clip) });
        }
    }
}

fn lighten(c: Rgb) -> Rgb {
    let ch = |shift: u32| {
        let v = ((c >> shift) & 0xFF) as f64;
        ((v + (255.0 - v) * 0.4) as u32) << shift
    };
    ch(16) | ch(8) | ch(0)
}

/// Border lines for a cell box.
fn edge_lines(out: &mut Vec<Item>, st: &Style, r: Rect, s: f64) {
    let mut edge = |b: &xl_engine::Border, a: (f64, f64), z: (f64, f64), horizontal: bool| {
        if b.style == LineStyle::None {
            return;
        }
        let color = b.color.unwrap_or(0x000000);
        let width = match b.style {
            LineStyle::Medium => 1.0,
            LineStyle::Thick => 1.5,
            _ => 0.5,
        } * s;
        let dash = match b.style {
            LineStyle::Dashed => Some(3.0 * s),
            LineStyle::Dotted => Some(1.0 * s),
            _ => None,
        };
        if b.style == LineStyle::Double {
            let d = 1.0 * s;
            let (o1, o2) = if horizontal { ((0.0, -d), (0.0, d)) } else { ((-d, 0.0), (d, 0.0)) };
            out.push(Item::Line { a: (a.0 + o1.0, a.1 + o1.1), b: (z.0 + o1.0, z.1 + o1.1), width: 0.5 * s, color, dash: None });
            out.push(Item::Line { a: (a.0 + o2.0, a.1 + o2.1), b: (z.0 + o2.0, z.1 + o2.1), width: 0.5 * s, color, dash: None });
        } else {
            out.push(Item::Line { a, b: z, width, color, dash });
        }
    };
    let (x0, y0, x1, y1) = (r.x, r.y, r.x + r.w, r.y + r.h);
    edge(&st.top, (x0, y0), (x1, y0), true);
    edge(&st.bottom, (x0, y1), (x1, y1), true);
    edge(&st.left, (x0, y0), (x0, y1), false);
    edge(&st.right, (x1, y0), (x1, y1), false);
}

// ------------------------------------------------------------------ charts

/// About `target` tick intervals covering [lo, hi] on 1/2/2.5/5 × 10ⁿ steps.
fn ticks(lo: f64, hi: f64, target: usize) -> (f64, f64, f64) {
    let (lo, hi) = if hi > lo { (lo, hi) } else if hi == 0.0 { (0.0, 1.0) } else { (lo.min(0.0), hi.max(0.0) + hi.abs().max(1.0) * 0.1) };
    let raw = (hi - lo) / target.max(1) as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|m| m * mag).find(|st| *st >= raw).unwrap_or(10.0 * mag);
    ((lo / step).floor() * step, (hi / step).ceil() * step, step)
}

fn tick_label(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 {
        format!("{}B", xl_engine::precise((v / 1e9 * 100.0).round() / 100.0))
    } else if a >= 1e6 {
        format!("{}M", xl_engine::precise((v / 1e6 * 100.0).round() / 100.0))
    } else if a >= 1e4 {
        format!("{}K", xl_engine::precise((v / 1e3 * 100.0).round() / 100.0))
    } else {
        xl_engine::precise((v * 1e6).round() / 1e6)
    }
}

/// A chart drawn as vector items into `r` (page points).
#[allow(clippy::too_many_arguments)]
pub fn chart_items(out: &mut Vec<Item>, kind: ChartKind, stacked: bool, title: Option<&str>, data: &ChartData, r: Rect, s: f64, clip: Rect) {
    let clip = Some(clip);
    // Kinds the printer doesn't draw yet print as their nearest relative.
    let kind = match kind {
        ChartKind::Doughnut => ChartKind::Pie,
        ChartKind::Histogram | ChartKind::Waterfall => ChartKind::Column,
        ChartKind::Radar => ChartKind::Line,
        ChartKind::Bubble => ChartKind::Scatter,
        k => k,
    };
    let text = |out: &mut Vec<Item>, t: &str, x: f64, y: f64, size: f64, color: Rgb, bold: bool| {
        out.push(Item::Text { x, y, text: t.to_string(), face: face_of(bold, false), size, color, underline: false, strike: false, clip });
    };
    out.push(Item::Path { points: vec![(r.x, r.y), (r.x + r.w, r.y), (r.x + r.w, r.y + r.h), (r.x, r.y + r.h)], closed: true, fill: Some(0xFFFFFF), stroke: Some((0xC8C8C8, 0.75 * s)), clip });
    let pad = 8.0 * s;
    let mut top = r.y + pad;
    let title = title.map(str::to_string).or_else(|| (data.series.len() == 1).then(|| data.series[0].0.clone())).filter(|t| !t.is_empty());
    let tsize = 12.0 * s;
    if let Some(t) = &title {
        let tw = text_width(t, 2, tsize);
        text(out, t, r.x + (r.w - tw) / 2.0, top + tsize, tsize, 0x262626, true);
        top += tsize * 1.6;
    }
    let lsize = 8.0 * s;
    let pie = kind == ChartKind::Pie;
    // Legend along the bottom.
    let legend: Vec<String> = if pie { data.categories.clone() } else { data.series.iter().map(|x| x.0.clone()).collect() };
    let show_legend = pie || legend.len() > 1;
    let mut bottom = r.y + r.h - pad;
    if show_legend && !legend.is_empty() {
        let widths: Vec<f64> = legend.iter().map(|l| text_width(l, 0, lsize) + lsize * 2.2).collect();
        let total: f64 = widths.iter().sum();
        let mut lx = r.x + ((r.w - total) / 2.0).max(pad);
        let ly = bottom - lsize * 0.3;
        for (i, (l, w)) in legend.iter().zip(&widths).enumerate() {
            let c = SERIES[i % SERIES.len()];
            out.push(Item::Rect { r: Rect::new(lx, ly - lsize * 0.8, lsize * 0.8, lsize * 0.8), color: c });
            text(out, l, lx + lsize * 1.1, ly, lsize, 0x404040, false);
            lx += w;
        }
        bottom -= lsize * 2.0;
    }
    let (px0, py0, px1, py1) = (r.x + pad, top, r.x + r.w - pad, bottom);
    if px1 <= px0 + 4.0 || py1 <= py0 + 4.0 {
        return;
    }
    if pie {
        let Some((_, values)) = data.series.first() else { return };
        let vals: Vec<f64> = values.iter().map(|v| v.unwrap_or(0.0).max(0.0)).collect();
        let total: f64 = vals.iter().sum();
        if total <= 0.0 {
            return;
        }
        let (cx, cy) = ((px0 + px1) / 2.0, (py0 + py1) / 2.0);
        let rad = ((px1 - px0).min(py1 - py0) / 2.0) * 0.95;
        let mut a0 = -std::f64::consts::FRAC_PI_2;
        for (i, v) in vals.iter().enumerate() {
            let sweep = v / total * std::f64::consts::TAU;
            if sweep <= 0.0 {
                continue;
            }
            let steps = ((sweep / 0.05).ceil() as usize).max(2);
            let mut pts = vec![(cx, cy)];
            for k in 0..=steps {
                let a = a0 + sweep * k as f64 / steps as f64;
                pts.push((cx + rad * a.cos(), cy + rad * a.sin()));
            }
            out.push(Item::Path { points: pts, closed: true, fill: Some(SERIES[i % SERIES.len()]), stroke: Some((0xFFFFFF, 0.75 * s)), clip });
            a0 += sweep;
        }
        return;
    }
    // Value range.
    let n_cat = data.series.iter().map(|x| x.1.len()).max().unwrap_or(0).max(data.categories.len());
    let mut lo: f64 = 0.0;
    let mut hi: f64 = 0.0;
    if stacked && matches!(kind, ChartKind::Column | ChartKind::Bar | ChartKind::Area) {
        for i in 0..n_cat {
            let (mut pos, mut neg) = (0.0, 0.0);
            for (_, v) in &data.series {
                let x = v.get(i).copied().flatten().unwrap_or(0.0);
                if x >= 0.0 { pos += x } else { neg += x }
            }
            hi = hi.max(pos);
            lo = lo.min(neg);
        }
    } else {
        for (_, v) in &data.series {
            for x in v.iter().flatten() {
                hi = hi.max(*x);
                lo = lo.min(*x);
            }
        }
    }
    let (vlo, vhi, step) = ticks(lo, hi, 5);
    let horizontal = kind == ChartKind::Bar;
    // Axis labels take room on the value side and the category side.
    let labels: Vec<String> = {
        let mut v = Vec::new();
        let mut t = vlo;
        while t <= vhi + step * 0.5 {
            v.push(tick_label(t));
            t += step;
        }
        v
    };
    let label_w = labels.iter().map(|l| text_width(l, 0, lsize)).fold(0.0, f64::max) + 4.0 * s;
    let (ax0, ay1) = if horizontal {
        let cat_w = data.categories.iter().map(|c| text_width(c, 0, lsize)).fold(0.0, f64::max).min((px1 - px0) * 0.3) + 4.0 * s;
        (px0 + cat_w, py1 - lsize * 1.6)
    } else {
        (px0 + label_w, py1 - lsize * 1.6)
    };
    let (ax1, ay0) = (px1, py0);
    if ax1 <= ax0 || ay1 <= ay0 {
        return;
    }
    let span = (vhi - vlo).max(1e-12);
    // Gridlines and value labels.
    for (k, l) in labels.iter().enumerate() {
        let v = vlo + step * k as f64;
        let f = (v - vlo) / span;
        if horizontal {
            let gx = ax0 + f * (ax1 - ax0);
            out.push(Item::Line { a: (gx, ay0), b: (gx, ay1), width: 0.4 * s, color: 0xE0E0E0, dash: None });
            let tw = text_width(l, 0, lsize);
            text(out, l, gx - tw / 2.0, ay1 + lsize * 1.3, lsize, 0x595959, false);
        } else {
            let gy = ay1 - f * (ay1 - ay0);
            out.push(Item::Line { a: (ax0, gy), b: (ax1, gy), width: 0.4 * s, color: 0xE0E0E0, dash: None });
            let tw = text_width(l, 0, lsize);
            text(out, l, ax0 - tw - 3.0 * s, gy + lsize * 0.35, lsize, 0x595959, false);
        }
    }
    if n_cat == 0 {
        return;
    }
    let to_y = |v: f64| ay1 - (v - vlo) / span * (ay1 - ay0);
    let to_x = |v: f64| ax0 + (v - vlo) / span * (ax1 - ax0);
    let ns = data.series.len().max(1);
    match kind {
        ChartKind::Column | ChartKind::Bar => {
            let len = if horizontal { ay1 - ay0 } else { ax1 - ax0 };
            let slot = len / n_cat as f64;
            let group = slot * 0.7;
            let bar = if stacked { group } else { group / ns as f64 };
            for i in 0..n_cat {
                let (mut pos, mut neg) = (0.0, 0.0);
                for (si, (_, vals)) in data.series.iter().enumerate() {
                    let Some(v) = vals.get(i).copied().flatten() else { continue };
                    let (a, b) = if stacked {
                        let base = if v >= 0.0 { pos } else { neg };
                        if v >= 0.0 { pos += v } else { neg += v }
                        (base, base + v)
                    } else {
                        (0f64.clamp(vlo, vhi), v)
                    };
                    let off = slot * i as f64 + (slot - group) / 2.0 + if stacked { 0.0 } else { bar * si as f64 };
                    let rect = if horizontal {
                        let (x0, x1) = (to_x(a.min(b)), to_x(a.max(b)));
                        Rect::new(x0, ay0 + off, x1 - x0, bar)
                    } else {
                        let (y0, y1) = (to_y(a.max(b)), to_y(a.min(b)));
                        Rect::new(ax0 + off, y0, bar, y1 - y0)
                    };
                    out.push(Item::Rect { r: rect, color: SERIES[si % SERIES.len()] });
                }
            }
            // Category labels.
            for (i, c) in data.categories.iter().enumerate().take(n_cat) {
                let mid = slot * (i as f64 + 0.5);
                let tw = text_width(c, 0, lsize);
                if horizontal {
                    text(out, c, ax0 - tw - 3.0 * s, ay0 + mid + lsize * 0.35, lsize, 0x595959, false);
                } else if tw < slot * 1.2 || i % ((tw / slot).ceil() as usize).max(1) == 0 {
                    text(out, c, ax0 + mid - tw / 2.0, ay1 + lsize * 1.3, lsize, 0x595959, false);
                }
            }
        }
        ChartKind::Line | ChartKind::Area | ChartKind::Scatter => {
            let xs: Vec<f64> = match (&data.x, kind) {
                (Some(x), ChartKind::Scatter) if !x.is_empty() => x.clone(),
                _ => (0..n_cat).map(|i| i as f64).collect(),
            };
            let (xlo, xhi) = xs.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), v| (a.min(*v), b.max(*v)));
            let (xlo, xhi) = if kind == ChartKind::Scatter && data.x.is_some() { (xlo, xhi.max(xlo + 1e-9)) } else { (-0.5, n_cat as f64 - 0.5) };
            let px = |v: f64| ax0 + (v - xlo) / (xhi - xlo).max(1e-12) * (ax1 - ax0);
            let mut cum = vec![0.0; n_cat];
            for (si, (_, vals)) in data.series.iter().enumerate() {
                let color = SERIES[si % SERIES.len()];
                let pts: Vec<(f64, f64)> = vals
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| {
                        let v = (*v)?;
                        let v = if stacked && kind == ChartKind::Area {
                            cum[i] += v;
                            cum[i]
                        } else {
                            v
                        };
                        Some((px(*xs.get(i)?), to_y(v)))
                    })
                    .collect();
                if pts.is_empty() {
                    continue;
                }
                match kind {
                    ChartKind::Area => {
                        let base = to_y(0f64.clamp(vlo, vhi));
                        let mut poly = pts.clone();
                        poly.push((pts.last().unwrap().0, base));
                        poly.push((pts[0].0, base));
                        out.push(Item::Path { points: poly, closed: true, fill: Some(lighten(color)), stroke: Some((color, 1.0 * s)), clip });
                    }
                    ChartKind::Line => out.push(Item::Path { points: pts, closed: false, fill: None, stroke: Some((color, 1.5 * s)), clip }),
                    _ => {
                        for (x, y) in pts {
                            let d = 2.2 * s;
                            out.push(Item::Path { points: vec![(x - d, y - d), (x + d, y - d), (x + d, y + d), (x - d, y + d)], closed: true, fill: Some(color), stroke: None, clip });
                        }
                    }
                }
            }
            if kind != ChartKind::Scatter || data.x.is_none() {
                let slot = (ax1 - ax0) / n_cat as f64;
                for (i, c) in data.categories.iter().enumerate().take(n_cat) {
                    let tw = text_width(c, 0, lsize);
                    if tw < slot * 1.2 || i % ((tw / slot).ceil() as usize).max(1) == 0 {
                        text(out, c, px(i as f64) - tw / 2.0, ay1 + lsize * 1.3, lsize, 0x595959, false);
                    }
                }
            }
        }
        // Pie is drawn above; other kinds were mapped onto these.
        _ => {}
    }
    // Axes.
    out.push(Item::Line { a: (ax0, ay1), b: (ax1, ay1), width: 0.6 * s, color: 0x808080, dash: None });
    out.push(Item::Line { a: (ax0, ay0), b: (ax0, ay1), width: 0.6 * s, color: 0x808080, dash: None });
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(page: &PageDraw) -> Vec<String> {
        page.items.iter().filter_map(|i| if let Item::Text { text, .. } = i { Some(text.clone()) } else { None }).collect()
    }

    #[test]
    fn measures_and_wraps_with_the_bundled_font() {
        let w = text_width("Hello", 0, 11.0);
        assert!(w > 20.0 && w < 35.0, "{w}");
        assert!(text_width("Hello", 2, 11.0) > w, "bold is wider");
        let lines = wrap_text("the quick brown fox jumps", 0, 11.0, 50.0);
        assert!(lines.len() > 1 && lines.iter().all(|l| text_width(l, 0, 11.0) <= 50.0 || !l.contains(' ')));
    }

    #[test]
    fn lays_out_cells_headers_and_page_numbers() {
        let mut wb = Workbook::new();
        let s = wb.sheet_ids()[0];
        let mut items = Vec::new();
        for r in 0..120u32 {
            items.push((Addr::new(s, CellRef::new(r, 0)), format!("Item {r}")));
            items.push((Addr::new(s, CellRef::new(r, 1)), format!("{}", r * 10)));
        }
        wb.set_inputs(items);
        wb.set_format(Addr::new(s, CellRef::new(5, 1)), Some("$#,##0.00"));
        wb.layout_mut(s).unwrap().page.footer = "&CPage &P of &N".into();
        wb.layout_mut(s).unwrap().page.gridlines = true;
        let doc = layout(&wb, &[s], &DocInfo::default());
        // 120 rows of 15pt in 792 - 108 = 684pt: 45 per page.
        assert_eq!(doc.pages.len(), 3);
        let first = texts(&doc.pages[0]);
        assert!(first.contains(&"Item 0".to_string()) && first.contains(&"$50.00".to_string()));
        assert!(first.contains(&"Page 1 of 3".to_string()));
        assert!(texts(&doc.pages[2]).contains(&"Page 3 of 3".to_string()));
        assert!(doc.pages[0].items.iter().any(|i| matches!(i, Item::Line { .. })), "gridlines");
    }

    #[test]
    fn title_rows_repeat_on_later_pages() {
        let mut wb = Workbook::new();
        let s = wb.sheet_ids()[0];
        let items: Vec<(Addr, String)> = (0..100u32).map(|r| (Addr::new(s, CellRef::new(r, 0)), if r == 0 { "Header".into() } else { format!("v{r}") })).collect();
        wb.set_inputs(items);
        wb.layout_mut(s).unwrap().page.title_rows = Some((0, 0));
        let doc = layout(&wb, &[s], &DocInfo::default());
        assert!(doc.pages.len() >= 2);
        assert!(texts(&doc.pages[1]).contains(&"Header".to_string()));
    }

    #[test]
    fn empty_sheets_have_no_pages() {
        let wb = Workbook::new();
        assert!(layout(&wb, &[wb.sheet_ids()[0]], &DocInfo::default()).pages.is_empty());
    }
}
