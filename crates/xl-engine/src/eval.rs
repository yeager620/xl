//! Expression evaluation and the built-in function library.

use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::address::{CellRef, Range, SheetId};
use crate::datetime::{datetime_from_serial, days_in_month, serial_from_parts};
use crate::format::NumberFormat;
use crate::formula::{BinOp, Expr};
use crate::input::classify_literal;
use crate::store::{Cell, CellStore};
use crate::value::{ErrorKind, EvalResult, Value, parse_number};
use crate::workbook::{Names, Sheets};

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
    Sum => "SUM", Product => "PRODUCT", Average => "AVERAGE", Min => "MIN", Max => "MAX", Median => "MEDIAN",
    Count => "COUNT", CountA => "COUNTA", CountBlank => "COUNTBLANK",
    SumIf => "SUMIF", CountIf => "COUNTIF", AverageIf => "AVERAGEIF",
    SumIfs => "SUMIFS", CountIfs => "COUNTIFS", AverageIfs => "AVERAGEIFS", SumProduct => "SUMPRODUCT",
    Abs => "ABS", Sqrt => "SQRT", Int => "INT", Mod => "MOD", Power => "POWER",
    Round => "ROUND", RoundUp => "ROUNDUP", RoundDown => "ROUNDDOWN",
    Pi => "PI", Exp => "EXP", Ln => "LN", Log10 => "LOG10", Log => "LOG", Sign => "SIGN",
    If => "IF", Ifs => "IFS", IfError => "IFERROR", IfNa => "IFNA", And => "AND", Or => "OR", Not => "NOT",
    True => "TRUE", False => "FALSE",
    Len => "LEN", Upper => "UPPER", Lower => "LOWER", Trim => "TRIM",
    Left => "LEFT", Right => "RIGHT", Mid => "MID",
    Concat => "CONCAT", Concatenate => "CONCATENATE", Rept => "REPT", Exact => "EXACT",
    Text => "TEXT", ValueFn => "VALUE",
    IsBlank => "ISBLANK", IsNumber => "ISNUMBER", IsText => "ISTEXT", IsError => "ISERROR", IsNa => "ISNA",
    VLookup => "VLOOKUP", HLookup => "HLOOKUP", XLookup => "XLOOKUP", Match => "MATCH", Index => "INDEX",
    Date => "DATE", Time => "TIME", Year => "YEAR", Month => "MONTH", Day => "DAY",
    Hour => "HOUR", Minute => "MINUTE", Second => "SECOND", Weekday => "WEEKDAY",
    EDate => "EDATE", EOMonth => "EOMONTH",
    Now => "NOW", Today => "TODAY", Rand => "RAND", RandBetween => "RANDBETWEEN",
    Offset => "OFFSET", Indirect => "INDIRECT", Row => "ROW", Column => "COLUMN", Rows => "ROWS", Columns => "COLUMNS",
}

impl Func {
    pub fn is_volatile(self) -> bool {
        // OFFSET and INDIRECT compute their references at run time, so their
        // dependencies can't be known statically: recalc them every time (as Excel does).
        matches!(self, Func::Now | Func::Today | Func::Rand | Func::RandBetween | Func::Offset | Func::Indirect)
    }

    /// Short signature shown in the UI.
    pub fn signature(self) -> &'static str {
        use Func::*;
        match self {
            Sum | Product | Average | Min | Max | Median | Count | CountA => "(value1, [value2], …)",
            CountBlank => "(range)",
            SumIf | AverageIf => "(range, criteria, [sum_range])",
            CountIf => "(range, criteria)",
            SumIfs | AverageIfs => "(sum_range, criteria_range1, criteria1, …)",
            CountIfs => "(criteria_range1, criteria1, …)",
            SumProduct => "(array1, [array2], …)",
            Abs | Sqrt | Int | Exp | Ln | Log10 | Sign => "(number)",
            Log => "(number, [base])",
            Mod => "(number, divisor)",
            Power => "(number, power)",
            Round | RoundUp | RoundDown => "(number, num_digits)",
            Pi | True | False | Now | Today | Rand => "()",
            RandBetween => "(bottom, top)",
            If => "(test, value_if_true, [value_if_false])",
            Ifs => "(test1, value1, [test2, value2], …)",
            IfError | IfNa => "(value, value_if_error)",
            And | Or => "(logical1, [logical2], …)",
            Not => "(logical)",
            Len | Upper | Lower | Trim | ValueFn => "(text)",
            Left | Right => "(text, [num_chars])",
            Mid => "(text, start_num, num_chars)",
            Concat | Concatenate => "(text1, [text2], …)",
            Rept => "(text, number_times)",
            Exact => "(text1, text2)",
            Text => "(value, format_text)",
            IsBlank | IsNumber | IsText | IsError | IsNa => "(value)",
            VLookup => "(lookup_value, table_array, col_index_num, [range_lookup])",
            HLookup => "(lookup_value, table_array, row_index_num, [range_lookup])",
            XLookup => "(lookup_value, lookup_array, return_array, [if_not_found], [match_mode], [search_mode])",
            Match => "(lookup_value, lookup_array, [match_type])",
            Index => "(array, row_num, [column_num])",
            Date => "(year, month, day)",
            Time => "(hour, minute, second)",
            Year | Month | Day | Hour | Minute | Second => "(serial_number)",
            Weekday => "(serial_number, [return_type])",
            EDate | EOMonth => "(start_date, months)",
            Offset => "(reference, rows, cols, [height], [width])",
            Indirect => "(ref_text, [a1])",
            Row | Column => "([reference])",
            Rows | Columns => "(array)",
        }
    }
}

