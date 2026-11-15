//! Dynamic arrays: spilling, array functions, LET and LAMBDA.

use xl_engine::{Addr, CellRef, ErrorKind, LoadedCell, Move, Range, SheetId, Shift, Value, Workbook};

struct T {
    wb: Workbook,
    s: SheetId,
}

fn n(x: f64) -> Value {
    Value::Number(x)
}

fn t(s: &str) -> Value {
    Value::text(s)
}

fn r(s: &str) -> Range {
    let (a, b) = s.split_once(':').unwrap();
    Range::new(CellRef::parse(a).unwrap(), CellRef::parse(b).unwrap())
}

impl T {
    fn new() -> Self {
        let wb = Workbook::new();
        let s = wb.sheet_ids()[0];
        Self { wb, s }
    }
    fn a(&self, a1: &str) -> Addr {
        Addr::new(self.s, CellRef::parse(a1).unwrap())
    }
    fn set(&mut self, a1: &str, raw: &str) {
        let a = self.a(a1);
        self.wb.set_input(a, raw).unwrap();
    }
    fn v(&self, a1: &str) -> Value {
        self.wb.value(self.a(a1)).clone()
    }
    /// Values of a block, row by row.
    fn grid(&self, range: &str) -> Vec<Vec<Value>> {
        let rg = r(range);
        (rg.start.row..=rg.end.row).map(|row| (rg.start.col..=rg.end.col).map(|c| self.wb.value(Addr::new(self.s, CellRef::new(row, c))).clone()).collect()).collect()
    }
    fn nums(&self, range: &str) -> Vec<Vec<f64>> {
        self.grid(range).into_iter().map(|row| row.into_iter().map(|v| if let Value::Number(x) = v { x } else { panic!("{range}: {v:?}") }).collect()).collect()
    }
    fn spill(&self, a1: &str) -> Option<Range> {
        self.wb.spill_range(self.a(a1))
    }
    /// Put a formula in Z1 and return its spilled values.
    fn eval(&mut self, formula: &str) -> Vec<Vec<Value>> {
        self.set("Z1", formula);
        match self.spill("Z1") {
            Some(rg) => {
                let (r0, c0) = (rg.start.row, rg.start.col);
                (r0..=rg.end.row).map(|row| (c0..=rg.end.col).map(|c| self.wb.value(Addr::new(self.s, CellRef::new(row, c))).clone()).collect()).collect()
            }
            None => vec![vec![self.v("Z1")]],
        }
    }
    fn col(&mut self, col: &str, first: u32, raws: &[&str]) {
        for (i, raw) in raws.iter().enumerate() {
            self.set(&format!("{col}{}", first + i as u32), raw);
        }
    }
}

fn nv(rows: &[&[f64]]) -> Vec<Vec<Value>> {
    rows.iter().map(|r| r.iter().map(|x| n(*x)).collect()).collect()
}

fn tv(rows: &[&[&str]]) -> Vec<Vec<Value>> {
    rows.iter().map(|r| r.iter().map(|x| t(x)).collect()).collect()
}

#[test]
fn sequence_spills_into_cells_below_and_right() {
    let mut x = T::new();
    x.set("B2", "=SEQUENCE(3,2)");
    assert_eq!(x.nums("B2:C4"), vec![vec![1.0, 2.0], vec![3.0, 4.0], vec![5.0, 6.0]]);
    assert_eq!(x.spill("B2"), Some(r("B2:C4")));
    assert_eq!(x.wb.spill_anchor(x.a("C4")), Some(CellRef::parse("B2").unwrap()));
    assert_eq!(x.wb.spill_anchor(x.a("B2")), Some(CellRef::parse("B2").unwrap()));
    assert_eq!(x.wb.spill_anchor(x.a("D4")), None);
    // Spilled cells aren't inputs.
    assert_eq!(x.wb.input_text(x.a("C3")), "");
    assert_eq!(x.wb.input_text(x.a("B2")), "=SEQUENCE(3,2)");
    // Shrinking clears the cells no longer covered.
    x.set("B2", "=SEQUENCE(2)");
    assert_eq!(x.grid("B2:C4"), vec![vec![n(1.0), Value::Empty], vec![n(2.0), Value::Empty], vec![Value::Empty, Value::Empty]]);
    // Replacing the formula removes the spill.
    x.set("B2", "7");
    assert_eq!(x.v("B3"), Value::Empty);
    assert_eq!(x.spill("B2"), None);
}

