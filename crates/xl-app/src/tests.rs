//! Headless interaction tests: real key dispatch through GPUI's test platform.

use gpui::{Entity, TestAppContext, VisualTestContext};
use xl_engine::{Addr, CellRef, Value};

use crate::app::Spreadsheet;

/// A fixed workbook for interaction tests (independent of the welcome sheet).
fn fixture() -> xl_engine::Workbook {
    use xl_engine::{Border, Style, Workbook};
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    wb.rename_sheet(s, "Welcome").unwrap();
    let rows: &[&[&str]] = &[
        &["Welcome to xl"],
        &[],
        &["Item", "Qty", "Price", "Total"],
        &["Coffee beans", "3", "$14.50", "=B4*C4"],
        &["Oat milk", "6", "$2.25", "=B5*C5"],
        &["Croissants", "12", "$1.80", "=B6*C6"],
        &["", "", "Subtotal", "=SUM(D4:D6)"],
        &["", "", "Tax rate", "8.25%"],
        &["", "", "Tax", "=ROUND(D7*D8,2)"],
        &["", "", "Total", "=D7+D9"],
        &[],
        &["Ordered", "=TODAY()"],
        &[],
        &["A note in the last row of data"],
    ];
    let mut items = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            if !v.is_empty() {
                items.push((Addr::new(s, CellRef::new(r as u32, c as u32)), v.to_string()));
            }
        }
    }
    wb.set_inputs(items);
    for a1 in ["D4", "D5", "D6", "D7", "D9", "D10"] {
        wb.set_format(Addr::new(s, CellRef::parse(a1).unwrap()), Some("$#,##0.00"));
    }
    wb.set_style(Addr::new(s, CellRef::parse("A3").unwrap()), &Style { bold: true, bottom: Border::THIN, ..Style::default() });
    wb.set_col_width(s, 0, Some(140.0));
    wb
}

/// A unique scratch directory name for one test.
fn scratch(kind: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("xl-test-{kind}-{}-{n}", std::process::id()))
}

/// A window as xl starts without a file (on the Home page), with private
/// recovery and recent-files directories.
fn setup_home(cx: &mut TestAppContext) -> (Entity<Spreadsheet>, &mut VisualTestContext) {
    let (view, cx) = cx.add_window_view(|window, cx| Spreadsheet::new(None, window, cx));
    view.update(cx, |s, _| {
        // Never touch the real recovery or data directories from tests.
        s.recovery_offer = None;
        s.recovery_dir = scratch("recovery");
        s.recent = crate::recent::Recent::load(&scratch("recent"));
        s.scripts = crate::scripts_ui::ScriptState::isolated();
    });
    cx.run_until_parked();
    (view, cx)
}

/// A window showing the fixture workbook (past the Home page).
fn setup(cx: &mut TestAppContext) -> (Entity<Spreadsheet>, &mut VisualTestContext) {
    let (view, cx) = setup_home(cx);
    view.update(cx, |s, cx| {
        s.home = None;
        s.wb = fixture();
        s.cur = s.wb.sheet_ids()[0];
        s.active = CellRef::parse("B4").unwrap();
        s.anchor = s.active;
        s.dirty = false;
        cx.notify();
    });
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
    view.update(cx, |s, cx| s.load(path.clone(), cx));
    view.read_with(cx, |s, _| assert!(s.loading.is_some(), "loading happens in the background"));
    cx.run_until_parked();
    view.read_with(cx, |s, _| assert!(s.loading.is_none()));
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

fn style(view: &Entity<Spreadsheet>, cx: &mut VisualTestContext, a1: &str) -> xl_engine::Style {
    view.read_with(cx, |s, _| s.wb.style(s.at(CellRef::parse(a1).unwrap())).clone())
}

#[gpui::test]
fn style_shortcuts_toggle_and_undo(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    go(&view, cx, "B4");
    cx.simulate_keystrokes("shift-down ctrl-b");
    assert!(style(&view, cx, "B4").bold && style(&view, cx, "B5").bold);
    cx.simulate_keystrokes("ctrl-b");
    assert!(!style(&view, cx, "B4").bold, "second press turns it off");
    cx.simulate_keystrokes("ctrl-i ctrl-u ctrl-5");
    let st = style(&view, cx, "B5");
    assert!(st.italic && st.underline && st.strike);
    cx.simulate_keystrokes("ctrl-z ctrl-z ctrl-z");
    assert!(style(&view, cx, "B5").is_default());
}

#[gpui::test]
fn colors_alignment_and_borders(cx: &mut TestAppContext) {
    use xl_engine::{HAlign, LineStyle};
    let (view, cx) = setup(cx);
    go(&view, cx, "F2");
    cx.simulate_keystrokes("shift-right shift-down"); // F2:G3
    view.update(cx, |s, _| {
        s.set_fill(Some(0xFFF2CC));
        s.set_text_color(Some(0xC00000));
        s.set_align(HAlign::Center);
        s.apply_borders(crate::app::BorderPreset::Outside);
    });
    let tl = style(&view, cx, "F2");
    assert_eq!((tl.fill, tl.color, tl.align), (Some(0xFFF2CC), Some(0xC00000), HAlign::Center));
    assert_eq!((tl.top.style, tl.left.style, tl.bottom.style, tl.right.style), (LineStyle::Thin, LineStyle::Thin, LineStyle::None, LineStyle::None));
    let br = style(&view, cx, "G3");
    assert_eq!((br.top.style, br.right.style, br.bottom.style), (LineStyle::None, LineStyle::Thin, LineStyle::Thin));
    // Clicking the same alignment again goes back to General.
    view.update(cx, |s, _| s.set_align(HAlign::Center));
    assert_eq!(style(&view, cx, "F2").align, HAlign::General);
    // Styles copy with the cell.
    go(&view, cx, "F2");
    cx.simulate_keystrokes("ctrl-c");
    go(&view, cx, "J1");
    cx.simulate_keystrokes("ctrl-v");
    assert_eq!(style(&view, cx, "J1").fill, Some(0xFFF2CC));
    view.update(cx, |s, _| s.clear_formatting());
    assert!(style(&view, cx, "J1").is_default());
}

#[gpui::test]
fn input_method_composition(cx: &mut TestAppContext) {
    use gpui::EntityInputHandler;
    let (view, cx) = setup(cx);
    go(&view, cx, "F1");
    // A CJK input method composes "にほん" then commits "日本".
    view.update_in(cx, |s, window, cx| {
        s.replace_and_mark_text_in_range(None, "にほん", Some(3..3), window, cx);
        assert_eq!(s.edit.as_ref().unwrap().text, "にほん");
        assert_eq!(s.marked_text_range(window, cx), Some(0..3));
        s.replace_text_in_range(None, "日本", window, cx);
        assert_eq!(s.edit.as_ref().unwrap().marked, None);
    });
    // Arrow keys belong to the composition while one is active, so commit first.
    cx.simulate_input("語");
    cx.simulate_keystrokes("enter");
    assert_eq!(val(&view, cx, "F1"), Value::text("日本語"));
    // A dead-key sequence arrives as one composed character.
    go(&view, cx, "F2");
    view.update_in(cx, |s, window, cx| {
        s.replace_and_mark_text_in_range(None, "´", None, window, cx);
        s.replace_text_in_range(None, "é", window, cx);
    });
    cx.simulate_input("t");
    cx.simulate_keystrokes("enter");
    assert_eq!(val(&view, cx, "F2"), Value::text("ét"));
    // While composing, Enter goes to the input method, not to commit the cell.
    go(&view, cx, "F3");
    view.update_in(cx, |s, window, cx| s.replace_and_mark_text_in_range(None, "ka", None, window, cx));
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert!(s.edit.is_some(), "edit stays open during composition"));
}

#[gpui::test]
fn dragging_the_scrollbar_scrolls(cx: &mut TestAppContext) {
    use crate::app::Axis;
    use gpui::{Modifiers, MouseButton, point, px};
    let (view, cx) = setup(cx);
    // Give the sheet enough rows that the thumb has room to travel.
    view.update(cx, |s, _| {
        let a = s.at(CellRef::parse("A5000").unwrap());
        s.wb.set_input(a, "far").unwrap();
    });
    cx.run_until_parked();
    let thumb = view.read_with(cx, |s, _| s.thumb(Axis::Vertical).unwrap());
    let grab = point(px((thumb.x + thumb.w / 2.0) as f32), px((thumb.y + 5.0) as f32));
    cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(point(grab.x, grab.y + px(200.)), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(point(grab.x, grab.y + px(200.)), MouseButton::Left, Modifiers::none());
    let (scrolled, after) = view.read_with(cx, |s, _| (s.scroll_y, s.thumb(Axis::Vertical).unwrap()));
    assert!(scrolled > 1000.0, "dragged 200px down the track, scroll_y = {scrolled}");
    assert!((after.y - (thumb.y + 200.0)).abs() < 2.0, "thumb follows the pointer: {} vs {}", after.y, thumb.y + 200.0);
    assert_eq!(active(&view, cx), "B4", "dragging the scrollbar doesn't move the selection");
}

#[gpui::test]
fn command_palette_runs_commands_jumps_and_inserts_functions(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    // Run a command by fuzzy name.
    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_input("bold");
    cx.simulate_keystrokes("enter");
    assert!(style(&view, cx, "B4").bold);
    view.read_with(cx, |s, _| assert!(s.palette.is_none()));
    // Jump to a cell.
    cx.simulate_keystrokes("ctrl-shift-p");
    cx.simulate_input("H200");
    cx.simulate_keystrokes("enter");
    assert_eq!(active(&view, cx), "H200");
    // Start a formula with a function.
    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_input("xlook");
    view.read_with(cx, |s, _| {
        let items = s.palette.as_ref().unwrap().items();
        assert_eq!(items.first(), Some(&crate::palette::Item::Function(xl_engine::Func::XLookup)));
    });
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert_eq!(s.edit.as_ref().unwrap().text, "=XLOOKUP("));
    cx.simulate_keystrokes("escape");
    // Escape closes without running anything; typing doesn't leak into cells.
    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_input("zzz");
    cx.simulate_keystrokes("escape");
    view.read_with(cx, |s, _| assert!(s.palette.is_none() && s.edit.is_none()));
    assert_eq!(val(&view, cx, "H200"), Value::Empty);
}

