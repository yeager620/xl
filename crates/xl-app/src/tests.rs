//! Headless interaction tests: real key dispatch through GPUI's test platform.

use gpui::{Entity, TestAppContext, VisualTestContext};
use xl_engine::{Addr, CellRef, Value};

use crate::app::Spreadsheet;

fn setup(cx: &mut TestAppContext) -> (Entity<Spreadsheet>, &mut VisualTestContext) {
    let (view, cx) = cx.add_window_view(|window, cx| Spreadsheet::new(None, window, cx));
    cx.run_until_parked();
    (view, cx)
}

fn addr(view: &Entity<Spreadsheet>, cx: &mut VisualTestContext, sheet: &str, a1: &str) -> Addr {
    view.read_with(cx, |s, _| Addr::new(s.wb.sheet_id(sheet).unwrap(), CellRef::parse(a1).unwrap()))
}

/// Value on the currently shown sheet.
fn val(view: &Entity<Spreadsheet>, cx: &mut VisualTestContext, a1: &str) -> Value {
    view.read_with(cx, |s, _| s.wb.value(s.at(CellRef::parse(a1).unwrap())).clone())
}

fn input(view: &Entity<Spreadsheet>, cx: &mut VisualTestContext, a1: &str) -> String {
    view.read_with(cx, |s, _| s.wb.input_text(s.at(CellRef::parse(a1).unwrap())))
}

fn shown(view: &Entity<Spreadsheet>, cx: &mut VisualTestContext, a1: &str) -> String {
    view.read_with(cx, |s, _| s.wb.display(s.at(CellRef::parse(a1).unwrap())).text)
}

fn active(view: &Entity<Spreadsheet>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |s, _| s.active.to_string())
}

fn sheet(view: &Entity<Spreadsheet>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |s, _| s.wb.sheet_name(s.cur).to_string())
}

fn go(view: &Entity<Spreadsheet>, cx: &mut VisualTestContext, a1: &str) {
    view.update(cx, |s, _| s.select(CellRef::parse(a1).unwrap(), false));
}

#[gpui::test]
fn type_over_cell_recalcs_dependents(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    assert_eq!(active(&view, cx), "B4");
    cx.simulate_input("10");
    cx.simulate_keystrokes("enter");
    assert_eq!(val(&view, cx, "B4"), Value::Number(10.0));
    assert_eq!(val(&view, cx, "D4"), Value::Number(145.0));
    assert_eq!(shown(&view, cx, "D4"), "$145.00");
    assert_eq!(val(&view, cx, "D7"), Value::Number(180.1));
    assert_eq!(active(&view, cx), "B5");
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(val(&view, cx, "D4"), Value::Number(43.5));
    cx.simulate_keystrokes("ctrl-y");
    assert_eq!(val(&view, cx, "D4"), Value::Number(145.0));
}

#[gpui::test]
fn point_mode_builds_references_with_arrows(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    cx.simulate_keystrokes("right right right"); // E4
    cx.simulate_input("=");
    cx.simulate_keystrokes("left left left"); // B4
    view.read_with(cx, |s, _| assert_eq!(s.edit.as_ref().unwrap().text, "=B4"));
    cx.simulate_input("*2+");
    cx.simulate_keystrokes("left left"); // C4
    cx.simulate_keystrokes("enter");
    assert_eq!(input(&view, cx, "E4"), "=B4*2+C4");
    assert_eq!(val(&view, cx, "E4"), Value::Number(20.5));
}

#[gpui::test]
fn shift_arrow_in_point_mode_makes_a_range(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    go(&view, cx, "E9");
    cx.simulate_input("=SUM(");
    cx.simulate_keystrokes("left up up up up up shift-down shift-down");
    cx.simulate_input(")");
    cx.simulate_keystrokes("enter");
    assert_eq!(input(&view, cx, "E9"), "=SUM(D4:D6)");
    assert_eq!(val(&view, cx, "E9"), Value::Number(78.6));
}

