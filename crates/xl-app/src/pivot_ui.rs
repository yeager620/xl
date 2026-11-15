//! Pivot tables in the app: inserting one on a new sheet, the field panel
//! (areas, summaries, "show values as", sorting, grouping, filters,
//! calculated fields), writing (refreshing) the output cells, and pivot
//! charts. Slicers and timelines are in `slicer_ui.rs`.

use std::collections::BTreeSet;
use std::sync::Arc;

use gpui::{AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px};
use xl_engine::chart::{Chart, ChartKind, ChartSeries};
use xl_engine::formula::parse_formula;
use xl_engine::pivot::{Agg, CalcExpr, CalcField, DateLevel, FieldKind, FieldSort, Grouping, LabelFilter, LabelOp, Pivot, PivotOutput, TopFilter, ValueField, ValueFilter, ValueOp};
use xl_engine::{Addr, CellRef, CellSnapshot, Range, SheetId, SheetLayout, Style, Value, quote_sheet_name};

use crate::app::{Field, Spreadsheet, UndoEntry, absolute_range, value_input};
use crate::solver_ui::{FormDialog, FormKind, SolverGoalKind};

/// Where a field is used in a pivot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Area {
    Rows,
    Cols,
    Values,
}

/// The small forms the field panel opens (in the shared form dialog).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PivotForm {
    CalcField,
    /// Group a number field into ranges.
    Ranges(usize),
    /// Name a group for the items selected in the panel.
    Group(usize),
    LabelFilter(usize),
    ValueFilter(usize),
    Top(usize),
}

/// Panel and panel-drag state for pivots, slicers and timelines.
#[derive(Default)]
pub(crate) struct PivotUi {
    /// The field whose settings are expanded in the panel.
    pub open_field: Option<usize>,
    /// Items picked in the panel (for grouping), as (field, label).
    pub picked: BTreeSet<(usize, String)>,
    /// A slicer or timeline being moved or resized.
    pub drag: Option<crate::slicer_ui::PanelDrag>,
    /// Dragging across a timeline's periods: (sheet, timeline, first period).
    pub timeline_drag: Option<(SheetId, usize, usize)>,
    /// The layout before a timeline drag, to undo it in one step.
    pub timeline_before: Option<SheetLayout>,
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
        let mut pivot = Pivot::new(Arc::new(expr), CellRef::new(0, 0));
        pivot.name = self.wb.free_pivot_name();
        pivot.rows = text_col.into_iter().collect();
        pivot.values = num_col.map(|c| ValueField::new(c, Agg::Sum)).into_iter().collect();
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

    /// `change_layout` for a sheet other than the current one.
    pub(crate) fn change_layout_on(&mut self, sheet: SheetId, touched: Option<Range>, f: impl FnOnce(&mut Self)) {
        let keep = self.cur;
        self.cur = sheet;
        self.change_layout(touched, f);
        self.cur = keep;
    }

    /// Run `f`, folding the undo steps it pushes into one.
    pub(crate) fn one_undo_step(&mut self, f: impl FnOnce(&mut Self)) {
        let first = self.undo.len();
        f(self);
        if self.undo.len() > first + 1 {
            let steps: Vec<_> = self.undo.drain(first..).collect();
            self.undo.push(UndoEntry::Group(steps));
        }
    }

    /// Recompute a pivot and rewrite its output cells and its pivot charts
    /// (one undo step together with whatever change led here).
    pub(crate) fn write_pivot(&mut self, sheet: SheetId, index: usize) {
        let Some(p) = self.wb.layout(sheet).pivots.get(index).cloned() else { return };
        let out = self.wb.pivot_table(sheet, &p);
        let grid = &out.grid;
        let rows = grid.len() as u32;
        let cols = grid.iter().map(|r| r.len()).max().unwrap_or(1) as u32;
        let a = p.anchor;
        let extent = Range::new(a, CellRef::new(a.row + rows.max(1) - 1, a.col + cols.max(1) - 1));
        // Value columns keep the source's number format, unless shown as a percentage or rank.
        let nsrc = self.wb.pivot_source_fields(sheet, &p).len();
        let source_format = |vi: usize| -> Option<String> {
            let v = p.values.get(vi)?;
            let (src_sheet, r) = p.source.reference()?;
            if v.agg == Agg::Count || v.field >= nsrc {
                return None;
            }
            self.wb.format_code(Addr::new(src_sheet.unwrap_or(sheet), CellRef::new(r.start.row + 1, r.start.col + v.field as u32))).map(str::to_string)
        };
        let formats: Vec<Option<String>> = (0..cols as usize).map(|j| out.col_formats.get(j).copied().flatten().map(str::to_string).or_else(|| out.col_values.get(j).copied().flatten().and_then(source_format))).collect();
        let mut items: Vec<(Addr, CellSnapshot)> = Vec::new();
        if let Some(old) = p.extent {
            for c in old.iter() {
                items.push((Addr::new(sheet, c), CellSnapshot::default()));
            }
        }
        let bold = Style { bold: true, ..Style::default() };
        for (i, line) in grid.iter().enumerate() {
            for (j, v) in line.iter().enumerate() {
                let cell = CellRef::new(a.row + i as u32, a.col + j as u32);
                let heading = out.bold_rows.contains(&i);
                let format = (i > 0).then(|| formats.get(j).cloned().flatten()).flatten();
                let snap = CellSnapshot { input: value_input(v), format, style: if heading { bold.clone() } else { Style::default() } };
                items.retain(|(x, _)| x.cell != cell);
                items.push((Addr::new(sheet, cell), snap));
            }
        }
        let touched = p.extent.map_or(extent, |old| Range::new(CellRef::new(old.start.row.min(extent.start.row), old.start.col.min(extent.start.col)), CellRef::new(old.end.row.max(extent.end.row), old.end.col.max(extent.end.col))));
        self.one_undo_step(|s| {
            s.change_layout_on(sheet, Some(touched), |s| {
                s.last_stats = Some(s.wb.restore(items));
                if let Some(p) = s.wb.layout_mut(sheet).and_then(|l| l.pivots.get_mut(index)) {
                    p.extent = Some(extent);
                }
            });
            s.refresh_pivot_charts(sheet, &p, &out, extent);
        });
    }

