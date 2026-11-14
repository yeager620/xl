use xl_engine::datetime::serial_from_date;
use xl_engine::{Addr, CellRef, CellSnapshot, ErrorKind, LoadedCell, SheetId, Value, Workbook};

fn a(sheet: SheetId, a1: &str) -> Addr {
    Addr::new(sheet, CellRef::parse(a1).unwrap())
}

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
        self.wb.set_input(a(self.s, a1), raw).unwrap();
    }
    fn v(&self, a1: &str) -> Value {
        self.wb.value(a(self.s, a1)).clone()
    }
    fn shown(&self, a1: &str) -> String {
        self.wb.display(a(self.s, a1)).text
    }
}

fn n(x: f64) -> Value {
    Value::Number(x)
}

#[test]
fn basic_arithmetic_and_propagation() {
    let mut t = T::new();
    t.set("A1", "2");
    t.set("A2", "3");
    t.set("A3", "=A1*A2+1");
    t.set("A4", "=A3^2");
    assert_eq!(t.v("A3"), n(7.0));
    assert_eq!(t.v("A4"), n(49.0));
    t.set("A1", "10");
    assert_eq!(t.v("A4"), n(961.0));
    t.set("A2", "");
    assert_eq!(t.v("A3"), n(1.0));
}

#[test]
fn precedence_matches_excel() {
    let mut t = T::new();
    t.set("A1", "=-2^2");
    t.set("A2", "=2^3^2");
    t.set("A3", "=1+2*3-4/2");
    t.set("A4", "=50%*10");
    t.set("A5", "=\"a\"&1+1");
    assert_eq!(t.v("A1"), n(4.0));
    assert_eq!(t.v("A2"), n(64.0));
    assert_eq!(t.v("A3"), n(5.0));
    assert_eq!(t.v("A4"), n(5.0));
    assert_eq!(t.v("A5"), Value::text("a2"));
}

#[test]
fn ranges_and_aggregates() {
    let mut t = T::new();
    for (i, x) in ["1", "2", "3", "4", "hello"].iter().enumerate() {
        t.set(&format!("A{}", i + 1), x);
    }
    t.set("B1", "=SUM(A1:A5)");
    t.set("B2", "=AVERAGE(A:A)");
    t.set("B3", "=COUNT(A1:A5)");
    t.set("B4", "=COUNTA(A1:A5)");
    t.set("B5", "=MEDIAN(A1:A4)");
    t.set("B6", "=SUMIF(A1:A4,\">2\")");
    t.set("B7", "=COUNTIF(A1:A5,\"h*\")");
    t.set("B8", "=COUNTIF(A1:A10,\"<>3\")");
    assert_eq!(t.v("B1"), n(10.0));
    assert_eq!(t.v("B2"), n(2.5));
    assert_eq!(t.v("B3"), n(4.0));
    assert_eq!(t.v("B4"), n(5.0));
    assert_eq!(t.v("B5"), n(2.5));
    assert_eq!(t.v("B6"), n(7.0));
    assert_eq!(t.v("B7"), n(1.0));
    assert_eq!(t.v("B8"), n(9.0), "blanks count as <>3, including past the data");
    t.set("A2", "20");
    assert_eq!(t.v("B1"), n(28.0));
    t.set("A1000", "100");
    assert_eq!(t.v("B2"), n(128.0 / 5.0));
}

