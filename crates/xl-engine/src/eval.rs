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
    Substitute => "SUBSTITUTE", Find => "FIND", Search => "SEARCH", Replace => "REPLACE", TextJoin => "TEXTJOIN",
    Proper => "PROPER", TextBefore => "TEXTBEFORE", TextAfter => "TEXTAFTER", Char => "CHAR", Code => "CODE",
    Stdev => "STDEV", StdevS => "STDEV.S", StdevP => "STDEV.P", Var => "VAR", VarS => "VAR.S", VarP => "VAR.P",
    Percentile => "PERCENTILE", PercentileInc => "PERCENTILE.INC", Quartile => "QUARTILE", QuartileInc => "QUARTILE.INC",
    Rank => "RANK", RankEq => "RANK.EQ", Large => "LARGE", Small => "SMALL", Correl => "CORREL",
    Mode => "MODE", ModeSngl => "MODE.SNGL", MaxIfs => "MAXIFS", MinIfs => "MINIFS",
    Pmt => "PMT", Pv => "PV", Fv => "FV", Nper => "NPER", Rate => "RATE", Npv => "NPV", Irr => "IRR",
    Days => "DAYS", DateDif => "DATEDIF", NetworkDays => "NETWORKDAYS", Workday => "WORKDAY", WeekNum => "WEEKNUM",
    Choose => "CHOOSE", XMatch => "XMATCH", Switch => "SWITCH",
    Ceiling => "CEILING", Floor => "FLOOR", Trunc => "TRUNC", SumSq => "SUMSQ",
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
            Substitute => "(text, old_text, new_text, [instance_num])",
            Find | Search => "(find_text, within_text, [start_num])",
            Replace => "(old_text, start_num, num_chars, new_text)",
            TextJoin => "(delimiter, ignore_empty, text1, [text2], …)",
            Proper | Code => "(text)",
            TextBefore | TextAfter => "(text, delimiter, [instance_num])",
            Char => "(number)",
            Stdev | StdevS | StdevP | Var | VarS | VarP | Mode | ModeSngl | SumSq => "(number1, [number2], …)",
            Percentile | PercentileInc => "(array, k)",
            Quartile | QuartileInc => "(array, quart)",
            Rank | RankEq => "(number, ref, [order])",
            Large | Small => "(array, k)",
            Correl => "(array1, array2)",
            MaxIfs | MinIfs => "(range, criteria_range1, criteria1, …)",
            Pmt => "(rate, nper, pv, [fv], [type])",
            Pv => "(rate, nper, pmt, [fv], [type])",
            Fv => "(rate, nper, pmt, [pv], [type])",
            Nper => "(rate, pmt, pv, [fv], [type])",
            Rate => "(nper, pmt, pv, [fv], [type], [guess])",
            Npv => "(rate, value1, [value2], …)",
            Irr => "(values, [guess])",
            Days => "(end_date, start_date)",
            DateDif => "(start_date, end_date, unit)",
            NetworkDays => "(start_date, end_date, [holidays])",
            Workday => "(start_date, days, [holidays])",
            WeekNum => "(serial_number, [return_type])",
            Choose => "(index_num, value1, [value2], …)",
            XMatch => "(lookup_value, lookup_array, [match_mode], [search_mode])",
            Switch => "(expression, value1, result1, …, [default])",
            Ceiling | Floor => "(number, significance)",
            Trunc => "(number, [num_digits])",
        }
    }

    /// One-line, plain-English description shown in autocomplete.
    pub fn description(self) -> &'static str {
        use Func::*;
        match self {
            Sum => "Adds numbers",
            Product => "Multiplies numbers",
            Average => "Returns the average (arithmetic mean) of numbers",
            Min => "Returns the smallest number",
            Max => "Returns the largest number",
            Median => "Returns the middle value of a set of numbers",
            Count => "Counts cells that contain numbers",
            CountA => "Counts cells that are not empty",
            CountBlank => "Counts empty cells in a range",
            SumIf => "Adds the cells that meet a condition",
            CountIf => "Counts the cells that meet a condition",
            AverageIf => "Averages the cells that meet a condition",
            SumIfs => "Adds the cells that meet multiple conditions",
            CountIfs => "Counts the cells that meet multiple conditions",
            AverageIfs => "Averages the cells that meet multiple conditions",
            SumProduct => "Multiplies corresponding cells in ranges and adds the products",
            Abs => "Returns the absolute value of a number",
            Sqrt => "Returns the square root of a number",
            Int => "Rounds a number down to the nearest integer",
            Mod => "Returns the remainder after division",
            Power => "Raises a number to a power",
            Round => "Rounds a number to a given number of digits",
            RoundUp => "Rounds a number away from zero",
            RoundDown => "Rounds a number toward zero",
            Pi => "Returns the value of pi",
            Exp => "Returns e raised to a power",
            Ln => "Returns the natural logarithm of a number",
            Log10 => "Returns the base-10 logarithm of a number",
            Log => "Returns the logarithm of a number to a given base",
            Sign => "Returns the sign of a number: 1, 0 or -1",
            If => "Returns one value if a condition is true and another if it is false",
            Ifs => "Returns the value for the first condition that is true",
            IfError => "Returns a fallback value if an expression is an error",
            IfNa => "Returns a fallback value if an expression is #N/A",
            And => "Returns TRUE if all arguments are true",
            Or => "Returns TRUE if any argument is true",
            Not => "Reverses a logical value",
            True => "Returns the logical value TRUE",
            False => "Returns the logical value FALSE",
            Len => "Returns the number of characters in text",
            Upper => "Converts text to uppercase",
            Lower => "Converts text to lowercase",
            Trim => "Removes extra spaces from text",
            Left => "Returns characters from the start of text",
            Right => "Returns characters from the end of text",
            Mid => "Returns characters from the middle of text",
            Concat => "Joins text from values and ranges",
            Concatenate => "Joins several text values into one",
            Rept => "Repeats text a given number of times",
            Exact => "Checks whether two texts are exactly the same, including case",
            Text => "Formats a value as text using a number format",
            ValueFn => "Converts text that looks like a number into a number",
            IsBlank => "Returns TRUE if the value is empty",
            IsNumber => "Returns TRUE if the value is a number",
            IsText => "Returns TRUE if the value is text",
            IsError => "Returns TRUE if the value is any error",
            IsNa => "Returns TRUE if the value is the #N/A error",
            VLookup => "Looks up a value in the first column of a table and returns a value in the same row",
            HLookup => "Looks up a value in the first row of a table and returns a value in the same column",
            XLookup => "Looks up a value in one range and returns the matching item from another",
            Match => "Returns the position of a value in a range",
            Index => "Returns the value at a given row and column of a range",
            Date => "Builds a date from year, month and day",
            Time => "Builds a time from hours, minutes and seconds",
            Year => "Returns the year of a date",
            Month => "Returns the month of a date (1 to 12)",
            Day => "Returns the day of the month of a date (1 to 31)",
            Hour => "Returns the hour of a time (0 to 23)",
            Minute => "Returns the minute of a time (0 to 59)",
            Second => "Returns the second of a time (0 to 59)",
            Weekday => "Returns the day of the week of a date as a number",
            EDate => "Returns the date a given number of months before or after a date",
            EOMonth => "Returns the last day of the month a given number of months away",
            Now => "Returns the current date and time",
            Today => "Returns the current date",
            Rand => "Returns a random number between 0 and 1",
            RandBetween => "Returns a random integer between two numbers",
            Offset => "Returns a reference shifted from a starting reference",
            Indirect => "Returns the reference named by a text string",
            Row => "Returns the row number of a reference",
            Column => "Returns the column number of a reference",
            Rows => "Returns the number of rows in a range",
            Columns => "Returns the number of columns in a range",
            Substitute => "Replaces occurrences of old text with new text",
            Find => "Finds text within text (case-sensitive) and returns its position",
            Search => "Finds text within text (ignoring case, with wildcards) and returns its position",
            Replace => "Replaces part of text at a given position with new text",
            TextJoin => "Joins text from values and ranges with a delimiter",
            Proper => "Capitalizes the first letter of each word",
            TextBefore => "Returns the text before a delimiter",
            TextAfter => "Returns the text after a delimiter",
            Char => "Returns the character for a character code",
            Code => "Returns the character code of the first character in text",
            Stdev | StdevS => "Estimates standard deviation from a sample",
            StdevP => "Returns the standard deviation of an entire population",
            Var | VarS => "Estimates variance from a sample",
            VarP => "Returns the variance of an entire population",
            Percentile | PercentileInc => "Returns the k-th percentile of values, with k from 0 to 1",
            Quartile | QuartileInc => "Returns a quartile of a set of values",
            Rank | RankEq => "Returns the rank of a number in a list of numbers",
            Large => "Returns the k-th largest value",
            Small => "Returns the k-th smallest value",
            Correl => "Returns the correlation coefficient of two ranges",
            Mode | ModeSngl => "Returns the most frequently occurring number",
            MaxIfs => "Returns the largest value among cells that meet multiple conditions",
            MinIfs => "Returns the smallest value among cells that meet multiple conditions",
            Pmt => "Returns the periodic payment for a loan",
            Pv => "Returns the present value of an investment",
            Fv => "Returns the future value of an investment",
            Nper => "Returns the number of periods for an investment",
            Rate => "Returns the interest rate per period of an annuity",
            Npv => "Returns the net present value of periodic cash flows",
            Irr => "Returns the internal rate of return of periodic cash flows",
            Days => "Returns the number of days between two dates",
            DateDif => "Returns the difference between two dates in years, months or days",
            NetworkDays => "Returns the number of working days between two dates",
            Workday => "Returns the date a given number of working days away",
            WeekNum => "Returns the week number of a date in the year",
            Choose => "Returns the value at a given position in a list of values",
            XMatch => "Returns the position of a value in a range, with flexible matching",
            Switch => "Compares a value against a list and returns the matching result",
            Ceiling => "Rounds a number up to the nearest multiple of significance",
            Floor => "Rounds a number down to the nearest multiple of significance",
            Trunc => "Truncates a number to a given number of digits",
            SumSq => "Returns the sum of the squares of numbers",
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
    /// Script functions, consulted for names the engine doesn't define.
    pub(crate) udfs: Option<&'a dyn crate::udf::UserFunctions>,
}

