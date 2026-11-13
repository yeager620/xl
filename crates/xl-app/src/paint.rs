//! Grid painting: builds a list of quads and shaped text for the visible
//! window of cells, then paints it.

use gpui::{
    App, BorderStyle, Bounds, ContentMask, Corners, Edges, Font, FontStyle, FontWeight, Hsla, PaintQuad, Pixels, Point, ShapedLine, SharedString,
    StrikethroughStyle, TextAlign, TextRun, UnderlineStyle, Window, fill, font, point, px, quad, size,
};
use xl_engine::style::luminance;
use xl_engine::{CellRef, FmtColor, HAlign, LineStyle, MAX_COLS, Range, Rgb, Style, Value, col_name};

use crate::app::{Axis, CELL_FONT, CELL_PAD, Drag, HEADER_FONT, HEADER_H, HEADER_W, ROW_H, Spreadsheet};
use crate::refs::{RefSpan, scan_refs};
use crate::theme::Theme;

pub enum Op {
    Quad(PaintQuad, Bounds<Pixels>),
    Text { line: Box<ShapedLine>, origin: Point<Pixels>, clip: Bounds<Pixels>, line_height: Pixels },
}

pub fn paint_ops(ops: Vec<Op>, window: &mut Window, cx: &mut App) {
    for op in ops {
        match op {
            Op::Quad(q, clip) => window.with_content_mask(Some(ContentMask { bounds: clip }), |w| w.paint_quad(q)),
            Op::Text { line, origin, clip, line_height } => window.with_content_mask(Some(ContentMask { bounds: clip }), |w| {
                line.paint(origin, line_height, TextAlign::Left, None, w, cx).ok();
            }),
        }
    }
}

pub fn cell_font() -> Font {
    font("Inter")
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
        (Some(c), None) if t.dark && luminance(c) < 0.03 => t.text,
        (Some(c), _) => rgb(c),
        (None, Some(f)) if luminance(f) > 0.35 => rgb(0x1A1A1E),
        (None, Some(_)) => rgb(0xFFFFFF),
        (None, None) => t.text,
    }
}

