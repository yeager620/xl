//! Page setup round trips: read from files, written in fresh saves, and in
//! patched saves regenerated only when changed.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use rust_xlsxwriter::Workbook as XlsxBook;
use xl_engine::page::{Orientation, PageSetup};
use xl_engine::{CellRef, Range, Shift};
use xl_io::SavePlan;

fn tmp(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("xl-page-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn parts(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut out = BTreeMap::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        let mut b = Vec::new();
        f.read_to_end(&mut b).unwrap();
        out.insert(f.name().unwrap().to_string(), b);
    }
    out
}

fn text(parts: &BTreeMap<String, Vec<u8>>, name: &str) -> String {
    String::from_utf8_lossy(&parts[name]).to_string()
}

fn range(a: &str, b: &str) -> Range {
    Range::new(CellRef::parse(a).unwrap(), CellRef::parse(b).unwrap())
}

/// A file with a full page setup, written by rust_xlsxwriter (as Excel would).
fn setup_file(path: &Path) {
    let mut book = XlsxBook::new();
    let ws = book.add_worksheet();
    ws.set_name("Report").unwrap();
    for r in 0..40u32 {
        ws.write_string(r, 0, format!("Row {r}")).unwrap();
        ws.write_number(r, 1, r as f64).unwrap();
    }
    ws.set_landscape();
    ws.set_paper_size(9);
    ws.set_margins(0.5, 0.5, 1.0, 1.0, 0.4, 0.4);
    ws.set_print_fit_to_pages(1, 0);
    ws.set_print_gridlines(true);
    ws.set_print_center_horizontally(true);
    ws.set_header("&CQuarterly &A");
    ws.set_footer("&LPage &P of &N&R&D");
    ws.set_page_breaks(&[20]).unwrap();
    ws.set_print_area(0, 0, 39, 1).unwrap();
    ws.set_repeat_rows(0, 0).unwrap();
    let other = book.add_worksheet();
    other.set_name("Other").unwrap();
    other.write_string(0, 0, "x").unwrap();
    book.save(path).unwrap();
}

#[test]
fn reads_page_setup_and_print_names() {
    let src = tmp("setup.xlsx");
    setup_file(&src);
    let loaded = xl_io::open(&src).unwrap();
    let wb = &loaded.workbook;
    let p = &wb.layout(wb.sheet_ids()[0]).page;
    assert_eq!((p.orientation, p.paper, p.fit), (Orientation::Landscape, 9, Some((1, 0))));
    assert_eq!((p.margins.left, p.margins.top, p.margins.header), (0.5, 1.0, 0.4));
    assert!(p.gridlines && p.center_h);
    assert_eq!(p.header, "&CQuarterly &A");
    assert_eq!(p.footer, "&LPage &P of &N&R&D");
    assert_eq!(p.row_breaks.iter().copied().collect::<Vec<_>>(), vec![20]);
    assert_eq!(p.print_area, Some(range("A1", "B40")));
    assert_eq!(p.title_rows, Some((0, 0)));
    assert!(wb.layout(wb.sheet_ids()[1]).page.is_default());
    // The built-in names don't show up as user names.
    assert!(wb.names().iter().all(|d| !d.name.starts_with("_xlnm")));
}

#[test]
fn fresh_saves_write_the_page_setup() {
    let src = tmp("setup-fresh-src.xlsx");
    setup_file(&src);
    let loaded = xl_io::open(&src).unwrap();
    let out = tmp("setup-fresh.xlsx");
    // No package: a fresh write.
    xl_io::save(&loaded.workbook, &out, loaded.workbook.sheet_ids()[0], None).unwrap();
    let back = xl_io::open(&out).unwrap();
    let (a, b) = (&loaded.workbook, &back.workbook);
    assert_eq!(a.layout(a.sheet_ids()[0]).page, b.layout(b.sheet_ids()[0]).page);
    assert!(b.layout(b.sheet_ids()[1]).page.is_default());
}

#[test]
fn unchanged_page_setup_stays_byte_identical() {
    let src = tmp("setup-noop.xlsx");
    setup_file(&src);
    let mut loaded = xl_io::open(&src).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    // A cell edit rewrites the sheet's data, not its page setup.
    loaded.workbook.set_input(xl_engine::Addr::new(s, CellRef::parse("C1").unwrap()), "note").unwrap();
    let pkg = loaded.package.as_ref().unwrap();
    let out = tmp("setup-noop-out.xlsx");
    xl_io::save(&loaded.workbook, &out, s, Some(pkg)).unwrap();
    let (before, after) = (parts(&src), parts(&out));
    assert_eq!(text(&before, "xl/workbook.xml"), text(&after, "xl/workbook.xml"));
    let tail = |x: &str| x[x.find("</sheetData>").unwrap()..].to_string();
    assert_eq!(tail(&text(&before, "xl/worksheets/sheet1.xml")), tail(&text(&after, "xl/worksheets/sheet1.xml")));
}