#[gpui::test]
fn bad_formula_keeps_editor_open(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    go(&view, cx, "E4");
    cx.simulate_input("=1+");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| {
        assert!(s.edit.is_some());
        assert!(s.message.as_ref().unwrap().1);
    });
    cx.simulate_input("2");
    cx.simulate_keystrokes("enter");
    assert_eq!(val(&view, cx, "E4"), Value::Number(3.0));
    cx.simulate_keystrokes("up");
    cx.simulate_input("99");
    cx.simulate_keystrokes("escape");
    assert_eq!(val(&view, cx, "E4"), Value::Number(3.0));
}

#[gpui::test]
fn fill_down_and_copy_paste_carry_formulas_and_formats(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    go(&view, cx, "E4");
    cx.simulate_input("=D4*2");
    cx.simulate_keystrokes("enter ctrl-shift-4 up shift-down shift-down ctrl-d");
    assert_eq!(input(&view, cx, "E6"), "=D6*2");
    assert_eq!(val(&view, cx, "E6"), Value::Number(43.2));
    go(&view, cx, "E5");
    cx.simulate_keystrokes("ctrl-shift-4");
    go(&view, cx, "E4");
    cx.simulate_keystrokes("ctrl-c");
    go(&view, cx, "F8");
    cx.simulate_keystrokes("ctrl-v");
    assert_eq!(input(&view, cx, "F8"), "=E8*2");
    // E5 got the currency format; pasting it carries the format.
    go(&view, cx, "E5");
    cx.simulate_keystrokes("ctrl-c");
    go(&view, cx, "G1");
    cx.simulate_keystrokes("ctrl-v");
    assert_eq!(shown(&view, cx, "G1"), "$0.00", "F2*2 is empty -> 0, shown as currency");
}

#[gpui::test]
fn ctrl_arrows_jump_to_data_edges(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    go(&view, cx, "A3");
    cx.simulate_keystrokes("ctrl-down");
    assert_eq!(active(&view, cx), "A6");
    cx.simulate_keystrokes("ctrl-down");
    assert_eq!(active(&view, cx), "A12");
    cx.simulate_keystrokes("ctrl-home");
    assert_eq!(active(&view, cx), "A1");
    cx.simulate_keystrokes("ctrl-end");
    assert_eq!(active(&view, cx), "D14");
}

#[gpui::test]
fn delete_clears_contents_but_keeps_formats(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    go(&view, cx, "D4");
    cx.simulate_keystrokes("delete");
    assert_eq!(val(&view, cx, "D4"), Value::Empty);
    cx.simulate_input("5");
    cx.simulate_keystrokes("enter");
    assert_eq!(shown(&view, cx, "D4"), "$5.00");
}

#[gpui::test]
fn number_format_shortcuts_and_decimals(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    go(&view, cx, "F1");
    cx.simulate_input("0.256");
    cx.simulate_keystrokes("enter up ctrl-shift-5");
    assert_eq!(shown(&view, cx, "F1"), "26%");
    view.update(cx, |s, _| s.bump_decimals(1));
    assert_eq!(shown(&view, cx, "F1"), "25.6%");
    cx.simulate_keystrokes("ctrl-shift-1");
    assert_eq!(shown(&view, cx, "F1"), "0.26");
    cx.simulate_keystrokes("ctrl-shift-`");
    assert_eq!(shown(&view, cx, "F1"), "0.256");
    // Typed input picks its own format.
    go(&view, cx, "F2");
    cx.simulate_input("3/14/2026");
    cx.simulate_keystrokes("enter");
    assert_eq!(shown(&view, cx, "F2"), "3/14/2026");
    assert_eq!(input(&view, cx, "F2"), "3/14/2026");
    // Undo of a format change.
    go(&view, cx, "F1");
    cx.simulate_keystrokes("ctrl-shift-4");
    assert_eq!(shown(&view, cx, "F1"), "$0.26");
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(shown(&view, cx, "F1"), "0.256");
}

