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

#[test]
fn array_formulas_keep_their_kind_when_the_sheet_is_rewritten() {
    let src = tmp("arrays.xlsx");
    {
        let mut book = XlsxBook::new();
        let ws = book.add_worksheet();
        for r in 0..3 {
            ws.write_number(r, 0, (r + 1) as f64).unwrap();
        }
        ws.write_dynamic_array_formula(0, 2, 2, 2, "=_xlfn._xlws.SORT(A1:A3,1,-1)").unwrap();
        ws.write_array_formula(0, 4, 0, 4, "{=SUM(A1:A3*2)}").unwrap();
        book.save(&src).unwrap();
    }
    let mut loaded = xl_io::open(&src).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    loaded.workbook.set_input(Addr::new(s, CellRef::parse("G1").unwrap()), "edited").unwrap();
    let out = tmp("arrays-edit.xlsx");
    xl_io::save(&loaded.workbook, &out, s, loaded.package.as_ref()).unwrap();
    let sheet = text(&parts(&out), "xl/worksheets/sheet1.xml");
    let before = text(&parts(&src), "xl/worksheets/sheet1.xml");
    let c1 = |x: &str| x.split("<c r=\"C1\"").nth(1).map(|t| t.split("</c>").next().unwrap_or("").to_string()).unwrap_or_default();
    assert!(c1(&before).contains("cm=\"1\"") && c1(&before).contains("t=\"array\""), "fixture: {}", c1(&before));
    assert!(c1(&sheet).contains("cm=\"1\""), "dynamic array metadata kept: {}", c1(&sheet));
    assert!(c1(&sheet).contains("t=\"array\"") && c1(&sheet).contains("ref=\"C1:C3\""), "array kind kept: {}", c1(&sheet));
    assert!(sheet.contains("<c r=\"G1\""));
}

#[test]
fn dates_in_1904_files_show_and_save_correctly() {
    use std::io::Write;
    let plain = tmp("dates1900.xlsx");
    {
        let mut book = XlsxBook::new();
        let ws = book.add_worksheet();
        // 1904-based serial 0 is 1904-01-01; 1462 days later is 1908-01-02.
        ws.write_number_with_format(0, 0, 1462.0, &Format::new().set_num_format("yyyy-mm-dd")).unwrap();
        ws.write_number_with_format(1, 0, 0.5, &Format::new().set_num_format("h:mm")).unwrap();
        book.save(&plain).unwrap();
    }
    // Flip the file to the 1904 system.
    let src = tmp("dates1904.xlsx");
    {
        let mut zin = zip::ZipArchive::new(std::fs::File::open(&plain).unwrap()).unwrap();
        let mut zout = zip::ZipWriter::new(std::fs::File::create(&src).unwrap());
        for i in 0..zin.len() {
            let mut f = zin.by_index(i).unwrap();
            let name = f.name().unwrap().to_string();
            let mut b = Vec::new();
            f.read_to_end(&mut b).unwrap();
            if name == "xl/workbook.xml" {
                b = String::from_utf8(b).unwrap().replace("<workbookPr ", "<workbookPr date1904=\"1\" ").into_bytes();
            }
            zout.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            zout.write_all(&b).unwrap();
        }
        zout.finish().unwrap();
    }
    let mut loaded = xl_io::open(&src).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    let shown = |wb: &xl_engine::Workbook, a1: &str| wb.display(Addr::new(s, CellRef::parse(a1).unwrap())).text;
    assert_eq!(shown(&loaded.workbook, "A1"), "1908-01-02");
    assert_eq!(shown(&loaded.workbook, "A2"), "12:00", "pure times are unaffected");
    loaded.workbook.set_input(Addr::new(s, CellRef::parse("B1").unwrap()), "x").unwrap();
    let out = tmp("dates1904-edit.xlsx");
    xl_io::save(&loaded.workbook, &out, s, loaded.package.as_ref()).unwrap();
    let sheet = text(&parts(&out), "xl/worksheets/sheet1.xml");
    assert!(sheet.contains("<v>1462</v>"), "written back in the file's own date system: {sheet}");
    let back = xl_io::open(&out).unwrap();
    assert_eq!(back.workbook.display(Addr::new(back.workbook.sheet_ids()[0], CellRef::parse("A1").unwrap())).text, "1908-01-02");
}

