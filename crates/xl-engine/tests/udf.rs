//! The user-function extension point, with a Rust stand-in for scripts.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use xl_engine::udf::Array;
use xl_engine::{Addr, CellRef, ErrorKind, LoadedCell, UdfArg, UdfValue, UserFunctions, Value, Workbook};

fn a(wb: &Workbook, a1: &str) -> Addr {
    Addr::new(wb.sheet_ids()[0], CellRef::parse(a1).unwrap())
}

#[derive(Default)]
struct Fns {
    calls: AtomicUsize,
}

impl UserFunctions for Fns {
    fn has(&self, name: &str) -> bool {
        matches!(name, "DISCOUNT" | "TOTAL" | "SHAPE" | "TICK" | "GRID" | "FAIL")
    }
    fn is_volatile(&self, name: &str) -> bool {
        name == "TICK"
    }
    fn call(&self, name: &str, args: &[UdfArg], _at: Addr) -> UdfValue {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let num = |i: usize| match args.get(i) {
            Some(UdfArg::Value(v)) => v.to_number().unwrap_or(0.0),
            _ => 0.0,
        };
        UdfValue::Value(match name {
            "DISCOUNT" => Value::Number(num(0) * (1.0 - num(1))),
            "TOTAL" => match &args[0] {
                UdfArg::Array(a) => Value::Number(a.values.iter().filter_map(|v| v.to_number().ok()).sum()),
                UdfArg::Value(v) => v.clone(),
            },
            "SHAPE" => match &args[0] {
                UdfArg::Array(a) => Value::text(format!("{}x{}", a.rows, a.cols)),
                UdfArg::Value(_) => Value::text("scalar"),
            },
            "TICK" => Value::Number(n as f64),
            "GRID" => return UdfValue::Array(Array { rows: 2, cols: 2, values: vec![Value::Number(7.0), Value::Number(8.0), Value::Number(9.0), Value::Number(10.0)] }),
            _ => Value::Error(ErrorKind::Value),
        })
    }
}

#[test]
fn unknown_function_is_name_error_until_registered() {
    let mut wb = Workbook::new();
    wb.set_input(a(&wb, "A1"), "100").unwrap();
    wb.set_input(a(&wb, "B1"), "=DISCOUNT(A1, 0.1)").unwrap();
    assert_eq!(*wb.value(a(&wb, "B1")), Value::Error(ErrorKind::Name));
    wb.set_user_functions(Some(Arc::new(Fns::default())));
    assert_eq!(*wb.value(a(&wb, "B1")), Value::Number(90.0));
    // Dependencies: editing the argument recalculates.
    wb.set_input(a(&wb, "A1"), "200").unwrap();
    assert_eq!(*wb.value(a(&wb, "B1")), Value::Number(180.0));
    // And dependents of the script cell follow.
    wb.set_input(a(&wb, "C1"), "=B1+1").unwrap();
    wb.set_input(a(&wb, "A1"), "10").unwrap();
    assert_eq!(*wb.value(a(&wb, "C1")), Value::Number(10.0));
    // Removing the functions brings back #NAME?.
    wb.set_user_functions(None);
    assert_eq!(*wb.value(a(&wb, "B1")), Value::Error(ErrorKind::Name));
}

#[test]
fn ranges_arrive_as_arrays() {
    let mut wb = Workbook::new();
    wb.set_user_functions(Some(Arc::new(Fns::default())));
    for (c, v) in [("A1", "1"), ("A2", "2"), ("B1", "3"), ("B2", "x")] {
        wb.set_input(a(&wb, c), v).unwrap();
    }
    wb.set_input(a(&wb, "D1"), "=TOTAL(A1:B2)").unwrap();
    wb.set_input(a(&wb, "D2"), "=SHAPE(A1:B3)").unwrap();
    wb.set_input(a(&wb, "D3"), "=SHAPE(A1)").unwrap();
    assert_eq!(*wb.value(a(&wb, "D1")), Value::Number(6.0));
    assert_eq!(*wb.value(a(&wb, "D2")), Value::text("3x2"));
    assert_eq!(*wb.value(a(&wb, "D3")), Value::text("scalar"));
    // An edit inside the range recalculates (range dependency).
    wb.set_input(a(&wb, "B2"), "10").unwrap();
    assert_eq!(*wb.value(a(&wb, "D1")), Value::Number(16.0));
    // Built-ins compose with script functions.
    wb.set_input(a(&wb, "D4"), "=SUM(TOTAL(A1:A2), DISCOUNT(10, 0.5))").unwrap();
    assert_eq!(*wb.value(a(&wb, "D4")), Value::Number(8.0));
}

