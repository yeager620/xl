//! Spreadsheet view state, editing model and input handling.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{
    Bounds, ClipboardItem, Context, ExternalPaths, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PathPromptOptions, Pixels, Point, ScrollEvent, Subscription, Window, point, px,
};
use xl_engine::{Addr, Border, CellRef, CellSnapshot, HAlign, MAX_COLS, MAX_ROWS, Range, RecalcStats, Rgb, SheetId, Style, Value, Workbook, quote_sheet_name};

use crate::theme::Theme;

pub const ROW_H: f64 = 24.0;
pub const DEF_COL_W: f64 = 100.0;
pub const MIN_COL_W: f64 = 28.0;
pub const HEADER_W: f64 = 52.0;
pub const HEADER_H: f64 = 26.0;
pub const CELL_PAD: f64 = 6.0;
pub const CELL_FONT: f32 = 13.0;
pub const HEADER_FONT: f32 = 11.5;
/// Largest selection for copy/fill.
const MAX_BULK: u64 = 2_000_000;
/// Larger selections get formats only on their non-empty cells.
const MAX_FORMAT_CELLS: u64 = 200_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EditMode {
    /// Started by typing over a cell: arrows commit and move.
    Enter,
    /// Started with F2 / double-click: arrows move the caret.
    Edit,
}

#[derive(Clone, Debug)]
pub struct PointRef {
    pub anchor: CellRef,
    pub cell: CellRef,
    pub sheet: SheetId,
    pub span: std::ops::Range<usize>,
}

#[derive(Clone, Debug)]
pub struct Edit {
    /// The cell being edited (may be on another sheet than the one shown,
    /// while pointing at references there).
    pub at: Addr,
    pub text: String,
    pub cursor: usize,
    pub mode: EditMode,
    pub point: Option<PointRef>,
    /// Input-method composition in progress (byte range in `text`).
    pub marked: Option<std::ops::Range<usize>>,
}