#[test]
fn notes_are_read_edited_and_added() {
    use xl_engine::Note;
    // Edit the note in a file that has one.
    let src = tmp("notes1.xlsx");
    rich_file(&src);
    let mut loaded = xl_io::open(&src).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    let a2 = CellRef::parse("A2").unwrap();
    assert_eq!(loaded.workbook.layout(s).notes.get(&a2).map(|n| n.text.as_str()), Some("Best seller"));
    assert!(!loaded.unsupported.contains(&"comments"), "notes are modeled now");
    loaded.workbook.layout_mut(s).unwrap().notes.insert(a2, Note { author: "Ann".into(), text: "Restock & reorder".into() });
    loaded.workbook.layout_mut(s).unwrap().notes.insert(CellRef::parse("B3").unwrap(), Note { author: "Bo".into(), text: "Check count".into() });
    assert_eq!(xl_io::save_plan(&loaded.workbook, loaded.package.as_ref().unwrap()), SavePlan::Patch);
    let out = tmp("notes1-edit.xlsx");
    xl_io::save(&loaded.workbook, &out, s, loaded.package.as_ref()).unwrap();
    let back = xl_io::open(&out).unwrap();
    let notes = &back.workbook.layout(back.workbook.sheet_ids()[0]).notes;
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[&a2], Note { author: "Ann".into(), text: "Restock & reorder".into() });
    let vml = parts(&out).into_iter().find(|(k, _)| k.ends_with(".vml")).map(|(_, v)| String::from_utf8_lossy(&v).to_string()).unwrap();
    assert_eq!(vml.matches("ObjectType=\"Note\"").count(), 2, "one note box each");
    // Add the first note to a file without any.
    let plain = tmp("notes2.xlsx");
    {
        let mut book = XlsxBook::new();
        book.add_worksheet().write_number(0, 0, 7).unwrap();
        book.save(&plain).unwrap();
    }
    let mut loaded = xl_io::open(&plain).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    loaded.workbook.layout_mut(s).unwrap().notes.insert(CellRef::new(0, 0), Note { author: "Cy".into(), text: "Lucky number".into() });
    let out = tmp("notes2-edit.xlsx");
    xl_io::save(&loaded.workbook, &out, s, loaded.package.as_ref()).unwrap();
    let p = parts(&out);
    assert!(text(&p, "xl/worksheets/sheet1.xml").contains("<legacyDrawing r:id=\"rIdXlVml\"/>"));
    assert!(p.contains_key("xl/worksheets/_rels/sheet1.xml.rels"));
    assert!(text(&p, "[Content_Types].xml").contains("comments1.xml"));
    let back = xl_io::open(&out).unwrap();
    assert_eq!(back.workbook.layout(back.workbook.sheet_ids()[0]).notes[&CellRef::new(0, 0)].text, "Lucky number");
    // Fresh writes keep notes too.
    let fresh = tmp("notes3.xlsx");
    xl_io::save(&back.workbook, &fresh, s, None).unwrap();
    assert_eq!(xl_io::open(&fresh).unwrap().workbook.layout(s).notes.len(), 1);
}

