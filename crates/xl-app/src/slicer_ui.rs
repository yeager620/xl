//! Slicers and timelines: floating panels over the grid, anchored to a
//! cell like charts (moved by their title bar, resized by the corner grip).
//! A slicer's buttons filter its pivots (or table) to the chosen items; a
//! timeline filters its pivots to a range of periods dragged across.

use std::collections::BTreeSet;

use gpui::{AnyElement, Context, FontWeight, InteractiveElement, IntoElement, MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled, canvas, div, prelude::FluentBuilder, px};
use xl_engine::pivot::DateLevel;
use xl_engine::slicer::{Slicer, SlicerSource, Timeline, range_label};
use xl_engine::{CellRef, SheetId, SheetLayout};

use crate::app::{Spreadsheet, UndoEntry};

/// Which floating panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Panel {
    Slicer(usize),
    Timeline(usize),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum DragMode {
    /// Where the pointer grabbed the panel, relative to its corner.
    Move { grab: (f64, f64) },
    Resize { start: (f64, f64), size: (f64, f64) },
}

#[derive(Clone, Debug)]
pub(crate) struct PanelDrag {
    pub panel: Panel,
    pub mode: DragMode,
    pub before: SheetLayout,
}

/// A panel's anchor cell, offset in it, and size.
type Place = (CellRef, (f64, f64), (f64, f64));

const SLICER_SIZE: (f64, f64) = (180.0, 230.0);
const TIMELINE_SIZE: (f64, f64) = (420.0, 118.0);

impl Spreadsheet {
    /// Where a new panel goes: right of the active pivot's output (or the
    /// selection), past panels already there.
    fn panel_spot(&self) -> CellRef {
        let l = self.wb.layout(self.cur);
        let right = self.active_pivot().and_then(|i| l.pivots[i].extent).map_or(self.selection().end.col, |e| e.end.col);
        let col = (right + 2 + 2 * (l.slicers.len() + l.timelines.len()) as u32).min(xl_engine::MAX_COLS - 1);
        CellRef::new(self.active_pivot().and_then(|i| l.pivots[i].extent).map_or(self.selection().start.row, |e| e.start.row), col)
    }

