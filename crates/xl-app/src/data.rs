//! Data tools that rearrange ranges: sort (and, built on the same pieces,
//! filters, duplicate removal and text to columns).

use std::cmp::Ordering;

use xl_engine::{Addr, CellRef, MAX_COLS, MAX_ROWS, Range, Value};

use crate::app::Spreadsheet;

/// Excel's sort order: numbers, text (case-insensitive), logicals, errors;
/// blanks always go last, whichever direction.
pub(crate) fn compare_values(a: &Value, b: &Value, ascending: bool) -> Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Number(_) => 0,
            Value::Text(_) => 1,
            Value::Bool(_) => 2,
            Value::Error(_) => 3,
            Value::Empty => 4,
        }
    }
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    let ord = match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.total_cmp(y),
        (Value::Text(x), Value::Text(y)) => x.to_lowercase().cmp(&y.to_lowercase()),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => rank(a).cmp(&rank(b)),
    };
    if ascending { ord } else { ord.reverse() }
}

impl Spreadsheet {
    /// The block of non-empty cells around `c` (Excel's "current region"):
    /// grows while any cell bordering it, diagonals included, has content.
    pub(crate) fn current_region(&self, c: CellRef) -> Range {
        let store = self.wb.store(self.cur);
        let filled = |r: u32, col: u32| !store.value(CellRef::new(r, col)).is_empty();
        let (mut r0, mut r1, mut c0, mut c1) = (c.row, c.row, c.col, c.col);
        loop {
            let mut grew = false;
            let (top, bottom, left, right) = (r0.saturating_sub(1), (r1 + 1).min(MAX_ROWS - 1), c0.saturating_sub(1), (c1 + 1).min(MAX_COLS - 1));
            if r0 > 0 && (left..=right).any(|col| filled(top, col)) {
                r0 = top;
                grew = true;
            }
            if r1 + 1 < MAX_ROWS && (left..=right).any(|col| filled(bottom, col)) {
                r1 = bottom;
                grew = true;
            }
            if c0 > 0 && (top..=bottom).any(|r| filled(r, left)) {
                c0 = left;
                grew = true;
            }
            if c1 + 1 < MAX_COLS && (top..=bottom).any(|r| filled(r, right)) {
                c1 = right;
                grew = true;
            }
            if !grew {
                break;
            }
        }
        Range::new(CellRef::new(r0, c0), CellRef::new(r1, c1))
    }

    /// Does the range start with a header row? Like Excel's guess: the first
    /// row is all text while the row under it isn't, or it's styled
    /// differently (bold) from the data.
    pub(crate) fn guess_header(&self, range: Range) -> bool {
        if range.rows() < 2 {
            return false;
        }
        let store = self.wb.store(self.cur);
        let row_kinds = |r: u32| (range.start.col..=range.end.col).map(move |c| store.value(CellRef::new(r, c)));
        let first_all_text = row_kinds(range.start.row).all(|v| matches!(v, Value::Text(_) | Value::Empty)) && row_kinds(range.start.row).any(|v| !v.is_empty());
        let second_has_other = row_kinds(range.start.row + 1).any(|v| matches!(v, Value::Number(_) | Value::Bool(_)));
        let bold = |r: u32| self.wb.style(self.at(CellRef::new(r, range.start.col))).bold;
        first_all_text && (second_has_other || (bold(range.start.row) && !bold(range.start.row + 1)))
    }

    /// The range a data command works on: the selection, or the current
    /// region around a single selected cell. None if there's nothing there.
    pub(crate) fn data_range(&self) -> Option<Range> {
        let sel = self.selection();
        let r = if sel.start == sel.end { self.current_region(self.active) } else { sel };
        // Whole rows/columns: trim to the used area.
        let extent = self.wb.store(self.cur).extent()?;
        let r = Range::new(r.start, CellRef::new(r.end.row.min(extent.row), r.end.col.min(extent.col)));
        (r.start.row <= r.end.row && r.start.col <= r.end.col && self.wb.store(self.cur).iter_range(r).next().is_some()).then_some(r)
    }

