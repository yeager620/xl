//! The spreadsheet view: state, input handling and grid painting.

use std::collections::BTreeMap;
use std::path::PathBuf;

use gpui::{
    BorderStyle, Bounds, ClipboardItem, ContentMask, Context, Corners, Edges, FocusHandle, Font, FontWeight,
    Hsla, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, PathPromptOptions,
    Pixels, Point, Render, ScrollEvent, ShapedLine, SharedString, Subscription, TextAlign, TextRun, Window,
    canvas, div, fill, font, point, prelude::*, px, quad, size,
};
use xl_engine::{CellRef, MAX_COLS, MAX_ROWS, Range, RecalcStats, Sheet, Value, col_name};

use crate::refs::{RefSpan, scan_refs};
use crate::theme::Theme;

pub const ROW_H: f64 = 24.0;
pub const DEF_COL_W: f64 = 100.0;
pub const MIN_COL_W: f64 = 28.0;
pub const HEADER_W: f64 = 52.0;
pub const HEADER_H: f64 = 26.0;
const CELL_PAD: f64 = 6.0;
const CELL_FONT: f32 = 13.0;
const HEADER_FONT: f32 = 11.5;
const MAX_BULK: u64 = 2_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EditMode {
    /// Started by typing over a cell: arrows commit and move.
    Enter,
    /// Started with F2 / double-click: arrows move the caret.
    Edit,
}

#[derive(Clone, Debug)]
pub struct PointRef {
    anchor: CellRef,
    cell: CellRef,
    span: std::ops::Range<usize>,
}

#[derive(Clone, Debug)]
pub struct Edit {
    pub text: String,
    pub cursor: usize,
    pub mode: EditMode,
    point: Option<PointRef>,
}

impl Edit {
    /// Whether a reference can be inserted at the caret (Excel "Point" mode).
    fn can_point(&self) -> bool {
        if !self.text.starts_with('=') || self.cursor != self.text.len() {
            return false;
        }
        if self.point.is_some() {
            return true;
        }
        matches!(self.text.trim_end().chars().last(), Some('=' | '(' | ',' | '+' | '-' | '*' | '/' | '^' | '&' | '<' | '>' | ':'))
    }
}

#[derive(Clone, Copy)]
enum Drag {
    Select,
    Point,
    ResizeCol { col: u32, start_x: f64, start_w: f64 },
}

enum Hit {
    Corner,
    ColHeader { col: u32, border: Option<u32> },
    RowHeader(u32),
    Cell(CellRef),
}

struct Clip {
    range: Range,
    text: String,
}

pub struct Spreadsheet {
    sheet: Sheet,
    focus: FocusHandle,
    active: CellRef,
    anchor: CellRef,
    scroll_x: f64,
    scroll_y: f64,
    col_widths: BTreeMap<u32, f64>,
    viewport: Bounds<Pixels>,
    edit: Option<Edit>,
    drag: Option<Drag>,
    undo: Vec<Vec<(CellRef, String)>>,
    redo: Vec<Vec<(CellRef, String)>>,
    clip: Option<Clip>,
    last_stats: Option<RecalcStats>,
    message: Option<(String, bool)>,
    path: Option<PathBuf>,
    dirty: bool,
    theme: Theme,
    theme_override: Option<bool>,
    last_title: String,
    _appearance: Subscription,
}

impl Spreadsheet {
    pub fn new(path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let appearance = cx.observe_window_appearance(window, |this: &mut Self, window, cx| {
            if this.theme_override.is_none() {
                this.theme = Theme::for_appearance(window.appearance());
                cx.notify();
            }
        });
        let mut this = Self {
            sheet: Sheet::new(),
            focus,
            active: CellRef::new(0, 0),
            anchor: CellRef::new(0, 0),
            scroll_x: 0.0,
            scroll_y: 0.0,
            col_widths: BTreeMap::new(),
            viewport: Bounds::default(),
            edit: None,
            drag: None,
            undo: Vec::new(),
            redo: Vec::new(),
            clip: None,
            last_stats: None,
            message: None,
            path: None,
            dirty: false,
            theme: Theme::for_appearance(window.appearance()),
            theme_override: None,
            last_title: String::new(),
            _appearance: appearance,
        };
        if let Some(p) = path {
            this.load(p);
        } else {
            this.load_welcome();
        }
        this
    }

    // ---------------------------------------------------------------- geometry

    fn col_w(&self, c: u32) -> f64 {
        self.col_widths.get(&c).copied().unwrap_or(DEF_COL_W)
    }

    /// Content-space x of the left edge of column `c`.
    fn col_x(&self, c: u32) -> f64 {
        let mut x = c as f64 * DEF_COL_W;
        for &w in self.col_widths.range(..c).map(|(_, w)| w) {
            x += w - DEF_COL_W;
        }
        x
    }