/// Regression: with our own title bar, pressing a toolbar or window button
/// armed "drag to move"; a 1px wobble during the click started a compositor
/// move (swallowing the click). The test window's start_window_move/zoom
/// panic, so any such move fails this test.
// Element lookups (debug_bounds) are only recorded with debug assertions.
#[cfg(debug_assertions)]
#[gpui::test]
fn clicking_title_bar_buttons_never_starts_a_window_move(cx: &mut TestAppContext) {
    use gpui::{Decorations, Modifiers, MouseButton, Tiling, point, px};
    let (view, cx) = setup(cx);
    let handle = cx.update(|window, _| window.window_handle());
    // Maximized: tiled on every side.
    cx.simulate_window_decorations_change(handle, Decorations::Client { tiling: Tiling::tiled() });
    cx.run_until_parked();
    for selector in ["win-min", "win-max", "win-close"] {
        assert!(cx.debug_bounds(selector).is_some(), "{selector} is shown with client decorations");
    }
    // A wobbly click on Bold toggles bold and doesn't move the window.
    let bold = cx.debug_bounds("bold").unwrap().center();
    cx.simulate_mouse_down(bold, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(point(bold.x + px(1.), bold.y), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(point(bold.x + px(1.), bold.y), MouseButton::Left, Modifiers::none());
    assert!(style(&view, cx, "B4").bold);
    // Pressing and wobbling on a window button doesn't arm a move either
    // (released off the button so the button's own action doesn't fire).
    let max = cx.debug_bounds("win-max").unwrap().center();
    cx.simulate_mouse_down(max, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(point(max.x + px(1.), max.y + px(1.)), MouseButton::Left, Modifiers::none());
    view.read_with(cx, |s, _| assert!(!s.should_move));
    cx.simulate_mouse_up(point(px(300.), px(500.)), MouseButton::Left, Modifiers::none());
}


#[gpui::test]
fn unsaved_changes_ask_before_closing(cx: &mut TestAppContext) {
    use crate::lifecycle::After;
    let (view, cx) = setup(cx);
    // Nothing unsaved: Ctrl+W closes without asking.
    view.update_in(cx, |s, window, cx| {
        s.request(After::Close, window, cx);
        assert!(s.pending.is_none() && s.close_confirmed);
        s.close_confirmed = false;
    });
}

#[gpui::test]
fn dialog_cancel_and_save(cx: &mut TestAppContext) {
    use crate::lifecycle::After;
    let (view, cx) = setup(cx);
    cx.simulate_input("99");
    cx.simulate_keystrokes("enter");
    cx.simulate_keystrokes("ctrl-w");
    view.read_with(cx, |s, _| assert_eq!(s.pending, Some(After::Close)));
    // The dialog is modal: typing doesn't reach the sheet.
    cx.simulate_input("5");
    cx.simulate_keystrokes("escape");
    view.read_with(cx, |s, _| {
        assert!(s.pending.is_none() && s.edit.is_none() && s.dirty);
        assert!(!s.close_confirmed);
    });
    // The window manager asking to close also goes through the dialog.
    let allowed = view.update(cx, |s, cx| s.should_close(cx));
    assert!(!allowed);
    // Save (Enter) writes the file, then closes.
    let path = std::env::temp_dir().join(format!("xl-dialog-test-{}.xlsx", std::process::id()));
    view.update(cx, |s, _| s.path = Some(path.clone()));
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert!(!s.dirty && s.close_confirmed && s.pending.is_none()));
    let reopened = xl_io::open(&path).unwrap().workbook;
    let b4 = Addr::new(reopened.sheet_ids()[0], CellRef::parse("B4").unwrap());
    assert_eq!(*reopened.value(b4), Value::Number(99.0));
    std::fs::remove_file(path).ok();
}

#[gpui::test]
fn autosave_survives_a_crash_and_restores(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    cx.simulate_input("77");
    cx.simulate_keystrokes("enter");
    let (dir, file) = view.update(cx, |s, _| {
        s.path = Some(std::path::PathBuf::from("/tmp/Budget.xlsx"));
        s.autosave();
        (s.recovery_dir.clone(), s.recovery_file())
    });
    assert!(file.exists(), "autosave wrote a recovery copy");
    // Pretend that instance crashed: rename the copy to a dead pid.
    let dead = dir.join("999999998.xlsx");
    std::fs::rename(&file, &dead).unwrap();
    std::fs::rename(file.with_extension("path"), dead.with_extension("path")).unwrap();
    // A fresh window offers it back.
    let offer = crate::lifecycle::find_recovery(&dir).unwrap();
    view.update(cx, |s, cx| {
        s.wb = xl_engine::Workbook::new();
        s.path = None;
        s.dirty = false;
        s.recovery_offer = Some(offer);
        s.restore_recovery(cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |s, _| {
        assert_eq!(s.path.as_deref(), Some(std::path::Path::new("/tmp/Budget.xlsx")), "restored as the original document");
        assert!(s.dirty, "restored changes still need saving");
        assert!(!dead.exists(), "the crash copy is consumed");
    });
    assert_eq!(val(&view, cx, "B4"), Value::Number(77.0));
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
fn insert_and_delete_rows_with_undo(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    // Fixture: D4..D6 = B*C, D7 = SUM(D4:D6). Select rows 5-6 and insert 2 above.
    go(&view, cx, "A5");
    cx.simulate_keystrokes("shift-down ctrl-shift-=");
    assert_eq!(input(&view, cx, "D9"), "=SUM(D4:D8)", "subtotal grew around the new rows");
    assert_eq!(input(&view, cx, "D7"), "=B7*C7", "moved formulas point at their moved inputs");
    assert_eq!(val(&view, cx, "A5"), Value::Empty);
    assert_eq!(val(&view, cx, "A7"), Value::text("Oat milk"));
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(input(&view, cx, "D7"), "=SUM(D4:D6)");
    assert_eq!(val(&view, cx, "A5"), Value::text("Oat milk"));
    cx.simulate_keystrokes("ctrl-y");
    assert_eq!(input(&view, cx, "D9"), "=SUM(D4:D8)");
    cx.simulate_keystrokes("ctrl-z");
    // Delete row 4 (Coffee beans): D7's range shrinks; a direct ref breaks, and undo repairs it.
    go(&view, cx, "F1");
    cx.simulate_input("=D4*2");
    cx.simulate_keystrokes("enter");
    go(&view, cx, "A4");
    cx.simulate_keystrokes("ctrl--");
    assert_eq!(input(&view, cx, "D6"), "=SUM(D4:D5)");
    assert_eq!(input(&view, cx, "F1"), "=#REF!*2");
    assert_eq!(val(&view, cx, "A4"), Value::text("Oat milk"));
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(input(&view, cx, "F1"), "=D4*2", "undo restores formulas that broke");
    assert_eq!(val(&view, cx, "A4"), Value::text("Coffee beans"));
    assert_eq!(shown(&view, cx, "D4"), "$43.50", "and the deleted cells' formats");
}

#[gpui::test]
fn right_click_row_header_opens_row_menu(cx: &mut TestAppContext) {
    use crate::app::ContextKind;
    use gpui::{Modifiers, MouseButton, point, px};
    let (view, cx) = setup(cx);
    let header = view.read_with(cx, |s, _| {
        let vp = s.viewport;
        // Row 6's header: left gutter, 5 rows down from the top of the grid.
        point(vp.origin.x + px(10.), vp.origin.y + px((s.hh() + crate::app::ROW_H * 5.5) as f32))
    });
    cx.simulate_mouse_down(header, MouseButton::Right, Modifiers::none());
    view.read_with(cx, |s, _| {
        assert_eq!(s.context.map(|(_, k)| k), Some(ContextKind::Rows));
        assert!(s.rows_selected());
        assert_eq!(s.selection().start.row, 5);
    });
    view.update(cx, |s, _| s.insert_rows(true));
    assert_eq!(val(&view, cx, "A7"), Value::Empty);
    assert_eq!(val(&view, cx, "A6"), Value::text("Croissants"), "inserted below row 6");
    assert_eq!(val(&view, cx, "C8"), Value::text("Subtotal"));
}

#[gpui::test]
fn merge_navigate_and_undo(cx: &mut TestAppContext) {
    use xl_engine::{HAlign, Range};
    let (view, cx) = setup(cx);
    // Select A1:D1 and merge & center; the title stays, the merge is one cell.
    go(&view, cx, "A1");
    cx.simulate_keystrokes("shift-right shift-right shift-right");
    view.update(cx, |s, _| s.toggle_merge(true));
    let a1d1 = Range::new(CellRef::parse("A1").unwrap(), CellRef::parse("D1").unwrap());
    view.read_with(cx, |s, _| {
        assert_eq!(s.wb.layout(s.cur).merges, vec![a1d1]);
        assert_eq!(s.wb.style(s.at(CellRef::parse("A1").unwrap())).align, HAlign::Center);
    });
    assert_eq!(val(&view, cx, "A1"), Value::text("Welcome to xl"));
    // Clicking inside the merge selects it; arrows step over it.
    go(&view, cx, "C1");
    view.read_with(cx, |s, _| assert_eq!(s.active, CellRef::parse("A1").unwrap()));
    cx.simulate_keystrokes("right");
    view.read_with(cx, |s, _| assert_eq!(s.active, CellRef::parse("E1").unwrap()));
    cx.simulate_keystrokes("left down");
    view.read_with(cx, |s, _| assert_eq!(s.active, CellRef::parse("A2").unwrap()));
    // Merging clears hidden cells; undo restores them and the layout.
    go(&view, cx, "A4");
    cx.simulate_keystrokes("shift-right");
    view.update(cx, |s, _| s.toggle_merge(false));
    assert_eq!(val(&view, cx, "B4"), Value::Empty);
    assert_eq!(val(&view, cx, "D4"), Value::Number(0.0), "D4 = B4*C4 sees the cleared cell");
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(val(&view, cx, "B4"), Value::Number(3.0));
    view.read_with(cx, |s, _| assert_eq!(s.wb.layout(s.cur).merges, vec![a1d1]));
    cx.simulate_keystrokes("ctrl-z");
    view.read_with(cx, |s, _| assert!(s.wb.layout(s.cur).merges.is_empty()));
    cx.simulate_keystrokes("ctrl-y");
    view.read_with(cx, |s, _| assert_eq!(s.wb.layout(s.cur).merges, vec![a1d1]));
}

#[gpui::test]
fn hidden_rows_are_skipped_and_sized_to_zero(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    go(&view, cx, "A5");
    cx.simulate_keystrokes("shift-down");
    view.update(cx, |s, _| s.set_hidden(true, true));
    view.read_with(cx, |s, _| {
        assert_eq!(s.row_h(4), 0.0);
        assert_eq!(s.row_y(6), s.row_y(4), "rows 5-6 take no space");
        assert_eq!(s.active, CellRef::parse("A7").unwrap(), "selection moves off hidden rows");
    });
    cx.simulate_keystrokes("up");
    view.read_with(cx, |s, _| assert_eq!(s.active, CellRef::parse("A4").unwrap()));
    cx.simulate_keystrokes("ctrl-z");
    view.read_with(cx, |s, _| assert_eq!(s.row_h(4), crate::app::ROW_H));
}

#[gpui::test]
fn frozen_panes_keep_headers_in_place(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    go(&view, cx, "B4");
    view.update(cx, |s, _| s.toggle_freeze());
    view.update(cx, |s, _| {
        assert_eq!(s.frozen(), (3, 1));
        s.scroll_y = 10.0 * crate::app::ROW_H;
        // The first grid row on screen is still row 1; below the frozen rows
        // come the scrolled ones.
        assert_eq!(s.row_at_grid(1.0), 0);
        assert_eq!(s.row_at_grid(3.0 * crate::app::ROW_H + 1.0), 13);
        assert_eq!(s.col_at_grid(10.0), 0);
        s.ensure_visible(CellRef::parse("B5").unwrap());
        assert_eq!(s.scroll_y, crate::app::ROW_H, "scrolls back so row 5 sits just under row 4, the first scrolling row");
        s.ensure_visible(CellRef::parse("B2").unwrap());
        assert_eq!(s.scroll_y, crate::app::ROW_H, "frozen rows never need scrolling");
    });
    view.update(cx, |s, _| s.toggle_freeze());
    view.read_with(cx, |s, _| assert_eq!(s.frozen(), (0, 0)));
}

#[gpui::test]
fn wrapped_text_fits_row_height(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    go(&view, cx, "A14");
    view.update(cx, |s, _| s.toggle_wrap());
    cx.update(|window, app| {
        view.update(app, |s, _| s.autofit_row(13, window));
    });
    view.read_with(cx, |s, _| assert!(s.row_h(13) >= 2.0 * crate::app::ROW_H, "a long note in a 140px column wraps: {}", s.row_h(13)));
    cx.simulate_keystrokes("ctrl-z");
    view.read_with(cx, |s, _| assert_eq!(s.row_h(13), crate::app::ROW_H));
}

#[gpui::test]
fn name_box_jumps_and_defines_names(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    cx.simulate_keystrokes("ctrl-g");
    cx.simulate_input("D10");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert_eq!(s.active, CellRef::parse("D10").unwrap()));
    // Name the tax rate cell, then use the name in a formula.
    go(&view, cx, "D8");
    cx.simulate_keystrokes("ctrl-g");
    cx.simulate_input("TaxRate");
    cx.simulate_keystrokes("enter");
    go(&view, cx, "F1");
    cx.simulate_input("=TaxRate*100");
    cx.simulate_keystrokes("enter");
    assert_eq!(val(&view, cx, "F1"), Value::Number(8.25));
    // Jumping to the name selects its cell; a range jump selects the range.
    cx.simulate_keystrokes("ctrl-g");
    cx.simulate_input("taxrate");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| {
        assert_eq!(s.active, CellRef::parse("D8").unwrap());
        assert_eq!(s.wb.name_for_range(s.cur, s.selection()), Some("TaxRate"));
    });
    cx.simulate_keystrokes("ctrl-g");
    cx.simulate_input("A3:D6");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert_eq!(s.selection(), xl_engine::Range::new(CellRef::parse("A3").unwrap(), CellRef::parse("D6").unwrap())));
    // Not a reference and not a valid name: an error, nothing changes.
    cx.simulate_keystrokes("ctrl-g");
    cx.simulate_input("1abc");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert!(s.message.as_ref().is_some_and(|(_, err)| *err)));
}

#[gpui::test]
fn saving_over_a_file_with_unkept_parts_asks_first(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    let path = std::env::temp_dir().join(format!("xl-lossy-{}.xlsx", std::process::id()));
    std::fs::remove_file(&path).ok();
    view.update(cx, |s, _| {
        s.path = Some(path.clone());
        s.unsupported = vec!["charts", "comments"];
        s.dirty = true;
    });
    cx.simulate_keystrokes("ctrl-s");
    view.read_with(cx, |s, _| assert!(s.lossy_prompt && s.dirty));
    assert!(!path.exists(), "nothing written until the user decides");
    cx.simulate_keystrokes("escape");
    view.read_with(cx, |s, _| assert!(!s.lossy_prompt));
    cx.simulate_keystrokes("ctrl-s");
    cx.update(|window, app| view.update(app, |s, cx| s.lossy_save_anyway(window, cx)));
    assert!(path.exists());
    view.read_with(cx, |s, _| assert!(!s.dirty && s.unsupported.is_empty() && !s.lossy_prompt));
    std::fs::remove_file(&path).ok();
}

#[gpui::test]
fn find_and_replace(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    go(&view, cx, "A1");
    cx.simulate_keystrokes("ctrl-f");
    cx.simulate_input("ta");
    // "Tax rate" (C8), "Tax" (C9), "Total" (D3 header, C10)... in row order.
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert_eq!(s.active, CellRef::parse("D3").unwrap(), "Total in the header row comes first"));
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert_eq!(s.active, CellRef::parse("C7").unwrap(), "Subtotal"));
    cx.simulate_keystrokes("shift-enter");
    view.read_with(cx, |s, _| assert_eq!(s.active, CellRef::parse("D3").unwrap()));
    // Typing goes to the find field, not the grid.
    assert_eq!(val(&view, cx, "A1"), Value::text("Welcome to xl"));
    // Replace "Tax" with "VAT" everywhere (case-insensitive), as one undo step.
    view.update(cx, |s, _| {
        s.find.query = "tax".into();
        s.open_find(true);
    });
    cx.simulate_input("VAT");
    view.update(cx, |s, _| s.replace_all());
    assert_eq!(val(&view, cx, "C8"), Value::text("VAT rate"));
    assert_eq!(val(&view, cx, "C9"), Value::text("VAT"));
    cx.simulate_keystrokes("escape");
    view.read_with(cx, |s, _| assert!(!s.find.open && s.field.is_none()));
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(val(&view, cx, "C8"), Value::text("Tax rate"));
    assert_eq!(val(&view, cx, "C9"), Value::text("Tax"));
    // Formulas are searched by their text: find "B4" lands on D4 (=B4*C4).
    view.update(cx, |s, _| {
        s.find.query = "b4".into();
        s.select(CellRef::parse("A1").unwrap(), false);
        s.find_next(true);
        assert_eq!(s.active, CellRef::parse("D4").unwrap());
    });
}

