//! Pivot tables: summarize a source range (header row + records) by the
//! distinct values of row and column fields, aggregating value fields. The
//! result is a grid of values written into the sheet.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::address::{Addr, CellRef, Range, SheetId};
use crate::formula::{Expr, Shift};
use crate::value::Value;
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

#[derive(Clone, Debug, PartialEq)]
pub struct Pivot {
    /// The source data, header row first (usually on another sheet).
    pub source: Arc<Expr>,
    /// Top-left cell of the output on the pivot's own sheet.
    pub anchor: CellRef,
    /// Fields by index into the source's columns.
    pub rows: Vec<usize>,
    pub cols: Vec<usize>,
    pub values: Vec<(usize, Agg)>,
    /// Where the last output was written (to clear it on refresh).
    pub extent: Option<Range>,
}

impl Pivot {
    pub(crate) fn apply_shift(&mut self, op: Shift, home: SheetId) {
        self.source = Arc::new(self.source.shifted(op, home));
    }
}

/// Running aggregate for one group.
#[derive(Clone, Copy, Default)]
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

/// A group key part: sorts numbers before text (case-insensitively), blanks last.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Num(i64),
    Text(String, String),
    Blank,
}

fn key_of(v: &Value, shown: &str) -> Key {
    match v {
        Value::Empty => Key::Blank,
        // Order numbers by value; f64 bits make them totally ordered.
        Value::Number(n) => {
            let bits = n.to_bits() as i64;
            Key::Num(if bits < 0 { i64::MIN - bits } else { bits })
        }
        _ => Key::Text(shown.to_lowercase(), shown.to_string()),
    }
}

fn key_label(k: &Key, labels: &BTreeMap<Key, String>) -> String {
    match k {
        Key::Blank => "(blank)".into(),
        _ => labels.get(k).cloned().unwrap_or_default(),
    }
}

impl Workbook {
    /// The source's column headers.
    pub fn pivot_fields(&self, home: SheetId, p: &Pivot) -> Vec<String> {
        let Some((sheet, r)) = p.source.reference() else { return Vec::new() };
        let sheet = sheet.unwrap_or(home);
        (r.start.col..=r.end.col).map(|c| self.display(Addr::new(sheet, CellRef::new(r.start.row, c))).text).map(|h| if h.is_empty() { "(blank)".into() } else { h }).collect()
    }

    /// Compute a pivot's output grid (rows of cells), header first.
    pub fn pivot_grid(&self, home: SheetId, p: &Pivot) -> Vec<Vec<Value>> {
        let Some((sheet, r)) = p.source.reference() else { return vec![vec![Value::Error(crate::value::ErrorKind::Ref)]] };
        let sheet = sheet.unwrap_or(home);
        let fields = self.pivot_fields(home, p);
        // Records stop at the used area (whole-column sources).
        let last = self.store(sheet).extent().map_or(r.end.row, |e| r.end.row.min(e.row));
        let cell = |row: u32, f: usize| Addr::new(sheet, CellRef::new(row, r.start.col + f as u32));
        let mut labels: BTreeMap<Key, String> = BTreeMap::new();
        let key_for = |row: u32, fs: &[usize], labels: &mut BTreeMap<Key, String>| -> Vec<Key> {
            fs.iter()
                .map(|&f| {
                    let a = cell(row, f);
                    let shown = self.display(a).text;
                    let k = key_of(self.value(a), &shown);
                    labels.entry(k.clone()).or_insert(shown);
                    k
                })
                .collect()
        };
        let nv = p.values.len().max(1);
        let mut groups: BTreeMap<(Vec<Key>, Vec<Key>), Vec<Acc>> = BTreeMap::new();
        let (mut row_keys, mut col_keys): (BTreeSet<Vec<Key>>, BTreeSet<Vec<Key>>) = (BTreeSet::new(), BTreeSet::new());
        for row in r.start.row + 1..=last {
            if (r.start.col..=r.end.col).all(|c| self.value(Addr::new(sheet, CellRef::new(row, c))).is_empty()) {
                continue;
            }
            let rk = key_for(row, &p.rows, &mut labels);
            let ck = key_for(row, &p.cols, &mut labels);
            let accs = groups.entry((rk.clone(), ck.clone())).or_insert_with(|| vec![Acc::default(); nv]);
            for (i, (f, _)) in p.values.iter().enumerate() {
                accs[i].add(self.value(cell(row, *f)));
            }
            row_keys.insert(rk);
            col_keys.insert(ck);
        }
        let name = |f: usize| fields.get(f).cloned().unwrap_or_default();
        let value_title = |(f, agg): &(usize, Agg)| format!("{} of {}", agg.label(), name(*f));
        let col_keys: Vec<Vec<Key>> = col_keys.into_iter().collect();
        let with_cols = !p.cols.is_empty();
        let mut grid: Vec<Vec<Value>> = Vec::new();
        // Header row: row field names, then a column per (column key × value), then totals.
        let mut header: Vec<Value> = p.rows.iter().map(|&f| Value::text(name(f))).collect();
        if header.is_empty() {
            header.push(Value::Empty);
        }
        for ck in col_keys.iter().filter(|_| with_cols) {
            let label = ck.iter().map(|k| key_label(k, &labels)).collect::<Vec<_>>().join(" / ");
            for v in &p.values {
                header.push(Value::text(if nv > 1 { format!("{label} – {}", value_title(v)) } else { label.clone() }));
            }
        }
        for v in &p.values {
            header.push(Value::text(if with_cols { format!("Total {}", value_title(v)) } else { value_title(v) }));
        }
        grid.push(header);
        let empty_accs = vec![Acc::default(); nv];
        let mut grand_cols: Vec<Vec<Acc>> = vec![vec![Acc::default(); nv]; col_keys.len()];
        let mut grand = vec![Acc::default(); nv];
        let push_values = |line: &mut Vec<Value>, accs: &[Acc]| {
            for (i, (_, agg)) in p.values.iter().enumerate() {
                line.push(accs.get(i).map_or(Value::Empty, |a| a.result(*agg)));
            }
        };
        for rk in &row_keys {
            let mut line: Vec<Value> = rk.iter().map(|k| Value::text(key_label(k, &labels))).collect();
            if line.is_empty() {
                line.push(Value::text("Total"));
            }
            let mut row_total = vec![Acc::default(); nv];
            for (ci, ck) in col_keys.iter().enumerate() {
                let accs = groups.get(&(rk.clone(), ck.clone())).unwrap_or(&empty_accs);
                if with_cols {
                    push_values(&mut line, accs);
                }
                for i in 0..nv {
                    row_total[i].merge(&accs[i]);
                    grand_cols[ci][i].merge(&accs[i]);
                    grand[i].merge(&accs[i]);
                }
            }
            push_values(&mut line, &row_total);
            grid.push(line);
        }
        if !p.rows.is_empty() {
            let mut line: Vec<Value> = vec![Value::text("Grand Total")];
            line.extend(std::iter::repeat_n(Value::Empty, p.rows.len() - 1));
            if with_cols {
                for g in &grand_cols {
                    push_values(&mut line, g);
                }
            }
            push_values(&mut line, &grand);
            grid.push(line);
        }
        grid
    }
}