    /// Column containing content-space x.
    fn col_at(&self, x: f64) -> u32 {
        if x <= 0.0 {
            return 0;
        }
        let (mut lo, mut hi) = (0u32, MAX_COLS - 1);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if self.col_x(mid) <= x { lo = mid } else { hi = mid - 1 }
        }
        lo
    }

    fn row_at(&self, y: f64) -> u32 {
        ((y / ROW_H).floor().max(0.0) as u32).min(MAX_ROWS - 1)
    }

    fn grid_w(&self) -> f64 {
        (f64::from(self.viewport.size.width) - HEADER_W).max(0.0)
    }

    fn grid_h(&self) -> f64 {
        (f64::from(self.viewport.size.height) - HEADER_H).max(0.0)
    }

    fn visible_rows(&self) -> u32 {
        ((self.grid_h() / ROW_H).floor() as u32).max(1)
    }

    fn max_scroll_x(&self) -> f64 {
        (self.col_x(MAX_COLS) - self.grid_w()).max(0.0)
    }

    fn max_scroll_y(&self) -> f64 {
        (MAX_ROWS as f64 * ROW_H - self.grid_h()).max(0.0)
    }

    fn ensure_visible(&mut self, c: CellRef) {
        if self.viewport.size.width <= px(0.) {
            return;
        }
        let (x0, x1) = (self.col_x(c.col), self.col_x(c.col) + self.col_w(c.col));
        if x0 < self.scroll_x {
            self.scroll_x = x0;
        } else if x1 > self.scroll_x + self.grid_w() {
            self.scroll_x = (x1 - self.grid_w()).min(x0);
        }
        let (y0, y1) = (c.row as f64 * ROW_H, (c.row + 1) as f64 * ROW_H);
        if y0 < self.scroll_y {
            self.scroll_y = y0;
        } else if y1 > self.scroll_y + self.grid_h() {
            self.scroll_y = y1 - self.grid_h();
        }
    }

    fn hit_test(&self, pos: Point<Pixels>) -> Option<Hit> {
        let x = f64::from(pos.x - self.viewport.origin.x);
        let y = f64::from(pos.y - self.viewport.origin.y);
        if x < 0.0 || y < 0.0 || x > f64::from(self.viewport.size.width) || y > f64::from(self.viewport.size.height) {
            return None;
        }
        let cx = x - HEADER_W + self.scroll_x;
        let cy = y - HEADER_H + self.scroll_y;
        Some(match (x < HEADER_W, y < HEADER_H) {
            (true, true) => Hit::Corner,
            (false, true) => {
                let col = self.col_at(cx);
                let right = self.col_x(col) + self.col_w(col);
                let left = self.col_x(col);
                let border = if (right - cx).abs() <= 4.0 {
                    Some(col)
                } else if (cx - left).abs() <= 4.0 && col > 0 {
                    Some(col - 1)
                } else {
                    None
                };
                Hit::ColHeader { col, border }
            }
            (true, false) => Hit::RowHeader(self.row_at(cy)),
            (false, false) => Hit::Cell(CellRef::new(self.row_at(cy), self.col_at(cx))),
        })
    }

    // ------------------------------------------------------------- selection

    pub fn selection(&self) -> Range {
        Range::new(self.anchor, self.active)
    }

    fn select(&mut self, c: CellRef, extend: bool) {
        self.active = c;
        if !extend {
            self.anchor = c;
        }
        self.ensure_visible(c);
    }

    fn move_by(&mut self, dr: i64, dc: i64, extend: bool) {
        let from = self.active;
        let to = CellRef::new(
            (from.row as i64 + dr).clamp(0, MAX_ROWS as i64 - 1) as u32,
            (from.col as i64 + dc).clamp(0, MAX_COLS as i64 - 1) as u32,
        );
        self.select(to, extend);
    }

    /// Ctrl+Arrow: jump to the edge of the current data region.
    fn jump(&mut self, dr: i64, dc: i64, extend: bool) {
        let s = self.sheet.store();
        let filled = |c: CellRef| !s.value(c).is_empty();
        let mut cur = self.active;
        let step = |c: CellRef| c.offset(dr, dc);
        let Some(next) = step(cur) else { return };
        if filled(cur) && filled(next) {
            // Run to the last filled cell of this block.
            cur = next;
            while let Some(n) = step(cur) {
                if !filled(n) {
                    break;
                }
                cur = n;
            }
        } else if dc == 0 {
            let forward = dr > 0;
            cur = match s.next_filled_in_col(cur.col, cur.row, forward) {
                Some(r) => CellRef::new(r, cur.col),
                None => CellRef::new(if forward { MAX_ROWS - 1 } else { 0 }, cur.col),
            };
        } else {
            let limit = if dc > 0 { s.col_count().min(MAX_COLS) } else { 0 };
            let mut found = None;
            let mut c = cur;
            while let Some(n) = step(c) {
                if (dc > 0 && n.col >= limit) || filled(n) {
                    found = filled(n).then_some(n);
                    break;
                }
                c = n;
            }
            cur = found.unwrap_or(CellRef::new(cur.row, if dc > 0 { MAX_COLS - 1 } else { 0 }));
        }
        self.select(cur, extend);
    }

    // ---------------------------------------------------------------- editing

    fn begin_edit(&mut self, initial: Option<String>, mode: EditMode) {
        let text = initial.unwrap_or_else(|| self.sheet.input_text(self.active));
        let cursor = text.len();
        self.edit = Some(Edit { text, cursor, mode, point: None });
        self.message = None;
    }

    /// Returns false if the formula failed to parse (edit stays open).
    fn commit_edit(&mut self) -> bool {
        let Some(edit) = self.edit.take() else { return true };
        let old = self.sheet.input_text(self.active);
        if edit.text == old {
            return true;
        }
        match self.sheet.set_input(self.active, &edit.text) {
            Ok(stats) => {
                self.push_undo(vec![(self.active, old)]);
                self.last_stats = Some(stats);
                self.message = None;
                self.dirty = true;
                true
            }
            Err(e) => {
                self.message = Some((format!("Formula error: {e}"), true));
                self.edit = Some(edit);
                false
            }
        }
    }

    fn cancel_edit(&mut self) {
        self.edit = None;
        self.message = None;
    }

    fn push_undo(&mut self, entry: Vec<(CellRef, String)>) {
        self.undo.push(entry);
        self.redo.clear();
        if self.undo.len() > 500 {
            self.undo.remove(0);
        }
    }

    /// Apply a batch of raw inputs with a single recalc and an undo entry.
    fn apply(&mut self, changes: Vec<(CellRef, String)>) {
        if changes.is_empty() {
            return;
        }
        let old: Vec<_> = changes.iter().map(|(c, _)| (*c, self.sheet.input_text(*c))).collect();
        self.last_stats = Some(self.sheet.set_inputs(changes));
        self.push_undo(old);
        self.dirty = true;
    }

    fn undo_redo(&mut self, is_undo: bool) {
        let entry = if is_undo { self.undo.pop() } else { self.redo.pop() };
        let Some(entry) = entry else { return };
        let inverse: Vec<_> = entry.iter().map(|(c, _)| (*c, self.sheet.input_text(*c))).collect();
        if let (Some(first), Some(last)) = (entry.first(), entry.last()) {
            self.anchor = first.0;
            self.active = last.0;
            self.ensure_visible(self.active);
        }
        self.last_stats = Some(self.sheet.set_inputs(entry));
        if is_undo { self.redo.push(inverse) } else { self.undo.push(inverse) }
        self.dirty = true;
    }

    fn clear_selection(&mut self) {
        let sel = self.selection();
        let cells: Vec<_> = self.sheet.store().iter_range(sel).map(|(c, _)| (c, String::new())).collect();
        self.apply(cells);
    }

    fn guard_bulk(&mut self, r: Range) -> bool {
        if r.cell_count() > MAX_BULK {
            self.message = Some((format!("Selection too large ({} cells)", r.cell_count()), true));
            return false;
        }
        true
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        let sel = self.selection();
        if !self.guard_bulk(sel) {
            return;
        }
        let mut text = String::new();
        for r in sel.start.row..=sel.end.row {
            for c in sel.start.col..=sel.end.col {
                if c > sel.start.col {
                    text.push('\t');
                }
                text.push_str(&self.sheet.value(CellRef::new(r, c)).display());
            }
            text.push('\n');
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        self.clip = Some(Clip { range: sel, text });
        self.message = Some((format!("Copied {}", sel), false));
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) else { return };
        let origin = self.selection().start;
        let mut changes = Vec::new();
        let mut end = origin;
        if let Some(clip) = self.clip.as_ref().filter(|c| c.text == text) {
            // Internal paste: carry formulas, shifting relative references.
            let (dr, dc) = (origin.row as i64 - clip.range.start.row as i64, origin.col as i64 - clip.range.start.col as i64);
            for src in clip.range.iter() {
                if let Some(dst) = src.offset(dr, dc) {
                    changes.push((dst, self.sheet.input_text_shifted(src, dr, dc)));
                    end = dst;
                }
            }
        } else {
            let text = text.strip_suffix('\n').unwrap_or(&text);
            for (i, line) in text.split('\n').enumerate() {
                for (j, field) in line.trim_end_matches('\r').split('\t').enumerate() {
                    if let Some(dst) = origin.offset(i as i64, j as i64) {
                        changes.push((dst, field.to_string()));
                        end = CellRef::new(end.row.max(dst.row), end.col.max(dst.col));
                    }
                }
            }
        }
        self.apply(changes);
        self.anchor = origin;
        self.active = end;
    }

    /// Ctrl+D / Ctrl+R.
    fn fill(&mut self, down: bool) {
        let sel = self.selection();
        if !self.guard_bulk(sel) {
            return;
        }
        let mut changes = Vec::new();
        if down {
            let (src_row, first) = if sel.rows() == 1 {
                if sel.start.row == 0 {
                    return;
                }
                (sel.start.row - 1, sel.start.row)
            } else {
                (sel.start.row, sel.start.row + 1)
            };
            for c in sel.start.col..=sel.end.col {
                let src = CellRef::new(src_row, c);
                for r in first..=sel.end.row {
                    changes.push((CellRef::new(r, c), self.sheet.input_text_shifted(src, (r - src_row) as i64, 0)));
                }
            }
        } else {
            let (src_col, first) = if sel.cols() == 1 {
                if sel.start.col == 0 {
                    return;
                }
                (sel.start.col - 1, sel.start.col)
            } else {
                (sel.start.col, sel.start.col + 1)
            };
            for r in sel.start.row..=sel.end.row {
                let src = CellRef::new(r, src_col);
                for c in first..=sel.end.col {
                    changes.push((CellRef::new(r, c), self.sheet.input_text_shifted(src, 0, (c - src_col) as i64)));
                }
            }
        }
        self.apply(changes);
    }

    /// Insert or replace the pointed reference in the formula being edited.
    fn point_to(&mut self, cell: CellRef, extend: bool) {
        let Some(edit) = self.edit.as_mut() else { return };
        let (anchor, span) = match &edit.point {
            Some(p) => (if extend { p.anchor } else { cell }, p.span.clone()),
            None => (cell, edit.cursor..edit.cursor),
        };
        let r = Range::new(anchor, cell);
        let text = r.to_string();
        edit.text.replace_range(span.clone(), &text);
        let span = span.start..span.start + text.len();
        edit.cursor = span.end;
        edit.point = Some(PointRef { anchor, cell, span });
        self.ensure_visible(cell);
    }

    // ------------------------------------------------------------------ files

    fn load_welcome(&mut self) {
        let rows: &[&[&str]] = &[
            &["Welcome to xl", "", "", ""],
            &["", "", "", ""],
            &["Item", "Qty", "Price", "Total"],
            &["Coffee beans", "3", "14.5", "=B4*C4"],
            &["Oat milk", "6", "2.25", "=B5*C5"],
            &["Croissants", "12", "1.8", "=B6*C6"],
            &["", "", "Subtotal", "=SUM(D4:D6)"],
            &["", "", "Tax", "=ROUND(D7*0.0825,2)"],
            &["", "", "Total", "=D7+D8"],
            &["", "", "", ""],
            &["Try: edit a Qty — totals update instantly. Type = then use the arrow keys to point at cells.", "", "", ""],
        ];
        let mut items = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            for (c, v) in row.iter().enumerate() {
                if !v.is_empty() {
                    items.push((CellRef::new(r as u32, c as u32), v.to_string()));
                }
            }
        }
        self.col_widths.insert(0, 140.0);
        self.last_stats = Some(self.sheet.set_inputs(items));
        self.active = CellRef::new(3, 1);
        self.anchor = self.active;
    }

    fn load(&mut self, path: PathBuf) {
        match read_csv(&path) {
            Ok(items) => {
                let mut sheet = Sheet::new();
                let n = items.len();
                let stats = sheet.set_inputs(items);
                self.sheet = sheet;
                self.last_stats = Some(stats);
                self.undo.clear();
                self.redo.clear();
                self.col_widths.clear();
                self.active = CellRef::new(0, 0);
                self.anchor = self.active;
                self.scroll_x = 0.0;
                self.scroll_y = 0.0;
                self.edit = None;
                self.dirty = false;
                self.message = Some((format!("Opened {} ({n} cells)", path.display()), false));
                self.path = Some(path);
            }
            Err(e) => self.message = Some((format!("Could not open {}: {e}", path.display()), true)),
        }
    }

    fn save_to(&mut self, path: PathBuf) {
        match write_csv(&self.sheet, &path) {
            Ok(()) => {
                self.dirty = false;
                self.message = Some((format!("Saved {}", path.display()), false));
                self.path = Some(path);
            }
            Err(e) => self.message = Some((format!("Save failed: {e}"), true)),
        }
    }

    fn save(&mut self, save_as: bool, cx: &mut Context<Self>) {
        if let (Some(p), false) = (self.path.clone(), save_as) {
            self.save_to(p);
            return;
        }
        let dir = self
            .path
            .as_ref()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some("Untitled.csv"));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                this.update(cx, |this, cx| {
                    this.save_to(path);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn open(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| {
                    this.load(path);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    // ------------------------------------------------------------------ input

    fn on_key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let m = ks.modifiers;
        let key = ks.key.as_str();
        let ctrl = m.control || m.platform;
        let handled = if self.edit.is_some() {
            self.key_editing(key, ks.key_char.as_deref(), ctrl, m.shift, m.alt)
        } else {
            self.key_navigating(key, ks.key_char.as_deref(), ctrl, m.shift, m.alt, cx)
        };
        if handled {
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn key_editing(&mut self, key: &str, ch: Option<&str>, ctrl: bool, shift: bool, alt: bool) -> bool {
        let pointing = self.edit.as_ref().is_some_and(|e| e.can_point());
        let mode = self.edit.as_ref().unwrap().mode;
        let arrow = match key {
            "up" => Some((-1, 0)),
            "down" => Some((1, 0)),
            "left" => Some((0, -1)),
            "right" => Some((0, 1)),
            _ => None,
        };
        if let Some((dr, dc)) = arrow {
            if pointing && mode == EditMode::Enter {
                let edit = self.edit.as_ref().unwrap();
                let base = edit.point.as_ref().map_or(self.active, |p| p.cell);
                if let Some(target) = base.offset(dr, dc) {
                    self.point_to(target, shift);
                }
                return true;
            }
            if mode == EditMode::Enter {
                if self.commit_edit() {
                    self.move_by(dr, dc, false);
                }
                return true;
            }
            let edit = self.edit.as_mut().unwrap();
            edit.point = None;
            match key {
                "left" => edit.cursor = prev_boundary(&edit.text, edit.cursor),
                "right" => edit.cursor = next_boundary(&edit.text, edit.cursor),
                "up" => edit.cursor = 0,
                _ => edit.cursor = edit.text.len(),
            }
            return true;
        }
        match key {
            "enter" if alt => self.insert_text("\u{2028}"),
            "enter" => {
                if self.commit_edit() {
                    self.move_by(if shift { -1 } else { 1 }, 0, false);
                }
            }
            "tab" => {
                if self.commit_edit() {
                    self.move_by(0, if shift { -1 } else { 1 }, false);
                }
            }
            "escape" => self.cancel_edit(),
            "f2" => {
                let e = self.edit.as_mut().unwrap();
                e.mode = if e.mode == EditMode::Enter { EditMode::Edit } else { EditMode::Enter };
            }
            "home" => self.edit.as_mut().unwrap().cursor = 0,
            "end" => {
                let e = self.edit.as_mut().unwrap();
                e.cursor = e.text.len();
            }
            "backspace" => {
                let e = self.edit.as_mut().unwrap();
                e.point = None;
                let p = prev_boundary(&e.text, e.cursor);
                e.text.replace_range(p..e.cursor, "");
                e.cursor = p;
            }
            "delete" => {
                let e = self.edit.as_mut().unwrap();
                e.point = None;
                let n = next_boundary(&e.text, e.cursor);
                e.text.replace_range(e.cursor..n, "");
            }
            "a" if ctrl => {
                let e = self.edit.as_mut().unwrap();
                e.cursor = e.text.len();
            }
            _ if ctrl => return false,
            _ => match ch {
                Some(s) if !s.is_empty() && !s.chars().any(char::is_control) => self.insert_text(s),
                _ => return false,
            },
        }
        true
    }

    fn insert_text(&mut self, s: &str) {
        let e = self.edit.as_mut().unwrap();
        e.point = None;
        e.text.insert_str(e.cursor, s);
        e.cursor += s.len();
    }

    #[allow(clippy::too_many_arguments)]
    fn key_navigating(
        &mut self,
        key: &str,
        ch: Option<&str>,
        ctrl: bool,
        shift: bool,
        alt: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let page = self.visible_rows() as i64;
        match (key, ctrl) {
            ("up", false) => self.move_by(-1, 0, shift),
            ("down", false) => self.move_by(1, 0, shift),
            ("left", false) => self.move_by(0, -1, shift),
            ("right", false) => self.move_by(0, 1, shift),
            ("up", true) => self.jump(-1, 0, shift),
            ("down", true) => self.jump(1, 0, shift),
            ("left", true) => self.jump(0, -1, shift),
            ("right", true) => self.jump(0, 1, shift),
            ("pageup", _) => {
                self.scroll_y = (self.scroll_y - page as f64 * ROW_H).max(0.0);
                self.move_by(-page, 0, shift);
            }
            ("pagedown", _) => {
                self.scroll_y = (self.scroll_y + page as f64 * ROW_H).min(self.max_scroll_y());
                self.move_by(page, 0, shift);
            }
            ("home", false) => self.select(CellRef::new(self.active.row, 0), shift),
            ("home", true) => self.select(CellRef::new(0, 0), shift),
            ("end", true) => {
                let end = self.sheet.store().extent().unwrap_or(CellRef::new(0, 0));
                self.select(end, shift);
            }
            ("enter", _) => self.move_by(if shift { -1 } else { 1 }, 0, false),
            ("tab", _) => self.move_by(0, if shift { -1 } else { 1 }, false),
            ("f2", _) => self.begin_edit(None, EditMode::Edit),
            ("delete" | "backspace", false) => self.clear_selection(),
            ("escape", _) => {
                self.anchor = self.active;
                self.message = None;
            }
            ("c", true) => self.copy(cx),
            ("x", true) => {
                self.copy(cx);
                self.clear_selection();
            }
            ("v", true) => self.paste(cx),
            ("z", true) if shift => self.undo_redo(false),
            ("z", true) => self.undo_redo(true),
            ("y", true) => self.undo_redo(false),
            ("d", true) => self.fill(true),
            ("r", true) => self.fill(false),
            ("a", true) => {
                let end = self.sheet.store().extent().unwrap_or(CellRef::new(0, 0));
                self.anchor = CellRef::new(0, 0);
                self.active = end;
            }
            ("s", true) => self.save(shift, cx),
            ("o", true) => self.open(cx),
            ("q", true) => cx.quit(),
            (_, true) => return false,
            _ => match ch {
                Some(s) if !s.is_empty() && !s.chars().any(char::is_control) && !alt => {
                    self.begin_edit(Some(s.to_string()), EditMode::Enter);
                }
                _ => return false,
            },
        }
        true
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        let Some(hit) = self.hit_test(ev.position) else { return };
        let shift = ev.modifiers.shift;
        // Clicking a cell while writing a formula inserts a reference.
        if let (Hit::Cell(c), Some(edit)) = (&hit, &self.edit)
            && edit.can_point()
        {
            self.point_to(*c, shift);
            self.drag = Some(Drag::Point);
            cx.notify();
            return;
        }
        if self.edit.is_some() && !self.commit_edit() {
            cx.notify();
            return;
        }
        match hit {
            Hit::Corner => {
                self.anchor = CellRef::new(0, 0);
                self.active = CellRef::new(MAX_ROWS - 1, MAX_COLS - 1);
            }
            Hit::ColHeader { border: Some(col), .. } => {
                let start_x = f64::from(ev.position.x);
                self.drag = Some(Drag::ResizeCol { col, start_x, start_w: self.col_w(col) });
                if ev.click_count == 2 {
                    self.autofit(col, window);
                    self.drag = None;
                }
            }
            Hit::ColHeader { col, .. } => {
                if !shift {
                    self.anchor = CellRef::new(0, col);
                }
                self.active = CellRef::new(MAX_ROWS - 1, col);
                self.anchor.row = 0;
            }
            Hit::RowHeader(row) => {
                if !shift {
                    self.anchor = CellRef::new(row, 0);
                }
                self.active = CellRef::new(row, MAX_COLS - 1);
                self.anchor.col = 0;
            }
            Hit::Cell(c) => {
                if ev.click_count >= 2 {
                    self.select(c, false);
                    self.begin_edit(None, EditMode::Edit);
                } else {
                    self.select(c, shift);
                    self.drag = Some(Drag::Select);
                }
            }
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag else { return };
        if ev.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return;
        }
        match drag {
            Drag::ResizeCol { col, start_x, start_w } => {
                let w = (start_w + f64::from(ev.position.x) - start_x).max(MIN_COL_W);
                self.col_widths.insert(col, w);
            }
            Drag::Select | Drag::Point => {
                // Clamp into the grid so dragging past an edge still extends.
                let vp = self.viewport;
                let p = point(
                    ev.position.x.clamp(vp.origin.x + px(HEADER_W as f32 + 1.), vp.origin.x + vp.size.width - px(1.)),
                    ev.position.y.clamp(vp.origin.y + px(HEADER_H as f32 + 1.), vp.origin.y + vp.size.height - px(1.)),
                );
                if let Some(Hit::Cell(c)) = self.hit_test(p) {
                    if matches!(drag, Drag::Point) {
                        self.point_to(c, true);
                    } else if c != self.active {
                        self.select(c, true);
                    }
                }
            }
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, _ev: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            cx.notify();
        }
    }

    fn on_scroll(&mut self, ev: &ScrollEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let d = ev.delta.pixel_delta(px(ROW_H as f32));
        let (mut dx, mut dy) = (f64::from(d.x), f64::from(d.y));
        if ev.modifiers.shift && dx == 0.0 {
            std::mem::swap(&mut dx, &mut dy);
        }
        self.scroll_x = (self.scroll_x - dx).clamp(0.0, self.max_scroll_x());
        self.scroll_y = (self.scroll_y - dy).clamp(0.0, self.max_scroll_y());
        cx.notify();
    }

    fn autofit(&mut self, col: u32, window: &mut Window) {
        let r = Range::new(CellRef::new(0, col), CellRef::new(MAX_ROWS - 1, col));
        let mut w: f64 = MIN_COL_W;
        let f = cell_font();
        for (_, cell) in self.sheet.store().iter_range(r).take(5_000) {
            let s = cell.value.display();
            let line = shape(window, s, f.clone(), self.theme.text, CELL_FONT);
            w = w.max(f64::from(line.width) + 2.0 * CELL_PAD + 2.0);
        }
        self.col_widths.insert(col, w.min(800.0));
    }

    fn toggle_theme(&mut self) {
        let dark = !self.theme.dark;
        self.theme_override = Some(dark);
        self.theme = if dark { Theme::dark() } else { Theme::light() };
    }

    // ------------------------------------------------------------------ render

    fn title(&self) -> String {
        let name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into());
        format!("{}{} — xl", if self.dirty { "• " } else { "" }, name)
    }

    fn selection_summary(&self) -> Option<String> {
        let sel = self.selection();
        if sel.cell_count() < 2 {
            return None;
        }
        let (mut sum, mut nums, mut count) = (0.0, 0u64, 0u64);
        for (_, cell) in self.sheet.store().iter_range(sel) {
            match &cell.value {
                Value::Number(n) => {
                    sum += n;
                    nums += 1;
                    count += 1;
                }
                Value::Empty => {}
                _ => count += 1,
            }
        }
        if count == 0 {
            return None;
        }
        if nums == 0 {
            return Some(format!("Count  {count}"));
        }
        Some(format!(
            "Sum  {}     Average  {}     Count  {count}",
            xl_engine::format_general(sum),
            xl_engine::format_general(sum / nums as f64)
        ))
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into());
        div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(42.))
            .px(px(14.))
            .gap(px(10.))
            .bg(t.surface)
            .border_b_1()
            .border_color(t.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(22.))
                    .rounded(px(6.))
                    .bg(t.accent)
                    .text_color(t.accent_fg)
                    .text_size(px(11.))
                    .font_weight(FontWeight::BOLD)
                    .child("xl"),
            )
            .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(13.5)).child(name))
            .when(self.dirty, |d| d.child(div().size(px(6.)).rounded_full().bg(t.muted)))
            .child(div().flex_1())
            .child(toolbar_hint(&t, "Ctrl+O", "Open"))
            .child(toolbar_hint(&t, "Ctrl+S", "Save"))
            .child(
                div()
                    .id("theme-toggle")
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(26.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(t.border)
                    .text_size(px(12.))
                    .text_color(t.muted)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.header_bg))
                    .child(if t.dark { "Light" } else { "Dark" })
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.toggle_theme();
                        cx.notify();
                    })),
            )
    }

    fn render_formula_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let sel = self.selection();
        let name_box = if sel.start == sel.end {
            self.active.to_string()
        } else {
            format!("{}R × {}C", sel.rows(), sel.cols())
        };
        let (text, caret) = match &self.edit {
            Some(e) => (e.text.clone(), Some(e.cursor)),
            None => (self.sheet.input_text(self.active), None),
        };
        let refs = scan_refs(&text);
        let mut segments: Vec<(String, Hsla)> = Vec::new();
        let mut pos = 0;
        let push = |s: &str, color: Hsla, segs: &mut Vec<(String, Hsla)>| {
            if !s.is_empty() {
                segs.push((s.to_string(), color));
            }
        };
        let mut boundaries: Vec<(usize, usize, Hsla)> =
            refs.iter().map(|r| (r.span.start, r.span.end, t.ref_colors[r.color % t.ref_colors.len()])).collect();
        boundaries.sort_by_key(|b| b.0);
        for (s, e, col) in boundaries {
            push(&text[pos..s], t.text, &mut segments);
            push(&text[s..e], col, &mut segments);
            pos = e;
        }
        push(&text[pos..], t.text, &mut segments);

        // Insert the caret between segments.
        let mut children: Vec<gpui::AnyElement> = Vec::new();
        let mut offset = 0;
        let caret_el = || div().w(px(1.5)).h(px(16.)).bg(t.accent).into_any_element();
        let mut caret_done = caret.is_none();
        for (s, color) in segments {
            let end = offset + s.len();
            if let Some(c) = caret
                && !caret_done
                && c >= offset
                && c < end
            {
                let (a, b) = s.split_at(c - offset);
                if !a.is_empty() {
                    children.push(div().text_color(color).child(a.replace('\u{2028}', "↵")).into_any_element());
                }
                children.push(caret_el());
                children.push(div().text_color(color).child(b.replace('\u{2028}', "↵")).into_any_element());
                caret_done = true;
            } else {
                children.push(div().text_color(color).child(s.replace('\u{2028}', "↵")).into_any_element());
            }
            offset = end;
        }
        if !caret_done {
            children.push(caret_el());
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(36.))
            .bg(t.bg)
            .border_b_1()
            .border_color(t.border)
            .child(
                div()
                    .w(px(96.))
                    .h_full()
                    .flex()
                    .items_center()
                    .px(px(12.))
                    .border_r_1()
                    .border_color(t.border)
                    .text_size(px(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.muted)
                    .child(name_box),
            )
            .child(
                div()
                    .px(px(12.))
                    .text_size(px(12.))
                    .italic()
                    .text_color(if self.edit.is_some() { t.accent } else { t.muted })
                    .child("fx"),
            )
            .child(
                div()
                    .id("formula-bar")
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(13.))
                    .font_family("Inter")
                    .cursor_text()
                    .children(children)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.focus.focus(window, cx);
                            if this.edit.is_none() {
                                this.begin_edit(None, EditMode::Edit);
                            }
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            )
    }

    fn render_status_bar(&self) -> impl IntoElement {
        let t = self.theme;
        let mode = match &self.edit {
            None => "Ready",
            Some(e) if e.can_point() && e.mode == EditMode::Enter => "Point",
            Some(e) if e.mode == EditMode::Enter => "Enter",
            Some(_) => "Edit",
        };
        let recalc = self.last_stats.map(|s| {
            let us = s.elapsed.as_secs_f64() * 1e6;
            let time = if us < 1000.0 { format!("{us:.0} µs") } else { format!("{:.1} ms", us / 1000.0) };
            format!("Recalc {time} · {} cells", s.evaluated)
        });
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.))
            .h(px(28.))
            .px(px(12.))
            .bg(t.surface)
            .border_t_1()
            .border_color(t.border)
            .text_size(px(11.5))
            .text_color(t.muted)
            .child(
                div()
                    .px(px(7.))
                    .py(px(1.))
                    .rounded(px(4.))
                    .bg(t.header_active_bg)
                    .text_color(t.header_active_fg)
                    .font_weight(FontWeight::MEDIUM)
                    .child(mode),
            )
            .when_some(self.message.clone(), |d, (msg, err)| d.child(div().text_color(if err { t.error } else { t.muted }).child(msg)))
            .child(div().flex_1())
            .when_some(self.selection_summary(), |d, s| d.child(div().text_color(t.text).child(s)))
            .when_some(recalc, |d, s| d.child(s))
    }
}

