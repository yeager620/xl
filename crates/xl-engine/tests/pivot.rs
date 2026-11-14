//! Pivot table summaries.

use std::sync::Arc;

use xl_engine::formula::parse_formula;
use xl_engine::pivot::{Agg, Pivot};
use xl_engine::{Addr, CellRef, Value, Workbook};

fn data() -> (Workbook, u32) {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let rows = [
        ["Region", "Rep", "Product", "Units", "Revenue"],
        ["West", "Ann", "Pens", "10", "15"],
        ["East", "Bo", "Pens", "4", "6"],
        ["West", "Cy", "Pads", "3", "9"],
        ["East", "Bo", "Pads", "", "12"],
        ["West", "Ann", "Pads", "5", "15"],
    ];
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            if !v.is_empty() {
                wb.set_input(Addr::new(s, CellRef::new(r as u32, c as u32)), v).unwrap();
            }
        }
    }
    (wb, s)
}

fn pivot(wb: &Workbook, rows: Vec<usize>, cols: Vec<usize>, values: Vec<(usize, Agg)>) -> Pivot {
    Pivot { source: Arc::new(parse_formula("Sheet1!$A$1:$E$6", wb.sheets()).unwrap()), anchor: CellRef::new(0, 0), rows, cols, values, extent: None }
}

fn t(s: &str) -> Value {
    Value::text(s)
}

fn n(x: f64) -> Value {
    Value::Number(x)
}

#[test]
fn rows_and_values() {
    let (wb, s) = data();
    let p = pivot(&wb, vec![0], vec![], vec![(4, Agg::Sum), (3, Agg::Count)]);
    let g = wb.pivot_grid(s, &p);
    assert_eq!(g[0], vec![t("Region"), t("Sum of Revenue"), t("Count of Units")]);
    assert_eq!(g[1], vec![t("East"), n(18.0), n(1.0)], "blank units don't count");
    assert_eq!(g[2], vec![t("West"), n(39.0), n(3.0)]);
    assert_eq!(g[3], vec![t("Grand Total"), n(57.0), n(4.0)]);
    assert_eq!(wb.pivot_fields(s, &p)[2], "Product");
}

#[test]
fn rows_by_columns_with_averages() {
    let (wb, s) = data();
    let p = pivot(&wb, vec![0, 1], vec![2], vec![(4, Agg::Average)]);
    let g = wb.pivot_grid(s, &p);
    assert_eq!(g[0], vec![t("Region"), t("Rep"), t("Pads"), t("Pens"), t("Total Average of Revenue")]);
    assert_eq!(g[1], vec![t("East"), t("Bo"), n(12.0), n(6.0), n(9.0)]);
    assert_eq!(g[2], vec![t("West"), t("Ann"), n(15.0), n(15.0), n(15.0)]);
    assert_eq!(g[3], vec![t("West"), t("Cy"), n(9.0), Value::Empty, n(9.0)], "no pens for Cy");
    assert_eq!(g.last().unwrap(), &vec![t("Grand Total"), Value::Empty, n(12.0), n(10.5), n(57.0 / 5.0)]);
}

#[test]
fn numbers_sort_numerically_and_min_max() {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let rows = [["Size", "Price"], ["10", "3"], ["9", "5"], ["-2", "1"], ["10", "7"]];
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            wb.set_input(Addr::new(s, CellRef::new(r as u32, c as u32)), v).unwrap();
        }
    }
    let p = Pivot { source: Arc::new(parse_formula("A1:B5", wb.sheets()).unwrap()), anchor: CellRef::new(0, 3), rows: vec![0], cols: vec![], values: vec![(1, Agg::Max), (1, Agg::Min)], extent: None };
    let g = wb.pivot_grid(s, &p);
    let keys: Vec<Value> = g[1..4].iter().map(|r| r[0].clone()).collect();
    assert_eq!(keys, vec![t("-2"), t("9"), t("10")]);
    assert_eq!(g[3], vec![t("10"), n(7.0), n(3.0)]);
}
