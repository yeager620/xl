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

fn setup(cx: &mut TestAppContext) -> (Entity<Spreadsheet>, &mut VisualTestContext) {
    let (view, cx) = cx.add_window_view(|window, cx| Spreadsheet::new(None, window, cx));
    view.update(cx, |s, _| {
        s.wb = fixture();
        s.cur = s.wb.sheet_ids()[0];
        s.active = CellRef::parse("B4").unwrap();
        s.anchor = s.active;
        s.dirty = false;
        // Never touch the real recovery directory from tests.
        s.recovery_offer = None;
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        s.recovery_dir = std::env::temp_dir().join(format!("xl-test-recovery-{}-{n}", std::process::id()));
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
