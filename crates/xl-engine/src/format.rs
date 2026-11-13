//! Excel number format codes: parsing and rendering.
//!
//! Supports up to four `;`-separated sections (positive; negative; zero; text),
//! `0 # ?` digit placeholders, thousands separators and scaling commas,
//! percent, scientific notation, quoted/escaped literals, `[Color]`, `[$€-409]`
//! currency tags, `@` text, and date/time codes (`yyyy mmm d h:mm:ss AM/PM`,
//! elapsed `[h]`, fractional seconds).

use std::fmt::Write as _;
use std::sync::Arc;

use crate::datetime::{MONTHS, WEEKDAYS, datetime_from_serial};
use crate::value::{Value, format_general};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FmtColor {
    Black,
    White,
    Red,
    Green,
    Blue,
    Yellow,
    Magenta,
    Cyan,
}

/// A rendered value: text plus an optional color from a `[Red]`-style tag.
#[derive(Clone, Debug, PartialEq)]
pub struct Formatted {
    pub text: String,
    pub color: Option<FmtColor>,
}

#[derive(Clone, Debug, PartialEq)]
enum DatePart {
    Lit(String),
    Year(usize),
    Month(usize),
    Day(usize),
    Hour(usize),
    Minute(usize),
    Second(usize),
    /// Fractional seconds with N digits.
    Frac(usize),
    ElapsedHours,
    ElapsedMinutes,
    ElapsedSeconds,
    AmPm { short: bool, lower: bool },
}

#[derive(Clone, Debug, PartialEq)]
enum TextPart {
    Lit(String),
    At,
}

#[derive(Clone, Debug, PartialEq)]
struct NumberSection {
    prefix: String,
    suffix: String,
    int_min: usize,
    grouping: bool,
    has_point: bool,
    dec_min: usize,
    dec_max: usize,
    percent: i32,
    scale: i32,
    exp: Option<(bool, usize)>,
}

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    General { prefix: String, suffix: String },
    Number(NumberSection),
    Date(Vec<DatePart>),
    Text(Vec<TextPart>),
}

