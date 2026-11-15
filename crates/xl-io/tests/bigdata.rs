//! Sheets past Excel's row limit: xlsx refuses them, CSV keeps every row.

use xl_engine::bulk::ColumnBuilder;
use xl_engine::{Addr, CellRef, Value, Workbook, XLSX_MAX_ROWS};

fn tall(rows: u32) -> Workbook {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let (mut a, mut b) = (ColumnBuilder::new(0), ColumnBuilder::new(0));
    for r in 0..rows {
        a.push_number(r as f64);
        b.push_text(if r % 2 == 0 { "007" } else { "x,y" });
    }
    wb.load_columns(s, 0, vec![a.finish(), b.finish()]).unwrap();
    wb
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("xl-io-bigdata-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn xlsx_refuses_rows_past_its_limit() {
    let wb = tall(XLSX_MAX_ROWS + 1);
    assert_eq!(xl_io::xlsx_overflow(&wb), Some(("Sheet1".to_string(), XLSX_MAX_ROWS + 1)));
    let dir = scratch("refuse");
    let path = dir.join("tall.xlsx");
    let err = xl_io::save(&wb, &path, wb.sheet_ids()[0], None).unwrap_err();
    assert!(err.contains("1048577 rows") && err.contains("1048576") && err.contains(".csv"), "{err}");
    assert!(!path.exists(), "nothing written");
    // Exactly at the limit is fine.
    assert_eq!(xl_io::xlsx_overflow(&tall(XLSX_MAX_ROWS)), None);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn csv_round_trips_a_tall_sheet() {
    let rows = XLSX_MAX_ROWS + 10;
    let wb = tall(rows);
    let dir = scratch("csv");
    let path = dir.join("tall.csv");
    xl_io::save(&wb, &path, wb.sheet_ids()[0], None).unwrap();
    let loaded = xl_io::open(&path).unwrap();
    assert_eq!(loaded.cells, 2 * rows as usize);
    let w = &loaded.workbook;
    let s = w.sheet_ids()[0];
    let last = CellRef::new(rows - 1, 0);
    assert_eq!(w.value(Addr::new(s, last)), &Value::Number((rows - 1) as f64));
    assert_eq!(w.value(Addr::new(s, CellRef::new(rows - 1, 1))), &Value::text("x,y"), "quoted on save, unquoted on load");
    assert_eq!(w.value(Addr::new(s, CellRef::new(rows - 2, 1))), &Value::text("007"), "leading zeros survive the round trip");
    assert_eq!(w.store(s).extent(), Some(CellRef::new(rows - 1, 1)));
    std::fs::remove_dir_all(&dir).ok();
}