    /// Point the charts bound to a pivot at its new output.
    fn refresh_pivot_charts(&mut self, sheet: SheetId, p: &Pivot, out: &PivotOutput, extent: Range) {
        let series = self.pivot_chart_series(sheet, out, extent);
        let targets: Vec<SheetId> = self.wb.sheet_ids().iter().copied().filter(|&id| self.wb.layout(id).charts.iter().any(|c| c.pivot.as_deref() == Some(p.name.as_str()) && c.series != series)).collect();
        for id in targets {
            let series = series.clone();
            let name = p.name.clone();
            self.change_layout_on(id, None, move |s| {
                if let Some(l) = s.wb.layout_mut(id) {
                    for c in l.charts.iter_mut().filter(|c| c.pivot.as_deref() == Some(name.as_str())) {
                        c.series = series.clone();
                    }
                }
            });
        }
    }

    /// A pivot chart's series: one per plotted column, over the item rows.
    fn pivot_chart_series(&self, sheet: SheetId, out: &PivotOutput, extent: Range) -> Vec<ChartSeries> {
        let Some((first, last)) = out.body else { return Vec::new() };
        let name = quote_sheet_name(self.wb.sheet_name(sheet));
        let a = extent.start;
        let reference = |r: Range| parse_formula(&format!("{name}!{}", absolute_range(r)), self.wb.sheets()).ok().map(Arc::new);
        let column = |c: usize| Range::new(CellRef::new(a.row + first as u32, a.col + c as u32), CellRef::new(a.row + last as u32, a.col + c as u32));
        let categories = reference(column(out.label_cols.saturating_sub(1)));
        out.series_cols
            .iter()
            .filter_map(|&c| Some(ChartSeries { name: reference(Range::single(CellRef::new(a.row, a.col + c as u32))), categories: categories.clone(), values: reference(column(c))? }))
            .collect()
    }

    /// Change the active pivot and rewrite it.
    pub(crate) fn update_pivot(&mut self, f: impl FnOnce(&mut Pivot)) {
        let Some(i) = self.active_pivot() else {
            self.message = Some(("Select a cell in a pivot table first".into(), true));
            return;
        };
        let sheet = self.cur;
        let before = self.wb.layout(sheet).pivots[i].clone();
        let mut p = before.clone();
        f(&mut p);
        if p == before {
            return;
        }
        // The definition change and the new output undo together.
        self.one_undo_step(|s| {
            s.change_layout(None, |s| {
                if let Some(slot) = s.wb.layout_mut(sheet).and_then(|l| l.pivots.get_mut(i)) {
                    *slot = p;
                }
            });
            s.write_pivot(sheet, i);
        });
    }