#[gpui::test]
fn sheets_add_switch_rename_and_keep_their_own_selection(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    go(&view, cx, "C3");
    view.update(cx, |s, _| s.add_sheet());
    assert_eq!(sheet(&view, cx), "Sheet1");
    assert_eq!(active(&view, cx), "A1");
    cx.simulate_input("42");
    cx.simulate_keystrokes("enter");
    cx.simulate_keystrokes("ctrl-pageup");
    assert_eq!(sheet(&view, cx), "Welcome");
    assert_eq!(active(&view, cx), "C3", "each sheet remembers its selection");
    cx.simulate_keystrokes("ctrl-pagedown");
    assert_eq!(active(&view, cx), "A2");
    // Rename by typing into the tab.
    view.update(cx, |s, _| {
        let id = s.cur;
        s.start_rename(id);
        s.renaming.as_mut().unwrap().1.clear();
    });
    cx.simulate_input("Data");
    cx.simulate_keystrokes("enter");
    assert_eq!(sheet(&view, cx), "Data");
    // Invalid names are refused with a message.
    view.update(cx, |s, _| {
        let id = s.cur;
        s.start_rename(id);
        s.renaming.as_mut().unwrap().1 = "Welcome".into();
        s.finish_rename(true);
        assert!(s.message.as_ref().unwrap().0.contains("already exists"));
        s.renaming = None;
    });
}

#[gpui::test]
fn pointing_across_sheets_writes_qualified_references(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    view.update(cx, |s, _| {
        s.add_sheet();
        let id = s.cur;
        s.wb.rename_sheet(id, "Rates").unwrap();
    });
    cx.simulate_input("0.5");
    cx.simulate_keystrokes("enter ctrl-pageup");
    go(&view, cx, "E4");
    cx.simulate_input("=D4*");
    // Switch to the Rates sheet mid-formula and point at A1.
    let rates = view.read_with(cx, |s, _| s.wb.sheet_id("Rates").unwrap());
    view.update(cx, |s, _| s.switch_sheet(rates));
    assert_eq!(sheet(&view, cx), "Rates", "pointing keeps the edit open across sheets");
    cx.simulate_keystrokes("up"); // A2 -> A1
    view.read_with(cx, |s, _| assert_eq!(s.edit.as_ref().unwrap().text, "=D4*Rates!A1"));
    cx.simulate_keystrokes("enter");
    assert_eq!(sheet(&view, cx), "Welcome", "committing returns to the edited sheet");
    let e4 = addr(&view, cx, "Welcome", "E4");
    view.read_with(cx, |s, _| {
        assert_eq!(s.wb.input_text(e4), "=D4*Rates!A1");
        assert_eq!(*s.wb.value(e4), Value::Number(21.75));
    });
}

#[gpui::test]
fn deleting_a_sheet_needs_confirmation_and_breaks_refs(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    view.update(cx, |s, _| s.add_sheet());
    cx.simulate_input("7");
    cx.simulate_keystrokes("enter ctrl-pageup");
    go(&view, cx, "F1");
    cx.simulate_input("=Sheet1!A1*2");
    cx.simulate_keystrokes("enter");
    assert_eq!(val(&view, cx, "F1"), Value::Number(14.0));
    let id = view.read_with(cx, |s, _| s.wb.sheet_id("Sheet1").unwrap());
    view.update(cx, |s, _| s.delete_sheet(id));
    assert_eq!(val(&view, cx, "F1"), Value::Error(xl_engine::ErrorKind::Ref));
    assert_eq!(input(&view, cx, "F1"), "=#REF!*2");
}

#[gpui::test]
fn save_and_reopen_xlsx(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    let path = std::env::temp_dir().join(format!("xl-app-test-{}.xlsx", std::process::id()));
    view.update(cx, |s, _| {
        s.add_sheet();
        s.save_to(path.clone());
        assert!(!s.dirty);
    });
    view.update(cx, |s, _| s.load(path.clone()));
    cx.run_until_parked();
    assert_eq!(active(&view, cx), "A1", "opening a file starts at A1");
    view.read_with(cx, |s, _| {
        let names: Vec<&str> = s.wb.sheet_ids().iter().map(|&i| s.wb.sheet_name(i)).collect();
        assert_eq!(names, ["Welcome", "Sheet1"]);
        let d10 = Addr::new(s.wb.sheet_ids()[0], CellRef::parse("D10").unwrap());
        assert_eq!(s.wb.display(d10).text, "$85.08");
        // Excel stores whole Excel-pixels, so widths come back within a pixel.
        let w = s.wb.col_width(s.wb.sheet_ids()[0], 0).unwrap();
        assert!((w - 140.0).abs() <= 1.0, "width {w}");
    });
    std::fs::remove_file(path).ok();
}
