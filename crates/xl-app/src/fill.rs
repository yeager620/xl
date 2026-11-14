//! The fill handle: dragging a selection's corner to extend it with a series
//! (numbers, dates, months and weekdays, "Item 1"…) or copies of the pattern.

use xl_engine::datetime::{datetime_from_serial, serial_from_parts};
use xl_engine::{Addr, CellRef, CellSnapshot, Range, Value, precise};

use crate::app::Spreadsheet;

const MONTHS_SHORT: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const MONTHS_LONG: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const DAYS_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const DAYS_LONG: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const NAME_LISTS: [&[&str]; 4] = [&MONTHS_SHORT, &MONTHS_LONG, &DAYS_SHORT, &DAYS_LONG];

/// How one row or column of source cells continues.
#[derive(Debug, PartialEq)]
pub(crate) enum Series {
    /// Repeat the source cells (formulas adjust their references).
    Copy,
    /// a + b·t
    Linear { a: f64, b: f64 },
    /// Dates stepping by whole months from the first one.
    Months { year: i64, month: i64, day: i64, step: i64 },
    /// Text with a number at the end: "Item 1", "Q3".
    Numbered { prefix: String, start: i64, step: i64, width: usize },
    /// Month or weekday names.
    Names { list: &'static [&'static str], start: i64, step: i64, case: Case },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Case {
    AsListed,
    Upper,
    Lower,
}

fn split_number(s: &str) -> Option<(&str, &str)> {
    let digits = s.bytes().rev().take_while(u8::is_ascii_digit).count();
    (digits > 0 && digits <= 15).then(|| s.split_at(s.len() - digits))
}

/// The common step between consecutive items, if there's exactly one.
fn constant_step(xs: &[i64]) -> Option<i64> {
    let step = xs.get(1)? - xs.first()?;
    xs.windows(2).all(|w| w[1] - w[0] == step).then_some(step)
}

impl Series {
    /// Work out the series from source values. `dates` = formatted as dates.
    pub(crate) fn detect(values: &[Value], dates: bool) -> Series {
        let n = values.len();
        if let Some(nums) = values.iter().map(|v| if let Value::Number(x) = v { Some(*x) } else { None }).collect::<Option<Vec<f64>>>() {
            if n == 1 {
                return if dates { Series::Linear { a: nums[0], b: 1.0 } } else { Series::Copy };
            }
            if dates && let Some(m) = Self::months(&nums) {
                return m;
            }
            // Least-squares line through (t, value), like Excel's AutoFill trend.
            let mean_t = (n as f64 - 1.0) / 2.0;
            let mean_v = nums.iter().sum::<f64>() / n as f64;
            let (mut sxy, mut sxx) = (0.0, 0.0);
            for (t, v) in nums.iter().enumerate() {
                sxy += (t as f64 - mean_t) * (v - mean_v);
                sxx += (t as f64 - mean_t).powi(2);
            }
            let b = sxy / sxx;
            return Series::Linear { a: mean_v - b * mean_t, b };
        }
        let Some(texts) = values.iter().map(|v| if let Value::Text(t) = v { Some(t.as_ref()) } else { None }).collect::<Option<Vec<&str>>>() else {
            return Series::Copy;
        };
        for list in NAME_LISTS {
            let idx: Option<Vec<i64>> = texts.iter().map(|t| list.iter().position(|item| item.eq_ignore_ascii_case(t)).map(|i| i as i64)).collect();
            if let Some(idx) = idx {
                let step = if n == 1 { 1 } else { match constant_step(&idx) { Some(s) => s, None => return Series::Copy } };
                let first = texts[0];
                let case = if first.len() > 1 && first == first.to_uppercase() {
                    Case::Upper
                } else if first == first.to_lowercase() {
                    Case::Lower
                } else {
                    Case::AsListed
                };
                return Series::Names { list, start: idx[0], step, case };
            }
        }
        let parts: Option<Vec<(&str, &str)>> = texts.iter().map(|t| split_number(t)).collect();
        if let Some(parts) = parts
            && parts.iter().all(|(p, _)| *p == parts[0].0)
        {
            let nums: Vec<i64> = parts.iter().map(|(_, d)| d.parse().unwrap_or(0)).collect();
            let step = if n == 1 { 1 } else { match constant_step(&nums) { Some(s) => s, None => return Series::Copy } };
            let width = if parts[0].1.starts_with('0') { parts[0].1.len() } else { 0 };
            return Series::Numbered { prefix: parts[0].0.to_string(), start: nums[0], step, width };
        }
        Series::Copy
    }