#[derive(Clone, Debug, PartialEq)]
struct Section {
    kind: Kind,
    color: Option<FmtColor>,
    /// Section contains literal sign characters (so we don't add our own '-').
    has_literal_sign: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NumberFormat {
    code: Arc<str>,
    sections: Vec<Section>,
}

pub const GENERAL: &str = "General";

impl NumberFormat {
    pub fn parse(code: &str) -> Self {
        let sections = split_sections(code).iter().map(|s| parse_section(s)).collect::<Vec<_>>();
        let sections = if sections.is_empty() { vec![parse_section("General")] } else { sections };
        Self { code: code.into(), sections }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn is_general(&self) -> bool {
        self.sections.len() == 1 && matches!(&self.sections[0].kind, Kind::General { prefix, suffix } if prefix.is_empty() && suffix.is_empty())
    }

    pub fn is_date(&self) -> bool {
        matches!(self.sections[0].kind, Kind::Date(_))
    }

    /// Date format that shows a time of day.
    pub fn has_time(&self) -> bool {
        match &self.sections[0].kind {
            Kind::Date(parts) => parts.iter().any(|p| matches!(p, DatePart::Hour(_) | DatePart::Minute(_) | DatePart::Second(_) | DatePart::ElapsedHours)),
            _ => false,
        }
    }

    pub fn has_date(&self) -> bool {
        match &self.sections[0].kind {
            Kind::Date(parts) => parts.iter().any(|p| matches!(p, DatePart::Year(_) | DatePart::Month(_) | DatePart::Day(_))),
            _ => false,
        }
    }

    pub fn is_percent(&self) -> bool {
        matches!(&self.sections[0].kind, Kind::Number(n) if n.percent > 0)
    }

    pub fn is_text(&self) -> bool {
        self.sections.len() == 1 && matches!(self.sections[0].kind, Kind::Text(_))
    }

    /// Number of decimals of the first numeric section, if any.
    pub fn decimals(&self) -> Option<usize> {
        match &self.sections[0].kind {
            Kind::Number(n) if n.exp.is_none() => Some(n.dec_max),
            _ => None,
        }
    }

    /// A copy of this format with `delta` more (or fewer) decimals.
    /// Returns None for formats where that makes no sense.
    pub fn with_decimals_delta(&self, delta: i32) -> Option<String> {
        let current = self.decimals()?;
        let target = (current as i32 + delta).clamp(0, 15) as usize;
        if target == current {
            return Some(self.code.to_string());
        }
        // Rewrite each numeric section's decimal run.
        let parts: Vec<String> = split_sections(&self.code)
            .into_iter()
            .map(|s| rewrite_decimals(&s, target))
            .collect();
        Some(parts.join(";"))
    }

    pub fn format(&self, v: &Value) -> Formatted {
        match v {
            Value::Number(n) => self.format_number(*n),
            Value::Text(t) => {
                let sec = match self.sections.len() {
                    4 => Some(&self.sections[3]),
                    1 if matches!(self.sections[0].kind, Kind::Text(_)) => Some(&self.sections[0]),
                    _ => self.sections.iter().find(|s| matches!(s.kind, Kind::Text(_))),
                };
                match sec {
                    Some(Section { kind: Kind::Text(parts), color, .. }) => {
                        let mut out = String::new();
                        for p in parts {
                            match p {
                                TextPart::Lit(l) => out.push_str(l),
                                TextPart::At => out.push_str(t),
                            }
                        }
                        Formatted { text: out, color: *color }
                    }
                    _ => Formatted { text: t.to_string(), color: None },
                }
            }
            other => Formatted { text: other.display(), color: None },
        }
    }

    fn format_number(&self, n: f64) -> Formatted {
        // Sections containing '@' are for text; literal-only sections still apply to numbers.
        let numeric: Vec<&Section> =
            self.sections.iter().filter(|s| !matches!(&s.kind, Kind::Text(p) if p.contains(&TextPart::At)) || self.sections.len() == 1).collect();
        let (sec, value, auto_sign) = match numeric.len() {
            0 => return Formatted { text: format_general(n), color: None },
            1 => (numeric[0], n, true),
            2 => {
                if n < 0.0 {
                    (numeric[1], -n, false)
                } else {
                    (numeric[0], n, false)
                }
            }
            _ => {
                if n > 0.0 {
                    (numeric[0], n, false)
                } else if n < 0.0 {
                    (numeric[1], -n, false)
                } else {
                    (numeric[2], n, false)
                }
            }
        };
        let text = match &sec.kind {
            Kind::General { prefix, suffix } => {
                let body = if auto_sign { format_general(value) } else { format_general(value.abs()) };
                format!("{prefix}{body}{suffix}")
            }
            Kind::Number(ns) => render_number(ns, value, auto_sign && !sec.has_literal_sign),
            Kind::Date(parts) => match render_date(parts, value) {
                Some(s) => s,
                None => "#".repeat(8),
            },
            Kind::Text(parts) => {
                // Text-only format applied to a number shows the number.
                let mut out = String::new();
                for p in parts {
                    match p {
                        TextPart::Lit(l) => out.push_str(l),
                        TextPart::At => out.push_str(&format_general(n)),
                    }
                }
                out
            }
        };
        Formatted { text, color: sec.color }
    }
}

fn split_sections(code: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut chars = code.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ';' => out.push(String::new()),
            '"' => {
                let cur = out.last_mut().unwrap();
                cur.push('"');
                for c2 in chars.by_ref() {
                    cur.push(c2);
                    if c2 == '"' {
                        break;
                    }
                }
            }
            '\\' => {
                let cur = out.last_mut().unwrap();
                cur.push('\\');
                if let Some(c2) = chars.next() {
                    cur.push(c2);
                }
            }
            '[' => {
                let cur = out.last_mut().unwrap();
                cur.push('[');
                for c2 in chars.by_ref() {
                    cur.push(c2);
                    if c2 == ']' {
                        break;
                    }
                }
            }
            _ => out.last_mut().unwrap().push(c),
        }
    }
    out.truncate(4);
    out
}