#[test]
fn hyperlinks_are_read_edited_and_saved() {
    use xl_engine::Link;
    let src = tmp("links.xlsx");
    rich_file(&src); // A9 links to https://example.com
    let mut loaded = xl_io::open(&src).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    let a9 = CellRef::parse("A9").unwrap();
    assert_eq!(loaded.workbook.layout(s).links.get(&a9).map(|l| l.target()), Some("https://example.com"));
    assert!(!loaded.unsupported.contains(&"hyperlinks"));
    // Unchanged links survive an inserted row without being rewritten as new ones.
    loaded.workbook.apply_shift(xl_engine::Shift::InsertRows { sheet: s, at: 0, count: 1 });
    let a10 = CellRef::parse("A10").unwrap();
    assert!(loaded.workbook.layout(s).links.contains_key(&a10));
    // Add a web link and a link to a place.
    {
        let l = loaded.workbook.layout_mut(s).unwrap();
        l.links.insert(CellRef::parse("C1").unwrap(), Link::Url { url: "https://xl.example/docs?a=1&b=2".into(), tooltip: Some("Docs".into()) });
        l.links.insert(CellRef::parse("C2").unwrap(), Link::Place { location: "'Data'!B5".into(), tooltip: None });
    }
    assert_eq!(xl_io::save_plan(&loaded.workbook, loaded.package.as_ref().unwrap()), SavePlan::Patch);
    let out = tmp("links-edit.xlsx");
    let pkg2 = xl_io::save(&loaded.workbook, &out, s, loaded.package.as_ref()).unwrap().unwrap();
    let back = xl_io::open(&out).unwrap();
    let links = &back.workbook.layout(back.workbook.sheet_ids()[0]).links;
    assert_eq!(links.len(), 3);
    assert_eq!(links[&a10].target(), "https://example.com");
    assert_eq!(links[&CellRef::parse("C1").unwrap()], Link::Url { url: "https://xl.example/docs?a=1&b=2".into(), tooltip: Some("Docs".into()) });
    assert_eq!(links[&CellRef::parse("C2").unwrap()].target(), "'Data'!B5");
    // Saving again doesn't pile up relationships.
    let mut again = back.workbook;
    again.layout_mut(again.sheet_ids()[0]).unwrap().links.remove(&CellRef::parse("C2").unwrap());
    let out2 = tmp("links-edit2.xlsx");
    xl_io::save(&again, &out2, again.sheet_ids()[0], Some(&pkg2)).unwrap();
    let rels = text(&parts(&out2), "xl/worksheets/_rels/sheet1.xml.rels");
    assert_eq!(rels.matches("relationships/hyperlink\"").count(), 2, "{rels}");
}

