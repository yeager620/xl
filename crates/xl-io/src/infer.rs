//! Type inference for imported text fields (CSV): numbers, dates, times,
//! booleans — and, just as important, what must stay text.
//!
//! Rules, chosen so nothing is silently mangled:
//! - Plain decimals (`-12.5`, `.5`, `1e6`) are numbers. Surrounding spaces
//!   are ignored.
//! - Leading zeros (`007`, `-0012`, `00.5`) mean an identifier: text.
//! - More than 15 integer digits can't be held exactly by a double
//!   (credit-card numbers, long IDs): text.
//! - A leading `+` (phone numbers) is text.
//! - Grouped thousands only with proper groups of three (`1,234,567.8`);
//!   `1,2` (a decimal comma) stays text.
//! - `12%` and `$1,234.50` (also `-$5`, `($5)`) are numbers with a percent /
//!   currency format.
//! - Dates: ISO `2024-03-09` (optionally with ` hh:mm[:ss[.fff]]` or `T`),
//!   `2024/03/09`, `09.03.2024` (day first), and slash dates with a 4-digit
//!   year whose day/month order is decided per column (see [`DateOrder`]).
//!   Times with a zone suffix (`Z`, `+02:00`) stay text: there's no right
//!   local value to pick. Two-digit years stay text.
//! - `h:mm` / `h:mm:ss` are times.
//! - `TRUE` / `FALSE` (any case) are booleans.
//! - Everything else, including error-looking text (`#N/A`) and formulas
//!   (`=A1`), is text.

use xl_engine::datetime::{days_in_month, serial_from_date};

/// Day/month order for ambiguous slash dates (`03/04/2024`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DateOrder {
    /// Month first (US).
    Mdy,
    /// Day first (most of the world).
    Dmy,
}

impl DateOrder {
    /// From the locale environment: US English (or no locale at all) is
    /// month-first, everything else day-first.
    pub fn from_env() -> Self {
        let loc = ["LC_ALL", "LC_TIME", "LANG"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty())).unwrap_or_default();
        if loc.is_empty() || loc == "C" || loc == "POSIX" || loc.starts_with("C.") || loc.starts_with("en_US") { DateOrder::Mdy } else { DateOrder::Dmy }
    }
}

/// What a field turned out to be.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Inferred<'a> {
    Empty,
    Number(f64),
    /// A number shown with a format (dates, percents, currency…).
    Formatted(f64, &'static str),
    Bool(bool),
    Text(&'a str),
}

/// Evidence about slash-date order seen in one column.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct OrderEvidence {
    /// A slash date whose first part can only be a day (> 12).
    pub day_first: bool,
    /// A slash date whose second part can only be a day.
    pub month_first: bool,
}

impl OrderEvidence {
    pub fn observe(&mut self, field: &str) {
        if let Some((a, b, _, _)) = slash_parts(field.trim()) {
            self.day_first |= a > 12 && b <= 12;
            self.month_first |= b > 12 && a <= 12;
        }
    }

    pub fn decide(self, default: DateOrder) -> DateOrder {
        match (self.day_first, self.month_first) {
            (true, false) => DateOrder::Dmy,
            (false, true) => DateOrder::Mdy,
            _ => default,
        }
    }
}

pub fn infer(field: &str, order: DateOrder) -> Inferred<'_> {
    if field.is_empty() {
        return Inferred::Empty;
    }
    let t = field.trim_matches(' ');
    if t.is_empty() {
        return Inferred::Text(field);
    }
    let b = t.as_bytes();
    let first = b[0];
    if first.is_ascii_digit() || first == b'-' || first == b'.' {
        if let Some(n) = plain_number(t) {
            return Inferred::Number(n);
        }
        if let Some(x) = typed_number(t) {
            return x;
        }
        if let Some((serial, fmt)) = date_time(t, order) {
            return Inferred::Formatted(serial, fmt);
        }
        return Inferred::Text(field);
    }
    if first == b'$' || first == b'(' {
        if let Some(x) = typed_number(t) {
            return x;
        }
        return Inferred::Text(field);
    }
    if t.eq_ignore_ascii_case("true") {
        return Inferred::Bool(true);
    }
    if t.eq_ignore_ascii_case("false") {
        return Inferred::Bool(false);
    }
    Inferred::Text(field)
}