    /// Dates on the same day of the month, a constant number of months apart.
    fn months(serials: &[f64]) -> Option<Series> {
        let dates: Vec<_> = serials.iter().map(|&s| datetime_from_serial(s)).collect::<Option<_>>()?;
        let first = dates.first()?;
        if dates.iter().any(|d| d.day != first.day) || serials.iter().any(|s| s.fract() != 0.0) {
            return None;
        }
        let index: Vec<i64> = dates.iter().map(|d| d.year * 12 + d.month as i64 - 1).collect();
        let step = constant_step(&index).filter(|&s| s != 0)?;
        Some(Series::Months { year: first.year, month: first.month as i64, day: first.day as i64, step })
    }

    /// The input for position `t` (0 = first source cell; negative = before it).
    pub(crate) fn input_at(&self, t: i64) -> Option<String> {
        Some(match self {
            Series::Copy => return None,
            Series::Linear { a, b } => precise(a + b * t as f64),
            Series::Months { year, month, day, step } => {
                // Clamp to the month's last day (Jan 31 + 1 month = Feb 28/29), like EDATE.
                let m = month - 1 + step * t;
                let (y, m) = (year + m.div_euclid(12), m.rem_euclid(12) + 1);
                let last = xl_engine::datetime::days_in_month(y, m as u32) as i64;
                precise(serial_from_parts(y, m, (*day).min(last)))
            }
            Series::Numbered { prefix, start, step, width } => {
                let v = start + step * t;
                if v < 0 {
                    // Excel stops counting at zero and keeps the sign out of the text.
                    format!("{prefix}{}", v.abs())
                } else {
                    format!("{prefix}{v:0width$}")
                }
            }
            Series::Names { list, start, step, case } => {
                let item = list[(start + step * t).rem_euclid(list.len() as i64) as usize];
                match case {
                    Case::AsListed => item.to_string(),
                    Case::Upper => item.to_uppercase(),
                    Case::Lower => item.to_lowercase(),
                }
            }
        })
    }
}

impl Spreadsheet {
    /// Extend `src` to cover `target` (which contains it and grows in one
    /// direction), continuing each row or column's series.
    pub(crate) fn fill_series(&mut self, src: Range, target: Range) {
        if target == src || !self.guard_bulk(target) {
            return;
        }
        let vertical = target.rows() != src.rows();
        let mut items = Vec::new();
        let lines = if vertical { src.start.col..=src.end.col } else { src.start.row..=src.end.row };
        for line in lines {
            let cells: Vec<CellRef> = if vertical {
                (src.start.row..=src.end.row).map(|r| CellRef::new(r, line)).collect()
            } else {
                (src.start.col..=src.end.col).map(|c| CellRef::new(line, c)).collect()
            };
            let any_formula = cells.iter().any(|&c| self.wb.cell(self.at(c)).is_some_and(|x| x.is_formula()));
            let values: Vec<Value> = cells.iter().map(|&c| self.wb.value(self.at(c)).clone()).collect();
            let dates = self.wb.format(self.at(cells[0])).is_some_and(|f| f.is_date());
            let series = if any_formula || values.iter().any(Value::is_empty) { Series::Copy } else { Series::detect(&values, dates) };
            let (lo, hi, first) = if vertical { (target.start.row, target.end.row, src.start.row) } else { (target.start.col, target.end.col, src.start.col) };
            let n = cells.len() as i64;
            for p in lo..=hi {
                let t = p as i64 - first as i64;
                if (0..n).contains(&t) {
                    continue;
                }
                let from = cells[t.rem_euclid(n) as usize];
                let dst = if vertical { CellRef::new(p, line) } else { CellRef::new(line, p) };
                let (dr, dc) = (dst.row as i64 - from.row as i64, dst.col as i64 - from.col as i64);
                let snap = match series.input_at(t) {
                    Some(input) => CellSnapshot { input, ..self.wb.snapshot(Addr::new(self.cur, from)) },
                    None => self.wb.snapshot_shifted(self.at(from), dr, dc),
                };
                items.push((self.at(dst), snap));
            }
        }
        self.apply_snapshots(items);
        self.anchor = target.start;
        self.active = target.end;
    }