#[test]
fn blocked_spill_shows_spill_error_until_cleared() {
    let mut x = T::new();
    x.set("A3", "x");
    x.set("A1", "=SEQUENCE(4)");
    assert_eq!(x.v("A1"), Value::Error(ErrorKind::Spill));
    assert_eq!(x.v("A2"), Value::Empty);
    assert_eq!(x.wb.blocked_spill(x.a("A1")), Some(r("A1:A4")));
    x.set("A3", "");
    assert_eq!(x.nums("A1:A4"), vec![vec![1.0], vec![2.0], vec![3.0], vec![4.0]]);
    // Typing into the spill range blocks it again.
    x.set("A4", "5");
    assert_eq!(x.v("A1"), Value::Error(ErrorKind::Spill));
    assert_eq!(x.v("A2"), Value::Empty);
    assert_eq!(x.v("A4"), n(5.0));
    // Clearing a spilled cell changes nothing.
    x.set("A4", "");
    x.set("A2", "");
    assert_eq!(x.nums("A1:A4"), vec![vec![1.0], vec![2.0], vec![3.0], vec![4.0]]);
    // Another spill in the way blocks too, and merged cells do.
    x.set("C1", "=SEQUENCE(1,3)");
    x.set("B1", "=SEQUENCE(1,2)");
    assert_eq!(x.v("B1"), Value::Error(ErrorKind::Spill));
    x.set("C1", "");
    assert_eq!(x.nums("B1:C1"), vec![vec![1.0, 2.0]]);
    x.wb.merge(x.s, r("F2:G2"));
    x.set("F1", "=SEQUENCE(3)");
    assert_eq!(x.v("F1"), Value::Error(ErrorKind::Spill));
    // Merging cells a spill covers blocks it.
    x.set("H1", "=SEQUENCE(3)");
    x.wb.merge(x.s, r("H2:I2"));
    assert_eq!(x.v("H1"), Value::Error(ErrorKind::Spill));
    assert_eq!(x.v("H3"), Value::Empty);
}

#[test]
fn spilled_values_take_part_in_dependencies() {
    let mut x = T::new();
    x.set("A1", "3");
    x.set("B1", "=SEQUENCE(A1)");
    x.set("D1", "=B3*10");
    x.set("D2", "=SUM(B1:B10)");
    x.set("D3", "=B5");
    x.set("D4", "=SUM(B1#)");
    x.set("D5", "=ROWS(B1#)");
    assert_eq!(x.v("D1"), n(30.0));
    assert_eq!(x.v("D2"), n(6.0));
    assert_eq!(x.v("D3"), Value::Empty);
    assert_eq!(x.v("D4"), n(6.0));
    assert_eq!(x.v("D5"), n(3.0));
    // Growing reaches cells the readers referenced but the spill didn't cover.
    x.set("A1", "5");
    assert_eq!(x.v("D2"), n(15.0));
    assert_eq!(x.v("D3"), n(5.0));
    assert_eq!(x.v("D4"), n(15.0));
    assert_eq!(x.v("D5"), n(5.0));
    // Shrinking too.
    x.set("A1", "2");
    assert_eq!(x.v("D1"), n(0.0));
    assert_eq!(x.v("D2"), n(3.0));
    assert_eq!(x.v("D3"), Value::Empty);
    // Blocking makes A1# an error.
    x.set("B2", "x");
    assert_eq!(x.v("D4"), Value::Error(ErrorKind::Ref));
    // A chain of spills.
    let mut y = T::new();
    y.col("A", 1, &["3", "1", "2"]);
    y.set("B1", "=SORT(A1:A3)");
    y.set("C1", "=B1#*10");
    y.set("D1", "=SUM(C1#)");
    assert_eq!(y.nums("C1:C3"), vec![vec![10.0], vec![20.0], vec![30.0]]);
    y.set("A4", "9");
    y.set("B1", "=SORT(A1:A4)");
    assert_eq!(y.nums("C1:C4"), vec![vec![10.0], vec![20.0], vec![30.0], vec![90.0]]);
    assert_eq!(y.v("D1"), n(150.0));
    // The incremental results match a full recalc.
    let before = y.grid("A1:D5");
    y.wb.recalc_all();
    assert_eq!(y.grid("A1:D5"), before);
}

#[test]
fn reading_its_own_spill_range_is_circular() {
    let mut x = T::new();
    x.set("A1", "=SEQUENCE(3)+SUM(A2:A3)");
    assert_eq!(x.v("A1"), Value::Error(ErrorKind::Circ));
}

