//! Grouping rows and columns (outlines): levels, the gutter beside the
//! headers with brackets and +/− buttons, and collapsing or expanding.

use gpui::{Bounds, Pixels, Point, Window, fill, point, px};
use xl_engine::{MAX_COLS, MAX_ROWS};

use crate::app::{HEADER_H, HEADER_W, OUTLINE_STEP, Spreadsheet};
use crate::paint::{Op, cell_font, outline, rect, shape};

/// Deepest outline level, like Excel.
const MAX_LEVEL: u8 = 7;
const BUTTON: f64 = 11.0;

/// What an outline button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutlineAction {
    /// Collapse or expand the group spanning these lines.
    Toggle { rows: bool, start: u32, end: u32 },
    /// Show only levels below this one (the numbered buttons in the corner).
    Level { rows: bool, level: u8 },
}

impl Spreadsheet {
    fn levels(&self, rows: bool) -> &std::collections::BTreeMap<u32, u8> {
        let l = self.wb.layout(self.cur);
        if rows { &l.row_levels } else { &l.col_levels }
    }

    /// Groups as (level, first line, last line), outer levels first.
    pub(crate) fn outline_runs(&self, rows: bool) -> Vec<(u8, u32, u32)> {
        let levels = self.levels(rows);
        let deepest = levels.values().copied().max().unwrap_or(0);
        let mut out = Vec::new();
        for level in 1..=deepest {
            let mut run: Option<(u32, u32)> = None;
            for (&i, &l) in levels.iter() {
                if l < level {
                    continue;
                }
                run = match run {
                    Some((a, b)) if i == b + 1 => Some((a, i)),
                    Some((a, b)) => {
                        out.push((level, a, b));
                        Some((i, i))
                    }
                    None => Some((i, i)),
                };
            }
            if let Some((a, b)) = run {
                out.push((level, a, b));
            }
        }
        out
    }

    fn line_hidden(&self, rows: bool, i: u32) -> bool {
        if rows { self.row_h(i) == 0.0 } else { self.col_w(i) == 0.0 }
    }

    fn group_collapsed(&self, rows: bool, start: u32, end: u32) -> bool {
        (start..=end).all(|i| self.line_hidden(rows, i))
    }

    /// The outline buttons on screen: where, what they do, and their label.
    pub(crate) fn outline_buttons(&self) -> Vec<(Bounds<Pixels>, OutlineAction, String)> {
        let (ox, oy) = (f64::from(self.viewport.origin.x), f64::from(self.viewport.origin.y));
        let (row_levels, col_levels) = self.outline_levels();
        let (gutter_w, gutter_h) = (row_levels as f64 * OUTLINE_STEP, col_levels as f64 * OUTLINE_STEP);
        let mut out = Vec::new();
        // Level buttons in the corner.
        for k in 1..=row_levels {
            let x = ox + (k - 1) as f64 * OUTLINE_STEP + (OUTLINE_STEP - BUTTON) / 2.0;
            let y = oy + gutter_h + (HEADER_H - BUTTON) / 2.0;
            out.push((rect(x, y, BUTTON, BUTTON), OutlineAction::Level { rows: true, level: k }, k.to_string()));
        }
        for k in 1..=col_levels {
            let x = ox + gutter_w + (HEADER_W - BUTTON) / 2.0;
            let y = oy + (k - 1) as f64 * OUTLINE_STEP + (OUTLINE_STEP - BUTTON) / 2.0;
            out.push((rect(x, y, BUTTON, BUTTON), OutlineAction::Level { rows: false, level: k }, k.to_string()));
        }
        // +/− after each group (Excel puts the summary row below, the summary column right).
        for (level, start, end) in self.outline_runs(true) {
            if end + 1 >= MAX_ROWS {
                continue;
            }
            let y = oy + self.hh() + self.grid_y(end + 1) + (self.row_h(end + 1).max(1.0) - BUTTON) / 2.0;
            let x = ox + (level - 1) as f64 * OUTLINE_STEP + (OUTLINE_STEP - BUTTON) / 2.0;
            let label = if self.group_collapsed(true, start, end) { "+" } else { "−" };
            out.push((rect(x, y, BUTTON, BUTTON), OutlineAction::Toggle { rows: true, start, end }, label.into()));
        }
        for (level, start, end) in self.outline_runs(false) {
            if end + 1 >= MAX_COLS {
                continue;
            }
            let x = ox + self.hw() + self.grid_x(end + 1) + (self.col_w(end + 1).max(1.0) - BUTTON) / 2.0;
            let y = oy + (level - 1) as f64 * OUTLINE_STEP + (OUTLINE_STEP - BUTTON) / 2.0;
            let label = if self.group_collapsed(false, start, end) { "+" } else { "−" };
            out.push((rect(x, y, BUTTON, BUTTON), OutlineAction::Toggle { rows: false, start, end }, label.into()));
        }
        out
    }

    pub(crate) fn outline_button_at(&self, pos: Point<Pixels>) -> Option<OutlineAction> {
        if self.outline_levels() == (0, 0) {
            return None;
        }
        self.outline_buttons().into_iter().find(|(b, _, _)| b.dilate(px(2.)).contains(&pos)).map(|(_, a, _)| a)
    }