    /// Put a field in an area of the active pivot, or take it out.
    pub(crate) fn toggle_pivot_field(&mut self, field: usize, area: Area) {
        self.update_pivot(|p| match area {
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
                if let Some(pos) = p.values.iter().position(|v| v.field == field) {
                    p.values.remove(pos);
                    // Filters and sorts that used later value fields shift down.
                    for flt in p.filters.values_mut() {
                        flt.value = flt.value.take().filter(|v| v.value != pos).map(|mut v| {
                            v.value -= (v.value > pos) as usize;
                            v
                        });
                        flt.top = flt.top.take().filter(|t| t.value != pos).map(|mut t| {
                            t.value -= (t.value > pos) as usize;
                            t
                        });
                    }
                    p.sorts.retain(|_, s| !matches!(s, FieldSort::ByValue { value, .. } if *value == pos));
                    for s in p.sorts.values_mut() {
                        if let FieldSort::ByValue { value, .. } = s
                            && *value > pos
                        {
                            *value -= 1;
                        }
                    }
                } else {
                    p.values.push(ValueField::new(field, Agg::Sum));
                }
            }
        });
    }

    /// Cycle a value field's summary (Sum → Count → Average → Min → Max).
    pub(crate) fn cycle_pivot_agg(&mut self, field: usize) {
        self.update_pivot(|p| {
            if let Some(v) = p.values.iter_mut().find(|v| v.field == field) {
                v.agg = v.agg.next();
            }
        });
    }

    /// Cycle how a value field is shown (no calculation, % of totals, difference, running total, rank).
    pub(crate) fn cycle_pivot_show(&mut self, field: usize) {
        self.update_pivot(|p| {
            if let Some(v) = p.values.iter_mut().find(|v| v.field == field) {
                v.show = v.show.next();
            }
        });
    }

    /// Cycle a row/column field's order: A→Z, Z→A, largest first, smallest first.
    pub(crate) fn cycle_pivot_sort(&mut self, field: usize) {
        self.update_pivot(|p| {
            let has_values = !p.values.is_empty();
            let next = match p.sorts.get(&field).copied().unwrap_or_default() {
                FieldSort::Ascending => FieldSort::Descending,
                FieldSort::Descending if has_values => FieldSort::ByValue { value: 0, descending: true },
                FieldSort::ByValue { value, descending: true } => FieldSort::ByValue { value, descending: false },
                _ => FieldSort::Ascending,
            };
            if next == FieldSort::Ascending {
                p.sorts.remove(&field);
            } else {
                p.sorts.insert(field, next);
            }
        });
    }

    pub(crate) fn toggle_pivot_subtotals(&mut self) {
        self.update_pivot(|p| p.subtotals = !p.subtotals);
    }

    /// Group a date field by one more level, or one fewer.
    pub(crate) fn toggle_pivot_date_level(&mut self, field: usize, level: DateLevel) {
        self.update_pivot(|p| {
            let mut levels = match p.groups.get(&field) {
                Some(Grouping::Dates(l)) => l.clone(),
                _ => Vec::new(),
            };
            if let Some(i) = levels.iter().position(|l| *l == level) {
                levels.remove(i);
            } else {
                levels.push(level);
                levels.sort();
            }
            if levels.is_empty() {
                p.groups.remove(&field);
            } else {
                p.groups.insert(field, Grouping::Dates(levels));
            }
        });
    }

    pub(crate) fn ungroup_pivot_field(&mut self, field: usize) {
        self.update_pivot(|p| {
            p.groups.remove(&field);
        });
        self.pivot_ui.picked.retain(|(f, _)| *f != field);
    }

    /// Hide an item of a field (at any level), or show it again.
    pub(crate) fn toggle_pivot_item(&mut self, field: usize, label: &str) {
        let label = label.to_string();
        self.update_pivot(|p| {
            let f = p.filters.entry(field).or_default();
            if !f.hidden.remove(&label) {
                f.hidden.insert(label);
            }
            if f.is_empty() {
                p.filters.remove(&field);
            }
        });
    }

    pub(crate) fn clear_pivot_filters(&mut self, field: usize) {
        self.update_pivot(|p| {
            p.filters.remove(&field);
        });
    }

    /// Group the items picked in the panel under `name`.
    pub(crate) fn group_pivot_items(&mut self, field: usize, name: &str) -> Result<(), String> {
        let items: BTreeSet<String> = self.pivot_ui.picked.iter().filter(|(f, _)| *f == field).map(|(_, l)| l.clone()).collect();
        if items.is_empty() {
            return Err("Pick the items to group first (click their names)".into());
        }
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err("Give the group a name".into());
        }
        self.update_pivot(|p| {
            let mut groups = match p.groups.get(&field) {
                Some(Grouping::Manual(g)) => g.clone(),
                _ => Vec::new(),
            };
            // An item is in one group; picked items leave their old groups.
            for (_, members) in groups.iter_mut() {
                members.retain(|m| !items.contains(m));
            }
            groups.retain(|(n, m)| !m.is_empty() && !n.eq_ignore_ascii_case(&name));
            groups.push((name, items));
            p.groups.insert(field, Grouping::Manual(groups));
        });
        self.pivot_ui.picked.retain(|(f, _)| *f != field);
        Ok(())
    }

    /// Make a pivot chart of the active pivot, next to it.
    pub(crate) fn insert_pivot_chart(&mut self, kind: ChartKind) {
        let Some(i) = self.active_pivot() else {
            self.message = Some(("Select a cell in a pivot table to chart it".into(), true));
            return;
        };
        let sheet = self.cur;
        let p = self.wb.layout(sheet).pivots[i].clone();
        let out = self.wb.pivot_table(sheet, &p);
        let extent = p.extent.unwrap_or(Range::single(p.anchor));
        let series = self.pivot_chart_series(sheet, &out, extent);
        if series.is_empty() {
            self.message = Some(("Add row and value fields to the pivot table first".into(), true));
            return;
        }
        let anchor = CellRef::new(extent.start.row, (extent.end.col + 2).min(xl_engine::MAX_COLS - 1));
        let chart = Chart { kind, title: Some(p.name.clone()), series, anchor, offset: (0.0, 0.0), size: (480.0, 288.0), stacked: false, pivot: Some(p.name.clone()) };
        let mut index = 0;
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(sheet) {
                l.charts.push(chart);
                index = l.charts.len() - 1;
            }
        });
        self.selected_chart = Some(index);
        self.ensure_visible(anchor);
        self.message = Some(("Added a pivot chart — it follows the pivot table's filters and changes".into(), false));
    }

    /// Remove the active pivot table: its output, and charts and panels bound to it lose it.
    pub(crate) fn delete_pivot(&mut self) {
        let Some(i) = self.active_pivot() else { return };
        let sheet = self.cur;
        let p = self.wb.layout(sheet).pivots[i].clone();
        let extent = p.extent.unwrap_or(Range::single(p.anchor));
        let items: Vec<(Addr, CellSnapshot)> = extent.iter().map(|c| (Addr::new(sheet, c), CellSnapshot::default())).collect();
        self.change_layout(Some(extent), |s| {
            s.last_stats = Some(s.wb.restore(items));
            if let Some(l) = s.wb.layout_mut(sheet) {
                l.pivots.remove(i);
                for c in &mut l.charts {
                    if c.pivot.as_deref() == Some(p.name.as_str()) {
                        c.pivot = None;
                    }
                }
            }
        });
        self.message = Some((format!("Deleted {}", p.name), false));
    }

    pub(crate) fn refresh_pivot(&mut self) {
        match self.active_pivot() {
            Some(i) => self.write_pivot(self.cur, i),
            None => self.message = Some(("Select a cell in a pivot table to refresh it".into(), true)),
        }
    }

    /// Rewrite the pivots named (after a slicer or timeline changed).
    pub(crate) fn refresh_pivots_named(&mut self, names: &[String]) {
        for n in names {
            if let Some((sheet, i)) = self.wb.find_pivot(n) {
                self.write_pivot(sheet, i);
            }
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
            self.undo.push(UndoEntry::Group(steps));
        }
    }

    // ------------------------------------------------------------- forms

    pub(crate) fn open_pivot_form(&mut self, kind: PivotForm) {
        let Some(i) = self.active_pivot() else { return };
        let p = &self.wb.layout(self.cur).pivots[i];
        let first_value = p.values.first().map(|v| self.wb.pivot_fields(self.cur, p).get(v.field).cloned().unwrap_or_default()).unwrap_or_default();
        let fields: Vec<(&'static str, &'static str, String)> = match kind {
            PivotForm::CalcField => vec![("Name", "e.g. Profit", format!("Field{}", p.calculated.len() + 1)), ("Formula", "e.g. =Revenue-Cost  ·  ='Unit Price'*0.1", "=".into())],
            PivotForm::Ranges(f) => {
                let (lo, hi) = self.wb.pivot_field_span(self.cur, p, f).unwrap_or((0.0, 100.0));
                let by = ((hi - lo) / 10.0).max(1.0).round();
                vec![("Starting at", "The first range starts here", xl_engine::precise(lo.floor())), ("Ending at", "Values above go in one last group", xl_engine::precise(hi.ceil())), ("By", "The size of each range", xl_engine::precise(by))]
            }
            PivotForm::Group(_) => vec![("Group name", "e.g. Fruit", format!("Group{}", self.pivot_group_count(p) + 1))],
            PivotForm::LabelFilter(_) => vec![("Show items whose label", "equals · does not equal · begins with · ends with · contains · does not contain", "contains".into()), ("Text", "", String::new())],
            PivotForm::ValueFilter(_) => vec![
                ("Value field", "Which value to compare", first_value),
                ("Condition", "> · >= · < · <= · = · <> · between · not between", ">".into()),
                ("Value", "", String::new()),
                ("And", "Only for between", String::new()),
            ],
            PivotForm::Top(_) => vec![("Show", "top or bottom", "top".into()), ("Items", "How many", "10".into()), ("By", "Which value field", first_value)],
        };
        let focus = match kind {
            PivotForm::CalcField | PivotForm::Group(_) => 0,
            PivotForm::LabelFilter(_) | PivotForm::Top(_) => 1,
            PivotForm::ValueFilter(_) => 2,
            PivotForm::Ranges(_) => 2,
        };
        self.form = Some(FormDialog { kind: FormKind::Pivot(kind), fields, goal: SolverGoalKind::Max, non_negative: false });
        self.field = Some(Field::Form(focus));
    }

    fn pivot_group_count(&self, p: &Pivot) -> usize {
        p.groups.values().map(|g| if let Grouping::Manual(g) = g { g.len() } else { 0 }).sum()
    }

    pub(crate) fn pivot_form_title(kind: PivotForm) -> &'static str {
        match kind {
            PivotForm::CalcField => "Calculated field",
            PivotForm::Ranges(_) => "Group into ranges",
            PivotForm::Group(_) => "Group items",
            PivotForm::LabelFilter(_) => "Label filter",
            PivotForm::ValueFilter(_) => "Value filter",
            PivotForm::Top(_) => "Top 10 filter",
        }
    }

    /// Index of a value field named in a form ("Revenue", "Sum of Revenue", or a 1-based number).
    fn value_index(&self, p: &Pivot, text: &str) -> Result<usize, String> {
        let t = text.trim();
        if p.values.is_empty() {
            return Err("Add a value field to the pivot table first".into());
        }
        if t.is_empty() {
            return Ok(0);
        }
        if let Ok(n) = t.parse::<usize>()
            && (1..=p.values.len()).contains(&n)
        {
            return Ok(n - 1);
        }
        let names = self.wb.pivot_fields(self.cur, p);
        let t = t.to_lowercase();
        p.values
            .iter()
            .position(|v| names.get(v.field).is_some_and(|n| n.to_lowercase() == t || t.ends_with(&format!(" of {}", n.to_lowercase()))))
            .ok_or_else(|| format!("“{}” isn't one of the pivot's value fields", text.trim()))
    }

    /// Apply a pivot form; Err keeps the dialog open with a message.
    pub(crate) fn run_pivot_form(&mut self, kind: PivotForm, texts: &[String]) {
        let text = |i: usize| texts.get(i).cloned().unwrap_or_default();
        let number = |i: usize, what: &str| -> Result<f64, String> { text(i).trim().parse::<f64>().map_err(|_| format!("{what} should be a number")) };
        let Some(i) = self.active_pivot() else {
            self.form = None;
            self.field = None;
            return;
        };
        let p = self.wb.layout(self.cur).pivots[i].clone();
        let result: Result<(), String> = (|| match kind {
            PivotForm::CalcField => {
                let name = text(0).trim().to_string();
                if name.is_empty() {
                    return Err("Give the field a name".into());
                }
                if self.wb.pivot_field_index(self.cur, &p, &name).is_some() {
                    return Err(format!("There's already a field named {name}"));
                }
                let expr = CalcExpr::parse(&text(1))?;
                let fields = self.wb.pivot_source_fields(self.cur, &p);
                if let Some(bad) = expr.names().into_iter().find(|n| !fields.iter().any(|f| f.eq_ignore_ascii_case(n))) {
                    return Err(format!("“{bad}” isn't a field of the source data"));
                }
                let formula = format!("={}", text(1).trim().trim_start_matches('='));
                let index = fields.len() + p.calculated.len();
                self.update_pivot(|p| {
                    p.calculated.push(CalcField { name, formula });
                    p.values.push(ValueField::new(index, Agg::Sum));
                });
                Ok(())
            }
            PivotForm::Ranges(f) => {
                let (start, end, by) = (number(0, "Starting at")?, number(1, "Ending at")?, number(2, "By")?);
                if by <= 0.0 || end < start {
                    return Err("Ranges need a positive size and an end after the start".into());
                }
                self.update_pivot(|p| {
                    p.groups.insert(f, Grouping::Numbers { start, end, by });
                });
                Ok(())
            }
            PivotForm::Group(f) => self.group_pivot_items(f, &text(0)),
            PivotForm::LabelFilter(f) => {
                let t = text(0).trim().to_lowercase();
                let op = LabelOp::ALL.into_iter().find(|o| o.label() == t).ok_or_else(|| format!("“{}” isn't a condition (try contains)", text(0).trim()))?;
                let flt = LabelFilter { op, text: text(1).trim().to_string() };
                self.update_pivot(|p| p.filters.entry(f).or_default().label = Some(flt));
                Ok(())
            }
            PivotForm::ValueFilter(f) => {
                let value = self.value_index(&p, &text(0))?;
                let t = text(1).trim().to_lowercase();
                let op = ValueOp::ALL.into_iter().find(|o| o.symbol() == t).ok_or_else(|| format!("“{}” isn't a condition (try >)", text(1).trim()))?;
                let a = number(2, "Value")?;
                let b = if matches!(op, ValueOp::Between | ValueOp::NotBetween) { number(3, "And")? } else { 0.0 };
                self.update_pivot(|p| p.filters.entry(f).or_default().value = Some(ValueFilter { value, op, a, b }));
                Ok(())
            }
            PivotForm::Top(f) => {
                let bottom = match text(0).trim().to_lowercase().as_str() {
                    "top" | "" => false,
                    "bottom" => true,
                    other => return Err(format!("“{other}”: choose top or bottom")),
                };
                let n = text(1).trim().parse::<usize>().ok().filter(|n| *n > 0).ok_or("Items should be a whole number above 0")?;
                let value = self.value_index(&p, &text(2))?;
                self.update_pivot(|p| p.filters.entry(f).or_default().top = Some(TopFilter { n, bottom, value }));
                Ok(())
            }
        })();
        match result {
            Ok(()) => {
                self.form = None;
                self.field = None;
            }
            Err(e) => self.message = Some((e, true)),
        }
    }

    // ------------------------------------------------------------- panel

    pub(crate) fn render_pivot_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let i = self.active_pivot()?;
        let p = self.wb.layout(self.cur).pivots.get(i)?;
        let t = self.theme;
        let fields = self.wb.pivot_fields(self.cur, p);
        let nsrc = self.wb.pivot_source_fields(self.cur, p).len();
        let chip = |id: gpui::ElementId, label: SharedString, on: bool| {
            div()
                .id(id)
                .px(px(6.))
                .h(px(20.))
                .flex()
                .flex_none()
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
        let link = |id: gpui::ElementId, label: SharedString| div().id(id).px(px(6.)).h(px(20.)).flex().flex_none().items_center().rounded(px(3.)).cursor_pointer().text_size(px(12.)).text_color(t.accent).hover(|s| s.bg(t.chrome_hover)).child(label);
        let heading = |s: &'static str| div().pt(px(4.)).text_size(px(11.5)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink_muted).child(s);
        let mut list = div().flex().flex_col().gap(px(2.));
        for (f, name) in fields.iter().enumerate() {
            let calc = f >= nsrc;
            let in_rows = p.rows.contains(&f);
            let in_cols = p.cols.contains(&f);
            let value = p.values.iter().find(|v| v.field == f).cloned();
            let open = self.pivot_ui.open_field == Some(f);
            let grouped = p.groups.contains_key(&f);
            let filtered = p.filters.get(&f).is_some_and(|x| !x.is_empty());
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .py(px(3.))
                .child(
                    div()
                        .id(("pv-open", f))
                        .flex_1()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.))
                        .overflow_hidden()
                        .cursor_pointer()
                        .child(div().w(px(10.)).text_size(px(10.)).text_color(t.ink_muted).child(if open { "▾" } else { "▸" }))
                        .child(div().overflow_hidden().whitespace_nowrap().text_size(px(13.5)).text_color(t.ink).child(SharedString::from(name.clone())))
                        .when(calc, |d| d.child(div().text_size(px(11.)).text_color(t.ink_faint).child("ƒ")))
                        .when(grouped || filtered, |d| d.child(div().text_size(px(11.)).text_color(t.accent).child(if filtered { "⏷" } else { "⧉" })))
                        .tooltip(crate::chrome::tip(t, "Field settings: sorting, grouping, filters", ""))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pivot_ui.open_field = if this.pivot_ui.open_field == Some(f) { None } else { Some(f) };
                            cx.notify();
                        })),
                );
            if !calc {
                row = row
                    .child(chip(("pv-row", f).into(), "Row".into(), in_rows).on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_pivot_field(f, Area::Rows);
                        cx.notify();
                    })))
                    .child(chip(("pv-col", f).into(), "Col".into(), in_cols).on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_pivot_field(f, Area::Cols);
                        cx.notify();
                    })));
            }
            row = row.child(chip(("pv-val", f).into(), "Value".into(), value.is_some()).on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_pivot_field(f, Area::Values);
                cx.notify();
            })));
            if let Some(v) = &value {
                row = row.child(
                    link(("pv-agg", f).into(), if calc { "Sum".into() } else { v.agg.label().into() })
                        .w(px(58.))
                        .justify_center()
                        .tooltip(crate::chrome::tip(t, "Click to change how values are summarized", ""))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.cycle_pivot_agg(f);
                            cx.notify();
                        })),
                );
            }
            list = list.child(row);
            if open {
                list = list.child(self.render_field_settings(p, f, calc, cx).into_any_element());
            }
        }
        let has_charts = self.wb.layout(self.cur).charts.iter().any(|c| c.pivot.as_deref() == Some(p.name.as_str()));
        let button = |id: &'static str, label: &'static str| {
            div().id(id).h(px(28.)).px(px(8.)).flex().flex_1().items_center().justify_center().rounded(px(4.)).cursor_pointer().border_1().border_color(t.divider).text_size(px(12.5)).hover(|s| s.bg(t.chrome_hover)).child(label)
        };
        Some(
            div()
                .w(px(310.))
                .h_full()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(12.))
                .bg(t.chrome)
                .border_l_1()
                .border_color(t.divider)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .gap(px(6.))
                        .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child("Pivot table"))
                        .child(div().text_size(px(12.)).text_color(t.ink_faint).child(SharedString::from(p.name.clone()))),
                )
                .child(div().text_size(px(12.5)).text_color(t.ink_muted).child("Pick fields to group rows and columns by, and values to summarize. Click a field's name for sorting, grouping and filters."))
                .child(heading("FIELDS"))
                .child(div().id("pv-fields").flex_1().overflow_y_scroll().child(list))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.))
                        .child(chip("pv-subtotals".into(), "Subtotals".into(), p.subtotals).on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_pivot_subtotals();
                            cx.notify();
                        })))
                        .child(link("pv-add-calc".into(), "+ Calculated field…".into()).on_click(cx.listener(|this, _, _, cx| {
                            this.open_pivot_form(PivotForm::CalcField);
                            cx.notify();
                        }))),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.))
                        .child(button("pv-chart", if has_charts { "Add pivot chart" } else { "Pivot chart" }).on_click(cx.listener(|this, _, _, cx| {
                            this.insert_pivot_chart(ChartKind::Column);
                            cx.notify();
                        })))
                        .child(button("pv-refresh", "Refresh").on_click(cx.listener(|this, _, _, cx| {
                            this.refresh_pivot();
                            cx.notify();
                        }))),
                )
                .into_any_element(),
        )
    }

    /// The expanded settings under a field in the panel.
    fn render_field_settings(&self, p: &Pivot, f: usize, calc: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let link = |id: gpui::ElementId, label: SharedString| div().id(id).px(px(6.)).h(px(20.)).flex().flex_none().items_center().rounded(px(3.)).cursor_pointer().text_size(px(12.)).text_color(t.accent).hover(|s| s.bg(t.chrome_hover)).child(label);
        let toggle = |id: gpui::ElementId, label: SharedString, on: bool| {
            div()
                .id(id)
                .px(px(6.))
                .h(px(20.))
                .flex()
                .flex_none()
                .items_center()
                .rounded(px(3.))
                .cursor_pointer()
                .text_size(px(12.))
                .border_1()
                .border_color(if on { t.accent } else { t.divider })
                .when(on, |d| d.bg(t.header_selected).text_color(t.accent))
                .when(!on, |d| d.text_color(t.ink_muted).hover(|s| s.bg(t.chrome_hover)))
                .child(label)
        };
        let label = |s: &'static str| div().w(px(54.)).flex_none().text_size(px(11.5)).text_color(t.ink_faint).child(s);
        let line = || div().flex().flex_row().flex_wrap().items_center().gap(px(4.));
        let mut body = div().ml(px(14.)).mb(px(6.)).p(px(6.)).flex().flex_col().gap(px(5.)).rounded(px(4.)).bg(t.canvas).border_1().border_color(t.divider);
        if let Some(v) = p.values.iter().find(|v| v.field == f) {
            body = body.child(line().child(label("Show as")).child(toggle(("pv-show", f).into(), v.show.label().into(), v.show != Default::default()).on_click(cx.listener(move |this, _, _, cx| {
                this.cycle_pivot_show(f);
                cx.notify();
            }))));
        }
        if calc {
            let formula = p.calculated.get(f - self.wb.pivot_source_fields(self.cur, p).len()).map(|c| c.formula.clone()).unwrap_or_default();
            body = body.child(line().child(label("Formula")).child(div().text_size(px(12.)).text_color(t.ink).child(SharedString::from(formula)))).child(line().child(link(("pv-calc-del", f).into(), "Remove field".into()).on_click(cx.listener(move |this, _, _, cx| {
                this.remove_calc_field(f);
                cx.notify();
            }))));
            return body;
        }
        let sort = p.sorts.get(&f).copied().unwrap_or_default();
        let sort_label = match sort {
            FieldSort::Ascending => "A → Z",
            FieldSort::Descending => "Z → A",
            FieldSort::ByValue { descending: true, .. } => "Largest first",
            FieldSort::ByValue { descending: false, .. } => "Smallest first",
        };
        body = body.child(line().child(label("Sort")).child(toggle(("pv-sort", f).into(), sort_label.into(), sort != FieldSort::Ascending).on_click(cx.listener(move |this, _, _, cx| {
            this.cycle_pivot_sort(f);
            cx.notify();
        }))));
        // Grouping.
        let kind = self.wb.pivot_field_kind(self.cur, p, f);
        let mut group = line().child(label("Group"));
        match (kind, p.groups.get(&f)) {
            (FieldKind::Date, g) => {
                let levels = if let Some(Grouping::Dates(l)) = g { l.clone() } else { Vec::new() };
                for (li, level) in DateLevel::ALL.into_iter().enumerate() {
                    group = group.child(toggle(("pv-date", f * 4 + li).into(), level.label().into(), levels.contains(&level)).on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_pivot_date_level(f, level);
                        cx.notify();
                    })));
                }
            }
            (FieldKind::Number, None) => {
                group = group.child(link(("pv-ranges", f).into(), "Ranges…".into()).on_click(cx.listener(move |this, _, _, cx| {
                    this.open_pivot_form(PivotForm::Ranges(f));
                    cx.notify();
                })));
            }
            _ => {}
        }
        let picked = self.pivot_ui.picked.iter().filter(|(x, _)| *x == f).count();
        if !matches!(p.groups.get(&f), Some(Grouping::Dates(_) | Grouping::Numbers { .. })) {
            group = group.child(link(("pv-group", f).into(), if picked > 0 { format!("Group {picked} picked…").into() } else { "Pick items to group".into() }).when(picked > 0, |d| {
                d.on_click(cx.listener(move |this, _, _, cx| {
                    this.open_pivot_form(PivotForm::Group(f));
                    cx.notify();
                }))
            }));
        }
        if p.groups.contains_key(&f) {
            group = group.child(link(("pv-ungroup", f).into(), "Ungroup".into()).on_click(cx.listener(move |this, _, _, cx| {
                this.ungroup_pivot_field(f);
                cx.notify();
            })));
        }
        body = body.child(group);
        // Filters.
        let filter = p.filters.get(&f).cloned().unwrap_or_default();
        let mut filters = line().child(label("Filter"));
        filters = filters
            .child(toggle(("pv-flabel", f).into(), "Label…".into(), filter.label.is_some()).on_click(cx.listener(move |this, _, _, cx| {
                this.open_pivot_form(PivotForm::LabelFilter(f));
                cx.notify();
            })))
            .child(toggle(("pv-fvalue", f).into(), "Value…".into(), filter.value.is_some()).on_click(cx.listener(move |this, _, _, cx| {
                this.open_pivot_form(PivotForm::ValueFilter(f));
                cx.notify();
            })))
            .child(toggle(("pv-ftop", f).into(), "Top 10…".into(), filter.top.is_some()).on_click(cx.listener(move |this, _, _, cx| {
                this.open_pivot_form(PivotForm::Top(f));
                cx.notify();
            })));
        if !filter.is_empty() {
            filters = filters.child(link(("pv-fclear", f).into(), "Clear".into()).on_click(cx.listener(move |this, _, _, cx| {
                this.clear_pivot_filters(f);
                cx.notify();
            })));
        }
        body = body.child(filters);
        let mut summary = Vec::new();
        if let Some(l) = &filter.label {
            summary.push(format!("label {} “{}”", l.op.label(), l.text));
        }
        if let Some(v) = &filter.value {
            summary.push(format!("value {} {}", v.op.symbol(), xl_engine::precise(v.a)));
        }
        if let Some(tf) = &filter.top {
            summary.push(format!("{} {}", if tf.bottom { "bottom" } else { "top" }, tf.n));
        }
        if !summary.is_empty() {
            body = body.child(div().text_size(px(11.5)).text_color(t.ink_muted).child(SharedString::from(format!("Showing items with {}", summary.join(", ")))));
        }
        // Items: the checkbox shows or hides an item; clicking its name picks it for grouping.
        let levels = self.wb.pivot_field_items(self.cur, p, f);
        let multi = levels.len() > 1;
        let mut items = div().flex().flex_col().gap(px(1.));
        for (li, (level_name, labels)) in levels.iter().enumerate() {
            if multi {
                items = items.child(div().pt(px(2.)).text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink_faint).child(SharedString::from(level_name.clone())));
            }
            for (ii, item) in labels.iter().take(300).enumerate() {
                let shown = !filter.hidden.contains(item);
                let picked = self.pivot_ui.picked.contains(&(f, item.clone()));
                let id = (f * 8 + li) * 1000 + ii;
                let item_c = item.clone();
                let item_p = item.clone();
                items = items.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .id(("pv-item-check", id))
                                .size(px(14.))
                                .flex()
                                .flex_none()
                                .items_center()
                                .justify_center()
                                .rounded(px(3.))
                                .cursor_pointer()
                                .border_1()
                                .border_color(if shown { t.accent } else { t.ink_faint })
                                .when(shown, |d| d.bg(t.accent).child(gpui::svg().path("icons/check.svg").size(px(11.)).text_color(t.on_accent)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_pivot_item(f, &item_c);
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .id(("pv-item", id))
                                .flex_1()
                                .px(px(4.))
                                .rounded(px(3.))
                                .cursor_pointer()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_size(px(12.5))
                                .text_color(if shown { t.ink } else { t.ink_faint })
                                .when(picked, |d| d.bg(t.header_selected).text_color(t.accent))
                                .hover(|s| s.bg(t.chrome_hover))
                                .child(SharedString::from(item.clone()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let key = (f, item_p.clone());
                                    if !this.pivot_ui.picked.remove(&key) {
                                        this.pivot_ui.picked.insert(key);
                                    }
                                    cx.notify();
                                })),
                        ),
                );
            }
            if labels.len() > 300 {
                items = items.child(div().text_size(px(11.5)).text_color(t.ink_faint).child(SharedString::from(format!("… and {} more", labels.len() - 300))));
            }
        }
        body = body.child(items);
        let mut panels = line().child(label("Insert")).child(link(("pv-slicer", f).into(), "Slicer".into()).on_click(cx.listener(move |this, _, _, cx| {
            this.insert_pivot_slicer(f);
            cx.notify();
        })));
        if kind == FieldKind::Date {
            panels = panels.child(link(("pv-timeline", f).into(), "Timeline".into()).on_click(cx.listener(move |this, _, _, cx| {
                this.insert_pivot_timeline(f);
                cx.notify();
            })));
        }
        body.child(panels)
    }

    /// Remove a calculated field (and its use as a value).
    pub(crate) fn remove_calc_field(&mut self, field: usize) {
        let Some(i) = self.active_pivot() else { return };
        let nsrc = self.wb.pivot_source_fields(self.cur, &self.wb.layout(self.cur).pivots[i]).len();
        if field < nsrc {
            return;
        }
        if self.wb.layout(self.cur).pivots[i].values.iter().any(|v| v.field == field) {
            self.toggle_pivot_field(field, Area::Values);
        }
        self.update_pivot(|p| {
            if field - nsrc < p.calculated.len() {
                p.calculated.remove(field - nsrc);
                for v in &mut p.values {
                    if v.field > field {
                        v.field -= 1;
                    }
                }
            }
        });
        self.pivot_ui.open_field = None;
    }
}
