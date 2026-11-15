//! Headless tests for big data: tall sheets, SQL queries, CSV loading with
//! progress/cancel, and the xlsx row limit.

use gpui::{Entity, TestAppContext, VisualTestContext};
use xl_engine::bulk::ColumnBuilder;
use xl_engine::{Addr, CellRef, MAX_ROWS, Value, Workbook, XLSX_MAX_ROWS};

use crate::app::Spreadsheet;
use crate::paint::Op;

fn scratch(kind: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("xl-bigtest-{kind}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup(cx: &mut TestAppContext, wb: Workbook) -> (Entity<Spreadsheet>, &mut VisualTestContext) {
    let (view, cx) = cx.add_window_view(|window, cx| Spreadsheet::new(None, window, cx));
    view.update(cx, |s, cx| {
        s.recovery_offer = None;
        s.recovery_dir = scratch("recovery");
        s.recent = crate::recent::Recent::load(&scratch("recent"));
        s.home = None;
        s.wb = wb;
        s.cur = s.wb.sheet_ids()[0];
        s.active = CellRef::new(0, 0);
        s.anchor = s.active;
        cx.notify();
    });
    cx.run_until_parked();
    (view, cx)
}

/// Column A holds its row index on every row of the sheet.
fn tall_book(rows: u32) -> Workbook {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let mut b = ColumnBuilder::new(0);
    b.reserve(rows as usize);
    for r in 0..rows {
        b.push_number(r as f64);
    }
    wb.load_columns(s, 0, vec![b.finish()]).unwrap();
    wb
}

fn painted_texts(view: &Entity<Spreadsheet>, cx: &mut VisualTestContext) -> Vec<String> {
    view.update_in(cx, |s, window, _| {
        let vp = s.viewport;
        s.build_frame(vp, window).into_iter().filter_map(|op| if let Op::Text { line, .. } = op { Some(line.text.to_string()) } else { None }).collect()
    })
}

#[gpui::test]
fn tall_sheet_scrolls_to_the_last_row_and_paints(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx, tall_book(MAX_ROWS));
    // Ctrl+Down jumps to the last row of data: the very last row of the sheet.
    cx.simulate_keystrokes("ctrl-down");
    view.read_with(cx, |s, _| {
        assert_eq!(s.active, CellRef::new(MAX_ROWS - 1, 0));
        assert!(s.scroll_y > 300_000_000.0, "scrolled ~16.7M rows down: {}", s.scroll_y);
        let y = s.grid_y(s.active.row);
        assert!(y >= 0.0 && y + s.row_h(s.active.row) <= s.grid_h() + 0.5, "the active cell is in view: {y}");
        assert!(s.header_w() > crate::app::HEADER_W, "eight-digit row numbers widen the header");
    });
    cx.run_until_parked();
    let texts = painted_texts(&view, cx);
    assert!(texts.iter().any(|t| t == "16777216"), "the last row header is painted");
    assert!(texts.iter().any(|t| t == "16777215"), "the last cell's value is painted");
    // The name box goes anywhere in the sheet.
    view.update(cx, |s, _| s.name_box_go("C9999999"));
    view.read_with(cx, |s, _| assert_eq!(s.active, CellRef::new(9_999_998, 2)));
    let texts = painted_texts(&view, cx);
    assert!(texts.iter().any(|t| t == "9999999"));
    assert!(texts.iter().any(|t| t == "9999998"));
    // Ctrl+Up goes back to the top; the selection summary of the whole
    // column comes from compact storage without materializing it.
    cx.simulate_keystrokes("ctrl-up");
    view.read_with(cx, |s, _| assert_eq!(s.active.row, 0));
    view.update(cx, |s, _| {
        s.select(CellRef::new(0, 0), false);
        s.select(CellRef::new(MAX_ROWS - 1, 0), true);
    });
    view.read_with(cx, |s, _| {
        let summary = s.selection_summary().unwrap();
        assert!(summary.iter().any(|(k, v)| *k == "Count" && v == "16777216"), "{summary:?}");
        assert!(s.wb.compact_cache_bytes() < 100 << 20, "nothing big was materialized");
    });
    // A whole-column formula over all 16.7M rows.
    view.update(cx, |s, _| {
        let a = s.at(CellRef::new(0, 2));
        s.wb.set_input(a, "=SUM(A:A)").unwrap();
        let n = MAX_ROWS as f64;
        assert_eq!(s.wb.value(a), &Value::Number(n * (n - 1.0) / 2.0));
    });
}

fn sales_book() -> Workbook {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let rows = [["region", "amount"], ["East", "10"], ["West", "20"], ["East", "5"]];
    let mut items = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            items.push((Addr::new(s, CellRef::new(r as u32, c as u32)), v.to_string()));
        }
    }
    wb.set_inputs(items);
    wb
}

