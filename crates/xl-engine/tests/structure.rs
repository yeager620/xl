//! Inserting and deleting rows and columns.

use xl_engine::{Addr, CellRef, ErrorKind, Shift, Value, Workbook};

fn a(s: u32, a1: &str) -> Addr {
    Addr::new(s, CellRef::parse(a1).unwrap())
}

fn n(x: f64) -> Value {
    Value::Number(x)
}

fn book(cells: &[(&str, &str)]) -> (Workbook, u32) {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    wb.set_inputs(cells.iter().map(|(c, v)| (a(s, c), v.to_string())));
    (wb, s)
}

#[test]
fn insert_rows_moves_cells_and_adjusts_references() {
    let (mut wb, s) = book(&[("A1", "1"), ("A2", "2"), ("A3", "3"), ("B1", "=SUM(A1:A3)"), ("B2", "=A3*10"), ("B3", "=$A$2")]);
    wb.apply_shift(Shift::InsertRows { sheet: s, at: 1, count: 2 }); // before row 2
    assert_eq!(*wb.value(a(s, "A4")), n(2.0));
    assert_eq!(*wb.value(a(s, "A2")), Value::Empty);
    assert_eq!(wb.input_text(a(s, "B1")), "=SUM(A1:A5)", "range grows around the insertion");
    assert_eq!(wb.input_text(a(s, "B4")), "=A5*10");
    assert_eq!(wb.input_text(a(s, "B5")), "=$A$4", "absolute refs move too");
    assert_eq!(*wb.value(a(s, "B1")), n(6.0));
    wb.set_input(a(s, "A2"), "100").unwrap();
    assert_eq!(*wb.value(a(s, "B1")), n(106.0), "new rows inside the range are summed");
}

#[test]
fn insertion_at_range_edges() {
    let (mut wb, s) = book(&[("A2", "1"), ("A3", "2"), ("C1", "=SUM(A2:A3)")]);
    wb.apply_shift(Shift::InsertRows { sheet: s, at: 3, count: 1 }); // just after the range
    assert_eq!(wb.input_text(a(s, "C1")), "=SUM(A2:A3)");
    wb.apply_shift(Shift::InsertRows { sheet: s, at: 1, count: 1 }); // at the range start
    assert_eq!(wb.input_text(a(s, "C1")), "=SUM(A3:A4)");
}

#[test]
fn delete_rows_shrinks_ranges_and_breaks_direct_refs() {
    let (mut wb, s) = book(&[("A1", "1"), ("A2", "2"), ("A3", "3"), ("A4", "4"), ("B1", "=SUM(A1:A4)"), ("B5", "=A2+A4"), ("B6", "=SUM(A2:A3)")]);
    wb.apply_shift(Shift::DeleteRows { sheet: s, at: 1, count: 2 }); // rows 2-3
    assert_eq!(wb.input_text(a(s, "B1")), "=SUM(A1:A2)");
    assert_eq!(*wb.value(a(s, "B1")), n(5.0));
    assert_eq!(wb.input_text(a(s, "B3")), "=#REF!+A2");
    assert_eq!(*wb.value(a(s, "B3")), Value::Error(ErrorKind::Ref));
    assert_eq!(wb.input_text(a(s, "B4")), "=SUM(#REF!)", "a range entirely deleted is #REF!");
}