fn color_of(name: &str) -> Option<FmtColor> {
    Some(match name.to_ascii_lowercase().as_str() {
        "black" => FmtColor::Black,
        "white" => FmtColor::White,
        "red" => FmtColor::Red,
        "green" => FmtColor::Green,
        "blue" => FmtColor::Blue,
        "yellow" => FmtColor::Yellow,
        "magenta" => FmtColor::Magenta,
        "cyan" => FmtColor::Cyan,
        _ => return None,
    })
}

/// Lexical token of a format section.
#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Lit(String),
    Digit(char),
    Point,
    Comma,
    Percent,
    Exp(bool),
    At,
    General,
    Date(char, usize),
    AmPm { short: bool, lower: bool },
    Elapsed(char),
}

fn tokenize(s: &str) -> (Vec<Tok>, Option<FmtColor>) {
    let chars: Vec<char> = s.chars().collect();
    let mut toks = Vec::new();
    let mut color = None;
    let mut i = 0;
    let lit = |toks: &mut Vec<Tok>, s: &str| {
        if let Some(Tok::Lit(prev)) = toks.last_mut() {
            prev.push_str(s);
        } else {
            toks.push(Tok::Lit(s.to_string()));
        }
    };
    while i < chars.len() {
        let c = chars[i];
        let rest: String = chars[i..].iter().collect();
        let upper = rest.to_ascii_uppercase();
        match c {
            '"' => {
                let mut j = i + 1;
                let mut s = String::new();
                while j < chars.len() && chars[j] != '"' {
                    s.push(chars[j]);
                    j += 1;
                }
                lit(&mut toks, &s);
                i = j + 1;
            }
            '\\' => {
                if let Some(&n) = chars.get(i + 1) {
                    lit(&mut toks, &n.to_string());
                }
                i += 2;
            }
            '_' => {
                lit(&mut toks, " ");
                i += 2;
            }
            '*' => {
                // Fill: repeat to the column width. Without a width, show it once.
                if let Some(&n) = chars.get(i + 1) {
                    lit(&mut toks, &n.to_string());
                }
                i += 2;
            }
            '[' => {
                let end = chars[i..].iter().position(|&c| c == ']').map_or(chars.len(), |p| i + p);
                let inner: String = chars[i + 1..end].iter().collect();
                let lower = inner.to_ascii_lowercase();
                if let Some(col) = color_of(&inner) {
                    color = Some(col);
                } else if let Some(cur) = inner.strip_prefix('$') {
                    let sym = cur.split('-').next().unwrap_or("");
                    lit(&mut toks, sym);
                } else if !lower.is_empty() && lower.chars().all(|c| c == 'h') {
                    toks.push(Tok::Elapsed('h'));
                } else if !lower.is_empty() && lower.chars().all(|c| c == 'm') {
                    toks.push(Tok::Elapsed('m'));
                } else if !lower.is_empty() && lower.chars().all(|c| c == 's') {
                    toks.push(Tok::Elapsed('s'));
                }
                // Conditions like [>100] and other tags are ignored.
                i = end + 1;
            }
            _ if upper.starts_with("GENERAL") => {
                toks.push(Tok::General);
                i += 7;
            }
            _ if upper.starts_with("AM/PM") => {
                toks.push(Tok::AmPm { short: false, lower: c == 'a' });
                i += 5;
            }
            _ if upper.starts_with("A/P") => {
                toks.push(Tok::AmPm { short: true, lower: c == 'a' });
                i += 3;
            }
            '0' | '#' | '?' => {
                toks.push(Tok::Digit(c));
                i += 1;
            }
            '.' => {
                toks.push(Tok::Point);
                i += 1;
            }
            ',' => {
                toks.push(Tok::Comma);
                i += 1;
            }
            '%' => {
                toks.push(Tok::Percent);
                i += 1;
            }
            '@' => {
                toks.push(Tok::At);
                i += 1;
            }
            'E' | 'e' if matches!(chars.get(i + 1), Some('+') | Some('-')) => {
                toks.push(Tok::Exp(chars[i + 1] == '+'));
                i += 2;
            }
            'y' | 'Y' | 'm' | 'M' | 'd' | 'D' | 'h' | 'H' | 's' | 'S' => {
                let lc = c.to_ascii_lowercase();
                let mut n = 0;
                while i + n < chars.len() && chars[i + n].to_ascii_lowercase() == lc {
                    n += 1;
                }
                toks.push(Tok::Date(lc, n));
                i += n;
            }
            _ => {
                lit(&mut toks, &c.to_string());
                i += 1;
            }
        }
    }
    (toks, color)
}