/// A small table at F1:H6 (Name, Team, Score) away from the fixture's data.
fn table(view: &Entity<Spreadsheet>, cx: &mut VisualTestContext) {
    view.update(cx, |s, _| {
        let rows = [["Name", "Team", "Score"], ["dana", "Red", "71"], ["Ari", "Blue", "93"], ["cy", "Red", "88"], ["Bo", "Blue", "65"], ["Eve", "Green", ""]];
        let mut items = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            for (c, v) in row.iter().enumerate() {
                if !v.is_empty() {
                    items.push((s.at(CellRef::new(r as u32, 5 + c as u32)), v.to_string()));
                }
            }
        }
        items.push((s.at(CellRef::parse("I2").unwrap()), "=H2*2".to_string()));
        s.wb.set_inputs(items);
    });
}

#[gpui::test]
fn sort_detects_header_and_moves_formulas(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    table(&view, cx);
    go(&view, cx, "H3");
    view.update(cx, |s, _| s.sort_by_active(false)); // by Score, high to low; blank last
    let col = |view: &Entity<Spreadsheet>, cx: &mut VisualTestContext, c: &str| (2..=6).map(|r| val(view, cx, &format!("{c}{r}"))).collect::<Vec<_>>();
    assert_eq!(val(&view, cx, "F1"), Value::text("Name"), "header row stays put");
    assert_eq!(col(&view, cx, "F"), ["Ari", "cy", "dana", "Bo", "Eve"].map(Value::text).to_vec());
    // dana's row moved from 2 to 4 with its formula, which follows the row.
    assert_eq!(input(&view, cx, "I4"), "=H4*2");
    assert_eq!(val(&view, cx, "I4"), Value::Number(142.0));
    go(&view, cx, "F2");
    view.update(cx, |s, _| s.sort_by_active(true)); // by Name, case-insensitive
    assert_eq!(col(&view, cx, "F"), ["Ari", "Bo", "cy", "dana", "Eve"].map(Value::text).to_vec());
    cx.simulate_keystrokes("ctrl-z ctrl-z");
    assert_eq!(col(&view, cx, "F"), ["dana", "Ari", "cy", "Bo", "Eve"].map(Value::text).to_vec());
}

#[gpui::test]
fn filter_hides_rows_and_undoes(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    table(&view, cx);
    go(&view, cx, "G3");
    cx.simulate_keystrokes("ctrl-shift-l");
    let button = view.read_with(cx, |s, _| {
        let f = s.wb.layout(s.cur).filter.clone().expect("filter on");
        assert_eq!(f.range, xl_engine::Range::new(CellRef::parse("F1").unwrap(), CellRef::parse("I6").unwrap()));
        // The Team column's dropdown, at the right edge of G1.
        let x = f64::from(s.viewport.origin.x) + s.hw() + s.grid_x(6) + s.col_w(6) - 8.0;
        let y = f64::from(s.viewport.origin.y) + s.hh() + s.grid_y(0) + 10.0;
        gpui::point(gpui::px(x as f32), gpui::px(y as f32))
    });
    cx.simulate_click(button, gpui::Modifiers::none());
    view.update(cx, |s, _| {
        let m = s.filter_menu.as_mut().expect("dropdown open");
        assert_eq!(m.values.iter().map(|v| v.0.as_str()).collect::<Vec<_>>(), ["Blue", "Green", "Red"]);
        m.values[0].1 = false; // hide Blue
        m.values[1].1 = false; // hide Green
    });
    view.update(cx, |s, _| s.apply_filter_menu());
    view.read_with(cx, |s, _| {
        let hidden: Vec<u32> = s.wb.layout(s.cur).filtered_rows.iter().copied().collect();
        assert_eq!(hidden, vec![2, 4, 5], "Ari, Bo (Blue) and Eve (Green) are hidden");
        assert_eq!(s.row_h(2), 0.0);
    });
    cx.simulate_keystrokes("ctrl-z");
    view.read_with(cx, |s, _| assert!(s.wb.layout(s.cur).filtered_rows.is_empty()));
    cx.simulate_keystrokes("ctrl-z");
    view.read_with(cx, |s, _| assert!(s.wb.layout(s.cur).filter.is_none()));
}

fn cell_point(view: &Entity<Spreadsheet>, cx: &mut VisualTestContext, a1: &str, dx: f64, dy: f64) -> gpui::Point<gpui::Pixels> {
    let c = CellRef::parse(a1).unwrap();
    view.read_with(cx, |s, _| {
        let x = f64::from(s.viewport.origin.x) + s.hw() + s.grid_x(c.col) + dx;
        let y = f64::from(s.viewport.origin.y) + s.hh() + s.grid_y(c.row) + dy;
        gpui::point(gpui::px(x as f32), gpui::px(y as f32))
    })
}

#[gpui::test]
fn fill_handle_drag_continues_series(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton};
    let (view, cx) = setup(cx);
    view.update(cx, |s, _| {
        s.wb.set_inputs([(s.at(CellRef::parse("F1").unwrap()), "Item 1".to_string()), (s.at(CellRef::parse("G1").unwrap()), "Mon".to_string()), (s.at(CellRef::parse("H1").unwrap()), "=B4*2".to_string())]);
    });
    go(&view, cx, "F1");
    cx.simulate_keystrokes("shift-right shift-right");
    // Grab the handle at H1's bottom-right corner and drag down to row 4.
    let w = view.read_with(cx, |s, _| s.col_w(7));
    let handle = cell_point(&view, cx, "H1", w, crate::app::ROW_H);
    cx.simulate_mouse_down(handle, MouseButton::Left, Modifiers::none());
    view.read_with(cx, |s, _| assert!(s.fill_target.is_some(), "the drag started on the handle"));
    let to = cell_point(&view, cx, "H4", 10.0, 10.0);
    cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    assert_eq!(val(&view, cx, "F4"), Value::text("Item 4"));
    assert_eq!(val(&view, cx, "G4"), Value::text("Thu"));
    assert_eq!(input(&view, cx, "H4"), "=B7*2", "formulas copy with shifted references");
    view.read_with(cx, |s, _| assert_eq!(s.selection(), xl_engine::Range::new(CellRef::parse("F1").unwrap(), CellRef::parse("H4").unwrap())));
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(val(&view, cx, "F4"), Value::Empty);
}

#[gpui::test]
fn double_click_fill_handle_fills_to_neighbouring_data(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    // E4 = B4*C4 beside the item rows (A4:D6); double-click its handle.
    go(&view, cx, "E4");
    cx.simulate_input("=B4+1");
    cx.simulate_keystrokes("enter");
    go(&view, cx, "E4");
    let w = view.read_with(cx, |s, _| s.col_w(4));
    let handle = cell_point(&view, cx, "E4", w, crate::app::ROW_H);
    view.read_with(cx, |s, _| assert!(s.on_fill_handle(handle)));
    view.update(cx, |s, _| s.fill_down_to_data());
    assert_eq!(input(&view, cx, "E10"), "=B10+1", "down to the end of column D's block");
    assert_eq!(val(&view, cx, "E11"), Value::Empty);
}