fn shape_styled(window: &mut Window, text: impl Into<SharedString>, st: &Style, color: Hsla) -> ShapedLine {
    let text: SharedString = text.into();
    let f = Font {
        weight: if st.bold { FontWeight::BOLD } else { FontWeight::NORMAL },
        style: if st.italic { FontStyle::Italic } else { FontStyle::Normal },
        ..cell_font()
    };
    let run = TextRun {
        len: text.len(),
        font: f,
        color,
        background_color: None,
        underline: st.underline.then_some(UnderlineStyle { color: Some(color), thickness: px(1.), wavy: false }),
        strikethrough: st.strike.then_some(StrikethroughStyle { thickness: px(1.), color: Some(color) }),
    };
    window.text_system().shape_line(text, px(CELL_FONT), &[run], None)
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
        (FmtColor::Black, _) | (FmtColor::White, _) => return t.text,
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
        let gx = ox + HEADER_W;
        let gy = oy + HEADER_H;
        let (gw, gh) = (vw - HEADER_W, vh - HEADER_H);
        let all = bounds;
        let grid_clip = b(gx, gy, gw, gh);
        let col_hdr_clip = b(gx, oy, gw, HEADER_H);
        let row_hdr_clip = b(ox, gy, HEADER_W, gh);
        let store = self.wb.store(self.cur);
        let sel = self.selection();
        // The in-cell editor shows only on the sheet that owns the edit.
        let editing = self.edit.as_ref().filter(|e| e.at.sheet == self.cur);

        ops.push(Op::Quad(fill(all, t.bg), all));

        let first_row = self.row_at(self.scroll_y);
        let last_row = self.row_at(self.scroll_y + gh);
        let first_col = self.col_at(self.scroll_x);
        let mut cols: Vec<(u32, f64, f64)> = Vec::new();
        {
            let mut c = first_col;
            let mut x = gx + self.col_x(c) - self.scroll_x;
            while x < ox + vw && c < MAX_COLS {
                let w = self.col_w(c);
                cols.push((c, x, w));
                x += w;
                c += 1;
            }
        }
        let row_y = |r: u32| gy + r as f64 * ROW_H - self.scroll_y;
        let screen_x = |c: u32| gx + self.col_x(c) - self.scroll_x;
        let rect_of = |r: Range| -> Bounds<Pixels> {
            let x0 = screen_x(r.start.col).max(gx - 10.0);
            let x1 = (screen_x(r.end.col) + self.col_w(r.end.col)).min(gx + gw + 10.0);
            let y0 = row_y(r.start.row).max(gy - 10.0);
            let y1 = row_y(r.end.row + 1).min(gy + gh + 10.0);
            b(x0, y0, x1 - x0, y1 - y0)
        };

        // Headers.
        ops.push(Op::Quad(fill(b(ox, oy, vw, HEADER_H), t.header_bg), all));
        ops.push(Op::Quad(fill(b(ox, oy, HEADER_W, vh), t.header_bg), all));
        let sel_x0 = screen_x(sel.start.col).max(gx);
        let sel_x1 = (screen_x(sel.end.col) + self.col_w(sel.end.col)).min(gx + gw);
        if sel_x1 > sel_x0 {
            ops.push(Op::Quad(fill(b(sel_x0, oy, sel_x1 - sel_x0, HEADER_H), t.header_active_bg), col_hdr_clip));
            ops.push(Op::Quad(fill(b(sel_x0, oy + HEADER_H - 2.0, sel_x1 - sel_x0, 2.0), t.accent), col_hdr_clip));
        }
        let sel_y0 = row_y(sel.start.row).max(gy);
        let sel_y1 = row_y(sel.end.row + 1).min(gy + gh);
        if sel_y1 > sel_y0 {
            ops.push(Op::Quad(fill(b(ox, sel_y0, HEADER_W, sel_y1 - sel_y0), t.header_active_bg), row_hdr_clip));
            ops.push(Op::Quad(fill(b(ox + HEADER_W - 2.0, sel_y0, 2.0, sel_y1 - sel_y0), t.accent), row_hdr_clip));
        }

        // Gridlines.
        for &(_, x, w) in &cols {
            ops.push(Op::Quad(fill(b(x + w - 1.0, gy, 1.0, gh), t.grid), grid_clip));
            ops.push(Op::Quad(fill(b(x + w - 1.0, oy + 5.0, 1.0, HEADER_H - 10.0), t.border), col_hdr_clip));
        }
        for r in first_row..=last_row {
            let y = row_y(r + 1) - 1.0;
            ops.push(Op::Quad(fill(b(gx, y, gw, 1.0), t.grid), grid_clip));
            ops.push(Op::Quad(fill(b(ox, y, HEADER_W, 1.0), t.grid), row_hdr_clip));
        }
        ops.push(Op::Quad(fill(b(ox, oy + HEADER_H - 1.0, vw, 1.0), t.border), all));
        ops.push(Op::Quad(fill(b(ox + HEADER_W - 1.0, oy, 1.0, vh), t.border), all));

        // Cell fills (over gridlines, like Excel) and borders.
        let mut border_ops = Vec::new();
        for r in first_row..=last_row {
            let y = row_y(r);
            for &(c, x, w) in &cols {
                let st = self.wb.style(self.at(CellRef::new(r, c)));
                if let Some(f) = st.fill {
                    ops.push(Op::Quad(fill(b(x, y - 1.0, w, ROW_H + 1.0), rgb(f)), grid_clip));
                }
                if st.has_borders() {
                    let line = |b: &xl_engine::Border| b.color.map_or(t.text, rgb);
                    let mut edge = |bd: &xl_engine::Border, bx: f64, by: f64, bw: f64, bh: f64| {
                        if bd.style == LineStyle::None {
                            return;
                        }
                        let q = if bd.style == LineStyle::Double {
                            // Two thin lines with a gap.
                            let horiz = bw > bh;
                            let (w2, h2) = if horiz { (bw, 1.0) } else { (1.0, bh) };
                            border_ops.push(Op::Quad(fill(b(bx, by, w2, h2), line(bd)), grid_clip));
                            let (ox2, oy2) = if horiz { (0.0, 2.0) } else { (2.0, 0.0) };
                            fill(b(bx + ox2, by + oy2, w2, h2), line(bd))
                        } else {
                            fill(b(bx, by, bw, bh), line(bd))
                        };
                        border_ops.push(Op::Quad(q, grid_clip));
                    };
                    let tw = |bd: &xl_engine::Border| bd.style.width().max(1.0) as f64;
                    edge(&st.top, x - 1.0, y - 1.0, w + 1.0, tw(&st.top));
                    edge(&st.bottom, x - 1.0, y + ROW_H - tw(&st.bottom), w + 1.0, tw(&st.bottom));
                    edge(&st.left, x - 1.0, y - 1.0, tw(&st.left), ROW_H + 1.0);
                    edge(&st.right, x + w - tw(&st.right), y - 1.0, tw(&st.right), ROW_H + 1.0);
                }
            }
        }
        ops.extend(border_ops);

        // Selection fill (the active cell stays clear).
        if sel.start != sel.end {
            ops.push(Op::Quad(fill(rect_of(sel), t.selection), grid_clip));
            ops.push(Op::Quad(fill(rect_of(Range::single(self.active)), t.bg), grid_clip));
        }

        // Reference highlights while writing a formula.
        let refs = self.visible_refs();
        for r in &refs {
            let color = t.ref_colors[r.color % t.ref_colors.len()];
            let mut soft = color;
            soft.a = 0.08;
            let pointed = self.edit.as_ref().and_then(|e| e.point.as_ref()).is_some_and(|p| p.span == r.span);
            let style = if pointed { BorderStyle::Dashed } else { BorderStyle::Solid };
            ops.push(Op::Quad(border_quad(rect_of(r.range), 1.5, color, soft, style, 1.0), grid_clip));
        }

        // Cell text.
        let f = cell_font();
        let lh = px(ROW_H as f32);
        for r in first_row..=last_row {
            let y = row_y(r);
            for (i, &(c, x, w)) in cols.iter().enumerate() {
                let cell = CellRef::new(r, c);
                if editing.is_some() && cell == self.active {
                    continue;
                }
                let v = store.value(cell);
                if v.is_empty() {
                    continue;
                }
                let shown = self.wb.display(self.at(cell));
                let st = self.wb.style(self.at(cell));
                let color = match (v, shown.color) {
                    (Value::Error(_), _) => t.error,
                    (_, Some(fc)) => fmt_color(&t, fc),
                    _ => text_color(&t, st),
                };
                let mut line = shape_styled(window, shown.text, st, color);
                let tw = f64::from(line.width);
                let avail = w - 2.0 * CELL_PAD;
                if matches!(v, Value::Number(_)) && tw > avail {
                    line = shape_styled(window, "#".repeat((avail / 8.0).max(1.0) as usize), st, color);
                }
                let lw = f64::from(line.width);
                let align = match (st.align, v) {
                    (HAlign::General, Value::Number(_)) => HAlign::Right,
                    (HAlign::General, Value::Bool(_) | Value::Error(_)) => HAlign::Center,
                    (HAlign::General, _) => HAlign::Left,
                    (a, _) => a,
                };
                let (tx, clip_w) = match align {
                    HAlign::Right => (x + w - CELL_PAD - lw, w),
                    HAlign::Center => (x + (w - lw) / 2.0, w),
                    _ if matches!(v, Value::Number(_)) => (x + CELL_PAD, w),
                    _ => {
                        // Text overflows into empty neighbours, like Excel.
                        let mut cw = w;
                        if tw > avail {
                            for &(c2, _, w2) in &cols[i + 1..] {
                                let n = CellRef::new(r, c2);
                                if !store.value(n).is_empty() || (editing.is_some() && n == self.active) {
                                    break;
                                }
                                cw += w2;
                                if cw - 2.0 * CELL_PAD >= tw {
                                    break;
                                }
                            }
                        }
                        (x + CELL_PAD, cw)
                    }
                };
                let clip = b(x, y, clip_w - 1.0, ROW_H).intersect(&grid_clip);
                ops.push(Op::Text { line: Box::new(line), origin: point(px(tx as f32), px(y as f32)), clip, line_height: lh });
            }
        }

        // Selection outline, active cell, fill handle.
        if sel.start != sel.end {
            ops.push(Op::Quad(border_quad(rect_of(sel), 1.0, t.accent, gpui::transparent_black(), BorderStyle::Solid, 0.0), grid_clip));
        }
        let ab = rect_of(Range::single(self.active));
        if editing.is_none() {
            let outer = b(f64::from(ab.origin.x) - 1.0, f64::from(ab.origin.y) - 1.0, f64::from(ab.size.width) + 1.0, f64::from(ab.size.height) + 1.0);
            ops.push(Op::Quad(border_quad(outer, 2.0, t.accent, gpui::transparent_black(), BorderStyle::Solid, 2.0), grid_clip));
            let sb = rect_of(sel);
            let hx = f64::from(sb.origin.x + sb.size.width) - 4.0;
            let hy = f64::from(sb.origin.y + sb.size.height) - 4.0;
            ops.push(Op::Quad(border_quad(b(hx, hy, 7.0, 7.0), 1.5, t.bg, t.accent, BorderStyle::Solid, 1.5), grid_clip));
        }

        // Header labels.
        let hf = Font { weight: FontWeight::MEDIUM, ..cell_font() };
        for &(c, x, w) in &cols {
            let active = c >= sel.start.col && c <= sel.end.col;
            let line = shape(window, col_name(c), hf.clone(), if active { t.header_active_fg } else { t.header_fg }, HEADER_FONT);
            let tx = x + (w - f64::from(line.width)) / 2.0;
            ops.push(Op::Text { line: Box::new(line), origin: point(px(tx as f32), px(oy as f32)), clip: col_hdr_clip, line_height: px(HEADER_H as f32) });
        }
        for r in first_row..=last_row {
            let active = r >= sel.start.row && r <= sel.end.row;
            let line = shape(window, (r + 1).to_string(), hf.clone(), if active { t.header_active_fg } else { t.header_fg }, HEADER_FONT);
            let tx = ox + (HEADER_W - f64::from(line.width)) / 2.0;
            ops.push(Op::Text { line: Box::new(line), origin: point(px(tx as f32), px(row_y(r) as f32)), clip: row_hdr_clip, line_height: lh });
        }

        // In-cell editor.
        if let Some(e) = editing {
            let display = e.text.replace('\u{2028}', "↵");
            let all_refs = scan_refs(&e.text);
            let shift = |i: usize| e.text[..i].replace('\u{2028}', "↵").len();
            let base = TextRun { len: 0, font: f.clone(), color: t.text, background_color: None, underline: None, strikethrough: None };
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
            let eb = b(f64::from(ab.origin.x) - 1.0, f64::from(ab.origin.y) - 1.0, w, ROW_H + 1.0);
            ops.push(Op::Quad(border_quad(eb, 2.0, t.accent, t.editor_bg, BorderStyle::Solid, 2.0), grid_clip));
            let caret_x = f64::from(ab.origin.x) + CELL_PAD + f64::from(line.x_for_index(shift(e.cursor)));
            if let Some(m) = &e.marked {
                // Underline the input-method composition.
                let x0 = f64::from(ab.origin.x) + CELL_PAD + f64::from(line.x_for_index(shift(m.start)));
                let x1 = f64::from(ab.origin.x) + CELL_PAD + f64::from(line.x_for_index(shift(m.end)));
                ops.push(Op::Quad(fill(b(x0, f64::from(ab.origin.y) + ROW_H - 5.0, x1 - x0, 1.5), t.accent), grid_clip));
            }
            let origin = point(px((f64::from(ab.origin.x) + CELL_PAD) as f32), ab.origin.y);
            ops.push(Op::Text { line: Box::new(line), origin, clip: grid_clip, line_height: lh });
            ops.push(Op::Quad(fill(b(caret_x, f64::from(ab.origin.y) + 4.0, 1.5, ROW_H - 8.0), t.accent), grid_clip));
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