#[test]
fn spill_references_follow_structural_edits() {
    let mut x = T::new();
    x.set("B2", "=SEQUENCE(3)");
    x.set("D1", "=SUM(B2#)");
    assert_eq!(x.v("D1"), n(6.0));
    x.wb.apply_shift(Shift::InsertRows { sheet: x.s, at: 0, count: 2 });
    assert_eq!(x.wb.input_text(x.a("D3")), "=SUM(B4#)");
    assert_eq!(x.v("D3"), n(6.0));
    assert_eq!(x.spill("B4"), Some(r("B4:B6")));
    x.wb.apply_shift(Shift::InsertCols { sheet: x.s, at: 0, count: 1 });
    assert_eq!(x.wb.input_text(x.a("E3")), "=SUM(C4#)");
    assert_eq!(x.v("E3"), n(6.0));
    x.wb.apply_shift(Shift::DeleteRows { sheet: x.s, at: 0, count: 1 });
    assert_eq!(x.wb.input_text(x.a("E2")), "=SUM(C3#)");
    assert_eq!(x.nums("C3:C5"), vec![vec![1.0], vec![2.0], vec![3.0]]);
    // Moving the anchor carries the spill and the reference along.
    x.wb.move_range(Move { from_sheet: x.s, from: Range::single(CellRef::parse("C3").unwrap()), to_sheet: x.s, dr: 0, dc: 3, });
    assert_eq!(x.wb.input_text(x.a("E2")), "=SUM(F3#)");
    assert_eq!(x.v("E2"), n(6.0));
    assert_eq!(x.v("C4"), Value::Empty);
    assert_eq!(x.nums("F3:F5"), vec![vec![1.0], vec![2.0], vec![3.0]]);
    // Deleting the anchor's row breaks the reference.
    x.wb.apply_shift(Shift::DeleteRows { sheet: x.s, at: 2, count: 1 });
    assert_eq!(x.wb.input_text(x.a("E2")), "=SUM(#REF!)");
}

#[test]
fn operators_broadcast_over_arrays() {
    let mut x = T::new();
    x.col("A", 1, &["1", "2", "3"]);
    assert_eq!(x.eval("=A1:A3*2"), nv(&[&[2.0], &[4.0], &[6.0]]));
    assert_eq!(x.eval("=A1:A3&\"x\""), tv(&[&["1x"], &["2x"], &["3x"]]));
    assert_eq!(x.eval("=-A1:A2"), nv(&[&[-1.0], &[-2.0]]));
    assert_eq!(x.eval("=A1:A2+SEQUENCE(1,3)"), nv(&[&[2.0, 3.0, 4.0], &[3.0, 4.0, 5.0]]));
    assert_eq!(x.eval("=A1:A3>1"), vec![vec![Value::Bool(false)], vec![Value::Bool(true)], vec![Value::Bool(true)]]);
    assert_eq!(x.eval("=A1:A3"), nv(&[&[1.0], &[2.0], &[3.0]]));
    // Mismatched sizes give #N/A past the smaller one.
    assert_eq!(x.eval("=A1:A3+SEQUENCE(2)"), vec![vec![n(2.0)], vec![n(4.0)], vec![Value::Error(ErrorKind::NA)]]);
    // Functions taking one value apply to each.
    assert_eq!(x.eval("=LEN(A1:A2&\"ab\")"), nv(&[&[3.0], &[3.0]]));
    assert_eq!(x.eval("=ROUND(A1:A2/3,2)"), nv(&[&[0.33], &[0.67]]));
    assert_eq!(x.eval("=IF(A1:A3>1,\"big\",\"small\")"), tv(&[&["small"], &["big"], &["big"]]));
    assert_eq!(x.eval("=ROW(A1:A3)"), nv(&[&[1.0], &[2.0], &[3.0]]));
    assert_eq!(x.eval("=COUNTIF(A1:A3,A1:A2)"), nv(&[&[1.0], &[1.0]]));
    // Aggregates and SUMPRODUCT read arrays.
    assert_eq!(x.eval("=SUM(A1:A3*2)"), nv(&[&[12.0]]));
    assert_eq!(x.eval("=SUMPRODUCT((A1:A3>1)*A1:A3)"), nv(&[&[5.0]]));
    assert_eq!(x.eval("=MAX(SEQUENCE(4))+COUNT(SEQUENCE(2,2))"), nv(&[&[8.0]]));
    assert_eq!(x.eval("=TEXTJOIN(\"-\",TRUE,SEQUENCE(3))"), tv(&[&["1-2-3"]]));
    assert_eq!(x.eval("=INDEX(SEQUENCE(3,3),2,3)"), nv(&[&[6.0]]));
    assert_eq!(x.eval("=INDEX(SEQUENCE(3,3),0,2)"), nv(&[&[2.0], &[5.0], &[8.0]]));
    assert_eq!(x.eval("=MATCH(5,SEQUENCE(9),0)"), nv(&[&[5.0]]));
    assert_eq!(x.eval("=VLOOKUP(2,HSTACK(A1:A3,A1:A3*10),2,FALSE)"), nv(&[&[20.0]]));
    // @ takes one value.
    assert_eq!(x.eval("=@SEQUENCE(3)"), nv(&[&[1.0]]));
}