/// Integer digits that start with a zero, or too many to hold exactly.
fn bad_integer_part(int: &[u8]) -> bool {
    (int.len() > 1 && int[0] == b'0') || int.len() > 15
}

/// `-?digits[.digits][e[+-]digits]` or `-?.digits…`, under the rules above.
pub fn plain_number(t: &str) -> Option<f64> {
    let b = t.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int = &b[int_start..i];
    let mut frac = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let s = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac = i - s;
    }
    if int.is_empty() && frac == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let s = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == s || i - s > 3 {
            return None;
        }
    }
    if i != b.len() || bad_integer_part(int) {
        return None;
    }
    t.parse::<f64>().ok().filter(|n| n.is_finite())
}

/// `1,234,567[.89]` with proper groups.
fn grouped_number(t: &str) -> Option<f64> {
    let (neg, body) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t),
    };
    let (int, frac) = match body.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (body, None),
    };
    let mut groups = int.split(',');
    let head = groups.next()?;
    if head.is_empty() || head.len() > 3 || head.starts_with('0') || !head.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut digits = head.to_string();
    let mut n_groups = 0;
    for g in groups {
        if g.len() != 3 || !g.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        digits.push_str(g);
        n_groups += 1;
    }
    if n_groups == 0 || digits.len() > 15 {
        return None;
    }
    if let Some(f) = frac {
        if f.is_empty() || !f.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        digits.push('.');
        digits.push_str(f);
    }
    let n: f64 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// Percent, currency, grouped thousands, and times.
fn typed_number(t: &str) -> Option<Inferred<'static>> {
    if let Some(body) = t.strip_suffix('%') {
        let body = body.trim_end_matches(' ');
        let n = plain_number(body).or_else(|| grouped_number(body))?;
        return Some(Inferred::Formatted(n / 100.0, if body.contains('.') { "0.00%" } else { "0%" }));
    }
    let (neg, body) = if let Some(b) = t.strip_prefix('(').and_then(|b| b.strip_suffix(')')) {
        (true, b)
    } else if let Some(b) = t.strip_prefix('-') {
        (true, b)
    } else {
        (false, t)
    };
    if let Some(amount) = body.strip_prefix('$') {
        if amount.starts_with('-') || amount.starts_with('+') {
            return None;
        }
        let n = plain_number(amount).or_else(|| grouped_number(amount))?;
        let fmt = if amount.contains('.') { "$#,##0.00" } else { "$#,##0" };
        return Some(Inferred::Formatted(if neg { -n } else { n }, fmt));
    }
    if let Some(n) = grouped_number(t) {
        return Some(Inferred::Formatted(n, if t.contains('.') { "#,##0.00" } else { "#,##0" }));
    }
    let (frac, secs) = time(t)?;
    Some(Inferred::Formatted(frac, if secs { "h:mm:ss" } else { "h:mm" }))
}

