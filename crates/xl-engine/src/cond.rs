//! Conditional formatting: rules over ranges that restyle cells by their
//! values (highlight rules, top/bottom, averages, duplicates, formulas),
//! color scales and data bars. Evaluated when drawing, from current values.

use std::collections::HashMap;
use std::sync::Arc;

use crate::address::{Addr, CellRef, Range, SheetId};
use crate::formula::{Expr, Shift};
use crate::style::Rgb;
use crate::value::Value;
use crate::workbook::Workbook;

/// The formatting a highlight rule applies (Excel's "differential format").
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CfStyle {
    pub fill: Option<Rgb>,
    pub color: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CfOp {
    Between,
    NotBetween,
    Equal,
    NotEqual,
    Greater,
    Less,
    GreaterEq,
    LessEq,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextMatch {
    Contains,
    NotContains,
    BeginsWith,
    EndsWith,
}

/// A point on a color scale or data bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CfValue {
    Min,
    Max,
    Number(f64),
    Percent(f64),
    Percentile(f64),
}

#[derive(Clone, Debug, PartialEq)]
pub enum CfRule {
    /// Compare the cell with one value (or two, for between).
    Cell { op: CfOp, a: Arc<Expr>, b: Option<Arc<Expr>>, style: CfStyle },
    Text { kind: TextMatch, text: String, style: CfStyle },
    /// Top or bottom N items (or N percent).
    Top { n: u32, percent: bool, bottom: bool, style: CfStyle },
    Average { above: bool, style: CfStyle },
    Duplicates { unique: bool, style: CfStyle },
    Blanks { blanks: bool, style: CfStyle },
    /// A formula written for the range's top-left cell, true to apply.
    Formula { expr: Arc<Expr>, style: CfStyle },
    /// Two or three stops from low to high.
    ColorScale { stops: Vec<(CfValue, Rgb)> },
    DataBar { color: Rgb, min: CfValue, max: CfValue },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CondFormat {
    pub ranges: Vec<Range>,
    pub rule: CfRule,
    /// Lower-priority rules don't apply where this one does.
    pub stop_if_true: bool,
}

impl CondFormat {
    pub fn new(ranges: Vec<Range>, rule: CfRule) -> Self {
        Self { ranges, rule, stop_if_true: false }
    }

    pub fn contains(&self, c: CellRef) -> bool {
        self.ranges.iter().any(|r| r.contains(c))
    }

    /// Follow inserted/deleted rows or columns on `sheet`. False if the
    /// rule's ranges are all gone.
    pub(crate) fn apply_shift(&mut self, op: Shift, sheet: SheetId) -> bool {
        self.ranges = self.ranges.iter().filter_map(|r| op.map_range(*r)).collect();
        let shift = |e: &Arc<Expr>| Arc::new(e.shifted(op, sheet));
        match &mut self.rule {
            CfRule::Cell { a, b, .. } => {
                *a = shift(a);
                *b = b.as_ref().map(shift);
            }
            CfRule::Formula { expr, .. } => *expr = shift(expr),
            _ => {}
        }
        !self.ranges.is_empty()
    }
}

/// What conditional formatting does to one cell.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CfResult {
    pub fill: Option<Rgb>,
    pub color: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    /// Data bar: its length as a fraction of the cell, and color.
    pub bar: Option<(f64, Rgb)>,
}

impl CfResult {
    pub fn is_empty(&self) -> bool {
        *self == CfResult::default()
    }
}

/// Per-rule figures that depend on the whole range (computed once per view).
#[derive(Default)]
struct Stats {
    /// Numbers in the range, sorted.
    numbers: Vec<f64>,
    /// How often each value occurs (lowercased text, numbers as text).
    counts: HashMap<String, usize>,
}

fn value_key(v: &Value) -> Option<String> {
    match v {
        Value::Empty => None,
        Value::Text(t) => Some(t.to_lowercase()),
        other => Some(other.display()),
    }
}

fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    let n = sorted.len();
    if n == 0 {
        return None;
    }
    let rank = (p / 100.0).clamp(0.0, 1.0) * (n - 1) as f64;
    let (lo, hi) = (rank.floor() as usize, rank.ceil() as usize);
    Some(sorted[lo] + (sorted[hi] - sorted[lo]) * (rank - lo as f64))
}

