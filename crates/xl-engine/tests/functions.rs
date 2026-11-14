//! Worksheet functions checked against Excel's results.

use xl_engine::datetime::serial_from_date;
use xl_engine::{Addr, CellRef, ErrorKind, Func, SheetId, Value, Workbook};

struct T {
    wb: Workbook,
    s: SheetId,
}

impl T {
    fn new() -> Self {
        let wb = Workbook::new();
        let s = wb.sheet_ids()[0];
        Self { wb, s }
    }
    fn set(&mut self, a1: &str, raw: &str) {
        self.wb.set_input(Addr::new(self.s, CellRef::parse(a1).unwrap()), raw).unwrap();
    }
    /// Fill a column starting at `a1` with raw inputs.
    fn col(&mut self, col: &str, first_row: u32, raws: &[&str]) {
        for (i, raw) in raws.iter().enumerate() {
            self.set(&format!("{col}{}", first_row + i as u32), raw);
        }
    }
    fn v(&self, a1: &str) -> Value {
        self.wb.value(Addr::new(self.s, CellRef::parse(a1).unwrap())).clone()
    }
    /// Evaluate a formula in a scratch cell.
    fn eval(&mut self, formula: &str) -> Value {
        self.set("Z100", formula);
        self.v("Z100")
    }
    fn num(&mut self, formula: &str) -> f64 {
        match self.eval(formula) {
            Value::Number(n) => n,
            v => panic!("{formula} gave {v:?}"),
        }
    }
    fn approx(&mut self, formula: &str, expected: f64, tol: f64) {
        let got = self.num(formula);
        assert!((got - expected).abs() <= tol, "{formula}: got {got}, expected {expected}");
    }
    fn is(&mut self, formula: &str, expected: Value) {
        assert_eq!(self.eval(formula), expected, "{formula}");
    }
    fn err(&mut self, formula: &str, e: ErrorKind) {
        self.is(formula, Value::Error(e));
    }
    fn text(&mut self, formula: &str, expected: &str) {
        self.is(formula, Value::text(expected));
    }
}

fn n(x: f64) -> Value {
    Value::Number(x)
}

fn date(y: i64, m: u32, d: u32) -> Value {
    n(serial_from_date(y, m, d))
}

// ---------------------------------------------------------------- text

#[test]
fn substitute() {
    let mut t = T::new();
    t.text("=SUBSTITUTE(\"Sales Data\",\"Sales\",\"Cost\")", "Cost Data");
    t.text("=SUBSTITUTE(\"Quarter 1, 2008\",\"1\",\"2\",1)", "Quarter 2, 2008");
    t.text("=SUBSTITUTE(\"Quarter 1, 2011\",\"1\",\"2\",3)", "Quarter 1, 2012");
    t.text("=SUBSTITUTE(\"aaa\",\"a\",\"b\",5)", "aaa");
    t.text("=SUBSTITUTE(\"abc\",\"\",\"x\")", "abc");
    t.err("=SUBSTITUTE(\"abc\",\"a\",\"x\",0)", ErrorKind::Value);
}

#[test]
fn find_and_search() {
    let mut t = T::new();
    t.is("=FIND(\"M\",\"Miriam McGovern\")", n(1.0));
    t.is("=FIND(\"m\",\"Miriam McGovern\")", n(6.0));
    t.is("=FIND(\"M\",\"Miriam McGovern\",3)", n(8.0));
    t.is("=FIND(\"é\",\"café au lait\")", n(4.0)); // character, not byte, positions
    t.is("=FIND(\"\",\"abc\")", n(1.0));
    t.err("=FIND(\"z\",\"abc\")", ErrorKind::Value);
    t.err("=FIND(\"a\",\"abc\",0)", ErrorKind::Value);
    t.err("=FIND(\"a\",\"abc\",5)", ErrorKind::Value);
    t.err("=FIND(\"A\",\"abc\")", ErrorKind::Value); // case-sensitive
    t.err("=FIND(\"a*\",\"abc\")", ErrorKind::Value); // no wildcards

    t.is("=SEARCH(\"e\",\"Statements\",6)", n(7.0));
    t.is("=SEARCH(\"MARGIN\",\"Profit Margin\")", n(8.0));
    t.is("=SEARCH(\"p?e\",\"apple pie\")", n(3.0));
    t.is("=SEARCH(\"p*e\",\"apple pie\")", n(2.0));
    t.is("=SEARCH(\"~*\",\"a*b\")", n(2.0));
    t.is("=SEARCH(\"~?\",\"why? not\")", n(4.0));
    t.err("=SEARCH(\"x?z\",\"xz\")", ErrorKind::Value);
    t.err("=SEARCH(\"q\",\"abc\")", ErrorKind::Value);
}