fn parse_section(s: &str) -> Section {
    let (toks, color) = tokenize(s);
    let has_literal_sign = toks.iter().any(|t| matches!(t, Tok::Lit(l) if l.contains('-') || l.contains('(')));
    let is_date = toks.iter().any(|t| matches!(t, Tok::Date(..) | Tok::AmPm { .. } | Tok::Elapsed(_)));
    let has_digits = toks.iter().any(|t| matches!(t, Tok::Digit(_)));
    let kind = if is_date {
        Kind::Date(build_date(&toks))
    } else if toks.contains(&Tok::General) {
        let mut prefix = String::new();
        let mut suffix = String::new();
        let mut seen = false;
        for t in &toks {
            match t {
                Tok::General => seen = true,
                Tok::Lit(l) if !seen => prefix.push_str(l),
                Tok::Lit(l) => suffix.push_str(l),
                _ => {}
            }
        }
        Kind::General { prefix, suffix }
    } else if !has_digits {
        if toks.contains(&Tok::At) || !toks.is_empty() {
            Kind::Text(
                toks.iter()
                    .filter_map(|t| match t {
                        Tok::At => Some(TextPart::At),
                        Tok::Lit(l) => Some(TextPart::Lit(l.clone())),
                        Tok::Percent => Some(TextPart::Lit("%".into())),
                        Tok::Comma => Some(TextPart::Lit(",".into())),
                        Tok::Point => Some(TextPart::Lit(".".into())),
                        _ => None,
                    })
                    .collect(),
            )
        } else {
            Kind::General { prefix: String::new(), suffix: String::new() }
        }
    } else {
        Kind::Number(build_number(&toks))
    };
    Section { kind, color, has_literal_sign }
}

fn build_number(toks: &[Tok]) -> NumberSection {
    let first_digit = toks.iter().position(|t| matches!(t, Tok::Digit(_))).unwrap();
    let last_digit = toks.iter().rposition(|t| matches!(t, Tok::Digit(_))).unwrap();
    let mut ns = NumberSection {
        prefix: String::new(),
        suffix: String::new(),
        int_min: 0,
        grouping: false,
        has_point: false,
        dec_min: 0,
        dec_max: 0,
        percent: 0,
        scale: 0,
        exp: None,
    };
    let mut in_dec = false;
    let mut in_exp = false;
    let mut exp_digits = 0;
    let mut exp_plus = false;
    for (i, t) in toks.iter().enumerate() {
        match t {
            Tok::Lit(l) if i < first_digit && !in_dec => ns.prefix.push_str(l),
            Tok::Lit(l) if i > last_digit => ns.suffix.push_str(l),
            Tok::Lit(_) => {}
            Tok::Percent => {
                ns.percent += 1;
                if i < first_digit {
                    ns.prefix.push('%');
                } else {
                    ns.suffix.push('%');
                }
            }
            Tok::Digit(d) if in_exp => {
                let _ = d;
                exp_digits += 1;
            }
            Tok::Digit(d) if in_dec => {
                ns.dec_max += 1;
                if *d == '0' {
                    ns.dec_min = ns.dec_max;
                }
            }
            Tok::Digit(d) => {
                if *d == '0' {
                    ns.int_min += 1;
                }
            }
            Tok::Point if !in_exp => {
                in_dec = true;
                ns.has_point = true;
            }
            Tok::Comma => {
                // Comma followed (eventually) by more integer digits = grouping; trailing = scale.
                let more_int_digits = toks[i + 1..]
                    .iter()
                    .take_while(|t| !matches!(t, Tok::Point | Tok::Exp(_)))
                    .any(|t| matches!(t, Tok::Digit(_)));
                if more_int_digits && !in_dec {
                    ns.grouping = true;
                } else if i > last_digit || !more_int_digits {
                    ns.scale += 1;
                }
            }
            Tok::Exp(plus) => {
                in_exp = true;
                exp_plus = *plus;
            }
            _ => {}
        }
    }
    if in_exp {
        ns.exp = Some((exp_plus, exp_digits.max(1)));
    }
    ns
}