#[test]
fn multi_criteria_and_lookups() {
    let mut t = T::new();
    let rows = [("East", "Pen", "5"), ("West", "Pen", "7"), ("East", "Ink", "2"), ("East", "Pen", "4")];
    for (i, (r, p, q)) in rows.iter().enumerate() {
        t.set(&format!("A{}", i + 1), r);
        t.set(&format!("B{}", i + 1), p);
        t.set(&format!("C{}", i + 1), q);
    }
    t.set("E1", "=SUMIFS(C:C,A:A,\"East\",B:B,\"Pen\")");
    t.set("E2", "=COUNTIFS(A1:A4,\"East\",C1:C4,\">3\")");
    t.set("E3", "=AVERAGEIFS(C1:C4,B1:B4,\"Pen\")");
    t.set("E4", "=SUMPRODUCT(C1:C4,C1:C4)");
    t.set("E5", "=XLOOKUP(\"Ink\",B1:B4,C1:C4)");
    t.set("E6", "=XLOOKUP(\"Pen\",B1:B4,A1:A4,\"none\",0,-1)");
    t.set("E7", "=XLOOKUP(\"Quill\",B1:B4,C1:C4,\"none\")");
    t.set("E8", "=VLOOKUP(\"West\",A1:C4,3,FALSE)");
    t.set("E9", "=IFS(C1>6,\"big\",C1>3,\"mid\",TRUE,\"small\")");
    assert_eq!(t.v("E1"), n(9.0));
    assert_eq!(t.v("E2"), n(2.0));
    assert_eq!(t.v("E3"), n(16.0 / 3.0));
    assert_eq!(t.v("E4"), n(94.0));
    assert_eq!(t.v("E5"), n(2.0));
    assert_eq!(t.v("E6"), Value::text("East"));
    assert_eq!(t.v("E7"), Value::text("none"));
    assert_eq!(t.v("E8"), n(7.0));
    assert_eq!(t.v("E9"), Value::text("mid"));
}

#[test]
fn logic_and_text() {
    let mut t = T::new();
    t.set("A1", "apple");
    t.set("A2", "banana");
    t.set("C1", "=IF(LEN(A1)>3,UPPER(A1),\"no\")");
    t.set("C2", "=IFERROR(1/0,\"div\")");
    t.set("C3", "=LEFT(A2,3)&MID(A1,2,2)&RIGHT(A1)");
    t.set("C4", "=ROUND(2.675,2)");
    t.set("C5", "=TEXT(1234.5,\"$#,##0.00\")");
    t.set("C6", "=VALUE(\"15%\")");
    assert_eq!(t.v("C1"), Value::text("APPLE"));
    assert_eq!(t.v("C2"), Value::text("div"));
    assert_eq!(t.v("C3"), Value::text("banppe"));
    assert_eq!(t.v("C4"), n(2.68));
    assert_eq!(t.v("C5"), Value::text("$1,234.50"));
    assert_eq!(t.v("C6"), n(0.15));
}

#[test]
fn dates() {
    let mut t = T::new();
    t.set("A1", "2026-11-13");
    t.set("A2", "=A1+30");
    t.set("A3", "=DATE(2026,2,31)");
    t.set("A4", "=EOMONTH(A1,1)");
    t.set("A5", "=YEAR(A1)*10000+MONTH(A1)*100+DAY(A1)");
    t.set("A6", "=WEEKDAY(A1)");
    t.set("A7", "=TEXT(A1,\"dddd\")");
    assert_eq!(t.v("A1"), n(serial_from_date(2026, 11, 13)));
    assert_eq!(t.shown("A1"), "2026-11-13", "ISO input keeps an ISO format");
    assert_eq!(t.v("A3"), n(serial_from_date(2026, 3, 3)));
    assert_eq!(t.shown("A3"), "3/3/2026", "DATE() auto-formats as a date");
    assert_eq!(t.v("A4"), n(serial_from_date(2026, 12, 31)));
    assert_eq!(t.v("A5"), n(20261113.0));
    assert_eq!(t.v("A6"), n(6.0));
    assert_eq!(t.v("A7"), Value::text("Friday"));
}

