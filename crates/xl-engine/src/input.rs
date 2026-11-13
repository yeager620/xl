//! Interpreting typed input: numbers, currency, percentages, dates and times,
//! booleans and errors — each with the number format Excel would auto-apply.

use crate::datetime::{days_in_month, serial_from_date};
use crate::value::{ErrorKind, Value, parse_number};

#[derive(Debug, Clone, PartialEq)]
pub struct Literal {
    pub value: Value,
    /// Number format to apply if the cell has none yet.
    pub format: Option<&'static str>,
}

fn lit(value: Value, format: Option<&'static str>) -> Option<Literal> {
    Some(Literal { value, format })
}

/// Classify non-formula input. Returns None for plain text.
pub fn classify_literal(raw: &str) -> Option<Literal> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    if t.eq_ignore_ascii_case("TRUE") {
        return lit(Value::Bool(true), None);
    }
    if t.eq_ignore_ascii_case("FALSE") {
        return lit(Value::Bool(false), None);
    }
    if let Some(e) = ErrorKind::parse(t) {
        return lit(Value::Error(e), None);
    }
    if let Some(l) = currency(t) {
        return Some(l);
    }
    if let Some(n) = parse_number(t) {
        let decimals = t.contains('.');
        let fmt = if t.ends_with('%') {
            Some(if decimals { "0.00%" } else { "0%" })
        } else if t.contains(',') {
            Some(if decimals { "#,##0.00" } else { "#,##0" })
        } else {
            None
        };
        return lit(Value::Number(n), fmt);
    }
    date_time(t)
}

fn currency(t: &str) -> Option<Literal> {
    let (neg, body) = if let Some(b) = t.strip_prefix('-') {
        (true, b.trim_start())
    } else if let Some(b) = t.strip_prefix('(').and_then(|b| b.strip_suffix(')')) {
        (true, b)
    } else {
        (false, t)
    };
    let body = body.strip_prefix('$')?.trim_start();
    let (body, neg) = match body.strip_prefix('-') {
        Some(b) => (b, !neg),
        None => (body, neg),
    };
    if !body.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let n = parse_number(body)?;
    lit(Value::Number(if neg { -n } else { n }), Some(if body.contains('.') { "$#,##0.00" } else { "$#,##0" }))
}

fn two_digit_year(y: i64, digits: usize) -> i64 {
    if digits <= 2 {
        if y < 30 { 2000 + y } else { 1900 + y }
    } else {
        y
    }
}

fn month_from_name(s: &str) -> Option<u32> {
    let s = s.to_ascii_lowercase();
    if s.len() < 3 {
        return None;
    }
    crate::datetime::MONTHS.iter().position(|m| m.to_ascii_lowercase().starts_with(&s) || s.starts_with(&m[..3].to_ascii_lowercase())).map(|i| i as u32 + 1)
}

fn valid(y: i64, m: u32, d: u32) -> bool {
    (1900..=9999).contains(&y) && (1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m)
}

/// Parse `h:mm`, `h:mm:ss`, optionally followed by AM/PM. Returns (fraction of day, has seconds, 12h).
fn time(s: &str) -> Option<(f64, bool, bool)> {
    let s = s.trim();
    let upper = s.to_ascii_uppercase();
    let (body, ampm) = if let Some(b) = upper.strip_suffix("AM") {
        (b.trim_end().to_string(), Some(false))
    } else if let Some(b) = upper.strip_suffix("PM") {
        (b.trim_end().to_string(), Some(true))
    } else {
        (upper.clone(), None)
    };
    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let h: u32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let sec: f64 = if parts.len() == 3 { parts[2].parse().ok()? } else { 0.0 };
    if m > 59 || sec >= 60.0 {
        return None;
    }
    let h = match ampm {
        Some(pm) => {
            if !(1..=12).contains(&h) {
                return None;
            }
            (h % 12) + if pm { 12 } else { 0 }
        }
        None if h > 23 => return None,
        None => h,
    };
    Some(((h as f64 * 3600.0 + m as f64 * 60.0 + sec) / 86_400.0, parts.len() == 3, ampm.is_some()))
}

