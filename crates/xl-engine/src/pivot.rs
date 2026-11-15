//! Pivot tables: summarize a source range (header row + records) by the
//! distinct values of row and column fields, aggregating value fields. The
//! result is a grid of values written into the sheet.
//!
//! Beyond plain grouping: calculated fields (formulas over other fields'
//! sums), "show values as" (percentages, differences, running totals,
//! ranks), grouping (dates by years/quarters/months/days, numbers into
//! ranges, items into named groups), filters on row/column fields (items,
//! labels, values, top N), sorting by label or value, subtotals, and the
//! filters slicers and timelines put on the pivot.

mod calc;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

pub use calc::CalcExpr;

use crate::address::{Addr, CellRef, Range, SheetId};
use crate::datetime::datetime_from_serial;
use crate::formula::{Expr, Shift};
use crate::slicer::PageFilter;
use crate::value::{ErrorKind, Value};
use crate::workbook::Workbook;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Agg {
    Sum,
    Count,
    Average,
    Min,
    Max,
}

impl Agg {
    pub const ALL: [Agg; 5] = [Agg::Sum, Agg::Count, Agg::Average, Agg::Min, Agg::Max];

    pub fn label(self) -> &'static str {
        match self {
            Agg::Sum => "Sum",
            Agg::Count => "Count",
            Agg::Average => "Average",
            Agg::Min => "Min",
            Agg::Max => "Max",
        }
    }

    pub fn next(self) -> Agg {
        let i = Agg::ALL.iter().position(|a| *a == self).unwrap_or(0);
        Agg::ALL[(i + 1) % Agg::ALL.len()]
    }
}

/// The item a "difference from" compares with, among items of the innermost
/// row field under the same parent.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BaseItem {
    Previous,
    Next,
    /// The item with this label.
    Item(String),
}

/// How a value field's numbers are shown.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum ShowAs {
    #[default]
    Normal,
    PctGrandTotal,
    PctRowTotal,
    PctColTotal,
    /// The difference from another item (along the rows).
    DiffFrom(BaseItem),
    /// The percentage difference from another item.
    PctDiffFrom(BaseItem),
    /// Accumulated down the rows (restarting under each parent).
    RunningTotal,
    /// Rank among the items under the same parent: largest first or smallest first.
    Rank { descending: bool },
}

impl ShowAs {
    /// The choices the field panel cycles through.
    pub const CYCLE: [ShowAs; 9] = [
        ShowAs::Normal,
        ShowAs::PctGrandTotal,
        ShowAs::PctRowTotal,
        ShowAs::PctColTotal,
        ShowAs::DiffFrom(BaseItem::Previous),
        ShowAs::PctDiffFrom(BaseItem::Previous),
        ShowAs::RunningTotal,
        ShowAs::Rank { descending: true },
        ShowAs::Rank { descending: false },
    ];

    pub fn label(&self) -> String {
        let base = |b: &BaseItem| match b {
            BaseItem::Previous => "previous".to_string(),
            BaseItem::Next => "next".to_string(),
            BaseItem::Item(s) => format!("“{s}”"),
        };
        match self {
            ShowAs::Normal => "No calculation".into(),
            ShowAs::PctGrandTotal => "% of grand total".into(),
            ShowAs::PctRowTotal => "% of row total".into(),
            ShowAs::PctColTotal => "% of column total".into(),
            ShowAs::DiffFrom(b) => format!("Difference from {}", base(b)),
            ShowAs::PctDiffFrom(b) => format!("% difference from {}", base(b)),
            ShowAs::RunningTotal => "Running total".into(),
            ShowAs::Rank { descending: true } => "Rank largest to smallest".into(),
            ShowAs::Rank { descending: false } => "Rank smallest to largest".into(),
        }
    }

    pub fn next(&self) -> ShowAs {
        let i = ShowAs::CYCLE.iter().position(|s| s == self).unwrap_or(0);
        ShowAs::CYCLE[(i + 1) % ShowAs::CYCLE.len()].clone()
    }

    pub fn is_percent(&self) -> bool {
        matches!(self, ShowAs::PctGrandTotal | ShowAs::PctRowTotal | ShowAs::PctColTotal | ShowAs::PctDiffFrom(_))
    }
}

/// A field summarized in the values area.
#[derive(Clone, Debug, PartialEq)]
pub struct ValueField {
    /// A source column, or (from the number of source columns on) a calculated field.
    pub field: usize,
    pub agg: Agg,
    pub show: ShowAs,
}

impl ValueField {
    pub fn new(field: usize, agg: Agg) -> Self {
        ValueField { field, agg, show: ShowAs::Normal }
    }
}

impl From<(usize, Agg)> for ValueField {
    fn from((field, agg): (usize, Agg)) -> Self {
        ValueField::new(field, agg)
    }
}

/// A field computed from others, e.g. `Profit = Revenue - Cost`.
#[derive(Clone, Debug, PartialEq)]
pub struct CalcField {
    pub name: String,
    pub formula: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DateLevel {
    Years,
    Quarters,
    Months,
    Days,
}

impl DateLevel {
    pub const ALL: [DateLevel; 4] = [DateLevel::Years, DateLevel::Quarters, DateLevel::Months, DateLevel::Days];

    pub fn label(self) -> &'static str {
        match self {
            DateLevel::Years => "Years",
            DateLevel::Quarters => "Quarters",
            DateLevel::Months => "Months",
            DateLevel::Days => "Days",
        }
    }
}