fn toolbar_hint(t: &Theme, keys: &'static str, label: &'static str) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(5.))
        .text_size(px(12.))
        .text_color(t.muted)
        .child(
            div()
                .px(px(5.))
                .py(px(1.))
                .rounded(px(4.))
                .border_1()
                .border_color(t.border)
                .text_size(px(10.5))
                .child(keys),
        )
        .child(label)
}

impl Render for Spreadsheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.title();
        if title != self.last_title {
            window.set_window_title(&title);
            self.last_title = title;
        }
        let t = self.theme;
        let this = cx.entity();
        let grid = canvas(
            {
                let this = this.clone();
                move |bounds, _window, cx| {
                    this.update(cx, |s, _| {
                        s.viewport = bounds;
                        s.scroll_x = s.scroll_x.min(s.max_scroll_x());
                        s.scroll_y = s.scroll_y.min(s.max_scroll_y());
                    });
                }
            },
            move |bounds, (), window, cx| {
                let ops = this.read(cx).build_frame(bounds, window);
                for op in ops {
                    match op {
                        Op::Quad(q, clip) => window.with_content_mask(Some(ContentMask { bounds: clip }), |w| w.paint_quad(q)),
                        Op::Text { line, origin, clip, line_height } => {
                            window.with_content_mask(Some(ContentMask { bounds: clip }), |w| {
                                line.paint(origin, line_height, TextAlign::Left, None, w, cx).ok();
                            })
                        }
                    }
                }
            },
        )
        .size_full();

        div()
            .key_context("Spreadsheet")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .text_color(t.text)
            .font_family("Inter")
            .text_size(px(13.))
            .child(self.render_toolbar(cx))
            .child(self.render_formula_bar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .relative()
                    .overflow_hidden()
                    .child(grid)
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_scroll(cx.listener(Self::on_scroll)),
            )
            .child(self.render_status_bar())
    }
}

