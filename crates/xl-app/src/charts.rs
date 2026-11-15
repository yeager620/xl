//! Charts on the sheet: inserting one from data, selecting, moving,
//! resizing, changing its kind or title, deleting.

use std::sync::Arc;

use gpui::{Pixels, Point};
use xl_engine::chart::{Chart, ChartKind, ChartSeries};
use xl_engine::formula::parse_formula;
use xl_engine::{CellRef, Range, Value, quote_sheet_name};

use crate::app::{Field, Spreadsheet, absolute_range};

const DEFAULT_SIZE: (f64, f64) = (480.0, 288.0);

impl Spreadsheet {
    /// Screen position and size of a chart: (x, y, w, h) in window pixels.
    pub(crate) fn chart_rect(&self, chart: &Chart) -> (f64, f64, f64, f64) {
        let x = f64::from(self.viewport.origin.x) + self.hw() + self.grid_x(chart.anchor.col) + chart.offset.0;
        let y = f64::from(self.viewport.origin.y) + self.hh() + self.grid_y(chart.anchor.row) + chart.offset.1;
        (x, y, chart.size.0, chart.size.1)
    }

    /// The topmost chart under a window position.
    pub(crate) fn chart_at(&self, pos: Point<Pixels>) -> Option<usize> {
        let (px, py) = (f64::from(pos.x), f64::from(pos.y));
        let grid_top = f64::from(self.viewport.origin.y) + self.hh();
        let grid_left = f64::from(self.viewport.origin.x) + self.hw();
        if px < grid_left || py < grid_top {
            return None;
        }
        self.wb.layout(self.cur).charts.iter().enumerate().rev().find_map(|(i, c)| {
            let (x, y, w, h) = self.chart_rect(c);
            (px >= x && px <= x + w && py >= y && py <= y + h).then_some(i)
        })
    }

    /// Is the position on the selected chart's resize grip?
    pub(crate) fn on_chart_grip(&self, pos: Point<Pixels>) -> bool {
        let Some(chart) = self.selected_chart.and_then(|i| self.wb.layout(self.cur).charts.get(i)) else { return false };
        let (x, y, w, h) = self.chart_rect(chart);
        (f64::from(pos.x) - (x + w)).abs() <= 7.0 && (f64::from(pos.y) - (y + h)).abs() <= 7.0
    }

    /// Insert a chart of the data around the selection: a text first column
    /// gives the categories, a header row the series names; each other
    /// column is a series.
    pub(crate) fn insert_chart(&mut self, kind: ChartKind) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let Some(range) = self.data_range() else {
            self.message = Some(("Select the data to chart first".into(), true));
            return;
        };
        let header = self.guess_header(range);
        let first = range.start.row + header as u32;
        if first > range.end.row {
            self.message = Some(("There's no data under the headers to chart".into(), true));
            return;
        }
        let store = self.wb.store(self.cur);
        let text_col = |c: u32| (first..=range.end.row).all(|r| !matches!(store.value(CellRef::new(r, c)), Value::Number(_)));
        let has_categories = range.cols() > 1 && (text_col(range.start.col) || kind == ChartKind::Scatter);
        let sheet = quote_sheet_name(self.wb.sheet_name(self.cur));
        let reference = |r: Range| parse_formula(&format!("{sheet}!{}", absolute_range(r)), self.wb.sheets()).ok().map(Arc::new);
        let column = |c: u32| Range::new(CellRef::new(first, c), CellRef::new(range.end.row, c));
        let categories = has_categories.then(|| reference(column(range.start.col))).flatten();
        let first_series_col = range.start.col + has_categories as u32;
        let mut series = Vec::new();
        for c in first_series_col..=range.end.col {
            // Columns of text (like a second label column) aren't series.
            if text_col(c) {
                continue;
            }
            let Some(values) = reference(column(c)) else { continue };
            let name = header.then(|| reference(Range::single(CellRef::new(range.start.row, c)))).flatten();
            series.push(ChartSeries { name, categories: categories.clone(), values });
        }
        if series.is_empty() {
            self.message = Some(("Select at least one column of numbers to chart".into(), true));
            return;
        }
        let anchor = CellRef::new(range.start.row, (range.end.col + 2).min(xl_engine::MAX_COLS - 1));
        let chart = Chart { kind, title: None, series, anchor, offset: (0.0, 0.0), size: DEFAULT_SIZE, stacked: false, pivot: None };
        let mut index = 0;
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                l.charts.push(chart);
                index = l.charts.len() - 1;
            }
        });
        self.selected_chart = Some(index);
        self.menu = None;
        let corner = CellRef::new(anchor.row + 12, anchor.col + 4);
        self.ensure_visible(CellRef::new(corner.row.min(xl_engine::MAX_ROWS - 1), corner.col.min(xl_engine::MAX_COLS - 1)));
        self.ensure_visible(anchor);
    }

    /// Change something about the selected chart (one undo step).
    pub(crate) fn update_chart(&mut self, f: impl FnOnce(&mut Chart)) {
        let Some(i) = self.selected_chart else { return };
        self.menu = None;
        self.context = None;
        self.change_layout(None, |s| {
            if let Some(chart) = s.wb.layout_mut(s.cur).and_then(|l| l.charts.get_mut(i)) {
                f(chart);
            }
        });
    }

    pub(crate) fn delete_chart(&mut self) {
        let Some(i) = self.selected_chart.take() else { return };
        self.context = None;
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur)
                && i < l.charts.len()
            {
                l.charts.remove(i);
            }
        });
    }

    pub(crate) fn edit_chart_title(&mut self) {
        let Some(chart) = self.selected_chart.and_then(|i| self.wb.layout(self.cur).charts.get(i)) else { return };
        self.chart_title_text = chart.title.clone().unwrap_or_default();
        self.context = None;
        self.field = Some(Field::ChartTitle);
    }

    pub(crate) fn key_chart_title(&mut self, key: &str) -> bool {
        match key {
            "enter" => {
                let title = std::mem::take(&mut self.chart_title_text);
                self.field = None;
                self.update_chart(move |c| c.title = (!title.trim().is_empty()).then_some(title));
            }
            "escape" => self.field = None,
            "backspace" => {
                self.chart_title_text.pop();
            }
            _ => return false,
        }
        true
    }

    /// Where a chart lands when its top-left corner is dragged to window
    /// position (x, y): the cell under it and the offset inside that cell.
    pub(crate) fn chart_anchor_at(&self, x: f64, y: f64) -> (CellRef, (f64, f64)) {
        let gx = (x - f64::from(self.viewport.origin.x) - self.hw()).max(0.0);
        let gy = (y - f64::from(self.viewport.origin.y) - self.hh()).max(0.0);
        let cell = CellRef::new(self.row_at_grid(gy), self.col_at_grid(gx));
        (cell, ((gx - self.grid_x(cell.col)).max(0.0), (gy - self.grid_y(cell.row)).max(0.0)))
    }
}