fn build_date(toks: &[Tok]) -> Vec<DatePart> {
    let mut parts = Vec::new();
    let has_ampm = toks.iter().any(|t| matches!(t, Tok::AmPm { .. }));
    for (i, t) in toks.iter().enumerate() {
        let part = match t {
            Tok::Lit(l) => DatePart::Lit(l.clone()),
            Tok::Comma => DatePart::Lit(",".into()),
            Tok::Percent => DatePart::Lit("%".into()),
            Tok::Date('y', n) => DatePart::Year(*n),
            Tok::Date('d', n) => DatePart::Day(*n),
            Tok::Date('h', n) => DatePart::Hour(*n),
            Tok::Date('s', n) => DatePart::Second(*n),
            Tok::Date('m', n) => {
                // Minutes if right after an hour or right before seconds.
                let prev_hour = toks[..i].iter().rev().find(|t| !matches!(t, Tok::Lit(_))).is_some_and(|t| matches!(t, Tok::Date('h', _) | Tok::Elapsed('h')));
                let next_sec = toks[i + 1..].iter().find(|t| !matches!(t, Tok::Lit(_))).is_some_and(|t| matches!(t, Tok::Date('s', _)));
                if (prev_hour || next_sec) && *n <= 2 { DatePart::Minute(*n) } else { DatePart::Month(*n) }
            }
            Tok::Point => {
                // ".0" / ".00" after seconds = fractional seconds.
                let zeros = toks[i + 1..].iter().take_while(|t| matches!(t, Tok::Digit('0'))).count();
                if zeros > 0 {
                    DatePart::Frac(zeros)
                } else {
                    DatePart::Lit(".".into())
                }
            }
            Tok::Digit(_) => continue, // consumed by Frac
            Tok::AmPm { short, lower } => DatePart::AmPm { short: *short, lower: *lower },
            Tok::Elapsed('h') => DatePart::ElapsedHours,
            Tok::Elapsed('m') => DatePart::ElapsedMinutes,
            Tok::Elapsed(_) => DatePart::ElapsedSeconds,
            _ => continue,
        };
        parts.push(part);
    }
    let _ = has_ampm;
    parts
}

