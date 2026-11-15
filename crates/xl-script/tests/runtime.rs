//! The Lua runtime: sandbox, limits, conversions, functions in formulas, macros.

use std::sync::Arc;
use std::time::{Duration, Instant};

use xl_engine::udf::Array;
use xl_engine::{Addr, CellRef, ErrorKind, UdfArg, UdfValue, Value, Workbook};
use xl_script::{FunctionHost, Limits, ScriptModule, WorkbookHost, run_macro};

fn m(source: &str) -> Vec<ScriptModule> {
    vec![ScriptModule { name: "Module1".into(), source: source.into() }]
}

fn host(source: &str) -> FunctionHost {
    FunctionHost::new(&m(source), Limits::FUNCTIONS).unwrap()
}

fn quick() -> Limits {
    Limits { memory: 16 << 20, time: Duration::from_millis(300) }
}

fn call(h: &FunctionHost, name: &str, args: &[UdfArg]) -> Result<UdfValue, xl_script::ScriptError> {
    h.call_direct(name, args)
}

fn v(x: Value) -> UdfArg {
    UdfArg::Value(x)
}

#[test]
fn sandbox_has_no_way_out() {
    let h = host(
        r#"
        xl.func("PROBE", function(name)
          local ok, val = pcall(function() return load("return " .. name)() end)
          if not ok then return "error" end
          return type(val)
        end)
    "#,
    );
    for name in ["io", "os", "package", "require", "debug", "dofile", "loadfile", "collectgarbage", "string.dump", "ffi", "jit"] {
        let got = call(&h, "PROBE", &[v(Value::text(name))]).unwrap();
        assert!(matches!(&got, UdfValue::Value(Value::Text(t)) if &**t == "nil" || &**t == "error"), "{name} is reachable: {got:?}");
    }
    for name in ["string", "table", "math", "utf8", "coroutine"] {
        assert_eq!(call(&h, "PROBE", &[v(Value::text(name))]).unwrap(), UdfValue::Value(Value::text("table")), "{name}");
    }
    // Precompiled bytecode is refused (it can escape the VM).
    let h = host(r#"xl.func("BC", function() local f, err = load("\27Lua", "x", "b") return f == nil and "refused" or "loaded" end)"#);
    assert_eq!(call(&h, "BC", &[]).unwrap(), UdfValue::Value(Value::text("refused")));
}

#[test]
fn runaway_scripts_are_stopped() {
    let h = FunctionHost::new(&m(r#"xl.func("SPIN", function() while true do end end)"#), quick()).unwrap();
    let start = Instant::now();
    let err = call(&h, "SPIN", &[]).unwrap_err();
    assert!(err.is_timeout(), "{err:?}");
    assert!(err.to_string().contains("took too long"));
    assert!(start.elapsed() < Duration::from_secs(3));
    // pcall can't swallow the deadline.
    let h = FunctionHost::new(&m(r#"xl.func("SNEAKY", function() while true do pcall(function() while true do end end) end end)"#), quick()).unwrap();
    assert!(call(&h, "SNEAKY", &[]).unwrap_err().is_timeout());
    // The state still works after a timeout.
    let h = FunctionHost::new(&m(r#"xl.func("SPIN", function(x) if x then while true do end end return 1 end)"#), quick()).unwrap();
    assert!(call(&h, "SPIN", &[v(Value::Bool(true))]).is_err());
    assert_eq!(call(&h, "SPIN", &[]).unwrap(), UdfValue::Value(Value::Number(1.0)));
    // A script whose top level never finishes fails to load, without hanging.
    let h = FunctionHost::new(&m("while true do end"), quick()).unwrap();
    assert!(h.catalog().errors[0].is_timeout());
}

#[test]
fn memory_is_capped() {
    let h = FunctionHost::new(&m(r#"xl.func("HOG", function() local t = {} for i = 1, 1e8 do t[i] = string.rep("x", 100) .. i end return #t end)"#), Limits { memory: 8 << 20, time: Duration::from_secs(10) }).unwrap();
    let err = call(&h, "HOG", &[]).unwrap_err();
    assert!(err.message.contains("too much memory"), "{err:?}");
    let h = FunctionHost::new(&m(r#"xl.func("BIG", function() return string.rep("x", 1e9) end)"#), Limits { memory: 8 << 20, time: Duration::from_secs(10) }).unwrap();
    assert!(call(&h, "BIG", &[]).unwrap_err().message.contains("too much memory"));
}

#[test]
fn values_convert_both_ways() {
    let h = host(
        r##"
        xl.func("ECHO", function(x) return x end)
        xl.func("KIND", function(x)
          if xl.iserror(x) then return "error " .. tostring(x) end
          return type(x)
        end)
        xl.func("MISSING", function() return xl.error("#N/A") end)
        xl.func("SHAPE", function(r) return r.rows .. "x" .. r.cols .. ":" .. tostring(r[2][1]) .. ":" .. tostring(r[1][2]) end)
        xl.func("GRID", function() return { {1, 2}, {3, "four"} } end)
        xl.func("LIST", function() return { 1, 2, 3 } end)
        xl.func("HALF", function() return 7 // 2 end)
        xl.func("NAN", function() return 0/0 end)
        xl.func("FN", function() return print end)
    "##,
    );
    let echo = |x: Value| call(&h, "ECHO", &[v(x)]).unwrap();
    assert_eq!(echo(Value::Number(2.5)), UdfValue::Value(Value::Number(2.5)));
    assert_eq!(echo(Value::text("hé")), UdfValue::Value(Value::text("hé")));
    assert_eq!(echo(Value::Bool(true)), UdfValue::Value(Value::Bool(true)));
    assert_eq!(echo(Value::Empty), UdfValue::Value(Value::Empty));
    assert_eq!(echo(Value::Error(ErrorKind::Div0)), UdfValue::Value(Value::Error(ErrorKind::Div0)));
    assert_eq!(call(&h, "KIND", &[v(Value::Error(ErrorKind::NA))]).unwrap(), UdfValue::Value(Value::text("error #N/A")));
    assert_eq!(call(&h, "KIND", &[v(Value::Number(1.0))]).unwrap(), UdfValue::Value(Value::text("number")));
    assert_eq!(call(&h, "MISSING", &[]).unwrap(), UdfValue::Value(Value::Error(ErrorKind::NA)));
    let range = Array { rows: 2, cols: 2, values: vec![Value::Number(1.0), Value::Empty, Value::text("b"), Value::Bool(false)] };
    assert_eq!(call(&h, "SHAPE", &[UdfArg::Array(range)]).unwrap(), UdfValue::Value(Value::text("2x2:b:nil")));
    assert_eq!(
        call(&h, "GRID", &[]).unwrap(),
        UdfValue::Array(Array { rows: 2, cols: 2, values: vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0), Value::text("four")] })
    );
    assert_eq!(call(&h, "LIST", &[]).unwrap(), UdfValue::Array(Array { rows: 1, cols: 3, values: vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)] }));
    assert_eq!(call(&h, "HALF", &[]).unwrap(), UdfValue::Value(Value::Number(3.0)));
    assert_eq!(call(&h, "NAN", &[]).unwrap(), UdfValue::Value(Value::Error(ErrorKind::Num)));
    assert!(call(&h, "FN", &[]).unwrap_err().message.contains("function"));
}

#[test]
fn load_errors_have_lines_and_others_still_load() {
    let mods = vec![
        ScriptModule { name: "Good".into(), source: "xl.func('ONE', function() return 1 end)".into() },
        ScriptModule { name: "Broken".into(), source: "\n\nlocal x = = 2".into() },
        ScriptModule { name: "Throws".into(), source: "xl.func('TWO', function() return 2 end)\nerror('nope')".into() },
        ScriptModule { name: "Clash".into(), source: "xl.func('SUM', function() return 0 end)".into() },
    ];
    let h = FunctionHost::new(&mods, Limits::FUNCTIONS).unwrap();
    let errs = &h.catalog().errors;
    let broken = errs.iter().find(|e| e.module.as_deref() == Some("Broken")).unwrap();
    assert_eq!(broken.line, Some(3), "{broken:?}");
    let throws = errs.iter().find(|e| e.module.as_deref() == Some("Throws")).unwrap();
    assert_eq!((throws.line, throws.message.as_str()), (Some(2), "nope"));
    assert!(errs.iter().any(|e| e.message.contains("SUM is a built-in")));
    let names: Vec<_> = h.catalog().functions.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["ONE", "TWO"]);
}

fn addr(wb: &Workbook, a1: &str) -> Addr {
    Addr::new(wb.sheet_ids()[0], CellRef::parse(a1).unwrap())
}

#[test]
fn script_functions_in_formulas() {
    let h = Arc::new(host(
        r#"
        xl.func("DISCOUNT", function(price, rate) return price * (1 - rate) end)
        xl.func("TOTAL", function(r)
          local s = 0
          for i = 1, r.rows do for j = 1, r.cols do
            local x = r[i][j]
            if type(x) == "number" then s = s + x end
          end end
          return s
        end)
        xl.func("BOOM", function(x) error("bad input: " .. tostring(x)) end)
        xl.func("COUNTER", function() n = (n or 0) + 1 return n end, { volatile = true })
    "#,
    ));
    let mut wb = Workbook::new();
    wb.set_user_functions(Some(h.clone()));
    wb.set_input(addr(&wb, "A1"), "100").unwrap();
    wb.set_input(addr(&wb, "A2"), "50").unwrap();
    wb.set_input(addr(&wb, "B1"), "=DISCOUNT(A1, 0.1)").unwrap();
    wb.set_input(addr(&wb, "B2"), "=TOTAL(A1:A3)").unwrap();
    wb.set_input(addr(&wb, "B3"), "=BOOM(A1)").unwrap();
    wb.set_input(addr(&wb, "B4"), "=COUNTER()").unwrap();
    assert_eq!(*wb.value(addr(&wb, "B1")), Value::Number(90.0));
    assert_eq!(*wb.value(addr(&wb, "B2")), Value::Number(150.0));
    wb.set_input(addr(&wb, "A3"), "25").unwrap();
    assert_eq!(*wb.value(addr(&wb, "B2")), Value::Number(175.0));
    // Script errors show #VALUE! with the message kept for the cell.
    assert_eq!(*wb.value(addr(&wb, "B3")), Value::Error(ErrorKind::Value));
    let msg = h.cell_error(addr(&wb, "B3")).unwrap();
    assert!(msg.contains("Module1, line 11") && msg.contains("bad input: 100"), "{msg}");
    // Volatile functions rerun on every change.
    let before = wb.value(addr(&wb, "B4")).clone();
    wb.set_input(addr(&wb, "C9"), "x").unwrap();
    assert_ne!(*wb.value(addr(&wb, "B4")), before);
    assert!(h.catalog().functions.iter().any(|f| f.name == "COUNTER" && f.volatile));
}

#[test]
fn macros_automate_the_workbook() {
    let mods = m(
        r##"
        xl.macro("Report", function()
          xl.set("A1", "Item")
          xl.set("B1", "Price")
          xl.set("A2:B3", { {"Tea", 3}, {"Cake", 4.5} })
          xl.set_formula("B4", "=SUM(B2:B3)")
          xl.bold("A1:B1")
          xl.fill("A1:B1", "#FFCC00")
          xl.number_format("B2:B4", "0.00")
          local total = xl.get("B4")
          xl.message("Total " .. total)
          print("rows", xl.get("A1:B3").rows)
          local name = xl.add_sheet("Summary")
          xl.activate(name)
          xl.set("A1", "=not a formula")
          xl.rename_sheet("Summary", "Totals")
          xl.select("B2:C3")
        end, { key = "ctrl-alt-r" })
        xl.macro("Rows", function()
          xl.insert_rows(1, 2)
          xl.delete_rows(5)
        end)
        xl.macro("Fails", function() xl.get("not a ref!") end)
        xl.macro("Spin", function() while true do end end)
    "##,
    );
    let mut wb = Workbook::new();
    let mut host = WorkbookHost::new(&mut wb);
    let out = run_macro(&mods, "Report", &mut host, Limits::MACROS);
    assert_eq!(out.error, None);
    assert_eq!(out.output, ["rows\t3"]);
    assert_eq!(host.messages, ["Total 7.5"]);
    assert_eq!(xl_script::macros::range_text(host.selection), "B2:C3");
    let s1 = wb.sheet_ids()[0];
    let at = |s, a1: &str| Addr::new(s, CellRef::parse(a1).unwrap());
    assert_eq!(*wb.value(at(s1, "B4")), Value::Number(7.5));
    assert_eq!(wb.input_text(at(s1, "B4")), "=SUM(B2:B3)");
    assert!(wb.style(at(s1, "A1")).bold && wb.style(at(s1, "B1")).fill == Some(0xFFCC00));
    assert_eq!(wb.format_code(at(s1, "B3")), Some("0.00"));
    let totals = wb.sheet_id("Totals").unwrap();
    assert_eq!(*wb.value(at(totals, "A1")), Value::text("=not a formula"), "xl.set writes values, not formulas");

    let mut host = WorkbookHost::new(&mut wb);
    assert_eq!(run_macro(&mods, "Rows", &mut host, Limits::MACROS).error, None);
    assert_eq!(*wb.value(at(s1, "A3")), Value::text("Item"));
    assert_eq!(*wb.value(at(s1, "A4")), Value::text("Tea"));
    assert_eq!(wb.input_text(at(s1, "B5")), "=SUM(B4:B4)", "row 5 (Cake) was deleted");

    let mut host = WorkbookHost::new(&mut wb);
    let err = run_macro(&mods, "Fails", &mut host, Limits::MACROS).error.unwrap();
    assert!(err.message.contains("isn't a cell") && err.line == Some(23), "{err:?}");
    let err = run_macro(&mods, "Spin", &mut host, quick()).error.unwrap();
    assert!(err.is_timeout());
    let err = run_macro(&mods, "Nope", &mut host, Limits::MACROS).error.unwrap();
    assert!(err.message.contains("no macro"));
}

#[test]
fn catalog_lists_macros_in_order_with_keys() {
    let h = host("xl.macro('B', function() end)\nxl.macro('A', function() end, { key = 'ctrl-alt-a', description = 'does a' })");
    let ms = &h.catalog().macros;
    assert_eq!(ms.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["B", "A"]);
    assert_eq!(ms[1].key.as_deref(), Some("ctrl-alt-a"));
    assert_eq!(ms[1].description.as_deref(), Some("does a"));
}