#[gpui::test]
fn paste_special_and_cut_move(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    // Copy D4 (=B4*C4, $ format) and paste values into F4: number only.
    go(&view, cx, "D4");
    cx.simulate_keystrokes("ctrl-c");
    go(&view, cx, "F4");
    cx.simulate_keystrokes("ctrl-shift-v");
    assert_eq!(input(&view, cx, "F4"), "43.5");
    view.read_with(cx, |s, _| assert_eq!(s.wb.format_code(s.at(CellRef::parse("F4").unwrap())), None, "values keep the destination's format"));
    // Paste formats onto F4: now shown as currency.
    view.update(cx, |s, cx| s.paste_with(crate::app::PasteMode::Formats, cx));
    assert_eq!(shown(&view, cx, "F4"), "$43.50");
    // Transpose B4:B6 into a row at F8.
    go(&view, cx, "B4");
    cx.simulate_keystrokes("shift-down shift-down ctrl-c");
    go(&view, cx, "F8");
    view.update(cx, |s, cx| s.paste_with(crate::app::PasteMode::Transpose, cx));
    assert_eq!([val(&view, cx, "F8"), val(&view, cx, "G8"), val(&view, cx, "H8")], [Value::Number(3.0), Value::Number(6.0), Value::Number(12.0)]);
    // Cut the tax rate (D8) and paste it at F1: D9 = ROUND(D7*D8,2) follows it.
    go(&view, cx, "D8");
    cx.simulate_keystrokes("ctrl-x");
    assert_eq!(val(&view, cx, "D8"), Value::Number(0.0825), "cut leaves the cells until pasted");
    go(&view, cx, "F1");
    cx.simulate_keystrokes("ctrl-v");
    assert_eq!(val(&view, cx, "D8"), Value::Empty);
    assert_eq!(input(&view, cx, "D9"), "=ROUND(D7*F1,2)");
    assert_eq!(shown(&view, cx, "D9"), "$6.48", "78.60 × 8.25%, the rate now read from F1");
    view.read_with(cx, |s, _| assert!(s.clip.is_none(), "a move uses up the clipboard"));
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(input(&view, cx, "D9"), "=ROUND(D7*D8,2)");
    assert_eq!(val(&view, cx, "D8"), Value::Number(0.0825));
    assert_eq!(val(&view, cx, "F1"), Value::Empty);
}

#[gpui::test]
fn formula_autocomplete_and_hints(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    go(&view, cx, "F1");
    cx.simulate_input("=rou");
    view.read_with(cx, |s, _| {
        let (_, items) = s.completions().expect("suggestions");
        let labels: Vec<&str> = items.iter().map(|c| c.label()).collect();
        assert_eq!(labels, ["ROUND", "ROUNDDOWN", "ROUNDUP"]);
    });
    cx.simulate_keystrokes("down tab");
    view.read_with(cx, |s, _| {
        let e = s.edit.as_ref().unwrap();
        assert_eq!(e.text, "=ROUNDDOWN(");
        assert_eq!(e.cursor, e.text.len());
        assert!(s.completions().is_none());
        assert_eq!(crate::complete::call_at(&e.text, e.cursor), Some((xl_engine::Func::RoundDown, 0)));
    });
    cx.simulate_input("D4,1)");
    cx.simulate_keystrokes("enter");
    assert_eq!(val(&view, cx, "F1"), Value::Number(43.5));
    // Escape hides the list without cancelling the edit; names are offered too.
    view.update(cx, |s, _| {
        s.wb.define_name("Rate", None, "Welcome!$D$8").unwrap();
    });
    go(&view, cx, "F2");
    cx.simulate_input("=ra");
    view.read_with(cx, |s, _| assert!(s.completions().unwrap().1.iter().any(|c| c.label() == "Rate")));
    cx.simulate_keystrokes("escape");
    view.read_with(cx, |s, _| assert!(s.edit.is_some() && s.completions().is_none()));
    cx.simulate_input("te*2");
    cx.simulate_keystrokes("enter");
    assert_eq!(val(&view, cx, "F2"), Value::Number(0.165));
}

#[gpui::test]
fn conditional_formatting_dialog(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    go(&view, cx, "B4");
    cx.simulate_keystrokes("shift-down shift-down");
    view.update(cx, |s, _| s.open_cf_dialog(crate::cond_ui::CfKind::Greater));
    cx.simulate_input("5");
    view.update(cx, |s, _| s.cf_dialog.as_mut().unwrap().preset = 2); // green
    cx.simulate_keystrokes("enter");
    let fill = |view: &Entity<Spreadsheet>, cx: &mut VisualTestContext, a1: &str| {
        view.read_with(cx, |s, _| xl_engine::cond::CondView::new(&s.wb, s.cur).cell(CellRef::parse(a1).unwrap()).fill)
    };
    view.read_with(cx, |s, _| assert!(s.cf_dialog.is_none() && s.field.is_none(), "{:?} {:?}", s.message, s.wb.layout(s.cur).cond));
    assert_eq!(fill(&view, cx, "B4"), None, "3 is not > 5");
    assert_eq!(fill(&view, cx, "B5"), Some(0xC6EFCE));
    assert_eq!(fill(&view, cx, "B6"), Some(0xC6EFCE));
    // Typing into the grid while the dialog is open doesn't reach cells.
    view.update(cx, |s, _| s.open_cf_dialog(crate::cond_ui::CfKind::Formula));
    cx.simulate_input("B4>10");
    cx.simulate_keystrokes("enter");
    assert_eq!(val(&view, cx, "B4"), Value::Number(3.0));
    assert_eq!(fill(&view, cx, "B6"), Some(0xFFC7CE), "the newer rule (12 > 10, red preset) wins");
    cx.simulate_keystrokes("ctrl-z ctrl-z");
    view.read_with(cx, |s, _| assert!(s.wb.layout(s.cur).cond.is_empty()));
}

#[gpui::test]
fn insert_move_and_edit_a_chart(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton};
    use xl_engine::chart::ChartKind;
    let (view, cx) = setup(cx);
    table(&view, cx); // F1:H6 Name / Team / Score
    go(&view, cx, "F1");
    cx.simulate_keystrokes("shift-down shift-down shift-down shift-down shift-down shift-right shift-right");
    view.update(cx, |s, _| s.insert_chart(ChartKind::Column));
    let data = view.read_with(cx, |s, _| {
        let l = s.wb.layout(s.cur);
        assert_eq!(l.charts.len(), 1);
        assert_eq!(s.selected_chart, Some(0));
        s.wb.chart_data(s.cur, &l.charts[0])
    });
    assert_eq!(data.categories, ["dana", "Ari", "cy", "Bo", "Eve"], "the text column gives the categories");
    assert_eq!(data.series.len(), 1, "Team is text, so only Score is a series");
    assert_eq!(data.series[0].0, "Score");
    assert_eq!(data.series[0].1, vec![Some(71.0), Some(93.0), Some(88.0), Some(65.0), None]);
    // Values update live.
    view.update(cx, |s, _| s.wb.set_input(s.at(CellRef::parse("H2").unwrap()), "10").unwrap());
    let v = view.read_with(cx, |s, _| s.wb.chart_data(s.cur, &s.wb.layout(s.cur).charts[0]).series[0].1[0]);
    assert_eq!(v, Some(10.0));
    // Drag it by its body 3 rows down.
    let (x, y) = view.read_with(cx, |s, _| {
        let (x, y, _, _) = s.chart_rect(&s.wb.layout(s.cur).charts[0]);
        (x, y)
    });
    let grab = gpui::point(gpui::px((x + 50.0) as f32), gpui::px((y + 50.0) as f32));
    cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
    let to = gpui::point(gpui::px((x + 50.0) as f32), gpui::px((y + 50.0 + 3.0 * crate::app::ROW_H) as f32));
    cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    view.read_with(cx, |s, _| assert_eq!(s.wb.layout(s.cur).charts[0].anchor, CellRef::parse("J4").unwrap()));
    cx.simulate_keystrokes("ctrl-z");
    view.read_with(cx, |s, _| assert_eq!(s.wb.layout(s.cur).charts[0].anchor, CellRef::parse("J1").unwrap()));
    // Retitle, change kind, delete.
    view.update(cx, |s, _| {
        s.selected_chart = Some(0);
        s.edit_chart_title();
    });
    cx.simulate_input("Scores");
    cx.simulate_keystrokes("enter");
    view.update(cx, |s, _| s.update_chart(|c| c.kind = ChartKind::Line));
    view.read_with(cx, |s, _| {
        let c = &s.wb.layout(s.cur).charts[0];
        assert_eq!((c.title.as_deref(), c.kind), (Some("Scores"), ChartKind::Line));
    });
    cx.simulate_keystrokes("delete");
    view.read_with(cx, |s, _| assert!(s.wb.layout(s.cur).charts.is_empty()));
    assert_eq!(val(&view, cx, "J1"), Value::Empty, "deleting the chart didn't clear cells");
    // Inserting rows above moves the chart and its data references.
    cx.simulate_keystrokes("ctrl-z");
    go(&view, cx, "A1");
    cx.simulate_keystrokes("ctrl-shift-=");
    view.read_with(cx, |s, _| {
        let c = &s.wb.layout(s.cur).charts[0];
        assert_eq!(c.anchor, CellRef::parse("J2").unwrap());
        assert_eq!(s.wb.chart_data(s.cur, c).series[0].1[1], Some(93.0));
    });
}

#[gpui::test]
fn remove_duplicates_and_split_text(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    view.update(cx, |s, _| {
        let rows = [["City", "Count"], ["Oslo", "1"], ["oslo", "1"], ["Lima", "2"], ["Oslo", "3"], ["Lima", "2"]];
        let mut items = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            for (c, v) in row.iter().enumerate() {
                items.push((s.at(CellRef::new(r as u32, 6 + c as u32)), v.to_string()));
            }
        }
        items.push((s.at(CellRef::parse("K1").unwrap()), "Ada Lovelace, 1815, \"London, UK\"".to_string()));
        items.push((s.at(CellRef::parse("K2").unwrap()), "Alan Turing, 1912, Maida Vale".to_string()));
        s.wb.set_inputs(items);
    });
    go(&view, cx, "G2");
    view.update(cx, |s, _| s.remove_duplicates());
    let col = |view: &Entity<Spreadsheet>, cx: &mut VisualTestContext, c: &str| (1..=6).map(|r| val(view, cx, &format!("{c}{r}"))).collect::<Vec<_>>();
    assert_eq!(col(&view, cx, "G"), [Value::text("City"), Value::text("Oslo"), Value::text("Lima"), Value::text("Oslo"), Value::Empty, Value::Empty]);
    assert_eq!(col(&view, cx, "H")[3], Value::Number(3.0), "Oslo/3 differs from Oslo/1; oslo/1 is a case-insensitive repeat");
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(val(&view, cx, "G3"), Value::text("oslo"));
    // Split K1:K2 on commas (detected), respecting quotes; numbers become numbers.
    go(&view, cx, "K1");
    cx.simulate_keystrokes("shift-down");
    view.update(cx, |s, _| s.text_to_columns(crate::data::Delimiter::Auto));
    assert_eq!([val(&view, cx, "K1"), val(&view, cx, "L1"), val(&view, cx, "M1")], [Value::text("Ada Lovelace"), Value::Number(1815.0), Value::text("London, UK")]);
    assert_eq!(val(&view, cx, "M2"), Value::text("Maida Vale"));
}