fn render_date(parts: &[DatePart], serial: f64) -> Option<String> {
    let frac_digits = parts.iter().find_map(|p| if let DatePart::Frac(n) = p { Some(*n) } else { None }).unwrap_or(0);
    // Excel rounds to whole seconds unless fractional seconds are displayed.
    let rounded = if frac_digits == 0 { (serial * 86_400.0).round() / 86_400.0 } else { serial };
    let dt = datetime_from_serial(rounded)?;
    let twelve = parts.iter().any(|p| matches!(p, DatePart::AmPm { .. }));
    let mut out = String::new();
    for p in parts {
        match p {
            DatePart::Lit(l) => out.push_str(l),
            DatePart::Year(n) if *n <= 2 => {
                let _ = write!(out, "{:02}", dt.year.rem_euclid(100));
            }
            DatePart::Year(_) => {
                let _ = write!(out, "{}", dt.year);
            }
            DatePart::Month(1) => {
                let _ = write!(out, "{}", dt.month);
            }
            DatePart::Month(2) => {
                let _ = write!(out, "{:02}", dt.month);
            }
            DatePart::Month(3) => out.push_str(&MONTHS[dt.month as usize - 1][..3]),
            DatePart::Month(5) => out.push_str(&MONTHS[dt.month as usize - 1][..1]),
            DatePart::Month(_) => out.push_str(MONTHS[dt.month as usize - 1]),
            DatePart::Day(1) => {
                let _ = write!(out, "{}", dt.day);
            }
            DatePart::Day(2) => {
                let _ = write!(out, "{:02}", dt.day);
            }
            DatePart::Day(3) => out.push_str(&WEEKDAYS[dt.weekday as usize][..3]),
            DatePart::Day(_) => out.push_str(WEEKDAYS[dt.weekday as usize]),
            DatePart::Hour(n) => {
                let h = if twelve {
                    match dt.hour % 12 {
                        0 => 12,
                        h => h,
                    }
                } else {
                    dt.hour
                };
                let _ = if *n >= 2 { write!(out, "{h:02}") } else { write!(out, "{h}") };
            }
            DatePart::Minute(n) => {
                let _ = if *n >= 2 { write!(out, "{:02}", dt.minute) } else { write!(out, "{}", dt.minute) };
            }
            DatePart::Second(n) => {
                let _ = if *n >= 2 { write!(out, "{:02}", dt.second) } else { write!(out, "{}", dt.second) };
            }
            DatePart::Frac(n) => {
                let ms = format!("{:03}", dt.millis);
                out.push('.');
                out.push_str(&ms[..(*n).min(3)]);
            }
            DatePart::ElapsedHours => {
                let _ = write!(out, "{}", (serial * 24.0).floor() as i64);
            }
            DatePart::ElapsedMinutes => {
                let _ = write!(out, "{}", (serial * 1440.0).floor() as i64);
            }
            DatePart::ElapsedSeconds => {
                let _ = write!(out, "{}", (serial * 86_400.0).round() as i64);
            }
            DatePart::AmPm { short, lower } => {
                let pm = dt.hour >= 12;
                let s = match (short, pm) {
                    (false, false) => "AM",
                    (false, true) => "PM",
                    (true, false) => "A",
                    (true, true) => "P",
                };
                out.push_str(&if *lower { s.to_lowercase() } else { s.to_string() });
            }
        }
    }
    Some(out)
}

fn group_thousands(int: &str) -> String {
    let mut out = String::with_capacity(int.len() + int.len() / 3);
    let n = int.len();
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (n - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn render_number(ns: &NumberSection, n: f64, auto_sign: bool) -> String {
    let mut v = n * 100f64.powi(ns.percent) / 1000f64.powi(ns.scale);
    let neg = v < 0.0;
    v = v.abs();
    let mut body = String::new();
    if let Some((plus, exp_digits)) = ns.exp {
        let mut e = if v == 0.0 { 0 } else { v.log10().floor() as i32 };
        let mut mant = v / 10f64.powi(e);
        let rounded = round_excel(mant, ns.dec_max);
        if rounded >= 10.0 {
            e += 1;
            mant = v / 10f64.powi(e);
        }
        body.push_str(&render_fixed(ns, mant));
        let sign = if e < 0 { "-" } else if plus { "+" } else { "" };
        let _ = write!(body, "E{sign}{:0width$}", e.abs(), width = exp_digits);
    } else {
        body = render_fixed(ns, v);
    }
    // Avoid "-0" when everything rounded away.
    let all_zero = !body.chars().any(|c| c.is_ascii_digit() && c != '0');
    let sign = if neg && auto_sign && !all_zero { "-" } else { "" };
    format!("{sign}{}{body}{}", ns.prefix, ns.suffix)
}

/// Round half away from zero (Excel), nudging values that are a hair below a
/// tie only because of binary representation (e.g. 2.675).
fn round_excel(v: f64, decimals: usize) -> f64 {
    let m = 10f64.powi(decimals as i32);
    let scaled = v * m;
    let nudged = if (scaled - scaled.trunc()).abs() > 0.5 - 1e-9 && (scaled - scaled.trunc()).abs() < 0.5 { scaled.trunc() + scaled.signum() * 0.5 } else { scaled };
    nudged.round() / m
}

fn render_fixed(ns: &NumberSection, v: f64) -> String {
    let s = format!("{:.*}", ns.dec_max, round_excel(v, ns.dec_max));
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i.to_string(), f.to_string()),
        None => (s.clone(), String::new()),
    };
    let mut frac = frac;
    while frac.len() > ns.dec_min && frac.ends_with('0') {
        frac.pop();
    }
    let mut int = int.trim_start_matches('0').to_string();
    while int.len() < ns.int_min {
        int.insert(0, '0');
    }
    if ns.grouping {
        int = group_thousands(&int);
    }
    let mut out = int;
    if ns.has_point && (ns.dec_max > 0 || !frac.is_empty()) {
        if !frac.is_empty() || ns.dec_min > 0 {
            out.push('.');
            out.push_str(&frac);
        } else if ns.dec_max > 0 && out.is_empty() {
            out.push('0');
        }
    }
    if out.is_empty() {
        out.push('0');
    }
    out
}