#[test]
fn replace() {
    let mut t = T::new();
    t.text("=REPLACE(\"abcdefghijk\",6,5,\"*\")", "abcde*k");
    t.text("=REPLACE(\"2009\",3,2,\"10\")", "2010");
    t.text("=REPLACE(\"123456\",1,3,\"@\")", "@456");
    t.text("=REPLACE(\"abc\",10,1,\"!\")", "abc!");
    t.text("=REPLACE(\"héllo\",2,1,\"e\")", "hello");
    t.err("=REPLACE(\"abc\",0,1,\"x\")", ErrorKind::Value);
    t.err("=REPLACE(\"abc\",1,-1,\"x\")", ErrorKind::Value);
}

#[test]
fn textjoin() {
    let mut t = T::new();
    t.set("A1", "a");
    t.set("B1", "b");
    t.set("A2", "c");
    t.set("B3", "d");
    // Row-major order: A1 B1 A2 (B2 blank) (A3 blank) B3.
    t.text("=TEXTJOIN(\",\",TRUE,A1:B3)", "a,b,c,d");
    t.text("=TEXTJOIN(\",\",FALSE,A1:B3)", "a,b,c,,,d");
    t.text("=TEXTJOIN(\"-\",TRUE,\"x\",A1,\"\",1)", "x-a-1");
    t.text("=TEXTJOIN(\"\",TRUE,A:B)", "abcd");
    t.err("=TEXTJOIN(\",\",TRUE)", ErrorKind::Value);
}

#[test]
fn proper() {
    let mut t = T::new();
    t.text("=PROPER(\"this is a TITLE\")", "This Is A Title");
    t.text("=PROPER(\"2-way street\")", "2-Way Street");
    t.text("=PROPER(\"76BudGet\")", "76Budget");
    t.text("=PROPER(\"o'neil\")", "O'Neil");
}

#[test]
fn textbefore_textafter() {
    let mut t = T::new();
    t.set("A1", "Red riding hood's, red hood");
    t.text("=TEXTBEFORE(A1,\"hood\")", "Red riding ");
    t.text("=TEXTAFTER(A1,\"hood\")", "'s, red hood");
    t.text("=TEXTBEFORE(A1,\"hood\",2)", "Red riding hood's, red ");
    t.text("=TEXTAFTER(A1,\"hood\",-1)", "");
    t.text("=TEXTBEFORE(A1,\"hood\",-1)", "Red riding hood's, red ");
    t.err("=TEXTAFTER(A1,\"red\",-2)", ErrorKind::NA); // case-sensitive: only one "red"
    t.text("=TEXTAFTER(\"a-b-c\",\"-\",-2)", "b-c");
    t.text("=TEXTBEFORE(\"a-b-c\",\"-\",-2)", "a");
    t.err("=TEXTBEFORE(A1,\"xyz\")", ErrorKind::NA);
    t.err("=TEXTAFTER(\"a-b\",\"-\",2)", ErrorKind::NA);
    t.err("=TEXTAFTER(\"a-b\",\"-\",0)", ErrorKind::Value);
}

#[test]
fn char_and_code() {
    let mut t = T::new();
    t.text("=CHAR(65)", "A");
    t.text("=CHAR(33)", "!");
    t.text("=CHAR(128)", "€"); // Windows-1252
    t.text("=CHAR(233)", "é");
    t.is("=CODE(\"A\")", n(65.0));
    t.is("=CODE(\"alphabet\")", n(97.0));
    t.is("=CODE(\"€\")", n(128.0));
    t.is("=CODE(CHAR(200))", n(200.0));
    t.err("=CHAR(0)", ErrorKind::Value);
    t.err("=CHAR(256)", ErrorKind::Value);
    t.err("=CODE(\"\")", ErrorKind::Value);
}