impl Edit {
    /// Whether a reference can be inserted at the caret (Excel "Point" mode).
    pub fn can_point(&self) -> bool {
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
pub enum Drag {
    Select,
    Point,
    ResizeCol { col: u32, start_x: f64, start_w: f64 },
    /// Dragging a scrollbar thumb; `grab` is the pointer offset within it.
    Scroll { axis: Axis, grab: f64 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    Vertical,
    Horizontal,
}

/// A scrollbar thumb in window coordinates.
#[derive(Clone, Copy, Debug)]
pub struct Thumb {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Width of the strip along the grid edge that grabs the scrollbar.
const SCROLLBAR_HIT: f64 = 12.0;

pub enum Hit {
    Corner,
    ColHeader { col: u32, border: Option<u32> },
    RowHeader(u32),
    Cell(CellRef),
}

struct Clip {
    sheet: SheetId,
    range: Range,
    text: String,
}

/// Per-sheet selection and scroll position.
#[derive(Clone, Copy, Default)]
struct View {
    active: CellRef,
    anchor: CellRef,
    scroll_x: f64,
    scroll_y: f64,
}

type UndoEntry = Vec<(Addr, CellSnapshot)>;

/// Which toolbar popover is open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Menu {
    TextColor,
    Fill,
    Borders,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BorderPreset {
    All,
    Outside,
    Bottom,
    ThickBottom,
    Top,
    None,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Emphasis {
    Bold,
    Italic,
    Underline,
    Strike,
}

pub struct Spreadsheet {
    pub(crate) wb: Workbook,
    pub(crate) cur: SheetId,
    views: HashMap<SheetId, View>,
    pub(crate) focus: FocusHandle,
    pub(crate) active: CellRef,
    pub(crate) anchor: CellRef,
    pub(crate) scroll_x: f64,
    pub(crate) scroll_y: f64,
    pub(crate) viewport: Bounds<Pixels>,
    pub(crate) edit: Option<Edit>,
    pub(crate) drag: Option<Drag>,
    /// Scrollbar under the pointer (drawn wider).
    pub(crate) scroll_hover: Option<Axis>,
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    clip: Option<Clip>,
    pub(crate) last_stats: Option<RecalcStats>,
    pub(crate) message: Option<(String, bool)>,
    pub(crate) path: Option<PathBuf>,
    pub(crate) dirty: bool,
    pub(crate) theme: Theme,
    theme_override: Option<bool>,
    pub(crate) last_title: String,
    /// Sheet tab being renamed, with the text typed so far.
    pub(crate) renaming: Option<(SheetId, String)>,
    /// Sheet whose delete button was clicked once (click again to confirm).
    pub(crate) confirm_delete: Option<SheetId>,
    pub(crate) menu: Option<Menu>,
    /// File being opened in the background.
    pub(crate) loading: Option<String>,
    pub(crate) palette: Option<crate::palette::Palette>,
    /// Mouse went down on empty title-bar space; a following move drags the window.
    pub(crate) should_move: bool,
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
        let wb = Workbook::new();
        let cur = wb.sheet_ids()[0];
        let mut this = Self {
            wb,
            cur,
            views: HashMap::new(),
            focus,
            active: CellRef::new(0, 0),
            anchor: CellRef::new(0, 0),
            scroll_x: 0.0,
            scroll_y: 0.0,
            viewport: Bounds::default(),
            edit: None,
            drag: None,
            scroll_hover: None,
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
            renaming: None,
            confirm_delete: None,
            menu: None,
            loading: None,
            palette: None,
            should_move: false,
            _appearance: appearance,
        };
        match path {
            Some(p) => this.load(p, cx),
            None => this.load_welcome(),
        }
        this
    }

    pub(crate) fn at(&self, c: CellRef) -> Addr {
        Addr::new(self.cur, c)
    }

    // ---------------------------------------------------------------- geometry

    pub(crate) fn col_w(&self, c: u32) -> f64 {
        self.wb.col_width(self.cur, c).unwrap_or(DEF_COL_W)
    }

    /// Content-space x of the left edge of column `c`.
    pub(crate) fn col_x(&self, c: u32) -> f64 {
        let mut x = c as f64 * DEF_COL_W;
        for &w in self.wb.col_widths(self.cur).range(..c).map(|(_, w)| w) {
            x += w - DEF_COL_W;
        }
        x
    }

    /// Column containing content-space x.
    pub(crate) fn col_at(&self, x: f64) -> u32 {
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

    pub(crate) fn row_at(&self, y: f64) -> u32 {
        ((y / ROW_H).floor().max(0.0) as u32).min(MAX_ROWS - 1)
    }

    pub(crate) fn grid_w(&self) -> f64 {
        (f64::from(self.viewport.size.width) - HEADER_W).max(0.0)
    }

    pub(crate) fn grid_h(&self) -> f64 {
        (f64::from(self.viewport.size.height) - HEADER_H).max(0.0)
    }

    fn visible_rows(&self) -> u32 {
        ((self.grid_h() / ROW_H).floor() as u32).max(1)
    }

    pub(crate) fn max_scroll_x(&self) -> f64 {
        (self.col_x(MAX_COLS) - self.grid_w()).max(0.0)
    }

    pub(crate) fn max_scroll_y(&self) -> f64 {
        (MAX_ROWS as f64 * ROW_H - self.grid_h()).max(0.0)
    }

    /// Scrollable content size: the used area plus one screen, at least as
    /// far as the current scroll position.
    fn content_size(&self) -> (f64, f64) {
        let extent = self.wb.store(self.cur).extent().unwrap_or_default();
        let (gw, gh) = (self.grid_w(), self.grid_h());
        let h = ((extent.row.max(self.active.row) + 1) as f64 * ROW_H + gh).max(self.scroll_y + gh);
        let w = (self.col_x(extent.col.max(self.active.col) + 1) + gw).max(self.scroll_x + gw);
        (w, h)
    }

    pub(crate) fn thumb(&self, axis: Axis) -> Option<Thumb> {
        let ox = f64::from(self.viewport.origin.x);
        let oy = f64::from(self.viewport.origin.y);
        let (vw, vh) = (f64::from(self.viewport.size.width), f64::from(self.viewport.size.height));
        let (gx, gy, gw, gh) = (ox + HEADER_W, oy + HEADER_H, self.grid_w(), self.grid_h());
        let (cw, ch) = self.content_size();
        let wide = self.scroll_hover == Some(axis) || matches!(self.drag, Some(Drag::Scroll { axis: a, .. }) if a == axis);
        let thickness = if wide { 9.0 } else { 5.0 };
        match axis {
            Axis::Vertical if gh > 40.0 => {
                let th = (gh * gh / ch).max(28.0);
                let ty = gy + (self.scroll_y / (ch - gh).max(1.0)) * (gh - th);
                Some(Thumb { x: ox + vw - thickness - 3.0, y: ty.min(gy + gh - th), w: thickness, h: th })
            }
            Axis::Horizontal if gw > 40.0 => {
                let tw = (gw * gw / cw).max(28.0);
                let tx = gx + (self.scroll_x / (cw - gw).max(1.0)) * (gw - tw);
                Some(Thumb { x: tx.min(gx + gw - tw), y: oy + vh - thickness - 3.0, w: tw, h: thickness })
            }
            _ => None,
        }
    }

    /// Which scrollbar strip (if any) is under a window position.
    fn scrollbar_at(&self, pos: Point<Pixels>) -> Option<Axis> {
        let (x, y) = (f64::from(pos.x - self.viewport.origin.x), f64::from(pos.y - self.viewport.origin.y));
        let (vw, vh) = (f64::from(self.viewport.size.width), f64::from(self.viewport.size.height));
        if x >= vw - SCROLLBAR_HIT && x <= vw && y >= HEADER_H && y <= vh - SCROLLBAR_HIT {
            Some(Axis::Vertical)
        } else if y >= vh - SCROLLBAR_HIT && y <= vh && x >= HEADER_W && x <= vw - SCROLLBAR_HIT {
            Some(Axis::Horizontal)
        } else {
            None
        }
    }

    /// Scroll so the thumb's leading edge sits at window coordinate `edge`.
    fn scroll_thumb_to(&mut self, axis: Axis, edge: f64) {
        let Some(t) = self.thumb(axis) else { return };
        let (cw, ch) = self.content_size();
        match axis {
            Axis::Vertical => {
                let gy = f64::from(self.viewport.origin.y) + HEADER_H;
                let track = (self.grid_h() - t.h).max(1.0);
                self.scroll_y = (((edge - gy) / track).clamp(0.0, 1.0) * (ch - self.grid_h())).clamp(0.0, self.max_scroll_y());
            }
            Axis::Horizontal => {
                let gx = f64::from(self.viewport.origin.x) + HEADER_W;
                let track = (self.grid_w() - t.w).max(1.0);
                self.scroll_x = (((edge - gx) / track).clamp(0.0, 1.0) * (cw - self.grid_w())).clamp(0.0, self.max_scroll_x());
            }
        }
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
                let (left, right) = (self.col_x(col), self.col_x(col) + self.col_w(col));
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

    pub(crate) fn select(&mut self, c: CellRef, extend: bool) {
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
        let s = self.wb.store(self.cur);
        let filled = |c: CellRef| !s.value(c).is_empty();
        let step = |c: CellRef| c.offset(dr, dc);
        let mut cur = self.active;
        let Some(next) = step(cur) else { return };
        if filled(cur) && filled(next) {
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

    // ----------------------------------------------------------------- sheets

    /// Show another sheet, remembering each sheet's selection and scroll.
    pub(crate) fn switch_sheet(&mut self, id: SheetId) {
        if id == self.cur || !self.wb.sheet_ids().contains(&id) {
            return;
        }
        // Leaving the sheet commits an edit, unless it's pointing at references.
        if self.edit.as_ref().is_some_and(|e| !e.can_point()) && !self.commit_edit() {
            return;
        }
        self.views.insert(self.cur, View { active: self.active, anchor: self.anchor, scroll_x: self.scroll_x, scroll_y: self.scroll_y });
        let v = self.views.get(&id).copied().unwrap_or_default();
        self.cur = id;
        self.active = v.active;
        self.anchor = v.anchor;
        self.scroll_x = v.scroll_x;
        self.scroll_y = v.scroll_y;
        self.confirm_delete = None;
    }

    pub(crate) fn cycle_sheet(&mut self, delta: i64) {
        let ids = self.wb.sheet_ids();
        let i = ids.iter().position(|&s| s == self.cur).unwrap_or(0) as i64;
        let next = ids[(i + delta).rem_euclid(ids.len() as i64) as usize];
        self.switch_sheet(next);
    }

    pub(crate) fn add_sheet(&mut self) {
        if let Ok(id) = self.wb.add_sheet(None) {
            self.switch_sheet(id);
            self.dirty = true;
        }
    }

    pub(crate) fn start_rename(&mut self, id: SheetId) {
        self.renaming = Some((id, self.wb.sheet_name(id).to_string()));
        self.confirm_delete = None;
    }

    pub(crate) fn finish_rename(&mut self, commit: bool) {
        let Some((id, name)) = self.renaming.take() else { return };
        if !commit || name == self.wb.sheet_name(id) {
            return;
        }
        match self.wb.rename_sheet(id, &name) {
            Ok(()) => self.dirty = true,
            Err(e) => {
                self.message = Some((e, true));
                self.renaming = Some((id, name));
            }
        }
    }

    pub(crate) fn delete_sheet(&mut self, id: SheetId) {
        let ids = self.wb.sheet_ids().to_vec();
        let pos = ids.iter().position(|&s| s == id).unwrap_or(0);
        match self.wb.delete_sheet(id) {
            Ok(stats) => {
                self.last_stats = Some(stats);
                self.views.remove(&id);
                if self.cur == id {
                    let next = self.wb.sheet_ids()[pos.min(self.wb.sheet_ids().len() - 1)];
                    self.cur = next; // the old sheet is gone; nothing to save
                    let v = self.views.get(&next).copied().unwrap_or_default();
                    (self.active, self.anchor, self.scroll_x, self.scroll_y) = (v.active, v.anchor, v.scroll_x, v.scroll_y);
                }
                self.undo.retain(|e| e.iter().all(|(a, _)| a.sheet != id));
                self.redo.retain(|e| e.iter().all(|(a, _)| a.sheet != id));
                self.message = Some((format!("Deleted sheet ({} formulas recalculated)", stats.evaluated), false));
                self.dirty = true;
            }
            Err(e) => self.message = Some((e, true)),
        }
        self.confirm_delete = None;
    }

    // ---------------------------------------------------------------- editing

    pub(crate) fn begin_edit(&mut self, initial: Option<String>, mode: EditMode) {
        let at = self.at(self.active);
        let text = initial.unwrap_or_else(|| self.wb.input_text(at));
        let cursor = text.len();
        self.edit = Some(Edit { at, text, cursor, mode, point: None, marked: None });
        self.message = None;
    }

    /// Returns false if the formula failed to parse (edit stays open).
    pub(crate) fn commit_edit(&mut self) -> bool {
        let Some(edit) = self.edit.take() else { return true };
        let before = self.wb.snapshot(edit.at);
        let result = if edit.text == before.input { Ok(None) } else { self.wb.set_input(edit.at, &edit.text).map(Some) };
        match result {
            Ok(stats) => {
                if let Some(stats) = stats {
                    self.push_undo(vec![(edit.at, before)]);
                    self.last_stats = Some(stats);
                    self.dirty = true;
                }
                self.message = None;
                if self.cur != edit.at.sheet {
                    self.switch_sheet(edit.at.sheet);
                }
                true
            }
            Err(e) => {
                self.message = Some((format!("Formula error: {e}"), true));
                self.edit = Some(edit);
                false
            }
        }
    }

    pub(crate) fn cancel_edit(&mut self) {
        if let Some(edit) = self.edit.take()
            && self.cur != edit.at.sheet
        {
            self.switch_sheet(edit.at.sheet);
        }
        self.message = None;
    }

    fn push_undo(&mut self, entry: UndoEntry) {
        self.undo.push(entry);
        self.redo.clear();
        if self.undo.len() > 500 {
            self.undo.remove(0);
        }
    }

    fn snapshots(&self, addrs: impl IntoIterator<Item = Addr>) -> UndoEntry {
        addrs.into_iter().map(|a| (a, self.wb.snapshot(a))).collect()
    }

    /// Apply raw inputs (with auto-formatting) as one undoable step.
    fn apply_inputs(&mut self, changes: Vec<(Addr, String)>) {
        if changes.is_empty() {
            return;
        }
        let undo = self.snapshots(changes.iter().map(|(a, _)| *a));
        self.last_stats = Some(self.wb.set_inputs(changes));
        self.push_undo(undo);
        self.dirty = true;
    }

    /// Apply exact snapshots (inputs + formats) as one undoable step.
    fn apply_snapshots(&mut self, items: UndoEntry) {
        if items.is_empty() {
            return;
        }
        let undo = self.snapshots(items.iter().map(|(a, _)| *a));
        self.last_stats = Some(self.wb.restore(items));
        self.push_undo(undo);
        self.dirty = true;
    }

    pub(crate) fn undo_redo(&mut self, is_undo: bool) {
        let entry = if is_undo { self.undo.pop() } else { self.redo.pop() };
        let Some(entry) = entry else { return };
        let inverse = self.snapshots(entry.iter().map(|(a, _)| *a));
        if let Some(&(first, _)) = entry.first() {
            self.switch_sheet(first.sheet);
            let cells: Vec<CellRef> = entry.iter().filter(|(a, _)| a.sheet == first.sheet).map(|(a, _)| a.cell).collect();
            let r0 = cells.iter().map(|c| c.row).min().unwrap();
            let r1 = cells.iter().map(|c| c.row).max().unwrap();
            let c0 = cells.iter().map(|c| c.col).min().unwrap();
            let c1 = cells.iter().map(|c| c.col).max().unwrap();
            self.anchor = CellRef::new(r0, c0);
            self.active = CellRef::new(r1, c1);
            self.ensure_visible(self.active);
        }
        self.last_stats = Some(self.wb.restore(entry));
        if is_undo { self.redo.push(inverse) } else { self.undo.push(inverse) }
        self.dirty = true;
    }

    /// Delete clears contents but keeps formats, like Excel.
    pub(crate) fn clear_selection(&mut self) {
        let sel = self.selection();
        let cells: Vec<_> = self.wb.store(self.cur).iter_range(sel).map(|(c, _)| (self.at(c), String::new())).collect();
        self.apply_inputs(cells);
    }

    fn guard_bulk(&mut self, r: Range) -> bool {
        if r.cell_count() > MAX_BULK {
            self.message = Some((format!("Selection too large ({} cells)", r.cell_count()), true));
            return false;
        }
        true
    }

    pub(crate) fn copy(&mut self, cx: &mut Context<Self>) {
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
                text.push_str(&self.wb.display(self.at(CellRef::new(r, c))).text);
            }
            text.push('\n');
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        self.clip = Some(Clip { sheet: self.cur, range: sel, text });
        self.message = Some((format!("Copied {sel}"), false));
    }

    pub(crate) fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) else { return };
        let origin = self.selection().start;
        let mut end = origin;
        if let Some(clip) = self.clip.as_ref().filter(|c| c.text == text && self.wb.sheet_ids().contains(&c.sheet)) {
            // Internal paste: formulas shift their relative references, formats come along.
            let (dr, dc) = (origin.row as i64 - clip.range.start.row as i64, origin.col as i64 - clip.range.start.col as i64);
            let src_sheet = clip.sheet;
            let mut items = Vec::new();
            for src in clip.range.iter() {
                if let Some(dst) = src.offset(dr, dc) {
                    items.push((self.at(dst), self.wb.snapshot_shifted(Addr::new(src_sheet, src), dr, dc)));
                    end = dst;
                }
            }
            self.apply_snapshots(items);
        } else {
            let text = text.strip_suffix('\n').unwrap_or(&text);
            let mut changes = Vec::new();
            for (i, line) in text.split('\n').enumerate() {
                for (j, field) in line.trim_end_matches('\r').split('\t').enumerate() {
                    if let Some(dst) = origin.offset(i as i64, j as i64) {
                        changes.push((self.at(dst), field.to_string()));
                        end = CellRef::new(end.row.max(dst.row), end.col.max(dst.col));
                    }
                }
            }
            self.apply_inputs(changes);
        }
        self.anchor = origin;
        self.active = end;
    }

    pub(crate) fn cut(&mut self, cx: &mut Context<Self>) {
        self.copy(cx);
        self.clear_selection();
    }

    /// Ctrl+D / Ctrl+R: copy the first row/column of the selection across it.
    pub(crate) fn fill(&mut self, down: bool) {
        let sel = self.selection();
        if !self.guard_bulk(sel) {
            return;
        }
        let mut items = Vec::new();
        if down {
            let (src_row, first) = match sel.rows() {
                1 if sel.start.row == 0 => return,
                1 => (sel.start.row - 1, sel.start.row),
                _ => (sel.start.row, sel.start.row + 1),
            };
            for c in sel.start.col..=sel.end.col {
                let src = self.at(CellRef::new(src_row, c));
                for r in first..=sel.end.row {
                    items.push((self.at(CellRef::new(r, c)), self.wb.snapshot_shifted(src, (r - src_row) as i64, 0)));
                }
            }
        } else {
            let (src_col, first) = match sel.cols() {
                1 if sel.start.col == 0 => return,
                1 => (sel.start.col - 1, sel.start.col),
                _ => (sel.start.col, sel.start.col + 1),
            };
            for r in sel.start.row..=sel.end.row {
                let src = self.at(CellRef::new(r, src_col));
                for c in first..=sel.end.col {
                    items.push((self.at(CellRef::new(r, c)), self.wb.snapshot_shifted(src, 0, (c - src_col) as i64)));
                }
            }
        }
        self.apply_snapshots(items);
    }

    /// Insert or replace the pointed reference in the formula being edited.
    fn point_to(&mut self, cell: CellRef, extend: bool) {
        let cur = self.cur;
        let prefix = {
            let Some(edit) = self.edit.as_ref() else { return };
            if cur == edit.at.sheet { String::new() } else { format!("{}!", quote_sheet_name(self.wb.sheet_name(cur))) }
        };
        let edit = self.edit.as_mut().unwrap();
        let (anchor, span) = match &edit.point {
            Some(p) if p.sheet == cur => (if extend { p.anchor } else { cell }, p.span.clone()),
            Some(p) => (cell, p.span.clone()),
            None => (cell, edit.cursor..edit.cursor),
        };
        let text = format!("{prefix}{}", Range::new(anchor, cell));
        edit.text.replace_range(span.clone(), &text);
        let span = span.start..span.start + text.len();
        edit.cursor = span.end;
        edit.point = Some(PointRef { anchor, cell, sheet: cur, span });
        self.ensure_visible(cell);
    }

    // ---------------------------------------------------------------- formats

    /// Cells of the selection that a format change applies to.
    fn format_targets(&mut self) -> Vec<Addr> {
        let sel = self.selection();
        if sel.cell_count() <= MAX_FORMAT_CELLS {
            sel.iter().map(|c| self.at(c)).collect()
        } else {
            self.message = Some(("Large selection: formatted the non-empty cells only".into(), false));
            self.wb.store(self.cur).iter_range(sel).map(|(c, _)| self.at(c)).collect()
        }
    }

    pub(crate) fn apply_format(&mut self, code: Option<&str>) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let targets = self.format_targets();
        let undo = self.snapshots(targets.iter().copied());
        for a in targets {
            self.wb.set_format(a, code);
        }
        self.push_undo(undo);
        self.dirty = true;
    }

    /// Change the style of every targeted cell as one undoable step.
    /// `f` gets the cell's position within the selection.
    fn modify_styles(&mut self, f: impl Fn(&mut Style, CellRef, Range)) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let sel = self.selection();
        let targets = self.format_targets();
        let undo = self.snapshots(targets.iter().copied());
        for a in targets {
            let mut st = self.wb.style(a).clone();
            f(&mut st, a.cell, sel);
            self.wb.set_style(a, &st);
        }
        self.push_undo(undo);
        self.dirty = true;
        self.menu = None;
    }

    /// Toggle bold/italic/underline/strikethrough, keyed off the active cell.
    pub(crate) fn toggle_emphasis(&mut self, e: Emphasis) {
        let cur = self.wb.style(self.at(self.active));
        let on = !match e {
            Emphasis::Bold => cur.bold,
            Emphasis::Italic => cur.italic,
            Emphasis::Underline => cur.underline,
            Emphasis::Strike => cur.strike,
        };
        self.modify_styles(|st, _, _| match e {
            Emphasis::Bold => st.bold = on,
            Emphasis::Italic => st.italic = on,
            Emphasis::Underline => st.underline = on,
            Emphasis::Strike => st.strike = on,
        });
    }

    pub(crate) fn set_text_color(&mut self, c: Option<Rgb>) {
        self.modify_styles(|st, _, _| st.color = c);
    }

    pub(crate) fn set_fill(&mut self, c: Option<Rgb>) {
        self.modify_styles(|st, _, _| st.fill = c);
    }

    /// Clicking the active alignment again returns to General.
    pub(crate) fn set_align(&mut self, a: HAlign) {
        let target = if self.wb.style(self.at(self.active)).align == a { HAlign::General } else { a };
        self.modify_styles(|st, _, _| st.align = target);
    }

    pub(crate) fn apply_borders(&mut self, preset: BorderPreset) {
        let thin = Border::THIN;
        let thick = Border { style: xl_engine::LineStyle::Thick, color: None };
        self.modify_styles(|st, c, sel| match preset {
            BorderPreset::All => (st.top, st.bottom, st.left, st.right) = (thin, thin, thin, thin),
            BorderPreset::Outside => {
                if c.row == sel.start.row {
                    st.top = thin;
                }
                if c.row == sel.end.row {
                    st.bottom = thin;
                }
                if c.col == sel.start.col {
                    st.left = thin;
                }
                if c.col == sel.end.col {
                    st.right = thin;
                }
            }
            BorderPreset::Bottom if c.row == sel.end.row => st.bottom = thin,
            BorderPreset::ThickBottom if c.row == sel.end.row => st.bottom = thick,
            BorderPreset::Top if c.row == sel.start.row => st.top = thin,
            BorderPreset::None => (st.top, st.bottom, st.left, st.right) = (Border::default(), Border::default(), Border::default(), Border::default()),
            _ => {}
        });
    }

    /// Remove number formats and styles from the selection, keeping contents.
    pub(crate) fn clear_formatting(&mut self) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let targets = self.format_targets();
        let undo = self.snapshots(targets.iter().copied());
        for a in targets {
            self.wb.set_style(a, &Style::default());
            self.wb.set_format(a, None);
        }
        self.push_undo(undo);
        self.dirty = true;
    }

    /// Add or remove decimal places, based on the active cell's format.
    pub(crate) fn bump_decimals(&mut self, delta: i32) {
        let a = self.at(self.active);
        let code = match self.wb.format(a) {
            Some(f) if !f.is_general() => f.with_decimals_delta(delta),
            _ => {
                // General: start from the decimals currently shown.
                let shown = self.wb.display(a).text;
                let decimals = shown.split_once('.').map_or(0, |(_, d)| d.chars().take_while(char::is_ascii_digit).count()) as i32;
                let n = (decimals + delta).clamp(0, 15) as usize;
                Some(if n == 0 { "0".to_string() } else { format!("0.{}", "0".repeat(n)) })
            }
        };
        match code {
            Some(c) => self.apply_format(Some(&c)),
            None => self.message = Some(("This format has no decimal places to change".into(), false)),
        }
    }

    // ------------------------------------------------------------------ files

    fn load_welcome(&mut self) {
        let s = self.cur;
        self.wb.rename_sheet(s, "Welcome").ok();
        let rows: &[&[&str]] = &[
            &["Welcome to xl", "", "", ""],
            &["", "", "", ""],
            &["Item", "Qty", "Price", "Total"],
            &["Coffee beans", "3", "$14.50", "=B4*C4"],
            &["Oat milk", "6", "$2.25", "=B5*C5"],
            &["Croissants", "12", "$1.80", "=B6*C6"],
            &["", "", "Subtotal", "=SUM(D4:D6)"],
            &["", "", "Tax rate", "8.25%"],
            &["", "", "Tax", "=ROUND(D7*D8,2)"],
            &["", "", "Total", "=D7+D9"],
            &["", "", "", ""],
            &["Ordered", "=TODAY()", "", ""],
            &["", "", "", ""],
            &["Try: edit a Qty — totals update instantly. Type = then use the arrow keys to point at cells.", "", "", ""],
        ];
        let mut items = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            for (c, v) in row.iter().enumerate() {
                if !v.is_empty() {
                    items.push((Addr::new(s, CellRef::new(r as u32, c as u32)), v.to_string()));
                }
            }
        }
        self.last_stats = Some(self.wb.set_inputs(items));
        for r in 4..=7 {
            self.wb.set_format(Addr::new(s, CellRef::new(r - 1, 3)), Some("$#,##0.00"));
        }
        for r in [9, 10] {
            self.wb.set_format(Addr::new(s, CellRef::new(r - 1, 3)), Some("$#,##0.00"));
        }
        self.wb.set_col_width(s, 0, Some(140.0));
        let at = |a1: &str| Addr::new(s, CellRef::parse(a1).unwrap());
        self.wb.set_style(at("A1"), &Style { bold: true, ..Style::default() });
        let header = Style { bold: true, fill: Some(0xDDEBF7), bottom: Border::THIN, ..Style::default() };
        for c in ["A3", "B3", "C3", "D3"] {
            let mut st = header.clone();
            st.align = if c == "A3" { HAlign::General } else { HAlign::Right };
            self.wb.set_style(at(c), &st);
        }
        let total = Style { bold: true, top: Border::THIN, bottom: Border { style: xl_engine::LineStyle::Double, color: None }, ..Style::default() };
        self.wb.set_style(at("C10"), &total);
        self.wb.set_style(at("D10"), &total);
        self.wb.set_style(at("A14"), &Style { italic: true, color: Some(0x7F7F7F), ..Style::default() });
        self.active = CellRef::new(3, 1);
        self.anchor = self.active;
        self.dirty = false;
    }

    /// Open a file on a background thread; the window stays responsive.
    pub(crate) fn load(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        self.loading = Some(name.clone());
        self.message = Some((format!("Opening {name}…"), false));
        let task = cx.background_executor().spawn({
            let path = path.clone();
            async move { xl_io::open(&path) }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.finish_load(path, result);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn finish_load(&mut self, path: PathBuf, result: Result<xl_io::Loaded, String>) {
        self.loading = None;
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        match result {
            Ok(loaded) => {
                self.wb = loaded.workbook;
                self.cur = self.wb.sheet_ids()[0];
                self.views.clear();
                self.last_stats = Some(loaded.recalc);
                self.undo.clear();
                self.redo.clear();
                self.clip = None;
                self.active = CellRef::new(0, 0);
                self.anchor = self.active;
                self.scroll_x = 0.0;
                self.scroll_y = 0.0;
                self.edit = None;
                self.renaming = None;
                self.menu = None;
                self.dirty = false;
                self.message = Some(match loaded.warnings.first() {
                    Some(w) => (format!("Opened {name} with {} warning(s): {w}", loaded.warnings.len()), true),
                    None => (format!("Opened {name} ({} cells)", loaded.cells), false),
                });
                self.path = Some(path);
            }
            Err(e) => self.message = Some((format!("Could not open {name}: {e}"), true)),
        }
    }

    pub(crate) fn save_to(&mut self, path: PathBuf) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        match xl_io::save(&self.wb, &path, self.cur) {
            Ok(()) => {
                let csv = xl_io::FileKind::of(&path) == Some(xl_io::FileKind::Csv);
                self.dirty = false;
                self.message = Some(if csv && self.wb.sheet_ids().len() > 1 {
                    (format!("Saved {} (CSV holds only the current sheet)", path.display()), false)
                } else {
                    (format!("Saved {}", path.display()), false)
                });
                self.path = Some(path);
            }
            Err(e) => self.message = Some((format!("Save failed: {e}"), true)),
        }
    }

    pub(crate) fn save(&mut self, save_as: bool, cx: &mut Context<Self>) {
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
        let suggested = self.path.as_ref().and_then(|p| p.file_stem()).map_or("Untitled".to_string(), |s| s.to_string_lossy().into_owned()) + ".xlsx";
        let rx = cx.prompt_for_new_path(&dir, Some(&suggested));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut path))) = rx.await {
                if xl_io::FileKind::of(&path).is_none() {
                    path.set_extension("xlsx");
                }
                this.update(cx, |this, cx| {
                    this.save_to(path);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    pub(crate) fn open(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Open".into()) });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| {
                    this.load(path, cx);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    pub(crate) fn on_drop_files(&mut self, paths: &ExternalPaths, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = paths.paths().iter().find(|p| xl_io::FileKind::of(p).is_some()) {
            self.load(p.clone(), cx);
            cx.notify();
        }
    }

    // ------------------------------------------------------------------ input

    pub(crate) fn on_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let m = ks.modifiers;
        let key = ks.key.as_str();
        let ctrl = m.control || m.platform;
        let palette_key = ctrl && (key == "k" || (m.shift && key == "p"));
        let handled = if palette_key {
            if self.palette.take().is_none() {
                self.open_palette();
            }
            true
        } else if self.palette.is_some() {
            self.key_palette(key, window, cx)
        } else if self.renaming.is_some() {
            self.key_renaming(key)
        } else if self.edit.is_some() {
            self.key_editing(key, ctrl, m.shift, m.alt)
        } else {
            self.key_navigating(key, ctrl, m.shift, cx)
        };
        if handled {
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn key_renaming(&mut self, key: &str) -> bool {
        let Some((_, name)) = self.renaming.as_mut() else { return false };
        match key {
            "enter" => self.finish_rename(true),
            "escape" => {
                self.renaming = None;
                self.message = None;
            }
            "backspace" => {
                name.pop();
            }
            // Characters arrive through the input handler (see ime.rs).
            _ => return false,
        }
        true
    }

    fn key_editing(&mut self, key: &str, ctrl: bool, shift: bool, alt: bool) -> bool {
        if self.edit.as_ref().is_some_and(|e| e.marked.is_some()) {
            return false;
        }
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
                let base = match &edit.point {
                    Some(p) if p.sheet == self.cur => p.cell,
                    _ if self.cur == edit.at.sheet => edit.at.cell,
                    _ => self.active,
                };
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
            "pageup" | "pagedown" if ctrl => self.cycle_sheet(if key == "pageup" { -1 } else { 1 }),
            "a" if ctrl => {
                let e = self.edit.as_mut().unwrap();
                e.cursor = e.text.len();
            }
            // Characters arrive through the input handler (see ime.rs).
            _ => return false,
        }
        true
    }

    fn insert_text(&mut self, s: &str) {
        let e = self.edit.as_mut().unwrap();
        e.point = None;
        e.text.insert_str(e.cursor, s);
        e.cursor += s.len();
    }

    fn key_navigating(&mut self, key: &str, ctrl: bool, shift: bool, cx: &mut Context<Self>) -> bool {
        let page = self.visible_rows() as i64;
        // Excel's number-format shortcuts (Ctrl+Shift+1/3/4/5/`).
        if ctrl && shift {
            let fmt = match key {
                "1" | "!" => Some(Some("#,##0.00")),
                "3" | "#" => Some(Some("d-mmm-yy")),
                "4" | "$" => Some(Some("$#,##0.00")),
                "5" | "%" => Some(Some("0%")),
                "`" | "~" => Some(None),
                _ => None,
            };
            if let Some(code) = fmt {
                self.apply_format(code);
                return true;
            }
        }
        match (key, ctrl) {
            ("up", false) => self.move_by(-1, 0, shift),
            ("down", false) => self.move_by(1, 0, shift),
            ("left", false) => self.move_by(0, -1, shift),
            ("right", false) => self.move_by(0, 1, shift),
            ("up", true) => self.jump(-1, 0, shift),
            ("down", true) => self.jump(1, 0, shift),
            ("left", true) => self.jump(0, -1, shift),
            ("right", true) => self.jump(0, 1, shift),
            ("pageup", true) => self.cycle_sheet(-1),
            ("pagedown", true) => self.cycle_sheet(1),
            ("pageup", false) => {
                self.scroll_y = (self.scroll_y - page as f64 * ROW_H).max(0.0);
                self.move_by(-page, 0, shift);
            }
            ("pagedown", false) => {
                self.scroll_y = (self.scroll_y + page as f64 * ROW_H).min(self.max_scroll_y());
                self.move_by(page, 0, shift);
            }
            ("home", false) => self.select(CellRef::new(self.active.row, 0), shift),
            ("home", true) => self.select(CellRef::new(0, 0), shift),
            ("end", true) => {
                let end = self.wb.store(self.cur).extent().unwrap_or(CellRef::new(0, 0));
                self.select(end, shift);
            }
            ("enter", _) => self.move_by(if shift { -1 } else { 1 }, 0, false),
            ("tab", _) => self.move_by(0, if shift { -1 } else { 1 }, false),
            ("f2", _) => self.begin_edit(None, EditMode::Edit),
            ("delete" | "backspace", false) => self.clear_selection(),
            ("escape", _) => {
                self.anchor = self.active;
                self.message = None;
                self.confirm_delete = None;
                self.menu = None;
            }
            ("b", true) => self.toggle_emphasis(Emphasis::Bold),
            ("i", true) => self.toggle_emphasis(Emphasis::Italic),
            ("u", true) => self.toggle_emphasis(Emphasis::Underline),
            ("5", true) if !shift => self.toggle_emphasis(Emphasis::Strike),
            ("c", true) => self.copy(cx),
            ("x", true) => self.cut(cx),
            ("v", true) => self.paste(cx),
            ("z", true) if shift => self.undo_redo(false),
            ("z", true) => self.undo_redo(true),
            ("y", true) => self.undo_redo(false),
            ("d", true) => self.fill(true),
            ("r", true) => self.fill(false),
            ("a", true) => {
                let end = self.wb.store(self.cur).extent().unwrap_or(CellRef::new(0, 0));
                self.anchor = CellRef::new(0, 0);
                self.active = end;
            }
            ("s", true) => self.save(shift, cx),
            ("o", true) => self.open(cx),
            ("n", true) if shift => self.add_sheet(),
            ("q", true) => cx.quit(),
            // Typing starts an edit through the input handler (see ime.rs).
            _ => return false,
        }
        true
    }

    pub(crate) fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        if self.renaming.is_some() {
            self.finish_rename(true);
        }
        if let Some(axis) = self.scrollbar_at(ev.position)
            && let Some(t) = self.thumb(axis)
        {
            let along = if axis == Axis::Vertical { f64::from(ev.position.y) } else { f64::from(ev.position.x) };
            let (start, len) = if axis == Axis::Vertical { (t.y, t.h) } else { (t.x, t.w) };
            let grab = if along >= start && along <= start + len {
                along - start
            } else {
                // Clicking the track jumps the thumb there (centered on the pointer).
                self.scroll_thumb_to(axis, along - len / 2.0);
                len / 2.0
            };
            self.drag = Some(Drag::Scroll { axis, grab });
            cx.notify();
            return;
        }
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
                if ev.click_count == 2 {
                    self.autofit(col, window);
                } else {
                    self.drag = Some(Drag::ResizeCol { col, start_x: f64::from(ev.position.x), start_w: self.col_w(col) });
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

    pub(crate) fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let hover = if self.drag.is_none() { self.scrollbar_at(ev.position) } else { self.scroll_hover };
        if hover != self.scroll_hover {
            self.scroll_hover = hover;
            cx.notify();
        }
        let Some(drag) = self.drag else { return };
        if ev.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return;
        }
        match drag {
            Drag::Scroll { axis, grab } => {
                let along = if axis == Axis::Vertical { f64::from(ev.position.y) } else { f64::from(ev.position.x) };
                self.scroll_thumb_to(axis, along - grab);
            }
            Drag::ResizeCol { col, start_x, start_w } => {
                let w = (start_w + f64::from(ev.position.x) - start_x).max(MIN_COL_W);
                self.wb.set_col_width(self.cur, col, Some(w));
                self.dirty = true;
            }
            Drag::Select | Drag::Point => {
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

    pub(crate) fn on_mouse_up(&mut self, _ev: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            cx.notify();
        }
    }

    pub(crate) fn on_scroll(&mut self, ev: &ScrollEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let d = ev.delta.pixel_delta(px(ROW_H as f32));
        let (mut dx, mut dy) = (f64::from(d.x), f64::from(d.y));
        if ev.modifiers.shift && dx == 0.0 {
            std::mem::swap(&mut dx, &mut dy);
        }
        self.scroll_x = (self.scroll_x - dx).clamp(0.0, self.max_scroll_x());
        self.scroll_y = (self.scroll_y - dy).clamp(0.0, self.max_scroll_y());
        cx.notify();
    }

    pub(crate) fn autofit(&mut self, col: u32, window: &mut Window) {
        let r = Range::new(CellRef::new(0, col), CellRef::new(MAX_ROWS - 1, col));
        let mut w: f64 = MIN_COL_W;
        let f = crate::paint::cell_font();
        let cells: Vec<CellRef> = self.wb.store(self.cur).iter_range(r).take(5_000).map(|(c, _)| c).collect();
        for c in cells {
            let s = self.wb.display(self.at(c)).text;
            let line = crate::paint::shape(window, s, f.clone(), self.theme.text, CELL_FONT);
            w = w.max(f64::from(line.width) + 2.0 * CELL_PAD + 2.0);
        }
        self.wb.set_col_width(self.cur, col, Some(w.min(800.0)));
        self.dirty = true;
    }

    pub(crate) fn toggle_theme(&mut self) {
        let dark = !self.theme.dark;
        self.theme_override = Some(dark);
        self.theme = if dark { Theme::dark() } else { Theme::light() };
    }

    // ---------------------------------------------------------------- queries

    pub(crate) fn title(&self) -> String {
        format!("{}{} — xl", if self.dirty { "• " } else { "" }, self.file_name())
    }

    pub(crate) fn file_name(&self) -> String {
        self.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Untitled".into())
    }

    pub(crate) fn selection_summary(&self) -> Option<String> {
        let sel = self.selection();
        if sel.cell_count() < 2 {
            return None;
        }
        let (mut sum, mut nums, mut count) = (0.0, 0u64, 0u64);
        for (_, cell) in self.wb.store(self.cur).iter_range(sel) {
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
        // Show totals in the active cell's number format when it has one.
        let fmt = self.wb.format(self.at(self.active)).filter(|f| !f.is_general()).cloned();
        let show = |n: f64| match &fmt {
            Some(f) => f.format(&Value::Number(n)).text,
            None => xl_engine::format_general(n),
        };
        Some(format!("Sum  {}     Average  {}     Count  {count}", show(sum), show(sum / nums as f64)))
    }

    /// Short label for the active cell's number format.
    pub(crate) fn format_label(&self) -> &'static str {
        match self.wb.format(self.at(self.active)) {
            None => "General",
            Some(f) if f.is_general() => "General",
            Some(f) if f.is_date() && f.has_time() && !f.has_date() => "Time",
            Some(f) if f.is_date() => "Date",
            Some(f) if f.is_percent() => "Percent",
            Some(f) if f.is_text() => "Text",
            Some(f) if f.code().contains('$') || f.code().contains('€') || f.code().contains('£') => "Currency",
            Some(f) if f.code().contains('E') => "Scientific",
            Some(_) => "Number",
        }
    }
}

pub(crate) fn prev_boundary(s: &str, i: usize) -> usize {
    s[..i].char_indices().next_back().map_or(0, |(p, _)| p)
}

pub(crate) fn next_boundary(s: &str, i: usize) -> usize {
    s[i..].chars().next().map_or(i, |c| i + c.len_utf8())
}