    /// Reorder the rows of `range` by `keys` (column, ascending). Formulas
    /// move like a cut and paste: relative references follow their row.
    /// Returns false if nothing changed.
    pub(crate) fn sort_rows(&mut self, range: Range, keys: &[(u32, bool)], header: bool) -> bool {
        let first = range.start.row + header as u32;
        if first >= range.end.row {
            return false;
        }
        let store = self.wb.store(self.cur);
        let mut order: Vec<u32> = (first..=range.end.row).collect();
        // A stable sort keeps equal rows in their original order, like Excel.
        order.sort_by(|&a, &b| {
            keys.iter()
                .map(|&(col, asc)| compare_values(store.value(CellRef::new(a, col)), store.value(CellRef::new(b, col)), asc))
                .find(|o| o.is_ne())
                .unwrap_or(Ordering::Equal)
        });
        if order.iter().enumerate().all(|(i, &r)| r == first + i as u32) {
            return false;
        }
        let mut items = Vec::new();
        for (i, &src) in order.iter().enumerate() {
            let dst = first + i as u32;
            for col in range.start.col..=range.end.col {
                let from = Addr::new(self.cur, CellRef::new(src, col));
                items.push((Addr::new(self.cur, CellRef::new(dst, col)), self.wb.snapshot_shifted(from, dst as i64 - src as i64, 0)));
            }
        }
        // Rows that don't move needn't be rewritten.
        let items = items.into_iter().filter(|(a, snap)| self.wb.snapshot(*a) != *snap).collect();
        self.apply_snapshots(items);
        true
    }

    /// Sort the data around the selection by the active cell's column.
    pub(crate) fn sort_by_active(&mut self, ascending: bool) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let Some(range) = self.data_range() else {
            self.message = Some(("Select a cell in a table of data to sort".into(), true));
            return;
        };
        let col = self.active.col.clamp(range.start.col, range.end.col);
        let header = self.guess_header(range);
        self.sort_rows(range, &[(col, ascending)], header);
        // Select the sorted rows.
        self.anchor = range.end;
        self.active = CellRef::new(range.start.row + header as u32, range.start.col);
        self.message = Some((format!("Sorted {} rows {}", range.rows() - header as u32, if ascending { "A → Z" } else { "Z → A" }), false));
    }
}

/// The open filter dropdown for one column.
pub(crate) struct FilterMenu {
    pub col: u32,
    pub pos: gpui::Point<gpui::Pixels>,
    /// Distinct displayed values in the column ("" = blanks), in sort order,
    /// and whether each is shown.
    pub values: Vec<(String, bool)>,
}

/// Distinct values listed in a filter dropdown, at most.
const MAX_FILTER_VALUES: usize = 10_000;

