//! Expression evaluation and the built-in function library.

use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::address::{CellRef, Range};
use crate::parser::{BinOp, Expr};
use crate::store::CellStore;
use crate::value::{ErrorKind, EvalResult, Value, parse_number};

macro_rules! funcs {
    ($($variant:ident => $name:literal),* $(,)?) => {
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
        pub enum Func { $($variant),* }

        impl Func {
            pub const ALL: &'static [Func] = &[$(Func::$variant),*];

            pub fn name(self) -> &'static str {
                match self { $(Func::$variant => $name),* }
            }

            pub fn from_name(s: &str) -> Option<Func> {
                let up = s.to_ascii_uppercase();
                match up.as_str() { $($name => Some(Func::$variant),)* _ => None }
            }
        }
    };
}

funcs! {
    Sum => "SUM", Product => "PRODUCT", Average => "AVERAGE", Min => "MIN", Max => "MAX",
    Count => "COUNT", CountA => "COUNTA", CountBlank => "COUNTBLANK",
    SumIf => "SUMIF", CountIf => "COUNTIF", AverageIf => "AVERAGEIF",
    Abs => "ABS", Sqrt => "SQRT", Int => "INT", Mod => "MOD", Power => "POWER",
    Round => "ROUND", RoundUp => "ROUNDUP", RoundDown => "ROUNDDOWN",
    Pi => "PI", Exp => "EXP", Ln => "LN", Log10 => "LOG10", Log => "LOG", Sign => "SIGN",
    If => "IF", IfError => "IFERROR", IfNa => "IFNA", And => "AND", Or => "OR", Not => "NOT",
    True => "TRUE", False => "FALSE",
    Len => "LEN", Upper => "UPPER", Lower => "LOWER", Trim => "TRIM",
    Left => "LEFT", Right => "RIGHT", Mid => "MID",
    Concat => "CONCAT", Concatenate => "CONCATENATE", Rept => "REPT", Exact => "EXACT",
    IsBlank => "ISBLANK", IsNumber => "ISNUMBER", IsText => "ISTEXT", IsError => "ISERROR", IsNa => "ISNA",
    VLookup => "VLOOKUP", HLookup => "HLOOKUP", Match => "MATCH", Index => "INDEX",
    Now => "NOW", Today => "TODAY", Rand => "RAND", RandBetween => "RANDBETWEEN",
}

impl Func {
    pub fn is_volatile(self) -> bool {
        matches!(self, Func::Now | Func::Today | Func::Rand | Func::RandBetween)
    }

    /// Short signature shown in the UI.
    pub fn signature(self) -> &'static str {
        use Func::*;
        match self {
            Sum | Product | Average | Min | Max | Count | CountA => "(value1, [value2], …)",
            CountBlank => "(range)",
            SumIf | AverageIf => "(range, criteria, [sum_range])",
            CountIf => "(range, criteria)",
            Abs | Sqrt | Int | Exp | Ln | Log10 | Sign => "(number)",
            Log => "(number, [base])",
            Mod => "(number, divisor)",
            Power => "(number, power)",
            Round | RoundUp | RoundDown => "(number, num_digits)",
            Pi | True | False | Now | Today | Rand => "()",
            RandBetween => "(bottom, top)",
            If => "(test, value_if_true, [value_if_false])",
            IfError | IfNa => "(value, value_if_error)",
            And | Or => "(logical1, [logical2], …)",
            Not => "(logical)",
            Len | Upper | Lower | Trim => "(text)",
            Left | Right => "(text, [num_chars])",
            Mid => "(text, start_num, num_chars)",
            Concat | Concatenate => "(text1, [text2], …)",
            Rept => "(text, number_times)",
            Exact => "(text1, text2)",
            IsBlank | IsNumber | IsText | IsError | IsNa => "(value)",
            VLookup => "(lookup_value, table_array, col_index_num, [range_lookup])",
            HLookup => "(lookup_value, table_array, row_index_num, [range_lookup])",
            Match => "(lookup_value, lookup_array, [match_type])",
            Index => "(array, row_num, [column_num])",
        }
    }
}