#[test]
fn filter_sort_unique() {
    let mut x = T::new();
    x.col("A", 1, &["b", "a", "c", "a"]);
    x.col("B", 1, &["3", "1", "2", "5"]);
    assert_eq!(x.eval("=FILTER(A1:B4,B1:B4>1)"), vec![vec![t("b"), n(3.0)], vec![t("c"), n(2.0)], vec![t("a"), n(5.0)]]);
    assert_eq!(x.eval("=FILTER(A1:A4,B1:B4>9)"), vec![vec![Value::Error(ErrorKind::Calc)]]);
    assert_eq!(x.eval("=FILTER(A1:A4,B1:B4>9,\"none\")"), tv(&[&["none"]]));
    assert_eq!(x.eval("=FILTER(SEQUENCE(1,4),{1,0,1,1}=1)"), nv(&[&[1.0, 3.0, 4.0]]));
    assert_eq!(x.eval("={1,2;3,4}*{10;100}"), nv(&[&[10.0, 20.0], &[300.0, 400.0]]));
    assert_eq!(x.eval("=SUM({1,2,3})"), nv(&[&[6.0]]));
    assert_eq!(x.eval("={\"a\",-1,TRUE}"), vec![vec![t("a"), n(-1.0), Value::Bool(true)]]);
    assert_eq!(x.eval("=FILTER(SEQUENCE(1,4),SEQUENCE(1,4)>2)"), nv(&[&[3.0, 4.0]]));
    assert_eq!(x.eval("=SORT(B1:B4)"), nv(&[&[1.0], &[2.0], &[3.0], &[5.0]]));
    assert_eq!(x.eval("=SORT(B1:B4,,-1)"), nv(&[&[5.0], &[3.0], &[2.0], &[1.0]]));
    assert_eq!(x.eval("=SORT(A1:B4,2,-1)"), vec![vec![t("a"), n(5.0)], vec![t("b"), n(3.0)], vec![t("c"), n(2.0)], vec![t("a"), n(1.0)]]);
    assert_eq!(x.eval("=SORT(SEQUENCE(1,3),1,-1,TRUE)"), nv(&[&[3.0, 2.0, 1.0]]));
    assert_eq!(x.eval("=SORTBY(A1:A4,B1:B4)"), tv(&[&["a"], &["c"], &["b"], &["a"]]));
    assert_eq!(x.eval("=SORTBY(A1:A4,A1:A4,1,B1:B4,-1)"), tv(&[&["a"], &["a"], &["b"], &["c"]]));
    assert_eq!(x.eval("=UNIQUE(A1:A4)"), tv(&[&["b"], &["a"], &["c"]]));
    assert_eq!(x.eval("=UNIQUE(A1:A4,,TRUE)"), tv(&[&["b"], &["c"]]));
    assert_eq!(x.eval("=SORT(UNIQUE(A1:A4))"), tv(&[&["a"], &["b"], &["c"]]));
    assert_eq!(x.eval("=ROWS(UNIQUE(A1:A4))"), nv(&[&[3.0]]));
    assert_eq!(x.eval("=UNIQUE(HSTACK(\"A\",\"a\",\"b\"),TRUE)"), tv(&[&["A", "b"]]));
}

#[test]
fn sequence_randarray_transpose() {
    let mut x = T::new();
    assert_eq!(x.eval("=SEQUENCE(2,3,10,-2)"), nv(&[&[10.0, 8.0, 6.0], &[4.0, 2.0, 0.0]]));
    assert_eq!(x.eval("=SEQUENCE(0)"), vec![vec![Value::Error(ErrorKind::Calc)]]);
    assert_eq!(x.eval("=TRANSPOSE(SEQUENCE(2,3))"), nv(&[&[1.0, 4.0], &[2.0, 5.0], &[3.0, 6.0]]));
    let g = x.eval("=RANDARRAY(4,3,5,7,TRUE)");
    assert_eq!((g.len(), g[0].len()), (4, 3));
    assert!(g.iter().flatten().all(|v| matches!(v, Value::Number(n) if (5.0..=7.0).contains(n) && n.fract() == 0.0)), "{g:?}");
    let g = x.eval("=RANDARRAY(2)");
    assert!(g.iter().flatten().all(|v| matches!(v, Value::Number(n) if (0.0..1.0).contains(n))));
    x.col("A", 1, &["1", "2"]);
    x.set("B1", "=TRANSPOSE(A1:A2)");
    assert_eq!(x.nums("B1:C1"), vec![vec![1.0, 2.0]]);
}