impl Spreadsheet {
    /// Turn the filter on for the data around the selection, or off.
    pub(crate) fn toggle_filter(&mut self) {
        if self.wb.layout(self.cur).filter.is_some() {
            self.change_layout(None, |s| {
                if let Some(l) = s.wb.layout_mut(s.cur) {
                    l.filter = None;
                    l.filtered_rows.clear();
                }
            });
            self.filter_menu = None;
            return;
        }
        let Some(range) = self.data_range() else {
            self.message = Some(("Select a cell in a table of data to filter".into(), true));
            return;
        };
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                l.filter = Some(xl_engine::AutoFilter { range, columns: Default::default() });
            }
            s.wb.reapply_filter(s.cur);
        });
    }

    /// The filter column whose dropdown button is under a window position.
    pub(crate) fn filter_button_at(&self, pos: gpui::Point<gpui::Pixels>) -> Option<u32> {
        let f = self.wb.layout(self.cur).filter.as_ref()?;
        let r = f.range.start.row;
        let gy = f64::from(self.viewport.origin.y) + self.hh() + self.grid_y(r);
        let (x, y) = (f64::from(pos.x), f64::from(pos.y));
        if y < gy || y > gy + self.row_h(r) {
            return None;
        }
        (f.range.start.col..=f.range.end.col).find(|&c| {
            let right = f64::from(self.viewport.origin.x) + self.hw() + self.grid_x(c) + self.col_w(c);
            x >= right - FILTER_BUTTON - 3.0 && x <= right
        })
    }

    pub(crate) fn open_filter_menu(&mut self, col: u32, pos: gpui::Point<gpui::Pixels>) {
        self.wb.reapply_filter(self.cur);
        let Some(f) = self.wb.layout(self.cur).filter.clone() else { return };
        let allowed = f.columns.get(&col);
        let mut seen = std::collections::HashSet::new();
        let mut values: Vec<(Value, String)> = Vec::new();
        for r in f.range.start.row + 1..=f.range.end.row {
            let a = self.at(CellRef::new(r, col));
            let text = self.wb.display(a).text;
            if seen.insert(text.clone()) {
                values.push((self.wb.value(a).clone(), text));
                if values.len() >= MAX_FILTER_VALUES {
                    break;
                }
            }
        }
        values.sort_by(|a, b| compare_values(&a.0, &b.0, true).then_with(|| a.1.cmp(&b.1)));
        let values = values.into_iter().map(|(_, text)| {
            let on = allowed.is_none_or(|set| set.contains(&text));
            (text, on)
        });
        self.menu = None;
        self.context = None;
        self.filter_menu = Some(FilterMenu { col, pos, values: values.collect() });
    }

    /// Apply the dropdown's checkboxes to the filter (one undo step).
    pub(crate) fn apply_filter_menu(&mut self) {
        let Some(menu) = self.filter_menu.take() else { return };
        let col = menu.col;
        let all = menu.values.iter().all(|(_, on)| *on);
        let allowed: std::collections::BTreeSet<String> = menu.values.into_iter().filter(|(_, on)| *on).map(|(t, _)| t).collect();
        self.change_layout(None, |s| {
            if let Some(f) = s.wb.layout_mut(s.cur).and_then(|l| l.filter.as_mut()) {
                if all {
                    f.columns.remove(&col);
                } else {
                    f.columns.insert(col, allowed);
                }
            }
            s.wb.reapply_filter(s.cur);
        });
    }

    /// Sort the filtered range by a column from its dropdown.
    pub(crate) fn sort_filter_column(&mut self, ascending: bool) {
        let Some(menu) = self.filter_menu.take() else { return };
        let Some(f) = self.wb.layout(self.cur).filter.clone() else { return };
        self.sort_rows(f.range, &[(menu.col, ascending)], true);
        self.wb.reapply_filter(self.cur);
    }

    pub(crate) fn render_filter_menu(&self, cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        use gpui::{InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled, div, px};
        use crate::chrome::{menu_item, menu_panel, menu_separator};
        let t = self.theme;
        let Some(menu) = &self.filter_menu else { return div().into_any_element() };
        let all = menu.values.iter().all(|(_, on)| *on);
        let checkbox = move |on: bool| {
            div()
                .size(px(15.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .border_1()
                .border_color(if on { t.accent } else { t.ink_faint })
                .when(on, |d| d.bg(t.accent).child(gpui::svg().path("icons/check.svg").size(px(12.)).text_color(t.on_accent)))
        };
        let row = |id: gpui::ElementId, label: String, on: bool| {
            div().id(id).flex().flex_row().items_center().gap(px(8.)).h(px(26.)).px(px(8.)).rounded(px(4.)).cursor_pointer().hover(|s| s.bg(t.chrome_hover)).child(checkbox(on)).child(label)
        };
        let mut list = div().id("filter-values").flex().flex_col().max_h(px(260.)).overflow_y_scroll();
        for (i, (text, on)) in menu.values.iter().enumerate() {
            let label = if text.is_empty() { "(Blanks)".to_string() } else { text.clone() };
            list = list.child(row(("filter-value", i).into(), label, *on).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(m) = this.filter_menu.as_mut()
                    && let Some(v) = m.values.get_mut(i)
                {
                    v.1 = !v.1;
                }
                cx.notify();
            })));
        }
        let button = |id: &'static str, label: &'static str| {
            div().id(id).flex().items_center().justify_center().h(px(28.)).px(px(12.)).rounded(px(4.)).cursor_pointer().text_size(px(13.5)).child(label)
        };
        use gpui::prelude::FluentBuilder;
        menu_panel(&t, 250.)
            .child(menu_item(&t, "filter-sort-asc", "Sort A → Z", "").on_click(cx.listener(|this, _, _, cx| {
                this.sort_filter_column(true);
                cx.notify();
            })))
            .child(menu_item(&t, "filter-sort-desc", "Sort Z → A", "").on_click(cx.listener(|this, _, _, cx| {
                this.sort_filter_column(false);
                cx.notify();
            })))
            .child(menu_separator(&t))
            .child(row("filter-all".into(), "Select all".into(), all).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(m) = this.filter_menu.as_mut() {
                    for v in &mut m.values {
                        v.1 = !all;
                    }
                }
                cx.notify();
            })))
            .child(list)
            .child(menu_separator(&t))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(6.))
                    .p(px(4.))
                    .child(div().flex_1())
                    .child(button("filter-cancel", "Cancel").text_color(t.ink).border_1().border_color(t.divider).hover(|s| s.bg(t.chrome_hover)).on_click(cx.listener(|this, _, _, cx| {
                        this.filter_menu = None;
                        cx.notify();
                    })))
                    .child(button("filter-ok", "OK").bg(t.accent).text_color(t.on_accent).on_click(cx.listener(|this, _, _, cx| {
                        this.apply_filter_menu();
                        cx.notify();
                    }))),
            )
            .into_any_element()
    }
}