/// Evaluate an expression in scalar context.
pub fn eval(e: &Expr, s: &CellStore) -> Value {
    match e {
        Expr::Number(n) => Value::Number(*n),
        Expr::Text(t) => Value::Text(t.clone()),
        Expr::Bool(b) => Value::Bool(*b),
        Expr::Error(err) => Value::Error(*err),
        Expr::Ref(r) => s.value(r.cell).clone(),
        Expr::Range(..) => {
            // Single-cell ranges collapse; others need implicit intersection (not yet).
            let r = e.range().unwrap();
            if r.start == r.end { s.value(r.start).clone() } else { Value::Error(ErrorKind::Value) }
        }
        Expr::Paren(x) | Expr::Plus(x) => eval(x, s),
        Expr::Neg(x) => match eval(x, s).to_number() {
            Ok(n) => Value::num(-n),
            Err(err) => Value::Error(err),
        },
        Expr::Percent(x) => match eval(x, s).to_number() {
            Ok(n) => Value::num(n / 100.0),
            Err(err) => Value::Error(err),
        },
        Expr::Binary(op, a, b) => binary(*op, eval(a, s), eval(b, s)),
        Expr::Call(f, args) => call(*f, args, s).unwrap_or_else(Value::Error),
        Expr::UnknownFunc(..) => Value::Error(ErrorKind::Name),
    }
}

fn binary(op: BinOp, a: Value, b: Value) -> Value {
    if let Value::Error(e) = a {
        return Value::Error(e);
    }
    if let Value::Error(e) = b {
        return Value::Error(e);
    }
    let arith = |f: fn(f64, f64) -> Value| match (a.to_number(), b.to_number()) {
        (Ok(x), Ok(y)) => f(x, y),
        (Err(e), _) | (_, Err(e)) => Value::Error(e),
    };
    match op {
        BinOp::Add => arith(|x, y| Value::num(x + y)),
        BinOp::Sub => arith(|x, y| Value::num(x - y)),
        BinOp::Mul => arith(|x, y| Value::num(x * y)),
        BinOp::Div => arith(|x, y| if y == 0.0 { Value::Error(ErrorKind::Div0) } else { Value::num(x / y) }),
        BinOp::Pow => arith(|x, y| {
            if x == 0.0 && y == 0.0 { Value::Error(ErrorKind::Num) } else { Value::num(x.powf(y)) }
        }),
        BinOp::Concat => match (a.to_text(), b.to_text()) {
            (Ok(x), Ok(y)) => Value::text(format!("{x}{y}")),
            (Err(e), _) | (_, Err(e)) => Value::Error(e),
        },
        BinOp::Eq => Value::Bool(compare(&a, &b) == Ordering::Equal),
        BinOp::Ne => Value::Bool(compare(&a, &b) != Ordering::Equal),
        BinOp::Lt => Value::Bool(compare(&a, &b) == Ordering::Less),
        BinOp::Le => Value::Bool(compare(&a, &b) != Ordering::Greater),
        BinOp::Gt => Value::Bool(compare(&a, &b) == Ordering::Greater),
        BinOp::Ge => Value::Bool(compare(&a, &b) != Ordering::Less),
    }
}

/// Excel ordering: numbers < text < booleans; text is case-insensitive;
/// empty compares as 0 / "" / FALSE depending on the other side.
pub fn compare(a: &Value, b: &Value) -> Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Number(_) => 0,
            Value::Text(_) => 1,
            Value::Bool(_) => 2,
            _ => 3,
        }
    }
    match (a, b) {
        (Value::Empty, Value::Empty) => Ordering::Equal,
        (Value::Empty, Value::Number(y)) => 0.0f64.total_cmp(y),
        (Value::Number(x), Value::Empty) => x.total_cmp(&0.0),
        (Value::Empty, Value::Text(y)) => "".cmp(y.to_lowercase().as_str()),
        (Value::Text(x), Value::Empty) => x.to_lowercase().as_str().cmp(""),
        (Value::Empty, Value::Bool(y)) => false.cmp(y),
        (Value::Bool(x), Value::Empty) => x.cmp(&false),
        (Value::Number(x), Value::Number(y)) => x.total_cmp(y),
        (Value::Text(x), Value::Text(y)) => x.to_lowercase().cmp(&y.to_lowercase()),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => rank(a).cmp(&rank(b)),
    }
}