#[test]
fn reshaping_functions() {
    let mut x = T::new();
    assert_eq!(x.eval("=TEXTSPLIT(\"a,b;c,d\",\",\",\";\")"), tv(&[&["a", "b"], &["c", "d"]]));
    assert_eq!(x.eval("=TEXTSPLIT(\"a,b;c\",\",\",\";\",FALSE,0,\"-\")"), tv(&[&["a", "b"], &["c", "-"]]));
    assert_eq!(x.eval("=TEXTSPLIT(\"a,,b\",\",\",,TRUE)"), tv(&[&["a", "b"]]));
    assert_eq!(x.eval("=TEXTSPLIT(\"aXbxc\",\"x\",,,1)"), tv(&[&["a", "b", "c"]]));
    assert_eq!(x.eval("=TEXTSPLIT(\"a b-c\",HSTACK(\" \",\"-\"))"), tv(&[&["a", "b", "c"]]));
    assert_eq!(x.eval("=VSTACK(SEQUENCE(1,2),SEQUENCE(1,3))"), vec![vec![n(1.0), n(2.0), Value::Error(ErrorKind::NA)], vec![n(1.0), n(2.0), n(3.0)]]);
    assert_eq!(x.eval("=HSTACK(SEQUENCE(2),7)"), vec![vec![n(1.0), n(7.0)], vec![n(2.0), Value::Error(ErrorKind::NA)]]);
    assert_eq!(x.eval("=TAKE(SEQUENCE(4,3),2)"), nv(&[&[1.0, 2.0, 3.0], &[4.0, 5.0, 6.0]]));
    assert_eq!(x.eval("=TAKE(SEQUENCE(4,3),-1,-2)"), nv(&[&[11.0, 12.0]]));
    assert_eq!(x.eval("=TAKE(SEQUENCE(4,3),,1)"), nv(&[&[1.0], &[4.0], &[7.0], &[10.0]]));
    assert_eq!(x.eval("=DROP(SEQUENCE(4,3),3)"), nv(&[&[10.0, 11.0, 12.0]]));
    assert_eq!(x.eval("=DROP(SEQUENCE(2,3),-1,1)"), nv(&[&[2.0, 3.0]]));
    assert_eq!(x.eval("=DROP(SEQUENCE(2),5)"), vec![vec![Value::Error(ErrorKind::Calc)]]);
    assert_eq!(x.eval("=CHOOSECOLS(SEQUENCE(2,3),3,1)"), nv(&[&[3.0, 1.0], &[6.0, 4.0]]));
    assert_eq!(x.eval("=CHOOSECOLS(SEQUENCE(2,3),-1)"), nv(&[&[3.0], &[6.0]]));
    assert_eq!(x.eval("=CHOOSEROWS(SEQUENCE(3,2),2)"), nv(&[&[3.0, 4.0]]));
    assert_eq!(x.eval("=CHOOSEROWS(SEQUENCE(3,2),4)"), vec![vec![Value::Error(ErrorKind::Value)]]);
    assert_eq!(x.eval("=TOCOL(SEQUENCE(2,2))"), nv(&[&[1.0], &[2.0], &[3.0], &[4.0]]));
    assert_eq!(x.eval("=TOCOL(SEQUENCE(2,2),,TRUE)"), nv(&[&[1.0], &[3.0], &[2.0], &[4.0]]));
    assert_eq!(x.eval("=TOROW(SEQUENCE(2,2))"), nv(&[&[1.0, 2.0, 3.0, 4.0]]));
    x.col("A", 1, &["1", "", "3"]);
    assert_eq!(x.eval("=TOROW(A1:A3,1)"), nv(&[&[1.0, 3.0]]));
    assert_eq!(x.eval("=WRAPROWS(SEQUENCE(5),2)"), vec![vec![n(1.0), n(2.0)], vec![n(3.0), n(4.0)], vec![n(5.0), Value::Error(ErrorKind::NA)]]);
    assert_eq!(x.eval("=WRAPCOLS(SEQUENCE(1,5),2,0)"), nv(&[&[1.0, 3.0, 5.0], &[2.0, 4.0, 0.0]]));
    assert_eq!(x.eval("=EXPAND(SEQUENCE(1,2),2,3,0)"), nv(&[&[1.0, 2.0, 0.0], &[0.0, 0.0, 0.0]]));
    assert_eq!(x.eval("=EXPAND(SEQUENCE(2),1)"), vec![vec![Value::Error(ErrorKind::Value)]]);
}

