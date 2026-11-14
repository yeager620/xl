//! Grid painting: builds a list of quads and shaped text for the visible
//! window of cells, then paints it.

use gpui::{
    App, BorderStyle, Bounds, ContentMask, Corners, Edges, Font, FontStyle, FontWeight, Hsla, PaintQuad, Pixels, Point, ShapedLine, SharedString,
    StrikethroughStyle, TextAlign, TextRun, UnderlineStyle, Window, fill, font, point, px, quad, size,
};
use xl_engine::style::luminance;
use xl_engine::{CellRef, FmtColor, HAlign, LineStyle, MAX_COLS, MAX_ROWS, Range, Rgb, Style, VAlign, Value, col_name};

use crate::app::{Axis, CELL_FONT, CELL_PAD, Drag, HEADER_FONT, ROW_H, Spreadsheet};
use crate::refs::{RefSpan, scan_refs};
use crate::theme::Theme;

pub enum Op {
    Quad(PaintQuad, Bounds<Pixels>),
    Path(gpui::Path, Hsla, Bounds<Pixels>),
    Text { line: Box<ShapedLine>, origin: Point<Pixels>, clip: Bounds<Pixels>, line_height: Pixels },
}

pub fn paint_ops(ops: Vec<Op>, window: &mut Window, cx: &mut App) {
    for op in ops {
        match op {
            Op::Quad(q, clip) => window.with_content_mask(Some(ContentMask { bounds: clip }), |w| w.paint_quad(q)),
            Op::Path(path, color, clip) => window.with_content_mask(Some(ContentMask { bounds: clip }), |w| w.paint_path(path, color)),
            Op::Text { line, origin, clip, line_height } => window.with_content_mask(Some(ContentMask { bounds: clip }), |w| {
                line.paint(origin, line_height, TextAlign::Left, None, w, cx).ok();
            }),
        }
    }
}

pub fn cell_font() -> Font {
    font(crate::FONT)
}

/// The font for a styled cell (its family if set, else the app font).
pub fn cell_font_for(st: &Style) -> Font {
    let base = st.font_name.as_deref().map_or_else(cell_font, |name| font(SharedString::from(name.to_string())));
    Font { weight: if st.bold { FontWeight::BOLD } else { FontWeight::NORMAL }, style: if st.italic { FontStyle::Italic } else { FontStyle::Normal }, ..base }
}

/// Pixel size of a cell's text: Excel's default 11pt is drawn at CELL_FONT.
pub fn font_px(st: &Style) -> f32 {
    st.font_size.map_or(CELL_FONT, |tenths| tenths as f32 / 10.0 * CELL_FONT / 11.0)
}

/// Line height for text of a given pixel size (ROW_H at the default size).
pub fn line_height(size: f32) -> f64 {
    (size as f64 * ROW_H / CELL_FONT as f64).round()
}