// ---------------------------------------------------------- statistics

#[test]
fn stdev_and_var() {
    let mut t = T::new();
    t.col("A", 1, &["1345", "1301", "1368", "1322", "1310", "1370", "1318", "1350", "1303", "1299"]);
    t.set("A11", "text is ignored");
    // Excel's documented results for this data.
    t.approx("=STDEV(A1:A11)", 27.463915719843495, 1e-9);
    t.approx("=STDEV.S(A1:A11)", 27.463915719843495, 1e-9);
    t.approx("=STDEV.P(A1:A11)", 26.054558142482477, 1e-9);
    t.approx("=VAR(A1:A11)", 754.2666666666667, 1e-9);
    t.approx("=VAR.S(A1:A11)", 754.2666666666667, 1e-9);
    t.approx("=VAR.P(A1:A11)", 678.84, 1e-9);
    t.is("=VAR.P(5)", n(0.0));
    t.err("=STDEV(5)", ErrorKind::Div0);
    t.err("=VAR.S(B1:B5)", ErrorKind::Div0);
    t.err("=STDEV.P(B1:B5)", ErrorKind::Div0);
}

#[test]
fn percentile_and_quartile() {
    let mut t = T::new();
    t.col("A", 1, &["1", "3", "2", "4"]);
    t.approx("=PERCENTILE(A1:A4,0.3)", 1.9, 1e-12);
    t.approx("=PERCENTILE.INC(A1:A4,0.3)", 1.9, 1e-12);
    t.is("=PERCENTILE(A1:A4,0)", n(1.0));
    t.is("=PERCENTILE(A1:A4,1)", n(4.0));
    t.err("=PERCENTILE(A1:A4,1.5)", ErrorKind::Num);
    t.err("=PERCENTILE(A1:A4,-0.1)", ErrorKind::Num);
    t.err("=PERCENTILE(B1:B4,0.5)", ErrorKind::Num);

    t.col("C", 1, &["1", "2", "4", "7", "8", "9", "10", "12"]);
    t.is("=QUARTILE(C1:C8,1)", n(3.5));
    t.is("=QUARTILE.INC(C1:C8,2)", n(7.5));
    t.is("=QUARTILE(C1:C8,0)", n(1.0));
    t.is("=QUARTILE(C1:C8,4)", n(12.0));
    t.err("=QUARTILE(C1:C8,5)", ErrorKind::Num);
    t.err("=QUARTILE(C1:C8,-1)", ErrorKind::Num);
}

#[test]
fn rank() {
    let mut t = T::new();
    t.col("A", 1, &["7", "3.5", "3.5", "1", "2"]);
    t.is("=RANK(A2,A1:A5,1)", n(3.0));
    t.is("=RANK(A1,A1:A5)", n(1.0));
    t.is("=RANK.EQ(3.5,A1:A5)", n(2.0)); // ties share the higher rank
    t.is("=RANK.EQ(1,A1:A5,0)", n(5.0));
    t.err("=RANK(4,A1:A5)", ErrorKind::NA);
}

#[test]
fn large_and_small() {
    let mut t = T::new();
    t.col("A", 1, &["3", "5", "3", "5", "4"]);
    t.col("B", 1, &["4", "2", "4", "6", "7"]);
    t.is("=LARGE(A1:B5,3)", n(5.0));
    t.is("=LARGE(A1:B5,7)", n(4.0));
    t.is("=SMALL(A1:B5,1)", n(2.0));
    t.is("=SMALL(A1:B5,4)", n(4.0));
    t.err("=LARGE(A1:B5,0)", ErrorKind::Num);
    t.err("=SMALL(A1:B5,11)", ErrorKind::Num);
}