#[gpui::test]
fn group_collapse_and_expand_rows(cx: &mut TestAppContext) {
    use gpui::Modifiers;
    use crate::outline::OutlineAction;
    let (view, cx) = setup(cx);
    // Group the item rows (4-6), then rows 4-5 again (level 2).
    go(&view, cx, "A4");
    cx.simulate_keystrokes("shift-down shift-down alt-shift-right");
    go(&view, cx, "A4");
    cx.simulate_keystrokes("shift-down alt-shift-right");
    view.read_with(cx, |s, _| {
        assert_eq!(s.outline_runs(true), vec![(1, 3, 5), (2, 3, 4)]);
        assert_eq!(s.hw(), crate::app::HEADER_W + 3.0 * crate::app::OUTLINE_STEP, "two levels of groups: level buttons 1-3, like Excel");
    });
    // Click the outer group's − button (beside row 7): rows 4-6 collapse.
    let button = |view: &Entity<Spreadsheet>, cx: &mut VisualTestContext, want: &str| {
        view.read_with(cx, |s, _| {
            let (b, _, label) = s.outline_buttons().into_iter().find(|(_, a, _)| *a == OutlineAction::Toggle { rows: true, start: 3, end: 5 }).unwrap();
            assert_eq!(label, want);
            b.center()
        })
    };
    let minus = button(&view, cx, "−");
    cx.simulate_click(minus, Modifiers::none());
    view.read_with(cx, |s, _| assert!((3..=5).all(|r| s.row_h(r) == 0.0)));
    // The button moved up with the collapsed rows and now shows +.
    let plus = button(&view, cx, "+");
    cx.simulate_click(plus, Modifiers::none());
    view.read_with(cx, |s, _| assert!((3..=5).all(|r| s.row_h(r) > 0.0)));
    // Level 2 button: show level-1 detail only, hiding level-2 rows.
    view.update(cx, |s, _| s.outline_click(OutlineAction::Level { rows: true, level: 2 }));
    view.read_with(cx, |s, _| {
        assert_eq!((s.row_h(3), s.row_h(4), s.row_h(5) > 0.0), (0.0, 0.0, true));
    });
    // Ungroup everything; collapsed rows come back.
    go(&view, cx, "A4");
    cx.simulate_keystrokes("shift-down shift-down alt-shift-left alt-shift-left");
    view.read_with(cx, |s, _| {
        assert!(s.outline_runs(true).is_empty());
        assert_eq!(s.hw(), crate::app::HEADER_W);
        assert!((3..=5).all(|r| s.row_h(r) > 0.0));
    });
}

#[gpui::test]
fn sparklines_per_row(cx: &mut TestAppContext) {
    use xl_engine::chart::SparkKind;
    let (view, cx) = setup(cx);
    view.update(cx, |s, _| {
        let rows = [["Region", "Q1", "Q2", "Q3"], ["North", "5", "7", "6"], ["South", "3", "-2", "4"]];
        let mut items = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            for (c, v) in row.iter().enumerate() {
                items.push((s.at(CellRef::new(20 + r as u32, c as u32)), v.to_string()));
            }
        }
        s.wb.set_inputs(items);
    });
    go(&view, cx, "B22");
    view.update(cx, |s, _| s.insert_sparklines(SparkKind::Column));
    view.read_with(cx, |s, _| {
        let sp = &s.wb.layout(s.cur).sparklines;
        assert_eq!(sp.iter().map(|x| x.cell).collect::<Vec<_>>(), vec![CellRef::parse("E22").unwrap(), CellRef::parse("E23").unwrap()], "one per data row, right of the table");
        assert_eq!(s.wb.sparkline_values(s.cur, &sp[1]), vec![Some(3.0), Some(-2.0), Some(4.0)], "labels and headers aren't data");
    });
    // Deleting a row drops its sparkline; the other follows.
    go(&view, cx, "A22");
    cx.simulate_keystrokes("ctrl--");
    view.read_with(cx, |s, _| {
        let sp = &s.wb.layout(s.cur).sparklines;
        assert_eq!(sp.len(), 1);
        assert_eq!(sp[0].cell, CellRef::parse("E22").unwrap());
    });
    cx.simulate_keystrokes("ctrl-z ctrl-z");
    view.read_with(cx, |s, _| assert!(s.wb.layout(s.cur).sparklines.is_empty()));
}

#[gpui::test]
fn format_as_table_and_grow(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    table(&view, cx); // F1:H6 plus I2
    go(&view, cx, "G3");
    cx.simulate_keystrokes("ctrl-t");
    view.read_with(cx, |s, _| {
        let t = &s.wb.layout(s.cur).tables[0];
        assert_eq!(t.name, "Table1");
        assert_eq!(t.range, xl_engine::Range::new(CellRef::parse("F1").unwrap(), CellRef::parse("I6").unwrap()));
        assert!(s.wb.layout(s.cur).filter.is_some(), "tables come with filter buttons");
    });
    go(&view, cx, "K1");
    cx.simulate_input("=SUM(Table1[Score])");
    cx.simulate_keystrokes("enter");
    assert_eq!(val(&view, cx, "K1"), Value::Number(71.0 + 93.0 + 88.0 + 65.0));
    // Typing under the table adds a row that the total includes.
    go(&view, cx, "H7");
    cx.simulate_input("50");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert_eq!(s.wb.layout(s.cur).tables[0].range.end.row, 6));
    assert_eq!(val(&view, cx, "K1"), Value::Number(367.0));
    // Undo the table entirely: the formula's reference no longer resolves.
    view.update(cx, |s, _| {
        s.select(CellRef::parse("G3").unwrap(), false);
        s.convert_table_to_range();
    });
    assert_eq!(val(&view, cx, "K1"), Value::Error(xl_engine::ErrorKind::Ref));
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(val(&view, cx, "K1"), Value::Number(367.0), "undo brings the table and its references back");
}

#[gpui::test]
fn pivot_table_panel_and_live_refresh(cx: &mut TestAppContext) {
    use crate::pivot_ui::Area;
    let (view, cx) = setup(cx);
    table(&view, cx); // F1:H6 Name/Team/Score (+ I2 formula)
    go(&view, cx, "G2");
    view.update(cx, |s, _| s.insert_pivot());
    view.read_with(cx, |s, _| {
        assert_eq!(s.wb.sheet_name(s.cur), "Pivot 1");
        assert!(s.active_pivot().is_some(), "the panel shows for the new pivot");
    });
    // Defaults: rows by Name (first text column), sum of the last number column.
    // Switch rows to Team.
    view.update(cx, |s, _| {
        let fields = s.wb.pivot_fields(s.cur, &s.wb.layout(s.cur).pivots[0]);
        assert_eq!(&fields[..3], ["Name", "Team", "Score"]);
        s.toggle_pivot_field(0, Area::Rows);
        s.toggle_pivot_field(1, Area::Rows);
        let p = &s.wb.layout(s.cur).pivots[0];
        assert_eq!(p.rows, vec![1]);
    });
    // Make sure Score is the value field.
    view.update(cx, |s, _| {
        let p = s.wb.layout(s.cur).pivots[0].clone();
        if !p.values.iter().any(|(f, _)| *f == 2) {
            s.toggle_pivot_field(2, Area::Values);
        }
        for (f, _) in p.values.iter().filter(|(f, _)| *f != 2) {
            s.toggle_pivot_field(*f, Area::Values);
        }
    });
    let grid = |view: &Entity<Spreadsheet>, cx: &mut VisualTestContext| (1..=5).map(|r| (val(view, cx, &format!("A{r}")), val(view, cx, &format!("B{r}")))).collect::<Vec<_>>();
    assert_eq!(
        grid(&view, cx),
        vec![
            (Value::text("Team"), Value::text("Sum of Score")),
            (Value::text("Blue"), Value::Number(158.0)),
            (Value::text("Green"), Value::Empty),
            (Value::text("Red"), Value::Number(159.0)),
            (Value::text("Grand Total"), Value::Number(317.0)),
        ]
    );
    // Editing the source refreshes the pivot; one undo reverts both.
    view.update(cx, |s, _| {
        let welcome = s.wb.sheet_ids()[0];
        s.switch_sheet(welcome);
        s.select(CellRef::parse("H6").unwrap(), false);
    });
    cx.simulate_input("20");
    cx.simulate_keystrokes("enter");
    let pivot_sheet = view.read_with(cx, |s, _| s.wb.sheet_ids()[1]);
    let green = |view: &Entity<Spreadsheet>, cx: &mut VisualTestContext| view.read_with(cx, |s, _| s.wb.value(Addr::new(pivot_sheet, CellRef::parse("B3").unwrap())).clone());
    assert_eq!(green(&view, cx), Value::Number(20.0));
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(green(&view, cx), Value::Empty);
    view.read_with(cx, |s, _| assert_eq!(*s.wb.value(Addr::new(s.wb.sheet_ids()[0], CellRef::parse("H6").unwrap())), Value::Empty));
}

#[gpui::test]
fn goal_seek_and_solver_dialogs(cx: &mut TestAppContext) {
    use crate::solver_ui::FormKind;
    let (view, cx) = setup(cx);
    // Goal Seek: make the total (D10) 100 by changing croissants' qty (B6).
    go(&view, cx, "D10");
    view.update(cx, |s, _| s.open_form(FormKind::GoalSeek));
    cx.simulate_input("100");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("B6");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert!(s.form.is_none(), "{:?}", s.message));
    let total = |view: &Entity<Spreadsheet>, cx: &mut VisualTestContext| match val(view, cx, "D10") {
        Value::Number(n) => n,
        v => panic!("{v:?}"),
    };
    assert!((total(&view, cx) - 100.0).abs() < 0.01, "total {}", total(&view, cx));
    cx.simulate_keystrokes("ctrl-z");
    assert_eq!(val(&view, cx, "B6"), Value::Number(12.0), "undo restores the changing cell");
    // Solver: maximize F3 = 3*F1 + 5*F2 subject to F1 <= 4, 2*F2 <= 12, 3*F1+2*F2 <= 18.
    view.update(cx, |s, _| {
        let cells = [("F1", "0"), ("F2", "0"), ("F3", "=3*F1+5*F2"), ("F4", "=2*F2"), ("F5", "=3*F1+2*F2")];
        let items: Vec<_> = cells.iter().map(|(c, v)| (s.at(CellRef::parse(c).unwrap()), v.to_string())).collect();
        s.wb.set_inputs(items);
        s.select(CellRef::parse("F3").unwrap(), false);
        s.open_form(FormKind::Solver);
    });
    cx.simulate_input("F1:F2");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("F1 <= 4; F4 <= 12; F5 <= 18");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert!(s.form.is_none(), "{:?}", s.message));
    assert_eq!([val(&view, cx, "F1"), val(&view, cx, "F2"), val(&view, cx, "F3")], [Value::Number(2.0), Value::Number(6.0), Value::Number(36.0)]);
    // A bad constraint keeps the dialog open with an error.
    view.update(cx, |s, _| s.open_form(FormKind::Solver));
    cx.simulate_input("F1:F2");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("F1 about 4");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert!(s.form.is_some() && s.message.as_ref().is_some_and(|(m, err)| *err && m.contains("needs"))));
}