    /// Add a slicer on a field of the active pivot.
    pub(crate) fn insert_pivot_slicer(&mut self, field: usize) {
        let Some(i) = self.active_pivot() else {
            self.message = Some(("Select a cell in a pivot table (or a table) to add a slicer".into(), true));
            return;
        };
        let p = &self.wb.layout(self.cur).pivots[i];
        let Some(name) = self.wb.pivot_source_fields(self.cur, p).get(field).cloned() else { return };
        let slicer = Slicer { caption: name.clone(), field: name, source: SlicerSource::Pivots(vec![p.name.clone()]), selected: BTreeSet::new(), anchor: self.panel_spot(), offset: (0.0, 0.0), size: SLICER_SIZE };
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                l.slicers.push(slicer);
            }
        });
    }

    /// Add a timeline on a date field of the active pivot.
    pub(crate) fn insert_pivot_timeline(&mut self, field: usize) {
        let Some(i) = self.active_pivot() else {
            self.message = Some(("Select a cell in a pivot table to add a timeline".into(), true));
            return;
        };
        let p = &self.wb.layout(self.cur).pivots[i];
        let Some(name) = self.wb.pivot_source_fields(self.cur, p).get(field).cloned() else { return };
        let timeline = Timeline { caption: name.clone(), field: name, pivots: vec![p.name.clone()], level: DateLevel::Months, range: None, anchor: self.panel_spot(), offset: (0.0, 0.0), size: TIMELINE_SIZE };
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                l.timelines.push(timeline);
            }
        });
    }

    /// Palette "Insert slicer": on the active cell's column of a table, or
    /// on the first row field (else first field) of the active pivot.
    pub(crate) fn insert_slicer(&mut self) {
        if let Some(i) = self.active_pivot() {
            let p = &self.wb.layout(self.cur).pivots[i];
            let field = p.rows.first().or(p.cols.first()).copied().unwrap_or(0);
            self.insert_pivot_slicer(field);
            return;
        }
        let Some(t) = self.wb.table_at(self.cur, self.active).cloned() else {
            self.message = Some(("Select a cell in a pivot table or a table to add a slicer".into(), true));
            return;
        };
        let header = self.wb.display(self.at(CellRef::new(t.range.start.row, self.active.col))).text;
        if header.is_empty() {
            self.message = Some(("The table column needs a header to make a slicer".into(), true));
            return;
        }
        let anchor = CellRef::new(t.range.start.row, (t.range.end.col + 2 + 2 * self.wb.layout(self.cur).slicers.len() as u32).min(xl_engine::MAX_COLS - 1));
        let slicer = Slicer { caption: header.clone(), field: header, source: SlicerSource::Table(t.name.clone()), selected: BTreeSet::new(), anchor, offset: (0.0, 0.0), size: SLICER_SIZE };
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                l.slicers.push(slicer);
            }
        });
    }

    /// Palette "Insert timeline": on the first date field of the active pivot.
    pub(crate) fn insert_timeline(&mut self) {
        let Some(i) = self.active_pivot() else {
            self.message = Some(("Select a cell in a pivot table to add a timeline".into(), true));
            return;
        };
        let p = &self.wb.layout(self.cur).pivots[i];
        let n = self.wb.pivot_source_fields(self.cur, p).len();
        match (0..n).find(|&f| self.wb.pivot_field_kind(self.cur, p, f) == xl_engine::pivot::FieldKind::Date) {
            Some(f) => self.insert_pivot_timeline(f),
            None => self.message = Some(("The pivot table's data has no dates for a timeline".into(), true)),
        }
    }

    /// Change slicers or timelines on `sheet` and refresh what they filter,
    /// as one undo step.
    pub(crate) fn change_filters(&mut self, sheet: SheetId, f: impl FnOnce(&mut SheetLayout)) {
        let before = self.wb.layout(sheet).clone();
        self.one_undo_step(|s| {
            s.change_layout_on(sheet, None, |s| {
                if let Some(l) = s.wb.layout_mut(sheet) {
                    f(l);
                }
            });
            s.refresh_after_filters(sheet, &before);
        });
    }

    /// Refresh the pivots and tables whose slicers/timelines on `sheet`
    /// differ from `before`.
    fn refresh_after_filters(&mut self, sheet: SheetId, before: &SheetLayout) {
        let now = self.wb.layout(sheet).clone();
        let mut pivots: Vec<String> = Vec::new();
        let mut tables: Vec<String> = Vec::new();
        let slicer_targets = |s: &Slicer, pivots: &mut Vec<String>, tables: &mut Vec<String>| match &s.source {
            SlicerSource::Pivots(p) => pivots.extend(p.iter().cloned()),
            SlicerSource::Table(t) => tables.push(t.clone()),
        };
        for s in now.slicers.iter().filter(|s| !before.slicers.contains(s)).chain(before.slicers.iter().filter(|s| !now.slicers.contains(s))) {
            slicer_targets(s, &mut pivots, &mut tables);
        }
        for t in now.timelines.iter().filter(|t| !before.timelines.contains(t)).chain(before.timelines.iter().filter(|t| !now.timelines.contains(t))) {
            pivots.extend(t.pivots.iter().cloned());
        }
        pivots.sort();
        pivots.dedup();
        tables.sort();
        tables.dedup();
        self.refresh_pivots_named(&pivots);
        for t in tables {
            if let Some((ts, _)) = self.wb.find_table(&t) {
                self.change_layout_on(ts, None, |s| s.wb.apply_table_slicers(&t));
            }
        }
    }

    /// A click on a slicer item: alone it shows just that item (clicking the
    /// only shown item shows all again); with Ctrl it adds or removes it.
    pub(crate) fn click_slicer_item(&mut self, index: usize, item: &str, multi: bool) {
        let sheet = self.cur;
        let item = item.to_string();
        self.change_filters(sheet, |l| {
            let Some(s) = l.slicers.get_mut(index) else { return };
            if multi {
                if !s.selected.remove(&item) {
                    s.selected.insert(item);
                }
            } else if s.selected.len() == 1 && s.selected.contains(&item) {
                s.selected.clear();
            } else {
                s.selected = [item].into();
            }
        });
    }

    pub(crate) fn clear_slicer(&mut self, panel: Panel) {
        let sheet = self.cur;
        self.change_filters(sheet, |l| match panel {
            Panel::Slicer(i) => {
                if let Some(s) = l.slicers.get_mut(i) {
                    s.selected.clear();
                }
            }
            Panel::Timeline(i) => {
                if let Some(t) = l.timelines.get_mut(i) {
                    t.range = None;
                }
            }
        });
    }

    pub(crate) fn delete_panel(&mut self, panel: Panel) {
        let sheet = self.cur;
        self.change_filters(sheet, |l| match panel {
            Panel::Slicer(i) if i < l.slicers.len() => {
                l.slicers.remove(i);
            }
            Panel::Timeline(i) if i < l.timelines.len() => {
                l.timelines.remove(i);
            }
            _ => {}
        });
    }

    /// Select periods `a` through `b` (in either order) of a timeline.
    pub(crate) fn set_timeline_range(&mut self, index: usize, a: usize, b: usize, undoable: bool) {
        let sheet = self.cur;
        let Some(t) = self.wb.layout(sheet).timelines.get(index) else { return };
        let periods = self.wb.timeline_periods(t);
        let (lo, hi) = (a.min(b), a.max(b));
        let (Some(first), Some(last)) = (periods.get(lo), periods.get(hi)) else { return };
        let range = Some((first.1, last.2));
        if undoable {
            self.change_filters(sheet, |l| {
                if let Some(t) = l.timelines.get_mut(index) {
                    t.range = range;
                }
            });
        } else if t.range != range {
            // While dragging: show it live, undo it as one step at the end.
            if let Some(t) = self.wb.layout_mut(sheet).and_then(|l| l.timelines.get_mut(index)) {
                t.range = range;
            }
            let names = self.wb.layout(sheet).timelines[index].pivots.clone();
            for n in names {
                if let Some((ps, pi)) = self.wb.find_pivot(&n) {
                    let keep = self.undo.len();
                    self.write_pivot(ps, pi);
                    self.undo.truncate(keep);
                }
            }
        }
    }

    pub(crate) fn cycle_timeline_level(&mut self, index: usize) {
        let sheet = self.cur;
        self.change_filters(sheet, |l| {
            if let Some(t) = l.timelines.get_mut(index) {
                t.level = match t.level {
                    DateLevel::Years => DateLevel::Quarters,
                    DateLevel::Quarters => DateLevel::Months,
                    DateLevel::Months => DateLevel::Days,
                    DateLevel::Days => DateLevel::Years,
                };
                // Keep the selection whole periods at the new level.
                if let Some((a, b)) = t.range {
                    let start = xl_engine::slicer::period_start(a, t.level);
                    let mut end = xl_engine::slicer::period_start(b - 0.5, t.level);
                    end = xl_engine::slicer::period_next(end, t.level);
                    t.range = Some((start, end));
                }
            }
        });
    }

    /// The end of a timeline drag: one undo step for the whole drag.
    fn finish_timeline_drag(&mut self) {
        self.pivot_ui.timeline_drag = None;
        let Some(before) = self.pivot_ui.timeline_before.take() else { return };
        let sheet = self.cur;
        if *self.wb.layout(sheet) == before {
            return;
        }
        // Undo restores the timeline and rewrites the pivots as they were.
        let after = self.wb.layout(sheet).timelines.clone();
        if let Some(l) = self.wb.layout_mut(sheet) {
            l.timelines = before.timelines.clone();
        }
        let names: Vec<String> = after.iter().flat_map(|t| t.pivots.clone()).collect();
        for n in &names {
            if let Some((ps, pi)) = self.wb.find_pivot(n) {
                let keep = self.undo.len();
                self.write_pivot(ps, pi);
                self.undo.truncate(keep);
            }
        }
        self.change_filters(sheet, move |l| l.timelines = after);
    }

    /// Screen rectangle of a panel, relative to the cell area's top-left.
    fn panel_rect(&self, anchor: CellRef, offset: (f64, f64), size: (f64, f64)) -> (f64, f64, f64, f64) {
        (self.grid_x(anchor.col) + offset.0, self.grid_y(anchor.row) + offset.1, size.0, size.1)
    }

    fn panel_place(&self, panel: Panel) -> Option<Place> {
        let l = self.wb.layout(self.cur);
        match panel {
            Panel::Slicer(i) => l.slicers.get(i).map(|s| (s.anchor, s.offset, s.size)),
            Panel::Timeline(i) => l.timelines.get(i).map(|t| (t.anchor, t.offset, t.size)),
        }
    }

    fn set_panel_place(&mut self, panel: Panel, anchor: CellRef, offset: (f64, f64), size: (f64, f64)) {
        let Some(l) = self.wb.layout_mut(self.cur) else { return };
        match panel {
            Panel::Slicer(i) => {
                if let Some(s) = l.slicers.get_mut(i) {
                    (s.anchor, s.offset, s.size) = (anchor, offset, size);
                }
            }
            Panel::Timeline(i) => {
                if let Some(t) = l.timelines.get_mut(i) {
                    (t.anchor, t.offset, t.size) = (anchor, offset, size);
                }
            }
        }
    }

    fn start_panel_drag(&mut self, panel: Panel, pos: gpui::Point<gpui::Pixels>, resize: bool) {
        let Some((anchor, offset, size)) = self.panel_place(panel) else { return };
        let (x, y, _, _) = self.panel_rect(anchor, offset, size);
        let (px_, py_) = (f64::from(pos.x) - f64::from(self.viewport.origin.x) - self.hw(), f64::from(pos.y) - f64::from(self.viewport.origin.y) - self.hh());
        let mode = if resize { DragMode::Resize { start: (f64::from(pos.x), f64::from(pos.y)), size } } else { DragMode::Move { grab: (px_ - x, py_ - y) } };
        self.pivot_ui.drag = Some(PanelDrag { panel, mode, before: self.wb.layout(self.cur).clone() });
    }

    fn drag_panel_to(&mut self, pos: gpui::Point<gpui::Pixels>) {
        let Some(d) = self.pivot_ui.drag.clone() else { return };
        let Some((anchor, offset, size)) = self.panel_place(d.panel) else { return };
        match d.mode {
            DragMode::Move { grab } => {
                // chart_anchor_at takes window coordinates of the corner.
                let (a, o) = self.chart_anchor_at(f64::from(pos.x) - grab.0, f64::from(pos.y) - grab.1);
                self.set_panel_place(d.panel, a, o, size);
            }
            DragMode::Resize { start, size: s0 } => {
                let min = if matches!(d.panel, Panel::Timeline(_)) { (240.0, 96.0) } else { (110.0, 90.0) };
                let w = (s0.0 + f64::from(pos.x) - start.0).max(min.0);
                let h = (s0.1 + f64::from(pos.y) - start.1).max(min.1);
                self.set_panel_place(d.panel, anchor, offset, (w, h));
            }
        }
    }

    fn end_panel_drag(&mut self) {
        if let Some(d) = self.pivot_ui.drag.take()
            && *self.wb.layout(self.cur) != d.before
        {
            self.push_undo(UndoEntry::Layout { sheet: self.cur, layout: d.before, cells: Vec::new() });
            self.mark_dirty();
        }
    }

    /// The slicers and timelines on the current sheet, over the cell area.
    pub(crate) fn render_floating_filters(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let l = self.wb.layout(self.cur);
        if l.slicers.is_empty() && l.timelines.is_empty() {
            return None;
        }
        let mut layer = div().absolute().left(px(self.hw() as f32)).top(px(self.hh() as f32)).right_0().bottom_0().overflow_hidden();
        for (i, s) in l.slicers.iter().enumerate() {
            layer = layer.child(self.render_slicer(i, s, cx));
        }
        for (i, t) in l.timelines.iter().enumerate() {
            layer = layer.child(self.render_timeline(i, t, cx));
        }
        // While dragging, follow the pointer anywhere in the window.
        if self.pivot_ui.drag.is_some() || self.pivot_ui.timeline_drag.is_some() {
            let this = cx.entity();
            layer = layer.child(
                canvas(
                    |_, _, _| {},
                    move |_, _, window, _| {
                        let e = this.clone();
                        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                            if phase == gpui::DispatchPhase::Bubble {
                                e.update(cx, |s, cx| {
                                    if s.pivot_ui.drag.is_some() {
                                        s.drag_panel_to(ev.position);
                                        cx.notify();
                                    }
                                });
                            }
                        });
                        let e = this.clone();
                        window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                            if phase == gpui::DispatchPhase::Bubble {
                                e.update(cx, |s, cx| {
                                    s.end_panel_drag();
                                    if s.pivot_ui.timeline_drag.is_some() {
                                        s.finish_timeline_drag();
                                    }
                                    cx.notify();
                                });
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            );
        }
        Some(layer.into_any_element())
    }

    /// A panel's frame: title bar (drag to move) with buttons, body, resize grip.
    #[allow(clippy::too_many_arguments)]
    fn panel_frame(&self, panel: Panel, id: usize, title: SharedString, subtitle: Option<SharedString>, filtered: bool, rect: (f64, f64, f64, f64), body: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let base = match panel {
            Panel::Slicer(_) => 0,
            Panel::Timeline(_) => 1_000_000,
        } + id * 10;
        let icon_button = |n: usize, glyph: &'static str, tip: &'static str| {
            div()
                .id(("panel-btn", base + n))
                .size(px(20.))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(t.ink_muted)
                .hover(|s| s.bg(t.chrome_hover))
                .tooltip(crate::chrome::tip(t, tip, ""))
                .child(glyph)
        };
        let kind = match panel {
            Panel::Slicer(_) => "slicer",
            Panel::Timeline(_) => "timeline",
        };
        div()
            .id(SharedString::from(format!("{kind}-{id}")))
            .debug_selector(|| format!("{kind}-{id}"))
            .absolute()
            .left(px(rect.0 as f32))
            .top(px(rect.1 as f32))
            .w(px(rect.2 as f32))
            .h(px(rect.3 as f32))
            .flex()
            .flex_col()
            .rounded(px(5.))
            .bg(t.popover)
            .border_1()
            .border_color(t.divider)
            .shadow_md()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id(("panel-title", base))
                    .debug_selector(|| format!("{kind}-{id}-title"))
                    .h(px(28.))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.))
                    .pl(px(8.))
                    .pr(px(4.))
                    .cursor_move()
                    .border_b_1()
                    .border_color(t.divider)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                            this.start_panel_drag(panel, ev.position, false);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(div().flex_1().overflow_hidden().whitespace_nowrap().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child(title))
                    .children(subtitle.map(|s| div().flex_none().text_size(px(11.5)).text_color(t.ink_muted).child(s)))
                    .child(
                        icon_button(1, "⨯", "Clear filter")
                            .when(!filtered, |d| d.text_color(t.ink_faint))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.clear_slicer(panel);
                                cx.notify();
                            })),
                    )
                    .child(icon_button(2, "🗑", "Remove").on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()).on_click(cx.listener(move |this, _, _, cx| {
                        this.delete_panel(panel);
                        cx.notify();
                    }))),
            )
            .child(div().flex_1().min_h(px(0.)).child(body))
            .child(
                div()
                    .id(("panel-grip", base))
                    .absolute()
                    .right_0()
                    .bottom_0()
                    .size(px(12.))
                    .cursor(gpui::CursorStyle::ResizeUpLeftDownRight)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                            this.start_panel_drag(panel, ev.position, true);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(div().absolute().right(px(3.)).bottom(px(3.)).size(px(5.)).border_r_1().border_b_1().border_color(t.ink_faint)),
            )
            .into_any_element()
    }

    fn render_slicer(&self, index: usize, s: &Slicer, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let items = self.wb.slicer_items(s);
        let all = s.selected.is_empty();
        let mut list = div().id(("slicer-items", index)).size_full().p(px(6.)).flex().flex_col().gap(px(4.)).overflow_y_scroll();
        for (j, item) in items.iter().enumerate().take(1000) {
            let on = all || s.selected.contains(item);
            let shown = if item.is_empty() { "(blank)".to_string() } else { item.clone() };
            let item = item.clone();
            list = list.child(
                div()
                    .id(("slicer-item", index * 10_000 + j))
                    .debug_selector(|| format!("slicer-{index}-item-{j}"))
                    .h(px(26.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(px(8.))
                    .rounded(px(3.))
                    .cursor_pointer()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(12.5))
                    .border_1()
                    .when(on, |d| d.bg(t.header_selected).border_color(t.accent).text_color(t.ink))
                    .when(!on, |d| d.border_color(t.divider).text_color(t.ink_faint))
                    .hover(|st| st.bg(t.chrome_hover))
                    .child(SharedString::from(shown))
                    .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, _, cx| {
                        let m = ev.modifiers();
                        this.click_slicer_item(index, &item, m.control || m.platform || m.shift);
                        cx.notify();
                    })),
            );
        }
        let subtitle = (!all).then(|| SharedString::from(format!("{} of {}", s.selected.len(), items.len())));
        let (anchor, offset, size) = (s.anchor, s.offset, s.size);
        self.panel_frame(Panel::Slicer(index), index, s.caption.clone().into(), subtitle, !all, self.panel_rect(anchor, offset, size), list.into_any_element(), cx)
    }

    fn render_timeline(&self, index: usize, tl: &Timeline, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let periods = self.wb.timeline_periods(tl);
        let level = tl.level;
        let selected = |a: f64, b: f64| tl.range.is_none_or(|(x, y)| a >= x && b <= y);
        let mut strip = div().id(("timeline-strip", index)).flex().flex_row().gap(px(2.)).px(px(6.)).pb(px(4.)).overflow_x_scroll();
        for (j, (label, a, b)) in periods.iter().enumerate().take(2000) {
            let on = selected(*a, *b);
            let short = match level {
                DateLevel::Years => label.clone(),
                _ => label.split(' ').take(if level == DateLevel::Days { 2 } else { 1 }).collect::<Vec<_>>().join(" "),
            };
            // A year label over the first period of each year.
            let year = label.rsplit(' ').next().unwrap_or_default().to_string();
            let first_of_year = level != DateLevel::Years && (j == 0 || periods[j - 1].0.rsplit(' ').next() != Some(year.as_str()));
            strip = strip.child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .gap(px(2.))
                    .child(div().h(px(14.)).text_size(px(10.5)).text_color(t.ink_muted).when(first_of_year, |d| d.child(SharedString::from(year.clone()))))
                    .child(
                        div()
                            .id(("timeline-period", index * 10_000 + j))
                            .debug_selector(|| format!("timeline-{index}-period-{j}"))
                            .w(px(if level == DateLevel::Years { 52. } else { 40. }))
                            .h(px(26.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.))
                            .cursor_pointer()
                            .text_size(px(11.5))
                            .when(on, |d| d.bg(t.accent).text_color(t.on_accent))
                            .when(!on, |d| d.bg(t.chrome).text_color(t.ink_muted))
                            .child(SharedString::from(short))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    if ev.modifiers.shift
                                        && let Some(first) = this.timeline_first_selected(index)
                                    {
                                        this.set_timeline_range(index, first, j, true);
                                    } else {
                                        this.pivot_ui.timeline_before = Some(this.wb.layout(this.cur).clone());
                                        this.pivot_ui.timeline_drag = Some((this.cur, index, j));
                                        this.set_timeline_range(index, j, j, false);
                                    }
                                    cx.notify();
                                }),
                            )
                            .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
                                if let Some((sheet, ti, first)) = this.pivot_ui.timeline_drag
                                    && sheet == this.cur
                                    && ti == index
                                {
                                    this.set_timeline_range(index, first, j, false);
                                    cx.notify();
                                }
                            })),
                    ),
            );
        }
        let caption = match tl.range {
            Some(r) => range_label(r, level),
            None => "All periods".to_string(),
        };
        let body = div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px(px(8.))
                    .pt(px(4.))
                    .gap(px(6.))
                    .child(div().flex_1().overflow_hidden().whitespace_nowrap().text_size(px(12.)).text_color(t.ink).child(SharedString::from(caption)))
                    .child(
                        div()
                            .id(("timeline-level", index))
                            .px(px(6.))
                            .h(px(18.))
                            .flex()
                            .items_center()
                            .rounded(px(3.))
                            .cursor_pointer()
                            .text_size(px(11.5))
                            .text_color(t.accent)
                            .hover(|s| s.bg(t.chrome_hover))
                            .tooltip(crate::chrome::tip(t, "Click to change the time level", ""))
                            .child(SharedString::from(format!("{} ▾", level.label().to_uppercase())))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.cycle_timeline_level(index);
                                cx.notify();
                            })),
                    ),
            )
            .child(strip);
        let (anchor, offset, size) = (tl.anchor, tl.offset, tl.size);
        self.panel_frame(Panel::Timeline(index), index, tl.caption.clone().into(), None, tl.range.is_some(), self.panel_rect(anchor, offset, size), body.into_any_element(), cx)
    }

    /// The first selected period of a timeline, if it has a selection.
    fn timeline_first_selected(&self, index: usize) -> Option<usize> {
        let t = self.wb.layout(self.cur).timelines.get(index)?;
        let (a, _) = t.range?;
        self.wb.timeline_periods(t).iter().position(|p| p.1 >= a)
    }
}