#[test]
fn correl() {
    let mut t = T::new();
    t.col("A", 1, &["3", "2", "4", "5", "6"]);
    t.col("B", 1, &["9", "7", "12", "15", "17"]);
    t.approx("=CORREL(A1:A5,B1:B5)", 0.9970544855015815, 1e-12);
    // Pairs with a non-number on either side are skipped.
    t.set("A6", "x");
    t.set("B6", "100");
    t.approx("=CORREL(A1:A6,B1:B6)", 0.9970544855015815, 1e-12);
    t.err("=CORREL(A1:A5,B1:B4)", ErrorKind::NA);
    t.col("C", 1, &["1", "1", "1"]);
    t.err("=CORREL(C1:C3,A1:A3)", ErrorKind::Div0);
}

#[test]
fn mode() {
    let mut t = T::new();
    t.col("A", 1, &["5.6", "4", "4", "3", "2", "4"]);
    t.is("=MODE(A1:A6)", n(4.0));
    t.is("=MODE.SNGL(A1:A6)", n(4.0));
    // Equal frequency: the value seen first wins.
    t.is("=MODE(1,2,2,1,3)", n(1.0));
    t.is("=MODE(3,2,2,3)", n(3.0));
    t.err("=MODE(1,2,3)", ErrorKind::NA);
}

#[test]
fn maxifs_minifs() {
    let mut t = T::new();
    t.col("A", 1, &["89", "93", "96", "85", "91", "88"]);
    t.col("B", 1, &["1", "2", "2", "3", "1", "1"]);
    t.col("C", 1, &["a", "b", "a", "a", "b", "a"]);
    t.is("=MAXIFS(A1:A6,B1:B6,1)", n(91.0));
    t.is("=MINIFS(A1:A6,B1:B6,1)", n(88.0));
    t.is("=MAXIFS(A1:A6,B1:B6,\">1\",C1:C6,\"a\")", n(96.0));
    t.is("=MINIFS(A1:A6,B1:B6,\">1\",C1:C6,\"a\")", n(85.0));
    t.is("=MAXIFS(A1:A6,B1:B6,9)", n(0.0));
    t.is("=MINIFS(A1:A6,B1:B6,9)", n(0.0));
    t.err("=MAXIFS(A1:A6,B1:B5,1)", ErrorKind::Value);
    t.err("=MAXIFS(A1:A6,B1:B6)", ErrorKind::Value);
}

// ------------------------------------------------------------- finance

#[test]
fn pmt_pv_fv_nper() {
    let mut t = T::new();
    // Excel: -1073.64
    t.approx("=PMT(5%/12,360,200000)", -1073.6432460242795, 1e-9);
    t.approx("=PMT(8%/12,10,10000)", -1037.0320893591636, 1e-9);
    // Paying at the beginning of each period.
    t.approx("=PMT(8%/12,10,10000,0,1)", -1030.1643271779772, 1e-9);
    t.approx("=PMT(0,10,1000)", -100.0, 1e-12);
    t.approx("=PMT(0,10,1000,200)", -120.0, 1e-12);
    t.err("=PMT(5%,0,1000)", ErrorKind::Num);

    t.approx("=PV(0.08/12,12*20,500)", -59777.14585118777, 1e-7);
    t.approx("=PV(0,10,-100)", 1000.0, 1e-12);

    t.approx("=FV(0.06/12,10,-200,-500,1)", 2581.4033740601362, 1e-9);
    t.approx("=FV(0.12/12,12,-1000)", 12682.503013196976, 1e-9);
    t.approx("=FV(0,12,-100,-50)", 1250.0, 1e-12);

    // Excel: 59.6738657
    t.approx("=NPER(0.12/12,-100,-1000,10000,1)", 59.67386567429457, 1e-9);
    t.approx("=NPER(0,-100,1000)", 10.0, 1e-12);
    t.err("=NPER(0,0,1000)", ErrorKind::Num);
    t.err("=NPER(0.1,-50,1000)", ErrorKind::Num); // payment below the interest: never pays off
}