/// Break text into lines no wider than `width`: at explicit line breaks, then
/// between words, and inside words that alone are too wide.
pub fn wrap_lines(window: &mut Window, text: &str, f: Font, size: f32, width: f64) -> Vec<String> {
    let measure = |window: &mut Window, s: &str| f64::from(shape(window, s.to_string(), f.clone(), gpui::black(), size).width);
    let mut out = Vec::new();
    for para in text.split(['\n', '\u{2028}']) {
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if line.is_empty() || measure(window, &candidate) <= width {
                line = candidate;
            } else {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
            }
            // A single word wider than the cell breaks between characters.
            while line.chars().count() > 1 && measure(window, &line) > width {
                let mut cut = line.len();
                loop {
                    cut = line[..cut].char_indices().last().map_or(0, |(i, _)| i);
                    if cut == 0 || measure(window, &line[..cut]) <= width {
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

pub(crate) fn rect(x: f64, y: f64, w: f64, h: f64) -> Bounds<Pixels> {
    b(x, y, w, h)
}

pub(crate) fn outline(bounds: Bounds<Pixels>, width: f32, color: Hsla, bg: Hsla, radius: f32) -> PaintQuad {
    border_quad(bounds, width, color, bg, BorderStyle::Solid, radius)
}

pub fn shape(window: &mut Window, text: impl Into<SharedString>, f: Font, color: Hsla, size_px: f32) -> ShapedLine {
    let text: SharedString = text.into();
    let run = TextRun { len: text.len(), font: f, color, background_color: None, underline: None, strikethrough: None };
    window.text_system().shape_line(text, px(size_px), &[run], None)
}

fn rgb(c: Rgb) -> Hsla {
    gpui::rgb(c).into()
}

/// Text color for a styled cell: explicit color, else one that reads well on
/// the fill, else the theme's text color.
fn text_color(t: &Theme, st: &Style) -> Hsla {
    match (st.color, st.fill) {
        // Near-black text on the plain dark-theme background would vanish.
        (Some(c), None) if t.dark && luminance(c) < 0.03 => t.ink,
        (Some(c), _) => rgb(c),
        (None, Some(f)) if luminance(f) > 0.35 => rgb(0x1A1A1E),
        (None, Some(_)) => rgb(0xFFFFFF),
        (None, None) => t.ink,
    }
}

fn shape_styled(window: &mut Window, text: impl Into<SharedString>, st: &Style, color: Hsla) -> ShapedLine {
    let text: SharedString = text.into();
    let f = cell_font_for(st);
    let run = TextRun {
        len: text.len(),
        font: f,
        color,
        background_color: None,
        underline: st.underline.then_some(UnderlineStyle { color: Some(color), thickness: px(1.), wavy: false }),
        strikethrough: st.strike.then_some(StrikethroughStyle { thickness: px(1.), color: Some(color) }),
    };
    window.text_system().shape_line(text, px(font_px(st)), &[run], None)
}

fn shape_runs(window: &mut Window, text: SharedString, runs: Vec<TextRun>, size_px: f32) -> ShapedLine {
    window.text_system().shape_line(text, px(size_px), &runs, None)
}

fn b(x: f64, y: f64, w: f64, h: f64) -> Bounds<Pixels> {
    Bounds::new(point(px(x as f32), px(y as f32)), size(px(w.max(0.0) as f32), px(h.max(0.0) as f32)))
}

fn border_quad(bounds: Bounds<Pixels>, width: f32, color: Hsla, bg: Hsla, style: BorderStyle, radius: f32) -> PaintQuad {
    quad(bounds, Corners::all(px(radius)), bg, Edges::all(px(width)), color, style)
}

fn fmt_color(t: &Theme, c: FmtColor) -> Hsla {
    let hex = match (c, t.dark) {
        (FmtColor::Red, _) => return t.error,
        (FmtColor::Black, _) | (FmtColor::White, _) => return t.ink,
        (FmtColor::Green, false) => 0x15803D,
        (FmtColor::Green, true) => 0x4ADE80,
        (FmtColor::Blue, false) => 0x1D4ED8,
        (FmtColor::Blue, true) => 0x60A5FA,
        (FmtColor::Yellow, false) => 0xA16207,
        (FmtColor::Yellow, true) => 0xFACC15,
        (FmtColor::Magenta, false) => 0xBE185D,
        (FmtColor::Magenta, true) => 0xF472B6,
        (FmtColor::Cyan, false) => 0x0E7490,
        (FmtColor::Cyan, true) => 0x22D3EE,
    };
    gpui::rgb(hex).into()
}

impl Spreadsheet {
    /// References in the formula being edited that point at the shown sheet.
    fn visible_refs(&self) -> Vec<RefSpan> {
        let Some(e) = &self.edit else { return Vec::new() };
        scan_refs(&e.text)
            .into_iter()
            .filter(|r| match &r.sheet {
                None => self.cur == e.at.sheet,
                Some(name) => self.wb.sheet_name(self.cur).eq_ignore_ascii_case(name),
            })
            .collect()
    }

    pub fn build_frame(&self, bounds: Bounds<Pixels>, window: &mut Window) -> Vec<Op> {
        let t = self.theme;
        let mut ops = Vec::with_capacity(2048);
        let ox = f64::from(bounds.origin.x);
        let oy = f64::from(bounds.origin.y);
        let (vw, vh) = (f64::from(bounds.size.width), f64::from(bounds.size.height));
        let gx = ox + self.hw();
        let gy = oy + self.hh();
        let (gw, gh) = (vw - self.hw(), vh - self.hh());
        let all = bounds;
        let grid_clip = b(gx, gy, gw, gh);
        let store = self.wb.store(self.cur);
        let layout = self.wb.layout(self.cur);
        let sel = self.selection();
        // The in-cell editor shows only on the sheet that owns the edit.
        let editing = self.edit.as_ref().filter(|e| e.at.sheet == self.cur);
        let (fr, fc) = self.frozen();
        let (fw, fh) = self.frozen_size();

        // Panes: frozen lines stay put, the rest scroll underneath them.
        let xpane = |c: u32| if c < fc { (gx, fw.min(gw)) } else { (gx + fw, (gw - fw).max(0.0)) };
        let ypane = |r: u32| if r < fr { (gy, fh.min(gh)) } else { (gy + fh, (gh - fh).max(0.0)) };
        let clip_of = |r: u32, c: u32| {
            let ((x, w), (y, h)) = (xpane(c), ypane(r));
            b(x, y, w, h)
        };
        let col_hdr_clip = |c: u32| {
            let (x, w) = xpane(c);
            b(x, oy, w, self.hh())
        };
        let row_hdr_clip = |r: u32| {
            let (y, h) = ypane(r);
            b(ox, y, self.hw(), h)
        };

        ops.push(Op::Quad(fill(all, t.canvas), all));

        // Visible (non-hidden) lines with their screen positions and sizes.
        let lines = |count: u32, frozen: u32, first_scrolling: u32, origin: f64, limit: f64, pos: &dyn Fn(u32) -> f64, size: &dyn Fn(u32) -> f64| {
            let mut out: Vec<(u32, f64, f64)> = Vec::new();
            for i in 0..frozen.min(count) {
                let sz = size(i);
                if sz > 0.0 {
                    out.push((i, origin + pos(i), sz));
                }
            }
            let mut i = first_scrolling.max(frozen);
            let mut p = origin + pos(i);
            while p < limit && i < count {
                let sz = size(i);
                if sz > 0.0 {
                    out.push((i, p, sz));
                }
                p += sz;
                i += 1;
            }
            out
        };
        let first_col = self.col_at(self.col_x(fc) + self.scroll_x);
        let first_row = self.row_at(self.row_y(fr) + self.scroll_y);
        let cols = lines(MAX_COLS, fc, first_col, gx, ox + vw, &|c| self.grid_x(c), &|c| self.col_w(c));
        let rows = lines(MAX_ROWS, fr, first_row, gy, oy + vh, &|r| self.grid_y(r), &|r| self.row_h(r));
        let (Some(&(max_c, _, _)), Some(&(max_r, _, _))) = (cols.last(), rows.last()) else {
            return ops;
        };
        let (min_c, min_r) = (first_col.max(fc), first_row.max(fr));
        let in_view = |m: &Range| (m.end.col >= min_c || m.start.col < fc) && m.start.col <= max_c && (m.end.row >= min_r || m.start.row < fr) && m.start.row <= max_r;
        let merges: Vec<Range> = layout.merges.iter().copied().filter(in_view).collect();
        let merge_at = |c: CellRef| merges.iter().copied().find(|m| m.contains(c));
        let x_of = |c: u32| gx + self.grid_x(c);
        let y_of = |r: u32| gy + self.grid_y(r);

        // A range's on-screen rectangles, one per pane it spans, with that pane's clip.
        let pieces = |r: Range| -> Vec<(Bounds<Pixels>, Bounds<Pixels>)> {
            let split = |a: u32, z: u32, f: u32| {
                let mut v = Vec::new();
                if a < f {
                    v.push((a, z.min(f - 1)));
                }
                if z >= f {
                    v.push((a.max(f), z));
                }
                v
            };
            let mut out = Vec::new();
            for &(r0, r1) in &split(r.start.row, r.end.row, fr) {
                for &(c0, c1) in &split(r.start.col, r.end.col, fc) {
                    let clip = clip_of(r0, c0);
                    let (cx0, cy0) = (f64::from(clip.origin.x), f64::from(clip.origin.y));
                    let (cx1, cy1) = (cx0 + f64::from(clip.size.width), cy0 + f64::from(clip.size.height));
                    let x0 = x_of(c0).max(cx0 - 10.0);
                    let x1 = (x_of(c1) + self.col_w(c1)).min(cx1 + 10.0);
                    let y0 = y_of(r0).max(cy0 - 10.0);
                    let y1 = (y_of(r1) + self.row_h(r1)).min(cy1 + 10.0);
                    if x1 > x0 && y1 > y0 {
                        out.push((b(x0, y0, x1 - x0, y1 - y0), clip));
                    }
                }
            }
            out
        };
        let active_range = layout.merge_at(self.active).unwrap_or(Range::single(self.active));

        // Conditional formatting for the visible cells.
        let mut cf: std::collections::HashMap<CellRef, xl_engine::cond::CfResult> = std::collections::HashMap::new();
        if !layout.cond.is_empty() {
            let view = xl_engine::cond::CondView::new(&self.wb, self.cur);
            for &(r, _, _) in &rows {
                for &(c, _, _) in &cols {
                    let cell = CellRef::new(r, c);
                    let res = view.cell(cell);
                    if !res.is_empty() {
                        cf.insert(cell, res);
                    }
                }
            }
        }
        // A cell's style with its table's look and conditional formatting on top.
        let styled = |cell: CellRef| -> std::borrow::Cow<'_, Style> {
            let mut base = std::borrow::Cow::Borrowed(self.wb.style(self.at(cell)));
            if let Some(tb) = layout.tables.iter().find(|tb| tb.range.contains(cell)) {
                let (header, band) = crate::data::table_colors(tb.style, t.dark);
                let mut st = base.into_owned();
                if cell.row == tb.range.start.row {
                    st.fill = st.fill.or(Some(header));
                    st.color = st.color.or(Some(0xFFFFFF));
                    st.bold = true;
                } else if tb.banded && (cell.row - tb.range.start.row).is_multiple_of(2) && st.fill.is_none() {
                    st.fill = Some(band);
                }
                base = std::borrow::Cow::Owned(st);
            }
            match cf.get(&cell) {
                Some(res) if res.fill.is_some() || res.color.is_some() || res.bold || res.italic => {
                    let mut st = base.into_owned();
                    st.fill = res.fill.or(st.fill);
                    st.color = res.color.or(st.color);
                    st.bold |= res.bold;
                    st.italic |= res.italic;
                    std::borrow::Cow::Owned(st)
                }
                _ => base,
            }
        };

        // Headers (after the outline gutters, if any).
        let (gutter_w, gutter_h) = (self.hw() - crate::app::HEADER_W, self.hh() - crate::app::HEADER_H);
        ops.push(Op::Quad(fill(b(ox, oy, vw, self.hh()), t.chrome), all));
        ops.push(Op::Quad(fill(b(ox, oy, self.hw(), vh), t.chrome), all));
        for &(c, x, w) in &cols {
            if (sel.start.col..=sel.end.col).contains(&c) {
                ops.push(Op::Quad(fill(b(x, oy + gutter_h, w, self.hh() - gutter_h), t.header_selected), col_hdr_clip(c)));
                ops.push(Op::Quad(fill(b(x, oy + self.hh() - 2.0, w, 2.0), t.accent), col_hdr_clip(c)));
            }
        }
        for &(r, y, h) in &rows {
            if (sel.start.row..=sel.end.row).contains(&r) {
                ops.push(Op::Quad(fill(b(ox + gutter_w, y, self.hw() - gutter_w, h), t.header_selected), row_hdr_clip(r)));
                ops.push(Op::Quad(fill(b(ox + self.hw() - 2.0, y, 2.0, h), t.accent), row_hdr_clip(r)));
            }
        }

        // Gridlines.
        for &(c, x, w) in &cols {
            let (px0, pw) = xpane(c);
            ops.push(Op::Quad(fill(b(x + w - 1.0, gy, 1.0, gh), t.rule), b(px0, gy, pw, gh)));
            ops.push(Op::Quad(fill(b(x + w - 1.0, oy + gutter_h + 5.0, 1.0, crate::app::HEADER_H - 10.0), t.divider), col_hdr_clip(c)));
        }
        for &(r, y, h) in &rows {
            let (py0, ph) = ypane(r);
            ops.push(Op::Quad(fill(b(gx, y + h - 1.0, gw, 1.0), t.rule), b(gx, py0, gw, ph)));
            ops.push(Op::Quad(fill(b(ox + gutter_w, y + h - 1.0, self.hw() - gutter_w, 1.0), t.rule), row_hdr_clip(r)));
        }
        ops.push(Op::Quad(fill(b(ox, oy + self.hh() - 1.0, vw, 1.0), t.divider), all));
        ops.push(Op::Quad(fill(b(ox + self.hw() - 1.0, oy, 1.0, vh), t.divider), all));

        // Cell fills (over gridlines, like Excel) and borders. A merged range
        // paints as one cell using its top-left cell's style.
        let mut border_ops = Vec::new();
        let mut paint_box = |ops: &mut Vec<Op>, st: &Style, x: f64, y: f64, w: f64, h: f64, clip: Bounds<Pixels>, merged: bool| {
            if let Some(f) = st.fill {
                ops.push(Op::Quad(fill(b(x, y - 1.0, w, h + 1.0), rgb(f)), clip));
            } else if merged {
                // Hide the gridlines inside the merge.
                ops.push(Op::Quad(fill(b(x, y, w - 1.0, h - 1.0), t.canvas), clip));
            }
            if st.has_borders() {
                let line = |b: &xl_engine::Border| b.color.map_or(t.ink, rgb);
                let mut edge = |bd: &xl_engine::Border, bx: f64, by: f64, bw: f64, bh: f64| {
                    if bd.style == LineStyle::None {
                        return;
                    }
                    let q = if bd.style == LineStyle::Double {
                        let horiz = bw > bh;
                        let (w2, h2) = if horiz { (bw, 1.0) } else { (1.0, bh) };
                        border_ops.push(Op::Quad(fill(b(bx, by, w2, h2), line(bd)), clip));
                        let (ox2, oy2) = if horiz { (0.0, 2.0) } else { (2.0, 0.0) };
                        fill(b(bx + ox2, by + oy2, w2, h2), line(bd))
                    } else {
                        fill(b(bx, by, bw, bh), line(bd))
                    };
                    border_ops.push(Op::Quad(q, clip));
                };
                let tw = |bd: &xl_engine::Border| bd.style.width().max(1.0) as f64;
                edge(&st.top, x - 1.0, y - 1.0, w + 1.0, tw(&st.top));
                edge(&st.bottom, x - 1.0, y + h - tw(&st.bottom), w + 1.0, tw(&st.bottom));
                edge(&st.left, x - 1.0, y - 1.0, tw(&st.left), h + 1.0);
                edge(&st.right, x + w - tw(&st.right), y - 1.0, tw(&st.right), h + 1.0);
            }
        };
        for &(r, y, h) in &rows {
            for &(c, x, w) in &cols {
                let cell = CellRef::new(r, c);
                if merge_at(cell).is_some() {
                    continue;
                }
                let st = styled(cell);
                if st.fill.is_some() || st.has_borders() {
                    paint_box(&mut ops, &st, x, y, w, h, clip_of(r, c), false);
                }
            }
        }
        for m in &merges {
            let st = styled(m.start);
            for (rect, clip) in pieces(*m) {
                let (x, y) = (f64::from(rect.origin.x), f64::from(rect.origin.y));
                paint_box(&mut ops, &st, x, y, f64::from(rect.size.width), f64::from(rect.size.height), clip, true);
            }
        }
        ops.extend(border_ops);

        // Data bars, under the text.
        for &(r, y, h) in &rows {
            for &(c, x, w) in &cols {
                if let Some((frac, color)) = cf.get(&CellRef::new(r, c)).and_then(|res| res.bar) {
                    let mut fill_color = rgb(color);
                    fill_color.a = 0.6;
                    let len = ((w - 5.0) * frac).max(2.0);
                    ops.push(Op::Quad(quad(b(x + 2.0, y + 3.0, len, h - 7.0), Corners::all(px(1.5)), fill_color, Edges::default(), fill_color, BorderStyle::Solid), clip_of(r, c)));
                }
            }
        }

        // Selection fill (the active cell stays clear).
        let multi = sel.start != sel.end && sel != active_range;
        if multi {
            for (rect, clip) in pieces(sel) {
                ops.push(Op::Quad(fill(rect, t.selection), clip));
            }
            let st = styled(active_range.start);
            for (rect, clip) in pieces(active_range) {
                ops.push(Op::Quad(fill(rect, st.fill.map_or(t.canvas, rgb)), clip));
            }
        }

        // Reference highlights while writing a formula.
        let refs = self.visible_refs();
        for r in &refs {
            let color = t.ref_colors[r.color % t.ref_colors.len()];
            let mut soft = color;
            soft.a = 0.08;
            let pointed = self.edit.as_ref().and_then(|e| e.point.as_ref()).is_some_and(|p| p.span == r.span);
            let style = if pointed { BorderStyle::Dashed } else { BorderStyle::Solid };
            for (rect, clip) in pieces(r.range) {
                ops.push(Op::Quad(border_quad(rect, 1.5, color, soft, style, 1.0), clip));
            }
        }

        // Sparklines, under any text.
        for sp in &layout.sparklines {
            let (Some(&(r, y, h)), Some(&(c, x, w))) = (rows.iter().find(|row| row.0 == sp.cell.row), cols.iter().find(|col| col.0 == sp.cell.col)) else { continue };
            let values = self.wb.sparkline_values(self.cur, sp);
            ops.extend(crate::chart_paint::sparkline_ops(self, sp.kind, &values, sp.color, (x, y, w, h), clip_of(r, c)));
        }

        // Cell text.
        let mut draw_text = |ops: &mut Vec<Op>, cell: CellRef, x: f64, y: f64, w: f64, h: f64, clip: Bounds<Pixels>, overflow: Option<usize>| {
            let v = store.value(cell);
            if v.is_empty() {
                return;
            }
            let shown = self.wb.display(self.at(cell));
            let st_cow = styled(cell);
            let st: &Style = &st_cow;
            let color = match (v, shown.color) {
                (Value::Error(_), _) => t.error,
                (_, Some(fc)) => fmt_color(&t, fc),
                _ => text_color(&t, st),
            };
            let size = font_px(st);
            let line_h = line_height(size);
            let avail = w - 2.0 * CELL_PAD;
            let align = match (st.align, v) {
                (HAlign::General, Value::Number(_)) => HAlign::Right,
                (HAlign::General, Value::Bool(_) | Value::Error(_)) => HAlign::Center,
                (HAlign::General, _) => HAlign::Left,
                (a, _) => a,
            };
            let wrapped = st.wrap && !matches!(v, Value::Number(_));
            let texts: Vec<String> =
                if wrapped { wrap_lines(window, &shown.text, cell_font_for(st), size, avail) } else { vec![shown.text.replace(['\n', '\u{2028}'], " ")] };
            let total = texts.len() as f64 * line_h;
            // Bottom-aligned single lines keep the default look: centred in a
            // default-height row, sitting at the bottom of taller ones.
            let mut ty = match st.valign {
                VAlign::Top => y,
                VAlign::Center => y + (h - total) / 2.0,
                VAlign::Bottom => y + h - total,
            };
            if st.valign == VAlign::Top && total < h {
                ty = y + ((ROW_H - line_h) / 2.0).max(0.0);
            }
            let cell_clip = b(x, y, w - 1.0, h).intersect(&clip);
            for text in texts {
                let mut line = shape_styled(window, text, st, color);
                let tw = f64::from(line.width);
                if matches!(v, Value::Number(_)) && tw > avail {
                    line = shape_styled(window, "#".repeat((avail / (size as f64 * 0.57)).max(1.0) as usize), st, color);
                }
                let lw = f64::from(line.width);
                let (tx, line_clip) = match align {
                    HAlign::Right => (x + w - CELL_PAD - lw, cell_clip),
                    HAlign::Center => (x + (w - lw) / 2.0, cell_clip),
                    _ if matches!(v, Value::Number(_)) || wrapped => (x + CELL_PAD, cell_clip),
                    _ => {
                        // Text overflows into empty neighbours, like Excel.
                        let mut cw = w;
                        if let Some(i) = overflow.filter(|_| tw > avail) {
                            for &(c2, _, w2) in &cols[i + 1..] {
                                let n = CellRef::new(cell.row, c2);
                                if (c2 >= fc) != (cell.col >= fc) || !store.value(n).is_empty() || merge_at(n).is_some() || (editing.is_some() && n == self.active) {
                                    break;
                                }
                                cw += w2;
                                if cw - 2.0 * CELL_PAD >= tw {
                                    break;
                                }
                            }
                        }
                        (x + CELL_PAD, b(x, y, cw - 1.0, h).intersect(&clip))
                    }
                };
                ops.push(Op::Text { line: Box::new(line), origin: point(px(tx as f32), px(ty as f32)), clip: line_clip, line_height: px(line_h as f32) });
                ty += line_h;
            }
        };
        for &(r, y, h) in &rows {
            for (i, &(c, x, w)) in cols.iter().enumerate() {
                let cell = CellRef::new(r, c);
                if (editing.is_some() && cell == self.active) || merge_at(cell).is_some() {
                    continue;
                }
                draw_text(&mut ops, cell, x, y, w, h, clip_of(r, c), Some(i));
            }
        }
        for m in &merges {
            if editing.is_some() && m.start == self.active {
                continue;
            }
            // Text lays out across the whole merge; each pane clips its part.
            let (x, y) = (x_of(m.start.col), y_of(m.start.row));
            let w = x_of(m.end.col) + self.col_w(m.end.col) - x;
            let h = y_of(m.end.row) + self.row_h(m.end.row) - y;
            for (_, clip) in pieces(*m) {
                draw_text(&mut ops, m.start, x, y, w, h, clip, None);
            }
        }

        // Filter dropdown buttons in the header row.
        if let Some(f) = &layout.filter
            && let Some(&(r, y, h)) = rows.iter().find(|row| row.0 == f.range.start.row)
        {
            let size = crate::data::FILTER_BUTTON;
            for &(c, x, w) in cols.iter().filter(|col| (f.range.start.col..=f.range.end.col).contains(&col.0)) {
                let active = f.columns.contains_key(&c);
                let (bx, by) = (x + w - size - 3.0, y + (h - size) / 2.0);
                let clip = clip_of(r, c);
                let (bg, fg) = if active { (t.accent, t.on_accent) } else { (t.chrome, t.ink_muted) };
                ops.push(Op::Quad(border_quad(b(bx, by, size, size), 1.0, if active { t.accent } else { t.divider }, bg, BorderStyle::Solid, 3.0), clip));
                // A small down-pointing triangle, drawn as stacked bars.
                let (cx0, cy0) = (bx + size / 2.0, by + size / 2.0 - 1.5);
                for (i, half) in [3.5, 2.5, 1.5, 0.5].iter().enumerate() {
                    ops.push(Op::Quad(fill(b(cx0 - half, cy0 + i as f64, half * 2.0, 1.0), fg), clip));
                }
            }
        }

        // Freeze lines.
        if fc > 0 && fw < gw {
            ops.push(Op::Quad(fill(b(gx + fw - 1.0, oy, 1.0, vh), t.ink_faint), all));
        }
        if fr > 0 && fh < gh {
            ops.push(Op::Quad(fill(b(ox, gy + fh - 1.0, vw, 1.0), t.ink_faint), all));
        }

        // Selection outline, active cell, fill handle.
        if multi {
            for (rect, clip) in pieces(sel) {
                ops.push(Op::Quad(border_quad(rect, 1.0, t.accent, gpui::transparent_black(), BorderStyle::Solid, 0.0), clip));
            }
        }
        // The range a fill-handle drag would cover.
        if let Some(target) = self.fill_target {
            for (rect, clip_bounds) in pieces(target) {
                ops.push(Op::Quad(border_quad(rect, 1.5, t.ink_muted, gpui::transparent_black(), BorderStyle::Dashed, 0.0), clip_bounds));
            }
        }
        // Copied or cut cells: a dashed outline until the clipboard is used up.
        if let Some(clip) = self.clip.as_ref().filter(|c| c.sheet == self.cur) {
            for (rect, clip_bounds) in pieces(clip.range) {
                ops.push(Op::Quad(border_quad(rect, 1.5, t.accent, gpui::transparent_black(), BorderStyle::Dashed, 0.0), clip_bounds));
            }
        }
        let active_pieces = pieces(active_range);
        let ab = active_pieces.first().map_or(b(gx, gy, 0.0, 0.0), |p| p.0);
        if editing.is_none() {
            for (rect, clip) in &active_pieces {
                let outer = b(f64::from(rect.origin.x) - 1.0, f64::from(rect.origin.y) - 1.0, f64::from(rect.size.width) + 1.0, f64::from(rect.size.height) + 1.0);
                ops.push(Op::Quad(border_quad(outer, 2.0, t.accent, gpui::transparent_black(), BorderStyle::Solid, 0.0), *clip));
            }
            let corner = CellRef::new(sel.end.row.max(active_range.end.row), sel.end.col.max(active_range.end.col));
            if let Some((rect, clip)) = pieces(Range::single(corner)).first() {
                let hx = f64::from(rect.origin.x + rect.size.width) - 4.0;
                let hy = f64::from(rect.origin.y + rect.size.height) - 4.0;
                ops.push(Op::Quad(border_quad(b(hx, hy, 6.0, 6.0), 1.0, t.canvas, t.accent, BorderStyle::Solid, 0.0), *clip));
            }
        }

        // Header labels.
        let hf = cell_font();
        let hf_active = Font { weight: FontWeight::SEMIBOLD, ..cell_font() };
        for &(c, x, w) in &cols {
            let active = c >= sel.start.col && c <= sel.end.col;
            let line = shape(window, col_name(c), if active { hf_active.clone() } else { hf.clone() }, if active { t.accent } else { t.ink_muted }, HEADER_FONT);
            let tx = x + (w - f64::from(line.width)) / 2.0;
            ops.push(Op::Text { line: Box::new(line), origin: point(px(tx as f32), px((oy + gutter_h) as f32)), clip: col_hdr_clip(c), line_height: px(crate::app::HEADER_H as f32) });
        }
        for &(r, y, h) in &rows {
            let active = r >= sel.start.row && r <= sel.end.row;
            let line = shape(window, (r + 1).to_string(), if active { hf_active.clone() } else { hf.clone() }, if active { t.accent } else { t.ink_muted }, HEADER_FONT);
            let tx = ox + gutter_w + (crate::app::HEADER_W - f64::from(line.width)) / 2.0;
            let clip = b(ox, y, self.hw(), h).intersect(&row_hdr_clip(r));
            ops.push(Op::Text { line: Box::new(line), origin: point(px(tx as f32), px(y as f32)), clip, line_height: px(h as f32) });
        }

        // In-cell editor.
        if let Some(e) = editing {
            let f = cell_font();
            let lh = px(ROW_H as f32);
            let display = e.text.replace('\u{2028}', "↵");
            let all_refs = scan_refs(&e.text);
            let shift = |i: usize| e.text[..i].replace('\u{2028}', "↵").len();
            let base = TextRun { len: 0, font: f.clone(), color: t.ink, background_color: None, underline: None, strikethrough: None };
            let mut runs = Vec::new();
            let mut pos = 0;
            for r in &all_refs {
                let (s, en) = (shift(r.span.start), shift(r.span.end));
                if s > pos {
                    runs.push(TextRun { len: s - pos, ..base.clone() });
                }
                runs.push(TextRun { len: en - s, color: t.ref_colors[r.color % t.ref_colors.len()], ..base.clone() });
                pos = en;
            }
            if display.len() > pos {
                runs.push(TextRun { len: display.len() - pos, ..base.clone() });
            }
            let line = shape_runs(window, display.into(), runs, CELL_FONT);
            let w = (f64::from(line.width) + 2.0 * CELL_PAD + 4.0).max(f64::from(ab.size.width) + 1.0);
            let eh = f64::from(ab.size.height).max(ROW_H);
            let eb = b(f64::from(ab.origin.x) - 1.0, f64::from(ab.origin.y) - 1.0, w, eh + 1.0);
            ops.push(Op::Quad(border_quad(eb, 2.0, t.accent, t.canvas, BorderStyle::Solid, 0.0), grid_clip));
            // One editing line, at the bottom of a tall cell like the display.
            let ty = f64::from(ab.origin.y) + eh - ROW_H;
            let caret_x = f64::from(ab.origin.x) + CELL_PAD + f64::from(line.x_for_index(shift(e.cursor)));
            if let Some(m) = &e.marked {
                let x0 = f64::from(ab.origin.x) + CELL_PAD + f64::from(line.x_for_index(shift(m.start)));
                let x1 = f64::from(ab.origin.x) + CELL_PAD + f64::from(line.x_for_index(shift(m.end)));
                ops.push(Op::Quad(fill(b(x0, ty + ROW_H - 5.0, x1 - x0, 1.5), t.accent), grid_clip));
            }
            let origin = point(px((f64::from(ab.origin.x) + CELL_PAD) as f32), px(ty as f32));
            ops.push(Op::Text { line: Box::new(line), origin, clip: grid_clip, line_height: lh });
            ops.push(Op::Quad(fill(b(caret_x, ty + 4.0, 1.5, ROW_H - 8.0), t.accent), grid_clip));
        }

        ops.extend(self.outline_ops(window));

        // Charts float over the cells.
        for (i, chart) in layout.charts.iter().enumerate() {
            let (x, y, w, h) = self.chart_rect(chart);
            let clip = clip_of(chart.anchor.row, chart.anchor.col);
            let data = self.wb.chart_data(self.cur, chart);
            let selected = self.selected_chart == Some(i);
            ops.extend(crate::chart_paint::chart_ops(self, chart, &data, (x, y, w, h), clip, selected, window));
        }

        // Scrollbars (overlay).
        for axis in [Axis::Vertical, Axis::Horizontal] {
            if let Some(th) = self.thumb(axis) {
                let active = self.scroll_hover == Some(axis) || matches!(self.drag, Some(Drag::Scroll { axis: a, .. }) if a == axis);
                let mut color = t.scrollbar;
                if active {
                    color.a = (color.a * 1.8).min(1.0);
                }
                let r = (th.w.min(th.h) / 2.0) as f32;
                ops.push(Op::Quad(quad(b(th.x, th.y, th.w, th.h), Corners::all(px(r)), color, Edges::default(), color, BorderStyle::Solid), all));
            }
        }
        ops
    }
}