impl Stats {
    fn resolve(&self, v: CfValue) -> Option<f64> {
        let (min, max) = (*self.numbers.first()?, *self.numbers.last()?);
        Some(match v {
            CfValue::Min => min,
            CfValue::Max => max,
            CfValue::Number(x) => x,
            CfValue::Percent(p) => min + (max - min) * p / 100.0,
            CfValue::Percentile(p) => percentile(&self.numbers, p)?,
        })
    }
}

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let ch = |shift: u32| {
        let (x, y) = (((a >> shift) & 0xFF) as f64, ((b >> shift) & 0xFF) as f64);
        ((x + (y - x) * t.clamp(0.0, 1.0)).round() as u32) << shift
    };
    ch(16) | ch(8) | ch(0)
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Number(n) => *n != 0.0,
        Value::Bool(b) => *b,
        _ => false,
    }
}

/// Compare like a worksheet formula: numbers numerically, text without case;
/// an empty cell counts as 0 against a number and "" against text.
fn compare(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.partial_cmp(y),
        (Value::Empty, Value::Number(y)) => 0.0f64.partial_cmp(y),
        (Value::Number(x), Value::Empty) => x.partial_cmp(&0.0),
        (Value::Text(x), Value::Text(y)) => Some(x.to_lowercase().cmp(&y.to_lowercase())),
        (Value::Empty, Value::Text(y)) => Some("".cmp(&y.to_lowercase() as &str)),
        (Value::Text(x), Value::Empty) => Some(x.to_lowercase().as_str().cmp("")),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// Conditional formatting for one sheet, ready to answer per cell.
pub struct CondView<'a> {
    wb: &'a Workbook,
    sheet: SheetId,
    rules: &'a [CondFormat],
    stats: Vec<Stats>,
}