#[test]
fn rate() {
    let mut t = T::new();
    // Excel: 0.77% per month for a 4-year, $8000 loan paid at $200/month.
    t.approx("=RATE(48,-200,8000)", 0.007701472488201888, 1e-9);
    t.approx("=RATE(48,-200,8000)*12", 0.09241766985842266, 1e-9);
    t.approx("=RATE(10,-100,1000)", 0.0, 1e-9);
    // Round trip through PV.
    t.approx("=PV(RATE(60,-300,14000,0,1),60,-300,0,1)", 14000.0, 1e-6);
    // Payments that can never repay the loan have no rate.
    t.err("=RATE(10,100,1000)", ErrorKind::Num);
    t.err("=RATE(0,-100,1000)", ErrorKind::Num);
}

#[test]
fn npv_and_irr() {
    let mut t = T::new();
    // Excel: 1188.44
    t.approx("=NPV(10%,-10000,3000,4200,6800)", 1188.4434123352212, 1e-9);
    t.col("A", 1, &["-70000", "12000", "15000", "18000", "21000", "26000"]);
    t.approx("=NPV(10%,A2:A6)+A1", -2683.3114976001525, 1e-9);
    t.approx("=NPV(0,A1:A6)", 22000.0, 1e-9);
    t.err("=NPV(-1,A1:A6)", ErrorKind::Div0);

    // Excel: 8.66%
    t.approx("=IRR(A1:A6)", 0.0866309480365316, 1e-9);
    // Excel: -2.12% over the first four years.
    t.approx("=IRR(A1:A5)", -0.021244848272, 1e-9);
    // Excel: -44.35% over three years with a guess.
    t.approx("=IRR(A1:A3,-10%)", -0.44350694133, 1e-9);
    t.err("=IRR(A2:A6)", ErrorKind::Num); // no sign change
}

// --------------------------------------------------------------- dates

#[test]
fn days() {
    let mut t = T::new();
    t.is("=DAYS(DATE(2021,3,15),DATE(2021,2,1))", n(42.0));
    t.is("=DAYS(DATE(2021,2,1),DATE(2021,3,15))", n(-42.0));
    t.is("=DAYS(DATE(2021,3,15)+0.9,DATE(2021,3,14)+0.1)", n(1.0));
}

#[test]
fn datedif() {
    let mut t = T::new();
    t.is("=DATEDIF(DATE(2001,1,1),DATE(2003,1,1),\"Y\")", n(2.0));
    t.is("=DATEDIF(DATE(2001,6,1),DATE(2002,8,15),\"D\")", n(440.0));
    t.is("=DATEDIF(DATE(2001,6,1),DATE(2002,8,15),\"YD\")", n(75.0));
    t.is("=DATEDIF(DATE(2001,6,1),DATE(2002,8,15),\"M\")", n(14.0));
    t.is("=DATEDIF(DATE(2001,6,1),DATE(2002,8,15),\"YM\")", n(2.0));
    t.is("=DATEDIF(DATE(2001,6,1),DATE(2002,8,15),\"MD\")", n(14.0));
    // End day before start day: count from the 25th of the previous month (Feb 2020 has 29 days).
    t.is("=DATEDIF(DATE(2020,1,25),DATE(2020,3,5),\"MD\")", n(9.0));
    t.is("=DATEDIF(DATE(2020,1,25),DATE(2020,3,5),\"M\")", n(1.0));
    t.is("=DATEDIF(DATE(2020,1,25),DATE(2021,1,24),\"Y\")", n(0.0));
    t.is("=DATEDIF(DATE(2020,1,25),DATE(2021,1,24),\"YM\")", n(11.0));
    t.is("=DATEDIF(DATE(2020,5,10),DATE(2021,2,1),\"YD\")", n(267.0));
    t.is("=DATEDIF(DATE(2020,1,1),DATE(2020,1,1),\"d\")", n(0.0));
    t.err("=DATEDIF(DATE(2003,1,1),DATE(2001,1,1),\"Y\")", ErrorKind::Num);
    t.err("=DATEDIF(DATE(2001,1,1),DATE(2003,1,1),\"W\")", ErrorKind::Num);
}