#[gpui::test]
fn query_into_a_new_sheet_then_refresh(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx, sales_book());
    view.update(cx, |s, _| {
        s.open_form(crate::solver_ui::FormKind::Query);
        let f = s.form.as_mut().unwrap();
        f.fields[0].2 = "SELECT region, SUM(amount) AS total FROM Sheet1 GROUP BY region ORDER BY region".into();
        s.run_form();
    });
    view.read_with(cx, |s, _| {
        assert!(s.form.is_none(), "the dialog closes: {:?}", s.message);
        assert_eq!(s.wb.sheet_name(s.cur), "Query1");
        let v = |a1: &str| s.wb.value(s.at(CellRef::parse(a1).unwrap())).clone();
        assert_eq!(v("A1"), Value::text("region"));
        assert_eq!(v("B1"), Value::text("total"));
        assert_eq!((v("A2"), v("B2")), (Value::text("East"), Value::Number(15.0)));
        assert_eq!((v("A3"), v("B3")), (Value::text("West"), Value::Number(20.0)));
    });
    // Edit the source, refresh: the results follow.
    view.update(cx, |s, _| {
        let src = s.wb.sheet_ids()[0];
        s.wb.set_input(Addr::new(src, CellRef::new(4, 0)), "North").unwrap();
        s.wb.set_input(Addr::new(src, CellRef::new(4, 1)), "7").unwrap();
        s.mark_dirty();
        s.refresh_query();
    });
    view.read_with(cx, |s, _| {
        let v = |a1: &str| s.wb.value(s.at(CellRef::parse(a1).unwrap())).clone();
        assert_eq!((v("A3"), v("B3")), (Value::text("North"), Value::Number(7.0)), "{:?}", s.message);
        assert_eq!((v("A4"), v("B4")), (Value::text("West"), Value::Number(20.0)));
    });
}

#[gpui::test]
fn query_into_a_chosen_cell_never_overwrites(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx, sales_book());
    view.update(cx, |s, _| s.run_query_form("SELECT COUNT(*) AS n FROM Sheet1".into(), "Sheet1!B2".into()));
    view.read_with(cx, |s, _| {
        assert!(s.message.as_ref().is_some_and(|(m, err)| *err && m.contains("overwrite")), "{:?}", s.message);
        assert_eq!(s.wb.value(s.at(CellRef::parse("B2").unwrap())), &Value::Number(10.0));
    });
    view.update(cx, |s, _| s.run_query_form("SELECT COUNT(*) AS n FROM Sheet1".into(), "D1".into()));
    view.read_with(cx, |s, _| {
        assert_eq!(s.wb.value(s.at(CellRef::parse("D1").unwrap())), &Value::text("n"));
        assert_eq!(s.wb.value(s.at(CellRef::parse("D2").unwrap())), &Value::Number(3.0));
    });
    // Refreshing replaces its own results in place, and undo restores.
    view.update(cx, |s, _| {
        s.wb.set_input(s.at(CellRef::parse("A5").unwrap()), "South").unwrap();
        s.mark_dirty();
        s.select(CellRef::parse("D2").unwrap(), false);
        s.refresh_query();
    });
    view.read_with(cx, |s, _| assert_eq!(s.wb.value(s.at(CellRef::parse("D2").unwrap())), &Value::Number(4.0)));
    cx.simulate_keystrokes("ctrl-z");
    view.read_with(cx, |s, _| assert_eq!(s.wb.value(s.at(CellRef::parse("D2").unwrap())), &Value::Number(3.0)));
}

#[gpui::test]
fn too_tall_for_xlsx_offers_csv(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx, tall_book(XLSX_MAX_ROWS + 1));
    let dir = scratch("xlsx-limit");
    let xlsx = dir.join("tall.xlsx");
    view.update(cx, |s, _| s.save_to(xlsx.clone()));
    view.read_with(cx, |s, _| {
        let (_, why) = s.big.xlsx_refused.clone().expect("asked instead of saving");
        assert!(why.contains("1048577 rows") && why.contains(".csv"), "{why}");
        assert!(s.dirty || s.path.is_none());
    });
    assert!(!xlsx.exists());
    cx.simulate_keystrokes("enter");
    let csv = dir.join("tall.csv");
    view.read_with(cx, |s, _| {
        assert!(s.big.xlsx_refused.is_none());
        assert_eq!(s.path.as_deref(), Some(csv.as_path()), "{:?}", s.message);
    });
    let text = std::fs::read_to_string(&csv).unwrap();
    assert_eq!(text.lines().count(), XLSX_MAX_ROWS as usize + 1);
    assert_eq!(text.lines().last(), Some("1048576"));
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui::test]
fn csv_opens_in_the_background_and_can_be_cancelled(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx, sales_book());
    let dir = scratch("csv-open");
    let path = dir.join("data.csv");
    std::fs::write(&path, "id,zip\n007,02134\n8,90210\n").unwrap();
    // Cancelled before it runs: nothing changes.
    view.update(cx, |s, cx| {
        s.load(path.clone(), cx);
        assert!(s.big.progress.is_some());
        s.cancel_load();
    });
    cx.run_until_parked();
    view.read_with(cx, |s, _| {
        assert!(s.loading.is_none());
        assert_eq!(s.wb.sheet_name(s.cur), "Sheet1", "{:?}", s.message);
        assert!(s.message.as_ref().is_some_and(|(m, _)| m.starts_with("Stopped opening")));
    });
    view.update(cx, |s, cx| s.load(path.clone(), cx));
    cx.run_until_parked();
    view.read_with(cx, |s, _| {
        assert_eq!(s.wb.sheet_name(s.cur), "data");
        assert_eq!(s.wb.value(s.at(CellRef::parse("A2").unwrap())), &Value::text("007"));
        assert_eq!(s.wb.value(s.at(CellRef::parse("B3").unwrap())), &Value::Number(90210.0));
    });
    std::fs::remove_dir_all(&dir).ok();
}