    /// Double-clicking the fill handle: fill down as far as the data in the
    /// column next to the selection goes.
    pub(crate) fn fill_down_to_data(&mut self) {
        let src = self.selection();
        let store = self.wb.store(self.cur);
        let filled = |r: u32, c: u32| !store.value(CellRef::new(r, c)).is_empty();
        let neighbour_end = |c: u32| {
            let mut r = src.end.row;
            while r + 1 < xl_engine::MAX_ROWS && filled(r + 1, c) {
                r += 1;
            }
            r
        };
        let left = if src.start.col > 0 { neighbour_end(src.start.col - 1) } else { src.end.row };
        let right = if src.end.col + 1 < xl_engine::MAX_COLS { neighbour_end(src.end.col + 1) } else { src.end.row };
        let end = left.max(right);
        if end > src.end.row {
            self.fill_series(src, Range::new(src.start, CellRef::new(end, src.end.col)));
        }
    }

    /// While dragging the fill handle: the range the fill would cover.
    pub(crate) fn fill_target_for(&self, src: Range, cell: CellRef) -> Range {
        let below = cell.row as i64 - src.end.row as i64;
        let above = src.start.row as i64 - cell.row as i64;
        let right = cell.col as i64 - src.end.col as i64;
        let left = src.start.col as i64 - cell.col as i64;
        let vertical = below.max(above) >= right.max(left);
        match (vertical, below > 0, above > 0, right > 0, left > 0) {
            (true, true, _, _, _) => Range::new(src.start, CellRef::new(cell.row, src.end.col)),
            (true, _, true, _, _) => Range::new(CellRef::new(cell.row, src.start.col), src.end),
            (false, _, _, true, _) => Range::new(src.start, CellRef::new(src.end.row, cell.col)),
            (false, _, _, _, true) => Range::new(CellRef::new(src.start.row, cell.col), src.end),
            _ => src,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(s: &Series, ts: impl IntoIterator<Item = i64>) -> Vec<String> {
        ts.into_iter().map(|t| s.input_at(t).unwrap_or_default()).collect()
    }

    #[test]
    fn detects_excel_series() {
        let n = Value::Number;
        let t = |s: &str| Value::text(s);
        assert_eq!(Series::detect(&[n(5.0)], false), Series::Copy, "a single number copies");
        assert_eq!(inputs(&Series::detect(&[n(1.0), n(3.0)], false), 2..5), ["5", "7", "9"]);
        assert_eq!(inputs(&Series::detect(&[n(1.0), n(3.0)], false), [-1, -2]), ["-1", "-3"], "filling up continues backwards");
        assert_eq!(inputs(&Series::detect(&[t("Item 1")], false), 1..3), ["Item 2", "Item 3"]);
        assert_eq!(inputs(&Series::detect(&[t("Q1"), t("Q3")], false), 2..3), ["Q5"]);
        assert_eq!(inputs(&Series::detect(&[t("v007")], false), 1..2), ["v008"], "zero padding is kept");
        assert_eq!(inputs(&Series::detect(&[t("Mon")], false), 1..8), ["Tue", "Wed", "Thu", "Fri", "Sat", "Sun", "Mon"]);
        assert_eq!(inputs(&Series::detect(&[t("JANUARY"), t("MARCH")], false), 2..4), ["MAY", "JULY"]);
        assert_eq!(Series::detect(&[t("apple")], false), Series::Copy);
        assert_eq!(Series::detect(&[t("a1"), t("b2")], false), Series::Copy, "different prefixes");
        // Dates: a single date steps by days; same day-of-month steps by months.
        let jan31 = xl_engine::datetime::serial_from_date(2024, 1, 31);
        assert_eq!(inputs(&Series::detect(&[n(jan31)], true), 1..2), [precise(jan31 + 1.0)]);
        let feb29 = xl_engine::datetime::serial_from_date(2024, 2, 29);
        let mar31 = xl_engine::datetime::serial_from_date(2024, 3, 31);
        let m = Series::detect(&[n(jan31), n(mar31)], true);
        assert_eq!(m, Series::Months { year: 2024, month: 1, day: 31, step: 2 });
        assert_eq!(inputs(&Series::detect(&[n(xl_engine::datetime::serial_from_date(2024, 1, 29)), n(feb29)], true), 2..3), [precise(xl_engine::datetime::serial_from_date(2024, 3, 29))]);
    }
}
