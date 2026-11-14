//! Tables and structured references.

use xl_engine::{Addr, CellRef, ErrorKind, Range, Shift, Table, TableStyle, Value, Workbook};

fn a(s: u32, a1: &str) -> Addr {
    Addr::new(s, CellRef::parse(a1).unwrap())
}

fn sales() -> (Workbook, u32) {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let rows = [["Item", "Qty", "Unit Price"], ["Pen", "10", "1.5"], ["Pad", "4", "3"], ["Ink", "2", "8"]];
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            wb.set_input(Addr::new(s, CellRef::new(r as u32, c as u32)), v).unwrap();
        }
    }
    let t = Table { name: "Sales".into(), range: Range::new(CellRef::new(0, 0), CellRef::new(3, 2)), totals: false, style: TableStyle::Blue, banded: true };
    wb.add_table(s, t).unwrap();
    (wb, s)
}

#[test]
fn structured_references() {
    let (mut wb, s) = sales();
    wb.set_inputs([
        (a(s, "E1"), "=SUM(Sales[Qty])".to_string()),
        (a(s, "E2"), "=ROWS(Sales[#All])".to_string()),
        (a(s, "E3"), "=SUMPRODUCT(Sales[Qty],Sales[Unit Price])".to_string()),
        (a(s, "E4"), "=COUNTA(Sales[#Headers])".to_string()),
        (a(s, "E5"), "=SUM(Sales[[Qty]:[Unit Price]])".to_string()),
        (a(s, "E6"), "=Sales[[#Headers],[Unit Price]]".to_string()),
        (a(s, "E7"), "=SUM(Nope[Qty])".to_string()),
    ]);
    assert_eq!(*wb.value(a(s, "E1")), Value::Number(16.0));
    assert_eq!(*wb.value(a(s, "E2")), Value::Number(4.0));
    assert_eq!(*wb.value(a(s, "E3")), Value::Number(43.0));
    assert_eq!(*wb.value(a(s, "E4")), Value::Number(3.0));
    assert_eq!(*wb.value(a(s, "E5")), Value::Number(16.0 + 12.5));
    assert_eq!(*wb.value(a(s, "E6")), Value::text("Unit Price"));
    assert_eq!(*wb.value(a(s, "E7")), Value::Error(ErrorKind::Ref));
    assert_eq!(wb.input_text(a(s, "E5")), "=SUM(Sales[[Qty]:[Unit Price]])", "printed back the same way");
    assert_eq!(wb.input_text(a(s, "E3")), "=SUMPRODUCT(Sales[Qty],Sales[Unit Price])");
    // Values flow through: dependencies are tracked.
    wb.set_input(a(s, "B2"), "100").unwrap();
    assert_eq!(*wb.value(a(s, "E1")), Value::Number(106.0));
}

#[test]
fn this_row_and_growth() {
    let (mut wb, s) = sales();
    // A calculated column inside the table... next to it, in D: refers to its own row.
    wb.set_input(a(s, "D1"), "Total").unwrap();
    assert!(wb.grow_tables(s, &[CellRef::parse("D1").unwrap()]), "typing a header to the right extends the table");
    wb.set_inputs((2..=4).map(|r| (a(s, &format!("D{r}")), "=[@Qty]*[@[Unit Price]]".to_string())));
    assert_eq!(*wb.value(a(s, "D2")), Value::Number(15.0));
    assert_eq!(*wb.value(a(s, "D4")), Value::Number(16.0));
    assert_eq!(wb.input_text(a(s, "D3")), "=[@Qty]*[@[Unit Price]]");
    wb.set_input(a(s, "F1"), "=SUM(Sales[Total])").unwrap();
    assert_eq!(*wb.value(a(s, "F1")), Value::Number(15.0 + 12.0 + 16.0));
    // A new row typed under the table joins it, and totals include it.
    wb.set_inputs([(a(s, "A5"), "Cap".to_string()), (a(s, "B5"), "1".to_string()), (a(s, "C5"), "2".to_string()), (a(s, "D5"), "=[@Qty]*[@[Unit Price]]".to_string())]);
    assert!(wb.grow_tables(s, &[CellRef::parse("A5").unwrap()]));
    assert_eq!(wb.table_at(s, CellRef::parse("D5").unwrap()).map(|t| t.name.as_str()), Some("Sales"));
    assert_eq!(*wb.value(a(s, "D5")), Value::Number(2.0));
    assert_eq!(*wb.value(a(s, "F1")), Value::Number(45.0));
    // [@Col] outside the table's rows is #VALUE!.
    wb.set_input(a(s, "H9"), "=Sales[@Qty]").unwrap();
    assert_eq!(*wb.value(a(s, "H9")), Value::Error(ErrorKind::Value));
}

#[test]
fn tables_follow_structure_and_reject_bad_names() {
    let (mut wb, s) = sales();
    wb.set_input(a(s, "H1"), "=SUM(Sales[Qty])").unwrap();
    wb.apply_shift(Shift::InsertRows { sheet: s, at: 2, count: 1 });
    wb.set_input(a(s, "B3"), "5").unwrap();
    assert_eq!(*wb.value(a(s, "H1")), Value::Number(21.0), "a row inserted inside the table is part of it");
    let bad = Table { name: "A1".into(), range: Range::new(CellRef::new(10, 0), CellRef::new(12, 1)), totals: false, style: TableStyle::Blue, banded: true };
    assert!(wb.add_table(s, bad).is_err(), "cell-like names aren't allowed");
    let dup = Table { name: "sales".into(), range: Range::new(CellRef::new(10, 0), CellRef::new(12, 1)), totals: false, style: TableStyle::Blue, banded: true };
    assert!(wb.add_table(s, dup).is_err(), "names are case-insensitive and unique");
    assert_eq!(wb.next_table_name(), "Table1");
}
