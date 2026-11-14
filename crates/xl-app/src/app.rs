//! Spreadsheet view state, editing model and input handling.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{
    Bounds, ClipboardItem, Context, ExternalPaths, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PathPromptOptions, Pixels, Point, ScrollEvent, Subscription, Window, point, px,
};
use xl_engine::{Addr, Border, CellRef, CellSnapshot, HAlign, MAX_COLS, MAX_ROWS, Range, RecalcStats, Rgb, SheetId, SheetLayout, Shift, Style, VAlign, Value, Workbook, quote_sheet_name};

use crate::theme::Theme;

pub const ROW_H: f64 = 22.0;
pub const DEF_COL_W: f64 = 100.0;
pub const MIN_ROW_H: f64 = 8.0;
const MIN_COL_W: f64 = 28.0;
pub const HEADER_W: f64 = 46.0;
pub const HEADER_H: f64 = 22.0;
pub const CELL_PAD: f64 = 6.0;
pub const CELL_FONT: f32 = 14.0;
pub const HEADER_FONT: f32 = 12.5;
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
    ResizeRow { row: u32, start_y: f64, start_h: f64 },
    /// Dragging the fill handle of `src`.
    Fill { src: Range },
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
    RowHeader { row: u32, border: Option<u32> },
    Cell(CellRef),
}

pub(crate) struct Clip {
    pub(crate) sheet: SheetId,
    pub(crate) range: Range,
    text: String,
    /// Cut, not copied: pasting moves the cells.
    pub(crate) cut: bool,
}

/// What Paste Special brings over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteMode {
    All,
    /// Results only, keeping the destination's formatting.
    Values,
    /// Number formats and styles only.
    Formats,
    /// Inputs (formulas adjusted), keeping the destination's formatting.
    Formulas,
    /// Everything, rows and columns swapped.
    Transpose,
}

/// A value as typed input that reproduces it exactly.
fn value_input(v: &Value) -> String {
    match v {
        Value::Number(n) => xl_engine::precise(*n),
        Value::Text(t) if t.starts_with('=') || t.starts_with('\'') || xl_engine::input::classify_literal(t).is_some() => format!("'{t}"),
        Value::Text(t) => t.to_string(),
        Value::Empty => String::new(),
        other => other.display(),
    }
}

/// Per-sheet selection and scroll position.
#[derive(Clone, Copy, Default)]
struct View {
    active: CellRef,
    anchor: CellRef,
    scroll_x: f64,
    scroll_y: f64,
}

type Snapshots = Vec<(Addr, CellSnapshot)>;

/// One step on the undo (or redo) stack.
pub(crate) enum UndoEntry {
    /// Restore these cells.
    Cells(Snapshots),
    /// Undo a structural change: apply its inverse, then restore the cells
    /// it deleted or rewrote, and the sheet's layout from before.
    Structure { op: Shift, restore: Snapshots, layout: SheetLayout },
    /// Redo a structural change.
    Reapply(Shift),
    /// Restore a sheet's layout (sizes, hidden lines, merges, freeze) and cells.
    Layout { sheet: SheetId, layout: SheetLayout, cells: Snapshots },
}

impl UndoEntry {
    fn touches_sheet(&self, id: SheetId) -> bool {
        match self {
            UndoEntry::Cells(c) => c.iter().any(|(a, _)| a.sheet == id),
            UndoEntry::Structure { op, .. } | UndoEntry::Reapply(op) => op.sheet() == id,
            UndoEntry::Layout { sheet, .. } => *sheet == id,
        }
    }
}

/// A one-line text field outside the grid that has keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    NameBox,
    Find,
    Replace,
    /// Conditional formatting dialog values.
    Cf1,
    Cf2,
}

/// What a right-click menu is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextKind {
    Cells,
    Rows,
    Cols,
}