fn rewrite_decimals(section: &str, target: usize) -> String {
    // Find the run of digit placeholders after the first unquoted '.', or the
    // last digit placeholder if there's no point, and rewrite it.
    let chars: Vec<char> = section.chars().collect();
    let mut in_quote = false;
    let mut in_bracket = false;
    let mut last_digit = None;
    let mut point = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' => in_quote = !in_quote,
            '[' if !in_quote => in_bracket = true,
            ']' if !in_quote => in_bracket = false,
            '\\' if !in_quote => i += 1,
            '0' | '#' | '?' if !in_quote && !in_bracket => {
                if point.is_none() {
                    last_digit = Some(i);
                }
            }
            '.' if !in_quote && !in_bracket && point.is_none() && last_digit.is_some() => point = Some(i),
            _ => {}
        }
        i += 1;
    }
    let Some(last_int) = last_digit else { return section.to_string() };
    let (head_end, tail_start) = match point {
        Some(p) => {
            let mut e = p + 1;
            while e < chars.len() && matches!(chars[e], '0' | '#' | '?') {
                e += 1;
            }
            (p, e)
        }
        None => (last_int + 1, last_int + 1),
    };
    let mut out: String = chars[..head_end].iter().collect();
    if target > 0 {
        out.push('.');
        out.push_str(&"0".repeat(target));
    }
    out.extend(&chars[tail_start..]);
    out
}