#[test]
fn columns_other_sheets_names_formats() {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let other = wb.add_sheet(Some("Other")).unwrap();
    wb.set_inputs([(a(s, "A1"), "1".into()), (a(s, "B1"), "2".into()), (a(s, "C1"), "3".into()), (a(s, "D1"), "=SUM(A1:C1)".into())]);
    wb.set_input(a(other, "A1"), "=Sheet1!C1*2").unwrap();
    wb.set_input(a(other, "A2"), "=C1").unwrap(); // Other!C1: a different sheet, must not move
    wb.define_name("Third", None, "Sheet1!$C$1").unwrap();
    wb.set_input(a(other, "A3"), "=Third").unwrap();
    wb.set_format(a(s, "C1"), Some("0.00"));
    wb.set_col_width(s, 2, Some(150.0));
    wb.apply_shift(Shift::InsertCols { sheet: s, at: 1, count: 1 });
    assert_eq!(wb.input_text(a(s, "E1")), "=SUM(A1:D1)");
    assert_eq!(wb.input_text(a(other, "A1")), "=Sheet1!D1*2");
    assert_eq!(wb.input_text(a(other, "A2")), "=C1");
    assert_eq!(wb.format_code(a(s, "D1")), Some("0.00"), "formats move with their cells");
    assert_eq!(wb.col_width(s, 3), Some(150.0), "widths move with their columns");
    wb.set_input(a(s, "D1"), "30").unwrap();
    assert_eq!(*wb.value(a(other, "A3")), n(30.0), "names follow the moved cell");
    wb.apply_shift(Shift::DeleteCols { sheet: s, at: 3, count: 1 });
    assert_eq!(*wb.value(a(other, "A3")), Value::Error(ErrorKind::Ref));
    assert_eq!(wb.input_text(a(other, "A1")), "=#REF!*2");
}

#[test]
fn whole_column_ranges_ignore_row_changes() {
    let (mut wb, s) = book(&[("A5", "5"), ("B1", "=SUM(A:A)")]);
    wb.apply_shift(Shift::DeleteRows { sheet: s, at: 0, count: 1 });
    assert_eq!(wb.input_text(a(s, "B1")), "", "the formula's own row was deleted");
    assert_eq!(*wb.value(a(s, "A4")), n(5.0));
    wb.set_input(a(s, "C1"), "=SUM(A:A)").unwrap();
    wb.apply_shift(Shift::InsertRows { sheet: s, at: 0, count: 3 });
    assert_eq!(wb.input_text(a(s, "C4")), "=SUM(A:A)");
    assert_eq!(*wb.value(a(s, "C4")), n(5.0));
}

/// Tiny deterministic PRNG.
struct Lcg(u64);
impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

/// Inserting lines and then deleting the same lines restores every formula
/// and value exactly (on the shifted sheet and on others).
#[test]
fn insert_then_delete_is_identity() {
    for seed in 0..60 {
        let mut rng = Lcg(seed * 31 + 7);
        let mut wb = Workbook::new();
        let s = wb.sheet_ids()[0];
        let other = wb.add_sheet(Some("Other")).unwrap();
        let mut items = Vec::new();
        for _ in 0..60 {
            let sheet = if rng.below(3) == 0 { other } else { s };
            let cell = CellRef::new(rng.below(12) as u32, rng.below(6) as u32);
            let r = |rng: &mut Lcg| format!("{}{}", xl_engine::col_name(rng.below(6) as u32), rng.below(12) + 1);
            let raw = match rng.below(4) {
                0 => format!("{}", rng.below(50)),
                1 => format!("={}+Sheet1!{}", r(&mut rng), r(&mut rng)),
                2 => {
                    let col = xl_engine::col_name(rng.below(6) as u32);
                    format!("=SUM({col}{}:{col}{})", rng.below(6) + 1, rng.below(6) + 7)
                }
                _ => format!("=${}*2", r(&mut rng)),
            };
            items.push((Addr::new(sheet, cell), raw));
        }
        wb.set_inputs(items);
        let before: Vec<(Addr, String, Value)> =
            [s, other].iter().flat_map(|&sh| (0..12).flat_map(move |r| (0..6).map(move |c| Addr::new(sh, CellRef::new(r, c))))).map(|x| (x, wb.input_text(x), wb.value(x).clone())).collect();
        let at = rng.below(12) as u32;
        let count = 1 + rng.below(3) as u32;
        let (ins, rows) = if seed % 2 == 0 { (Shift::InsertRows { sheet: s, at, count }, true) } else { (Shift::InsertCols { sheet: s, at, count }, false) };
        wb.apply_shift(ins);
        wb.apply_shift(ins.inverse());
        for (x, text, value) in before {
            assert_eq!(wb.input_text(x), text, "seed {seed} ({}) at {at}+{count}: {x:?}", if rows { "rows" } else { "cols" });
            assert_eq!(*wb.value(x), value, "seed {seed}: {x:?}");
        }
    }
}