/// Which toolbar popover is open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Menu {
    NumberFormat,
    TextColor,
    Fill,
    Borders,
    CondFormat,
    Main,
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
    pub(crate) clip: Option<Clip>,
    pub(crate) last_stats: Option<RecalcStats>,
    pub(crate) message: Option<(String, bool)>,
    pub(crate) path: Option<PathBuf>,
    pub(crate) dirty: bool,
    pub(crate) theme: Theme,
    theme_override: Option<bool>,
    pub(crate) last_title: String,
    /// Sheet tab being renamed, with the text typed so far.
    pub(crate) renaming: Option<(SheetId, String)>,
    /// The focused text field and its contents.
    pub(crate) field: Option<Field>,
    pub(crate) name_text: String,
    pub(crate) find: crate::find::FindState,
    /// Parts of the opened xlsx that xl can't keep ("charts", …).
    pub(crate) unsupported: Vec<&'static str>,
    /// Asking whether to overwrite a file and drop those parts.
    pub(crate) lossy_prompt: bool,
    /// Sheet whose delete button was clicked once (click again to confirm).
    pub(crate) confirm_delete: Option<SheetId>,
    pub(crate) menu: Option<Menu>,
    /// The open conditional-formatting rule dialog.
    pub(crate) cf_dialog: Option<crate::cond_ui::CfDialog>,
    /// Highlighted formula suggestion.
    pub(crate) ac_selected: usize,
    /// Suggestions hidden (Escape) for the word starting at this position.
    pub(crate) ac_dismissed: Option<usize>,
    /// While dragging the fill handle: the range it would fill.
    pub(crate) fill_target: Option<Range>,
    /// The open filter dropdown.
    pub(crate) filter_menu: Option<crate::data::FilterMenu>,
    /// Layout when a resize drag began, for its undo step.
    layout_before: Option<(SheetId, SheetLayout)>,
    /// Right-click menu: where it opened and what it's for.
    pub(crate) context: Option<(Point<Pixels>, ContextKind)>,
    /// File being opened in the background.
    pub(crate) loading: Option<String>,
    pub(crate) palette: Option<crate::palette::Palette>,
    /// Mouse went down on empty title-bar space; a following move drags the window.
    pub(crate) should_move: bool,
    /// Unsaved-changes dialog: what to do once it's answered.
    pub(crate) pending: Option<crate::lifecycle::After>,
    /// The user chose to close without saving (or nothing was unsaved).
    pub(crate) close_confirmed: bool,
    /// A crash left a recovery copy behind; offered in a bar.
    pub(crate) recovery_offer: Option<crate::lifecycle::Recovery>,
    /// Loading a recovery copy: (original document path, recovery file).
    pub(crate) restore_as: Option<(Option<PathBuf>, PathBuf)>,
    pub(crate) recovery_dir: PathBuf,
    /// Bumped on every change; autosave writes only when it moved.
    pub(crate) edit_generation: u64,
    pub(crate) autosaved_generation: Option<u64>,
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
            field: None,
            name_text: String::new(),
            find: Default::default(),
            unsupported: Vec::new(),
            lossy_prompt: false,
            confirm_delete: None,
            menu: None,
            layout_before: None,
            filter_menu: None,
            fill_target: None,
            ac_selected: 0,
            cf_dialog: None,
            ac_dismissed: None,
            context: None,
            loading: None,
            palette: None,
            should_move: false,
            pending: None,
            close_confirmed: false,
            recovery_offer: None,
            restore_as: None,
            recovery_dir: crate::lifecycle::recovery_dir(),
            edit_generation: 0,
            autosaved_generation: None,
            _appearance: appearance,
        };
        this.recovery_offer = crate::lifecycle::find_recovery(&this.recovery_dir);
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_, cx| weak.update(cx, |this, cx| this.should_close(cx)).unwrap_or(true));
        this.start_autosave(cx);
        match path {
            Some(p) => this.load(p, cx),
            None => this.load_welcome(),
        }
        this
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
        self.edit_generation += 1;
    }

    pub(crate) fn at(&self, c: CellRef) -> Addr {
        Addr::new(self.cur, c)
    }

    // ---------------------------------------------------------------- geometry
    //
    // Content space: x/y measured from the top-left of the sheet, with custom
    // sizes and hidden (zero-size) lines. Screen ("grid") space: relative to
    // the top-left of the cell area; frozen rows/columns stay put and the rest
    // scrolls by (scroll_x, scroll_y).

    pub(crate) fn col_w(&self, c: u32) -> f64 {
        let l = self.wb.layout(self.cur);
        if l.hidden_cols.contains(&c) { 0.0 } else { l.col_widths.get(&c).copied().unwrap_or(DEF_COL_W) }
    }

    pub(crate) fn row_h(&self, r: u32) -> f64 {
        let l = self.wb.layout(self.cur);
        if l.hidden_rows.contains(&r) || l.filtered_rows.contains(&r) { 0.0 } else { l.row_heights.get(&r).copied().unwrap_or(ROW_H) }
    }

    /// Content-space x of the left edge of column `c`.
    pub(crate) fn col_x(&self, c: u32) -> f64 {
        let l = self.wb.layout(self.cur);
        line_offset(c, DEF_COL_W, &l.col_widths, &l.hidden_cols, &std::collections::BTreeSet::new())
    }

    /// Content-space y of the top edge of row `r`.
    pub(crate) fn row_y(&self, r: u32) -> f64 {
        let l = self.wb.layout(self.cur);
        line_offset(r, ROW_H, &l.row_heights, &l.hidden_rows, &l.filtered_rows)
    }

    /// Column containing content-space x (skipping hidden columns).
    pub(crate) fn col_at(&self, x: f64) -> u32 {
        last_at_or_before(x, MAX_COLS, |c| self.col_x(c))
    }

    /// Row containing content-space y (skipping hidden rows).
    pub(crate) fn row_at(&self, y: f64) -> u32 {
        last_at_or_before(y, MAX_ROWS, |r| self.row_y(r))
    }

    /// Frozen rows and columns.
    pub(crate) fn frozen(&self) -> (u32, u32) {
        self.wb.layout(self.cur).freeze
    }

    /// Size of the frozen area (width, height).
    pub(crate) fn frozen_size(&self) -> (f64, f64) {
        let (fr, fc) = self.frozen();
        (self.col_x(fc), self.row_y(fr))
    }

    /// Grid-space x of column `c`'s left edge.
    pub(crate) fn grid_x(&self, c: u32) -> f64 {
        let (_, fc) = self.frozen();
        if c < fc { self.col_x(c) } else { self.col_x(c) - self.col_x(fc) + self.frozen_size().0 - self.scroll_x }
    }

    /// Grid-space y of row `r`'s top edge.
    pub(crate) fn grid_y(&self, r: u32) -> f64 {
        let (fr, _) = self.frozen();
        if r < fr { self.row_y(r) } else { self.row_y(r) - self.row_y(fr) + self.frozen_size().1 - self.scroll_y }
    }

    /// Column under grid-space x.
    pub(crate) fn col_at_grid(&self, x: f64) -> u32 {
        let (_, fc) = self.frozen();
        let fw = self.frozen_size().0;
        if x < fw { self.col_at(x).min(fc.saturating_sub(1)) } else { self.col_at(x - fw + self.col_x(fc) + self.scroll_x).max(fc) }
    }

    /// Row under grid-space y.
    pub(crate) fn row_at_grid(&self, y: f64) -> u32 {
        let (fr, _) = self.frozen();
        let fh = self.frozen_size().1;
        if y < fh { self.row_at(y).min(fr.saturating_sub(1)) } else { self.row_at(y - fh + self.row_y(fr) + self.scroll_y).max(fr) }
    }

    pub(crate) fn grid_w(&self) -> f64 {
        (f64::from(self.viewport.size.width) - HEADER_W).max(0.0)
    }

    pub(crate) fn grid_h(&self) -> f64 {
        (f64::from(self.viewport.size.height) - HEADER_H).max(0.0)
    }

    /// Rows that fit in the scrolling part of the grid (for Page Up/Down).
    fn visible_rows(&self) -> u32 {
        let (fr, _) = self.frozen();
        let top = self.row_at_grid(self.frozen_size().1).max(fr);
        let bottom = self.row_at_grid(self.grid_h() - 1.0);
        (bottom.saturating_sub(top)).max(1)
    }

    pub(crate) fn max_scroll_x(&self) -> f64 {
        let (_, fc) = self.frozen();
        (self.col_x(MAX_COLS) - self.col_x(fc) - (self.grid_w() - self.frozen_size().0)).max(0.0)
    }

    pub(crate) fn max_scroll_y(&self) -> f64 {
        let (fr, _) = self.frozen();
        (self.row_y(MAX_ROWS) - self.row_y(fr) - (self.grid_h() - self.frozen_size().1)).max(0.0)
    }

    /// Scrollable content size (of the scrolling panes): the used area plus
    /// one screen, at least as far as the current scroll position.
    fn content_size(&self) -> (f64, f64) {
        let extent = self.wb.store(self.cur).extent().unwrap_or_default();
        let (fr, fc) = self.frozen();
        let (fw, fh) = self.frozen_size();
        let (gw, gh) = (self.grid_w() - fw, self.grid_h() - fh);
        let h = (self.row_y(extent.row.max(self.active.row) + 1) - self.row_y(fr) + gh).max(self.scroll_y + gh);
        let w = (self.col_x(extent.col.max(self.active.col) + 1) - self.col_x(fc) + gw).max(self.scroll_x + gw);
        (w, h)
    }

    pub(crate) fn thumb(&self, axis: Axis) -> Option<Thumb> {
        let ox = f64::from(self.viewport.origin.x);
        let oy = f64::from(self.viewport.origin.y);
        let (vw, vh) = (f64::from(self.viewport.size.width), f64::from(self.viewport.size.height));
        let (fw, fh) = self.frozen_size();
        // Scrollbars run along the scrolling part only.
        let (gx, gy, gw, gh) = (ox + HEADER_W + fw, oy + HEADER_H + fh, self.grid_w() - fw, self.grid_h() - fh);
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
        let (fw, fh) = self.frozen_size();
        match axis {
            Axis::Vertical => {
                let gy = f64::from(self.viewport.origin.y) + HEADER_H + fh;
                let gh = self.grid_h() - fh;
                let track = (gh - t.h).max(1.0);
                self.scroll_y = (((edge - gy) / track).clamp(0.0, 1.0) * (ch - gh)).clamp(0.0, self.max_scroll_y());
            }
            Axis::Horizontal => {
                let gx = f64::from(self.viewport.origin.x) + HEADER_W + fw;
                let gw = self.grid_w() - fw;
                let track = (gw - t.w).max(1.0);
                self.scroll_x = (((edge - gx) / track).clamp(0.0, 1.0) * (cw - gw)).clamp(0.0, self.max_scroll_x());
            }
        }
    }

    pub(crate) fn ensure_visible(&mut self, c: CellRef) {
        if self.viewport.size.width <= px(0.) {
            return;
        }
        let (fr, fc) = self.frozen();
        let (fw, fh) = self.frozen_size();
        if c.col >= fc {
            let (x0, x1) = (self.col_x(c.col) - self.col_x(fc), self.col_x(c.col) - self.col_x(fc) + self.col_w(c.col));
            let view = self.grid_w() - fw;
            if x0 < self.scroll_x {
                self.scroll_x = x0;
            } else if x1 > self.scroll_x + view {
                self.scroll_x = (x1 - view).min(x0);
            }
        }
        if c.row >= fr {
            let (y0, y1) = (self.row_y(c.row) - self.row_y(fr), self.row_y(c.row) - self.row_y(fr) + self.row_h(c.row));
            let view = self.grid_h() - fh;
            if y0 < self.scroll_y {
                self.scroll_y = y0;
            } else if y1 > self.scroll_y + view {
                self.scroll_y = (y1 - view).min(y0);
            }
        }
    }

    fn hit_test(&self, pos: Point<Pixels>) -> Option<Hit> {
        let x = f64::from(pos.x - self.viewport.origin.x);
        let y = f64::from(pos.y - self.viewport.origin.y);
        if x < 0.0 || y < 0.0 || x > f64::from(self.viewport.size.width) || y > f64::from(self.viewport.size.height) {
            return None;
        }
        let (gx, gy) = (x - HEADER_W, y - HEADER_H);
        Some(match (x < HEADER_W, y < HEADER_H) {
            (true, true) => Hit::Corner,
            (false, true) => {
                let col = self.col_at_grid(gx);
                let (left, right) = (self.grid_x(col), self.grid_x(col) + self.col_w(col));
                let border = if (right - gx).abs() <= 4.0 {
                    Some(col)
                } else if (gx - left).abs() <= 4.0 && col > 0 {
                    Some(self.prev_visible_col(col))
                } else {
                    None
                };
                Hit::ColHeader { col, border }
            }
            (true, false) => {
                let row = self.row_at_grid(gy);
                let (top, bottom) = (self.grid_y(row), self.grid_y(row) + self.row_h(row));
                let border = if (bottom - gy).abs() <= 3.0 {
                    Some(row)
                } else if (gy - top).abs() <= 3.0 && row > 0 {
                    Some(self.prev_visible_row(row))
                } else {
                    None
                };
                Hit::RowHeader { row, border }
            }
            (false, false) => Hit::Cell(CellRef::new(self.row_at_grid(gy), self.col_at_grid(gx))),
        })
    }

    fn prev_visible_col(&self, c: u32) -> u32 {
        (0..c).rev().find(|&x| self.col_w(x) > 0.0).unwrap_or(0)
    }

    fn prev_visible_row(&self, r: u32) -> u32 {
        (0..r).rev().find(|&x| self.row_h(x) > 0.0).unwrap_or(0)
    }

    // ------------------------------------------------------------- selection

    pub fn selection(&self) -> Range {
        Range::new(self.anchor, self.active)
    }

    pub(crate) fn select(&mut self, c: CellRef, extend: bool) {
        // A merged range is selected through its top-left cell.
        let c = if extend { c } else { self.wb.layout(self.cur).merge_at(c).map_or(c, |m| m.start) };
        self.active = c;
        if !extend {
            self.anchor = c;
        }
        self.ensure_visible(c);
    }

    /// Arrow keys: one visible cell over, stepping across merged ranges and
    /// skipping hidden rows and columns.
    fn move_by(&mut self, dr: i64, dc: i64, extend: bool) {
        let here = self.wb.layout(self.cur).merge_at(self.active).filter(|_| !extend).unwrap_or(Range::single(self.active));
        let mut row = if dr > 0 { here.end.row } else { here.start.row } as i64;
        let mut col = if dc > 0 { here.end.col } else { here.start.col } as i64;
        loop {
            let (nr, nc) = ((row + dr).clamp(0, MAX_ROWS as i64 - 1), (col + dc).clamp(0, MAX_COLS as i64 - 1));
            if (nr, nc) == (row, col) {
                break;
            }
            (row, col) = (nr, nc);
            if self.row_h(row as u32) > 0.0 && self.col_w(col as u32) > 0.0 {
                break;
            }
        }
        if self.row_h(row as u32) == 0.0 || self.col_w(col as u32) == 0.0 {
            return;
        }
        let to = CellRef::new(row as u32, if dc == 0 { self.active.col } else { col as u32 });
        let to = CellRef::new(if dr == 0 { self.active.row } else { to.row }, to.col);
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
            self.mark_dirty();
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
                self.undo.retain(|e| !e.touches_sheet(id));
                self.redo.retain(|e| !e.touches_sheet(id));
                self.message = Some((format!("Deleted sheet ({} formulas recalculated)", stats.evaluated), false));
                self.mark_dirty();
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
                    self.push_undo(UndoEntry::Cells(vec![(edit.at, before)]));
                    self.last_stats = Some(stats);
                    self.mark_dirty();
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

    fn snapshots(&self, addrs: impl IntoIterator<Item = Addr>) -> Snapshots {
        addrs.into_iter().map(|a| (a, self.wb.snapshot(a))).collect()
    }

    /// Apply raw inputs (with auto-formatting) as one undoable step.
    pub(crate) fn apply_inputs(&mut self, changes: Vec<(Addr, String)>) {
        if changes.is_empty() {
            return;
        }
        let undo = self.snapshots(changes.iter().map(|(a, _)| *a));
        self.last_stats = Some(self.wb.set_inputs(changes));
        self.push_undo(UndoEntry::Cells(undo));
        self.mark_dirty();
    }

    /// Apply exact snapshots (inputs + formats) as one undoable step.
    pub(crate) fn apply_snapshots(&mut self, items: Snapshots) {
        if items.is_empty() {
            return;
        }
        let undo = self.snapshots(items.iter().map(|(a, _)| *a));
        self.last_stats = Some(self.wb.restore(items));
        self.push_undo(UndoEntry::Cells(undo));
        self.mark_dirty();
    }

    pub(crate) fn undo_redo(&mut self, is_undo: bool) {
        let entry = if is_undo { self.undo.pop() } else { self.redo.pop() };
        let Some(entry) = entry else { return };
        let inverse = match entry {
            UndoEntry::Cells(cells) => {
                let inverse = self.snapshots(cells.iter().map(|(a, _)| *a));
                if let Some(&(first, _)) = cells.first() {
                    self.switch_sheet(first.sheet);
                    let on_sheet: Vec<CellRef> = cells.iter().filter(|(a, _)| a.sheet == first.sheet).map(|(a, _)| a.cell).collect();
                    let r0 = on_sheet.iter().map(|c| c.row).min().unwrap_or(0);
                    let r1 = on_sheet.iter().map(|c| c.row).max().unwrap_or(0);
                    let c0 = on_sheet.iter().map(|c| c.col).min().unwrap_or(0);
                    let c1 = on_sheet.iter().map(|c| c.col).max().unwrap_or(0);
                    self.anchor = CellRef::new(r0, c0);
                    self.active = CellRef::new(r1, c1);
                    self.ensure_visible(self.active);
                }
                self.last_stats = Some(self.wb.restore(cells));
                UndoEntry::Cells(inverse)
            }
            UndoEntry::Structure { op, restore, layout } => {
                self.switch_sheet(op.sheet());
                self.wb.apply_shift(op.inverse());
                // Put back only what the change actually altered.
                let changed: Snapshots = restore.into_iter().filter(|(a, snap)| self.wb.snapshot(*a) != *snap).collect();
                self.last_stats = Some(self.wb.restore(changed));
                if let Some(l) = self.wb.layout_mut(op.sheet()) {
                    *l = layout;
                }
                self.clip = None;
                UndoEntry::Reapply(op)
            }
            UndoEntry::Reapply(op) => {
                self.switch_sheet(op.sheet());
                self.structural(op)
            }
            UndoEntry::Layout { sheet, layout, cells } => {
                self.switch_sheet(sheet);
                let inverse = UndoEntry::Layout { sheet, layout: self.wb.layout(sheet).clone(), cells: self.snapshots(cells.iter().map(|(a, _)| *a)) };
                if let Some(l) = self.wb.layout_mut(sheet) {
                    *l = layout;
                }
                if !cells.is_empty() {
                    self.last_stats = Some(self.wb.restore(cells));
                }
                inverse
            }
        };
        if is_undo { self.redo.push(inverse) } else { self.undo.push(inverse) }
        self.mark_dirty();
    }

    /// Apply a structural change and return the entry that undoes it.
    fn structural(&mut self, op: Shift) -> UndoEntry {
        let sheet = op.sheet();
        let mut restore = match op {
            Shift::DeleteRows { at, count, .. } => {
                self.wb.region_snapshots(sheet, Range::new(CellRef::new(at, 0), CellRef::new(at + count - 1, MAX_COLS - 1)))
            }
            Shift::DeleteCols { at, count, .. } => {
                self.wb.region_snapshots(sheet, Range::new(CellRef::new(0, at), CellRef::new(MAX_ROWS - 1, at + count - 1)))
            }
            _ => Vec::new(),
        };
        // Formulas anywhere may be rewritten (e.g. to #REF!); remember them all.
        restore.extend(self.wb.formula_texts().into_iter().map(|(a, _)| (a, self.wb.snapshot(a))));
        let layout = self.wb.layout(sheet).clone();
        self.last_stats = Some(self.wb.apply_shift(op));
        self.clip = None;
        UndoEntry::Structure { op, restore, layout }
    }

    /// Change the current sheet's layout (and possibly cells in `touched`) as
    /// one undoable step.
    pub(crate) fn change_layout(&mut self, touched: Option<Range>, f: impl FnOnce(&mut Self)) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let sheet = self.cur;
        let before = self.wb.layout(sheet).clone();
        let cells = touched.map_or_else(Vec::new, |r| self.wb.region_snapshots(sheet, r));
        f(self);
        let changed_cells: Snapshots = cells.into_iter().filter(|(a, snap)| self.wb.snapshot(*a) != *snap).collect();
        if *self.wb.layout(sheet) != before || !changed_cells.is_empty() {
            self.push_undo(UndoEntry::Layout { sheet, layout: before, cells: changed_cells });
            self.mark_dirty();
        }
    }

    /// Merge the selection (keeping the top-left value), or unmerge it if it
    /// overlaps merged cells.
    pub(crate) fn toggle_merge(&mut self, center: bool) {
        let sel = self.selection();
        let sheet = self.cur;
        let overlaps = self.wb.layout(sheet).merges.iter().any(|m| m.start.row <= sel.end.row && sel.start.row <= m.end.row && m.start.col <= sel.end.col && sel.start.col <= m.end.col);
        if overlaps {
            self.change_layout(None, |s| s.wb.unmerge(sheet, sel));
            return;
        }
        if sel.cell_count() < 2 {
            return;
        }
        if sel.cell_count() > 1_000_000 {
            self.message = Some(("Select a smaller range to merge".into(), true));
            return;
        }
        self.change_layout(Some(sel), |s| {
            s.last_stats = Some(s.wb.merge(sheet, sel));
            if center {
                let a = s.at(sel.start);
                let mut st = s.wb.style(a).clone();
                st.align = HAlign::Center;
                st.valign = VAlign::Center;
                s.wb.restore(vec![(a, CellSnapshot { style: st, ..s.wb.snapshot(a) })]);
            }
        });
        self.anchor = sel.start;
        self.active = sel.start;
    }

    pub(crate) fn toggle_wrap(&mut self) {
        let on = !self.wb.style(self.at(self.active)).wrap;
        self.modify_styles(|st, _, _| st.wrap = on);
    }

    pub(crate) fn set_valign(&mut self, v: VAlign) {
        self.modify_styles(|st, _, _| st.valign = v);
    }

    /// Font size in points (None = default).
    pub(crate) fn set_font_size(&mut self, pt: Option<f64>) {
        let tenths = pt.map(|p| (p.clamp(1.0, 409.0) * 10.0).round() as u16);
        self.modify_styles(|st, _, _| st.font_size = tenths);
    }

    /// Freeze rows above and columns left of the active cell, or unfreeze.
    pub(crate) fn toggle_freeze(&mut self) {
        let target = if self.frozen() != (0, 0) { (0, 0) } else { (self.active.row, self.active.col) };
        self.set_freeze(target);
    }

    pub(crate) fn set_freeze(&mut self, freeze: (u32, u32)) {
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                l.freeze = freeze;
            }
            s.scroll_x = 0.0;
            s.scroll_y = 0.0;
        });
        self.ensure_visible(self.active);
    }

    /// Hide (or unhide) the selected rows or columns.
    pub(crate) fn set_hidden(&mut self, rows: bool, hide: bool) {
        let sel = self.selection();
        let (a, z) = if rows { (sel.start.row, sel.end.row) } else { (sel.start.col, sel.end.col) };
        // Unhiding a whole-sheet selection only touches lines that are hidden.
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                let set = if rows { &mut l.hidden_rows } else { &mut l.hidden_cols };
                if hide {
                    set.extend(a..=z.min(a + 1_048_576));
                } else {
                    let gone: Vec<u32> = set.range(a..=z).copied().collect();
                    for k in gone {
                        set.remove(&k);
                    }
                }
            }
        });
        if hide {
            // Move off the hidden lines.
            let next = if rows { (z + 1..MAX_ROWS).find(|&r| self.row_h(r) > 0.0).map(|r| CellRef::new(r, self.active.col)) } else { (z + 1..MAX_COLS).find(|&c| self.col_w(c) > 0.0).map(|c| CellRef::new(self.active.row, c)) };
            if let Some(c) = next {
                self.select(c, false);
            }
        }
    }

    /// Whole rows selected (every column)?
    pub(crate) fn rows_selected(&self) -> bool {
        let s = self.selection();
        s.start.col == 0 && s.end.col == MAX_COLS - 1
    }

    /// Whole columns selected (every row)?
    pub(crate) fn cols_selected(&self) -> bool {
        let s = self.selection();
        s.start.row == 0 && s.end.row == MAX_ROWS - 1
    }

    fn do_structural(&mut self, op: Shift) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let undo = self.structural(op);
        self.push_undo(undo);
        self.mark_dirty();
        self.context = None;
    }

    /// Insert as many rows as are selected, above (or below) the selection.
    pub(crate) fn insert_rows(&mut self, below: bool) {
        let s = self.selection();
        let at = if below { s.end.row + 1 } else { s.start.row };
        self.do_structural(Shift::InsertRows { sheet: self.cur, at, count: s.rows().min(MAX_ROWS - at) });
        if below {
            let (c0, c1) = (self.anchor.col.min(self.active.col), self.anchor.col.max(self.active.col));
            self.anchor = CellRef::new(at, c0);
            self.active = CellRef::new(at + s.rows() - 1, c1);
        }
    }

    pub(crate) fn insert_cols(&mut self, right: bool) {
        let s = self.selection();
        let at = if right { s.end.col + 1 } else { s.start.col };
        self.do_structural(Shift::InsertCols { sheet: self.cur, at, count: s.cols().min(MAX_COLS - at) });
        if right {
            let (r0, r1) = (self.anchor.row.min(self.active.row), self.anchor.row.max(self.active.row));
            self.anchor = CellRef::new(r0, at);
            self.active = CellRef::new(r1, at + s.cols() - 1);
        }
    }

    pub(crate) fn delete_rows(&mut self) {
        let s = self.selection();
        self.do_structural(Shift::DeleteRows { sheet: self.cur, at: s.start.row, count: s.rows() });
        self.active = CellRef::new(s.start.row, self.active.col.min(self.anchor.col));
        self.anchor = self.active;
    }

    pub(crate) fn delete_cols(&mut self) {
        let s = self.selection();
        self.do_structural(Shift::DeleteCols { sheet: self.cur, at: s.start.col, count: s.cols() });
        self.active = CellRef::new(self.active.row.min(self.anchor.row), s.start.col);
        self.anchor = self.active;
    }

    /// Ctrl+Shift+= / Ctrl+-: whole columns if columns are selected, else rows.
    fn insert_or_delete(&mut self, insert: bool) {
        match (insert, self.cols_selected() && !self.rows_selected()) {
            (true, true) => self.insert_cols(false),
            (true, false) => self.insert_rows(false),
            (false, true) => self.delete_cols(),
            (false, false) => self.delete_rows(),
        }
    }

    /// Delete clears contents but keeps formats, like Excel.
    pub(crate) fn clear_selection(&mut self) {
        let sel = self.selection();
        let cells: Vec<_> = self.wb.store(self.cur).iter_range(sel).map(|(c, _)| (self.at(c), String::new())).collect();
        self.apply_inputs(cells);
    }

    pub(crate) fn guard_bulk(&mut self, r: Range) -> bool {
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
        self.clip = Some(Clip { sheet: self.cur, range: sel, text, cut: false });
        self.message = Some((format!("Copied {sel}"), false));
    }

    pub(crate) fn paste(&mut self, cx: &mut Context<Self>) {
        self.paste_with(PasteMode::All, cx);
    }

    pub(crate) fn paste_with(&mut self, mode: PasteMode, cx: &mut Context<Self>) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) else { return };
        let origin = self.selection().start;
        let mut end = origin;
        let internal = self.clip.as_ref().filter(|c| c.text == text && self.wb.sheet_ids().contains(&c.sheet)).map(|c| (c.sheet, c.range, c.cut));
        match internal {
            Some((src_sheet, range, true)) if mode == PasteMode::All => {
                self.move_cells(src_sheet, range, origin);
                return;
            }
            Some((src_sheet, range, _)) => {
                let transpose = mode == PasteMode::Transpose;
                let mut items = Vec::new();
                for src in range.iter() {
                    let (i, j) = ((src.row - range.start.row) as i64, (src.col - range.start.col) as i64);
                    let (i, j) = if transpose { (j, i) } else { (i, j) };
                    let Some(dst) = origin.offset(i, j) else { continue };
                    let (from, to) = (Addr::new(src_sheet, src), self.at(dst));
                    let (dr, dc) = (dst.row as i64 - src.row as i64, dst.col as i64 - src.col as i64);
                    let here = self.wb.snapshot(to);
                    let snap = match mode {
                        PasteMode::All | PasteMode::Transpose => self.wb.snapshot_shifted(from, dr, dc),
                        PasteMode::Values => CellSnapshot { input: value_input(self.wb.value(from)), ..here },
                        PasteMode::Formulas => CellSnapshot { input: self.wb.input_text_shifted(from, dr, dc), ..here },
                        PasteMode::Formats => CellSnapshot { input: here.input, ..self.wb.snapshot(from) },
                    };
                    items.push((to, snap));
                    end = CellRef::new(end.row.max(dst.row), end.col.max(dst.col));
                }
                self.apply_snapshots(items);
            }
            None => {
                if mode == PasteMode::Formats {
                    return;
                }
                let text = text.strip_suffix('\n').unwrap_or(&text);
                let mut changes = Vec::new();
                for (i, line) in text.split('\n').enumerate() {
                    for (j, field) in line.trim_end_matches('\r').split('\t').enumerate() {
                        let (i, j) = if mode == PasteMode::Transpose { (j, i) } else { (i, j) };
                        if let Some(dst) = origin.offset(i as i64, j as i64) {
                            changes.push((self.at(dst), field.to_string()));
                            end = CellRef::new(end.row.max(dst.row), end.col.max(dst.col));
                        }
                    }
                }
                self.apply_inputs(changes);
            }
        }
        self.anchor = origin;
        self.active = end;
    }

    /// Paste after a cut: move the cells, carrying references to them along.
    fn move_cells(&mut self, src_sheet: SheetId, range: Range, to: CellRef) {
        let (dr, dc) = (to.row as i64 - range.start.row as i64, to.col as i64 - range.start.col as i64);
        if range.end.row as i64 + dr >= MAX_ROWS as i64 || range.end.col as i64 + dc >= MAX_COLS as i64 {
            self.message = Some(("The cells don't fit there".into(), true));
            return;
        }
        let mv = xl_engine::Move { from_sheet: src_sheet, from: range, to_sheet: self.cur, dr, dc };
        if (dr, dc) == (0, 0) && src_sheet == self.cur {
            self.clip = None;
            return;
        }
        // Undo restores both blocks and every formula (any may have been rewritten).
        let mut undo = self.wb.region_snapshots(src_sheet, range);
        undo.extend(self.wb.region_snapshots(self.cur, mv.to_range()));
        // Cells in the destination that are currently empty must be cleared on undo.
        for c in mv.to_range().iter() {
            let a = self.at(c);
            if !undo.iter().any(|(x, _)| *x == a) {
                undo.push((a, CellSnapshot::default()));
            }
        }
        let formulas = self.wb.formula_texts();
        undo.extend(formulas.into_iter().filter(|(a, _)| !undo.iter().any(|(x, _)| x == a)).map(|(a, _)| (a, self.wb.snapshot(a))).collect::<Vec<_>>());
        self.last_stats = Some(self.wb.move_range(mv));
        self.push_undo(UndoEntry::Cells(undo));
        self.mark_dirty();
        self.clip = None;
        self.anchor = mv.to_range().end;
        self.active = mv.to_range().start;
    }

    pub(crate) fn cut(&mut self, cx: &mut Context<Self>) {
        self.copy(cx);
        if let Some(clip) = self.clip.as_mut() {
            clip.cut = true;
            self.message = Some((format!("Cut {} — paste to move it", clip.range), false));
        }
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
        self.push_undo(UndoEntry::Cells(undo));
        self.mark_dirty();
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
        self.push_undo(UndoEntry::Cells(undo));
        self.mark_dirty();
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
        self.push_undo(UndoEntry::Cells(undo));
        self.mark_dirty();
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

    /// A small, real-looking workbook so the first launch shows what xl does.
    fn load_welcome(&mut self) {
        let s = self.cur;
        self.wb.rename_sheet(s, "Q1 Budget").ok();
        let rows: &[&[&str]] = &[
            &["Operating budget, Q1 2026"],
            &[],
            &["Category", "Jan", "Feb", "Mar", "Q1 total", "Budget", "Variance"],
            &["Salaries", "$42,000", "$42,000", "$44,500", "=SUM(B4:D4)", "$132,000", "=E4/F4-1"],
            &["Rent", "$6,200", "$6,200", "$6,200", "=SUM(B5:D5)", "$18,600", "=E5/F5-1"],
            &["Software", "$1,940", "$2,115", "$2,380", "=SUM(B6:D6)", "$6,000", "=E6/F6-1"],
            &["Travel", "$3,400", "$1,250", "$4,860", "=SUM(B7:D7)", "$9,000", "=E7/F7-1"],
            &["Marketing", "$5,000", "$7,500", "$6,100", "=SUM(B8:D8)", "$18,000", "=E8/F8-1"],
            &["Total", "=SUM(B4:B8)", "=SUM(C4:C8)", "=SUM(D4:D8)", "=SUM(E4:E8)", "=SUM(F4:F8)", "=E9/F9-1"],
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
        let at = |a1: &str| Addr::new(s, CellRef::parse(a1).unwrap());
        for r in 4..=9 {
            for c in ["B", "C", "D", "E", "F"] {
                self.wb.set_format(at(&format!("{c}{r}")), Some("$#,##0"));
            }
            // Over budget (positive variance) is the number to notice.
            self.wb.set_format(at(&format!("G{r}")), Some("[Red]+0.0%;-0.0%;0.0%"));
        }
        self.wb.set_style(at("A1"), &Style { bold: true, ..Style::default() });
        for c in ["A", "B", "C", "D", "E", "F", "G"] {
            let align = if c == "A" { HAlign::General } else { HAlign::Right };
            self.wb.set_style(at(&format!("{c}3")), &Style { bold: true, align, bottom: Border::THIN, ..Style::default() });
            self.wb.set_style(at(&format!("{c}9")), &Style { bold: true, top: Border::THIN, ..Style::default() });
        }
        self.wb.set_col_width(s, 0, Some(150.0));
        self.active = CellRef::new(3, 1);
        self.anchor = self.active;
        self.dirty = false;
    }

    /// Open a file, first asking about unsaved changes if there are any.
    pub(crate) fn open_or_ask(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.dirty {
            self.pending = Some(crate::lifecycle::After::Open(path));
        } else {
            self.load(path, cx);
        }
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
                self.unsupported = loaded.unsupported;
                self.message = Some(match loaded.warnings.first() {
                    Some(w) => (format!("Opened {name} with {} warning(s): {w}", loaded.warnings.len()), true),
                    None => (format!("Opened {name} ({} cells)", loaded.cells), false),
                });
                self.path = Some(path);
                if let Some((original, file)) = self.restore_as.take() {
                    // A restored recovery copy stands in for the original document.
                    self.path = original;
                    self.mark_dirty();
                    crate::lifecycle::remove_recovery(&file);
                    self.message = Some(("Restored your unsaved changes. Save to keep them.".into(), false));
                }
            }
            Err(e) => self.message = Some((format!("Could not open {name}: {e}"), true)),
        }
    }

    /// Saving over the opened file would drop parts xl can't keep.
    pub(crate) fn would_lose_parts(&self) -> bool {
        !self.unsupported.is_empty() && self.path.as_deref().and_then(xl_io::FileKind::of) == Some(xl_io::FileKind::Xlsx)
    }

    pub(crate) fn save_to(&mut self, path: PathBuf) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        match xl_io::save(&self.wb, &path, self.cur) {
            Ok(()) => {
                let csv = xl_io::FileKind::of(&path) == Some(xl_io::FileKind::Csv);
                self.dirty = false;
                self.clear_recovery();
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
            if self.would_lose_parts() {
                self.lossy_prompt = true;
                return;
            }
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
                    this.open_or_ask(path, cx);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    pub(crate) fn on_drop_files(&mut self, paths: &ExternalPaths, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = paths.paths().iter().find(|p| xl_io::FileKind::of(p).is_some()) {
            self.open_or_ask(p.clone(), cx);
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
        let handled = if self.lossy_prompt {
            self.key_lossy(key, cx)
        } else if self.cf_dialog.is_some() {
            self.key_cf_dialog(key)
        } else if self.pending.is_some() {
            self.key_dialog(key, window, cx)
        } else if ctrl && (key == "q" || key == "w") {
            let after = if key == "q" { crate::lifecycle::After::Quit } else { crate::lifecycle::After::Close };
            self.request(after, window, cx);
            true
        } else if palette_key {
            if self.palette.take().is_none() {
                self.open_palette();
            }
            true
        } else if self.palette.is_some() {
            self.key_palette(key, window, cx)
        } else if self.renaming.is_some() {
            self.key_renaming(key)
        } else if matches!(self.field, Some(Field::Find | Field::Replace)) {
            self.key_find(key, m.shift)
        } else if self.field.is_some() {
            self.key_field(key)
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

    fn key_field(&mut self, key: &str) -> bool {
        if self.field != Some(Field::NameBox) {
            return false;
        }
        match key {
            "enter" => {
                let text = std::mem::take(&mut self.name_text);
                self.field = None;
                self.name_box_go(&text);
            }
            "escape" => self.field = None,
            "backspace" => {
                self.name_text.pop();
            }
            _ => return false,
        }
        true
    }

    /// Focus the name box (Ctrl+G), ready to type a reference or name.
    pub(crate) fn focus_name_box(&mut self) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        self.name_text.clear();
        self.field = Some(Field::NameBox);
    }

    /// Name box Enter: jump to a cell, range or defined name, or define a
    /// new name for the selection.
    pub(crate) fn name_box_go(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if let Some((sheet, range)) = self.wb.resolve_reference(text, self.cur) {
            self.switch_sheet(sheet);
            self.anchor = range.start;
            self.active = range.end;
            self.ensure_visible(range.start);
            self.active = range.start;
            self.anchor = range.end;
            return;
        }
        let sel = self.selection();
        let target = format!("{}!{}", quote_sheet_name(self.wb.sheet_name(self.cur)), absolute_range(sel));
        match self.wb.define_name(text, None, &target) {
            Ok(stats) => {
                self.last_stats = Some(stats);
                self.mark_dirty();
                self.message = Some((format!("Defined the name {text} for {}", absolute_range(sel)), false));
            }
            Err(e) => self.message = Some((format!("Not a reference or a valid name: {e}"), true)),
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
        if !ctrl && !alt && self.key_completion(key) {
            return true;
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
                self.context = None;
                self.filter_menu = None;
                self.clip = None;
            }
            ("=" | "+", true) if shift => self.insert_or_delete(true),
            ("-", true) => self.insert_or_delete(false),
            ("g", true) => self.focus_name_box(),
            ("l", true) if shift => self.toggle_filter(),
            ("f", true) => self.open_find(false),
            ("h", true) => self.open_find(true),
            ("b", true) => self.toggle_emphasis(Emphasis::Bold),
            ("i", true) => self.toggle_emphasis(Emphasis::Italic),
            ("u", true) => self.toggle_emphasis(Emphasis::Underline),
            ("5", true) if !shift => self.toggle_emphasis(Emphasis::Strike),
            ("c", true) => self.copy(cx),
            ("x", true) => self.cut(cx),
            ("v", true) if shift => self.paste_with(PasteMode::Values, cx),
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
            // Typing starts an edit through the input handler (see ime.rs).
            _ => return false,
        }
        true
    }

    pub(crate) fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        self.field = None;
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
        if matches!(hit, Hit::Cell(_)) && self.on_fill_handle(ev.position) {
            let src = self.selection();
            if ev.click_count >= 2 {
                self.fill_down_to_data();
            } else {
                self.drag = Some(Drag::Fill { src });
                self.fill_target = Some(src);
            }
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
                    self.layout_before = Some((self.cur, self.wb.layout(self.cur).clone()));
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
            Hit::RowHeader { border: Some(row), .. } => {
                if ev.click_count == 2 {
                    self.autofit_row(row, window);
                } else {
                    self.layout_before = Some((self.cur, self.wb.layout(self.cur).clone()));
                    self.drag = Some(Drag::ResizeRow { row, start_y: f64::from(ev.position.y), start_h: self.row_h(row) });
                }
            }
            Hit::RowHeader { row, .. } => {
                if !shift {
                    self.anchor = CellRef::new(row, 0);
                }
                self.active = CellRef::new(row, MAX_COLS - 1);
                self.anchor.col = 0;
            }
            Hit::Cell(c) if self.filter_button_at(ev.position) == Some(c.col) => {
                self.open_filter_menu(c.col, ev.position);
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

    /// Right-click: select what was clicked (unless it's already selected),
    /// then open the context menu for it.
    pub(crate) fn on_right_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let Some(hit) = self.hit_test(ev.position) else { return };
        let sel = self.selection();
        let kind = match hit {
            Hit::RowHeader { row: r, .. } => {
                if !(self.rows_selected() && (sel.start.row..=sel.end.row).contains(&r)) {
                    self.anchor = CellRef::new(r, 0);
                    self.active = CellRef::new(r, MAX_COLS - 1);
                }
                ContextKind::Rows
            }
            Hit::ColHeader { col, .. } => {
                if !(self.cols_selected() && (sel.start.col..=sel.end.col).contains(&col)) {
                    self.anchor = CellRef::new(0, col);
                    self.active = CellRef::new(MAX_ROWS - 1, col);
                }
                ContextKind::Cols
            }
            Hit::Cell(c) => {
                if !sel.contains(c) {
                    self.select(c, false);
                }
                ContextKind::Cells
            }
            Hit::Corner => ContextKind::Cells,
        };
        self.menu = None;
        self.context = Some((ev.position, kind));
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
                self.mark_dirty();
            }
            Drag::ResizeRow { row, start_y, start_h } => {
                let h = (start_h + f64::from(ev.position.y) - start_y).max(MIN_ROW_H);
                self.wb.set_row_height(self.cur, row, Some(h));
                self.mark_dirty();
            }
            Drag::Fill { src } => {
                let vp = self.viewport;
                let p = point(
                    ev.position.x.clamp(vp.origin.x + px(HEADER_W as f32 + 1.), vp.origin.x + vp.size.width - px(1.)),
                    ev.position.y.clamp(vp.origin.y + px(HEADER_H as f32 + 1.), vp.origin.y + vp.size.height - px(1.)),
                );
                if let Some(Hit::Cell(c)) = self.hit_test(p) {
                    self.fill_target = Some(self.fill_target_for(src, c));
                    self.ensure_visible(c);
                }
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

    /// Is a window position on the selection's fill handle?
    pub(crate) fn on_fill_handle(&self, pos: Point<Pixels>) -> bool {
        if self.edit.is_some() {
            return false;
        }
        let sel = self.selection();
        let end = self.wb.layout(self.cur).merge_at(sel.end).map_or(sel.end, |m| m.end);
        let x = f64::from(self.viewport.origin.x) + HEADER_W + self.grid_x(end.col) + self.col_w(end.col);
        let y = f64::from(self.viewport.origin.y) + HEADER_H + self.grid_y(end.row) + self.row_h(end.row);
        (f64::from(pos.x) - x).abs() <= 5.0 && (f64::from(pos.y) - y).abs() <= 5.0
    }

    pub(crate) fn on_mouse_up(&mut self, _ev: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let (Some(Drag::Fill { src }), Some(target)) = (self.drag, self.fill_target.take()) {
            self.fill_series(src, target);
        }
        if let Some((sheet, layout)) = self.layout_before.take()
            && *self.wb.layout(sheet) != layout
        {
            self.push_undo(UndoEntry::Layout { sheet, layout, cells: Vec::new() });
        }
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
        let before = self.wb.layout(self.cur).clone();
        self.autofit_inner(col, window);
        if *self.wb.layout(self.cur) != before {
            self.push_undo(UndoEntry::Layout { sheet: self.cur, layout: before, cells: Vec::new() });
        }
    }

    fn autofit_inner(&mut self, col: u32, window: &mut Window) {
        let r = Range::new(CellRef::new(0, col), CellRef::new(MAX_ROWS - 1, col));
        let mut w: f64 = MIN_COL_W;
        let f = crate::paint::cell_font();
        let cells: Vec<CellRef> = self.wb.store(self.cur).iter_range(r).take(5_000).map(|(c, _)| c).collect();
        for c in cells {
            let s = self.wb.display(self.at(c)).text;
            let line = crate::paint::shape(window, s, f.clone(), self.theme.ink, CELL_FONT);
            w = w.max(f64::from(line.width) + 2.0 * CELL_PAD + 2.0);
        }
        self.wb.set_col_width(self.cur, col, Some(w.min(800.0)));
        self.mark_dirty();
    }

    /// Fit a row to its tallest wrapped or enlarged cell (default height if none).
    pub(crate) fn autofit_row(&mut self, row: u32, window: &mut Window) {
        let h = self.fitted_row_height(row, window);
        self.change_layout(None, |s| s.wb.set_row_height(s.cur, row, if (h - ROW_H).abs() < 0.5 { None } else { Some(h) }));
    }

    /// Height a row needs for its contents: wrapped text and font sizes.
    pub(crate) fn fitted_row_height(&self, row: u32, window: &mut Window) -> f64 {
        let r = Range::new(CellRef::new(row, 0), CellRef::new(row, MAX_COLS - 1));
        let mut h: f64 = ROW_H;
        let cells: Vec<CellRef> = self.wb.store(self.cur).iter_range(r).take(2_000).map(|(c, _)| c).collect();
        for c in cells {
            let st = self.wb.style(self.at(c));
            let size = crate::paint::font_px(st);
            let line_h = crate::paint::line_height(size);
            let lines = if st.wrap {
                let text = self.wb.display(self.at(c)).text;
                crate::paint::wrap_lines(window, &text, crate::paint::cell_font_for(st), size, self.col_w(c.col) - 2.0 * CELL_PAD).len().max(1)
            } else {
                1
            };
            h = h.max(lines as f64 * line_h);
        }
        h.min(400.0)
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

    /// Sum / Average / Count of the selection, as (label, value) pairs.
    pub(crate) fn selection_summary(&self) -> Option<Vec<(&'static str, String)>> {
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
            return Some(vec![("Count", count.to_string())]);
        }
        // Show totals in the active cell's number format when it has one.
        let fmt = self.wb.format(self.at(self.active)).filter(|f| !f.is_general()).cloned();
        let show = |n: f64| match &fmt {
            Some(f) => f.format(&Value::Number(n)).text.trim().to_string(),
            None => xl_engine::format_general(n),
        };
        Some(vec![("Sum", show(sum)), ("Average", show(sum / nums as f64)), ("Count", count.to_string())])
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

/// Offset of line `i` along an axis: `i` default-sized lines, adjusted for
/// custom sizes and hidden (zero-size) lines before it. Lines can be hidden
/// two ways (by hand, by a filter).
fn line_offset(
    i: u32,
    default: f64,
    sizes: &std::collections::BTreeMap<u32, f64>,
    hidden: &std::collections::BTreeSet<u32>,
    also_hidden: &std::collections::BTreeSet<u32>,
) -> f64 {
    let is_hidden = |k: &u32| hidden.contains(k) || also_hidden.contains(k);
    let mut x = i as f64 * default;
    for (k, w) in sizes.range(..i) {
        if !is_hidden(k) {
            x += w - default;
        }
    }
    let count = hidden.range(..i).count() + also_hidden.range(..i).filter(|k| !hidden.contains(k)).count();
    x - count as f64 * default
}

/// The last index in [0, n) whose offset is <= v (offsets are non-decreasing).
fn last_at_or_before(v: f64, n: u32, offset: impl Fn(u32) -> f64) -> u32 {
    if v <= 0.0 {
        return 0;
    }
    let (mut lo, mut hi) = (0u32, n - 1);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if offset(mid) <= v { lo = mid } else { hi = mid - 1 }
    }
    lo
}

/// "$A$1" or "$A$1:$C$9".
pub(crate) fn absolute_range(r: Range) -> String {
    let one = |c: CellRef| format!("${}${}", xl_engine::col_name(c.col), c.row + 1);
    if r.start == r.end { one(r.start) } else { format!("{}:{}", one(r.start), one(r.end)) }
}