/// Excel's built-in number formats by numFmtId (as used in xlsx styles).
pub fn builtin_format(id: u32) -> Option<&'static str> {
    Some(match id {
        0 => "General",
        1 => "0",
        2 => "0.00",
        3 => "#,##0",
        4 => "#,##0.00",
        5 => "$#,##0_);($#,##0)",
        6 => "$#,##0_);[Red]($#,##0)",
        7 => "$#,##0.00_);($#,##0.00)",
        8 => "$#,##0.00_);[Red]($#,##0.00)",
        9 => "0%",
        10 => "0.00%",
        11 => "0.00E+00",
        12 => "# ?/?",
        13 => "# ??/??",
        14 => "m/d/yyyy",
        15 => "d-mmm-yy",
        16 => "d-mmm",
        17 => "mmm-yy",
        18 => "h:mm AM/PM",
        19 => "h:mm:ss AM/PM",
        20 => "h:mm",
        21 => "h:mm:ss",
        22 => "m/d/yyyy h:mm",
        37 => "#,##0 ;(#,##0)",
        38 => "#,##0 ;[Red](#,##0)",
        39 => "#,##0.00;(#,##0.00)",
        40 => "#,##0.00;[Red](#,##0.00)",
        44 => "_($* #,##0.00_);_($* (#,##0.00);_($* \"-\"??_);_(@_)",
        45 => "mm:ss",
        46 => "[h]:mm:ss",
        47 => "mm:ss.0",
        48 => "##0.0E+0",
        49 => "@",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datetime::serial_from_date;

    fn f(code: &str, v: f64) -> String {
        NumberFormat::parse(code).format(&Value::Number(v)).text
    }

    #[test]
    fn numbers() {
        assert_eq!(f("General", 1234.5), "1234.5");
        assert_eq!(f("0", 2.5), "3");
        assert_eq!(f("0.00", 4.56789), "4.57");
        assert_eq!(f("#,##0", 1234567.8), "1,234,568");
        assert_eq!(f("#,##0.00", -1234.5), "-1,234.50");
        assert_eq!(f("$#,##0.00", 1234.5), "$1,234.50");
        assert_eq!(f("0%", 0.256), "26%");
        assert_eq!(f("0.0%", 0.256), "25.6%");
        assert_eq!(f("0.00E+00", 12345.0), "1.23E+04");
        assert_eq!(f("0.00E+00", 0.000123), "1.23E-04");
        assert_eq!(f("#.00", 0.5), ".50");
        assert_eq!(f("000", 7.0), "007");
        assert_eq!(f("#,##0,\"K\"", 12_345.0), "12K");
        assert_eq!(f("0.0#", 1.0), "1.0");
        assert_eq!(f("0.0#", 1.234), "1.23");
    }

    #[test]
    fn sections_and_colors() {
        assert_eq!(f("#,##0.00;(#,##0.00)", -5.0), "(5.00)");
        let fmt = NumberFormat::parse("0;[Red]-0");
        let r = fmt.format(&Value::Number(-3.0));
        assert_eq!(r.text, "-3");
        assert_eq!(r.color, Some(FmtColor::Red));
        assert_eq!(f("0;-0;\"zero\"", 0.0), "zero");
        assert_eq!(f("$#,##0.00_);($#,##0.00)", 12.0), "$12.00 ");
        let acct = builtin_format(44).unwrap();
        assert_eq!(f(acct, 1234.5), " $ 1,234.50 ");
    }

    #[test]
    fn dates_and_times() {
        let d = serial_from_date(2026, 11, 13);
        assert_eq!(f("m/d/yyyy", d), "11/13/2026");
        assert_eq!(f("yyyy-mm-dd", d), "2026-11-13");
        assert_eq!(f("d-mmm-yy", d), "13-Nov-26");
        assert_eq!(f("dddd, mmmm d", d), "Friday, November 13");
        assert_eq!(f("h:mm AM/PM", d + 0.75), "6:00 PM");
        assert_eq!(f("h:mm:ss", 0.5 + 1.0 / 86400.0 * 5.0), "12:00:05");
        assert_eq!(f("[h]:mm", 1.5), "36:00");
        assert_eq!(f("mm:ss.0", 1.5 / 86400.0), "00:01.5");
        assert_eq!(f("m/d/yyyy h:mm", d + 0.25), "11/13/2026 6:00");
    }

    #[test]
    fn text_sections() {
        let fmt = NumberFormat::parse("0;-0;0;\"(\"@\")\"");
        assert_eq!(fmt.format(&Value::text("hi")).text, "(hi)");
        assert_eq!(NumberFormat::parse("@").format(&Value::text("x")).text, "x");
        assert!(NumberFormat::parse("@").is_text());
    }

    #[test]
    fn decimal_adjustment() {
        let fmt = NumberFormat::parse("#,##0.00");
        assert_eq!(fmt.with_decimals_delta(1).unwrap(), "#,##0.000");
        assert_eq!(fmt.with_decimals_delta(-2).unwrap(), "#,##0");
        let fmt = NumberFormat::parse("0%");
        assert_eq!(fmt.with_decimals_delta(1).unwrap(), "0.0%");
        let fmt = NumberFormat::parse("$#,##0.00_);($#,##0.00)");
        assert_eq!(fmt.with_decimals_delta(-1).unwrap(), "$#,##0.0_);($#,##0.0)");
    }

    #[test]
    fn classification() {
        assert!(NumberFormat::parse("m/d/yyyy").is_date());
        assert!(NumberFormat::parse("h:mm").has_time());
        assert!(!NumberFormat::parse("h:mm").has_date());
        assert!(NumberFormat::parse("0.0%").is_percent());
        assert!(NumberFormat::parse("General").is_general());
        assert!(!NumberFormat::parse("0.00").is_date());
    }
}