#[test]
fn number_formats_and_editing_text() {
    let mut t = T::new();
    let s = t.s;
    t.set("A1", "$1,200.5");
    assert_eq!(t.v("A1"), n(1200.5));
    assert_eq!(t.shown("A1"), "$1,200.50");
    assert_eq!(t.wb.input_text(a(s, "A1")), "1200.5");
    t.set("A2", "12.5%");
    assert_eq!(t.shown("A2"), "12.50%");
    assert_eq!(t.wb.input_text(a(s, "A2")), "12.5%");
    t.set("A3", "11/13/2026");
    assert_eq!(t.wb.input_text(a(s, "A3")), "11/13/2026");
    // Typing a number into a formatted cell keeps the format.
    t.set("A1", "7");
    assert_eq!(t.shown("A1"), "$7.00");
    // Explicit format change.
    t.wb.set_format(a(s, "A4"), Some("0.000"));
    t.set("A4", "=1/3");
    assert_eq!(t.shown("A4"), "0.333");
    // Text that looks like a number round-trips through the editor with an apostrophe.
    t.set("A5", "'00123");
    assert_eq!(t.v("A5"), Value::text("00123"));
    assert_eq!(t.wb.input_text(a(s, "A5")), "'00123");
    // Precision: 15 significant digits.
    t.set("A6", "=0.1+0.2");
    assert_eq!(xl_engine::precise(0.1 + 0.2), "0.3");
    // Negative numbers in a red format.
    t.wb.set_format(a(s, "A7"), Some("0.0;[Red]-0.0"));
    t.set("A7", "-2");
    let d = t.wb.display(a(s, "A7"));
    assert_eq!((d.text.as_str(), d.color), ("-2.0", Some(xl_engine::FmtColor::Red)));
}

#[test]
fn cross_sheet_references() {
    let mut wb = Workbook::new();
    let s1 = wb.sheet_ids()[0];
    let data = wb.add_sheet(Some("Q4 Data")).unwrap();
    wb.set_input(a(data, "A1"), "10").unwrap();
    wb.set_input(a(data, "A2"), "32").unwrap();
    wb.set_input(a(s1, "B1"), "='Q4 Data'!A1*2").unwrap();
    wb.set_input(a(s1, "B2"), "=SUM('q4 data'!A:A)").unwrap();
    assert_eq!(*wb.value(a(s1, "B1")), n(20.0));
    assert_eq!(*wb.value(a(s1, "B2")), n(42.0));
    // Editing the other sheet propagates across.
    wb.set_input(a(data, "A1"), "100").unwrap();
    assert_eq!(*wb.value(a(s1, "B1")), n(200.0));
    assert_eq!(*wb.value(a(s1, "B2")), n(132.0));
    // Renaming updates formula text without breaking anything.
    wb.rename_sheet(data, "Data").unwrap();
    assert_eq!(wb.input_text(a(s1, "B1")), "=Data!A1*2");
    wb.set_input(a(data, "A2"), "1").unwrap();
    assert_eq!(*wb.value(a(s1, "B2")), n(101.0));
    // Unknown sheets are parse errors; duplicate / invalid names are rejected.
    assert!(wb.set_input(a(s1, "C1"), "=Nope!A1").is_err());
    assert!(wb.add_sheet(Some("data")).is_err());
    assert!(wb.rename_sheet(s1, "bad:name").is_err());
    // Deleting a sheet turns references into #REF!, including refs to its empty cells.
    wb.set_input(a(s1, "C2"), "=Data!Z99+1").unwrap();
    assert_eq!(*wb.value(a(s1, "C2")), n(1.0));
    wb.delete_sheet(data).unwrap();
    assert_eq!(*wb.value(a(s1, "B1")), Value::Error(ErrorKind::Ref));
    assert_eq!(*wb.value(a(s1, "C2")), Value::Error(ErrorKind::Ref));
    assert_eq!(wb.input_text(a(s1, "B1")), "=#REF!*2");
    assert!(wb.delete_sheet(s1).is_err(), "can't delete the last sheet");
}

#[test]
fn errors_and_cycles() {
    let mut t = T::new();
    t.set("A1", "=1/0");
    t.set("A2", "=A1+1");
    t.set("A3", "=NOSUCHFN(1)");
    assert_eq!(t.v("A2"), Value::Error(ErrorKind::Div0));
    assert_eq!(t.v("A3"), Value::Error(ErrorKind::Name));
    t.set("B1", "=B2+1");
    t.set("B2", "=B1+1");
    assert_eq!(t.v("B1"), Value::Error(ErrorKind::Circ));
    t.set("B2", "5");
    assert_eq!(t.v("B1"), n(6.0));
    t.set("C1", "=C1");
    assert_eq!(t.v("C1"), Value::Error(ErrorKind::Circ));
    assert!(t.wb.set_input(a(t.s, "D1"), "=1+").is_err());
}