fn arg_count(args: &[Expr], min: usize, max: usize) -> EvalResult<()> {
    if args.len() < min || args.len() > max { Err(ErrorKind::Value) } else { Ok(()) }
}

fn num_arg(args: &[Expr], i: usize, s: &CellStore) -> EvalResult<f64> {
    eval(&args[i], s).to_number()
}

fn opt_num(args: &[Expr], i: usize, s: &CellStore, default: f64) -> EvalResult<f64> {
    match args.get(i) {
        Some(e) => eval(e, s).to_number(),
        None => Ok(default),
    }
}

fn text_arg(args: &[Expr], i: usize, s: &CellStore) -> EvalResult<Arc<str>> {
    eval(&args[i], s).to_text()
}

fn range_arg(args: &[Expr], i: usize) -> EvalResult<Range> {
    args.get(i).and_then(Expr::range).ok_or(ErrorKind::Value)
}

/// Visit numbers the way SUM does: references contribute only numeric cells
/// (errors propagate), direct arguments are coerced.
fn for_each_number(args: &[Expr], s: &CellStore, mut f: impl FnMut(f64)) -> EvalResult<()> {
    for a in args {
        if let Some(r) = a.range() {
            for (_, cell) in s.iter_range(r) {
                match &cell.value {
                    Value::Number(n) => f(*n),
                    Value::Error(e) => return Err(*e),
                    _ => {}
                }
            }
        } else {
            match eval(a, s) {
                Value::Empty => {}
                v => f(v.to_number()?),
            }
        }
    }
    Ok(())
}

fn round_to(n: f64, digits: f64, mode: fn(f64) -> f64) -> Value {
    let d = digits.trunc() as i32;
    let m = 10f64.powi(d);
    // Nudge to counter binary representation error (2.675 -> 2.68 like Excel).
    let scaled = n * m;
    let scaled = if (scaled - scaled.round()).abs() < 1e-9 { scaled.round() } else { scaled };
    Value::num(mode(scaled) / m)
}

fn round_half_away(x: f64) -> f64 {
    x.round()
}

fn round_away(x: f64) -> f64 {
    if x >= 0.0 { x.ceil() } else { x.floor() }
}

static RNG: AtomicU64 = AtomicU64::new(0);

fn random() -> f64 {
    let mut x = RNG.load(AtomicOrdering::Relaxed);
    if x == 0 {
        x = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0x9E37_79B9, |d| d.as_nanos() as u64) | 1;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    RNG.store(x, AtomicOrdering::Relaxed);
    (x >> 11) as f64 / (1u64 << 53) as f64
}

/// Current time as an Excel serial date (days since 1899-12-30, UTC).
fn excel_now() -> f64 {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    secs / 86_400.0 + 25_569.0
}

/// A parsed COUNTIF/SUMIF criterion like `">=5"`, `"<>x"`, `"ap*"`.
struct Criterion {
    op: BinOp,
    value: Value,
}

impl Criterion {
    fn new(v: Value) -> Self {
        let Value::Text(t) = &v else { return Self { op: BinOp::Eq, value: v } };
        let (op, rest) = [("<=", BinOp::Le), (">=", BinOp::Ge), ("<>", BinOp::Ne), ("<", BinOp::Lt), (">", BinOp::Gt), ("=", BinOp::Eq)]
            .iter()
            .find_map(|(p, op)| t.strip_prefix(p).map(|r| (*op, r)))
            .unwrap_or((BinOp::Eq, t));
        let value = match parse_number(rest) {
            Some(n) => Value::Number(n),
            None if rest.eq_ignore_ascii_case("TRUE") => Value::Bool(true),
            None if rest.eq_ignore_ascii_case("FALSE") => Value::Bool(false),
            None if rest.is_empty() => Value::Empty,
            None => Value::text(rest),
        };
        Self { op, value }
    }