#[test]
fn moving_cells_carries_references_along() {
    use xl_engine::Move;
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let other = wb.add_sheet(Some("Other")).unwrap();
    wb.set_inputs([
        (a(s, "A1"), "10".into()),
        (a(s, "A2"), "20".into()),
        (a(s, "A3"), "=A1+A2".into()),
        (a(s, "B1"), "=SUM(A1:A2)".into()),
        (a(s, "B2"), "=SUM(A1:A5)".into()), // partly outside the block: unchanged
        (a(s, "D1"), "99".into()),
        (a(s, "E1"), "=D1".into()),         // D1 gets overwritten
    ]);
    wb.set_input(a(other, "A1"), "=Sheet1!A3*2").unwrap();
    wb.set_format(a(s, "A1"), Some("0.00"));
    // Move A1:A3 to C1:C3... then onto D1:D3.
    wb.move_range(Move { from_sheet: s, from: xl_engine::Range::new(CellRef::parse("A1").unwrap(), CellRef::parse("A3").unwrap()), to_sheet: s, dr: 0, dc: 3 });
    assert_eq!(*wb.value(a(s, "A1")), Value::Empty);
    assert_eq!(*wb.value(a(s, "D1")), n(10.0));
    assert_eq!(wb.format_code(a(s, "D1")), Some("0.00"));
    assert_eq!(wb.format_code(a(s, "A1")), None);
    assert_eq!(wb.input_text(a(s, "D3")), "=D1+D2", "a moved formula's refs into the block move with it");
    assert_eq!(wb.input_text(a(s, "B1")), "=SUM(D1:D2)");
    assert_eq!(wb.input_text(a(s, "B2")), "=SUM(A1:A5)");
    assert_eq!(wb.input_text(a(s, "E1")), "=#REF!", "the overwritten cell's references break");
    assert_eq!(wb.input_text(a(other, "A1")), "=Sheet1!D3*2");
    assert_eq!(*wb.value(a(other, "A1")), n(60.0));
    // To another sheet: the formula keeps pointing at Sheet1's cells.
    wb.move_range(Move { from_sheet: s, from: xl_engine::Range::new(CellRef::parse("B1").unwrap(), CellRef::parse("B1").unwrap()), to_sheet: other, dr: 4, dc: 0 });
    assert_eq!(wb.input_text(a(other, "B5")), "=SUM(Sheet1!D1:D2)");
    assert_eq!(*wb.value(a(other, "B5")), n(30.0));
}

#[test]
fn overlapping_move() {
    use xl_engine::Move;
    let (mut wb, s) = book(&[("A1", "1"), ("A2", "2"), ("A3", "3"), ("B1", "=SUM(A1:A3)")]);
    // Shift A1:A3 down one row (overlapping its own cells).
    wb.move_range(Move { from_sheet: s, from: xl_engine::Range::new(CellRef::parse("A1").unwrap(), CellRef::parse("A3").unwrap()), to_sheet: s, dr: 1, dc: 0 });
    assert_eq!((1..=4).map(|r| wb.value(a(s, &format!("A{r}"))).clone()).collect::<Vec<_>>(), vec![Value::Empty, n(1.0), n(2.0), n(3.0)]);
    assert_eq!(wb.input_text(a(s, "B1")), "=SUM(A2:A4)");
    assert_eq!(*wb.value(a(s, "B1")), n(6.0));
}