#[test]
fn early_cutoff_skips_unchanged_branches() {
    let mut t = T::new();
    t.set("A1", "1");
    t.set("B1", "=IF(A1>0,1,0)");
    for r in 2..=1000 {
        t.set(&format!("B{r}"), &format!("=B{}+1", r - 1));
    }
    let stats = t.wb.set_input(a(t.s, "A1"), "5").unwrap();
    assert_eq!(stats.evaluated, 1);
    let stats = t.wb.set_input(a(t.s, "A1"), "-5").unwrap();
    assert_eq!(stats.evaluated, 1000);
    assert_eq!(t.v("B1000"), n(999.0));
}

#[test]
fn snapshots_restore_inputs_and_formats() {
    let mut t = T::new();
    let s = t.s;
    t.set("A1", "15%");
    t.set("A2", "=A1*2");
    let snap = vec![(a(s, "A1"), t.wb.snapshot(a(s, "A1"))), (a(s, "A2"), t.wb.snapshot(a(s, "A2")))];
    t.set("A1", "hello");
    t.wb.set_format(a(s, "A1"), None);
    t.wb.restore(snap);
    assert_eq!(t.v("A1"), n(0.15));
    assert_eq!(t.shown("A1"), "15%");
    assert_eq!(t.v("A2"), n(0.3));
    let shifted = t.wb.snapshot_shifted(a(s, "A2"), 1, 1);
    assert_eq!(shifted, CellSnapshot { input: "=B2*2".into(), format: None, style: Default::default() });
}

#[test]
fn loading_keeps_cached_values_for_unsupported_formulas() {
    let mut wb = Workbook::empty();
    let s = wb.add_sheet(Some("Sales")).unwrap();
    wb.load_cell(a(s, "A1"), LoadedCell::Value(n(4.0)));
    wb.load_cell(a(s, "A2"), LoadedCell::Formula { src: "A1*10".into(), cached: n(0.0) });
    wb.load_cell(a(s, "A3"), LoadedCell::Formula { src: "_xlfn.LAMBDA(x,x+1)(A1)".into(), cached: n(5.0) });
    wb.load_cell(a(s, "A4"), LoadedCell::Formula { src: "SUM({1,2,6})".into(), cached: n(9.0) });
    wb.load_cell(a(s, "A5"), LoadedCell::Formula { src: "A3+A4".into(), cached: n(0.0) });
    wb.recalc_all();
    assert_eq!(*wb.value(a(s, "A2")), n(40.0), "supported formulas are recalculated");
    assert_eq!(*wb.value(a(s, "A3")), n(5.0), "unsupported functions keep the cached value");
    assert_eq!(*wb.value(a(s, "A4")), n(9.0), "unparseable syntax keeps the cached value");
    assert_eq!(*wb.value(a(s, "A5")), n(14.0), "dependents use cached values");
    assert_eq!(wb.input_text(a(s, "A4")), "=SUM({1,2,6})");
}

#[test]
fn styles_follow_snapshots_and_copies() {
    use xl_engine::{Border, HAlign, Style};
    let mut t = T::new();
    let s = t.s;
    let bold_red = Style { bold: true, color: Some(0xC00000), fill: Some(0xFFF2CC), align: HAlign::Center, bottom: Border::THIN, ..Style::default() };
    t.set("A1", "=1+1");
    t.wb.set_style(a(s, "A1"), &bold_red);
    assert_eq!(t.wb.style(a(s, "A1")), &bold_red);
    let snap = t.wb.snapshot(a(s, "A1"));
    t.wb.set_style(a(s, "A1"), &Style::default());
    assert!(t.wb.style(a(s, "A1")).is_default());
    assert_eq!(t.wb.styled_cells(s).count(), 0);
    t.wb.restore(vec![(a(s, "A1"), snap)]);
    assert_eq!(t.wb.style(a(s, "A1")), &bold_red);
    // Copies carry the style; identical styles are shared.
    let copy = t.wb.snapshot_shifted(a(s, "A1"), 1, 0);
    t.wb.restore(vec![(a(s, "A2"), copy)]);
    assert_eq!(t.wb.style(a(s, "A2")), &bold_red);
    assert_eq!(t.wb.styled_cells(s).count(), 2);
}

