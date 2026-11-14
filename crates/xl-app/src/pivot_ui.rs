//! Pivot tables in the app: inserting one on a new sheet, the field panel,
//! and writing (refreshing) the output cells.

use std::sync::Arc;

use gpui::{AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px};
use xl_engine::formula::parse_formula;
use xl_engine::pivot::{Agg, Pivot};
use xl_engine::{Addr, CellRef, CellSnapshot, Range, SheetId, Style, Value, quote_sheet_name};

use crate::app::{Spreadsheet, absolute_range, value_input};

/// Where a field is used in a pivot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Area {
    Rows,
    Cols,
    Values,
}

impl Spreadsheet {
    /// The pivot whose output contains the active cell, on the current sheet.
    pub(crate) fn active_pivot(&self) -> Option<usize> {
        self.wb.layout(self.cur).pivots.iter().position(|p| p.extent.is_some_and(|e| e.contains(self.active)) || p.anchor == self.active)
    }

    /// Make a pivot table of the data around the selection, on a new sheet.
    pub(crate) fn insert_pivot(&mut self) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let Some(range) = self.data_range() else {
            self.message = Some(("Select a table of data (with a header row) to summarize".into(), true));
            return;
        };
        if range.rows() < 2 {
            self.message = Some(("A pivot table needs a header row and some data".into(), true));
            return;
        }
        let src_sheet = self.cur;
        let source = format!("{}!{}", quote_sheet_name(self.wb.sheet_name(src_sheet)), absolute_range(range));
        let Ok(expr) = parse_formula(&source, self.wb.sheets()) else { return };
        // Sensible defaults: rows by the first text column, sum of the first number column.
        let store = self.wb.store(src_sheet);
        let numeric = |c: u32| (range.start.row + 1..=range.end.row).any(|r| matches!(store.value(CellRef::new(r, c)), Value::Number(_)));
        let text_col = (range.start.col..=range.end.col).find(|&c| !numeric(c)).map(|c| (c - range.start.col) as usize);
        let num_col = (range.start.col..=range.end.col).rev().find(|&c| numeric(c)).map(|c| (c - range.start.col) as usize);
        let pivot = Pivot { source: Arc::new(expr), anchor: CellRef::new(0, 0), rows: text_col.into_iter().collect(), cols: Vec::new(), values: num_col.map(|c| (c, Agg::Sum)).into_iter().collect(), extent: None };
        let name = (1..).map(|i| format!("Pivot {i}")).find(|n| self.wb.sheet_id(n).is_none()).unwrap_or_default();
        let Ok(sheet) = self.wb.add_sheet(Some(&name)) else { return };
        self.switch_sheet(sheet);
        if let Some(l) = self.wb.layout_mut(sheet) {
            l.pivots.push(pivot);
        }
        // Creating the sheet isn't undoable; filling it in is.
        self.write_pivot(sheet, 0);
        self.wb.set_col_width(sheet, 0, Some(140.0));
        self.select(CellRef::new(0, 0), false);
        self.mark_dirty();
        self.message = Some((format!("Created {name} — choose fields in the panel on the right"), false));
    }

    /// Recompute a pivot and rewrite its output cells (one undo step together
    /// with whatever change led here).
    pub(crate) fn write_pivot(&mut self, sheet: SheetId, index: usize) {
        let Some(p) = self.wb.layout(sheet).pivots.get(index).cloned() else { return };
        let grid = self.wb.pivot_grid(sheet, &p);
        let rows = grid.len() as u32;
        let cols = grid.iter().map(|r| r.len()).max().unwrap_or(1) as u32;
        let a = p.anchor;
        let extent = Range::new(a, CellRef::new(a.row + rows.max(1) - 1, a.col + cols.max(1) - 1));
        // Value columns keep the source's number format.
        let formats: Vec<Option<String>> = p
            .values
            .iter()
            .map(|(f, agg)| {
                let (src_sheet, r) = p.source.reference()?;
                let code = self.wb.format_code(Addr::new(src_sheet.unwrap_or(sheet), CellRef::new(r.start.row + 1, r.start.col + *f as u32)))?;
                (*agg != Agg::Count).then(|| code.to_string())
            })
            .collect();
        let label_cols = p.rows.len().max(1);
        let mut items: Vec<(Addr, CellSnapshot)> = Vec::new();
        if let Some(old) = p.extent {
            for c in old.iter() {
                items.push((Addr::new(sheet, c), CellSnapshot::default()));
            }
        }
        let bold = Style { bold: true, ..Style::default() };
        let total_row = !p.rows.is_empty();
        for (i, line) in grid.iter().enumerate() {
            for (j, v) in line.iter().enumerate() {
                let cell = CellRef::new(a.row + i as u32, a.col + j as u32);
                let heading = i == 0 || (total_row && i + 1 == grid.len());
                let format = (j >= label_cols && i > 0).then(|| formats.get((j - label_cols) % formats.len().max(1)).cloned().flatten()).flatten();
                let snap = CellSnapshot { input: value_input(v), format, style: if heading { bold.clone() } else { Style::default() } };
                items.retain(|(x, _)| x.cell != cell);
                items.push((Addr::new(sheet, cell), snap));
            }
        }
        let touched = p.extent.map_or(extent, |old| Range::new(CellRef::new(old.start.row.min(extent.start.row), old.start.col.min(extent.start.col)), CellRef::new(old.end.row.max(extent.end.row), old.end.col.max(extent.end.col))));
        let keep = self.cur;
        self.cur = sheet;
        self.change_layout(Some(touched), |s| {
            s.last_stats = Some(s.wb.restore(items));
            if let Some(p) = s.wb.layout_mut(sheet).and_then(|l| l.pivots.get_mut(index)) {
                p.extent = Some(extent);
            }
        });
        self.cur = keep;
    }

    /// Put a field in an area of the active pivot, or take it out.
    pub(crate) fn toggle_pivot_field(&mut self, field: usize, area: Area) {
        let Some(i) = self.active_pivot() else { return };
        let sheet = self.cur;
        if let Some(p) = self.wb.layout_mut(sheet).and_then(|l| l.pivots.get_mut(i)) {
            match area {
                Area::Rows | Area::Cols => {
                    let list = if area == Area::Rows { &mut p.rows } else { &mut p.cols };
                    if let Some(pos) = list.iter().position(|&f| f == field) {
                        list.remove(pos);
                    } else {
                        list.push(field);
                        // A field is a row or a column, not both.
                        let other = if area == Area::Rows { &mut p.cols } else { &mut p.rows };
                        other.retain(|&f| f != field);
                    }
                }
                Area::Values => {
                    if let Some(pos) = p.values.iter().position(|(f, _)| *f == field) {
                        p.values.remove(pos);
                    } else {
                        p.values.push((field, Agg::Sum));
                    }
                }
            }
        }
        self.write_pivot(sheet, i);
    }

    /// Cycle a value field's summary (Sum → Count → Average → Min → Max).
    pub(crate) fn cycle_pivot_agg(&mut self, field: usize) {
        let Some(i) = self.active_pivot() else { return };
        let sheet = self.cur;
        if let Some(p) = self.wb.layout_mut(sheet).and_then(|l| l.pivots.get_mut(i))
            && let Some(v) = p.values.iter_mut().find(|(f, _)| *f == field)
        {
            v.1 = v.1.next();
        }
        self.write_pivot(sheet, i);
    }

    pub(crate) fn refresh_pivot(&mut self) {
        match self.active_pivot() {
            Some(i) => self.write_pivot(self.cur, i),
            None => self.message = Some(("Select a cell in a pivot table to refresh it".into(), true)),
        }
    }

    /// After cells change, refresh pivots summarizing them (on any sheet).
    pub(crate) fn refresh_pivots_using(&mut self, sheet: SheetId, cells: &[CellRef]) {
        let mut todo = Vec::new();
        for &id in self.wb.sheet_ids() {
            for (i, p) in self.wb.layout(id).pivots.iter().enumerate() {
                if let Some((s, r)) = p.source.reference()
                    && s.unwrap_or(id) == sheet
                    && cells.iter().any(|c| r.contains(*c))
                {
                    todo.push((id, i));
                }
            }
        }
        if todo.is_empty() {
            return;
        }
        // The edit and the refreshes it caused undo as one step.
        let first = self.undo.len().saturating_sub(1);
        for (id, i) in todo {
            self.write_pivot(id, i);
        }
        if self.undo.len() > first + 1 {
            let steps: Vec<_> = self.undo.drain(first..).collect();
            self.undo.push(crate::app::UndoEntry::Group(steps));
        }
    }

    pub(crate) fn render_pivot_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let i = self.active_pivot()?;
        let p = self.wb.layout(self.cur).pivots.get(i)?;
        let t = self.theme;
        let fields = self.wb.pivot_fields(self.cur, p);
        let chip = |id: (&'static str, usize), label: &'static str, on: bool| {
            div()
                .id(id)
                .px(px(6.))
                .h(px(20.))
                .flex()
                .items_center()
                .rounded(px(3.))
                .cursor_pointer()
                .text_size(px(12.))
                .border_1()
                .border_color(if on { t.accent } else { t.divider })
                .when(on, |d| d.bg(t.accent).text_color(t.on_accent))
                .when(!on, |d| d.text_color(t.ink_muted).hover(|s| s.bg(t.chrome_hover)))
                .child(label)
        };
        let mut list = div().flex().flex_col().gap(px(2.));
        for (f, name) in fields.iter().enumerate() {
            let in_rows = p.rows.contains(&f);
            let in_cols = p.cols.contains(&f);
            let value = p.values.iter().find(|(x, _)| *x == f).map(|(_, a)| *a);
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .py(px(3.))
                .child(div().flex_1().overflow_hidden().whitespace_nowrap().text_size(px(13.5)).text_color(t.ink).child(SharedString::from(name.clone())))
                .child(chip(("pv-row", f), "Row", in_rows).on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_pivot_field(f, Area::Rows);
                    cx.notify();
                })))
                .child(chip(("pv-col", f), "Col", in_cols).on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_pivot_field(f, Area::Cols);
                    cx.notify();
                })))
                .child(chip(("pv-val", f), "Value", value.is_some()).on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_pivot_field(f, Area::Values);
                    cx.notify();
                })));
            if let Some(agg) = value {
                row = row.child(
                    div()
                        .id(("pv-agg", f))
                        .w(px(58.))
                        .h(px(20.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(3.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(t.accent)
                        .hover(|s| s.bg(t.chrome_hover))
                        .tooltip(crate::chrome::tip(t, "Click to change how values are summarized", ""))
                        .child(agg.label())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.cycle_pivot_agg(f);
                            cx.notify();
                        })),
                );
            }
            list = list.child(row);
        }
        Some(
            div()
                .w(px(300.))
                .h_full()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(10.))
                .p(px(12.))
                .bg(t.chrome)
                .border_l_1()
                .border_color(t.divider)
                .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child("Pivot table"))
                .child(div().text_size(px(12.5)).text_color(t.ink_muted).child("Pick fields to group rows and columns by, and values to summarize. Updates as the source data changes."))
                .child(div().id("pv-fields").flex_1().overflow_y_scroll().child(list))
                .child(
                    div()
                        .id("pv-refresh")
                        .h(px(30.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.))
                        .cursor_pointer()
                        .border_1()
                        .border_color(t.divider)
                        .text_size(px(13.5))
                        .hover(|s| s.bg(t.chrome_hover))
                        .child("Refresh")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.refresh_pivot();
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }
}