fn num(s: &str, min: usize, max: usize) -> Option<u32> {
    (s.len() >= min && s.len() <= max && s.bytes().all(|c| c.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

/// `h:mm`, `hh:mm:ss[.fff]` -> (fraction of a day, has seconds).
fn time(s: &str) -> Option<(f64, bool)> {
    let mut parts = s.split(':');
    let h = num(parts.next()?, 1, 2)?;
    let m = num(parts.next()?, 2, 2)?;
    let (sec, has_secs) = match parts.next() {
        Some(p) => {
            let (whole, frac) = match p.split_once('.') {
                Some((w, f)) => (w, Some(f)),
                None => (p, None),
            };
            let mut sec = num(whole, 2, 2)? as f64;
            if let Some(f) = frac {
                let d = num(f, 1, 9)?;
                sec += d as f64 / 10f64.powi(f.len() as i32);
            }
            (sec, true)
        }
        None => (0.0, false),
    };
    if parts.next().is_some() || h > 23 || m > 59 || sec >= 60.0 {
        return None;
    }
    Some(((h as f64 * 3600.0 + m as f64 * 60.0 + sec) / 86_400.0, has_secs))
}

fn valid(y: i64, m: u32, d: u32) -> bool {
    (1900..=9999).contains(&y) && (1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m)
}

/// `a/b/yyyy` -> (a, b, year, zero-padded).
fn slash_parts(s: &str) -> Option<(u32, u32, i64, bool)> {
    let mut p = s.split('/');
    let (a, b, y) = (p.next()?, p.next()?, p.next()?);
    if p.next().is_some() {
        return None;
    }
    let padded = a.len() == 2 && b.len() == 2;
    Some((num(a, 1, 2)?, num(b, 1, 2)?, num(y, 4, 4)? as i64, padded))
}

/// A date alone -> (serial, format).
fn date(s: &str, order: DateOrder) -> Option<(f64, &'static str)> {
    let b = s.as_bytes();
    // yyyy-mm-dd / yyyy/mm/dd
    if b.len() == 10 && (b[4] == b'-' || b[4] == b'/') && b[7] == b[4] {
        let y = num(&s[..4], 4, 4)? as i64;
        let (m, d) = (num(&s[5..7], 2, 2)?, num(&s[8..], 2, 2)?);
        return valid(y, m, d).then(|| (serial_from_date(y, m, d), if b[4] == b'-' { "yyyy-mm-dd" } else { "yyyy/mm/dd" }));
    }
    // dd.mm.yyyy
    if b.len() == 10 && b[2] == b'.' && b[5] == b'.' {
        let (d, m, y) = (num(&s[..2], 2, 2)?, num(&s[3..5], 2, 2)?, num(&s[6..], 4, 4)? as i64);
        return valid(y, m, d).then(|| (serial_from_date(y, m, d), "dd.mm.yyyy"));
    }
    let (a, c, y, padded) = slash_parts(s)?;
    let (m, d, fmt) = match (order, padded) {
        (DateOrder::Mdy, false) => (a, c, "m/d/yyyy"),
        (DateOrder::Mdy, true) => (a, c, "mm/dd/yyyy"),
        (DateOrder::Dmy, false) => (c, a, "d/m/yyyy"),
        (DateOrder::Dmy, true) => (c, a, "dd/mm/yyyy"),
    };
    valid(y, m, d).then(|| (serial_from_date(y, m, d), fmt))
}

fn date_time(t: &str, order: DateOrder) -> Option<(f64, &'static str)> {
    if let Some(d) = date(t, order) {
        return Some(d);
    }
    // Date, then a time after a space (or ISO `T`).
    let split = t.find(' ').or_else(|| (t.len() > 10 && t.as_bytes()[10] == b'T' && t.as_bytes()[4] == b'-').then_some(10))?;
    let (d, rest) = (&t[..split], t[split + 1..].trim_start_matches(' '));
    let (serial, dfmt) = date(d, order)?;
    let (frac, secs) = time(rest)?;
    let fmt = match (dfmt, secs) {
        ("yyyy-mm-dd", true) => "yyyy-mm-dd hh:mm:ss",
        ("yyyy-mm-dd", false) => "yyyy-mm-dd hh:mm",
        ("yyyy/mm/dd", true) => "yyyy/mm/dd hh:mm:ss",
        ("yyyy/mm/dd", false) => "yyyy/mm/dd hh:mm",
        ("dd.mm.yyyy", true) => "dd.mm.yyyy hh:mm:ss",
        ("dd.mm.yyyy", false) => "dd.mm.yyyy hh:mm",
        ("m/d/yyyy" | "mm/dd/yyyy", true) => "m/d/yyyy h:mm:ss",
        ("m/d/yyyy" | "mm/dd/yyyy", false) => "m/d/yyyy h:mm",
        (_, true) => "d/m/yyyy h:mm:ss",
        (_, false) => "d/m/yyyy h:mm",
    };
    Some((serial + frac, fmt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Inferred::*;

    fn i(s: &str) -> Inferred<'_> {
        infer(s, DateOrder::Mdy)
    }

    #[test]
    fn numbers() {
        assert_eq!(i("42"), Number(42.0));
        assert_eq!(i("-3.25"), Number(-3.25));
        assert_eq!(i("0"), Number(0.0));
        assert_eq!(i("0.5"), Number(0.5));
        assert_eq!(i("-0.5"), Number(-0.5));
        assert_eq!(i(".5"), Number(0.5));
        assert_eq!(i("1e6"), Number(1e6));
        assert_eq!(i("6.02E+23"), Number(6.02e23));
        assert_eq!(i(" 7 "), Number(7.0));
        assert_eq!(i("1."), Number(1.0));
        assert_eq!(i("999999999999999"), Number(999_999_999_999_999.0));
        assert_eq!(i(""), Empty);
    }

    #[test]
    fn identifiers_stay_text() {
        for s in ["007", "00123", "-0012", "00.5", "0123456789", "1234567890123456", "4111111111111111", "12345678901234567890", "+15551234567", "1e400", "1e5000", "12-34", "1.2.3", "1,2", "1,23", "12,3456", "0,123", "--5", "-", ".", "1e", "5 6", "#N/A", "=A1+1", "N/A", "1/2/03", "2024-02-30", "2024-03-09T10:00:00Z", "2024-03-09 10:00+02:00", "$-5", "$$5"] {
            assert_eq!(i(s), Text(s), "{s:?} should stay text");
        }
    }

    #[test]
    fn typed_numbers() {
        assert_eq!(i("12%"), Formatted(0.12, "0%"));
        assert_eq!(i("12.5%"), Formatted(0.125, "0.00%"));
        assert_eq!(i("1,234"), Formatted(1234.0, "#,##0"));
        assert_eq!(i("-1,234,567.5"), Formatted(-1_234_567.5, "#,##0.00"));
        assert_eq!(i("$1,200"), Formatted(1200.0, "$#,##0"));
        assert_eq!(i("$3.50"), Formatted(3.5, "$#,##0.00"));
        assert_eq!(i("-$3.50"), Formatted(-3.5, "$#,##0.00"));
        assert_eq!(i("($7)"), Formatted(-7.0, "$#,##0"));
        assert_eq!(i("TRUE"), Bool(true));
        assert_eq!(i("false"), Bool(false));
        assert_eq!(i("14:30"), Formatted(14.5 / 24.0, "h:mm"));
        assert_eq!(i("06:00:30"), Formatted((6.0 * 3600.0 + 30.0) / 86_400.0, "h:mm:ss"));
    }

    #[test]
    fn dates() {
        let d = serial_from_date(2024, 3, 9);
        assert_eq!(i("2024-03-09"), Formatted(d, "yyyy-mm-dd"));
        assert_eq!(i("2024/03/09"), Formatted(d, "yyyy/mm/dd"));
        assert_eq!(i("09.03.2024"), Formatted(d, "dd.mm.yyyy"));
        assert_eq!(i("2024-03-09 06:00"), Formatted(d + 0.25, "yyyy-mm-dd hh:mm"));
        assert_eq!(i("2024-03-09T06:00:00"), Formatted(d + 0.25, "yyyy-mm-dd hh:mm:ss"));
        assert_eq!(i("2024-03-09 06:00:00.5"), Formatted(d + (6.0 * 3600.0 + 0.5) / 86_400.0, "yyyy-mm-dd hh:mm:ss"));
        // Slash dates follow the column's order.
        assert_eq!(infer("3/9/2024", DateOrder::Mdy), Formatted(d, "m/d/yyyy"));
        assert_eq!(infer("9/3/2024", DateOrder::Dmy), Formatted(d, "d/m/yyyy"));
        assert_eq!(infer("09/03/2024", DateOrder::Dmy), Formatted(d, "dd/mm/yyyy"));
        assert_eq!(infer("3/9/2024 6:00", DateOrder::Mdy), Formatted(d + 0.25, "m/d/yyyy h:mm"));
        // Impossible in the chosen order: text, never a swapped date.
        assert_eq!(infer("25/12/2024", DateOrder::Mdy), Text("25/12/2024"));
        assert_eq!(infer("12/25/2024", DateOrder::Dmy), Text("12/25/2024"));
    }

    #[test]
    fn date_order_evidence() {
        let mut e = OrderEvidence::default();
        for s in ["01/02/2024", "03/04/2024"] {
            e.observe(s);
        }
        assert_eq!(e.decide(DateOrder::Mdy), DateOrder::Mdy);
        assert_eq!(e.decide(DateOrder::Dmy), DateOrder::Dmy);
        e.observe("25/04/2024");
        assert_eq!(e.decide(DateOrder::Mdy), DateOrder::Dmy);
        e.observe("04/25/2024");
        assert_eq!(e.decide(DateOrder::Mdy), DateOrder::Mdy, "conflicting evidence falls back to the default");
    }
}