const MAX_NAME_DEPTH: u8 = 16;

static EMPTY: Value = Value::Empty;

impl<'a> Ctx<'a> {
    pub fn new(sheets: &'a Sheets, names: &'a Names, sheet: SheetId, cell: CellRef) -> Self {
        Self { sheets, names, sheet, cell, depth: std::cell::Cell::new(0), udfs: None }
    }

    /// Let formulas call these script functions.
    pub fn with_udfs(mut self, udfs: Option<&'a dyn crate::udf::UserFunctions>) -> Self {
        self.udfs = udfs;
        self
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
    pub(crate) fn resolve(&self, e: &Expr) -> Option<EvalResult<RangeRef>> {
        match e {
            Expr::Ref(_) | Expr::Range(..) => {
                let (sheet, range) = e.reference()?;
                let sheet = sheet.unwrap_or(self.sheet);
                Some(if self.store(sheet).is_some() { Ok(RangeRef { sheet, range }) } else { Err(ErrorKind::Ref) })
            }
            Expr::Paren(x) => self.resolve(x),
            Expr::Table(t) => Some(self.sheets.resolve_table(t, self.sheet, self.cell).map(|(sheet, range)| RangeRef { sheet, range })),
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

    pub(crate) fn value(&self, sheet: SheetId, c: CellRef) -> &Value {
        self.store(sheet).map_or(&EMPTY, |s| s.value(c))
    }

    fn iter(&self, rr: RangeRef) -> impl Iterator<Item = (CellRef, &Cell)> + '_ {
        self.store(rr.sheet).into_iter().flat_map(move |s| s.iter_range(rr.range))
    }

    /// Clamp a range to the used area so full-column scans stay cheap.
    pub(crate) fn clamp_used(&self, rr: RangeRef) -> RangeRef {
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
        Expr::Ref(_) | Expr::Range(..) | Expr::Table(_) | Expr::Call(Func::Offset | Func::Indirect | Func::Index, _) => match cx.resolve(e) {
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
        Expr::UnknownFunc(name, args) => crate::udf::call(name, args, cx),
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
    let mut total = 0.0;
    let count = scan_criteria(cx, sum_range, pairs, |v| {
        match v {
            Value::Number(n) => total += n,
            Value::Error(e) => return Err(*e),
            _ => {}
        }
        Ok(())
    })?;
    Ok((total, count))
}

/// Visit the cells of `value_range` whose criteria all match (passing each
/// cell's value to `on_match`); returns how many positions matched.
fn scan_criteria(
    cx: &Ctx,
    value_range: Option<RangeRef>,
    pairs: &[(RangeRef, Criterion)],
    mut on_match: impl FnMut(&Value) -> EvalResult<()>,
) -> EvalResult<u64> {
    let (first, first_crit) = &pairs[0];
    let (rows, cols) = (first.range.rows(), first.range.cols());
    for (r, _) in pairs {
        if r.range.rows() != rows || r.range.cols() != cols {
            return Err(ErrorKind::Value);
        }
    }
    let mut count = 0u64;
    let mut visit = |off_r: u32, off_c: u32| -> EvalResult<()> {
        for (r, crit) in pairs {
            let c = CellRef::new(r.range.start.row + off_r, r.range.start.col + off_c);
            if !crit.matches(cx.value(r.sheet, c)) {
                return Ok(());
            }
        }
        count += 1;
        if let Some(sr) = value_range {
            let c = CellRef::new(sr.range.start.row + off_r, sr.range.start.col + off_c);
            on_match(cx.value(sr.sheet, c))?;
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
    Ok(count)
}

/// Parse `(range, criterion)` pairs for the *IFS functions.
fn criteria_pairs(rest: &[Expr], cx: &Ctx) -> EvalResult<Vec<(RangeRef, Criterion)>> {
    if rest.is_empty() || !rest.len().is_multiple_of(2) {
        return Err(ErrorKind::Value);
    }
    rest.chunks(2)
        .map(|pair| Ok((cx.resolve(&pair[0]).unwrap_or(Err(ErrorKind::Value))?, Criterion::new(eval(&pair[1], cx)))))
        .collect()
}

/// Non-empty cells of a range in row-major order.
fn cells_row_major<'c>(cx: &'c Ctx, rr: RangeRef) -> Vec<(CellRef, &'c Value)> {
    let mut cells: Vec<(CellRef, &Value)> = cx.iter(rr).map(|(c, cell)| (c, &cell.value)).collect();
    cells.sort_by_key(|(c, _)| (c.row, c.col));
    cells
}

/// Numbers the way SUM sees them (see [`for_each_number`]), with ranges read
/// row by row so order-sensitive functions (MODE, NPV, IRR) match Excel.
fn collect_numbers(args: &[Expr], cx: &Ctx) -> EvalResult<Vec<f64>> {
    let mut out = Vec::new();
    for a in args {
        if let Some(rr) = cx.resolve(a) {
            for (_, v) in cells_row_major(cx, rr?) {
                match v {
                    Value::Number(n) => out.push(*n),
                    Value::Error(e) => return Err(*e),
                    _ => {}
                }
            }
        } else {
            match eval(a, cx) {
                Value::Empty => {}
                v => out.push(v.to_number()?),
            }
        }
    }
    Ok(out)
}

fn variance(v: &[f64], sample: bool) -> EvalResult<f64> {
    if v.len() < if sample { 2 } else { 1 } {
        return Err(ErrorKind::Div0);
    }
    let n = v.len() as f64;
    let mean = v.iter().sum::<f64>() / n;
    let ss: f64 = v.iter().map(|x| (x - mean).powi(2)).sum();
    Ok(ss / if sample { n - 1.0 } else { n })
}

/// Inclusive percentile with linear interpolation (PERCENTILE.INC).
fn percentile_inc(mut v: Vec<f64>, k: f64) -> EvalResult<f64> {
    if v.is_empty() || !(0.0..=1.0).contains(&k) {
        return Err(ErrorKind::Num);
    }
    v.sort_by(f64::total_cmp);
    let pos = k * (v.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let frac = pos - lo as f64;
    Ok(match v.get(lo + 1) {
        Some(hi) => v[lo] + frac * (hi - v[lo]),
        None => v[lo],
    })
}

/// Snap a quotient to the nearest integer when it's within floating-point noise.
fn snap(q: f64) -> f64 {
    if (q - q.round()).abs() < 1e-9 { q.round() } else { q }
}

/// Find a root of `f` near `guess`: Newton's method, falling back to
/// bisection over a bracket found by scanning plausible rates (> -1).
fn solve_rate(f: impl Fn(f64) -> f64, df: impl Fn(f64) -> f64, guess: f64) -> Option<f64> {
    let mut r = guess;
    for _ in 0..100 {
        let (y, d) = (f(r), df(r));
        if !y.is_finite() || !d.is_finite() || d == 0.0 {
            break;
        }
        let next = r - y / d;
        if !next.is_finite() || next <= -1.0 {
            break;
        }
        if (next - r).abs() < 1e-12 * r.abs().max(1.0) {
            return Some(next);
        }
        r = next;
    }
    const GRID: [f64; 20] = [-0.9999, -0.999, -0.99, -0.9, -0.7, -0.5, -0.3, -0.2, -0.1, -0.05, -0.01, 0.0, 0.01, 0.05, 0.1, 0.2, 0.5, 1.0, 10.0, 100.0];
    let (mut lo, mut hi) = GRID.windows(2).find_map(|w| {
        let (a, b) = (f(w[0]), f(w[1]));
        (a.is_finite() && b.is_finite() && a.signum() != b.signum()).then_some((w[0], w[1]))
    })?;
    let mut flo = f(lo);
    for _ in 0..200 {
        let mid = (lo + hi) / 2.0;
        let fm = f(mid);
        if fm == 0.0 || (hi - lo).abs() < 1e-15 {
            return Some(mid);
        }
        if fm.signum() == flo.signum() {
            lo = mid;
            flo = fm;
        } else {
            hi = mid;
        }
    }
    Some((lo + hi) / 2.0)
}

/// Optional `type` argument of the annuity functions: 0 = end of period, 1 = beginning.
fn pay_type(args: &[Expr], i: usize, cx: &Ctx) -> EvalResult<f64> {
    Ok(if opt_num(args, i, cx, 0.0)? != 0.0 { 1.0 } else { 0.0 })
}

/// Serial date with the time of day dropped; #NUM! outside the valid range.
fn date_arg(args: &[Expr], i: usize, cx: &Ctx) -> EvalResult<i64> {
    let n = num_arg(args, i, cx)?;
    datetime_from_serial(n).ok_or(ErrorKind::Num)?;
    Ok(n.floor() as i64)
}

/// 0 = Sunday … 6 = Saturday.
fn weekday_of(serial: i64) -> i64 {
    (serial + 6).rem_euclid(7)
}

fn holidays(args: &[Expr], i: usize, cx: &Ctx) -> EvalResult<std::collections::HashSet<i64>> {
    let mut set = std::collections::HashSet::new();
    if let Some(a) = args.get(i) {
        for_each_number(std::slice::from_ref(a), cx, |n| {
            set.insert(n.floor() as i64);
        })?;
    }
    Ok(set)
}

/// One element of a SEARCH pattern.
#[derive(Copy, Clone, PartialEq)]
enum Tok {
    Any,
    One,
    Lit(char),
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Parse a SEARCH pattern: `*` and `?` are wildcards, `~` escapes the next char.
fn search_pattern(pat: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut it = pat.chars();
    while let Some(c) = it.next() {
        out.push(match c {
            '*' => Tok::Any,
            '?' => Tok::One,
            '~' => Tok::Lit(it.next().map_or('~', fold)),
            c => Tok::Lit(fold(c)),
        });
    }
    out
}

/// Whether the pattern matches some prefix of `t`.
fn tokens_match_prefix(p: &[Tok], t: &[char]) -> bool {
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    loop {
        if pi == p.len() {
            return true;
        }
        if ti < t.len() && (p[pi] == Tok::One || p[pi] == Tok::Lit(t[ti])) {
            pi += 1;
            ti += 1;
        } else if p[pi] == Tok::Any {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(sp) = star
            && mark < t.len()
        {
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
}

/// CHAR/CODE use the Windows-1252 code page for 128–159.
const CP1252_HIGH: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™',
    'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
];

/// Position (offset within `look`) of `target` per XLOOKUP/XMATCH match_mode
/// (0 exact, -1 exact or next smaller, 1 exact or next larger, 2 wildcard).
fn xlookup_find(cx: &Ctx, target: &Value, look: RangeRef, mode: i64, reverse: bool) -> EvalResult<Option<u32>> {
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
    Ok(match mode {
        0 => lookup_exact(target, keys.into_iter(), false),
        2 => lookup_exact(target, keys.into_iter(), true),
        -1 | 1 => {
            // Exact match, else next smaller (-1) / next larger (1); data need not be sorted.
            let mut best: Option<(u32, Value)> = None;
            let mut exact = None;
            for (i, v) in keys {
                if v.is_empty() || std::mem::discriminant(&v) != std::mem::discriminant(target) {
                    continue;
                }
                match compare(&v, target) {
                    Ordering::Equal => {
                        exact = Some(i);
                        break;
                    }
                    Ordering::Less if mode == -1 => {
                        if best.as_ref().is_none_or(|(_, b)| compare(&v, b) == Ordering::Greater) {
                            best = Some((i, v));
                        }
                    }
                    Ordering::Greater if mode == 1 && best.as_ref().is_none_or(|(_, b)| compare(&v, b) == Ordering::Less) => {
                        best = Some((i, v));
                    }
                    _ => {}
                }
            }
            exact.or(best.map(|(i, _)| i))
        }
        _ => return Err(ErrorKind::Value),
    })
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
            match xlookup_find(cx, &target, look, mode, reverse)? {
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

        // ---- Text ----
        Substitute => {
            arg_count(args, 3, 4)?;
            let (t, old, new) = (text_arg(args, 0, cx)?, text_arg(args, 1, cx)?, text_arg(args, 2, cx)?);
            if old.is_empty() {
                return Ok(Value::Text(t));
            }
            match args.get(3) {
                None => Value::text(t.replace(&*old, &new)),
                Some(_) => {
                    let k = num_arg(args, 3, cx)?.trunc();
                    if k < 1.0 {
                        return Err(ErrorKind::Value);
                    }
                    match t.match_indices(&*old).nth(k as usize - 1) {
                        Some((i, _)) => Value::text(format!("{}{}{}", &t[..i], new, &t[i + old.len()..])),
                        None => Value::Text(t),
                    }
                }
            }
        }
        Find | Search => {
            arg_count(args, 2, 3)?;
            let (needle, hay) = (text_arg(args, 0, cx)?, text_arg(args, 1, cx)?);
            let start = opt_num(args, 2, cx, 1.0)?.trunc();
            let hay: Vec<char> = hay.chars().collect();
            if start < 1.0 || start > hay.len() as f64 + 1.0 {
                return Err(ErrorKind::Value);
            }
            let start = start as usize - 1;
            let pos = if f == Find {
                let needle: Vec<char> = needle.chars().collect();
                (start..=hay.len()).find(|&i| hay[i..].starts_with(&needle))
            } else {
                let pat = search_pattern(&needle);
                let hay: Vec<char> = hay.iter().map(|&c| fold(c)).collect();
                (start..=hay.len()).find(|&i| tokens_match_prefix(&pat, &hay[i..]))
            };
            Value::Number(pos.ok_or(ErrorKind::Value)? as f64 + 1.0)
        }
        Replace => {
            arg_count(args, 4, 4)?;
            let t = text_arg(args, 0, cx)?;
            let (start, n) = (num_arg(args, 1, cx)?.trunc(), num_arg(args, 2, cx)?.trunc());
            let new = text_arg(args, 3, cx)?;
            if start < 1.0 || n < 0.0 {
                return Err(ErrorKind::Value);
            }
            let skip = start as usize - 1;
            let mut out: String = t.chars().take(skip).collect();
            out.push_str(&new);
            out.extend(t.chars().skip(skip.saturating_add(n as usize)));
            Value::text(out)
        }
        TextJoin => {
            if args.len() < 3 {
                return Err(ErrorKind::Value);
            }
            let delim = text_arg(args, 0, cx)?;
            let ignore_empty = eval(&args[1], cx).to_bool()?;
            const MAX_LEN: usize = 32_767;
            let mut parts: Vec<Arc<str>> = Vec::new();
            let mut len = 0;
            let mut push = |s: Arc<str>| -> EvalResult<()> {
                if ignore_empty && s.is_empty() {
                    return Ok(());
                }
                len += s.len() + delim.len();
                if len > MAX_LEN + delim.len() {
                    return Err(ErrorKind::Value);
                }
                parts.push(s);
                Ok(())
            };
            for a in &args[2..] {
                match cx.resolve(a) {
                    Some(rr) => {
                        let rr = rr?;
                        // Blank cells only matter when kept and separated by a delimiter.
                        let rr = if ignore_empty || delim.is_empty() { cx.clamp_used(rr) } else { rr };
                        for c in rr.range.iter() {
                            push(cx.value(rr.sheet, c).to_text()?)?;
                        }
                    }
                    None => push(eval(a, cx).to_text()?)?,
                }
            }
            Value::text(parts.join(&delim))
        }
        Proper => {
            arg_count(args, 1, 1)?;
            let t = text_arg(args, 0, cx)?;
            let mut out = String::with_capacity(t.len());
            let mut prev_letter = false;
            for c in t.chars() {
                if c.is_alphabetic() {
                    if prev_letter { out.extend(c.to_lowercase()) } else { out.extend(c.to_uppercase()) }
                    prev_letter = true;
                } else {
                    out.push(c);
                    prev_letter = false;
                }
            }
            Value::text(out)
        }
        TextBefore | TextAfter => {
            arg_count(args, 2, 3)?;
            let (t, d) = (text_arg(args, 0, cx)?, text_arg(args, 1, cx)?);
            let k = opt_num(args, 2, cx, 1.0)?.trunc();
            if k == 0.0 || k.abs() > t.chars().count().max(1) as f64 {
                return Err(ErrorKind::Value);
            }
            let at = if d.is_empty() {
                // An empty delimiter matches at the very start (or end, counting backwards).
                if k > 0.0 { 0 } else { t.len() }
            } else if k > 0.0 {
                t.match_indices(&*d).nth(k as usize - 1).ok_or(ErrorKind::NA)?.0
            } else {
                t.rmatch_indices(&*d).nth((-k) as usize - 1).ok_or(ErrorKind::NA)?.0
            };
            Value::text(if f == TextBefore { &t[..at] } else { &t[at + d.len()..] })
        }
        Char => {
            arg_count(args, 1, 1)?;
            let n = num_arg(args, 0, cx)?.trunc();
            if !(1.0..=255.0).contains(&n) {
                return Err(ErrorKind::Value);
            }
            let n = n as u8;
            let c = if (0x80..0xA0).contains(&n) { CP1252_HIGH[n as usize - 0x80] } else { char::from(n) };
            Value::text(c.to_string())
        }
        Code => {
            arg_count(args, 1, 1)?;
            let t = text_arg(args, 0, cx)?;
            let c = t.chars().next().ok_or(ErrorKind::Value)?;
            let code = match CP1252_HIGH.iter().position(|&x| x == c) {
                Some(i) => 0x80 + i as u32,
                None if (c as u32) < 256 => c as u32,
                None => '?' as u32, // not representable in the code page
            };
            Value::Number(code as f64)
        }

        // ---- Statistics ----
        Stdev | StdevS | StdevP | Var | VarS | VarP => {
            if args.is_empty() {
                return Err(ErrorKind::Value);
            }
            let v = collect_numbers(args, cx)?;
            let var = variance(&v, !matches!(f, StdevP | VarP))?;
            Value::num(if matches!(f, Stdev | StdevS | StdevP) { var.sqrt() } else { var })
        }
        Percentile | PercentileInc | Quartile | QuartileInc => {
            arg_count(args, 2, 2)?;
            let v = collect_numbers(&args[..1], cx)?;
            let k = num_arg(args, 1, cx)?;
            let k = if matches!(f, Quartile | QuartileInc) {
                let q = k.trunc();
                if !(0.0..=4.0).contains(&q) {
                    return Err(ErrorKind::Num);
                }
                q / 4.0
            } else {
                k
            };
            Value::num(percentile_inc(v, k)?)
        }
        Rank | RankEq => {
            arg_count(args, 2, 3)?;
            let x = num_arg(args, 0, cx)?;
            let rr = range_arg(args, 1, cx)?;
            let ascending = opt_num(args, 2, cx, 0.0)? != 0.0;
            let (mut found, mut before) = (false, 0u64);
            for (_, cell) in cx.iter(rr) {
                if let Value::Number(n) = cell.value {
                    found |= n == x;
                    if (ascending && n < x) || (!ascending && n > x) {
                        before += 1;
                    }
                }
            }
            if !found {
                return Err(ErrorKind::NA);
            }
            Value::Number(before as f64 + 1.0)
        }
        Large | Small => {
            arg_count(args, 2, 2)?;
            let mut v = collect_numbers(&args[..1], cx)?;
            let k = num_arg(args, 1, cx)?.ceil();
            if k < 1.0 || k > v.len() as f64 {
                return Err(ErrorKind::Num);
            }
            v.sort_by(f64::total_cmp);
            let k = k as usize;
            Value::num(if f == Small { v[k - 1] } else { v[v.len() - k] })
        }
        Correl => {
            arg_count(args, 2, 2)?;
            let (a, b) = (range_arg(args, 0, cx)?, range_arg(args, 1, cx)?);
            if a.range.cell_count() != b.range.cell_count() {
                return Err(ErrorKind::NA);
            }
            // Pair cells by position (row-major) when the shapes differ but sizes match.
            let b_at = |c: CellRef| {
                let i = (c.row - a.range.start.row) as u64 * a.range.cols() as u64 + (c.col - a.range.start.col) as u64;
                let (r, k) = (i / b.range.cols() as u64, i % b.range.cols() as u64);
                cx.value(b.sheet, CellRef::new(b.range.start.row + r as u32, b.range.start.col + k as u32))
            };
            let mut pairs = Vec::new();
            for (c, cell) in cx.iter(a) {
                match (&cell.value, b_at(c)) {
                    (Value::Error(e), _) | (_, Value::Error(e)) => return Err(*e),
                    (Value::Number(x), Value::Number(y)) => pairs.push((*x, *y)),
                    _ => {}
                }
            }
            let n = pairs.len() as f64;
            if pairs.is_empty() {
                return Err(ErrorKind::Div0);
            }
            let (mx, my) = (pairs.iter().map(|p| p.0).sum::<f64>() / n, pairs.iter().map(|p| p.1).sum::<f64>() / n);
            let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
            for (x, y) in &pairs {
                sxy += (x - mx) * (y - my);
                sxx += (x - mx).powi(2);
                syy += (y - my).powi(2);
            }
            if sxx == 0.0 || syy == 0.0 {
                return Err(ErrorKind::Div0);
            }
            Value::num(sxy / (sxx * syy).sqrt())
        }
        Mode | ModeSngl => {
            if args.is_empty() {
                return Err(ErrorKind::Value);
            }
            let v = collect_numbers(args, cx)?;
            let key = |x: f64| (x + 0.0).to_bits(); // +0.0 folds -0 into 0
            let mut counts = std::collections::HashMap::new();
            for &x in &v {
                *counts.entry(key(x)).or_insert(0usize) += 1;
            }
            // Ties go to the value that appears first.
            let mut best: Option<(f64, usize)> = None;
            for &x in &v {
                let count = counts[&key(x)];
                if count > 1 && best.is_none_or(|(_, c)| count > c) {
                    best = Some((x, count));
                }
            }
            Value::Number(best.ok_or(ErrorKind::NA)?.0)
        }
        MaxIfs | MinIfs => {
            if args.len() < 3 {
                return Err(ErrorKind::Value);
            }
            let target = range_arg(args, 0, cx)?;
            let pairs = criteria_pairs(&args[1..], cx)?;
            if target.range.rows() != pairs[0].0.range.rows() || target.range.cols() != pairs[0].0.range.cols() {
                return Err(ErrorKind::Value);
            }
            let mut acc: Option<f64> = None;
            scan_criteria(cx, Some(target), &pairs, |v| {
                match v {
                    Value::Number(n) => acc = Some(acc.map_or(*n, |a| if f == MaxIfs { a.max(*n) } else { a.min(*n) })),
                    Value::Error(e) => return Err(*e),
                    _ => {}
                }
                Ok(())
            })?;
            Value::num(acc.unwrap_or(0.0))
        }

        // ---- Finance ----
        Pmt => {
            arg_count(args, 3, 5)?;
            let (r, n, pv) = (num_arg(args, 0, cx)?, num_arg(args, 1, cx)?, num_arg(args, 2, cx)?);
            let (fv, t) = (opt_num(args, 3, cx, 0.0)?, pay_type(args, 4, cx)?);
            if n == 0.0 {
                return Err(ErrorKind::Num);
            }
            if r == 0.0 {
                Value::num(-(pv + fv) / n)
            } else {
                let g = (1.0 + r).powf(n);
                Value::num(-(r * (pv * g + fv)) / ((1.0 + r * t) * (g - 1.0)))
            }
        }
        Pv => {
            arg_count(args, 3, 5)?;
            let (r, n, pmt) = (num_arg(args, 0, cx)?, num_arg(args, 1, cx)?, num_arg(args, 2, cx)?);
            let (fv, t) = (opt_num(args, 3, cx, 0.0)?, pay_type(args, 4, cx)?);
            if r == 0.0 {
                Value::num(-(fv + pmt * n))
            } else {
                let g = (1.0 + r).powf(n);
                Value::num(-(fv + pmt * (1.0 + r * t) * (g - 1.0) / r) / g)
            }
        }
        Fv => {
            arg_count(args, 3, 5)?;
            let (r, n, pmt) = (num_arg(args, 0, cx)?, num_arg(args, 1, cx)?, num_arg(args, 2, cx)?);
            let (pv, t) = (opt_num(args, 3, cx, 0.0)?, pay_type(args, 4, cx)?);
            if r == 0.0 {
                Value::num(-(pv + pmt * n))
            } else {
                let g = (1.0 + r).powf(n);
                Value::num(-(pv * g + pmt * (1.0 + r * t) * (g - 1.0) / r))
            }
        }
        Nper => {
            arg_count(args, 3, 5)?;
            let (r, pmt, pv) = (num_arg(args, 0, cx)?, num_arg(args, 1, cx)?, num_arg(args, 2, cx)?);
            let (fv, t) = (opt_num(args, 3, cx, 0.0)?, pay_type(args, 4, cx)?);
            if r == 0.0 {
                if pmt == 0.0 {
                    return Err(ErrorKind::Num);
                }
                Value::num(-(pv + fv) / pmt)
            } else {
                let k = pmt * (1.0 + r * t);
                let ratio = (k - fv * r) / (k + pv * r);
                if ratio <= 0.0 || r <= -1.0 {
                    return Err(ErrorKind::Num);
                }
                Value::num(ratio.ln() / (1.0 + r).ln())
            }
        }
        Rate => {
            arg_count(args, 3, 6)?;
            let (n, pmt, pv) = (num_arg(args, 0, cx)?, num_arg(args, 1, cx)?, num_arg(args, 2, cx)?);
            let (fv, t, guess) = (opt_num(args, 3, cx, 0.0)?, pay_type(args, 4, cx)?, opt_num(args, 5, cx, 0.1)?);
            if n <= 0.0 {
                return Err(ErrorKind::Num);
            }
            // Balance after n periods; zero at the solution.
            let f = |r: f64| {
                if r.abs() < 1e-10 {
                    return pv + pmt * n + fv;
                }
                let g = (1.0 + r).powf(n);
                pv * g + pmt * (1.0 + r * t) * (g - 1.0) / r + fv
            };
            let df = |r: f64| {
                if r.abs() < 1e-10 {
                    // Derivative at 0 from the series expansion.
                    return pv * n + pmt * (n * (n - 1.0) / 2.0 + t * n);
                }
                let g = (1.0 + r).powf(n);
                let dg = n * (1.0 + r).powf(n - 1.0);
                pv * dg + pmt * (t * (g - 1.0) / r + (1.0 + r * t) * (dg * r - (g - 1.0)) / (r * r))
            };
            let r = solve_rate(f, df, guess).ok_or(ErrorKind::Num)?;
            if f(r).abs() > 1e-6 * (pv.abs() + (pmt * n).abs() + fv.abs()).max(1.0) {
                return Err(ErrorKind::Num);
            }
            Value::num(r)
        }
        Npv => {
            if args.len() < 2 {
                return Err(ErrorKind::Value);
            }
            let r = num_arg(args, 0, cx)?;
            if r == -1.0 {
                return Err(ErrorKind::Div0);
            }
            let v = collect_numbers(&args[1..], cx)?;
            Value::num(v.iter().enumerate().map(|(i, x)| x / (1.0 + r).powi(i as i32 + 1)).sum())
        }
        Irr => {
            arg_count(args, 1, 2)?;
            let v = collect_numbers(&args[..1], cx)?;
            let guess = opt_num(args, 1, cx, 0.1)?;
            if !v.iter().any(|&x| x > 0.0) || !v.iter().any(|&x| x < 0.0) {
                return Err(ErrorKind::Num);
            }
            let f = |r: f64| v.iter().enumerate().map(|(i, x)| x / (1.0 + r).powi(i as i32)).sum::<f64>();
            let df = |r: f64| v.iter().enumerate().map(|(i, x)| -(i as f64) * x / (1.0 + r).powi(i as i32 + 1)).sum::<f64>();
            let r = solve_rate(f, df, guess).ok_or(ErrorKind::Num)?;
            if f(r).abs() > 1e-6 * v.iter().map(|x| x.abs()).sum::<f64>() {
                return Err(ErrorKind::Num);
            }
            Value::num(r)
        }

        // ---- Dates ----
        Days => {
            arg_count(args, 2, 2)?;
            Value::Number((date_arg(args, 0, cx)? - date_arg(args, 1, cx)?) as f64)
        }
        DateDif => {
            arg_count(args, 3, 3)?;
            let (s, e) = (date_arg(args, 0, cx)?, date_arg(args, 1, cx)?);
            let unit = text_arg(args, 2, cx)?.to_ascii_uppercase();
            if s > e {
                return Err(ErrorKind::Num);
            }
            let (a, b) = (datetime_from_serial(s as f64).ok_or(ErrorKind::Num)?, datetime_from_serial(e as f64).ok_or(ErrorKind::Num)?);
            let months = (b.year - a.year) * 12 + b.month as i64 - a.month as i64 - i64::from(b.day < a.day);
            Value::Number(match unit.as_str() {
                "Y" => months.div_euclid(12),
                "M" => months,
                "D" => e - s,
                "YM" => months.rem_euclid(12),
                // Excel computes MD against the start day in the month before the end
                // (which can be negative, e.g. Jan 31 → Mar 1 gives -2).
                "MD" if b.day >= a.day => (b.day - a.day) as i64,
                "MD" => e - serial_from_parts(b.year, b.month as i64 - 1, a.day as i64) as i64,
                "YD" => {
                    let mut anchor = serial_from_parts(b.year, a.month as i64, a.day as i64) as i64;
                    if anchor > e {
                        anchor = serial_from_parts(b.year - 1, a.month as i64, a.day as i64) as i64;
                    }
                    e - anchor
                }
                _ => return Err(ErrorKind::Num),
            } as f64)
        }
        NetworkDays => {
            arg_count(args, 2, 3)?;
            let (s, e) = (date_arg(args, 0, cx)?, date_arg(args, 1, cx)?);
            let hol = holidays(args, 2, cx)?;
            let (lo, hi) = (s.min(e), s.max(e));
            let count = (lo..=hi).filter(|&d| !matches!(weekday_of(d), 0 | 6) && !hol.contains(&d)).count() as f64;
            Value::Number(if e < s { -count } else { count })
        }
        Workday => {
            arg_count(args, 2, 3)?;
            let mut d = date_arg(args, 0, cx)?;
            let days = num_arg(args, 1, cx)?.trunc();
            if days.abs() > 1e6 {
                return Err(ErrorKind::Num);
            }
            let hol = holidays(args, 2, cx)?;
            let step = if days < 0.0 { -1 } else { 1 };
            let mut left = days.abs() as i64;
            while left > 0 {
                d += step;
                if !matches!(weekday_of(d), 0 | 6) && !hol.contains(&d) {
                    left -= 1;
                }
            }
            datetime_from_serial(d as f64).ok_or(ErrorKind::Num)?;
            Value::Number(d as f64)
        }
        WeekNum => {
            arg_count(args, 1, 2)?;
            let s = date_arg(args, 0, cx)?;
            let kind = opt_num(args, 1, cx, 1.0)?.trunc() as i64;
            let year_start = |y: i64| serial_from_parts(y, 1, 1) as i64;
            let week = if kind == 21 {
                // ISO 8601: the week belongs to the year containing its Thursday.
                let iso_wd = (weekday_of(s) + 6) % 7 + 1; // Monday = 1 … Sunday = 7
                let thursday = s - iso_wd + 4;
                let y = datetime_from_serial(thursday as f64).ok_or(ErrorKind::Num)?.year;
                (thursday - year_start(y)) / 7 + 1
            } else {
                let first_day = match kind {
                    1 | 17 => 0,
                    2 | 11 => 1,
                    12..=16 => kind - 10,
                    _ => return Err(ErrorKind::Num),
                };
                let jan1 = year_start(datetime_from_serial(s as f64).ok_or(ErrorKind::Num)?.year);
                let offset = (weekday_of(jan1) - first_day).rem_euclid(7);
                (s - jan1 + offset) / 7 + 1
            };
            Value::Number(week as f64)
        }

        // ---- Lookup & logic ----
        Choose => {
            if args.len() < 2 {
                return Err(ErrorKind::Value);
            }
            let i = num_arg(args, 0, cx)?.trunc();
            if i < 1.0 || i >= args.len() as f64 {
                return Err(ErrorKind::Value);
            }
            eval(&args[i as usize], cx)
        }
        XMatch => {
            arg_count(args, 2, 4)?;
            let target = eval(&args[0], cx);
            if let Value::Error(e) = target {
                return Err(e);
            }
            let look = range_arg(args, 1, cx)?;
            let mode = opt_num(args, 2, cx, 0.0)?.trunc() as i64;
            // Binary search modes (±2) give the same answer on sorted data.
            let reverse = match opt_num(args, 3, cx, 1.0)?.trunc() as i64 {
                1 | 2 => false,
                -1 | -2 => true,
                _ => return Err(ErrorKind::Value),
            };
            let off = xlookup_find(cx, &target, look, mode, reverse)?.ok_or(ErrorKind::NA)?;
            Value::Number(off as f64 + 1.0)
        }
        Switch => {
            if args.len() < 3 {
                return Err(ErrorKind::Value);
            }
            let v = eval(&args[0], cx);
            if let Value::Error(e) = v {
                return Err(e);
            }
            let rest = &args[1..];
            for [value, result] in rest.as_chunks::<2>().0 {
                let candidate = eval(value, cx);
                if let Value::Error(e) = candidate {
                    return Err(e);
                }
                if std::mem::discriminant(&candidate) == std::mem::discriminant(&v) && compare(&v, &candidate) == Ordering::Equal {
                    return Ok(eval(result, cx));
                }
            }
            match rest.len() % 2 {
                1 => eval(&rest[rest.len() - 1], cx),
                _ => Value::Error(ErrorKind::NA),
            }
        }

        // ---- Math ----
        Ceiling | Floor => {
            arg_count(args, 2, 2)?;
            let (n, sig) = (num_arg(args, 0, cx)?, num_arg(args, 1, cx)?);
            if n == 0.0 {
                return Ok(Value::Number(0.0));
            }
            if sig == 0.0 {
                return if f == Ceiling { Ok(Value::Number(0.0)) } else { Err(ErrorKind::Div0) };
            }
            if n > 0.0 && sig < 0.0 {
                return Err(ErrorKind::Num);
            }
            let q = snap(n / sig);
            Value::num(if f == Ceiling { q.ceil() } else { q.floor() } * sig)
        }
        Trunc => {
            arg_count(args, 1, 2)?;
            let n = num_arg(args, 0, cx)?;
            round_to(n, opt_num(args, 1, cx, 0.0)?, f64::trunc)
        }
        SumSq => {
            let mut t = 0.0;
            for_each_number(args, cx, |n| t += n * n)?;
            Value::num(t)
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