impl<'a> CondView<'a> {
    pub fn new(wb: &'a Workbook, sheet: SheetId) -> Self {
        let rules = wb.layout(sheet).cond.as_slice();
        let store = wb.store(sheet);
        let stats = rules
            .iter()
            .map(|cf| {
                let needs = matches!(cf.rule, CfRule::Top { .. } | CfRule::Average { .. } | CfRule::Duplicates { .. } | CfRule::ColorScale { .. } | CfRule::DataBar { .. });
                let mut st = Stats::default();
                if needs {
                    for r in &cf.ranges {
                        for (_, cell) in store.iter_range(*r) {
                            if let Value::Number(n) = cell.value {
                                st.numbers.push(n);
                            }
                            if matches!(cf.rule, CfRule::Duplicates { .. })
                                && let Some(k) = value_key(&cell.value)
                            {
                                *st.counts.entry(k).or_default() += 1;
                            }
                        }
                    }
                    st.numbers.sort_by(f64::total_cmp);
                }
                st
            })
            .collect();
        Self { wb, sheet, rules, stats }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn cell(&self, c: CellRef) -> CfResult {
        let mut out = CfResult::default();
        let (mut fill_set, mut color_set) = (false, false);
        let v = self.wb.value(Addr::new(self.sheet, c));
        for (cf, st) in self.rules.iter().zip(&self.stats) {
            if !cf.contains(c) {
                continue;
            }
            let origin = cf.ranges[0].start;
            let at = |e: &Expr| self.wb.eval_at(self.sheet, c, &e.offset(c.row as i64 - origin.row as i64, c.col as i64 - origin.col as i64));
            let (matched, style) = match &cf.rule {
                CfRule::ColorScale { stops } => {
                    if let (Value::Number(n), false) = (v, fill_set) {
                        let pts: Vec<(f64, Rgb)> = stops.iter().filter_map(|&(p, col)| Some((st.resolve(p)?, col))).collect();
                        if let (Some(first), Some(last)) = (pts.first(), pts.last()) {
                            let color = if *n <= first.0 {
                                first.1
                            } else if *n >= last.0 {
                                last.1
                            } else {
                                let i = pts.windows(2).position(|w| *n <= w[1].0).unwrap_or(0);
                                let (a, b) = (pts[i], pts[i + 1]);
                                mix(a.1, b.1, if b.0 > a.0 { (n - a.0) / (b.0 - a.0) } else { 1.0 })
                            };
                            out.fill = Some(color);
                            fill_set = true;
                        }
                    }
                    continue;
                }
                CfRule::DataBar { color, min, max } => {
                    if let (Value::Number(n), None) = (v, out.bar)
                        && let (Some(lo), Some(hi)) = (st.resolve(*min), st.resolve(*max))
                    {
                        let frac = if hi > lo { (n - lo) / (hi - lo) } else { 1.0 };
                        out.bar = Some((frac.clamp(0.0, 1.0), *color));
                    }
                    continue;
                }
                CfRule::Cell { op, a, b, style } => {
                    let a = at(a);
                    let ord = compare(v, &a);
                    let m = match op {
                        CfOp::Equal => ord == Some(std::cmp::Ordering::Equal),
                        CfOp::NotEqual => ord != Some(std::cmp::Ordering::Equal),
                        CfOp::Greater => ord == Some(std::cmp::Ordering::Greater),
                        CfOp::Less => ord == Some(std::cmp::Ordering::Less),
                        CfOp::GreaterEq => matches!(ord, Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)),
                        CfOp::LessEq => matches!(ord, Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)),
                        CfOp::Between | CfOp::NotBetween => {
                            let b = b.as_ref().map(|b| at(b)).unwrap_or(Value::Empty);
                            let (lo, hi) = match compare(&a, &b) {
                                Some(std::cmp::Ordering::Greater) => (b, a),
                                _ => (a, b),
                            };
                            let inside = matches!(compare(v, &lo), Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal))
                                && matches!(compare(v, &hi), Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal));
                            inside == (*op == CfOp::Between)
                        }
                    };
                    (m, style)
                }
                CfRule::Text { kind, text, style } => {
                    let hay = match v {
                        Value::Empty => String::new(),
                        Value::Text(t) => t.to_lowercase(),
                        other => other.display().to_lowercase(),
                    };
                    let needle = text.to_lowercase();
                    let m = match kind {
                        TextMatch::Contains => hay.contains(&needle),
                        TextMatch::NotContains => !hay.contains(&needle),
                        TextMatch::BeginsWith => hay.starts_with(&needle),
                        TextMatch::EndsWith => hay.ends_with(&needle),
                    };
                    (m, style)
                }
                CfRule::Top { n, percent, bottom, style } => {
                    let m = match v {
                        Value::Number(x) if !st.numbers.is_empty() => {
                            let len = st.numbers.len();
                            let k = if *percent { ((*n as f64 / 100.0) * len as f64).floor().max(1.0) as usize } else { *n as usize }.clamp(1, len);
                            if *bottom { *x <= st.numbers[k - 1] } else { *x >= st.numbers[len - k] }
                        }
                        _ => false,
                    };
                    (m, style)
                }
                CfRule::Average { above, style } => {
                    let m = match v {
                        Value::Number(x) if !st.numbers.is_empty() => {
                            let mean = st.numbers.iter().sum::<f64>() / st.numbers.len() as f64;
                            if *above { *x > mean } else { *x < mean }
                        }
                        _ => false,
                    };
                    (m, style)
                }
                CfRule::Duplicates { unique, style } => {
                    let count = value_key(v).and_then(|k| st.counts.get(&k).copied()).unwrap_or(0);
                    (if *unique { count == 1 } else { count > 1 }, style)
                }
                CfRule::Blanks { blanks, style } => {
                    let blank = matches!(v, Value::Empty) || matches!(v, Value::Text(t) if t.trim().is_empty());
                    (blank == *blanks, style)
                }
                CfRule::Formula { expr, style } => (truthy(&at(expr)), style),
            };
            if !matched {
                continue;
            }
            if let (Some(f), false) = (style.fill, fill_set) {
                out.fill = Some(f);
                fill_set = true;
            }
            if let (Some(col), false) = (style.color, color_set) {
                out.color = Some(col);
                color_set = true;
            }
            out.bold |= style.bold;
            out.italic |= style.italic;
            if cf.stop_if_true {
                break;
            }
        }
        out
    }
}