/// Size of the dropdown button in a filter's header cells.
pub(crate) const FILTER_BUTTON: f64 = 16.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excel_sort_order() {
        let mut v = vec![Value::text("b"), Value::Empty, Value::Number(10.0), Value::Bool(true), Value::text("A"), Value::Number(2.0)];
        v.sort_by(|a, b| compare_values(a, b, true));
        assert_eq!(v, vec![Value::Number(2.0), Value::Number(10.0), Value::text("A"), Value::text("b"), Value::Bool(true), Value::Empty]);
        v.sort_by(|a, b| compare_values(a, b, false));
        assert_eq!(v.last(), Some(&Value::Empty), "blanks stay last when descending");
        assert_eq!(v.first(), Some(&Value::Bool(true)));
    }
}

/// How a text-to-columns split finds its pieces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Delimiter {
    /// The most common of tab, semicolon, comma and pipe in the data.
    Auto,
    Comma,
    Tab,
    Semicolon,
    Space,
}

/// Split a line on `sep`, honouring double quotes ("a, b" stays whole; ""
/// is a literal quote) like CSV.
pub(crate) fn split_fields(line: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' if quoted => quoted = false,
            // A field may start with spaces before its opening quote ("a, "b"").
            '"' if cur.trim().is_empty() => {
                cur.clear();
                quoted = true;
            }
            c if c == sep && !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    if sep == ' ' {
        // Runs of spaces count as one separator.
        out.retain(|s| !s.is_empty());
    }
    out
}