#[test]
fn range_dependencies_across_index_blocks() {
    let mut t = T::new();
    // Crosses the 1024-row block boundary; tall (100k rows); whole column.
    t.set("B1", "=SUM(A1000:A1100)");
    t.set("B2", "=SUM(A1:A100000)");
    t.set("B3", "=COUNT(A:A)");
    t.set("A1024", "1");
    t.set("A1025", "2");
    t.set("A99999", "4");
    assert_eq!(t.v("B1"), n(3.0));
    assert_eq!(t.v("B2"), n(7.0));
    assert_eq!(t.v("B3"), n(3.0));
    // Replacing a formula removes its old range from the index.
    t.set("B1", "=SUM(A5000:A5001)");
    let stats = t.wb.set_input(a(t.s, "A1024"), "10").unwrap();
    assert_eq!(stats.evaluated, 2, "only B2 and B3 still depend on A1024");
    t.set("A5001", "8");
    assert_eq!(t.v("B1"), n(8.0));
}

#[test]
fn named_ranges() {
    let mut wb = Workbook::new();
    let s1 = wb.sheet_ids()[0];
    let rates = wb.add_sheet(Some("Rates")).unwrap();
    wb.set_input(a(rates, "B2"), "8.25%").unwrap();
    wb.set_input(a(s1, "A1"), "100").unwrap();
    wb.set_input(a(s1, "A2"), "200").unwrap();
    wb.define_name("TaxRate", None, "Rates!$B$2").unwrap();
    wb.define_name("Amounts", None, "Sheet1!$A$1:$A$2").unwrap();
    wb.set_input(a(s1, "B1"), "=SUM(Amounts)*TaxRate").unwrap();
    assert_eq!(*wb.value(a(s1, "B1")), n(24.75));
    // Changing the named cell recalculates dependents through the name.
    wb.set_input(a(rates, "B2"), "10%").unwrap();
    assert_eq!(*wb.value(a(s1, "B1")), n(30.0));
    wb.set_input(a(s1, "A2"), "300").unwrap();
    assert_eq!(*wb.value(a(s1, "B1")), n(40.0));
    // Constants, undefined names, redefinition, local scope.
    wb.define_name("Fee", None, "2.5").unwrap();
    wb.set_input(a(s1, "B2"), "=Fee*2").unwrap();
    assert_eq!(*wb.value(a(s1, "B2")), n(5.0));
    wb.set_input(a(s1, "B3"), "=Missing+1").unwrap();
    assert_eq!(*wb.value(a(s1, "B3")), Value::Error(ErrorKind::Name));
    wb.define_name("Missing", None, "41").unwrap();
    assert_eq!(*wb.value(a(s1, "B3")), n(42.0), "defining a name fixes formulas that used it");
    wb.define_name("Fee", Some(s1), "10").unwrap();
    assert_eq!(*wb.value(a(s1, "B2")), n(20.0), "sheet-local names win");
    assert!(wb.define_name("A1", None, "1").is_err());
    assert!(wb.define_name("R2C3", None, "1").is_err());
    assert!(wb.define_name("Bad Name", None, "1").is_err());
    assert_eq!(wb.input_text(a(s1, "B1")), "=SUM(Amounts)*TaxRate");
    wb.delete_name("TaxRate", None).unwrap();
    assert_eq!(*wb.value(a(s1, "B1")), Value::Error(ErrorKind::Name));
}

