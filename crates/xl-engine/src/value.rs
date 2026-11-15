//! Cell values and coercion rules.

use std::fmt;
use std::sync::Arc;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    Div0,
    Value,
    Ref,
    Name,
    Num,
    NA,
    Circ,
    /// A dynamic array can't spill: cells in the way aren't empty.
    Spill,
    /// A calculation Excel can't do yet: an empty array, a lambda used as a value.
    Calc,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Div0 => "#DIV/0!",
            ErrorKind::Value => "#VALUE!",
            ErrorKind::Ref => "#REF!",
            ErrorKind::Name => "#NAME?",
            ErrorKind::Num => "#NUM!",
            ErrorKind::NA => "#N/A",
            ErrorKind::Circ => "#CIRC!",
            ErrorKind::Spill => "#SPILL!",
            ErrorKind::Calc => "#CALC!",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_uppercase().as_str() {
            "#DIV/0!" => ErrorKind::Div0,
            "#VALUE!" => ErrorKind::Value,
            "#REF!" => ErrorKind::Ref,
            "#NAME?" => ErrorKind::Name,
            "#NUM!" => ErrorKind::Num,
            "#N/A" => ErrorKind::NA,
            "#SPILL!" => ErrorKind::Spill,
            "#CALC!" => ErrorKind::Calc,
            _ => return None,
        })
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub enum Value {
    #[default]
    Empty,
    Number(f64),
    Text(Arc<str>),
    Bool(bool),
    Error(ErrorKind),
}

pub type EvalResult<T> = Result<T, ErrorKind>;

/// A rectangular array of values (row-major), as produced by dynamic-array
/// functions and array arithmetic. Never empty.
#[derive(Clone, Debug, PartialEq)]
pub struct Array {
    pub rows: usize,
    pub cols: usize,
    pub data: Vec<Value>,
}

impl Array {
    pub fn new(rows: usize, cols: usize, data: Vec<Value>) -> Self {
        debug_assert_eq!(rows * cols, data.len());
        Self { rows, cols, data }
    }

    pub fn scalar(v: Value) -> Self {
        Self { rows: 1, cols: 1, data: vec![v] }
    }

    /// Build from rows of equal length.
    pub fn from_rows(rows: Vec<Vec<Value>>) -> Self {
        let cols = rows.first().map_or(0, Vec::len);
        let n = rows.len();
        Self { rows: n, cols, data: rows.into_iter().flatten().collect() }
    }

    pub fn get(&self, r: usize, c: usize) -> &Value {
        &self.data[r * self.cols + c]
    }

    pub fn row(&self, r: usize) -> &[Value] {
        &self.data[r * self.cols..(r + 1) * self.cols]
    }

    pub fn is_scalar(&self) -> bool {
        self.rows == 1 && self.cols == 1
    }
}

impl Value {
    pub fn text(s: impl Into<Arc<str>>) -> Self {
        Value::Text(s.into())
    }

    /// Wrap a float, mapping NaN/inf to #NUM!.
    pub fn num(n: f64) -> Self {
        if n.is_finite() { Value::Number(n) } else { Value::Error(ErrorKind::Num) }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Value::Empty)
    }

    pub fn is_number(&self) -> bool {
        matches!(self, Value::Number(_))
    }

    /// Scalar-to-number coercion (Excel rules for direct arguments/operators).
    pub fn to_number(&self) -> EvalResult<f64> {
        match self {
            Value::Empty => Ok(0.0),
            Value::Number(n) => Ok(*n),
            Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
            Value::Text(s) => parse_number(s).ok_or(ErrorKind::Value),
            Value::Error(e) => Err(*e),
        }
    }

    pub fn to_text(&self) -> EvalResult<Arc<str>> {
        match self {
            Value::Text(s) => Ok(s.clone()),
            Value::Error(e) => Err(*e),
            other => Ok(other.display().into()),
        }
    }

    pub fn to_bool(&self) -> EvalResult<bool> {
        match self {
            Value::Empty => Ok(false),
            Value::Bool(b) => Ok(*b),
            Value::Number(n) => Ok(*n != 0.0),
            Value::Text(s) => match s.to_ascii_uppercase().as_str() {
                "TRUE" => Ok(true),
                "FALSE" => Ok(false),
                _ => Err(ErrorKind::Value),
            },
            Value::Error(e) => Err(*e),
        }
    }

    /// How the value is shown in a cell with the "General" number format.
    pub fn display(&self) -> String {
        match self {
            Value::Empty => String::new(),
            Value::Number(n) => format_general(*n),
            Value::Text(s) => s.to_string(),
            Value::Bool(true) => "TRUE".into(),
            Value::Bool(false) => "FALSE".into(),
            Value::Error(e) => e.as_str().into(),
        }
    }
}

/// Parse user/text input as a number: `12`, `-3.5`, `1e3`, `1,234`, `50%`.
pub fn parse_number(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let (t, pct) = match t.strip_suffix('%') {
        Some(rest) => (rest.trim_end(), true),
        None => (t, false),
    };
    let first = t.as_bytes().first()?;
    if !(first.is_ascii_digit() || matches!(first, b'-' | b'+' | b'.')) {
        return None;
    }
    let cleaned: String;
    let t = if t.contains(',') {
        cleaned = t.replace(',', "");
        cleaned.as_str()
    } else {
        t
    };
    let n: f64 = t.parse().ok()?;
    if !n.is_finite() {
        return None;
    }
    Some(if pct { n / 100.0 } else { n })
}

/// Excel-like "General" formatting: up to ~11 significant characters.
pub fn format_general(n: f64) -> String {
    if n == 0.0 {
        return "0".into();
    }
    let a = n.abs();
    if n.fract() == 0.0 && a < 1e15 {
        return format!("{}", n as i64);
    }
    if !(1e-9..1e11).contains(&a) {
        let s = format!("{:.5E}", n);
        // Trim mantissa zeros: 1.50000E3 -> 1.5E+03
        let (m, e) = s.split_once('E').unwrap();
        let m = m.trim_end_matches('0').trim_end_matches('.');
        let e: i32 = e.parse().unwrap();
        return format!("{m}E{}{:02}", if e < 0 { '-' } else { '+' }, e.abs());
    }
    let int_digits = if a >= 1.0 { a.log10().floor() as i32 + 1 } else { 1 };
    let decimals = (10 - int_digits).max(0) as usize;
    let s = format!("{:.*}", decimals, n);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn general_format() {
        assert_eq!(format_general(1.0), "1");
        assert_eq!(format_general(-42.0), "-42");
        assert_eq!(format_general(0.1 + 0.2), "0.3");
        assert_eq!(format_general(1.0 / 3.0), "0.333333333");
        assert_eq!(format_general(1234.5678), "1234.5678");
        assert_eq!(format_general(1.5e20), "1.5E+20");
        assert_eq!(format_general(2.5e-12), "2.5E-12");
    }

    #[test]
    fn number_parsing() {
        assert_eq!(parse_number("12"), Some(12.0));
        assert_eq!(parse_number(" -3.5 "), Some(-3.5));
        assert_eq!(parse_number("1,234"), Some(1234.0));
        assert_eq!(parse_number("50%"), Some(0.5));
        assert_eq!(parse_number("abc"), None);
        assert_eq!(parse_number("inf"), None);
    }
}
