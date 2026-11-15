//! Saving into an opened xlsx keeps what xl doesn't model.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use rust_xlsxwriter::{Chart, ChartType, DataValidation, DataValidationRule, Format, Note, Url, Workbook as XlsxBook};
use xl_engine::{Addr, CellRef, Style, Value};
use xl_io::SavePlan;

fn tmp(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("xl-preserve-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Every part of a package, decompressed.
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

/// A file with things xl doesn't model: notes, validation, a hyperlink, a
/// chart, rotated text, a print area.
fn rich_file(path: &Path) {
    let mut book = XlsxBook::new();
    let ws = book.add_worksheet();
    ws.set_name("Data").unwrap();
    ws.write_string(0, 0, "Item").unwrap();
    ws.write_string(0, 1, "Qty").unwrap();
    for (r, (item, q)) in [("Pen", 3), ("Pad", 5), ("Ink", 2)].iter().enumerate() {
        ws.write_string(r as u32 + 1, 0, *item).unwrap();
        ws.write_number(r as u32 + 1, 1, *q as f64).unwrap();
    }
    ws.write_formula(4, 1, "=SUM(B2:B4)").unwrap();
    ws.write_string_with_format(6, 0, "Rotated", &Format::new().set_rotation(45)).unwrap();
    ws.insert_note(1, 0, &Note::new("Best seller")).unwrap();
    ws.add_data_validation(1, 1, 3, 1, &DataValidation::new().allow_whole_number(DataValidationRule::Between(0, 100))).unwrap();
    ws.write_url(8, 0, Url::new("https://example.com").set_text("Example")).unwrap();
    let mut chart = Chart::new(ChartType::Column);
    chart.add_series().set_values(("Data", 1, 1, 3, 1)).set_categories(("Data", 1, 0, 3, 0));
    ws.insert_chart(1, 4, &chart).unwrap();
    ws.set_print_area(0, 0, 8, 1).unwrap();
    book.save(path).unwrap();
}

#[test]
fn saving_without_changes_keeps_every_part() {
    let src = tmp("rich.xlsx");
    rich_file(&src);
    let loaded = xl_io::open(&src).unwrap();
    let pkg = loaded.package.as_ref().expect("package kept");
    assert_eq!(xl_io::save_plan(&loaded.workbook, pkg), SavePlan::Patch);
    let out = tmp("rich-noop.xlsx");
    xl_io::save(&loaded.workbook, &out, loaded.workbook.sheet_ids()[0], Some(pkg)).unwrap();
    assert_eq!(parts(&src), parts(&out), "a save with no edits changes nothing");
}

#[test]
fn editing_cells_keeps_everything_else() {
    let src = tmp("rich2.xlsx");
    rich_file(&src);
    let mut loaded = xl_io::open(&src).unwrap();
    let wb = &mut loaded.workbook;
    let s = wb.sheet_ids()[0];
    let a = |a1: &str| Addr::new(s, CellRef::parse(a1).unwrap());
    wb.set_input(a("B3"), "50").unwrap();
    wb.set_input(a("C1"), "Notes").unwrap(); // a new string
    wb.set_style(a("C1"), &Style { bold: true, color: Some(0xC00000), ..Style::default() }); // a new style
    wb.layout_mut(s).unwrap().merges.push(xl_engine::Range::new(CellRef::parse("C3").unwrap(), CellRef::parse("D3").unwrap()));
    let pkg = loaded.package.as_ref().unwrap();
    assert_eq!(xl_io::save_plan(&loaded.workbook, pkg), SavePlan::Patch);
    let out = tmp("rich2-edit.xlsx");
    xl_io::save(&loaded.workbook, &out, s, Some(pkg)).unwrap();
    let (before, after) = (parts(&src), parts(&out));
    // Everything but the sheet, shared strings and styles is untouched.
    for (name, bytes) in &before {
        if ["xl/worksheets/sheet1.xml", "xl/sharedStrings.xml", "xl/styles.xml", "xl/calcChain.xml", "[Content_Types].xml", "xl/_rels/workbook.xml.rels"].contains(&name.as_str()) {
            continue;
        }
        assert_eq!(after.get(name), Some(bytes), "{name} changed");
    }
    let sheet = text(&after, "xl/worksheets/sheet1.xml");
    for kept in ["<dataValidations", "<hyperlinks", "<drawing ", "<legacyDrawing", "<pageMargins", "<mergeCell ref=\"C3:D3\"/>"] {
        assert!(sheet.contains(kept), "{kept} missing from the sheet:\n{sheet}");
    }
    // The rotated cell keeps its original style index (rotation isn't modeled).
    let s_of = |xml: &str, r: &str| xml.split(&format!("<c r=\"{r}\"")).nth(1).and_then(|t| t.split("s=\"").nth(1)).and_then(|t| t.split('"').next()).map(str::to_string);
    assert_eq!(s_of(&sheet, "A7"), s_of(&text(&before, "xl/worksheets/sheet1.xml"), "A7"));
    // Styles were appended to, not renumbered.
    let (sb, sa) = (text(&before, "xl/styles.xml"), text(&after, "xl/styles.xml"));
    assert!(sa.len() > sb.len());
    // Reopening: the edits are there, and so are the parts xl reports.
    let back = xl_io::open(&out).unwrap();
    let bs = back.workbook.sheet_ids()[0];
    let b = |a1: &str| back.workbook.value(Addr::new(bs, CellRef::parse(a1).unwrap())).clone();
    assert_eq!(b("B3"), Value::Number(50.0));
    assert_eq!(b("B5"), Value::Number(55.0));
    assert_eq!(b("C1"), Value::text("Notes"));
    let c1 = back.workbook.style(Addr::new(bs, CellRef::parse("C1").unwrap()));
    assert!(c1.bold && c1.color == Some(0xC00000));
    assert_eq!(back.unsupported, loaded.unsupported, "comments, validation and links are all still there");
    assert!(back.workbook.layout(bs).charts.len() == 1);
}

#[test]
fn renames_and_new_sheets_patch_the_workbook() {
    let src = tmp("plain.xlsx");
    {
        let mut book = XlsxBook::new();
        let ws = book.add_worksheet();
        ws.set_name("One").unwrap();
        ws.write_number(0, 0, 1).unwrap();
        ws.insert_note(0, 0, &Note::new("keep me")).unwrap();
        book.save(&src).unwrap();
    }
    let mut loaded = xl_io::open(&src).unwrap();
    let wb = &mut loaded.workbook;
    let one = wb.sheet_ids()[0];
    wb.rename_sheet(one, "First").unwrap();
    let two = wb.add_sheet(Some("Second")).unwrap();
    wb.set_input(Addr::new(two, CellRef::new(0, 0)), "=First!A1*10").unwrap();
    wb.define_name("Base", None, "First!$A$1").unwrap();
    let pkg = loaded.package.as_ref().unwrap();
    assert_eq!(xl_io::save_plan(&loaded.workbook, pkg), SavePlan::Patch);
    let out = tmp("plain-renamed.xlsx");
    let new_pkg = xl_io::save(&loaded.workbook, &out, one, Some(pkg)).unwrap().expect("package of the saved file");
    let back = xl_io::open(&out).unwrap();
    let ids = back.workbook.sheet_ids().to_vec();
    assert_eq!(ids.iter().map(|&i| back.workbook.sheet_name(i).to_string()).collect::<Vec<_>>(), ["First", "Second"]);
    assert_eq!(*back.workbook.value(Addr::new(ids[1], CellRef::new(0, 0))), Value::Number(10.0));
    assert!(back.workbook.names().iter().any(|d| d.name == "Base"));
    assert!(parts(&out).keys().any(|k| k.starts_with("xl/comments")), "the note survived");
    // Saving again with the returned package changes nothing.
    let again = tmp("plain-renamed-2.xlsx");
    xl_io::save(&back.workbook, &again, ids[0], Some(&new_pkg)).unwrap();
    assert_eq!(parts(&out), parts(&again));
}

#[test]
fn inserted_rows_and_renames_move_preserved_addresses() {
    let src = tmp("rich3.xlsx");
    rich_file(&src);
    let mut loaded = xl_io::open(&src).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    // Two rows above the data, then rename the sheet.
    loaded.workbook.apply_shift(xl_engine::Shift::InsertRows { sheet: s, at: 0, count: 2 });
    loaded.workbook.rename_sheet(s, "Stock List").unwrap();
    let pkg = loaded.package.as_ref().unwrap();
    assert_eq!(xl_io::save_plan(&loaded.workbook, pkg), SavePlan::Patch);
    let out = tmp("rich3-shifted.xlsx");
    xl_io::save(&loaded.workbook, &out, s, Some(pkg)).unwrap();
    let p = parts(&out);
    let sheet = text(&p, "xl/worksheets/sheet1.xml");
    assert!(sheet.contains(r#"sqref="B4:B6""#), "validation moved down two rows:\n{sheet}");
    assert!(sheet.contains(r#"ref="A11""#), "hyperlink moved");
    let comments = p.iter().find(|(k, _)| k.starts_with("xl/comments")).map(|(_, v)| String::from_utf8_lossy(v).to_string()).unwrap();
    assert!(comments.contains(r#"ref="A4""#), "{comments}");
    let vml = p.iter().find(|(k, _)| k.ends_with(".vml")).map(|(_, v)| String::from_utf8_lossy(v).to_string()).unwrap();
    assert!(vml.contains("<x:Row>3</x:Row>"), "note box follows its cell");
    let chart = text(&p, "xl/charts/chart1.xml");
    assert!(chart.contains("'Stock List'!$B$4:$B$6"), "chart series renamed and moved:\n{chart}");
    let drawing = text(&p, "xl/drawings/drawing1.xml");
    assert!(drawing.contains("<xdr:row>3</xdr:row>"), "chart anchor moved down");
    let wbx = text(&p, "xl/workbook.xml");
    assert!(wbx.contains("'Stock List'!$A$3:$B$11"), "print area renamed and moved:\n{wbx}");
    // And it all reads back consistently.
    let back = xl_io::open(&out).unwrap();
    let bs = back.workbook.sheet_ids()[0];
    assert_eq!(back.workbook.sheet_name(bs), "Stock List");
    assert_eq!(*back.workbook.value(Addr::new(bs, CellRef::parse("B7").unwrap())), Value::Number(10.0));
    let chart = &back.workbook.layout(bs).charts[0];
    assert_eq!(chart.anchor, CellRef::new(3, 4));
    assert_eq!(back.workbook.chart_data(bs, chart).series[0].1, vec![Some(3.0), Some(5.0), Some(2.0)]);
}

#[test]
fn falls_back_when_patching_would_be_unsafe() {
    let src = tmp("rich4.xlsx");
    rich_file(&src);
    // Deleting the commented cell's row: its note box can't be removed safely yet.
    let mut loaded = xl_io::open(&src).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    loaded.workbook.apply_shift(xl_engine::Shift::DeleteRows { sheet: s, at: 1, count: 1 });
    assert!(matches!(xl_io::save_plan(&loaded.workbook, loaded.package.as_ref().unwrap()), SavePlan::Rewrite(_)));
    // Cut and paste isn't adjusted in preserved parts yet.
    let mut loaded = xl_io::open(&src).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    loaded.workbook.move_range(xl_engine::Move { from_sheet: s, from: xl_engine::Range::single(CellRef::new(2, 0)), to_sheet: s, dr: 20, dc: 0 });
    match xl_io::save_plan(&loaded.workbook, loaded.package.as_ref().unwrap()) {
        SavePlan::Rewrite(why) => assert!(why.iter().any(|w| w.contains("moved"))),
        SavePlan::Patch => panic!("should not patch"),
    }
}