/// A resolved range on a specific sheet.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct RangeRef {
    pub sheet: SheetId,
    pub range: Range,
}

/// Evaluation context: the workbook's sheets and names, and the cell holding
/// the formula (for ROW(), COLUMN() and implicit intersection).
pub struct Ctx<'a> {
    pub sheets: &'a Sheets,
    pub names: &'a Names,
    pub sheet: SheetId,
    pub cell: CellRef,
    /// Guards against names defined in terms of themselves.
    depth: std::cell::Cell<u8>,
}

const MAX_NAME_DEPTH: u8 = 16;

static EMPTY: Value = Value::Empty;

impl<'a> Ctx<'a> {
    pub fn new(sheets: &'a Sheets, names: &'a Names, sheet: SheetId, cell: CellRef) -> Self {
        Self { sheets, names, sheet, cell, depth: std::cell::Cell::new(0) }
    }

    fn store(&self, sheet: SheetId) -> Option<&CellStore> {
        self.sheets.store(sheet)
    }

    /// Run `f` one level deeper into name definitions (None past the limit).
    fn nested<T>(&self, f: impl FnOnce(&Self) -> T) -> Option<T> {
        let d = self.depth.get();
        if d >= MAX_NAME_DEPTH {
            return None;
        }
        self.depth.set(d + 1);
        let out = f(self);
        self.depth.set(d);
        Some(out)
    }

    /// Resolve an expression that denotes a reference: a cell or range, a
    /// defined name for one, or OFFSET / INDIRECT / INDEX.
    /// None = not a reference; Err = a reference that's invalid (#REF! etc.).
    fn resolve(&self, e: &Expr) -> Option<EvalResult<RangeRef>> {
        match e {
            Expr::Ref(_) | Expr::Range(..) => {
                let (sheet, range) = e.reference()?;
                let sheet = sheet.unwrap_or(self.sheet);
                Some(if self.store(sheet).is_some() { Ok(RangeRef { sheet, range }) } else { Err(ErrorKind::Ref) })
            }
            Expr::Paren(x) => self.resolve(x),
            Expr::Name(n) => {
                let def = self.names.lookup(n, self.sheet)?;
                self.nested(|cx| cx.resolve(&def.expr)).unwrap_or(Some(Err(ErrorKind::Name)))
            }
            Expr::Call(Func::Offset, args) => Some(self.offset(args)),
            Expr::Call(Func::Indirect, args) => Some(self.indirect(args)),
            Expr::Call(Func::Index, args) => Some(self.index_ref(args)),
            _ => None,
        }
    }

    /// A range used where one value is expected: a single cell gives its
    /// value; otherwise implicit intersection with the formula's row/column.
    fn scalar_of(&self, rr: RangeRef) -> Value {
        let r = rr.range;
        if r.start == r.end {
            return self.value(rr.sheet, r.start).clone();
        }
        if r.cols() == 1 && (r.start.row..=r.end.row).contains(&self.cell.row) {
            return self.value(rr.sheet, CellRef::new(self.cell.row, r.start.col)).clone();
        }
        if r.rows() == 1 && (r.start.col..=r.end.col).contains(&self.cell.col) {
            return self.value(rr.sheet, CellRef::new(r.start.row, self.cell.col)).clone();
        }
        Value::Error(ErrorKind::Value)
    }

    /// OFFSET(reference, rows, cols, [height], [width])
    fn offset(&self, args: &[Expr]) -> EvalResult<RangeRef> {
        arg_count(args, 3, 5)?;
        let base = self.resolve(&args[0]).unwrap_or(Err(ErrorKind::Value))?;
        let dr = num_arg(args, 1, self)?.trunc() as i64;
        let dc = num_arg(args, 2, self)?.trunc() as i64;
        let h = opt_num(args, 3, self, base.range.rows() as f64)?.trunc() as i64;
        let w = opt_num(args, 4, self, base.range.cols() as f64)?.trunc() as i64;
        if h < 1 || w < 1 {
            return Err(ErrorKind::Ref);
        }
        let start = base.range.start.offset(dr, dc).ok_or(ErrorKind::Ref)?;
        let end = start.offset(h - 1, w - 1).ok_or(ErrorKind::Ref)?;
        Ok(RangeRef { sheet: base.sheet, range: Range::new(start, end) })
    }