// ------------------------------------------------------------------- painting

enum Op {
    Quad(PaintQuad, Bounds<Pixels>),
    Text { line: Box<ShapedLine>, origin: Point<Pixels>, clip: Bounds<Pixels>, line_height: Pixels },
}

fn cell_font() -> Font {
    font("Inter")
}

fn shape(window: &mut Window, text: impl Into<SharedString>, f: Font, color: Hsla, size_px: f32) -> ShapedLine {
    let text: SharedString = text.into();
    let run = TextRun { len: text.len(), font: f, color, background_color: None, underline: None, strikethrough: None };
    window.text_system().shape_line(text, px(size_px), &[run], None)
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

impl Spreadsheet {
    fn build_frame(&self, bounds: Bounds<Pixels>, window: &mut Window) -> Vec<Op> {
        let t = self.theme;
        let mut ops = Vec::with_capacity(2048);
        let ox = f64::from(bounds.origin.x);
        let oy = f64::from(bounds.origin.y);
        let (vw, vh) = (f64::from(bounds.size.width), f64::from(bounds.size.height));
        let gx = ox + HEADER_W; // grid area origin
        let gy = oy + HEADER_H;
        let (gw, gh) = (vw - HEADER_W, vh - HEADER_H);
        let all = bounds;
        let grid_clip = b(gx, gy, gw, gh);
        let col_hdr_clip = b(gx, oy, gw, HEADER_H);
        let row_hdr_clip = b(ox, gy, HEADER_W, gh);
        let store = self.sheet.store();
        let sel = self.selection();
        let editing = self.edit.as_ref();

        ops.push(Op::Quad(fill(all, t.bg), all));

        // Visible window.
        let first_row = self.row_at(self.scroll_y);
        let last_row = self.row_at(self.scroll_y + gh);
        let first_col = self.col_at(self.scroll_x);
        let mut cols: Vec<(u32, f64, f64)> = Vec::new(); // (col, screen x, width)
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

        // Headers background.
        ops.push(Op::Quad(fill(b(ox, oy, vw, HEADER_H), t.header_bg), all));
        ops.push(Op::Quad(fill(b(ox, oy, HEADER_W, vh), t.header_bg), all));
        // Selected header bands.
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

        // Selection fill (active cell stays clear).
        if sel.start != sel.end {
            ops.push(Op::Quad(fill(rect_of(sel), t.selection), grid_clip));
            ops.push(Op::Quad(fill(rect_of(Range::single(self.active)), t.bg), grid_clip));
        }

        // Reference highlights while editing a formula.
        let refs: Vec<RefSpan> = editing.map(|e| scan_refs(&e.text)).unwrap_or_default();
        for r in &refs {
            let color = t.ref_colors[r.color % t.ref_colors.len()];
            let mut soft = color;
            soft.a = 0.08;
            let pointed = editing.and_then(|e| e.point.as_ref()).is_some_and(|p| p.span == r.span);
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
                let color = if matches!(v, Value::Error(_)) { t.error } else { t.text };
                let mut line = shape(window, v.display(), f.clone(), color, CELL_FONT);
                let tw = f64::from(line.width);
                let avail = w - 2.0 * CELL_PAD;
                let (tx, clip_w) = match v {
                    Value::Number(_) => {
                        if tw > avail {
                            line = shape(window, "#".repeat(((avail / 8.0).max(1.0)) as usize), f.clone(), color, CELL_FONT);
                        }
                        (x + w - CELL_PAD - f64::from(line.width), w)
                    }
                    Value::Bool(_) | Value::Error(_) => (x + (w - tw) / 2.0, w),
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
            let color = if active { t.header_active_fg } else { t.header_fg };
            let line = shape(window, col_name(c), hf.clone(), color, HEADER_FONT);
            let tx = x + (w - f64::from(line.width)) / 2.0;
            ops.push(Op::Text { line: Box::new(line), origin: point(px(tx as f32), px(oy as f32)), clip: col_hdr_clip, line_height: px(HEADER_H as f32) });
        }
        for r in first_row..=last_row {
            let active = r >= sel.start.row && r <= sel.end.row;
            let color = if active { t.header_active_fg } else { t.header_fg };
            let line = shape(window, (r + 1).to_string(), hf.clone(), color, HEADER_FONT);
            let tx = ox + (HEADER_W - f64::from(line.width)) / 2.0;
            ops.push(Op::Text { line: Box::new(line), origin: point(px(tx as f32), px(row_y(r) as f32)), clip: row_hdr_clip, line_height: lh });
        }

        // In-cell editor.
        if let Some(e) = editing {
            let display = e.text.replace('\u{2028}', "↵");
            let text: SharedString = display.clone().into();
            let mut runs = Vec::new();
            let mut pos = 0;
            let base = TextRun { len: 0, font: f.clone(), color: t.text, background_color: None, underline: None, strikethrough: None };
            // Map spans from e.text to display (same byte lengths except ↵, which only appears outside refs).
            let shift = |i: usize| e.text[..i].replace('\u{2028}', "↵").len();
            for r in &refs {
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
            let line = shape_runs(window, text, runs, CELL_FONT);
            let w = (f64::from(line.width) + 2.0 * CELL_PAD + 4.0).max(f64::from(ab.size.width) + 1.0);
            let eb = b(f64::from(ab.origin.x) - 1.0, f64::from(ab.origin.y) - 1.0, w, ROW_H + 1.0);
            ops.push(Op::Quad(border_quad(eb, 2.0, t.accent, t.editor_bg, BorderStyle::Solid, 2.0), grid_clip));
            let caret_x = f64::from(ab.origin.x) + CELL_PAD + f64::from(line.x_for_index(shift(e.cursor)));
            let origin = point(px((f64::from(ab.origin.x) + CELL_PAD) as f32), ab.origin.y);
            ops.push(Op::Text { line: Box::new(line), origin, clip: grid_clip, line_height: lh });
            ops.push(Op::Quad(fill(b(caret_x, f64::from(ab.origin.y) + 4.0, 1.5, ROW_H - 8.0), t.accent), grid_clip));
        }

        // Scrollbars (overlay).
        let extent = store.extent().unwrap_or(CellRef::new(0, 0));
        let content_h = ((extent.row.max(self.active.row) + 1) as f64 * ROW_H + gh).max(self.scroll_y + gh);
        let content_w = (self.col_x(extent.col.max(self.active.col) + 1) + gw).max(self.scroll_x + gw);
        if gh > 40.0 {
            let th = (gh * gh / content_h).max(28.0);
            let ty = gy + (self.scroll_y / (content_h - gh).max(1.0)) * (gh - th);
            ops.push(Op::Quad(quad(b(ox + vw - 8.0, ty.min(gy + gh - th), 5.0, th), Corners::all(px(2.5)), t.scrollbar, Edges::default(), t.scrollbar, BorderStyle::Solid), all));
        }
        if gw > 40.0 {
            let tw = (gw * gw / content_w).max(28.0);
            let tx = gx + (self.scroll_x / (content_w - gw).max(1.0)) * (gw - tw);
            ops.push(Op::Quad(quad(b(tx.min(gx + gw - tw), oy + vh - 8.0, tw, 5.0), Corners::all(px(2.5)), t.scrollbar, Edges::default(), t.scrollbar, BorderStyle::Solid), all));
        }
        ops
    }
}

fn prev_boundary(s: &str, i: usize) -> usize {
    s[..i].char_indices().next_back().map_or(0, |(p, _)| p)
}

fn next_boundary(s: &str, i: usize) -> usize {
    s[i..].chars().next().map_or(i, |c| i + c.len_utf8())
}

fn read_csv(path: &std::path::Path) -> Result<Vec<(CellRef, String)>, Box<dyn std::error::Error>> {
    let mut rdr = csv::ReaderBuilder::new().has_headers(false).flexible(true).from_path(path)?;
    let mut items = Vec::new();
    for (r, rec) in rdr.records().enumerate() {
        let rec = rec?;
        if r as u32 >= MAX_ROWS {
            break;
        }
        for (c, field) in rec.iter().enumerate() {
            if !field.is_empty() && (c as u32) < MAX_COLS {
                items.push((CellRef::new(r as u32, c as u32), field.to_string()));
            }
        }
    }
    Ok(items)
}

fn write_csv(sheet: &Sheet, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut w = csv::WriterBuilder::new().flexible(true).from_path(path)?;
    if let Some(end) = sheet.store().extent() {
        for r in 0..=end.row {
            let row: Vec<String> = (0..=end.col).map(|c| sheet.value(CellRef::new(r, c)).display()).collect();
            w.write_record(&row)?;
        }
    }
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    fn setup(cx: &mut TestAppContext) -> (gpui::Entity<Spreadsheet>, &mut VisualTestContext) {
        let (view, cx) = cx.add_window_view(|window, cx| Spreadsheet::new(None, window, cx));
        cx.run_until_parked();
        (view, cx)
    }

    fn val(view: &gpui::Entity<Spreadsheet>, cx: &mut VisualTestContext, a1: &str) -> Value {
        view.read_with(cx, |s, _| s.sheet.value(CellRef::parse(a1).unwrap()).clone())
    }

    fn input(view: &gpui::Entity<Spreadsheet>, cx: &mut VisualTestContext, a1: &str) -> String {
        view.read_with(cx, |s, _| s.sheet.input_text(CellRef::parse(a1).unwrap()))
    }

    fn active(view: &gpui::Entity<Spreadsheet>, cx: &mut VisualTestContext) -> String {
        view.read_with(cx, |s, _| s.active.to_string())
    }

    #[gpui::test]
    fn type_over_cell_recalcs_dependents(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx);
        assert_eq!(active(&view, cx), "B4");
        cx.simulate_input("10");
        cx.simulate_keystrokes("enter");
        assert_eq!(val(&view, cx, "B4"), Value::Number(10.0));
        assert_eq!(val(&view, cx, "D4"), Value::Number(145.0));
        assert_eq!(val(&view, cx, "D7"), Value::Number(180.1));
        assert_eq!(active(&view, cx), "B5");
        // Undo restores the old value; redo re-applies.
        cx.simulate_keystrokes("ctrl-z");
        assert_eq!(val(&view, cx, "D4"), Value::Number(43.5));
        cx.simulate_keystrokes("ctrl-y");
        assert_eq!(val(&view, cx, "D4"), Value::Number(145.0));
    }

    #[gpui::test]
    fn point_mode_builds_references_with_arrows(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx);
        cx.simulate_keystrokes("right right right"); // E4
        cx.simulate_input("=");
        cx.simulate_keystrokes("left left left"); // points at B4
        view.read_with(cx, |s, _| assert_eq!(s.edit.as_ref().unwrap().text, "=B4"));
        cx.simulate_input("*2+");
        cx.simulate_keystrokes("left left"); // points at C4
        view.read_with(cx, |s, _| assert_eq!(s.edit.as_ref().unwrap().text, "=B4*2+C4"));
        cx.simulate_keystrokes("enter");
        assert_eq!(input(&view, cx, "E4"), "=B4*2+C4");
        assert_eq!(val(&view, cx, "E4"), Value::Number(20.5));
    }

    #[gpui::test]
    fn shift_arrow_in_point_mode_makes_a_range(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx);
        cx.simulate_keystrokes("right right down down down down down"); // B4 -> D4 -> D9
        assert_eq!(active(&view, cx), "D9");
        cx.simulate_keystrokes("right"); // E9
        cx.simulate_input("=SUM(");
        cx.simulate_keystrokes("left up up up up up shift-down shift-down");
        cx.simulate_input(")");
        cx.simulate_keystrokes("enter");
        assert_eq!(input(&view, cx, "E9"), "=SUM(D4:D6)");
        assert_eq!(val(&view, cx, "E9"), Value::Number(78.6));
    }

    #[gpui::test]
    fn bad_formula_keeps_editor_open(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx);
        cx.simulate_keystrokes("right right right");
        cx.simulate_input("=1+");
        cx.simulate_keystrokes("enter");
        view.read_with(cx, |s, _| {
            assert!(s.edit.is_some());
            assert!(s.message.as_ref().unwrap().1);
        });
        cx.simulate_input("2");
        cx.simulate_keystrokes("enter");
        assert_eq!(val(&view, cx, "E4"), Value::Number(3.0));
        // Escape cancels an edit without changing the cell.
        cx.simulate_keystrokes("up");
        cx.simulate_input("99");
        cx.simulate_keystrokes("escape");
        assert_eq!(val(&view, cx, "E4"), Value::Number(3.0));
    }

    #[gpui::test]
    fn fill_down_and_copy_paste_shift_references(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx);
        cx.simulate_keystrokes("right right right"); // E4
        cx.simulate_input("=D4*2");
        cx.simulate_keystrokes("enter up shift-down shift-down ctrl-d");
        assert_eq!(input(&view, cx, "E6"), "=D6*2");
        assert_eq!(val(&view, cx, "E6"), Value::Number(43.2));
        // Copy E4, paste into F8: references shift by (+4, +1).
        cx.simulate_keystrokes("escape");
        view.update(cx, |s, _| {
            s.select(CellRef::parse("E4").unwrap(), false);
        });
        cx.simulate_keystrokes("ctrl-c");
        view.update(cx, |s, _| s.select(CellRef::parse("F8").unwrap(), false));
        cx.simulate_keystrokes("ctrl-v");
        assert_eq!(input(&view, cx, "F8"), "=E8*2");
    }

    #[gpui::test]
    fn ctrl_arrows_jump_to_data_edges(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx);
        view.update(cx, |s, _| s.select(CellRef::parse("A3").unwrap(), false));
        cx.simulate_keystrokes("ctrl-down");
        assert_eq!(active(&view, cx), "A6");
        cx.simulate_keystrokes("ctrl-down");
        assert_eq!(active(&view, cx), "A11");
        cx.simulate_keystrokes("ctrl-right");
        assert_eq!(active(&view, cx), "XFD11");
        cx.simulate_keystrokes("ctrl-home");
        assert_eq!(active(&view, cx), "A1");
        cx.simulate_keystrokes("ctrl-end");
        assert_eq!(active(&view, cx), "D11");
    }

    #[gpui::test]
    fn delete_clears_selection(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx);
        cx.simulate_keystrokes("shift-down shift-down delete");
        assert_eq!(val(&view, cx, "B5"), Value::Empty);
        // B4:B6 cleared, so every line total and the subtotal drop to zero.
        assert_eq!(val(&view, cx, "D7"), Value::Number(0.0));
    }
}
