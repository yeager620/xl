//! Slicers and timelines: floating filter panels on a sheet. A slicer lists
//! a field's items as buttons and filters one or more pivot tables (or a
//! table, through its AutoFilter) to the selected ones; a timeline filters
//! pivots to a range of dates at some granularity.

use std::collections::BTreeSet;

use crate::address::{Addr, CellRef, SheetId};
use crate::datetime::{datetime_from_serial, serial_from_date};
use crate::formula::Shift;
use crate::pivot::{DateLevel, FieldKey};
use crate::value::Value;
use crate::workbook::Workbook;

/// What a slicer filters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlicerSource {
    /// Pivot tables, by name (they should summarize the same data).
    Pivots(Vec<String>),
    /// A table, by name.
    Table(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Slicer {
    pub caption: String,
    /// The field (column header) whose items it lists.
    pub field: String,
    pub source: SlicerSource,
    /// Items shown (their displayed labels); empty shows everything.
    pub selected: BTreeSet<String>,
    /// The cell the top-left corner sits in, the offset within it, and size, in pixels.
    pub anchor: CellRef,
    pub offset: (f64, f64),
    pub size: (f64, f64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Timeline {
    pub caption: String,
    /// A date field (column header).
    pub field: String,
    pub pivots: Vec<String>,
    pub level: DateLevel,
    /// The selected dates as serials, start inclusive and end exclusive;
    /// None shows all.
    pub range: Option<(f64, f64)>,
    pub anchor: CellRef,
    pub offset: (f64, f64),
    pub size: (f64, f64),
}

/// Move an anchor with inserted/deleted lines on its sheet (a deleted
/// anchor line leaves it where the lines closed up).
fn shift_anchor(anchor: &mut CellRef, op: Shift, home: SheetId) {
    if op.sheet() != home {
        return;
    }
    let rows = matches!(op, Shift::InsertRows { .. } | Shift::DeleteRows { .. });
    let pos = if rows { anchor.row } else { anchor.col };
    let new = op.map_pos(pos).unwrap_or(match op {
        Shift::DeleteRows { at, .. } | Shift::DeleteCols { at, .. } => at,
        _ => pos,
    });
    if rows {
        anchor.row = new;
    } else {
        anchor.col = new;
    }
}

impl Slicer {
    pub fn apply_shift(&mut self, op: Shift, home: SheetId) {
        shift_anchor(&mut self.anchor, op, home);
    }

    pub fn filters_pivot(&self, name: &str) -> bool {
        matches!(&self.source, SlicerSource::Pivots(p) if p.iter().any(|n| n == name))
    }
}

impl Timeline {
    pub fn apply_shift(&mut self, op: Shift, home: SheetId) {
        shift_anchor(&mut self.anchor, op, home);
    }
}

/// A filter a slicer or timeline puts on a pivot, by field name.
#[derive(Clone, Debug, PartialEq)]
pub enum PageFilter {
    /// Keep records whose field shows one of these labels.
    Items(BTreeSet<String>),
    /// Keep records whose field is a date in [start, end).
    Dates(f64, f64),
}

/// The serial of the start of the period holding `serial`.
pub fn period_start(serial: f64, level: DateLevel) -> f64 {
    let Some(d) = datetime_from_serial(serial) else { return serial.floor() };
    match level {
        DateLevel::Years => serial_from_date(d.year, 1, 1),
        DateLevel::Quarters => serial_from_date(d.year, (d.month - 1) / 3 * 3 + 1, 1),
        DateLevel::Months => serial_from_date(d.year, d.month, 1),
        DateLevel::Days => serial.floor(),
    }
}

/// The serial of the start of the period after the one starting at `start`.
pub fn period_next(start: f64, level: DateLevel) -> f64 {
    let Some(d) = datetime_from_serial(start) else { return start + 1.0 };
    let months = match level {
        DateLevel::Days => return start.floor() + 1.0,
        DateLevel::Months => 1,
        DateLevel::Quarters => 3,
        DateLevel::Years => 12,
    };
    let m0 = d.year * 12 + (d.month as i64 - 1) + months;
    serial_from_date(m0.div_euclid(12), (m0.rem_euclid(12) + 1) as u32, 1)
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// How a timeline names the period starting at `start`.
pub fn period_label(start: f64, level: DateLevel) -> String {
    let Some(d) = datetime_from_serial(start) else { return String::new() };
    match level {
        DateLevel::Years => d.year.to_string(),
        DateLevel::Quarters => format!("Q{} {}", (d.month - 1) / 3 + 1, d.year),
        DateLevel::Months => format!("{} {}", MONTHS[d.month as usize - 1], d.year),
        DateLevel::Days => format!("{} {} {}", d.day, MONTHS[d.month as usize - 1], d.year),
    }
}

/// A short form of a date range for a timeline's caption ("Mar 2024 – Jun 2024").
pub fn range_label(range: (f64, f64), level: DateLevel) -> String {
    let first = period_label(range.0, level);
    let last_start = period_start(range.1 - 0.5, level);
    let last = period_label(last_start, level);
    if first == last { first } else { format!("{first} – {last}") }
}

impl Workbook {
    /// The pivot named `name`: its sheet and index there.
    pub fn find_pivot(&self, name: &str) -> Option<(SheetId, usize)> {
        self.sheet_ids().iter().find_map(|&id| self.layout(id).pivots.iter().position(|p| p.name == name).map(|i| (id, i)))
    }

    /// A pivot name not used yet ("PivotTable1", …).
    pub fn free_pivot_name(&self) -> String {
        (1..).map(|i| format!("PivotTable{i}")).find(|n| self.find_pivot(n).is_none()).unwrap_or_default()
    }

    /// The filters slicers and timelines put on the pivot named `name`.
    pub fn pivot_page_filters(&self, name: &str) -> Vec<(String, PageFilter)> {
        let mut out = Vec::new();
        for &id in self.sheet_ids() {
            let l = self.layout(id);
            for s in l.slicers.iter().filter(|s| !s.selected.is_empty() && s.filters_pivot(name)) {
                out.push((s.field.clone(), PageFilter::Items(s.selected.clone())));
            }
            for t in l.timelines.iter().filter(|t| t.pivots.iter().any(|p| p == name)) {
                if let Some((a, b)) = t.range {
                    out.push((t.field.clone(), PageFilter::Dates(a, b)));
                }
            }
        }
        out
    }

    /// The table a table slicer filters: its sheet and index.
    pub fn find_table(&self, name: &str) -> Option<(SheetId, usize)> {
        self.sheet_ids().iter().find_map(|&id| self.layout(id).tables.iter().position(|t| t.name.eq_ignore_ascii_case(name)).map(|i| (id, i)))
    }

    /// A slicer's items, in order. Table items are the cells' displayed
    /// text ("" for blanks); pivot items are labels like the pivot shows.
    pub fn slicer_items(&self, s: &Slicer) -> Vec<String> {
        match &s.source {
            SlicerSource::Pivots(names) => {
                let Some((sheet, i)) = names.iter().find_map(|n| self.find_pivot(n)) else { return Vec::new() };
                let p = &self.layout(sheet).pivots[i];
                let Some(f) = self.pivot_field_index(sheet, p, &s.field) else { return Vec::new() };
                self.pivot_base_items(sheet, p, f)
            }
            SlicerSource::Table(name) => {
                let Some((sheet, i)) = self.find_table(name) else { return Vec::new() };
                let t = &self.layout(sheet).tables[i];
                let r = t.range;
                let Some(col) = (r.start.col..=r.end.col).find(|&c| self.display(Addr::new(sheet, CellRef::new(r.start.row, c))).text.eq_ignore_ascii_case(&s.field)) else { return Vec::new() };
                let last = if t.totals { r.end.row.saturating_sub(1) } else { r.end.row };
                let mut seen: Vec<(FieldKey, String)> = Vec::new();
                for row in r.start.row + 1..=last {
                    let a = Addr::new(sheet, CellRef::new(row, col));
                    let shown = self.display(a).text;
                    if !seen.iter().any(|(_, t)| *t == shown) {
                        seen.push((FieldKey::of(self.value(a), &shown), shown));
                    }
                }
                seen.sort();
                seen.into_iter().map(|(_, t)| t).collect()
            }
        }
    }

    /// The periods a timeline shows: from the first to the last date in its
    /// field, as (label, start, end) serials.
    pub fn timeline_periods(&self, t: &Timeline) -> Vec<(String, f64, f64)> {
        let Some((sheet, i)) = t.pivots.iter().find_map(|n| self.find_pivot(n)) else { return Vec::new() };
        let p = &self.layout(sheet).pivots[i];
        let Some(f) = self.pivot_field_index(sheet, p, &t.field) else { return Vec::new() };
        let Some((lo, hi)) = self.pivot_field_span(sheet, p, f) else { return Vec::new() };
        let mut out = Vec::new();
        let mut start = period_start(lo, t.level);
        while start <= hi && out.len() < 5000 {
            let next = period_next(start, t.level);
            out.push((period_label(start, t.level), start, next));
            start = next;
        }
        out
    }

    /// Point a table's AutoFilter at the selection of the slicers on it.
    pub fn apply_table_slicers(&mut self, table: &str) {
        let Some((sheet, i)) = self.find_table(table) else { return };
        let r = self.layout(sheet).tables[i].range;
        // Column → the slicer's selection (None: the slicer shows everything).
        let mut columns: Vec<(u32, Option<BTreeSet<String>>)> = Vec::new();
        for &id in self.sheet_ids() {
            for s in &self.layout(id).slicers {
                if !matches!(&s.source, SlicerSource::Table(n) if n.eq_ignore_ascii_case(table)) {
                    continue;
                }
                if let Some(col) = (r.start.col..=r.end.col).find(|&c| self.display(Addr::new(sheet, CellRef::new(r.start.row, c))).text.eq_ignore_ascii_case(&s.field)) {
                    columns.push((col, (!s.selected.is_empty()).then(|| s.selected.clone())));
                }
            }
        }
        let Some(l) = self.layout_mut(sheet) else { return };
        let mut f = match l.filter.take() {
            Some(f) if f.range.start == r.start && f.range.end.col == r.end.col => f,
            _ => crate::workbook::AutoFilter { range: r, columns: Default::default() },
        };
        // Columns with a slicer follow it; others keep their filter.
        for (c, set) in columns {
            match set {
                Some(set) => {
                    f.columns.insert(c, set);
                }
                None => {
                    f.columns.remove(&c);
                }
            }
        }
        l.filter = Some(f);
        self.reapply_filter(sheet);
    }
}

/// Is a value a date serial a timeline can place?
pub fn date_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) if datetime_from_serial(*n).is_some() => Some(*n),
        _ => None,
    }
}