    /// INDIRECT(ref_text, [a1]): A1 text (with optional sheet) or R1C1 text.
    fn indirect(&self, args: &[Expr]) -> EvalResult<RangeRef> {
        arg_count(args, 1, 2)?;
        let text = text_arg(args, 0, self)?;
        let a1 = match args.get(1) {
            Some(e) => eval(e, self).to_bool()?,
            None => true,
        };
        let expr = if a1 {
            crate::formula::parse_formula(text.trim(), self.sheets).map_err(|_| ErrorKind::Ref)?
        } else {
            r1c1_to_expr(text.trim(), self.sheets).ok_or(ErrorKind::Ref)?
        };
        self.nested(|cx| cx.resolve(&expr)).flatten().unwrap_or(Err(ErrorKind::Ref))
    }

    /// INDEX(array, row_num, [column_num]) as a reference: 0 selects a whole
    /// row or column of the array.
    fn index_ref(&self, args: &[Expr]) -> EvalResult<RangeRef> {
        arg_count(args, 2, 3)?;
        let rr = self.resolve(&args[0]).unwrap_or(Err(ErrorKind::Value))?;
        let r = rr.range;
        let a = num_arg(args, 1, self)?.trunc() as i64;
        let b = opt_num(args, 2, self, 0.0)?.trunc() as i64;
        // A single row/column array takes the one index as its position.
        let (row, col) = match (r.rows(), r.cols(), args.len()) {
            (1, _, 2) => (1, a),
            (_, 1, 2) => (a, 1),
            _ => (a, b),
        };
        if row < 0 || col < 0 || row > r.rows() as i64 || col > r.cols() as i64 {
            return Err(ErrorKind::Ref);
        }
        let (r0, r1) = if row == 0 { (r.start.row, r.end.row) } else { let x = r.start.row + row as u32 - 1; (x, x) };
        let (c0, c1) = if col == 0 { (r.start.col, r.end.col) } else { let x = r.start.col + col as u32 - 1; (x, x) };
        Ok(RangeRef { sheet: rr.sheet, range: Range::new(CellRef::new(r0, c0), CellRef::new(r1, c1)) })
    }

    fn value(&self, sheet: SheetId, c: CellRef) -> &Value {
        self.store(sheet).map_or(&EMPTY, |s| s.value(c))
    }

    fn iter(&self, rr: RangeRef) -> impl Iterator<Item = (CellRef, &Cell)> + '_ {
        self.store(rr.sheet).into_iter().flat_map(move |s| s.iter_range(rr.range))
    }

    /// Clamp a range to the used area so full-column scans stay cheap.
    fn clamp_used(&self, rr: RangeRef) -> RangeRef {
        let Some(ext) = self.store(rr.sheet).and_then(|s| s.extent()) else {
            return RangeRef { range: Range::single(rr.range.start), ..rr };
        };
        let end = CellRef::new(rr.range.end.row.min(ext.row.max(rr.range.start.row)), rr.range.end.col.min(ext.col.max(rr.range.start.col)));
        RangeRef { range: Range::new(rr.range.start, end), ..rr }
    }
}

/// Parse absolute R1C1 text (`R2C3`, `R2C3:R4C5`, optional `Sheet!` prefix).
fn r1c1_to_expr(text: &str, sheets: &Sheets) -> Option<Expr> {
    use crate::formula::{RangeKind, RefAddr};
    let (sheet, body) = match text.rsplit_once('!') {
        Some((s, b)) => {
            let name = s.trim_matches('\'');
            (Some(crate::formula::SheetNames::sheet_id(sheets, name)?), b)
        }
        None => (None, text),
    };
    let one = |t: &str| -> Option<RefAddr> {
        let t = t.to_ascii_uppercase();
        let rest = t.strip_prefix('R')?;
        let (r, c) = rest.split_once('C')?;
        let (r, c): (u32, u32) = (r.parse().ok()?, c.parse().ok()?);
        (r >= 1 && c >= 1).then(|| RefAddr { sheet, cell: CellRef::new(r - 1, c - 1), col_abs: true, row_abs: true })
    };
    Some(match body.split_once(':') {
        Some((a, b)) => Expr::Range(one(a)?, one(b)?, RangeKind::Cells),
        None => Expr::Ref(one(body)?),
    })
}