#[test]
fn networkdays() {
    let mut t = T::new();
    t.set("A1", "=DATE(2012,10,1)");
    t.set("A2", "=DATE(2013,3,1)");
    t.set("B1", "=DATE(2012,11,22)");
    t.set("B2", "=DATE(2012,12,4)");
    t.set("B3", "=DATE(2013,1,21)");
    // Excel's documented examples.
    t.is("=NETWORKDAYS(A1,A2)", n(110.0));
    t.is("=NETWORKDAYS(A1,A2,B1)", n(109.0));
    t.is("=NETWORKDAYS(A1,A2,B1:B3)", n(107.0));
    t.is("=NETWORKDAYS(A2,A1,B1:B3)", n(-107.0));
    // A Saturday-to-Sunday span has no working days; a single weekday counts once.
    t.is("=NETWORKDAYS(DATE(2026,11,14),DATE(2026,11,15))", n(0.0));
    t.is("=NETWORKDAYS(DATE(2026,11,13),DATE(2026,11,13))", n(1.0));
}

#[test]
fn workday() {
    let mut t = T::new();
    t.set("A1", "=DATE(2008,10,1)");
    t.set("B1", "=DATE(2008,11,26)");
    t.set("B2", "=DATE(2008,12,4)");
    t.set("B3", "=DATE(2009,1,21)");
    t.is("=WORKDAY(A1,151)", date(2009, 4, 30));
    t.is("=WORKDAY(A1,151,B1:B3)", date(2009, 5, 5));
    t.is("=WORKDAY(DATE(2026,11,13),1)", date(2026, 11, 16)); // Friday -> Monday
    t.is("=WORKDAY(DATE(2026,11,16),-1)", date(2026, 11, 13));
    t.is("=WORKDAY(DATE(2026,11,14),0)", date(2026, 11, 14));
    t.is("=WORKDAY(DATE(2026,11,14),5)", date(2026, 11, 20)); // from a Saturday
}

#[test]
fn weeknum() {
    let mut t = T::new();
    t.is("=WEEKNUM(DATE(2012,3,9))", n(10.0));
    t.is("=WEEKNUM(DATE(2012,3,9),2)", n(11.0));
    t.is("=WEEKNUM(DATE(2026,1,1))", n(1.0));
    t.is("=WEEKNUM(DATE(2026,1,4))", n(2.0)); // 2026-01-04 is a Sunday
    t.is("=WEEKNUM(DATE(2026,1,4),2)", n(1.0));
    t.is("=WEEKNUM(DATE(2026,12,31))", n(53.0));
    // ISO weeks.
    t.is("=WEEKNUM(DATE(2021,1,1),21)", n(53.0));
    t.is("=WEEKNUM(DATE(2021,1,4),21)", n(1.0));
    t.is("=WEEKNUM(DATE(2026,12,31),21)", n(53.0));
    t.is("=WEEKNUM(DATE(2019,12,30),21)", n(1.0));
    t.is("=WEEKNUM(DATE(2026,11,13),21)", n(46.0));
    t.err("=WEEKNUM(DATE(2026,1,1),5)", ErrorKind::Num);
}

// ------------------------------------------------------ lookup & logic

#[test]
fn choose() {
    let mut t = T::new();
    t.text("=CHOOSE(2,\"1st\",\"2nd\",\"3rd\",\"Finished\")", "2nd");
    t.set("A1", "10");
    t.set("A2", "20");
    t.is("=CHOOSE(1.9,A1,A2)", n(10.0));
    t.is("=SUM(A1,CHOOSE(2,A1,A2))", n(30.0));
    t.err("=CHOOSE(3,A1,A2)", ErrorKind::Value);
    t.err("=CHOOSE(0,A1,A2)", ErrorKind::Value);
}

#[test]
fn xmatch() {
    let mut t = T::new();
    t.col("A", 1, &["Apple", "Grape", "Pear", "Banana", "Cherry"]);
    t.is("=XMATCH(\"pear\",A1:A5)", n(3.0));
    t.is("=XMATCH(\"Gr*\",A1:A5,2)", n(2.0));
    t.err("=XMATCH(\"Gr*\",A1:A5)", ErrorKind::NA);
    t.err("=XMATCH(\"Kiwi\",A1:A5)", ErrorKind::NA);

    t.col("B", 1, &["10", "20", "30", "20", "40"]);
    t.is("=XMATCH(20,B1:B5)", n(2.0));
    t.is("=XMATCH(20,B1:B5,0,-1)", n(4.0)); // search from the end
    t.is("=XMATCH(25,B1:B5,1)", n(3.0)); // next larger
    t.is("=XMATCH(25,B1:B5,-1)", n(2.0)); // next smaller
    t.err("=XMATCH(50,B1:B5,1)", ErrorKind::NA);
    t.err("=XMATCH(20,B1:B5,3)", ErrorKind::Value);

    t.set("C1", "a");
    t.set("D1", "b");
    t.set("E1", "c");
    t.is("=XMATCH(\"c\",C1:E1)", n(3.0));
}