#[gpui::test]
fn notes_insert_edit_hover_delete(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    go(&view, cx, "B4");
    cx.simulate_keystrokes("shift-f2");
    cx.simulate_input("Order more");
    cx.simulate_keystrokes("shift-enter");
    cx.simulate_input("by Friday");
    cx.simulate_keystrokes("enter");
    let note = |view: &Entity<Spreadsheet>, cx: &mut VisualTestContext| view.read_with(cx, |s, _| s.note_at(CellRef::parse("B4").unwrap()).map(|n| n.text.clone()));
    assert_eq!(note(&view, cx).as_deref(), Some("Order more\nby Friday"));
    assert_eq!(val(&view, cx, "B4"), Value::Number(3.0), "typing went into the note, not the cell");
    // Hovering the cell shows the card.
    let p = cell_point(&view, cx, "B4", 10.0, 10.0);
    cx.simulate_mouse_move(p, None, gpui::Modifiers::none());
    view.read_with(cx, |s, _| assert_eq!(s.hover_note, Some(CellRef::parse("B4").unwrap())));
    // Notes move with inserted rows, and undo works.
    go(&view, cx, "A1");
    cx.simulate_keystrokes("ctrl-shift-=");
    view.read_with(cx, |s, _| assert!(s.note_at(CellRef::parse("B5").unwrap()).is_some()));
    cx.simulate_keystrokes("ctrl-z");
    go(&view, cx, "B4");
    view.update(cx, |s, _| s.delete_note());
    assert_eq!(note(&view, cx), None);
    cx.simulate_keystrokes("ctrl-z");
    assert!(note(&view, cx).is_some());
    // Escape cancels an edit.
    cx.simulate_keystrokes("shift-f2");
    cx.simulate_input(" (urgent)");
    cx.simulate_keystrokes("escape");
    assert_eq!(note(&view, cx).as_deref(), Some("Order more\nby Friday"));
}

// ------------------------------------------------------------------ Home

fn on_home(view: &Entity<Spreadsheet>, cx: &mut VisualTestContext) -> bool {
    view.read_with(cx, |s, _| s.home.is_some())
}

fn recent_paths(view: &Entity<Spreadsheet>, cx: &mut VisualTestContext) -> Vec<std::path::PathBuf> {
    view.read_with(cx, |s, _| s.recent.entries().iter().map(|e| e.path.clone()).collect())
}

#[gpui::test]
fn home_shows_on_startup_without_a_file(cx: &mut TestAppContext) {
    let (view, cx) = setup_home(cx);
    view.read_with(cx, |s, _| {
        let h = s.home.as_ref().expect("Home is shown at startup");
        assert!(!h.back, "nothing to go back to at startup");
        assert_eq!(h.selected, crate::home::HomeItem::Blank);
    });
    #[cfg(debug_assertions)]
    {
        assert!(cx.debug_bounds("home-blank").is_some(), "the blank tile is drawn");
        assert!(cx.debug_bounds("home-open").is_some(), "the Open button is drawn");
        assert!(cx.debug_bounds("home-back").is_none(), "no Back button at startup");
    }
    // Escape has nowhere to go back to; typing doesn't start a cell edit.
    cx.simulate_keystrokes("escape");
    cx.simulate_input("abc");
    assert!(on_home(&view, cx));
    view.read_with(cx, |s, _| {
        assert_eq!(s.home.as_ref().unwrap().filter, "abc", "typing filters the recent list");
        assert!(s.edit.is_none() && !s.dirty);
    });
    cx.simulate_keystrokes("backspace backspace backspace");
    view.read_with(cx, |s, _| assert!(s.home.as_ref().unwrap().filter.is_empty()));
}

#[gpui::test]
fn new_blank_and_every_template_open_untitled_workbooks(cx: &mut TestAppContext) {
    let (view, cx) = setup_home(cx);
    // Enter on the selected Blank tile.
    cx.simulate_keystrokes("enter");
    assert!(!on_home(&view, cx));
    view.read_with(cx, |s, _| {
        assert!(s.path.is_none() && !s.dirty);
        assert_eq!(s.wb.sheet_ids().len(), 1);
        assert!(s.wb.store(s.cur).is_empty(), "a blank workbook");
    });
    // Each template, chosen from Home.
    for i in 0..crate::templates::TEMPLATES.len() {
        cx.simulate_keystrokes("alt-home");
        view.read_with(cx, |s, _| assert!(s.home.as_ref().unwrap().back, "Home offers to go back"));
        view.update_in(cx, |s, window, cx| s.activate_home_item(crate::home::HomeItem::Template(i), window, cx));
        assert!(!on_home(&view, cx), "template {i} opened");
        view.read_with(cx, |s, _| {
            assert!(s.path.is_none(), "templates open untitled");
            assert!(!s.dirty);
            assert!(s.wb.store(s.cur).len() > 10);
        });
    }
    // The last one was the grade book: a weighted average computes.
    assert_eq!(sheet(&view, cx), "Grades");
    view.read_with(cx, |s, _| {
        let Value::Number(avg) = s.wb.value(s.at(CellRef::parse("G6").unwrap())).clone() else { panic!("no average") };
        assert!((avg - 89.75).abs() < 1e-9, "weighted average {avg}");
    });
    // Keyboard: Home, then Right to the first template (the budget).
    cx.simulate_keystrokes("alt-home");
    view.read_with(cx, |s, _| assert_eq!(s.home.as_ref().unwrap().selected, crate::home::HomeItem::Blank));
    cx.simulate_keystrokes("right enter");
    assert_eq!(sheet(&view, cx), "Budget");
    assert_eq!(val(&view, cx, "D13"), Value::Number(-62.0));
    assert_eq!(shown(&view, cx, "G7"), "$1,946");
    // Ctrl+N anywhere starts a blank workbook; the palette has a Home command.
    cx.simulate_keystrokes("ctrl-n");
    view.read_with(cx, |s, _| assert!(s.wb.store(s.cur).is_empty() && s.home.is_none()));
    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_input("home");
    cx.simulate_keystrokes("enter");
    assert!(on_home(&view, cx));
    cx.simulate_keystrokes("escape");
    assert!(!on_home(&view, cx), "Escape goes back to the workbook");
}

#[gpui::test]
fn going_home_keeps_unsaved_work(cx: &mut TestAppContext) {
    use crate::home::{HomeItem, Starter};
    use crate::lifecycle::After;
    let (view, cx) = setup(cx);
    cx.simulate_input("99");
    cx.simulate_keystrokes("enter");
    cx.simulate_keystrokes("alt-home");
    assert!(on_home(&view, cx));
    // Back keeps the workbook untouched.
    view.update_in(cx, |s, window, cx| s.activate_home_item(HomeItem::Back, window, cx));
    assert!(!on_home(&view, cx));
    assert_eq!(val(&view, cx, "B4"), Value::Number(99.0));
    view.read_with(cx, |s, _| assert!(s.dirty));
    // Starting something else asks first; Cancel stays on Home with the work intact.
    cx.simulate_keystrokes("alt-home");
    view.update_in(cx, |s, window, cx| s.activate_home_item(HomeItem::Template(1), window, cx));
    view.read_with(cx, |s, _| assert_eq!(s.pending, Some(After::New(Starter::Template(1)))));
    cx.simulate_keystrokes("escape");
    assert!(on_home(&view, cx));
    view.read_with(cx, |s, _| assert!(s.pending.is_none() && s.dirty));
    cx.simulate_keystrokes("escape");
    assert_eq!(val(&view, cx, "B4"), Value::Number(99.0));
    // Opening a recent file asks too; Save writes the work, then opens it.
    let other = scratch("other").with_extension("xlsx");
    let blank = xl_engine::Workbook::new();
    xl_io::save(&blank, &other, blank.sheet_ids()[0], None).unwrap();
    let mine = scratch("mine").with_extension("xlsx");
    view.update(cx, |s, _| s.path = Some(mine.clone()));
    cx.simulate_keystrokes("alt-home");
    view.update_in(cx, |s, window, cx| s.activate_home_item(HomeItem::Recent(other.clone()), window, cx));
    view.read_with(cx, |s, _| assert_eq!(s.pending, Some(After::Open(other.clone()))));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(!on_home(&view, cx));
    view.read_with(cx, |s, _| assert_eq!(s.path.as_deref(), Some(other.as_path())));
    let saved = xl_io::open(&mine).unwrap().workbook;
    assert_eq!(*saved.value(Addr::new(saved.sheet_ids()[0], CellRef::parse("B4").unwrap())), Value::Number(99.0));
    std::fs::remove_file(other).ok();
    std::fs::remove_file(mine).ok();
}