#[test]
fn xlookup_returns_rows() {
    let mut x = T::new();
    x.col("A", 1, &["a", "b", "c"]);
    x.col("B", 1, &["1", "2", "3"]);
    x.col("C", 1, &["10", "20", "30"]);
    assert_eq!(x.eval("=XLOOKUP(\"b\",A1:A3,B1:C3)"), nv(&[&[2.0, 20.0]]));
    assert_eq!(x.eval("=XLOOKUP(\"c\",A1:A3,C1:C3)"), nv(&[&[30.0]]));
    assert_eq!(x.eval("=XLOOKUP(HSTACK(\"c\",\"a\"),A1:A3,B1:B3)"), nv(&[&[3.0, 1.0]]));
    assert_eq!(x.eval("=XLOOKUP(\"z\",A1:A3,B1:B3,\"-\")"), tv(&[&["-"]]));
    assert_eq!(x.eval("=XLOOKUP(2,SEQUENCE(3),SEQUENCE(3)*5)"), nv(&[&[10.0]]));
    assert_eq!(x.eval("=SUM(XLOOKUP(\"a\",A1:A3,B1:C3))"), nv(&[&[11.0]]));
}

#[test]
fn let_names_are_scoped() {
    let mut x = T::new();
    x.set("A1", "4");
    assert_eq!(x.eval("=LET(a,2,b,a*3,a+b)"), nv(&[&[8.0]]));
    assert_eq!(x.eval("=LET(x,A1,x*x)"), nv(&[&[16.0]]));
    assert_eq!(x.eval("=LET(r,SEQUENCE(3),SUM(r)+ROWS(r))"), nv(&[&[9.0]]));
    assert_eq!(x.eval("=LET(r,SEQUENCE(2),r*10)"), nv(&[&[10.0], &[20.0]]));
    // Inner names shadow outer ones; names don't leak.
    assert_eq!(x.eval("=LET(a,1,LET(a,2,a)+a)"), nv(&[&[3.0]]));
    assert_eq!(x.eval("=LET(a,1,a)+a"), vec![vec![Value::Error(ErrorKind::Name)]]);
    // A LET name shadows a defined name.
    x.wb.define_name("Rate", None, "0.5").unwrap();
    assert_eq!(x.eval("=LET(Rate,2,Rate)+Rate"), nv(&[&[2.5]]));
    assert_eq!(x.eval("=LET(a,1,b)"), vec![vec![Value::Error(ErrorKind::Name)]]);
    assert_eq!(x.eval("=LET(a,1)"), vec![vec![Value::Error(ErrorKind::Value)]]);
    // A range bound to a name still acts as a reference.
    x.set("B1", "=LET(r,A1:A2,ROWS(r)+SUM(r))");
    assert_eq!(x.v("B1"), n(6.0));
    x.set("A2", "10");
    assert_eq!(x.v("B1"), n(16.0), "dependencies are tracked through LET");
}