#[test]
fn switch() {
    let mut t = T::new();
    t.text("=SWITCH(2,1,\"one\",2,\"two\",\"other\")", "two");
    t.text("=SWITCH(9,1,\"one\",2,\"two\",\"other\")", "other");
    t.text("=SWITCH(\"b\",\"a\",\"A\",\"B\",\"Bee\")", "Bee");
    t.err("=SWITCH(9,1,\"one\",2,\"two\")", ErrorKind::NA);
    t.err("=SWITCH(\"1\",1,\"num\")", ErrorKind::NA); // text never equals a number
    t.err("=SWITCH(1,2)", ErrorKind::Value);
    t.err("=SWITCH(1/0,1,\"x\")", ErrorKind::Div0);
}

// ---------------------------------------------------------------- math

#[test]
fn ceiling_and_floor() {
    let mut t = T::new();
    t.is("=CEILING(2.5,1)", n(3.0));
    t.is("=CEILING(-2.5,-2)", n(-4.0));
    t.is("=CEILING(-2.5,2)", n(-2.0));
    t.approx("=CEILING(1.5,0.1)", 1.5, 1e-12);
    t.approx("=CEILING(0.234,0.01)", 0.24, 1e-12);
    t.is("=CEILING(5,0)", n(0.0));
    t.err("=CEILING(2.5,-2)", ErrorKind::Num);

    t.is("=FLOOR(3.7,2)", n(2.0));
    t.is("=FLOOR(-2.5,-2)", n(-2.0));
    t.is("=FLOOR(-2.5,2)", n(-4.0));
    t.approx("=FLOOR(1.58,0.1)", 1.5, 1e-12);
    t.approx("=FLOOR(0.234,0.01)", 0.23, 1e-12);
    t.approx("=FLOOR(0.3,0.1)", 0.3, 1e-12); // not 0.2 from 0.3/0.1 = 2.9999…
    t.err("=FLOOR(2.5,-2)", ErrorKind::Num);
    t.err("=FLOOR(2.5,0)", ErrorKind::Div0);
}

#[test]
fn trunc_and_sumsq() {
    let mut t = T::new();
    t.is("=TRUNC(8.9)", n(8.0));
    t.is("=TRUNC(-8.9)", n(-8.0));
    t.approx("=TRUNC(0.45,1)", 0.4, 1e-12);
    t.is("=TRUNC(1234.5,-2)", n(1200.0));
    t.is("=SUMSQ(3,4)", n(25.0));
    t.col("A", 1, &["1", "2", "x", "3"]);
    t.is("=SUMSQ(A1:A4)", n(14.0));
    t.err("=SUMSQ(\"x\")", ErrorKind::Value);
}

// ---------------------------------------------------------- metadata

#[test]
fn every_function_has_description_and_signature() {
    for f in Func::ALL {
        let d = f.description();
        assert!(!d.trim().is_empty(), "{} has no description", f.name());
        assert!(!d.ends_with('.'), "{}: description should not end with a period", f.name());
        assert!(d.len() <= 90, "{}: description too long", f.name());
        assert!(f.signature().starts_with('('), "{} has no signature", f.name());
    }
}

#[test]
fn dotted_names_parse() {
    let mut t = T::new();
    t.col("A", 1, &["2", "4", "4", "4", "5", "5", "7", "9"]);
    t.is("=STDEV.P(A1:A8)", n(2.0));
    t.is("=stdev.p(A1:A8)", n(2.0));
    assert_eq!(Func::from_name("mode.sngl"), Some(Func::ModeSngl));
}