#[test]
fn errors_and_array_results() {
    let mut wb = Workbook::new();
    wb.set_user_functions(Some(Arc::new(Fns::default())));
    wb.set_input(a(&wb, "A1"), "=FAIL()").unwrap();
    assert_eq!(*wb.value(a(&wb, "A1")), Value::Error(ErrorKind::Value));
    // Error arguments reach the function as values (it decides).
    wb.set_input(a(&wb, "A2"), "=DISCOUNT(1/0, 1)").unwrap();
    assert_eq!(*wb.value(a(&wb, "A2")), Value::Number(0.0), "scalar errors are passed through as values");
    wb.set_input(a(&wb, "A3"), "=GRID()").unwrap();
    assert_eq!(*wb.value(a(&wb, "A3")), Value::Number(7.0), "top-left until dynamic arrays spill");
}

#[test]
fn only_volatile_functions_rerun() {
    let mut wb = Workbook::new();
    let fns = Arc::new(Fns::default());
    wb.set_user_functions(Some(fns.clone()));
    wb.set_input(a(&wb, "A1"), "=DISCOUNT(5, 0)").unwrap();
    wb.set_input(a(&wb, "A2"), "=TICK()").unwrap();
    let before = fns.calls.load(Ordering::SeqCst);
    let tick = wb.value(a(&wb, "A2")).clone();
    wb.set_input(a(&wb, "Z9"), "1").unwrap();
    // One unrelated edit: TICK reran, DISCOUNT didn't.
    assert_eq!(fns.calls.load(Ordering::SeqCst), before + 1);
    assert_ne!(*wb.value(a(&wb, "A2")), tick);
}

#[test]
fn opaque_loaded_formulas_come_alive() {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    wb.load_cell(Addr::new(s, CellRef::parse("A1").unwrap()), LoadedCell::Value(Value::Number(50.0)));
    wb.load_cell(Addr::new(s, CellRef::parse("B1").unwrap()), LoadedCell::Formula { src: "DISCOUNT(A1,0.5)".into(), cached: Value::Number(1.0) });
    wb.load_cell(Addr::new(s, CellRef::parse("C1").unwrap()), LoadedCell::Formula { src: "NOPE(A1)".into(), cached: Value::Number(3.0) });
    wb.recalc_all();
    // Without scripts the file's cached value stays.
    assert_eq!(*wb.value(a(&wb, "B1")), Value::Number(1.0));
    wb.set_user_functions(Some(Arc::new(Fns::default())));
    assert_eq!(*wb.value(a(&wb, "B1")), Value::Number(25.0));
    assert_eq!(wb.formula_source(a(&wb, "B1")).as_deref(), Some("DISCOUNT(A1,0.5)"));
    // Formulas with functions nobody defines stay opaque with their cache.
    assert_eq!(*wb.value(a(&wb, "C1")), Value::Number(3.0));
    wb.set_input(a(&wb, "A1"), "10").unwrap();
    assert_eq!(*wb.value(a(&wb, "B1")), Value::Number(5.0));
}

#[test]
fn script_functions_spill_and_ignore_case() {
    let mut wb = Workbook::new();
    wb.set_user_functions(Some(Arc::new(Fns::default())));
    wb.set_input(a(&wb, "A1"), "100").unwrap();
    // Typed in lower case: still the script's DISCOUNT.
    wb.set_input(a(&wb, "B1"), "=discount(A1, 0.5)").unwrap();
    assert_eq!(*wb.value(a(&wb, "B1")), Value::Number(50.0));
    // An array result spills like any dynamic array.
    wb.set_input(a(&wb, "D1"), "=GRID()").unwrap();
    assert_eq!(*wb.value(a(&wb, "D1")), Value::Number(7.0));
    assert_eq!(*wb.value(a(&wb, "E1")), Value::Number(8.0));
    assert_eq!(*wb.value(a(&wb, "D2")), Value::Number(9.0));
    assert_eq!(*wb.value(a(&wb, "E2")), Value::Number(10.0));
    // And feeds spill references and LET.
    wb.set_input(a(&wb, "G1"), "=SUM(D1#)").unwrap();
    assert_eq!(*wb.value(a(&wb, "G1")), Value::Number(34.0));
    wb.set_input(a(&wb, "G2"), "=LET(x, discount(A1, 0.1), x * 2)").unwrap();
    assert_eq!(*wb.value(a(&wb, "G2")), Value::Number(180.0));
}