#[test]
fn data_validation_is_read_edited_and_saved() {
    use std::sync::Arc;
    use xl_engine::formula::parse_formula;
    use xl_engine::validation::{ListSource, Validation, ValidationKind, ValidationOp, ValidationRule};
    let src = tmp("dv.xlsx");
    rich_file(&src); // B2:B4 whole number between 0 and 100
    let mut loaded = xl_io::open(&src).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    let wb = &loaded.workbook;
    let v = wb.validation_at(s, CellRef::parse("B3").unwrap()).expect("rule read");
    assert!(matches!(&v.rule, ValidationRule::Compare { kind: ValidationKind::Whole, op: ValidationOp::Between, .. }));
    assert!(wb.check_value(s, CellRef::parse("B3").unwrap(), &Value::Number(50.0)).is_ok());
    assert!(wb.check_value(s, CellRef::parse("B3").unwrap(), &Value::Number(150.0)).is_err());
    assert!(wb.check_value(s, CellRef::parse("B3").unwrap(), &Value::Number(2.5)).is_err(), "whole numbers only");
    assert!(!loaded.unsupported.contains(&"data validation"));
    // Add a dropdown list and a list from cells.
    let e = |src: &str| Arc::new(parse_formula(src, loaded.workbook.sheets()).unwrap());
    let from_cells = e("$A$2:$A$4");
    {
        let l = loaded.workbook.layout_mut(s).unwrap();
        let mut colors = Validation::new(vec![xl_engine::Range::new(CellRef::parse("C2").unwrap(), CellRef::parse("C9").unwrap())], ValidationRule::List(ListSource::Items(vec!["Red".into(), "Green".into(), "Blue, light".into()])));
        colors.prompt = Some(("Color".into(), "Pick one".into()));
        colors.error = Some(("Oops".into(), "Not a color".into()));
        l.validations.push(colors);
        l.validations.push(Validation::new(vec![xl_engine::Range::single(CellRef::parse("D2").unwrap())], ValidationRule::List(ListSource::Cells(from_cells))));
    }
    let wb = &loaded.workbook;
    assert_eq!(wb.list_choices(s, wb.validation_at(s, CellRef::parse("D2").unwrap()).unwrap()), ["Pen", "Pad", "Ink"]);
    assert!(wb.check_value(s, CellRef::parse("C5").unwrap(), &Value::text("green")).is_ok());
    assert!(wb.check_value(s, CellRef::parse("C5").unwrap(), &Value::text("Purple")).is_err());
    assert_eq!(xl_io::save_plan(&loaded.workbook, loaded.package.as_ref().unwrap()), SavePlan::Patch);
    for (name, pkg) in [("dv-patch.xlsx", loaded.package.as_ref()), ("dv-fresh.xlsx", None)] {
        let out = tmp(name);
        xl_io::save(&loaded.workbook, &out, s, pkg).unwrap();
        let back = xl_io::open(&out).unwrap();
        let vs = &back.workbook.layout(back.workbook.sheet_ids()[0]).validations;
        assert_eq!(vs.len(), 3, "{name}");
        let colors = vs.iter().find(|v| v.ranges[0].start == CellRef::parse("C2").unwrap()).unwrap();
        assert_eq!(colors.rule, ValidationRule::List(ListSource::Items(vec!["Red".into(), "Green".into(), "Blue, light".into()])).clone().pipe_items(), "{name}");
        assert_eq!(colors.prompt, Some(("Color".into(), "Pick one".into())), "{name}");
    }
}

/// Excel can't store a comma inside a typed-in list item (it splits on commas).
trait PipeItems {
    fn pipe_items(self) -> Self;
}

impl PipeItems for xl_engine::validation::ValidationRule {
    fn pipe_items(self) -> Self {
        use xl_engine::validation::{ListSource, ValidationRule};
        match self {
            ValidationRule::List(ListSource::Items(items)) => ValidationRule::List(ListSource::Items(items.iter().flat_map(|i| i.split(',').map(|p| p.trim().to_string())).collect())),
            other => other,
        }
    }
}

#[test]
fn moved_and_resized_charts_are_reanchored() {
    let src = tmp("chartmove.xlsx");
    rich_file(&src);
    let mut loaded = xl_io::open(&src).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    {
        let c = &mut loaded.workbook.layout_mut(s).unwrap().charts[0];
        c.anchor = CellRef::new(10, 2);
        c.offset = (12.0, 4.0);
        c.size = (400.0, 220.0);
    }
    // Plus an inserted row above, which moves it further down by one.
    loaded.workbook.apply_shift(xl_engine::Shift::InsertRows { sheet: s, at: 0, count: 1 });
    assert_eq!(xl_io::save_plan(&loaded.workbook, loaded.package.as_ref().unwrap()), SavePlan::Patch);
    let out = tmp("chartmove-edit.xlsx");
    xl_io::save(&loaded.workbook, &out, s, loaded.package.as_ref()).unwrap();
    let back = xl_io::open(&out).unwrap();
    let c = &back.workbook.layout(back.workbook.sheet_ids()[0]).charts[0];
    assert_eq!(c.anchor, CellRef::new(11, 2));
    assert!((c.offset.0 - 12.0).abs() < 2.0 && (c.offset.1 - 4.0).abs() < 2.0, "{:?}", c.offset);
    assert!((c.size.0 - 400.0).abs() < 3.0 && (c.size.1 - 220.0).abs() < 3.0, "{:?}", c.size);
    // The chart part itself (styling and all) is untouched apart from the shifted series.
    assert!(text(&parts(&out), "xl/charts/chart1.xml").contains("Data!$B$3:$B$5"));
}