#[test]
fn lambdas_inline_named_and_helpers() {
    let mut x = T::new();
    assert_eq!(x.eval("=LAMBDA(x,x+1)(5)"), nv(&[&[6.0]]));
    assert_eq!(x.eval("=LAMBDA(x,y,x*y)(3,4)"), nv(&[&[12.0]]));
    assert_eq!(x.eval("=LAMBDA(x,x+1)"), vec![vec![Value::Error(ErrorKind::Calc)]], "a bare lambda is #CALC!");
    assert_eq!(x.eval("=LAMBDA(x,x)(1,2)"), vec![vec![Value::Error(ErrorKind::Value)]]);
    assert_eq!(x.eval("=LET(f,LAMBDA(n,n*2),f(21))"), nv(&[&[42.0]]));
    // Closures see the LET names around them.
    assert_eq!(x.eval("=LET(k,10,f,LAMBDA(n,n+k),f(1))"), nv(&[&[11.0]]));
    assert_eq!(x.eval("=LAMBDA(a,[b],IF(ISOMITTED(b),a,a+b))(1)"), vec![vec![Value::Error(ErrorKind::Value)]], "brackets aren't syntax");
    assert_eq!(x.eval("=LAMBDA(a,b,IF(ISOMITTED(b),a,a+b))(1)"), nv(&[&[1.0]]));
    assert_eq!(x.eval("=LAMBDA(a,b,IF(ISOMITTED(b),a,a+b))(1,2)"), nv(&[&[3.0]]));
    assert_eq!(x.eval("=LAMBDA(a,b,ISOMITTED(b))(1,)"), vec![vec![Value::Bool(true)]]);
    // Named lambdas.
    x.wb.define_name("Double", None, "=LAMBDA(x, x*2)").unwrap();
    x.wb.define_name("Fact", None, "=LAMBDA(n, IF(n<=1, 1, n*Fact(n-1)))").unwrap();
    assert_eq!(x.eval("=Double(3)"), nv(&[&[6.0]]));
    assert_eq!(x.eval("=Double(SEQUENCE(2))"), nv(&[&[2.0], &[4.0]]));
    assert_eq!(x.eval("=Fact(5)"), nv(&[&[120.0]]));
    assert_eq!(x.eval("=Nope(5)"), vec![vec![Value::Error(ErrorKind::Name)]]);
    assert_eq!(x.wb.input_text(x.a("Z1")), "=Nope(5)");
    // Redefining the name recalculates its users.
    x.set("A1", "=Double(10)");
    x.wb.define_name("Double", None, "=LAMBDA(x, x*3)").unwrap();
    assert_eq!(x.v("A1"), n(30.0));
    // Helpers.
    assert_eq!(x.eval("=MAP(SEQUENCE(3),LAMBDA(v,v*v))"), nv(&[&[1.0], &[4.0], &[9.0]]));
    assert_eq!(x.eval("=MAP(SEQUENCE(2),SEQUENCE(2)*10,LAMBDA(a,b,a+b))"), nv(&[&[11.0], &[22.0]]));
    assert_eq!(x.eval("=REDUCE(0,SEQUENCE(4),LAMBDA(acc,v,acc+v))"), nv(&[&[10.0]]));
    assert_eq!(x.eval("=REDUCE(,SEQUENCE(3),LAMBDA(acc,v,acc+v))"), nv(&[&[6.0]]));
    assert_eq!(x.eval("=REDUCE(1,SEQUENCE(3),LAMBDA(a,v,VSTACK(a,v)))"), nv(&[&[1.0], &[1.0], &[2.0], &[3.0]]));
    assert_eq!(x.eval("=SCAN(0,SEQUENCE(1,4),LAMBDA(acc,v,acc+v))"), nv(&[&[1.0, 3.0, 6.0, 10.0]]));
    assert_eq!(x.eval("=BYROW(SEQUENCE(2,3),LAMBDA(r,SUM(r)))"), nv(&[&[6.0], &[15.0]]));
    assert_eq!(x.eval("=BYCOL(SEQUENCE(2,3),LAMBDA(c,MAX(c)))"), nv(&[&[4.0, 5.0, 6.0]]));
    assert_eq!(x.eval("=MAKEARRAY(2,3,LAMBDA(r,c,r*10+c))"), nv(&[&[11.0, 12.0, 13.0], &[21.0, 22.0, 23.0]]));
    assert_eq!(x.eval("=MAP(SEQUENCE(2),LAMBDA(v,SEQUENCE(v)))"), vec![vec![n(1.0)], vec![Value::Error(ErrorKind::Calc)]]);
    // Runaway recursion stops.
    x.wb.define_name("Loop", None, "=LAMBDA(n, Loop(n+1))").unwrap();
    assert!(matches!(x.eval("=Loop(1)")[0][0], Value::Error(_)));
}

#[test]
fn formulas_print_back_and_save_with_prefixes() {
    let mut x = T::new();
    for f in ["={1,2;3,4}", "=SUM(A1#)", "=@A1:A3", "=LAMBDA(x,x+1)(5)", "=LET(a,1,a*2)", "=IF(A1,,2)", "=@(A1:A3*2)", "=Sheet1!B2#"] {
        x.set("H1", f);
        assert_eq!(x.wb.input_text(x.a("H1")), f);
    }
    x.set("H1", "=_xlfn._xlws.SORT(_xlfn.ANCHORARRAY(B1))+_xlfn.SINGLE(C1:C3)");
    assert_eq!(x.wb.input_text(x.a("H1")), "=SORT(B1#)+@C1:C3");
    assert_eq!(x.wb.formula_source_xlsx(x.a("H1")).unwrap(), "_xlfn._xlws.SORT(_xlfn.ANCHORARRAY(B1))+_xlfn.SINGLE(C1:C3)");
    x.set("H1", "=_xlfn.LET(_xlpm.a,1,_xlfn.LAMBDA(_xlpm.x,_xlpm.x+_xlpm.a)(2))");
    assert_eq!(x.wb.input_text(x.a("H1")), "=LET(a,1,LAMBDA(x,x+a)(2))");
    assert_eq!(x.wb.formula_source_xlsx(x.a("H1")).unwrap(), "_xlfn.LET(_xlpm.a,1,_xlfn.LAMBDA(_xlpm.x,_xlpm.x+_xlpm.a)(2))");
    assert_eq!(x.v("H1"), n(3.0));
    x.set("H1", "=LET(f,LAMBDA(v,v*2),f(4))");
    assert_eq!(x.wb.formula_source_xlsx(x.a("H1")).unwrap(), "_xlfn.LET(_xlpm.f,_xlfn.LAMBDA(_xlpm.v,_xlpm.v*2),_xlpm.f(4))");
    assert!(x.wb.is_dynamic_formula(x.a("H1")));
    // Plain formulas stay plain; array arithmetic makes a formula dynamic.
    x.set("H2", "=SUM(A1:A3)+CONCAT(B1,C1)");
    assert!(!x.wb.is_dynamic_formula(x.a("H2")));
    assert_eq!(x.wb.formula_source_xlsx(x.a("H2")).unwrap(), "SUM(A1:A3)+_xlfn.CONCAT(B1,C1)");
    x.set("H3", "=SUM(A1:A3*2)");
    assert!(x.wb.is_dynamic_formula(x.a("H3")));
}