/// Evaluate an expression in scalar context.
pub fn eval(e: &Expr, cx: &Ctx) -> Value {
    match e {
        Expr::Number(n) => Value::Number(*n),
        Expr::Text(t) => Value::Text(t.clone()),
        Expr::Bool(b) => Value::Bool(*b),
        Expr::Error(err) => Value::Error(*err),
        Expr::Ref(_) | Expr::Range(..) | Expr::Call(Func::Offset | Func::Indirect | Func::Index, _) => match cx.resolve(e) {
            Some(Ok(rr)) => cx.scalar_of(rr),
            Some(Err(err)) => Value::Error(err),
            None => Value::Error(ErrorKind::Value),
        },
        Expr::Name(n) => match cx.resolve(e) {
            Some(Ok(rr)) => cx.scalar_of(rr),
            Some(Err(err)) => Value::Error(err),
            // A name for a constant or formula rather than a range.
            None => match cx.names.lookup(n, cx.sheet) {
                Some(def) => cx.nested(|cx| eval(&def.expr, cx)).unwrap_or(Value::Error(ErrorKind::Name)),
                None => Value::Error(ErrorKind::Name),
            },
        },
        Expr::Paren(x) | Expr::Plus(x) => eval(x, cx),
        Expr::Neg(x) => match eval(x, cx).to_number() {
            Ok(n) => Value::num(-n),
            Err(err) => Value::Error(err),
        },
        Expr::Percent(x) => match eval(x, cx).to_number() {
            Ok(n) => Value::num(n / 100.0),
            Err(err) => Value::Error(err),
        },
        Expr::Binary(op, a, b) => binary(*op, eval(a, cx), eval(b, cx)),
        Expr::Call(f, args) => call(*f, args, cx).unwrap_or_else(Value::Error),
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
        BinOp::Pow => arith(|x, y| if x == 0.0 && y == 0.0 { Value::Error(ErrorKind::Num) } else { Value::num(x.powf(y)) }),
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

fn num_arg(args: &[Expr], i: usize, cx: &Ctx) -> EvalResult<f64> {
    eval(&args[i], cx).to_number()
}

fn opt_num(args: &[Expr], i: usize, cx: &Ctx, default: f64) -> EvalResult<f64> {
    match args.get(i) {
        Some(e) => eval(e, cx).to_number(),
        None => Ok(default),
    }
}

fn text_arg(args: &[Expr], i: usize, cx: &Ctx) -> EvalResult<Arc<str>> {
    eval(&args[i], cx).to_text()
}

fn range_arg(args: &[Expr], i: usize, cx: &Ctx) -> EvalResult<RangeRef> {
    args.get(i).and_then(|e| cx.resolve(e)).unwrap_or(Err(ErrorKind::Value))
}

/// Visit numbers the way SUM does: references contribute only numeric cells
/// (errors propagate), direct arguments are coerced.
fn for_each_number(args: &[Expr], cx: &Ctx, mut f: impl FnMut(f64)) -> EvalResult<()> {
    for a in args {
        if let Some(rr) = cx.resolve(a) {
            for (_, cell) in cx.iter(rr?) {
                match &cell.value {
                    Value::Number(n) => f(*n),
                    Value::Error(e) => return Err(*e),
                    _ => {}
                }
            }
        } else {
            match eval(a, cx) {
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

/// Current local-agnostic time as an Excel serial (UTC).
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
        let same_kind = std::mem::discriminant(v) == std::mem::discriminant(&self.value)
            || (self.value.is_empty() && matches!(v, Value::Text(t) if t.is_empty()));
        if !same_kind {
            return self.op == BinOp::Ne;
        }
        matches!(binary(self.op, v.clone(), self.value.clone()), Value::Bool(true))
    }
}

pub fn wildcard_match(pat: &str, text: &str) -> bool {
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

fn lookup_exact(target: &Value, cells: impl Iterator<Item = (u32, Value)>, wildcards: bool) -> Option<u32> {
    let wild = match target {
        Value::Text(t) if wildcards && (t.contains('*') || t.contains('?')) => Some(t.to_lowercase()),
        _ => None,
    };
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

/// Largest value <= target (or smallest >= target when `descending`), assuming sorted data.
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

/// Shared implementation for SUMIFS / COUNTIFS / AVERAGEIFS.
fn multi_criteria(cx: &Ctx, sum_range: Option<RangeRef>, pairs: &[(RangeRef, Criterion)]) -> EvalResult<(f64, u64)> {
    let (first, first_crit) = &pairs[0];
    let (rows, cols) = (first.range.rows(), first.range.cols());
    for (r, _) in pairs {
        if r.range.rows() != rows || r.range.cols() != cols {
            return Err(ErrorKind::Value);
        }
    }
    let (mut total, mut count) = (0.0, 0u64);
    let mut visit = |off_r: u32, off_c: u32| -> EvalResult<()> {
        for (r, crit) in pairs {
            let c = CellRef::new(r.range.start.row + off_r, r.range.start.col + off_c);
            if !crit.matches(cx.value(r.sheet, c)) {
                return Ok(());
            }
        }
        count += 1;
        if let Some(sr) = sum_range {
            let c = CellRef::new(sr.range.start.row + off_r, sr.range.start.col + off_c);
            match cx.value(sr.sheet, c) {
                Value::Number(n) => total += n,
                Value::Error(e) => return Err(*e),
                _ => {}
            }
        }
        Ok(())
    };
    if first_crit.matches(&Value::Empty) {
        let used = cx.clamp_used(*first);
        for c in used.range.iter() {
            visit(c.row - first.range.start.row, c.col - first.range.start.col)?;
        }
        // Cells past the used area are all blank on this sheet: if every
        // criterion accepts blanks they all match (and add nothing to a sum
        // range on the same sheet).
        let beyond = first.range.cell_count() - used.range.cell_count();
        let all_blank_ok = pairs.iter().all(|(r, c)| r.sheet == first.sheet && c.matches(&Value::Empty));
        if beyond > 0 && all_blank_ok {
            count += beyond;
        }
    } else {
        let cells: Vec<CellRef> = cx.iter(*first).map(|(c, _)| c).collect();
        for c in cells {
            visit(c.row - first.range.start.row, c.col - first.range.start.col)?;
        }
    }
    Ok((total, count))
}

fn date_parts(args: &[Expr], cx: &Ctx) -> EvalResult<crate::datetime::DateTime> {
    arg_count(args, 1, 2)?;
    let n = num_arg(args, 0, cx)?;
    datetime_from_serial(n).ok_or(ErrorKind::Num)
}

fn call(f: Func, args: &[Expr], cx: &Ctx) -> EvalResult<Value> {
    use Func::*;
    Ok(match f {
        Sum => {
            let mut t = 0.0;
            for_each_number(args, cx, |n| t += n)?;
            Value::num(t)
        }
        Product => {
            let mut t = 1.0;
            let mut any = false;
            for_each_number(args, cx, |n| {
                t *= n;
                any = true;
            })?;
            Value::num(if any { t } else { 0.0 })
        }
        Average => {
            let (mut t, mut c) = (0.0, 0u64);
            for_each_number(args, cx, |n| {
                t += n;
                c += 1;
            })?;
            if c == 0 { Value::Error(ErrorKind::Div0) } else { Value::num(t / c as f64) }
        }
        Median => {
            let mut v = Vec::new();
            for_each_number(args, cx, |n| v.push(n))?;
            if v.is_empty() {
                return Err(ErrorKind::Num);
            }
            v.sort_by(f64::total_cmp);
            let m = v.len() / 2;
            Value::num(if v.len() % 2 == 1 { v[m] } else { (v[m - 1] + v[m]) / 2.0 })
        }
        Min | Max => {
            let mut acc: Option<f64> = None;
            for_each_number(args, cx, |n| {
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
                if let Some(rr) = cx.resolve(a) {
                    for (_, cell) in cx.iter(rr?) {
                        if f == CountA || cell.value.is_number() {
                            c += 1;
                        }
                    }
                } else {
                    let v = eval(a, cx);
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
            let rr = range_arg(args, 0, cx)?;
            let filled = cx.iter(rr).filter(|(_, c)| !matches!(&c.value, Value::Empty) && !matches!(&c.value, Value::Text(t) if t.is_empty())).count();
            Value::Number((rr.range.cell_count() - filled as u64) as f64)
        }
        SumIf | CountIf | AverageIf => {
            arg_count(args, 2, if f == CountIf { 2 } else { 3 })?;
            let rr = range_arg(args, 0, cx)?;
            let crit = Criterion::new(eval(&args[1], cx));
            let sum = match args.get(2) {
                Some(_) => {
                    let s = range_arg(args, 2, cx)?;
                    RangeRef { sheet: s.sheet, range: Range::new(s.range.start, CellRef::new(s.range.start.row + rr.range.rows() - 1, s.range.start.col + rr.range.cols() - 1)) }
                }
                None => rr,
            };
            let (total, count) = multi_criteria(cx, (f != CountIf).then_some(sum), &[(rr, crit)])?;
            match f {
                CountIf => Value::Number(count as f64),
                SumIf => Value::num(total),
                _ if count == 0 => Value::Error(ErrorKind::Div0),
                _ => Value::num(total / count as f64),
            }
        }
        SumIfs | CountIfs | AverageIfs => {
            let (sum, rest) = if f == CountIfs { (None, args) } else { (Some(range_arg(args, 0, cx)?), args.get(1..).unwrap_or(&[])) };
            if rest.is_empty() || rest.len() % 2 != 0 {
                return Err(ErrorKind::Value);
            }
            let mut pairs = Vec::new();
            for pair in rest.chunks(2) {
                let r = cx.resolve(&pair[0]).unwrap_or(Err(ErrorKind::Value))?;
                pairs.push((r, Criterion::new(eval(&pair[1], cx))));
            }
            if let Some(s) = sum
                && (s.range.rows() != pairs[0].0.range.rows() || s.range.cols() != pairs[0].0.range.cols())
            {
                return Err(ErrorKind::Value);
            }
            let (total, count) = multi_criteria(cx, sum, &pairs)?;
            match f {
                CountIfs => Value::Number(count as f64),
                SumIfs => Value::num(total),
                _ if count == 0 => Value::Error(ErrorKind::Div0),
                _ => Value::num(total / count as f64),
            }
        }
        SumProduct => {
            if args.is_empty() {
                return Err(ErrorKind::Value);
            }
            let ranges: Vec<RangeRef> = args.iter().map(|a| cx.resolve(a).unwrap_or(Err(ErrorKind::Value))).collect::<EvalResult<_>>()?;
            let (rows, cols) = (ranges[0].range.rows(), ranges[0].range.cols());
            if ranges.iter().any(|r| r.range.rows() != rows || r.range.cols() != cols) {
                return Err(ErrorKind::Value);
            }
            let used = cx.clamp_used(ranges[0]);
            let mut total = 0.0;
            for c in used.range.iter() {
                let (dr, dc) = (c.row - ranges[0].range.start.row, c.col - ranges[0].range.start.col);
                let mut p = 1.0;
                for r in &ranges {
                    match cx.value(r.sheet, CellRef::new(r.range.start.row + dr, r.range.start.col + dc)) {
                        Value::Number(n) => p *= n,
                        Value::Error(e) => return Err(*e),
                        _ => p = 0.0,
                    }
                }
                total += p;
            }
            Value::num(total)
        }
        Abs | Sqrt | Int | Exp | Ln | Log10 | Sign => {
            arg_count(args, 1, 1)?;
            let n = num_arg(args, 0, cx)?;
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
            let n = num_arg(args, 0, cx)?;
            let base = opt_num(args, 1, cx, 10.0)?;
            if n <= 0.0 || base <= 0.0 || base == 1.0 { Value::Error(ErrorKind::Num) } else { Value::num(n.ln() / base.ln()) }
        }
        Mod => {
            arg_count(args, 2, 2)?;
            let (n, d) = (num_arg(args, 0, cx)?, num_arg(args, 1, cx)?);
            if d == 0.0 { Value::Error(ErrorKind::Div0) } else { Value::num(n - d * (n / d).floor()) }
        }
        Power => {
            arg_count(args, 2, 2)?;
            binary(BinOp::Pow, eval(&args[0], cx), eval(&args[1], cx))
        }
        Round | RoundUp | RoundDown => {
            arg_count(args, 1, 2)?;
            let n = num_arg(args, 0, cx)?;
            let d = opt_num(args, 1, cx, 0.0)?;
            let mode = match f {
                Round => f64::round,
                RoundUp => round_away,
                _ => f64::trunc,
            };
            round_to(n, d, mode)
        }
        Pi => Value::Number(std::f64::consts::PI),
        If => {
            arg_count(args, 2, 3)?;
            if eval(&args[0], cx).to_bool()? {
                eval(&args[1], cx)
            } else {
                args.get(2).map_or(Value::Bool(false), |e| eval(e, cx))
            }
        }
        Ifs => {
            if args.is_empty() || !args.len().is_multiple_of(2) {
                return Err(ErrorKind::Value);
            }
            for pair in args.chunks(2) {
                if eval(&pair[0], cx).to_bool()? {
                    return Ok(eval(&pair[1], cx));
                }
            }
            Value::Error(ErrorKind::NA)
        }
        IfError | IfNa => {
            arg_count(args, 2, 2)?;
            match eval(&args[0], cx) {
                Value::Error(e) if f == IfError || e == ErrorKind::NA => eval(&args[1], cx),
                v => v,
            }
        }
        And | Or => {
            let mut acc: Option<bool> = None;
            for a in args {
                let mut take = |b: bool| acc = Some(if f == And { acc.unwrap_or(true) && b } else { acc.unwrap_or(false) || b });
                if let Some(rr) = cx.resolve(a) {
                    for (_, cell) in cx.iter(rr?) {
                        match &cell.value {
                            Value::Bool(b) => take(*b),
                            Value::Number(n) => take(*n != 0.0),
                            Value::Error(e) => return Err(*e),
                            _ => {}
                        }
                    }
                } else {
                    take(eval(a, cx).to_bool()?);
                }
            }
            Value::Bool(acc.ok_or(ErrorKind::Value)?)
        }
        Not => {
            arg_count(args, 1, 1)?;
            Value::Bool(!eval(&args[0], cx).to_bool()?)
        }
        True => Value::Bool(true),
        False => Value::Bool(false),
        Len | Upper | Lower | Trim => {
            arg_count(args, 1, 1)?;
            let t = text_arg(args, 0, cx)?;
            match f {
                Len => Value::Number(t.chars().count() as f64),
                Upper => Value::text(t.to_uppercase()),
                Lower => Value::text(t.to_lowercase()),
                _ => Value::text(t.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")),
            }
        }
        Left | Right => {
            arg_count(args, 1, 2)?;
            let t = text_arg(args, 0, cx)?;
            let n = opt_num(args, 1, cx, 1.0)?;
            if n < 0.0 {
                return Err(ErrorKind::Value);
            }
            let n = n as usize;
            let len = t.chars().count();
            Value::text(if f == Left { t.chars().take(n).collect::<String>() } else { t.chars().skip(len.saturating_sub(n)).collect::<String>() })
        }
        Mid => {
            arg_count(args, 3, 3)?;
            let t = text_arg(args, 0, cx)?;
            let (start, n) = (num_arg(args, 1, cx)?, num_arg(args, 2, cx)?);
            if start < 1.0 || n < 0.0 {
                return Err(ErrorKind::Value);
            }
            Value::text(t.chars().skip(start as usize - 1).take(n as usize).collect::<String>())
        }
        Concat | Concatenate => {
            let mut out = String::new();
            for a in args {
                match cx.resolve(a) {
                    Some(rr) if f == Concat => {
                        for (_, cell) in cx.iter(rr?) {
                            out.push_str(&cell.value.to_text()?);
                        }
                    }
                    _ => out.push_str(&eval(a, cx).to_text()?),
                }
            }
            Value::text(out)
        }
        Rept => {
            arg_count(args, 2, 2)?;
            let t = text_arg(args, 0, cx)?;
            let n = num_arg(args, 1, cx)?;
            if n < 0.0 || t.len() as f64 * n > 32_767.0 {
                return Err(ErrorKind::Value);
            }
            Value::text(t.repeat(n as usize))
        }
        Exact => {
            arg_count(args, 2, 2)?;
            Value::Bool(text_arg(args, 0, cx)? == text_arg(args, 1, cx)?)
        }
        Text => {
            arg_count(args, 2, 2)?;
            let v = eval(&args[0], cx);
            if let Value::Error(e) = v {
                return Err(e);
            }
            let code = text_arg(args, 1, cx)?;
            // Text that looks like a number formats as that number.
            let v = match &v {
                Value::Text(t) => classify_literal(t).map(|l| l.value).unwrap_or(v.clone()),
                _ => v,
            };
            Value::text(NumberFormat::parse(&code).format(&v).text)
        }
        ValueFn => {
            arg_count(args, 1, 1)?;
            match eval(&args[0], cx) {
                Value::Number(n) => Value::Number(n),
                Value::Empty => Value::Number(0.0),
                Value::Error(e) => return Err(e),
                v => match classify_literal(&v.to_text()?).map(|l| l.value) {
                    Some(Value::Number(n)) => Value::Number(n),
                    _ => return Err(ErrorKind::Value),
                },
            }
        }
        IsBlank | IsNumber | IsText | IsError | IsNa => {
            arg_count(args, 1, 1)?;
            let v = eval(&args[0], cx);
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
            let target = eval(&args[0], cx);
            if let Value::Error(e) = target {
                return Err(e);
            }
            let table = range_arg(args, 1, cx)?;
            let idx = num_arg(args, 2, cx)?.trunc() as i64;
            let approx = match args.get(3) {
                Some(e) => eval(e, cx).to_bool()?,
                None => true,
            };
            let vertical = f == VLookup;
            let t = table.range;
            let span = if vertical { t.cols() } else { t.rows() } as i64;
            if idx < 1 {
                return Err(ErrorKind::Value);
            }
            if idx > span {
                return Err(ErrorKind::Ref);
            }
            let key_range = if vertical { Range::new(t.start, CellRef::new(t.end.row, t.start.col)) } else { Range::new(t.start, CellRef::new(t.start.row, t.end.col)) };
            let keys = cx.iter(RangeRef { range: key_range, ..table }).map(|(c, cell)| (if vertical { c.row } else { c.col }, cell.value.clone()));
            let hit = if approx { lookup_approx(&target, keys, false) } else { lookup_exact(&target, keys, true) };
            let i = hit.ok_or(ErrorKind::NA)?;
            let cell = if vertical { CellRef::new(i, t.start.col + idx as u32 - 1) } else { CellRef::new(t.start.row + idx as u32 - 1, i) };
            cx.value(table.sheet, cell).clone()
        }
        XLookup => {
            arg_count(args, 3, 6)?;
            let target = eval(&args[0], cx);
            if let Value::Error(e) = target {
                return Err(e);
            }
            let look = range_arg(args, 1, cx)?;
            let ret = range_arg(args, 2, cx)?;
            let mode = opt_num(args, 4, cx, 0.0)? as i64;
            let reverse = opt_num(args, 5, cx, 1.0)? < 0.0;
            let vertical = look.range.cols() == 1;
            if !vertical && look.range.rows() != 1 {
                return Err(ErrorKind::Value);
            }
            let used = cx.clamp_used(look);
            let mut keys: Vec<(u32, Value)> = used
                .range
                .iter()
                .map(|c| (if vertical { c.row - look.range.start.row } else { c.col - look.range.start.col }, cx.value(look.sheet, c).clone()))
                .collect();
            if reverse {
                keys.reverse();
            }
            let hit = match mode {
                0 => lookup_exact(&target, keys.into_iter(), false),
                2 => lookup_exact(&target, keys.into_iter(), true),
                -1 | 1 => {
                    // Exact match, else next smaller (-1) / next larger (1); data need not be sorted.
                    let mut best: Option<(u32, Value)> = None;
                    let mut exact = None;
                    for (i, v) in keys {
                        if v.is_empty() || std::mem::discriminant(&v) != std::mem::discriminant(&target) {
                            continue;
                        }
                        match compare(&v, &target) {
                            Ordering::Equal => {
                                exact = Some(i);
                                break;
                            }
                            Ordering::Less if mode == -1 => {
                                if best.as_ref().is_none_or(|(_, b)| compare(&v, b) == Ordering::Greater) {
                                    best = Some((i, v));
                                }
                            }
                            Ordering::Greater if mode == 1
                                && best.as_ref().is_none_or(|(_, b)| compare(&v, b) == Ordering::Less) => {
                                    best = Some((i, v));
                                }
                            _ => {}
                        }
                    }
                    exact.or(best.map(|(i, _)| i))
                }
                _ => return Err(ErrorKind::Value),
            };
            match hit {
                Some(off) => {
                    let c = if vertical { CellRef::new(ret.range.start.row + off, ret.range.start.col) } else { CellRef::new(ret.range.start.row, ret.range.start.col + off) };
                    cx.value(ret.sheet, c).clone()
                }
                None => match args.get(3) {
                    Some(e) => eval(e, cx),
                    None => Value::Error(ErrorKind::NA),
                },
            }
        }
        Match => {
            arg_count(args, 2, 3)?;
            let target = eval(&args[0], cx);
            if let Value::Error(e) = target {
                return Err(e);
            }
            let rr = range_arg(args, 1, cx)?;
            let r = rr.range;
            if r.rows() > 1 && r.cols() > 1 {
                return Err(ErrorKind::NA);
            }
            let mt = opt_num(args, 2, cx, 1.0)?;
            let vertical = r.cols() == 1;
            let keys = cx.iter(rr).map(|(c, cell)| (if vertical { c.row } else { c.col }, cell.value.clone()));
            let hit = if mt == 0.0 { lookup_exact(&target, keys, true) } else { lookup_approx(&target, keys, mt < 0.0) };
            let i = hit.ok_or(ErrorKind::NA)?;
            let base = if vertical { r.start.row } else { r.start.col };
            Value::Number((i - base + 1) as f64)
        }
        Index => cx.scalar_of(cx.index_ref(args)?),
        Offset | Indirect => {
            // Reached only via call(); eval() resolves these as references.
            let rr = if f == Offset { cx.offset(args)? } else { cx.indirect(args)? };
            cx.scalar_of(rr)
        }
        Row | Column => {
            arg_count(args, 0, 1)?;
            let c = match args.first() {
                Some(a) => cx.resolve(a).unwrap_or(Err(ErrorKind::Value))?.range.start,
                None => cx.cell,
            };
            Value::Number(if f == Row { c.row } else { c.col } as f64 + 1.0)
        }
        Rows | Columns => {
            arg_count(args, 1, 1)?;
            let r = range_arg(args, 0, cx)?.range;
            Value::Number(if f == Rows { r.rows() } else { r.cols() } as f64)
        }
        Date => {
            arg_count(args, 3, 3)?;
            let (y, m, d) = (num_arg(args, 0, cx)?.trunc() as i64, num_arg(args, 1, cx)?.trunc() as i64, num_arg(args, 2, cx)?.trunc() as i64);
            let s = serial_from_parts(y, m, d);
            if !(0.0..2_958_466.0).contains(&s) {
                return Err(ErrorKind::Num);
            }
            Value::Number(s)
        }
        Time => {
            arg_count(args, 3, 3)?;
            let (h, m, s) = (num_arg(args, 0, cx)?.trunc(), num_arg(args, 1, cx)?.trunc(), num_arg(args, 2, cx)?.trunc());
            let t = (h * 3600.0 + m * 60.0 + s) / 86_400.0;
            if t < 0.0 {
                return Err(ErrorKind::Num);
            }
            Value::num(t.fract())
        }
        Year | Month | Day | Hour | Minute | Second => {
            let dt = date_parts(args, cx)?;
            Value::Number(match f {
                Year => dt.year as f64,
                Month => dt.month as f64,
                Day => dt.day as f64,
                Hour => dt.hour as f64,
                Minute => dt.minute as f64,
                _ => dt.second as f64,
            })
        }
        Weekday => {
            let dt = date_parts(args, cx)?;
            let kind = opt_num(args, 1, cx, 1.0)? as i64;
            let w = dt.weekday as i64; // 0 = Sunday
            Value::Number(match kind {
                1 => w + 1,
                2 => (w + 6) % 7 + 1,
                3 => (w + 6) % 7,
                _ => return Err(ErrorKind::Num),
            } as f64)
        }
        EDate | EOMonth => {
            arg_count(args, 2, 2)?;
            let dt = datetime_from_serial(num_arg(args, 0, cx)?).ok_or(ErrorKind::Num)?;
            let months = num_arg(args, 1, cx)?.trunc() as i64;
            let total = dt.year * 12 + (dt.month as i64 - 1) + months;
            let (y, m) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
            let day = if f == EOMonth { days_in_month(y, m) } else { dt.day.min(days_in_month(y, m)) };
            Value::Number(serial_from_parts(y, m as i64, day as i64))
        }
        Now => Value::Number(excel_now()),
        Today => Value::Number(excel_now().floor()),
        Rand => Value::Number(random()),
        RandBetween => {
            arg_count(args, 2, 2)?;
            let (lo, hi) = (num_arg(args, 0, cx)?.ceil(), num_arg(args, 1, cx)?.floor());
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