#[gpui::test]
fn recent_files_record_opens_and_saves(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    let a = scratch("recent-a").with_extension("xlsx");
    let b = scratch("recent-b").with_extension("csv");
    view.update(cx, |s, _| {
        s.save_to(a.clone());
        s.save_to(b.clone());
    });
    let (a, b) = (std::fs::canonicalize(&a).unwrap(), std::fs::canonicalize(&b).unwrap());
    assert_eq!(recent_paths(&view, cx), [b.clone(), a.clone()], "most recent first");
    // Opening a file moves it to the top without duplicating it.
    view.update(cx, |s, cx| s.load(a.clone(), cx));
    cx.run_until_parked();
    assert_eq!(recent_paths(&view, cx), [a.clone(), b.clone()]);
    // Autosave (recovery copies) doesn't count.
    view.update(cx, |s, _| {
        s.mark_dirty();
        s.autosave();
    });
    assert_eq!(recent_paths(&view, cx).len(), 2);
    // Persisted: a fresh load of the store sees the same list.
    let dir = view.read_with(cx, |s, _| s.recent.dir().to_path_buf());
    let reloaded = crate::recent::Recent::load(&dir);
    assert_eq!(reloaded.entries().iter().map(|e| e.path.clone()).collect::<Vec<_>>(), [a.clone(), b.clone()]);
    // A file that's gone is shown as missing, and can be removed.
    std::fs::remove_file(&b).unwrap();
    cx.simulate_keystrokes("alt-home");
    view.read_with(cx, |s, _| {
        let e = s.recent.entries();
        assert!(!e[0].missing && e[1].missing);
    });
    // Opening the missing one explains instead of failing silently.
    view.update_in(cx, |s, window, cx| s.activate_home_item(crate::home::HomeItem::Recent(b.clone()), window, cx));
    view.read_with(cx, |s, _| {
        assert!(s.home.is_some() && s.pending.is_none());
        assert!(s.message.as_ref().is_some_and(|(m, err)| *err && m.contains("Can’t find")));
    });
    // Filter, then Delete removes the selected entry.
    cx.simulate_input("recent-b");
    view.read_with(cx, |s, _| assert_eq!(s.home.as_ref().unwrap().selected, crate::home::HomeItem::Recent(b.clone())));
    cx.simulate_keystrokes("delete");
    assert_eq!(recent_paths(&view, cx), std::slice::from_ref(&a));
    assert_eq!(crate::recent::Recent::load(&dir).entries().len(), 1);
    std::fs::remove_file(a).ok();
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
fn opening_a_recent_entry_loads_it(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    let path = scratch("reopen").with_extension("xlsx");
    view.update(cx, |s, _| s.save_to(path.clone()));
    // Something else is open now.
    cx.simulate_keystrokes("ctrl-n");
    assert_eq!(val(&view, cx, "D10"), Value::Empty);
    cx.simulate_keystrokes("alt-home");
    // Type to filter, which selects the match; Enter opens it.
    cx.simulate_input("reopen");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(!on_home(&view, cx));
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
    view.read_with(cx, |s, _| assert_eq!(s.path.as_deref().map(canon), Some(canon(&path))));
    assert_eq!(sheet(&view, cx), "Welcome");
    assert_eq!(shown(&view, cx, "D10"), "$85.08");
    std::fs::remove_file(path).ok();
}

#[gpui::test]
fn home_keeps_the_recovery_offer(cx: &mut TestAppContext) {
    let (view, cx) = setup_home(cx);
    let dir = view.read_with(cx, |s, _| s.recovery_dir.clone());
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("999999997.xlsx");
    let mut wb = xl_engine::Workbook::new();
    let s0 = wb.sheet_ids()[0];
    wb.set_input(Addr::new(s0, CellRef::parse("A1").unwrap()), "42").unwrap();
    xl_io::save(&wb, &file, s0, None).unwrap();
    view.update(cx, |s, cx| {
        s.recovery_offer = crate::lifecycle::find_recovery(&dir);
        assert!(s.recovery_offer.is_some());
        s.restore_recovery(cx);
    });
    cx.run_until_parked();
    assert!(!on_home(&view, cx), "restoring leaves Home for the workbook");
    assert_eq!(val(&view, cx, "A1"), Value::Number(42.0));
    view.read_with(cx, |s, _| {
        assert!(s.dirty && s.path.is_none());
        assert!(s.recent.entries().is_empty(), "recovery files aren't recent files");
    });
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
fn insert_follow_and_remove_links(cx: &mut TestAppContext) {
    use crate::solver_ui::FormKind;
    let (view, cx) = setup(cx);
    // A place link: Ctrl+click jumps there.
    go(&view, cx, "F1");
    view.update(cx, |s, _| s.open_form(FormKind::Link));
    cx.simulate_input("D10");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("Go to total");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| {
        assert!(s.form.is_none(), "{:?}", s.message);
        assert!(matches!(s.link_at(CellRef::parse("F1").unwrap()), Some(xl_engine::Link::Place { .. })));
        let st = s.wb.style(s.at(CellRef::parse("F1").unwrap()));
        assert!(st.underline && st.color == Some(0x0563C1), "styled like a link");
    });
    assert_eq!(val(&view, cx, "F1"), Value::text("Go to total"));
    let p = cell_point(&view, cx, "F1", 10.0, 10.0);
    cx.simulate_click(p, gpui::Modifiers { control: true, ..Default::default() });
    view.read_with(cx, |s, _| assert_eq!(s.active, CellRef::parse("D10").unwrap()));
    // A web address without a scheme gets https://; an empty cell shows the address.
    go(&view, cx, "F2");
    view.update(cx, |s, _| s.open_form(FormKind::Link));
    cx.simulate_input("example.org/docs");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert_eq!(s.link_at(CellRef::parse("F2").unwrap()).map(|l| l.target()), Some("https://example.org/docs")));
    assert_eq!(val(&view, cx, "F2"), Value::text("example.org/docs"));
    // Nonsense addresses are refused.
    go(&view, cx, "F3");
    view.update(cx, |s, _| s.open_form(FormKind::Link));
    cx.simulate_input("not a place");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert!(s.form.is_some() && s.message.as_ref().is_some_and(|(_, e)| *e)));
    cx.simulate_keystrokes("escape");
    go(&view, cx, "F2");
    view.update(cx, |s, _| s.remove_link());
    view.read_with(cx, |s, _| assert!(s.link_at(CellRef::parse("F2").unwrap()).is_none()));
    cx.simulate_keystrokes("ctrl-z");
    view.read_with(cx, |s, _| assert!(s.link_at(CellRef::parse("F2").unwrap()).is_some()));
}

#[gpui::test]
fn data_validation_dropdown_and_refusal(cx: &mut TestAppContext) {
    use crate::solver_ui::FormKind;
    let (view, cx) = setup(cx);
    // A list rule on F1:F5.
    go(&view, cx, "F1");
    cx.simulate_keystrokes("shift-down shift-down shift-down shift-down");
    view.update(cx, |s, _| s.open_form(FormKind::Validation));
    cx.simulate_input("Low, Medium, High");
    cx.simulate_keystrokes("tab tab");
    cx.simulate_input("Pick Low, Medium or High");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert!(s.form.is_none(), "{:?}", s.message));
    // Typing something else is refused and stays in the editor.
    go(&view, cx, "F2");
    cx.simulate_input("Urgent");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| {
        assert!(s.edit.is_some(), "still editing");
        assert_eq!(s.message.as_ref().map(|(m, e)| (m.as_str(), *e)), Some(("Pick Low, Medium or High", true)));
    });
    assert_eq!(val(&view, cx, "F2"), Value::Empty);
    cx.simulate_keystrokes("escape");
    cx.simulate_input("medium");
    cx.simulate_keystrokes("enter");
    assert_eq!(val(&view, cx, "F2"), Value::text("medium"), "choices match without case");
    // The dropdown: button beside the active cell, then a choice.
    go(&view, cx, "F3");
    let button = view.read_with(cx, |s, _| {
        let (x, y, w, h) = s.dropdown_rect().expect("dropdown button");
        gpui::point(gpui::px((x + w / 2.0) as f32), gpui::px((y + h / 2.0) as f32))
    });
    cx.simulate_click(button, gpui::Modifiers::none());
    view.read_with(cx, |s, _| assert_eq!(s.dropdown.as_ref().map(|d| d.1.clone()), Some(vec!["Low".to_string(), "Medium".into(), "High".into()])));
    view.update(cx, |s, _| s.choose(2));
    assert_eq!(val(&view, cx, "F3"), Value::text("High"));
    // A number rule.
    go(&view, cx, "G1");
    view.update(cx, |s, _| s.open_form(FormKind::Validation));
    view.update(cx, |s, _| {
        let f = s.form.as_mut().unwrap();
        f.fields[0].2 = "whole".into();
        f.fields[1].2 = ">=".into();
        f.fields[2].2 = "10".into();
    });
    cx.simulate_keystrokes("enter");
    go(&view, cx, "G1");
    cx.simulate_input("9");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert!(s.edit.is_some()));
    cx.simulate_keystrokes("escape");
    cx.simulate_input("12");
    cx.simulate_keystrokes("enter");
    assert_eq!(val(&view, cx, "G1"), Value::Number(12.0));
}

/// A 2×2 red PNG.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0xfd, 0xd4, 0x9a, 0x73, 0x00, 0x00,
    0x00, 0x10, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x44, 0x0c, 0x10, 0x0a, 0x00, 0x1f, 0xee, 0x03, 0xfd, 0x8b, 0x5f, 0x14, 0xd4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

/// Pictures are laid out over their cells, painted (pixels can't be checked
/// here), move with inserted rows and aren't touched by Delete.
#[gpui::test]
fn pictures_are_drawn_and_follow_inserted_rows(cx: &mut TestAppContext) {
    use std::sync::Arc;
    use xl_engine::picture::{Picture, PictureFormat};
    let (view, cx) = setup(cx);
    view.update(cx, |s, cx| {
        let data: Arc<[u8]> = Arc::from(TINY_PNG);
        let pic = |anchor: &str, end: Option<(&str, (f64, f64))>, format| Picture {
            anchor: CellRef::parse(anchor).unwrap(),
            offset: (4.0, 2.0),
            end: end.map(|(c, o)| (CellRef::parse(c).unwrap(), o)),
            size: (80.0, 40.0),
            data: data.clone(),
            format,
            name: "Picture 1".into(),
            alt: None,
        };
        let cur = s.cur;
        let l = s.wb.layout_mut(cur).unwrap();
        l.pictures.push(pic("C3", Some(("D5", (10.0, 4.0))), PictureFormat::Png));
        l.pictures.push(pic("F2", None, PictureFormat::Png));
        // EMF and the like get a placeholder box.
        l.pictures.push(pic("H8", None, PictureFormat::Other));
        cx.notify();
    });
    cx.run_until_parked();
    view.update(cx, |_, cx| {
        let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, TINY_PNG.to_vec());
        let decoded = image.to_image_data(cx.svg_renderer()).expect("GPUI decodes the PNG");
        assert_eq!(decoded.size(0), gpui::size(gpui::DevicePixels(2), gpui::DevicePixels(2)));
    });
    view.read_with(cx, |s, _| {
        assert_eq!(s.picture_cache.len(), 1, "both PNG pictures were painted from one image");
        let p = &s.wb.layout(s.cur).pictures[0];
        let (x, y, w, h) = s.picture_rect(p);
        let left = f64::from(s.viewport.origin.x) + s.hw() + s.grid_x(2);
        let top = f64::from(s.viewport.origin.y) + s.hh() + s.grid_y(2);
        assert_eq!((x, y), (left + 4.0, top + 2.0));
        // Spans C3+offset to D5+(10, 4): one column and two rows, less the offsets.
        let squash = xl_io::from_excel_height(0.75) / xl_io::from_excel_width_px(1.0);
        assert!((w - (s.col_w(2) - 4.0 + 10.0) * squash).abs() < 1e-9, "{w}");
        assert_eq!(h, s.row_h(2) + s.row_h(3) - 2.0 + 4.0);
    });
    // A row inserted above moves them; one inside the span stretches it.
    go(&view, cx, "A1");
    view.update(cx, |s, _| s.insert_rows(false));
    go(&view, cx, "A5");
    view.update(cx, |s, _| s.insert_rows(false));
    view.read_with(cx, |s, _| {
        let l = s.wb.layout(s.cur);
        assert_eq!(l.pictures[0].anchor, CellRef::parse("C4").unwrap());
        assert_eq!(l.pictures[0].end.map(|e| e.0), CellRef::parse("D7"));
        assert_eq!(l.pictures[1].anchor, CellRef::parse("F3").unwrap());
    });
    // Delete clears the cell under a picture, not the picture.
    go(&view, cx, "C4");
    cx.simulate_keystrokes("delete");
    view.read_with(cx, |s, _| assert_eq!(s.wb.layout(s.cur).pictures.len(), 3));
    cx.simulate_keystrokes("ctrl-z ctrl-z ctrl-z");
    view.read_with(cx, |s, _| assert_eq!(s.wb.layout(s.cur).pictures[0].anchor, CellRef::parse("C3").unwrap(), "undo puts them back"));
    // Frozen panes and scrolling: the frame still builds and paints.
    view.update(cx, |s, cx| {
        let cur = s.cur;
        s.wb.layout_mut(cur).unwrap().freeze = (4, 3);
        s.scroll_y = 200.0;
        s.scroll_x = 150.0;
        cx.notify();
    });
    cx.run_until_parked();
    // Closing the workbook lets the decoded image go.
    view.update(cx, |s, cx| {
        let cur = s.cur;
        s.wb.layout_mut(cur).unwrap().pictures.clear();
        s.undo.clear();
        s.redo.clear();
        cx.notify();
    });
    cx.run_until_parked();
    view.read_with(cx, |s, _| assert_eq!(s.picture_cache.len(), 0));
}

/// Put scripts in the fixture workbook as if it had been opened from a file.
fn with_scripts(view: &Entity<Spreadsheet>, cx: &mut VisualTestContext, source: &str) {
    view.update(cx, |s, cx| {
        s.scripts = crate::scripts_ui::ScriptState::isolated();
        s.wb.set_scripts(vec![xl_engine::ScriptModule { name: "Module1".into(), source: source.into() }]);
        s.scripts_loaded();
        cx.notify();
    });
    cx.run_until_parked();
}

const MACRO_SCRIPT: &str = r#"
xl.func("DOUBLE", function(x) return x * 2 end)
xl.macro("Restock", function()
  xl.set("B4:B6", 100)
  xl.set_formula("E4", "=DOUBLE(B4)")
  xl.bold("A4:A6")
  xl.insert_rows(1)
  local s = xl.add_sheet("Log")
  xl.rename_sheet("Welcome", "Shop")
  xl.message("Restocked")
end, { key = "ctrl-alt-r" })
"#;

