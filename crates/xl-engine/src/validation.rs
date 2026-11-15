//! Data validation: rules restricting what may be entered in cells — a
//! list of choices (shown as a dropdown), numbers, dates or text lengths in
//! a range, or a custom formula — with optional prompt and error messages.

use std::sync::Arc;

use crate::address::{Addr, CellRef, Range, SheetId};
use crate::formula::{Expr, Shift};
use crate::value::Value;
use crate::workbook::Workbook;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidationOp {
    Between,
    NotBetween,
    Equal,
    NotEqual,
    Greater,
    Less,
    GreaterEq,
    LessEq,
}

impl ValidationOp {
    pub const ALL: [ValidationOp; 8] =
        [ValidationOp::Between, ValidationOp::NotBetween, ValidationOp::Equal, ValidationOp::NotEqual, ValidationOp::Greater, ValidationOp::Less, ValidationOp::GreaterEq, ValidationOp::LessEq];

    pub fn label(self) -> &'static str {
        match self {
            ValidationOp::Between => "between",
            ValidationOp::NotBetween => "not between",
            ValidationOp::Equal => "equal to",
            ValidationOp::NotEqual => "not equal to",
            ValidationOp::Greater => "greater than",
            ValidationOp::Less => "less than",
            ValidationOp::GreaterEq => "at least",
            ValidationOp::LessEq => "at most",
        }
    }

    pub fn two_values(self) -> bool {
        matches!(self, ValidationOp::Between | ValidationOp::NotBetween)
    }

    fn holds(self, x: f64, a: f64, b: f64) -> bool {
        let (lo, hi) = (a.min(b), a.max(b));
        match self {
            ValidationOp::Between => x >= lo && x <= hi,
            ValidationOp::NotBetween => x < lo || x > hi,
            ValidationOp::Equal => x == a,
            ValidationOp::NotEqual => x != a,
            ValidationOp::Greater => x > a,
            ValidationOp::Less => x < a,
            ValidationOp::GreaterEq => x >= a,
            ValidationOp::LessEq => x <= a,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidationKind {
    Whole,
    Decimal,
    Date,
    Time,
    TextLength,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ValidationRule {
    /// Choices typed in ("Red,Green,Blue") or from cells.
    List(ListSource),
    Compare { kind: ValidationKind, op: ValidationOp, a: Arc<Expr>, b: Option<Arc<Expr>> },
    /// A formula for the range's top-left cell; true allows the value.
    Custom(Arc<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ListSource {
    Items(Vec<String>),
    Cells(Arc<Expr>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ErrorStyle {
    /// Refuse the value.
    #[default]
    Stop,
    /// Ask, but allow it.
    Warning,
    /// Tell, and allow it.
    Information,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Validation {
    pub ranges: Vec<Range>,
    pub rule: ValidationRule,
    pub allow_blank: bool,
    /// Lists: show the in-cell dropdown.
    pub dropdown: bool,
    pub prompt: Option<(String, String)>,
    pub error: Option<(String, String)>,
    pub error_style: ErrorStyle,
}

impl Validation {
    pub fn new(ranges: Vec<Range>, rule: ValidationRule) -> Self {
        Validation { ranges, rule, allow_blank: true, dropdown: true, prompt: None, error: None, error_style: ErrorStyle::Stop }
    }

    pub fn contains(&self, c: CellRef) -> bool {
        self.ranges.iter().any(|r| r.contains(c))
    }

    /// After inserting/deleting lines on `sheet` (the rule's own sheet is
    /// `home`). None if its cells are all gone.
    pub fn shifted(&self, op: Shift, home: SheetId) -> Option<Validation> {
        let mut v = self.clone();
        if op.sheet() == home {
            v.ranges = v.ranges.iter().filter_map(|r| op.map_range(*r)).collect();
            if v.ranges.is_empty() {
                return None;
            }
        }
        let shift = |e: &Arc<Expr>| Arc::new(e.shifted(op, home));
        v.rule = match &v.rule {
            ValidationRule::List(ListSource::Cells(e)) => ValidationRule::List(ListSource::Cells(shift(e))),
            ValidationRule::Compare { kind, op: o, a, b } => ValidationRule::Compare { kind: *kind, op: *o, a: shift(a), b: b.as_ref().map(shift) },
            ValidationRule::Custom(e) => ValidationRule::Custom(shift(e)),
            other => other.clone(),
        };
        Some(v)
    }
}

impl Workbook {
    /// The validation rule covering a cell, if any.
    pub fn validation_at(&self, sheet: SheetId, cell: CellRef) -> Option<&Validation> {
        self.layout(sheet).validations.iter().find(|v| v.contains(cell))
    }

    /// The choices of a list rule, as shown in the dropdown.
    pub fn list_choices(&self, sheet: SheetId, v: &Validation) -> Vec<String> {
        match &v.rule {
            ValidationRule::List(ListSource::Items(items)) => items.clone(),
            ValidationRule::List(ListSource::Cells(e)) => {
                // A range, or a defined name for one (=Colors).
                let found = match (e.reference(), &**e) {
                    (Some((s, r)), _) => Some((s.unwrap_or(sheet), r)),
                    (None, Expr::Name(n)) => self.resolve_reference(n, sheet),
                    _ => None,
                };
                let Some((s, r)) = found else { return Vec::new() };
                r.iter().take(10_000).map(|c| self.display(Addr::new(s, c)).text).filter(|t| !t.is_empty()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Would `value` be allowed in `cell` by its rule (`Ok` when there's no
    /// rule)? `Err` carries the rule so callers can show its message.
    pub fn check_value(&self, sheet: SheetId, cell: CellRef, value: &Value) -> Result<(), &Validation> {
        let Some(v) = self.validation_at(sheet, cell) else { return Ok(()) };
        if value.is_empty() {
            return if v.allow_blank { Ok(()) } else { Err(v) };
        }
        let origin = v.ranges[0].start;
        let at = |e: &Expr| self.eval_at(sheet, cell, &e.offset(cell.row as i64 - origin.row as i64, cell.col as i64 - origin.col as i64));
        let num = |e: &Expr| match at(e) {
            Value::Number(n) => Some(n),
            _ => None,
        };
        let ok = match &v.rule {
            ValidationRule::List(_) => {
                let shown = match value {
                    Value::Number(n) => crate::workbook::precise(*n),
                    other => other.display(),
                };
                self.list_choices(sheet, v).iter().any(|c| c.eq_ignore_ascii_case(&shown) || c.eq_ignore_ascii_case(&value.display()))
            }
            ValidationRule::Compare { kind, op, a, b } => {
                let x = match (kind, value) {
                    (ValidationKind::TextLength, v) => Some(v.display().chars().count() as f64),
                    (_, Value::Number(n)) => Some(*n),
                    _ => None,
                };
                let whole_ok = !matches!(kind, ValidationKind::Whole) || x.is_some_and(|x| x.fract() == 0.0);
                match (x, num(a), b.as_ref().map(|b| num(b)).unwrap_or(Some(0.0))) {
                    (Some(x), Some(a), Some(b)) => whole_ok && op.holds(x, a, b),
                    _ => false,
                }
            }
            ValidationRule::Custom(e) => matches!(at(e), Value::Bool(true)) || matches!(at(e), Value::Number(n) if n != 0.0),
        };
        if ok { Ok(()) } else { Err(v) }
    }
}