    fn matches(&self, v: &Value) -> bool {
        if let (Value::Text(pat), Value::Text(t)) = (&self.value, v)
            && matches!(self.op, BinOp::Eq | BinOp::Ne)
        {
            let m = wildcard_match(&pat.to_lowercase(), &t.to_lowercase());
            return (self.op == BinOp::Eq) == m;
        }
        // Type mismatches never match except for <>.
        let same_kind = std::mem::discriminant(v) == std::mem::discriminant(&self.value)
            || (self.value.is_empty() && matches!(v, Value::Text(t) if t.is_empty()));
        if !same_kind {
            return self.op == BinOp::Ne;
        }
        matches!(binary(self.op, v.clone(), self.value.clone()), Value::Bool(true))
    }
}

fn wildcard_match(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn lookup_exact(target: &Value, cells: impl Iterator<Item = (u32, Value)>) -> Option<u32> {
    let mut wild = None;
    if let Value::Text(t) = target
        && (t.contains('*') || t.contains('?'))
    {
        wild = Some(t.to_lowercase());
    }
    for (i, v) in cells {
        let hit = match (&wild, &v) {
            (Some(p), Value::Text(t)) => wildcard_match(p, &t.to_lowercase()),
            _ => !v.is_empty() && compare(&v, target) == Ordering::Equal,
        };
        if hit {
            return Some(i);
        }
    }
    None
}

/// Largest value <= target (match_type 1) or smallest >= target (-1), assuming sorted data.
fn lookup_approx(target: &Value, cells: impl Iterator<Item = (u32, Value)>, descending: bool) -> Option<u32> {
    let mut best = None;
    for (i, v) in cells {
        if v.is_empty() || std::mem::discriminant(&v) != std::mem::discriminant(target) {
            continue;
        }
        let ord = compare(&v, target);
        if ord == Ordering::Equal {
            return Some(i);
        }
        if (!descending && ord == Ordering::Less) || (descending && ord == Ordering::Greater) {
            best = Some(i);
        } else {
            break;
        }
    }
    best
}

fn call(f: Func, args: &[Expr], s: &CellStore) -> EvalResult<Value> {
    use Func::*;
    Ok(match f {
        Sum => {
            let mut t = 0.0;
            for_each_number(args, s, |n| t += n)?;
            Value::num(t)
        }
        Product => {
            let mut t = 1.0;
            let mut any = false;
            for_each_number(args, s, |n| {
                t *= n;
                any = true;
            })?;
            Value::num(if any { t } else { 0.0 })
        }
        Average => {
            let (mut t, mut c) = (0.0, 0u64);
            for_each_number(args, s, |n| {
                t += n;
                c += 1;
            })?;
            if c == 0 { Value::Error(ErrorKind::Div0) } else { Value::num(t / c as f64) }
        }
        Min | Max => {
            let mut acc: Option<f64> = None;
            for_each_number(args, s, |n| {
                acc = Some(match acc {
                    None => n,
                    Some(a) if f == Min => a.min(n),
                    Some(a) => a.max(n),
                })
            })?;
            Value::num(acc.unwrap_or(0.0))
        }
        Count | CountA => {
            let mut c = 0u64;
            for a in args {
                if let Some(r) = a.range() {
                    for (_, cell) in s.iter_range(r) {
                        if f == CountA || cell.value.is_number() {
                            c += 1;
                        }
                    }
                } else {
                    let v = eval(a, s);
                    let counts = match (&v, f) {
                        (v, CountA) => !v.is_empty(),
                        (Value::Number(_) | Value::Bool(_), _) => true,
                        (Value::Text(t), _) => parse_number(t).is_some(),
                        _ => false,
                    };
                    if counts {
                        c += 1;
                    }
                }
            }
            Value::Number(c as f64)
        }
        CountBlank => {
            arg_count(args, 1, 1)?;
            let r = range_arg(args, 0)?;
            let filled = s.iter_range(r).filter(|(_, c)| !matches!(&c.value, Value::Empty) && !matches!(&c.value, Value::Text(t) if t.is_empty())).count();
            Value::Number((r.cell_count() - filled as u64) as f64)
        }
        SumIf | CountIf | AverageIf => {
            arg_count(args, 2, if f == CountIf { 2 } else { 3 })?;
            let r = range_arg(args, 0)?;
            let crit = Criterion::new(eval(&args[1], s));
            let sum_origin = match args.get(2) {
                Some(e) => e.range().ok_or(ErrorKind::Value)?.start,
                None => r.start,
            };
            let (mut total, mut count) = (0.0, 0u64);
            let mut visit = |c: CellRef, v: &Value| -> EvalResult<()> {
                if crit.matches(v) {
                    count += 1;
                    if f != CountIf {
                        let target = CellRef::new(sum_origin.row + (c.row - r.start.row), sum_origin.col + (c.col - r.start.col));
                        if let Value::Number(n) = s.value(target) {
                            total += n;
                        } else if let Value::Error(e) = s.value(target) {
                            return Err(*e);
                        }
                    }
                }
                Ok(())
            };
            if crit.matches(&Value::Empty) {
                // Empty cells can match (e.g. "<>x"), so visit every cell.
                for c in r.iter() {
                    visit(c, s.value(c))?;
                }
            } else {
                for (c, cell) in s.iter_range(r) {
                    visit(c, &cell.value)?;
                }
            }
            match f {
                CountIf => Value::Number(count as f64),
                SumIf => Value::num(total),
                _ if count == 0 => Value::Error(ErrorKind::Div0),
                _ => Value::num(total / count as f64),
            }
        }
        Abs | Sqrt | Int | Exp | Ln | Log10 | Sign => {
            arg_count(args, 1, 1)?;
            let n = num_arg(args, 0, s)?;
            match f {
                Abs => Value::num(n.abs()),
                Sqrt if n < 0.0 => Value::Error(ErrorKind::Num),
                Sqrt => Value::num(n.sqrt()),
                Int => Value::num(n.floor()),
                Exp => Value::num(n.exp()),
                Ln | Log10 if n <= 0.0 => Value::Error(ErrorKind::Num),
                Ln => Value::num(n.ln()),
                Log10 => Value::num(n.log10()),
                _ => Value::num(if n > 0.0 { 1.0 } else if n < 0.0 { -1.0 } else { 0.0 }),
            }
        }
        Log => {
            arg_count(args, 1, 2)?;
            let n = num_arg(args, 0, s)?;
            let base = opt_num(args, 1, s, 10.0)?;
            if n <= 0.0 || base <= 0.0 || base == 1.0 { Value::Error(ErrorKind::Num) } else { Value::num(n.ln() / base.ln()) }
        }
        Mod => {
            arg_count(args, 2, 2)?;
            let (n, d) = (num_arg(args, 0, s)?, num_arg(args, 1, s)?);
            if d == 0.0 { Value::Error(ErrorKind::Div0) } else { Value::num(n - d * (n / d).floor()) }
        }
        Power => {
            arg_count(args, 2, 2)?;
            binary(BinOp::Pow, eval(&args[0], s), eval(&args[1], s))
        }
        Round | RoundUp | RoundDown => {
            arg_count(args, 1, 2)?;
            let n = num_arg(args, 0, s)?;
            let d = opt_num(args, 1, s, 0.0)?;
            let mode = match f {
                Round => round_half_away,
                RoundUp => round_away,
                _ => f64::trunc,
            };
            round_to(n, d, mode)
        }
        Pi => Value::Number(std::f64::consts::PI),
        If => {
            arg_count(args, 2, 3)?;
            if eval(&args[0], s).to_bool()? {
                eval(&args[1], s)
            } else {
                args.get(2).map_or(Value::Bool(false), |e| eval(e, s))
            }
        }
        IfError | IfNa => {
            arg_count(args, 2, 2)?;
            match eval(&args[0], s) {
                Value::Error(e) if f == IfError || e == ErrorKind::NA => eval(&args[1], s),
                v => v,
            }
        }
        And | Or => {
            let mut acc: Option<bool> = None;
            for a in args {
                let mut take = |b: bool| acc = Some(if f == And { acc.unwrap_or(true) && b } else { acc.unwrap_or(false) || b });
                if let Some(r) = a.range() {
                    for (_, cell) in s.iter_range(r) {
                        match &cell.value {
                            Value::Bool(b) => take(*b),
                            Value::Number(n) => take(*n != 0.0),
                            Value::Error(e) => return Err(*e),
                            _ => {}
                        }
                    }
                } else {
                    take(eval(a, s).to_bool()?);
                }
            }
            Value::Bool(acc.ok_or(ErrorKind::Value)?)
        }
        Not => {
            arg_count(args, 1, 1)?;
            Value::Bool(!eval(&args[0], s).to_bool()?)
        }
        True => Value::Bool(true),
        False => Value::Bool(false),
        Len | Upper | Lower | Trim => {
            arg_count(args, 1, 1)?;
            let t = text_arg(args, 0, s)?;
            match f {
                Len => Value::Number(t.chars().count() as f64),
                Upper => Value::text(t.to_uppercase()),
                Lower => Value::text(t.to_lowercase()),
                _ => Value::text(t.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")),
            }
        }
        Left | Right => {
            arg_count(args, 1, 2)?;
            let t = text_arg(args, 0, s)?;
            let n = opt_num(args, 1, s, 1.0)?;
            if n < 0.0 {
                return Err(ErrorKind::Value);
            }
            let n = n as usize;
            let len = t.chars().count();
            Value::text(if f == Left {
                t.chars().take(n).collect::<String>()
            } else {
                t.chars().skip(len.saturating_sub(n)).collect::<String>()
            })
        }
        Mid => {
            arg_count(args, 3, 3)?;
            let t = text_arg(args, 0, s)?;
            let (start, n) = (num_arg(args, 1, s)?, num_arg(args, 2, s)?);
            if start < 1.0 || n < 0.0 {
                return Err(ErrorKind::Value);
            }
            Value::text(t.chars().skip(start as usize - 1).take(n as usize).collect::<String>())
        }
        Concat | Concatenate => {
            let mut out = String::new();
            for a in args {
                match a.range() {
                    Some(r) if f == Concat => {
                        for (_, cell) in s.iter_range(r) {
                            out.push_str(&cell.value.to_text()?);
                        }
                    }
                    _ => out.push_str(&eval(a, s).to_text()?),
                }
            }
            Value::text(out)
        }
        Rept => {
            arg_count(args, 2, 2)?;
            let t = text_arg(args, 0, s)?;
            let n = num_arg(args, 1, s)?;
            if n < 0.0 || t.len() as f64 * n > 32_767.0 {
                return Err(ErrorKind::Value);
            }
            Value::text(t.repeat(n as usize))
        }
        Exact => {
            arg_count(args, 2, 2)?;
            Value::Bool(text_arg(args, 0, s)? == text_arg(args, 1, s)?)
        }
        IsBlank | IsNumber | IsText | IsError | IsNa => {
            arg_count(args, 1, 1)?;
            let v = eval(&args[0], s);
            Value::Bool(match f {
                IsBlank => v.is_empty(),
                IsNumber => v.is_number(),
                IsText => matches!(v, Value::Text(_)),
                IsError => matches!(v, Value::Error(_)),
                _ => v == Value::Error(ErrorKind::NA),
            })
        }
        VLookup | HLookup => {
            arg_count(args, 3, 4)?;
            let target = eval(&args[0], s);
            if let Value::Error(e) = target {
                return Err(e);
            }
            let table = range_arg(args, 1)?;
            let idx = num_arg(args, 2, s)?.trunc() as i64;
            let approx = match args.get(3) {
                Some(e) => eval(e, s).to_bool()?,
                None => true,
            };
            let vertical = f == VLookup;
            let span = if vertical { table.cols() } else { table.rows() } as i64;
            if idx < 1 {
                return Err(ErrorKind::Value);
            }
            if idx > span {
                return Err(ErrorKind::Ref);
            }
            let key_range = if vertical {
                Range::new(table.start, CellRef::new(table.end.row, table.start.col))
            } else {
                Range::new(table.start, CellRef::new(table.start.row, table.end.col))
            };
            let keys = s.iter_range(key_range).map(|(c, cell)| (if vertical { c.row } else { c.col }, cell.value.clone()));
            let hit = if approx { lookup_approx(&target, keys, false) } else { lookup_exact(&target, keys) };
            let i = hit.ok_or(ErrorKind::NA)?;
            let cell = if vertical {
                CellRef::new(i, table.start.col + idx as u32 - 1)
            } else {
                CellRef::new(table.start.row + idx as u32 - 1, i)
            };
            s.value(cell).clone()
        }
        Match => {
            arg_count(args, 2, 3)?;
            let target = eval(&args[0], s);
            if let Value::Error(e) = target {
                return Err(e);
            }
            let r = range_arg(args, 1)?;
            if r.rows() > 1 && r.cols() > 1 {
                return Err(ErrorKind::NA);
            }
            let mt = opt_num(args, 2, s, 1.0)?;
            let vertical = r.cols() == 1;
            let keys = s.iter_range(r).map(|(c, cell)| (if vertical { c.row } else { c.col }, cell.value.clone()));
            let hit = if mt == 0.0 { lookup_exact(&target, keys) } else { lookup_approx(&target, keys, mt < 0.0) };
            let i = hit.ok_or(ErrorKind::NA)?;
            let base = if vertical { r.start.row } else { r.start.col };
            Value::Number((i - base + 1) as f64)
        }
        Index => {
            arg_count(args, 2, 3)?;
            let r = range_arg(args, 0)?;
            let a = num_arg(args, 1, s)?.trunc() as i64;
            let b = opt_num(args, 2, s, 0.0)?.trunc() as i64;
            // A single row/column array takes the one index as its position.
            let (row, col) = match (r.rows(), r.cols(), args.len()) {
                (1, _, 2) => (1, a),
                (_, 1, 2) => (a, 1),
                _ => (a, b),
            };
            if row < 1 || col < 1 || row > r.rows() as i64 || col > r.cols() as i64 {
                return Err(ErrorKind::Ref);
            }
            s.value(CellRef::new(r.start.row + row as u32 - 1, r.start.col + col as u32 - 1)).clone()
        }
        Now => Value::Number(excel_now()),
        Today => Value::Number(excel_now().floor()),
        Rand => Value::Number(random()),
        RandBetween => {
            arg_count(args, 2, 2)?;
            let (lo, hi) = (num_arg(args, 0, s)?.ceil(), num_arg(args, 1, s)?.floor());
            if lo > hi {
                return Err(ErrorKind::Num);
            }
            Value::Number(lo + (random() * (hi - lo + 1.0)).floor())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(wildcard_match("a*", "apple"));
        assert!(wildcard_match("*le", "apple"));
        assert!(wildcard_match("a?ple", "apple"));
        assert!(!wildcard_match("b*", "apple"));
        assert!(wildcard_match("*", ""));
    }

    #[test]
    fn all_functions_have_unique_names() {
        let mut names: Vec<_> = Func::ALL.iter().map(|f| f.name()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), Func::ALL.len());
        for f in Func::ALL {
            assert_eq!(Func::from_name(f.name()), Some(*f));
        }
    }
}