#[gpui::test]
fn macro_runs_as_one_undo_step(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    with_scripts(&view, cx, MACRO_SCRIPT);
    // Scripts from a file wait for consent.
    view.read_with(cx, |s, _| assert!(s.scripts.consent_prompt && !s.scripts.allowed));
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert!(!s.scripts.consent_prompt && s.scripts.allowed));
    let undo_before = view.read_with(cx, |s, _| s.undo.len());
    // Run it from its shortcut.
    cx.simulate_keystrokes("ctrl-alt-r");
    view.read_with(cx, |s, _| {
        assert_eq!(s.message.as_ref().map(|m| m.0.as_str()), Some("Restocked"));
        assert_eq!(s.undo.len(), undo_before + 1, "one undo step");
        assert!(s.wb.sheet_id("Log").is_some() && s.wb.sheet_id("Shop").is_some());
    });
    // A row was inserted at the top, so B4 moved to B5.
    assert_eq!(sheet(&view, cx), "Shop");
    assert_eq!(val(&view, cx, "B5"), Value::Number(100.0));
    assert_eq!(val(&view, cx, "E5"), Value::Number(200.0), "script function in a formula");
    assert_eq!(val(&view, cx, "D5"), Value::Number(1450.0));
    view.read_with(cx, |s, _| assert!(s.wb.style(s.at(CellRef::parse("A5").unwrap())).bold));
    // One Ctrl+Z undoes everything.
    cx.simulate_keystrokes("ctrl-z");
    view.read_with(cx, |s, _| {
        assert!(s.wb.sheet_id("Log").is_none());
        assert!(s.wb.sheet_id("Welcome").is_some());
    });
    assert_eq!(val(&view, cx, "B4"), Value::Number(3.0));
    assert_eq!(val(&view, cx, "D4"), Value::Number(43.5));
    assert_eq!(input(&view, cx, "E4"), "");
    view.read_with(cx, |s, _| {
        assert!(!s.wb.style(s.at(CellRef::parse("A4").unwrap())).bold);
        assert_eq!(s.undo.len(), undo_before);
    });
}

#[gpui::test]
fn denied_scripts_dont_run_and_errors_show(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    with_scripts(&view, cx, "xl.func('BAD', function() error('no luck') end)\nxl.macro('M', function() xl.set('A1', 'ran') end)");
    cx.simulate_keystrokes("escape");
    view.read_with(cx, |s, _| assert!(!s.scripts.allowed && s.scripts.host.is_none()));
    // Typing a formula with the function: #NAME? while scripts are off.
    go(&view, cx, "F1");
    cx.simulate_input("=BAD()");
    cx.simulate_keystrokes("enter");
    assert_eq!(val(&view, cx, "F1"), Value::Error(xl_engine::ErrorKind::Name));
    // Running a macro asks again instead of running.
    view.update(cx, |s, _| s.run_macro_named("M"));
    view.read_with(cx, |s, _| assert!(s.scripts.consent_prompt));
    cx.simulate_keystrokes("enter");
    assert_eq!(val(&view, cx, "F1"), Value::Error(xl_engine::ErrorKind::Value));
    go(&view, cx, "F1");
    view.update(cx, |s, _| s.message = None);
    view.read_with(cx, |s, _| {
        let (msg, err) = s.script_error_status().unwrap();
        assert!(err && msg.contains("no luck") && msg.contains("line 1"), "{msg}");
    });
}

#[gpui::test]
fn script_editor_types_saves_and_runs(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    view.update_in(cx, |s, window, _| {
        s.scripts = crate::scripts_ui::ScriptState::isolated();
        s.open_script_editor(window);
        // Start from an empty module.
        let ed = s.scripts.editor.as_mut().unwrap();
        ed.modules[0].source.clear();
        ed.cursor = 0;
    });
    cx.simulate_input("xl.macro('Hi', function()");
    cx.simulate_keystrokes("enter tab");
    cx.simulate_input("xl.set('A1', 'hello')");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("end)");
    view.read_with(cx, |s, _| assert_eq!(s.scripts.editor.as_ref().unwrap().text(), "xl.macro('Hi', function()\n  xl.set('A1', 'hello')\n  end)"));
    cx.simulate_keystrokes("ctrl-enter");
    assert_eq!(val(&view, cx, "A1"), Value::text("hello"));
    view.read_with(cx, |s, _| {
        assert_eq!(s.wb.scripts().len(), 1);
        assert!(s.dirty);
    });
    // A syntax error is reported with its line.
    cx.simulate_keystrokes("ctrl-end");
    cx.simulate_input(" oops(");
    cx.simulate_keystrokes("ctrl-s");
    view.read_with(cx, |s, _| {
        let errs = &s.scripts.editor.as_ref().unwrap().errors;
        assert_eq!(errs.first().and_then(|e| e.line), Some(3), "{errs:?}");
    });
    cx.simulate_keystrokes("escape");
    view.read_with(cx, |s, _| assert!(s.field.is_none() && s.scripts.editor.is_some()));
}

#[gpui::test]
fn version_history_lists_versions_and_shows_diffs(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx);
    let path = scratch("history-doc").with_extension("xlsx");
    let at = |a1: &str| CellRef::parse(a1).unwrap();
    view.update(cx, |s, _| {
        s.history = crate::history_ui::HistoryState::new(scratch("history"));
        s.save_to(path.clone());
    });
    // Edit, insert a row above the data, and save again: a second version.
    go(&view, cx, "B4");
    cx.simulate_input("5");
    cx.simulate_keystrokes("enter");
    go(&view, cx, "A2");
    view.update(cx, |s, _| {
        s.insert_rows(false);
        s.save_to(path.clone());
    });
    // Opened from the palette.
    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_input("Version history");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let (count, older) = view.read_with(cx, |s, _| {
        let p = s.history.panel.as_ref().expect("panel open");
        (p.versions.len(), p.versions[1].id.clone())
    });
    assert_eq!(count, 2, "one version per save");

    // Compare with the first version: B4 (now B5) changed, a row inserted.
    view.update(cx, |s, cx| s.select_version(older.clone(), cx));
    cx.run_until_parked();
    view.read_with(cx, |s, _| {
        let v = s.history.view.as_ref().expect("diff shown");
        let marks = &v.marks["Welcome"];
        assert_eq!(marks.cells.get(&at("B5")).map(|m| m.0), Some(crate::history_ui::MarkKind::Modified));
        assert_eq!(marks.cells.get(&at("D5")).map(|m| m.0), Some(crate::history_ui::MarkKind::Modified), "its total was recomputed");
        // A blank row next to a blank row: either could be the new one.
        assert!(v.items.iter().any(|i| i.text == "Welcome row 3 inserted"), "{:?}", v.items);
        assert!(v.items.iter().all(|i| !i.text.contains("width")), "sizes that survive a save aren't changes: {:?}", v.items);
        assert!(v.items.iter().any(|i| i.text == "Welcome!B5: 3 → 5"), "{:?}", v.items);
    });
    // The overlay builds, and the whole frame with it.
    let bounds = gpui::Bounds::new(gpui::point(gpui::px(0.), gpui::px(0.)), gpui::size(gpui::px(900.), gpui::px(600.)));
    let overlay = cx.update(|_, cx| {
        let all = |_: &xl_engine::Range| true;
        let pieces = |_: xl_engine::Range| vec![(bounds, bounds)];
        view.read(cx).diff_ops(&pieces, &all).len()
    });
    assert!(overlay >= 3, "B5, D5 and the totals below are highlighted: {overlay}");
    let frame = cx.update(|window, cx| view.read(cx).build_frame(bounds, window).len());
    assert!(frame > overlay);
    // Hovering a changed cell shows old → new.
    view.update(cx, |s, _| s.history.hover = Some(at("B5")));
    assert!(view.read_with(cx, |s, _| s.render_diff_card().is_some()));
    cx.run_until_parked();
    // Jump to a listed change.
    let i = view.read_with(cx, |s, _| s.history.view.as_ref().unwrap().items.iter().position(|i| i.text.starts_with("Welcome!B5")).unwrap());
    view.update(cx, |s, _| s.jump_to_change(i));
    view.read_with(cx, |s, _| assert_eq!(s.active, at("B5")));

    // Name it.
    view.update(cx, |s, _| s.start_naming(older.clone()));
    cx.simulate_input("Before tea");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |s, _| assert_eq!(s.history.panel.as_ref().unwrap().versions[1].label.as_deref(), Some("Before tea")));

    // Restore it as an undoable edit.
    view.update(cx, |s, _| s.restore_shown());
    view.read_with(cx, |s, _| {
        assert_eq!(s.wb.value(Addr::new(s.cur, at("B4"))), &Value::Number(3.0));
        assert_eq!(s.wb.input_text(Addr::new(s.cur, at("A2"))), "", "the inserted row is gone");
        assert!(s.dirty);
    });
    cx.simulate_keystrokes("ctrl-z");
    view.read_with(cx, |s, _| assert_eq!(s.wb.value(Addr::new(s.cur, at("B5"))), &Value::Number(5.0), "undo brings the edits back"));

    // Changes since last save: none, then one after an edit.
    view.update(cx, |s, cx| s.changes_since_save(cx));
    cx.run_until_parked();
    view.read_with(cx, |s, _| assert_eq!(s.history.view.as_ref().unwrap().diff.change_count(), 0));
    go(&view, cx, "A1");
    cx.simulate_input("Hello");
    cx.simulate_keystrokes("enter");
    view.update(cx, |s, _| s.refresh_diff());
    view.read_with(cx, |s, _| {
        let v = s.history.view.as_ref().unwrap();
        assert_eq!(v.diff.change_count(), 1);
        assert_eq!(v.items[0].text, "Welcome!A1: \"Welcome to xl\" → \"Hello\"");
    });

    // Compare with another file.
    let other = scratch("history-other").with_extension("csv");
    std::fs::write(&other, "x,y\n").unwrap();
    view.update(cx, |s, cx| s.compare_with_path(other.clone(), cx));
    cx.run_until_parked();
    view.read_with(cx, |s, _| assert!(s.history.view.as_ref().unwrap().diff.change_count() > 0));

    // Save As continues the history at the new path.
    let moved = scratch("history-moved").with_extension("xlsx");
    view.update(cx, |s, _| s.save_to(moved.clone()));
    view.read_with(cx, |s, _| assert_eq!(s.history.store.versions(&moved).len(), 3));
    // With autosave versions turned on, autosave records one too.
    go(&view, cx, "A1");
    cx.simulate_input("Autosaved");
    cx.simulate_keystrokes("enter");
    view.update(cx, |s, _| {
        s.history.store.policy.autosave_minutes = 10;
        s.history.last_auto = None;
        s.autosave();
    });
    view.read_with(cx, |s, _| {
        let vs = s.history.store.versions(&moved);
        assert_eq!(vs.len(), 4);
        assert!(vs[0].auto);
    });
    view.update(cx, |s, _| s.autosave());
    view.read_with(cx, |s, _| assert_eq!(s.history.store.versions(&moved).len(), 4, "not again within the interval"));
    view.update(cx, |s, _| s.close_history());
    view.read_with(cx, |s, _| assert!(s.history.panel.is_none() && s.history.view.is_none()));
    for p in [path, other, moved] {
        std::fs::remove_file(p).ok();
    }
    std::fs::remove_dir_all(view.read_with(cx, |s, _| s.history.store.root().to_path_buf())).ok();
}