/// How a field's items are grouped.
#[derive(Clone, Debug, PartialEq)]
pub enum Grouping {
    /// Dates by these levels (outermost first); each is a row/column level.
    Dates(Vec<DateLevel>),
    /// Numbers into ranges of `by` from `start`; values outside go to
    /// "<start" and ">end".
    Numbers { start: f64, end: f64, by: f64 },
    /// Named groups of items (by label); a group level is added above the items.
    Manual(Vec<(String, BTreeSet<String>)>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FieldSort {
    #[default]
    Ascending,
    Descending,
    /// By a value field's totals (index into `values`).
    ByValue { value: usize, descending: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LabelOp {
    Equals,
    NotEquals,
    BeginsWith,
    EndsWith,
    Contains,
    NotContains,
}

impl LabelOp {
    pub const ALL: [LabelOp; 6] = [LabelOp::Equals, LabelOp::NotEquals, LabelOp::BeginsWith, LabelOp::EndsWith, LabelOp::Contains, LabelOp::NotContains];

    pub fn label(self) -> &'static str {
        match self {
            LabelOp::Equals => "equals",
            LabelOp::NotEquals => "does not equal",
            LabelOp::BeginsWith => "begins with",
            LabelOp::EndsWith => "ends with",
            LabelOp::Contains => "contains",
            LabelOp::NotContains => "does not contain",
        }
    }

    fn test(self, label: &str, text: &str) -> bool {
        let (l, t) = (label.to_lowercase(), text.to_lowercase());
        match self {
            LabelOp::Equals => l == t,
            LabelOp::NotEquals => l != t,
            LabelOp::BeginsWith => l.starts_with(&t),
            LabelOp::EndsWith => l.ends_with(&t),
            LabelOp::Contains => l.contains(&t),
            LabelOp::NotContains => !l.contains(&t),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ValueOp {
    Greater,
    GreaterEq,
    Less,
    LessEq,
    Equal,
    NotEqual,
    Between,
    NotBetween,
}

impl ValueOp {
    pub const ALL: [ValueOp; 8] = [ValueOp::Greater, ValueOp::GreaterEq, ValueOp::Less, ValueOp::LessEq, ValueOp::Equal, ValueOp::NotEqual, ValueOp::Between, ValueOp::NotBetween];

    pub fn symbol(self) -> &'static str {
        match self {
            ValueOp::Greater => ">",
            ValueOp::GreaterEq => ">=",
            ValueOp::Less => "<",
            ValueOp::LessEq => "<=",
            ValueOp::Equal => "=",
            ValueOp::NotEqual => "<>",
            ValueOp::Between => "between",
            ValueOp::NotBetween => "not between",
        }
    }

    fn test(self, v: f64, a: f64, b: f64) -> bool {
        let (lo, hi) = (a.min(b), a.max(b));
        match self {
            ValueOp::Greater => v > a,
            ValueOp::GreaterEq => v >= a,
            ValueOp::Less => v < a,
            ValueOp::LessEq => v <= a,
            ValueOp::Equal => v == a,
            ValueOp::NotEqual => v != a,
            ValueOp::Between => (lo..=hi).contains(&v),
            ValueOp::NotBetween => !(lo..=hi).contains(&v),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LabelFilter {
    pub op: LabelOp,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValueFilter {
    /// Index into the pivot's `values`.
    pub value: usize,
    pub op: ValueOp,
    pub a: f64,
    pub b: f64,
}

/// Keep the `n` items with the largest (or smallest) totals.
#[derive(Clone, Debug, PartialEq)]
pub struct TopFilter {
    pub n: usize,
    pub bottom: bool,
    /// Index into the pivot's `values`.
    pub value: usize,
}

/// Filters on a row or column field.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FieldFilter {
    /// Item labels (at any of the field's levels) left out.
    pub hidden: BTreeSet<String>,
    pub label: Option<LabelFilter>,
    pub value: Option<ValueFilter>,
    pub top: Option<TopFilter>,
}

impl FieldFilter {
    pub fn is_empty(&self) -> bool {
        self.hidden.is_empty() && self.label.is_none() && self.value.is_none() && self.top.is_none()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pivot {
    /// Unique in the workbook; slicers, timelines and pivot charts use it.
    pub name: String,
    /// The source data, header row first (usually on another sheet).
    pub source: Arc<Expr>,
    /// Top-left cell of the output on the pivot's own sheet.
    pub anchor: CellRef,
    /// Fields by index into the source's columns.
    pub rows: Vec<usize>,
    pub cols: Vec<usize>,
    pub values: Vec<ValueField>,
    /// Where the last output was written (to clear it on refresh).
    pub extent: Option<Range>,
    pub calculated: Vec<CalcField>,
    pub groups: BTreeMap<usize, Grouping>,
    pub filters: BTreeMap<usize, FieldFilter>,
    pub sorts: BTreeMap<usize, FieldSort>,
    /// A total row under each group of an outer row field.
    pub subtotals: bool,
}

impl Pivot {
    pub fn new(source: Arc<Expr>, anchor: CellRef) -> Self {
        Pivot {
            name: String::new(),
            source,
            anchor,
            rows: Vec::new(),
            cols: Vec::new(),
            values: Vec::new(),
            extent: None,
            calculated: Vec::new(),
            groups: BTreeMap::new(),
            filters: BTreeMap::new(),
            sorts: BTreeMap::new(),
            subtotals: false,
        }
    }

    pub(crate) fn apply_shift(&mut self, op: Shift, home: SheetId) {
        self.source = Arc::new(self.source.shifted(op, home));
        if op.sheet() != home {
            return;
        }
        let rows = matches!(op, Shift::InsertRows { .. } | Shift::DeleteRows { .. });
        let map = |c: CellRef| -> Option<CellRef> { Some(if rows { CellRef::new(op.map_pos(c.row)?, c.col) } else { CellRef::new(c.row, op.map_pos(c.col)?) }) };
        match map(self.anchor) {
            Some(a) => {
                self.anchor = a;
                self.extent = self.extent.and_then(|e| Some(Range::new(map(e.start)?, map(e.end)?)));
            }
            // The output's corner was deleted: start over where the lines closed up.
            None => {
                self.extent = None;
                if let Shift::DeleteRows { at, .. } | Shift::DeleteCols { at, .. } = op {
                    if rows {
                        self.anchor.row = at;
                    } else {
                        self.anchor.col = at;
                    }
                }
            }
        }
    }
}

/// Running aggregate for one group.
#[derive(Clone, Copy, Default, Debug)]
struct Acc {
    sum: f64,
    count: usize,
    numbers: usize,
    min: f64,
    max: f64,
}

impl Acc {
    fn add(&mut self, v: &Value) {
        if v.is_empty() {
            return;
        }
        self.count += 1;
        if let Value::Number(n) = v {
            if self.numbers == 0 {
                (self.min, self.max) = (*n, *n);
            }
            self.numbers += 1;
            self.sum += n;
            self.min = self.min.min(*n);
            self.max = self.max.max(*n);
        }
    }

    fn merge(&mut self, o: &Acc) {
        if o.numbers > 0 {
            if self.numbers == 0 {
                (self.min, self.max) = (o.min, o.max);
            }
            self.min = self.min.min(o.min);
            self.max = self.max.max(o.max);
        }
        self.sum += o.sum;
        self.count += o.count;
        self.numbers += o.numbers;
    }

    fn result(&self, agg: Agg) -> Value {
        match agg {
            Agg::Count => Value::Number(self.count as f64),
            _ if self.numbers == 0 => Value::Empty,
            Agg::Sum => Value::Number(self.sum),
            Agg::Average => Value::Number(self.sum / self.numbers as f64),
            Agg::Min => Value::Number(self.min),
            Agg::Max => Value::Number(self.max),
        }
    }
}

/// An item's sort key: numbers before text (case-insensitively), blanks last.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FieldKey {
    Num(i64),
    Text(String, String),
    Blank,
}

impl FieldKey {
    pub fn of(v: &Value, shown: &str) -> FieldKey {
        match v {
            Value::Empty => FieldKey::Blank,
            Value::Number(n) => num_key(*n),
            _ => FieldKey::Text(shown.to_lowercase(), shown.to_string()),
        }
    }
}

/// Order numbers by value; f64 bits make them totally ordered.
fn num_key(n: f64) -> FieldKey {
    let bits = n.to_bits() as i64;
    FieldKey::Num(if bits < 0 { i64::MIN - bits } else { bits })
}

/// One level of an item: how it sorts and what it shows. Compared by key.
#[derive(Clone, Debug)]
struct Part {
    key: FieldKey,
    label: String,
}

impl PartialEq for Part {
    fn eq(&self, o: &Self) -> bool {
        self.key == o.key
    }
}
impl Eq for Part {}
impl std::hash::Hash for Part {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.key.hash(h)
    }
}
impl PartialOrd for Part {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Part {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.key.cmp(&o.key)
    }
}

fn plain_part(v: &Value, shown: String) -> Part {
    let key = FieldKey::of(v, &shown);
    let label = if key == FieldKey::Blank { "(blank)".to_string() } else { shown };
    Part { key, label }
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// A level of a (possibly grouped) field.
#[derive(Clone, Debug)]
enum Level {
    Plain,
    Date(DateLevel),
    Range { start: f64, end: f64, by: f64 },
    /// The group above manually grouped items.
    Group,
}

fn date_part(n: f64, level: DateLevel) -> Option<Part> {
    let d = datetime_from_serial(n)?;
    let (key, label) = match level {
        DateLevel::Years => (d.year, d.year.to_string()),
        DateLevel::Quarters => {
            let q = (d.month as i64 - 1) / 3 + 1;
            (q, format!("Qtr{q}"))
        }
        DateLevel::Months => (d.month as i64, MONTHS[d.month as usize - 1].to_string()),
        DateLevel::Days => (d.month as i64 * 100 + d.day as i64, format!("{}-{}", d.day, MONTHS[d.month as usize - 1])),
    };
    Some(Part { key: num_key(key as f64), label })
}

fn range_part(n: f64, start: f64, end: f64, by: f64) -> Part {
    let p = crate::workbook::precise;
    if n < start {
        return Part { key: num_key(f64::NEG_INFINITY), label: format!("<{}", p(start)) };
    }
    if n > end {
        return Part { key: num_key(f64::INFINITY), label: format!(">{}", p(end)) };
    }
    let by = if by > 0.0 { by } else { 1.0 };
    let lo = start + ((n - start) / by).floor() * by;
    let label = if start.fract() == 0.0 && by.fract() == 0.0 { format!("{}-{}", p(lo), p(lo + by - 1.0)) } else { format!("{}-{}", p(lo), p(lo + by)) };
    Part { key: num_key(lo), label }
}

/// The kind of data in a field, for choosing how it can be grouped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Number,
    Date,
}

/// A pivot's computed output with what's needed to write and chart it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PivotOutput {
    /// Rows of cells, header first.
    pub grid: Vec<Vec<Value>>,
    /// Columns of labels at the left.
    pub label_cols: usize,
    /// Rows drawn bold: the header, subtotals and the grand total.
    pub bold_rows: Vec<usize>,
    /// The number format to give each value column (None: the source's).
    pub col_formats: Vec<Option<&'static str>>,
    /// The value field (index into `values`) each column shows.
    pub col_values: Vec<Option<usize>>,
    /// Rows of items (not the header or grand total), for charts: first and last.
    pub body: Option<(usize, usize)>,
    /// Columns a pivot chart plots (one series each; totals left out).
    pub series_cols: Vec<usize>,
}

/// The pivot's source: a sheet and range, with records up to `last`.
struct Src<'a> {
    wb: &'a Workbook,
    sheet: SheetId,
    range: Range,
    last: u32,
}

impl Src<'_> {
    fn addr(&self, row: u32, f: usize) -> Addr {
        Addr::new(self.sheet, CellRef::new(row, self.range.start.col + f as u32))
    }

    fn value(&self, row: u32, f: usize) -> &Value {
        self.wb.value(self.addr(row, f))
    }

    fn shown(&self, row: u32, f: usize) -> String {
        self.wb.display(self.addr(row, f)).text
    }

    /// Rows of records (blank rows skipped).
    fn records(&self) -> impl Iterator<Item = u32> + '_ {
        (self.range.start.row + 1..=self.last).filter(|&row| (self.range.start.col..=self.range.end.col).any(|c| !self.wb.value(Addr::new(self.sheet, CellRef::new(row, c))).is_empty()))
    }
}

/// A calculated field ready to evaluate: its formula and the source
/// columns its names stand for.
struct CalcPlan {
    expr: Result<CalcExpr, String>,
    refs: Vec<(String, usize)>,
}

/// What a value field needs while computing.
struct Plan {
    nsrc: usize,
    /// Accumulator slot of each source field that's needed.
    slot: HashMap<usize, usize>,
    calcs: Vec<CalcPlan>,
}

impl Plan {
    fn result(&self, vf: &ValueField, accs: &[Acc]) -> Value {
        if vf.field < self.nsrc {
            return self.slot.get(&vf.field).map_or(Value::Empty, |&s| accs[s].result(vf.agg));
        }
        let Some(c) = self.calcs.get(vf.field - self.nsrc) else { return Value::Error(ErrorKind::Ref) };
        let Ok(expr) = &c.expr else { return Value::Error(ErrorKind::Name) };
        let lookup = |name: &str| -> Option<f64> {
            let (_, f) = c.refs.iter().find(|(n, _)| n.eq_ignore_ascii_case(name))?;
            Some(self.slot.get(f).map_or(0.0, |&s| accs[s].sum))
        };
        match expr.eval(&lookup) {
            Ok(n) => Value::Number(n),
            Err(e) => Value::Error(e),
        }
    }
}

fn num(v: &Value) -> Option<f64> {
    if let Value::Number(n) = v { Some(*n) } else { None }
}

/// (row prefix, column key or None for all columns): where values accumulate.
type CellKey = (Vec<Part>, Option<Vec<Part>>);

/// A row of the output.
#[derive(Clone, Debug)]
enum RowKind {
    Leaf(Vec<Part>),
    Subtotal(Vec<Part>),
    Grand,
}

impl Workbook {
    fn pivot_src(&self, home: SheetId, p: &Pivot) -> Option<Src<'_>> {
        let (sheet, r) = p.source.reference()?;
        let sheet = sheet.unwrap_or(home);
        // Records stop at the used area (whole-column sources).
        let last = self.store(sheet).extent().map_or(r.end.row, |e| r.end.row.min(e.row));
        Some(Src { wb: self, sheet, range: r, last })
    }

    /// The source's column headers.
    pub fn pivot_source_fields(&self, home: SheetId, p: &Pivot) -> Vec<String> {
        let Some((sheet, r)) = p.source.reference() else { return Vec::new() };
        let sheet = sheet.unwrap_or(home);
        (r.start.col..=r.end.col).map(|c| self.display(Addr::new(sheet, CellRef::new(r.start.row, c))).text).map(|h| if h.is_empty() { "(blank)".into() } else { h }).collect()
    }

    /// Every field: the source's column headers, then calculated fields.
    pub fn pivot_fields(&self, home: SheetId, p: &Pivot) -> Vec<String> {
        let mut out = self.pivot_source_fields(home, p);
        out.extend(p.calculated.iter().map(|c| c.name.clone()));
        out
    }

    /// A field's index by name (case-insensitive).
    pub fn pivot_field_index(&self, home: SheetId, p: &Pivot, name: &str) -> Option<usize> {
        self.pivot_fields(home, p).iter().position(|f| f.eq_ignore_ascii_case(name))
    }

    /// What a source field holds: dates (numbers with a date format), numbers or text.
    pub fn pivot_field_kind(&self, home: SheetId, p: &Pivot, f: usize) -> FieldKind {
        let Some(src) = self.pivot_src(home, p) else { return FieldKind::Text };
        if f >= src.range.cols() as usize {
            return FieldKind::Number;
        }
        let mut kind = None;
        for row in src.records() {
            match src.value(row, f) {
                Value::Empty => {}
                Value::Number(_) => {
                    if kind.is_none() {
                        kind = Some(if self.format(src.addr(row, f)).is_some_and(|x| x.is_date()) { FieldKind::Date } else { FieldKind::Number });
                    }
                }
                _ => return FieldKind::Text,
            }
        }
        kind.unwrap_or(FieldKind::Text)
    }

    /// The smallest and largest number in a field (a date field's span).
    pub fn pivot_field_span(&self, home: SheetId, p: &Pivot, f: usize) -> Option<(f64, f64)> {
        let src = self.pivot_src(home, p)?;
        let mut span: Option<(f64, f64)> = None;
        for row in src.records() {
            if let Some(n) = crate::slicer::date_of(src.value(row, f)) {
                span = Some(span.map_or((n, n), |(a, b)| (a.min(n), b.max(n))));
            }
        }
        span
    }

    /// A field's distinct items as shown in the source, ungrouped and
    /// unfiltered (what a slicer lists).
    pub fn pivot_base_items(&self, home: SheetId, p: &Pivot, f: usize) -> Vec<String> {
        let Some(src) = self.pivot_src(home, p) else { return Vec::new() };
        if f >= src.range.cols() as usize {
            return Vec::new();
        }
        let set: BTreeSet<Part> = src.records().map(|row| plain_part(src.value(row, f), src.shown(row, f))).collect();
        set.into_iter().map(|p| p.label).collect()
    }

    /// A field's items at each of its levels (after grouping), unfiltered:
    /// (level name, labels in order).
    pub fn pivot_field_items(&self, home: SheetId, p: &Pivot, f: usize) -> Vec<(String, Vec<String>)> {
        let Some(src) = self.pivot_src(home, p) else { return Vec::new() };
        if f >= src.range.cols() as usize {
            return Vec::new();
        }
        let fields = self.pivot_source_fields(home, p);
        let levels = levels_of(p, f);
        let mut sets: Vec<BTreeSet<Part>> = vec![BTreeSet::new(); levels.len()];
        for row in src.records() {
            for (i, l) in levels.iter().enumerate() {
                sets[i].insert(part_of(p, &src, row, f, l));
            }
        }
        levels.iter().zip(sets).map(|(l, s)| (level_name(&fields, f, l, &levels), s.into_iter().map(|p| p.label).collect())).collect()
    }

    /// Compute a pivot's output grid (rows of cells), header first.
    pub fn pivot_grid(&self, home: SheetId, p: &Pivot) -> Vec<Vec<Value>> {
        self.pivot_table(home, p).grid
    }

    /// Compute a pivot's output.
    pub fn pivot_table(&self, home: SheetId, p: &Pivot) -> PivotOutput {
        let page = self.pivot_page_filters(&p.name);
        self.pivot_table_with(home, p, &page)
    }

    /// Compute a pivot's output with these slicer/timeline filters (by field name).
    pub fn pivot_table_with(&self, home: SheetId, p: &Pivot, page: &[(String, PageFilter)]) -> PivotOutput {
        let Some(src) = self.pivot_src(home, p) else {
            return PivotOutput { grid: vec![vec![Value::Error(ErrorKind::Ref)]], label_cols: 1, ..Default::default() };
        };
        let fields = self.pivot_source_fields(home, p);
        let nsrc = fields.len();
        let all_names = self.pivot_fields(home, p);
        let field_of = |name: &str| fields.iter().position(|f| f.eq_ignore_ascii_case(name));

        // Accumulators: one per source field a value (or calculated field) reads.
        let calcs: Vec<CalcPlan> = p
            .calculated
            .iter()
            .map(|c| {
                let expr = CalcExpr::parse(&c.formula);
                let refs = expr.as_ref().map(|e| e.names().into_iter().filter_map(|n| field_of(&n).map(|f| (n, f))).collect()).unwrap_or_default();
                CalcPlan { expr, refs }
            })
            .collect();
        let mut needed: BTreeSet<usize> = BTreeSet::new();
        for v in &p.values {
            if v.field < nsrc {
                needed.insert(v.field);
            } else if let Some(c) = calcs.get(v.field - nsrc) {
                needed.extend(c.refs.iter().map(|(_, f)| *f));
            }
        }
        let slot: HashMap<usize, usize> = needed.iter().enumerate().map(|(i, &f)| (f, i)).collect();
        let nslots = slot.len();
        let plan = Plan { nsrc, slot, calcs };

        // Record filters: slicers/timelines, then item and label filters.
        let page: Vec<(usize, &PageFilter)> = page.iter().filter_map(|(name, f)| Some((field_of(name)?, f))).collect();
        let mut keep: Vec<u32> = src
            .records()
            .filter(|&row| {
                page.iter().all(|(f, filter)| match filter {
                    PageFilter::Items(set) => set.contains(&plain_part(src.value(row, *f), src.shown(row, *f)).label),
                    PageFilter::Dates(a, b) => crate::slicer::date_of(src.value(row, *f)).is_some_and(|n| n >= *a && n < *b),
                })
            })
            .filter(|&row| {
                p.filters.iter().filter(|(f, _)| **f < nsrc).all(|(&f, filter)| {
                    let parts: Vec<Part> = levels_of(p, f).iter().map(|l| part_of(p, &src, row, f, l)).collect();
                    if parts.iter().any(|x| filter.hidden.contains(&x.label)) {
                        return false;
                    }
                    filter.label.as_ref().is_none_or(|lf| parts.last().is_some_and(|x| lf.op.test(&x.label, &lf.text)))
                })
            })
            .collect();
        // Value and top-N filters keep the items of a field whose totals pass.
        for (&f, filter) in p.filters.iter().filter(|(f, flt)| **f < nsrc && (flt.value.is_some() || flt.top.is_some())) {
            let levels = levels_of(p, f);
            let item = |row: u32| -> Vec<Part> { levels.iter().map(|l| part_of(p, &src, row, f, l)).collect() };
            let mut totals: HashMap<Vec<Part>, Vec<Acc>> = HashMap::new();
            for &row in &keep {
                let accs = totals.entry(item(row)).or_insert_with(|| vec![Acc::default(); nslots]);
                for (&field, &s) in &plan.slot {
                    accs[s].add(src.value(row, field));
                }
            }
            let value_of = |accs: &[Acc], vi: usize| p.values.get(vi).map(|vf| plan.result(vf, accs)).as_ref().and_then(num);
            let mut allowed: HashSet<Vec<Part>> = totals.keys().cloned().collect();
            if let Some(vf) = &filter.value {
                allowed.retain(|k| value_of(&totals[k], vf.value).is_some_and(|v| vf.op.test(v, vf.a, vf.b)));
            }
            if let Some(top) = &filter.top {
                let mut ranked: Vec<(&Vec<Part>, f64)> = totals.iter().filter(|(k, _)| allowed.contains(*k)).filter_map(|(k, a)| Some((k, value_of(a, top.value)?))).collect();
                ranked.sort_by(|a, b| if top.bottom { a.1.total_cmp(&b.1) } else { b.1.total_cmp(&a.1) }.then_with(|| a.0.cmp(b.0)));
                allowed = ranked.into_iter().take(top.n).map(|(k, _)| k.clone()).collect();
            }
            keep.retain(|&row| allowed.contains(&item(row)));
        }

        // Group the records.
        let row_levels: Vec<(usize, Level)> = p.rows.iter().filter(|&&f| f < nsrc).flat_map(|&f| levels_of(p, f).into_iter().map(move |l| (f, l))).collect();
        let col_levels: Vec<(usize, Level)> = p.cols.iter().filter(|&&f| f < nsrc).flat_map(|&f| levels_of(p, f).into_iter().map(move |l| (f, l))).collect();
        let key_for = |row: u32, levels: &[(usize, Level)]| -> Vec<Part> { levels.iter().map(|(f, l)| part_of(p, &src, row, *f, l)).collect() };
        // (row prefix, column key or None for all columns) → accumulators.
        let mut cells: HashMap<CellKey, Vec<Acc>> = HashMap::new();
        let mut row_keys: BTreeSet<Vec<Part>> = BTreeSet::new();
        let mut col_keys: BTreeSet<Vec<Part>> = BTreeSet::new();
        let nl = row_levels.len();
        for &row in &keep {
            let rk = key_for(row, &row_levels);
            let ck = key_for(row, &col_levels);
            let mut accs = vec![Acc::default(); nslots];
            for (&field, &s) in &plan.slot {
                accs[s].add(src.value(row, field));
            }
            for len in 0..=nl {
                for c in [Some(ck.clone()), None] {
                    let e = cells.entry((rk[..len].to_vec(), c)).or_insert_with(|| vec![Acc::default(); nslots]);
                    for (x, a) in e.iter_mut().zip(&accs) {
                        x.merge(a);
                    }
                }
            }
            row_keys.insert(rk);
            col_keys.insert(ck);
        }
        if row_keys.is_empty() && nl == 0 {
            row_keys.insert(Vec::new());
        }
        let empty = vec![Acc::default(); nslots];
        let accs_at = |rp: &[Part], ck: Option<&Vec<Part>>| cells.get(&(rp.to_vec(), ck.cloned())).unwrap_or(&empty);
        let sort_value = |accs: &[Acc], vi: usize| p.values.get(vi).map(|vf| plan.result(vf, accs)).as_ref().and_then(num).unwrap_or(f64::NEG_INFINITY);

        // Columns, in order.
        let mut col_keys: Vec<Vec<Part>> = col_keys.into_iter().collect();
        col_keys.sort_by(|a, b| {
            let Some(i) = (0..a.len().min(b.len())).find(|&i| a[i] != b[i]) else { return a.len().cmp(&b.len()) };
            match p.sorts.get(&col_levels[i].0).copied().unwrap_or_default() {
                FieldSort::Ascending => a[i].cmp(&b[i]),
                FieldSort::Descending => b[i].cmp(&a[i]),
                FieldSort::ByValue { value, descending } => {
                    let (x, y) = (sort_value(accs_at(&[], Some(a)), value), sort_value(accs_at(&[], Some(b)), value));
                    if descending { y.total_cmp(&x) } else { x.total_cmp(&y) }.then_with(|| a[i].cmp(&b[i]))
                }
            }
        });

        // Rows, in order: a tree of row levels, each level sorted by its field.
        let row_keys: Vec<Vec<Part>> = row_keys.into_iter().collect();
        let mut order: Vec<RowKind> = Vec::new();
        #[allow(clippy::too_many_arguments)]
        fn walk(keys: &[Vec<Part>], depth: usize, prefix: &mut Vec<Part>, levels: &[(usize, Level)], p: &Pivot, value: &dyn Fn(&[Part], usize) -> f64, out: &mut Vec<RowKind>) {
            if depth == levels.len() {
                out.push(RowKind::Leaf(prefix.clone()));
                return;
            }
            let mut children: Vec<(Part, &[Vec<Part>])> = Vec::new();
            let mut i = 0;
            while i < keys.len() {
                let mut j = i + 1;
                while j < keys.len() && keys[j][depth] == keys[i][depth] {
                    j += 1;
                }
                children.push((keys[i][depth].clone(), &keys[i..j]));
                i = j;
            }
            match p.sorts.get(&levels[depth].0).copied().unwrap_or_default() {
                FieldSort::Ascending => {}
                FieldSort::Descending => children.reverse(),
                FieldSort::ByValue { value: vi, descending } => children.sort_by(|a, b| {
                    let mut pa = prefix.clone();
                    pa.push(a.0.clone());
                    let mut pb = prefix.clone();
                    pb.push(b.0.clone());
                    let (x, y) = (value(&pa, vi), value(&pb, vi));
                    if descending { y.total_cmp(&x) } else { x.total_cmp(&y) }.then_with(|| a.0.cmp(&b.0))
                }),
            }
            for (part, sub) in children {
                prefix.push(part);
                walk(sub, depth + 1, prefix, levels, p, value, out);
                if p.subtotals && depth + 1 < levels.len() {
                    out.push(RowKind::Subtotal(prefix.clone()));
                }
                prefix.pop();
            }
        }
        let value_at = |rp: &[Part], vi: usize| sort_value(accs_at(rp, None), vi);
        walk(&row_keys, 0, &mut Vec::new(), &row_levels, p, &value_at, &mut order);
        if nl > 0 {
            order.push(RowKind::Grand);
        }

        // Values: per output row, per column slot (column keys, then the total), per value field.
        let with_cols = !col_levels.is_empty();
        let slots: Vec<Option<&Vec<Part>>> = if with_cols { col_keys.iter().map(Some).chain(std::iter::once(None)).collect() } else { vec![None] };
        let nv = p.values.len();
        let row_prefix = |k: &RowKind| -> Vec<Part> {
            match k {
                RowKind::Leaf(x) | RowKind::Subtotal(x) => x.clone(),
                RowKind::Grand => Vec::new(),
            }
        };
        let raw = |rp: &[Part]| -> Vec<Vec<Value>> { slots.iter().map(|ck| p.values.iter().map(|vf| plan.result(vf, accs_at(rp, *ck))).collect()).collect() };
        let mut vals: Vec<Vec<Vec<Value>>> = order.iter().map(|k| raw(&row_prefix(k))).collect();
        let grand = raw(&[]);
        let total_slot = slots.len() - 1;
        for vi in 0..nv {
            let show = &p.values[vi].show;
            match show {
                ShowAs::Normal => {}
                ShowAs::PctGrandTotal | ShowAs::PctRowTotal | ShowAs::PctColTotal => {
                    for row in vals.iter_mut() {
                        let row_total = row[total_slot][vi].clone();
                        for (s, cell) in row.iter_mut().enumerate() {
                            let denom = match show {
                                ShowAs::PctGrandTotal => &grand[total_slot][vi],
                                ShowAs::PctRowTotal => &row_total,
                                _ => &grand[s][vi],
                            };
                            cell[vi] = ratio(&cell[vi], denom);
                        }
                    }
                }
                _ => {
                    // Along the rows: items of the innermost row field under the same parent.
                    let mut groups: Vec<Vec<usize>> = Vec::new();
                    let mut last_parent: Option<Vec<Part>> = None;
                    for (i, k) in order.iter().enumerate() {
                        if let RowKind::Leaf(key) = k {
                            let parent = key[..key.len().saturating_sub(1)].to_vec();
                            if last_parent.as_ref() != Some(&parent) || groups.is_empty() {
                                groups.push(Vec::new());
                                last_parent = Some(parent);
                            }
                            groups.last_mut().expect("pushed").push(i);
                        }
                    }
                    let mut out: Vec<Vec<Value>> = vals.iter().map(|row| row.iter().map(|_| Value::Empty).collect()).collect();
                    for g in &groups {
                        for s in 0..slots.len() {
                            let series: Vec<Value> = g.iter().map(|&i| vals[i][s][vi].clone()).collect();
                            let shown = show_along(show, &series, &|j| match &order[g[j]] {
                                RowKind::Leaf(k) => k.last().map(|x| x.label.clone()).unwrap_or_default(),
                                _ => String::new(),
                            });
                            for (j, v) in shown.into_iter().enumerate() {
                                out[g[j]][s] = v;
                            }
                        }
                    }
                    for (row, o) in vals.iter_mut().zip(out) {
                        for (cell, v) in row.iter_mut().zip(o) {
                            cell[vi] = v;
                        }
                    }
                }
            }
        }

        // Lay out the grid.
        let level_names: Vec<String> = row_levels.iter().map(|(f, l)| level_name(&fields, *f, l, &levels_of(p, *f))).collect();
        let value_title = |vf: &ValueField| {
            let name = all_names.get(vf.field).cloned().unwrap_or_default();
            if vf.field >= nsrc { format!("Sum of {name}") } else { format!("{} of {name}", vf.agg.label()) }
        };
        let label_cols = nl.max(1);
        let mut grid: Vec<Vec<Value>> = Vec::new();
        let mut header: Vec<Value> = level_names.iter().map(|n| Value::text(n.clone())).collect();
        if header.is_empty() {
            header.push(Value::Empty);
        }
        let mut series_cols = Vec::new();
        let mut col_values = vec![None; label_cols];
        for ck in col_keys.iter().filter(|_| with_cols) {
            let label = ck.iter().map(|x| x.label.clone()).collect::<Vec<_>>().join(" / ");
            for (vi, v) in p.values.iter().enumerate() {
                series_cols.push(header.len());
                col_values.push(Some(vi));
                header.push(Value::text(if nv > 1 { format!("{label} – {}", value_title(v)) } else { label.clone() }));
            }
        }
        for (vi, v) in p.values.iter().enumerate() {
            if !with_cols {
                series_cols.push(header.len());
            }
            col_values.push(Some(vi));
            header.push(Value::text(if with_cols { format!("Total {}", value_title(v)) } else { value_title(v) }));
        }
        grid.push(header);
        let mut bold_rows = vec![0];
        for (k, row_vals) in order.iter().zip(&vals) {
            let mut line: Vec<Value> = match k {
                RowKind::Leaf(key) if key.is_empty() => vec![Value::text("Total")],
                RowKind::Leaf(key) => key.iter().map(|x| Value::text(x.label.clone())).collect(),
                RowKind::Subtotal(prefix) => {
                    bold_rows.push(grid.len());
                    let mut l: Vec<Value> = prefix.iter().map(|x| Value::text(x.label.clone())).collect();
                    if let Some(last) = l.last_mut() {
                        *last = Value::text(format!("{} Total", prefix.last().map(|x| x.label.as_str()).unwrap_or_default()));
                    }
                    l.resize(label_cols, Value::Empty);
                    l
                }
                RowKind::Grand => {
                    bold_rows.push(grid.len());
                    let mut l = vec![Value::text("Grand Total")];
                    l.resize(label_cols, Value::Empty);
                    l
                }
            };
            for slot in row_vals {
                line.extend(slot.iter().cloned());
            }
            grid.push(line);
        }
        let body_end = if nl > 0 { grid.len() - 2 } else { grid.len() - 1 };
        let body = (grid.len() > 1 && body_end >= 1).then_some((1, body_end));
        let col_formats = col_values
            .iter()
            .map(|v| {
                v.and_then(|vi| match &p.values[vi].show {
                    s if s.is_percent() => Some("0.00%"),
                    ShowAs::Rank { .. } => Some("0"),
                    _ => None,
                })
            })
            .collect();
        PivotOutput { grid, label_cols, bold_rows, col_formats, col_values, body, series_cols }
    }
}

/// `v / denom` for "% of" displays.
fn ratio(v: &Value, denom: &Value) -> Value {
    match (v, num(denom)) {
        (Value::Number(_), Some(0.0)) | (Value::Number(_), None) => Value::Error(ErrorKind::Div0),
        (Value::Number(x), Some(d)) => Value::Number(x / d),
        (other, _) => other.clone(),
    }
}

/// A "show values as" that runs along a list of sibling items.
fn show_along(show: &ShowAs, series: &[Value], label: &dyn Fn(usize) -> String) -> Vec<Value> {
    let n = series.len();
    let base_index = |b: &BaseItem, j: usize| -> Option<Option<usize>> {
        match b {
            BaseItem::Previous => Some(j.checked_sub(1)),
            BaseItem::Next => Some((j + 1 < n).then_some(j + 1)),
            BaseItem::Item(s) => (0..n).find(|&i| label(i).eq_ignore_ascii_case(s)).map(Some),
        }
    };
    match show {
        ShowAs::DiffFrom(b) | ShowAs::PctDiffFrom(b) => (0..n)
            .map(|j| match base_index(b, j) {
                None => Value::Error(ErrorKind::NA),
                Some(None) => Value::Empty,
                Some(Some(i)) if i == j => Value::Empty,
                Some(Some(i)) => match (num(&series[j]), num(&series[i])) {
                    (Some(x), Some(y)) if matches!(show, ShowAs::DiffFrom(_)) => Value::Number(x - y),
                    (Some(_), Some(0.0)) => Value::Error(ErrorKind::Div0),
                    (Some(x), Some(y)) => Value::Number((x - y) / y),
                    (Some(x), None) if matches!(show, ShowAs::DiffFrom(_)) => Value::Number(x),
                    (Some(_), None) => Value::Error(ErrorKind::Div0),
                    (None, Some(y)) if matches!(show, ShowAs::DiffFrom(_)) => Value::Number(-y),
                    (None, _) => Value::Empty,
                },
            })
            .collect(),
        ShowAs::RunningTotal => {
            let mut total = 0.0;
            let mut any = false;
            series
                .iter()
                .map(|v| {
                    if let Some(x) = num(v) {
                        total += x;
                        any = true;
                    }
                    if any { Value::Number(total) } else { Value::Empty }
                })
                .collect()
        }
        ShowAs::Rank { descending } => series
            .iter()
            .map(|v| match num(v) {
                Some(x) => Value::Number(1.0 + series.iter().filter_map(num).filter(|&y| if *descending { y > x } else { y < x }).count() as f64),
                None => Value::Empty,
            })
            .collect(),
        _ => series.to_vec(),
    }
}

/// A field's levels (one unless it's grouped by several date parts or manually).
fn levels_of(p: &Pivot, f: usize) -> Vec<Level> {
    match p.groups.get(&f) {
        Some(Grouping::Dates(lv)) if !lv.is_empty() => {
            let mut lv = lv.clone();
            lv.sort();
            lv.dedup();
            lv.into_iter().map(Level::Date).collect()
        }
        Some(Grouping::Numbers { start, end, by }) => vec![Level::Range { start: *start, end: *end, by: *by }],
        Some(Grouping::Manual(g)) if !g.is_empty() => vec![Level::Group, Level::Plain],
        _ => vec![Level::Plain],
    }
}

/// A record's item at one level of a field.
fn part_of(p: &Pivot, src: &Src, row: u32, f: usize, level: &Level) -> Part {
    let v = src.value(row, f);
    match (level, v) {
        (Level::Date(d), Value::Number(n)) => date_part(*n, *d).unwrap_or_else(|| plain_part(v, src.shown(row, f))),
        (Level::Range { start, end, by }, Value::Number(n)) => range_part(*n, *start, *end, *by),
        (Level::Group, _) => {
            let item = plain_part(v, src.shown(row, f));
            match p.groups.get(&f) {
                Some(Grouping::Manual(groups)) => match groups.iter().find(|(_, items)| items.contains(&item.label)) {
                    Some((name, _)) => Part { key: FieldKey::Text(name.to_lowercase(), name.clone()), label: name.clone() },
                    None => item,
                },
                _ => item,
            }
        }
        _ => plain_part(v, src.shown(row, f)),
    }
}

/// The header shown for a level: extra date levels and manual groups get
/// their own names; the finest level keeps the field's.
fn level_name(fields: &[String], f: usize, level: &Level, all: &[Level]) -> String {
    let name = fields.get(f).cloned().unwrap_or_default();
    match level {
        Level::Date(d) if all.last().is_some_and(|l| !matches!(l, Level::Date(x) if x == d)) => d.label().to_string(),
        Level::Group => format!("{name} Group"),
        _ => name,
    }
}