#[test]
fn changed_page_setup_is_patched_in() {
    let src = tmp("setup-edit.xlsx");
    setup_file(&src);
    let mut loaded = xl_io::open(&src).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    {
        let p = &mut loaded.workbook.layout_mut(s).unwrap().page;
        p.orientation = Orientation::Portrait;
        p.fit = None;
        p.scale = 80;
        p.footer = "&CPage &P".into();
        p.print_area = Some(range("A1", "B30"));
        p.title_rows = None;
        p.title_cols = Some((0, 0));
        p.col_breaks.insert(1);
    }
    let other = loaded.workbook.sheet_ids()[1];
    loaded.workbook.layout_mut(other).unwrap().page.print_area = Some(range("A1", "A1"));
    let pkg = loaded.package.as_ref().unwrap();
    assert_eq!(xl_io::save_plan(&loaded.workbook, pkg), SavePlan::Patch);
    let out = tmp("setup-edit-out.xlsx");
    xl_io::save(&loaded.workbook, &out, s, Some(pkg)).unwrap();
    let after = parts(&out);
    let sheet = text(&after, "xl/worksheets/sheet1.xml");
    assert!(sheet.contains("orientation=\"portrait\"") && sheet.contains("scale=\"80\""), "{sheet}");
    assert!(!sheet.contains("fitToPage=\"1\""), "{sheet}");
    let back = xl_io::open(&out).unwrap();
    let b = &back.workbook;
    assert_eq!(b.layout(b.sheet_ids()[0]).page, loaded.workbook.layout(s).page);
    assert_eq!(b.layout(b.sheet_ids()[1]).page.print_area, Some(range("A1", "A1")));
    let wbx = text(&after, "xl/workbook.xml");
    assert_eq!(wbx.matches("_xlnm.Print_Area").count(), 2, "{wbx}");
}

#[test]
fn inserted_rows_move_page_setup_consistently() {
    let src = tmp("setup-shift.xlsx");
    setup_file(&src);
    let mut loaded = xl_io::open(&src).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    loaded.workbook.apply_shift(Shift::InsertRows { sheet: s, at: 5, count: 3 });
    let p = loaded.workbook.layout(s).page.clone();
    assert_eq!(p.print_area, Some(range("A1", "B43")));
    assert_eq!(p.row_breaks.iter().copied().collect::<Vec<_>>(), vec![23]);
    let pkg = loaded.package.as_ref().unwrap();
    let out = tmp("setup-shift-out.xlsx");
    xl_io::save(&loaded.workbook, &out, s, Some(pkg)).unwrap();
    let back = xl_io::open(&out).unwrap();
    assert_eq!(back.workbook.layout(back.workbook.sheet_ids()[0]).page, p, "the file's rewritten addresses match the model");
}

#[test]
fn new_workbooks_get_page_setup_in_patched_and_fresh_files() {
    let mut wb = xl_engine::Workbook::new();
    let s = wb.sheet_ids()[0];
    wb.set_input(xl_engine::Addr::new(s, CellRef::new(0, 0)), "hi").unwrap();
    let mut p = PageSetup { headings: true, header: "&C&F".into(), title_rows: Some((0, 1)), ..Default::default() };
    p.row_breaks.insert(10);
    wb.layout_mut(s).unwrap().page = p.clone();
    let out = tmp("setup-new.xlsx");
    let pkg = xl_io::save(&wb, &out, s, None).unwrap().unwrap();
    let back = xl_io::open(&out).unwrap();
    assert_eq!(back.workbook.layout(back.workbook.sheet_ids()[0]).page, p);
    // A new sheet in a patched save carries its page setup too.
    let mut wb2 = back.workbook;
    let s2 = wb2.add_sheet(Some("Added")).unwrap();
    wb2.layout_mut(s2).unwrap().page = PageSetup { orientation: Orientation::Landscape, print_area: Some(range("A1", "C3")), ..Default::default() };
    let _ = pkg;
    let pkg2 = back.package.unwrap();
    let out2 = tmp("setup-new2.xlsx");
    xl_io::save(&wb2, &out2, s, Some(&pkg2)).unwrap();
    let back2 = xl_io::open(&out2).unwrap();
    let ids = back2.workbook.sheet_ids().to_vec();
    assert_eq!(back2.workbook.layout(ids[1]).page.orientation, Orientation::Landscape);
    assert_eq!(back2.workbook.layout(ids[1]).page.print_area, Some(range("A1", "C3")));
    assert_eq!(back2.workbook.layout(ids[0]).page, p);
}