#[test]
fn legacy_formulas_get_implicit_intersection() {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let a = |a1: &str| Addr::new(s, CellRef::parse(a1).unwrap());
    for (i, v) in [1.0, 2.0, 3.0].iter().enumerate() {
        wb.load_cell(a(&format!("A{}", i + 1)), LoadedCell::Value(n(*v)));
    }
    wb.load_cell(a("B2"), LoadedCell::LegacyFormula { src: "A1:A3*2".into(), cached: n(4.0) });
    wb.load_cell(a("B3"), LoadedCell::LegacyFormula { src: "SUM(A1:A3)+SUMPRODUCT(A1:A3*A1:A3)".into(), cached: n(20.0) });
    wb.load_cell(a("B4"), LoadedCell::Formula { src: "A1:A3*2".into(), cached: n(2.0) });
    wb.load_cell(a("C1"), LoadedCell::Value(n(0.0)));
    wb.recalc_all();
    assert_eq!(wb.input_text(a("B2")), "=@A1:A3*2");
    assert_eq!(*wb.value(a("B2")), n(4.0));
    assert_eq!(wb.formula_source_xlsx(a("B2")).unwrap(), "A1:A3*2");
    assert!(!wb.is_dynamic_formula(a("B2")));
    assert_eq!(wb.input_text(a("B3")), "=SUM(A1:A3)+SUMPRODUCT(A1:A3*A1:A3)");
    assert_eq!(*wb.value(a("B3")), n(20.0));
    // A dynamic-array formula from a file spills.
    assert_eq!(wb.spill_range(a("B4")), Some(r("B4:B6")));
    assert!(wb.is_dynamic_formula(a("B4")));
}

#[test]
fn loaded_spills_keep_cached_values_and_recalc() {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let a = |a1: &str| Addr::new(s, CellRef::parse(a1).unwrap());
    wb.load_cell(a("A1"), LoadedCell::Formula { src: "SEQUENCE(3)".into(), cached: n(1.0) });
    wb.load_cell(a("A2"), LoadedCell::Value(n(2.0)));
    wb.load_cell(a("A3"), LoadedCell::Value(n(3.0)));
    wb.load_cell(a("B1"), LoadedCell::Formula { src: "SUM(A1#)".into(), cached: n(6.0) });
    wb.load_spill(s, CellRef::parse("A1").unwrap(), r("A1:A3"));
    assert_eq!(wb.spill_anchor(a("A3")), Some(CellRef::parse("A1").unwrap()));
    assert_eq!(wb.input_text(a("A2")), "");
    wb.recalc_all();
    assert_eq!(wb.spill_range(a("A1")), Some(r("A1:A3")));
    assert_eq!(*wb.value(a("B1")), n(6.0));
    // Named lambdas used by loaded formulas aren't "unknown functions".
    wb.load_name("Twice", None, "_xlfn.LAMBDA(_xlpm.x,_xlpm.x*2)");
    wb.load_cell(a("C1"), LoadedCell::Formula { src: "Twice(4)".into(), cached: n(8.0) });
    wb.load_cell(a("C2"), LoadedCell::Formula { src: "Unknown(4)".into(), cached: n(9.0) });
    wb.recalc_all();
    assert_eq!(*wb.value(a("C1")), n(8.0));
    assert_eq!(wb.input_text(a("C1")), "=Twice(4)");
    assert_eq!(*wb.value(a("C2")), n(9.0), "opaque formula keeps its cached value");
}