impl Spreadsheet {
    /// Remove rows whose values repeat an earlier row (comparing every
    /// column, text without case), keeping the first; the rest close up.
    pub(crate) fn remove_duplicates(&mut self) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let Some(range) = self.data_range() else {
            self.message = Some(("Select a table of data to remove duplicates from".into(), true));
            return;
        };
        let header = self.guess_header(range);
        let first = range.start.row + header as u32;
        let store = self.wb.store(self.cur);
        let key = |r: u32| -> Vec<String> {
            (range.start.col..=range.end.col)
                .map(|c| match store.value(CellRef::new(r, c)) {
                    Value::Text(t) => format!("t{}", t.to_lowercase()),
                    Value::Empty => String::new(),
                    v => format!("v{}", v.display()),
                })
                .collect()
        };
        let mut seen = std::collections::HashSet::new();
        let kept: Vec<u32> = (first..=range.end.row).filter(|&r| seen.insert(key(r))).collect();
        let removed = (range.end.row + 1 - first) as usize - kept.len();
        if removed == 0 {
            self.message = Some(("No duplicate rows found".into(), false));
            return;
        }
        let mut items = Vec::new();
        for (i, &src) in kept.iter().enumerate() {
            let dst = first + i as u32;
            for col in range.start.col..=range.end.col {
                let from = Addr::new(self.cur, CellRef::new(src, col));
                items.push((Addr::new(self.cur, CellRef::new(dst, col)), self.wb.snapshot_shifted(from, dst as i64 - src as i64, 0)));
            }
        }
        for r in first + kept.len() as u32..=range.end.row {
            for col in range.start.col..=range.end.col {
                items.push((Addr::new(self.cur, CellRef::new(r, col)), xl_engine::CellSnapshot::default()));
            }
        }
        let items = items.into_iter().filter(|(a, snap)| self.wb.snapshot(*a) != *snap).collect();
        self.apply_snapshots(items);
        self.anchor = range.start;
        self.active = CellRef::new(first + kept.len() as u32 - 1, range.end.col);
        self.message = Some((format!("Removed {removed} duplicate row{}; {} unique remain", if removed == 1 { "" } else { "s" }, kept.len()), false));
    }

    /// Split the text in the selected column into columns to its right.
    pub(crate) fn text_to_columns(&mut self, delim: Delimiter) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let sel = self.selection();
        let col = sel.start.col;
        let extent = self.wb.store(self.cur).extent();
        let last = extent.map_or(sel.end.row, |e| sel.end.row.min(e.row));
        let rows: Vec<(u32, String)> = (sel.start.row..=last)
            .filter_map(|r| match self.wb.value(self.at(CellRef::new(r, col))) {
                Value::Text(t) => Some((r, t.to_string())),
                _ => None,
            })
            .collect();
        if rows.is_empty() {
            self.message = Some(("Select a column of text to split".into(), true));
            return;
        }
        let sep = match delim {
            Delimiter::Comma => ',',
            Delimiter::Tab => '\t',
            Delimiter::Semicolon => ';',
            Delimiter::Space => ' ',
            Delimiter::Auto => ['\t', ';', ',', '|']
                .into_iter()
                .max_by_key(|&c| rows.iter().filter(|(_, t)| t.contains(c)).count())
                .filter(|&c| rows.iter().any(|(_, t)| t.contains(c)))
                .unwrap_or(' '),
        };
        let mut changes = Vec::new();
        let mut widest = 0;
        for (r, text) in &rows {
            let parts = split_fields(text, sep);
            widest = widest.max(parts.len());
            for (i, p) in parts.into_iter().enumerate() {
                if let Some(dst) = CellRef::new(*r, col).offset(0, i as i64) {
                    changes.push((self.at(dst), p.trim().to_string()));
                }
            }
        }
        self.apply_inputs(changes);
        self.anchor = CellRef::new(sel.start.row, col);
        self.active = CellRef::new(last, col + widest.saturating_sub(1) as u32);
        self.message = Some((format!("Split {} cell{} into {widest} columns", rows.len(), if rows.len() == 1 { "" } else { "s" }), false));
    }
}

#[cfg(test)]
mod split_tests {
    use super::split_fields;

    #[test]
    fn splits_like_csv() {
        assert_eq!(split_fields("a,\"b, c\",d", ','), ["a", "b, c", "d"]);
        assert_eq!(split_fields("say \"\"hi\"\"", ','), ["say \"\"hi\"\""], "quotes only count at the start of a field");
        assert_eq!(split_fields("\"x \"\"y\"\"\",z", ','), ["x \"y\"", "z"]);
        assert_eq!(split_fields("Ada  Lovelace", ' '), ["Ada", "Lovelace"]);
        assert_eq!(split_fields("a;;b", ';'), ["a", "", "b"]);
    }
}