    pub(crate) fn outline_click(&mut self, action: OutlineAction) {
        self.change_layout(None, |s| {
            let Some(l) = s.wb.layout_mut(s.cur) else { return };
            match action {
                OutlineAction::Toggle { rows, start, end } => {
                    let hidden = if rows { &mut l.hidden_rows } else { &mut l.hidden_cols };
                    if (start..=end).all(|i| hidden.contains(&i)) {
                        for i in start..=end {
                            hidden.remove(&i);
                        }
                    } else {
                        hidden.extend(start..=end);
                    }
                }
                OutlineAction::Level { rows, level } => {
                    let (levels, hidden) = if rows { (&l.row_levels, &mut l.hidden_rows) } else { (&l.col_levels, &mut l.hidden_cols) };
                    for (&i, &lv) in levels {
                        if lv >= level {
                            hidden.insert(i);
                        } else {
                            hidden.remove(&i);
                        }
                    }
                }
            }
        });
    }

    /// Group (or ungroup) the selected rows — or columns, when whole columns
    /// are selected.
    pub(crate) fn group(&mut self, ungroup: bool) {
        let sel = self.selection();
        let rows = !(self.cols_selected() && !self.rows_selected());
        let (a, z) = if rows { (sel.start.row, sel.end.row) } else { (sel.start.col, sel.end.col) };
        if z - a > 100_000 {
            self.message = Some(("Select fewer rows or columns to group".into(), true));
            return;
        }
        self.change_layout(None, |s| {
            let Some(l) = s.wb.layout_mut(s.cur) else { return };
            let levels = if rows { &mut l.row_levels } else { &mut l.col_levels };
            for i in a..=z {
                let cur = levels.get(&i).copied().unwrap_or(0);
                let next = if ungroup { cur.saturating_sub(1) } else { (cur + 1).min(MAX_LEVEL) };
                if next == 0 {
                    levels.remove(&i);
                    // Ungrouping shows what a collapsed group hid.
                    if rows { l.hidden_rows.remove(&i) } else { l.hidden_cols.remove(&i) };
                } else {
                    levels.insert(i, next);
                }
            }
        });
    }

    /// Paint the outline gutters: brackets along each group and its button.
    pub(crate) fn outline_ops(&self, window: &mut Window) -> Vec<Op> {
        let t = self.theme;
        let mut ops = Vec::new();
        let (row_levels, col_levels) = self.outline_levels();
        if (row_levels, col_levels) == (0, 0) {
            return ops;
        }
        let (ox, oy) = (f64::from(self.viewport.origin.x), f64::from(self.viewport.origin.y));
        let (vw, vh) = (f64::from(self.viewport.size.width), f64::from(self.viewport.size.height));
        let row_gutter = rect(ox, oy + self.hh(), row_levels as f64 * OUTLINE_STEP, vh - self.hh());
        let col_gutter = rect(ox + self.hw(), oy, vw - self.hw(), col_levels as f64 * OUTLINE_STEP);
        let corner = rect(ox, oy, self.hw(), self.hh());
        for (level, start, end) in self.outline_runs(true) {
            if self.group_collapsed(true, start, end) {
                continue;
            }
            let x = ox + (level - 1) as f64 * OUTLINE_STEP + OUTLINE_STEP / 2.0;
            let y0 = oy + self.hh() + self.grid_y(start) + 2.0;
            let y1 = oy + self.hh() + self.grid_y(end) + self.row_h(end);
            ops.push(Op::Quad(fill(rect(x, y0, 1.0, y1 - y0), t.ink_faint), row_gutter));
            ops.push(Op::Quad(fill(rect(x - 3.0, y0, 4.0, 1.0), t.ink_faint), row_gutter));
        }
        for (level, start, end) in self.outline_runs(false) {
            if self.group_collapsed(false, start, end) {
                continue;
            }
            let y = oy + (level - 1) as f64 * OUTLINE_STEP + OUTLINE_STEP / 2.0;
            let x0 = ox + self.hw() + self.grid_x(start) + 2.0;
            let x1 = ox + self.hw() + self.grid_x(end) + self.col_w(end);
            ops.push(Op::Quad(fill(rect(x0, y, x1 - x0, 1.0), t.ink_faint), col_gutter));
            ops.push(Op::Quad(fill(rect(x0, y - 3.0, 1.0, 4.0), t.ink_faint), col_gutter));
        }
        for (b, action, label) in self.outline_buttons() {
            let clip = match action {
                OutlineAction::Level { .. } => corner,
                OutlineAction::Toggle { rows: true, .. } => row_gutter,
                OutlineAction::Toggle { rows: false, .. } => col_gutter,
            };
            ops.push(Op::Quad(outline(b, 1.0, t.ink_faint, t.canvas, 2.0), clip));
            let line = shape(window, label, cell_font(), t.ink_muted, 10.5);
            let (bx, by) = (f64::from(b.origin.x), f64::from(b.origin.y));
            let lw = f64::from(line.width);
            ops.push(Op::Text { line: Box::new(line), origin: point(px((bx + (BUTTON - lw) / 2.0) as f32), px((by - 1.0) as f32)), clip, line_height: px(BUTTON as f32 + 1.0) });
        }
        ops
    }
}