impl Spreadsheet {
    /// Add sparklines for the selected data: one per row in the column to
    /// its right (or, for a single column, one below it).
    pub(crate) fn insert_sparklines(&mut self, kind: xl_engine::chart::SparkKind) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let Some(range) = self.data_range() else {
            self.message = Some(("Select the numbers to draw sparklines for".into(), true));
            return;
        };
        let header = self.guess_header(range);
        let store = self.wb.store(self.cur);
        let first_row = range.start.row + header as u32;
        // A leading column of labels isn't data.
        let labels = range.cols() > 1 && (first_row..=range.end.row).all(|r| !matches!(store.value(CellRef::new(r, range.start.col)), Value::Number(_)));
        let first_col = range.start.col + labels as u32;
        let sheet = quote_sheet_name(self.wb.sheet_name(self.cur));
        let reference = |r: Range| parse_formula(&format!("{sheet}!{}", absolute_range(r)), self.wb.sheets()).ok().map(Arc::new);
        let mut new = Vec::new();
        if range.cols() == 1 + labels as u32 && range.rows() > 1 {
            let data = Range::new(CellRef::new(first_row, first_col), CellRef::new(range.end.row, first_col));
            if let (Some(data), true) = (reference(data), range.end.row + 1 < xl_engine::MAX_ROWS) {
                new.push(xl_engine::chart::Sparkline { cell: CellRef::new(range.end.row + 1, first_col), kind, data, color: None });
            }
        } else {
            let col = range.end.col + 1;
            if col >= xl_engine::MAX_COLS {
                return;
            }
            for r in first_row..=range.end.row {
                if let Some(data) = reference(Range::new(CellRef::new(r, first_col), CellRef::new(r, range.end.col))) {
                    new.push(xl_engine::chart::Sparkline { cell: CellRef::new(r, col), kind, data, color: None });
                }
            }
        }
        let count = new.len();
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                // A cell holds one sparkline: new ones replace old.
                l.sparklines.retain(|old| !new.iter().any(|n| n.cell == old.cell));
                l.sparklines.extend(new);
            }
        });
        self.message = Some((format!("Added {count} sparkline{}", if count == 1 { "" } else { "s" }), false));
    }

    pub(crate) fn clear_sparklines(&mut self) {
        let sel = self.selection();
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                l.sparklines.retain(|sp| !sel.contains(sp.cell));
            }
        });
    }
}