#[test]
fn reference_functions_and_intersection() {
    let mut t = T::new();
    for (i, v) in [10, 20, 30, 40].iter().enumerate() {
        t.set(&format!("A{}", i + 1), &v.to_string());
        t.set(&format!("B{}", i + 1), &(v * 2).to_string());
    }
    t.set("D1", "=SUM(OFFSET(A1,1,0,2,1))");
    t.set("D2", "=OFFSET(A1,3,1)");
    t.set("D3", "=SUM(INDIRECT(\"A1:A\"&2))");
    t.set("D4", "=INDIRECT(\"R2C2\",FALSE)");
    t.set("D5", "=SUM(INDEX(A1:B4,0,2))");
    t.set("D6", "=ROW()*10+COLUMN()");
    t.set("D7", "=ROWS(A1:B4)*COLUMNS(A1:B4)");
    t.set("D8", "=ROW(B3)");
    t.set("C3", "=A1:A4*2");
    t.set("D9", "=OFFSET(A1,-1,0)");
    assert_eq!(t.v("D1"), n(50.0));
    assert_eq!(t.v("D2"), n(80.0));
    assert_eq!(t.v("D3"), n(30.0));
    assert_eq!(t.v("D4"), n(40.0));
    assert_eq!(t.v("D5"), n(200.0));
    assert_eq!(t.v("D6"), n(64.0));
    assert_eq!(t.v("D7"), n(8.0));
    assert_eq!(t.v("D8"), n(3.0));
    assert_eq!(t.v("C3"), n(60.0), "implicit intersection picks the formula's row");
    assert_eq!(t.v("D9"), Value::Error(ErrorKind::Ref));
    // OFFSET/INDIRECT are volatile: they see edits to cells they don't name statically.
    t.set("A3", "100");
    assert_eq!(t.v("D1"), n(120.0));
    assert_eq!(t.v("D3"), n(30.0));
}

/// Tiny deterministic PRNG so the property test needs no extra deps.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn random_input(rng: &mut Lcg, rows: u32, cols: u32) -> String {
    let rc = |rng: &mut Lcg| {
        let sheet = if rng.below(4) == 0 { "Other!" } else { "" };
        format!("{sheet}{}{}", xl_engine::col_name(rng.below(cols as u64) as u32), rng.below(rows as u64) + 1)
    };
    match rng.below(10) {
        0 => String::new(),
        1..=3 => format!("{}", rng.below(100) as i64 - 50),
        4 => format!("={}+{}", rc(rng), rc(rng)),
        5 => format!("={}*2-{}", rc(rng), rc(rng)),
        6 => {
            let col = xl_engine::col_name(rng.below(cols as u64) as u32);
            format!("=SUM({col}{}:{col}{})", rng.below(rows as u64) + 1, rng.below(rows as u64) + 1)
        }
        7 => format!("=IF({}>0,{},{})", rc(rng), rc(rng), rng.below(5)),
        8 => format!("=MAX({},{})+COUNT(A1:{})", rc(rng), rc(rng), xl_engine::col_name(rng.below(cols as u64) as u32) + "3"),
        _ => format!("=IFERROR({}/{},0)", rc(rng), rc(rng)),
    }
}

#[test]
fn incremental_matches_full_recalc_across_sheets() {
    const ROWS: u32 = 10;
    const COLS: u32 = 4;
    for seed in 0..200 {
        let mut rng = Lcg(seed * 7919 + 1);
        let mut wb = Workbook::new();
        let other = wb.add_sheet(Some("Other")).unwrap();
        let sheets = [wb.sheet_ids()[0], other];
        let mut inputs = std::collections::BTreeMap::new();
        for step in 0..300 {
            let sheet = sheets[rng.below(2) as usize];
            let at = Addr::new(sheet, CellRef::new(rng.below(ROWS as u64) as u32, rng.below(COLS as u64) as u32));
            let raw = random_input(&mut rng, ROWS, COLS);
            wb.set_input(at, &raw).unwrap();
            inputs.insert(at, raw.clone());
            let mut fresh = Workbook::new();
            fresh.add_sheet(Some("Other")).unwrap();
            fresh.set_inputs(inputs.iter().map(|(a, r)| (*a, r.clone())));
            fresh.recalc_all();
            for &sh in &sheets {
                for r in 0..ROWS {
                    for c in 0..COLS {
                        let x = Addr::new(sh, CellRef::new(r, c));
                        assert_eq!(wb.value(x), fresh.value(x), "seed {seed} step {step}: edited {:?}={raw:?}; mismatch at {:?}", at, x);
                    }
                }
            }
        }
    }
}