fn date(s: &str) -> Option<(f64, &'static str)> {
    let s = s.trim();
    // ISO: yyyy-mm-dd
    if let [y, m, d] = s.split('-').collect::<Vec<_>>()[..]
        && y.len() == 4
        && let (Ok(y), Ok(m), Ok(d)) = (y.parse::<i64>(), m.parse::<u32>(), d.parse::<u32>())
    {
        return valid(y, m, d).then(|| (serial_from_date(y, m, d), "yyyy-mm-dd"));
    }
    // US: m/d/yyyy or m/d/yy
    if let [m, d, y] = s.split('/').collect::<Vec<_>>()[..]
        && let (Ok(m), Ok(d), Ok(yy)) = (m.parse::<u32>(), d.parse::<u32>(), y.parse::<i64>())
    {
        let y = two_digit_year(yy, y.len());
        return valid(y, m, d).then(|| (serial_from_date(y, m, d), "m/d/yyyy"));
    }
    // d-mmm-yy / d-mmm-yyyy / d mmm yyyy
    let parts: Vec<&str> = s.split(['-', ' ']).filter(|p| !p.is_empty()).collect();
    if let [d, m, y] = parts[..]
        && let (Ok(d), Some(m), Ok(yy)) = (d.parse::<u32>(), month_from_name(m), y.parse::<i64>())
    {
        let y = two_digit_year(yy, y.len());
        return valid(y, m, d).then(|| (serial_from_date(y, m, d), "d-mmm-yy"));
    }
    // mmm d, yyyy
    let parts: Vec<&str> = s.split([' ', ',']).filter(|p| !p.is_empty()).collect();
    if let [m, d, y] = parts[..]
        && let (Some(m), Ok(d), Ok(yy)) = (month_from_name(m), d.parse::<u32>(), y.parse::<i64>())
    {
        let y = two_digit_year(yy, y.len());
        return valid(y, m, d).then(|| (serial_from_date(y, m, d), "mmm d, yyyy"));
    }
    None
}

fn date_time(t: &str) -> Option<Literal> {
    if let Some((serial, fmt)) = date(t) {
        return lit(Value::Number(serial), Some(fmt));
    }
    if let Some((frac, secs, twelve)) = time(t) {
        let fmt = match (secs, twelve) {
            (false, false) => "h:mm",
            (true, false) => "h:mm:ss",
            (false, true) => "h:mm AM/PM",
            (true, true) => "h:mm:ss AM/PM",
        };
        return lit(Value::Number(frac), Some(fmt));
    }
    // Date followed by a time: "2026-11-13 14:30", "11/13/2026 2:30 PM"
    let (d, rest) = t.split_once(' ')?;
    let (serial, dfmt) = date(d)?;
    let (frac, _, twelve) = time(rest)?;
    let fmt = match (dfmt, twelve) {
        ("yyyy-mm-dd", _) => "yyyy-mm-dd h:mm",
        (_, true) => "m/d/yyyy h:mm AM/PM",
        _ => "m/d/yyyy h:mm",
    };
    lit(Value::Number(serial + frac), Some(fmt))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Option<(Value, Option<&'static str>)> {
        classify_literal(s).map(|l| (l.value, l.format))
    }

    #[test]
    fn numbers_currency_percent() {
        assert_eq!(c("42"), Some((Value::Number(42.0), None)));
        assert_eq!(c("1,234.5"), Some((Value::Number(1234.5), Some("#,##0.00"))));
        assert_eq!(c("15%"), Some((Value::Number(0.15), Some("0%"))));
        assert_eq!(c("$1,200"), Some((Value::Number(1200.0), Some("$#,##0"))));
        assert_eq!(c("-$3.50"), Some((Value::Number(-3.5), Some("$#,##0.00"))));
        assert_eq!(c("($7)"), Some((Value::Number(-7.0), Some("$#,##0"))));
        assert_eq!(c("TRUE"), Some((Value::Bool(true), None)));
        assert_eq!(c("hello"), None);
        assert_eq!(c("$abc"), None);
    }

    #[test]
    fn dates_and_times() {
        let d = serial_from_date(2026, 11, 13);
        assert_eq!(c("2026-11-13"), Some((Value::Number(d), Some("yyyy-mm-dd"))));
        assert_eq!(c("11/13/2026"), Some((Value::Number(d), Some("m/d/yyyy"))));
        assert_eq!(c("11/13/26"), Some((Value::Number(d), Some("m/d/yyyy"))));
        assert_eq!(c("13-Nov-2026"), Some((Value::Number(d), Some("d-mmm-yy"))));
        assert_eq!(c("Nov 13, 2026"), Some((Value::Number(d), Some("mmm d, yyyy"))));
        assert_eq!(c("14:30"), Some((Value::Number(14.5 / 24.0), Some("h:mm"))));
        assert_eq!(c("2:30 PM"), Some((Value::Number(14.5 / 24.0), Some("h:mm AM/PM"))));
        assert_eq!(c("2026-11-13 06:00"), Some((Value::Number(d + 0.25), Some("yyyy-mm-dd h:mm"))));
        assert_eq!(c("2/30/2026"), None);
        assert_eq!(c("25:00"), None);
    }
}
