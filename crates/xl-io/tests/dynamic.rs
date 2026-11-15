//! Dynamic-array formulas, LET and LAMBDA in xlsx files: read, written
//! fresh, and patched into opened packages.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use rust_xlsxwriter::Workbook as XlsxBook;
use xl_engine::{Addr, CellRef, Range, Value, Workbook};
use xl_io::SavePlan;

fn tmp(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("xl-dynamic-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn parts(path: &Path) -> BTreeMap<String, String> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut out = BTreeMap::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        let mut b = Vec::new();
        f.read_to_end(&mut b).unwrap();
        out.insert(f.name().unwrap().to_string(), String::from_utf8_lossy(&b).to_string());
    }
    out
}

/// The `<c>` element of a cell in sheet XML.
fn cell_xml<'a>(sheet: &'a str, r: &str) -> &'a str {
    let start = sheet.find(&format!("<c r=\"{r}\"")).unwrap_or_else(|| panic!("no cell {r} in\n{sheet}"));
    let rest = &sheet[start..];
    let tag_end = rest.find('>').unwrap() + 1;
    if rest[..tag_end].ends_with("/>") { &rest[..tag_end] } else { &rest[..rest.find("</c>").unwrap() + 4] }
}

fn r(s: &str) -> Range {
    let (a, b) = s.split_once(':').unwrap();
    Range::new(CellRef::parse(a).unwrap(), CellRef::parse(b).unwrap())
}

struct W<'a>(&'a Workbook);
impl W<'_> {
    fn a(&self, a1: &str) -> Addr {
        Addr::new(self.0.sheet_ids()[0], CellRef::parse(a1).unwrap())
    }
    fn v(&self, a1: &str) -> Value {
        self.0.value(self.a(a1)).clone()
    }
    fn input(&self, a1: &str) -> String {
        self.0.input_text(self.a(a1))
    }
}

/// A workbook using spills, LET, LAMBDA, `A1#` and `@`.
fn dynamic_book() -> Workbook {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let a = |a1: &str| Addr::new(s, CellRef::parse(a1).unwrap());
    for (i, (name, qty)) in [("pen", 3), ("ink", 1), ("pad", 5)].iter().enumerate() {
        wb.set_input(a(&format!("A{}", i + 1)), name).unwrap();
        wb.set_input(a(&format!("B{}", i + 1)), &qty.to_string()).unwrap();
    }
    wb.define_name("Twice", None, "=LAMBDA(x, x*2)").unwrap();
    wb.set_input(a("D1"), "=SORT(FILTER(A1:B3,B1:B3>1))").unwrap();
    wb.set_input(a("G1"), "=SUM(D1#)").unwrap();
    wb.set_input(a("G2"), "=LET(q,B1:B3,SUM(q*2))").unwrap();
    wb.set_input(a("G3"), "=Twice(21)").unwrap();
    wb.set_input(a("G4"), "=@A1:A3").unwrap();
    wb.set_input(a("G5"), "=SEQUENCE(1,3)").unwrap();
    wb.set_input(a("G6"), "=MAP(B1:B2,LAMBDA(v,v+1))").unwrap();
    wb.set_input(a("G8"), "=SUM(B1:B3)").unwrap();
    wb
}

fn check_values(wb: &Workbook) {
    let w = W(wb);
    assert_eq!(wb.spill_range(w.a("D1")), Some(r("D1:E2")));
    assert_eq!(w.v("D1"), Value::text("pad"));
    assert_eq!(w.v("E2"), Value::Number(3.0));
    assert_eq!(w.v("G1"), Value::Number(8.0));
    assert_eq!(w.v("G2"), Value::Number(18.0));
    assert_eq!(w.v("G3"), Value::Number(42.0));
    assert_eq!(w.v("I5"), Value::Number(3.0));
    assert_eq!(w.v("G7"), Value::Number(2.0));
    assert_eq!(w.input("D1"), "=SORT(FILTER(A1:B3,B1:B3>1))");
    assert_eq!(w.input("E2"), "", "spilled cells read back as spills");
    assert_eq!(w.input("G1"), "=SUM(D1#)");
    assert_eq!(w.input("G2"), "=LET(q,B1:B3,SUM(q*2))");
    assert_eq!(w.input("G3"), "=Twice(21)");
    assert_eq!(w.input("G4"), "=@A1:A3");
    assert_eq!(w.input("G6"), "=MAP(B1:B2,LAMBDA(v,v+1))");
}

fn check_xml(p: &BTreeMap<String, String>) {
    let sheet = &p["xl/worksheets/sheet1.xml"];
    let d1 = cell_xml(sheet, "D1");
    assert!(d1.contains("cm=\"1\""), "{d1}");
    assert!(d1.contains("t=\"array\"") && d1.contains("ref=\"D1:E2\""), "{d1}");
    assert!(d1.contains("_xlfn._xlws.SORT(_xlfn._xlws.FILTER(A1:B3,B1:B3&gt;1))"), "{d1}");
    // Spilled cells hold values, not formulas.
    let e2 = cell_xml(sheet, "E2");
    assert!(!e2.contains("<f"), "{e2}");
    assert!(e2.contains("<v>3</v>"), "{e2}");
    assert!(cell_xml(sheet, "G1").contains("SUM(_xlfn.ANCHORARRAY(D1))"));
    assert!(cell_xml(sheet, "G2").contains("_xlfn.LET(_xlpm.q,B1:B3,SUM(_xlpm.q*2))"));
    assert!(cell_xml(sheet, "G6").contains("_xlfn.MAP(B1:B2,_xlfn.LAMBDA(_xlpm.v,_xlpm.v+1))"));
    // `@` where old Excel implied it is left out of a plain formula.
    let g4 = cell_xml(sheet, "G4");
    assert!(g4.contains("<f>A1:A3</f>") && !g4.contains("cm="), "{g4}");
    let g8 = cell_xml(sheet, "G8");
    assert!(g8.contains("<f>SUM(B1:B3)</f>") && !g8.contains("cm="), "{g8}");
    let meta = &p["xl/metadata.xml"];
    assert!(meta.contains("XLDAPR") && meta.contains("fDynamic=\"1\""), "{meta}");
    assert!(p["[Content_Types].xml"].contains("/xl/metadata.xml"));
    assert!(p["xl/_rels/workbook.xml.rels"].contains("metadata.xml"));
    assert!(p["xl/workbook.xml"].contains("_xlfn.LAMBDA(_xlpm.x"), "{}", p["xl/workbook.xml"]);
}

#[test]
fn fresh_write_round_trips_dynamic_arrays() {
    let wb = dynamic_book();
    check_values(&wb);
    let path = tmp("fresh.xlsx");
    xl_io::save(&wb, &path, wb.sheet_ids()[0], None).unwrap();
    check_xml(&parts(&path));
    let back = xl_io::open(&path).unwrap();
    check_values(&back.workbook);
}

#[test]
fn patched_save_adds_metadata_and_round_trips() {
    // A plain file, without dynamic arrays or metadata.xml.
    let src = tmp("plain.xlsx");
    let mut book = XlsxBook::new();
    let ws = book.add_worksheet();
    for (i, (name, qty)) in [("pen", 3), ("ink", 1), ("pad", 5)].iter().enumerate() {
        ws.write_string(i as u32, 0, *name).unwrap();
        ws.write_number(i as u32, 1, *qty as f64).unwrap();
    }
    ws.write_formula(7, 6, "=SUM(B1:B3)").unwrap();
    book.save(&src).unwrap();
    assert!(!parts(&src).contains_key("xl/metadata.xml"));
    let mut loaded = xl_io::open(&src).unwrap();
    let wb = &mut loaded.workbook;
    let s = wb.sheet_ids()[0];
    let a = |a1: &str| Addr::new(s, CellRef::parse(a1).unwrap());
    wb.define_name("Twice", None, "=LAMBDA(x, x*2)").unwrap();
    for (c, f) in [("D1", "=SORT(FILTER(A1:B3,B1:B3>1))"), ("G1", "=SUM(D1#)"), ("G2", "=LET(q,B1:B3,SUM(q*2))"), ("G3", "=Twice(21)"), ("G4", "=@A1:A3"), ("G5", "=SEQUENCE(1,3)"), ("G6", "=MAP(B1:B2,LAMBDA(v,v+1))")] {
        wb.set_input(a(c), f).unwrap();
    }
    check_values(wb);
    let pkg = loaded.package.as_ref().unwrap();
    assert_eq!(xl_io::save_plan(&loaded.workbook, pkg), SavePlan::Patch);
    let out = tmp("plain-patched.xlsx");
    let pkg2 = xl_io::save(&loaded.workbook, &out, s, Some(pkg)).unwrap().unwrap();
    check_xml(&parts(&out));
    let back = xl_io::open(&out).unwrap();
    check_values(&back.workbook);

    // Saving again into the patched file: the spill grows, XLDAPR is reused.
    let mut again = back;
    let wb = &mut again.workbook;
    wb.set_input(a("B2"), "4").unwrap();
    assert_eq!(wb.spill_range(a("D1")), Some(r("D1:E3")));
    wb.set_input(a("J1"), "=SEQUENCE(2)").unwrap();
    let out2 = tmp("plain-patched-again.xlsx");
    let pkg = again.package.as_ref().unwrap();
    xl_io::save(&again.workbook, &out2, s, Some(pkg)).unwrap();
    let p = parts(&out2);
    assert_eq!(p["xl/metadata.xml"].matches("name=\"XLDAPR\"").count(), 2, "one metadataType and one futureMetadata");
    let sheet = &p["xl/worksheets/sheet1.xml"];
    assert!(cell_xml(sheet, "D1").contains("ref=\"D1:E3\""), "{}", cell_xml(sheet, "D1"));
    assert!(cell_xml(sheet, "J1").contains("cm=\"1\""));
    let back = xl_io::open(&out2).unwrap();
    let w = W(&back.workbook);
    assert_eq!(w.v("E1"), Value::Number(4.0));
    assert_eq!(back.workbook.spill_range(w.a("J1")), Some(r("J1:J2")));
    drop(pkg2);
}

#[test]
fn metadata_with_other_entries_gets_xldapr_added() {
    let doc = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<metadata xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:xlrd="http://schemas.microsoft.com/office/spreadsheetml/2017/richdata"><metadataTypes count="1"><metadataType name="XLRICHVALUE" minSupportedVersion="120000"/></metadataTypes><futureMetadata name="XLRICHVALUE" count="1"><bk><extLst><ext uri="{3e2802c4-a4d2-4d8b-9148-e3be6c30e623}"><xlrd:rvb i="0"/></ext></extLst></bk></futureMetadata><valueMetadata count="1"><bk><rc t="1" v="0"/></bk></valueMetadata></metadata>"#;
    let file = tmp("rich-meta.xlsx");
    // Build a package carrying that metadata part, then patch a spill into it.
    let mut book = XlsxBook::new();
    book.add_worksheet().write_number(0, 0, 1.0).unwrap();
    book.save(&file).unwrap();
    let mut zin = zip::ZipArchive::new(std::fs::File::open(&file).unwrap()).unwrap();
    let with_meta = tmp("rich-meta2.xlsx");
    {
        let mut zout = zip::ZipWriter::new(std::fs::File::create(&with_meta).unwrap());
        for i in 0..zin.len() {
            let f = zin.by_index(i).unwrap();
            zout.raw_copy_file(f).unwrap();
        }
        zout.start_file("xl/metadata.xml", zip::write::SimpleFileOptions::default()).unwrap();
        std::io::Write::write_all(&mut zout, doc).unwrap();
        zout.finish().unwrap();
    }
    let mut loaded = xl_io::open(&with_meta).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    loaded.workbook.set_input(Addr::new(s, CellRef::parse("B1").unwrap()), "=SEQUENCE(3)").unwrap();
    let out = tmp("rich-meta-out.xlsx");
    let pkg = loaded.package.as_ref().unwrap();
    xl_io::save(&loaded.workbook, &out, s, Some(pkg)).unwrap();
    let p = parts(&out);
    let meta = &p["xl/metadata.xml"];
    assert!(meta.contains("<metadataTypes count=\"2\">"), "{meta}");
    assert!(meta.contains("<cellMetadata count=\"1\"><bk><rc t=\"2\" v=\"0\"/></bk></cellMetadata>"), "{meta}");
    assert!(meta.contains("xmlns:xda="), "{meta}");
    assert!(meta.find("<futureMetadata name=\"XLDAPR\"").unwrap() < meta.find("<cellMetadata").unwrap());
    assert!(meta.find("<cellMetadata").unwrap() < meta.find("<valueMetadata").unwrap());
    assert!(cell_xml(&p["xl/worksheets/sheet1.xml"], "B1").contains("cm=\"1\""));
}

#[test]
fn reads_excel_dynamic_and_legacy_formulas() {
    // What Excel writes: a dynamic anchor with cached spill values, a legacy
    // formula relying on implicit intersection, and an old array formula.
    let src = tmp("excel-like.xlsx");
    let mut book = XlsxBook::new();
    let ws = book.add_worksheet();
    for i in 0..3u32 {
        ws.write_number(i, 0, (i + 1) as f64).unwrap();
    }
    ws.write_dynamic_array_formula(0, 2, 2, 2, "=_xlfn.SEQUENCE(3)*10").unwrap();
    ws.write_number(1, 2, 20.0).unwrap();
    ws.write_number(2, 2, 30.0).unwrap();
    ws.write_formula(1, 3, "=A1:A3*2").unwrap();
    ws.write_array_formula(0, 4, 2, 4, "{=A1:A3+1}").unwrap();
    ws.write_dynamic_formula(0, 5, "=_xlfn.LET(_xlpm.x,2,_xlpm.x*A1)").unwrap();
    book.save(&src).unwrap();
    let loaded = xl_io::open(&src).unwrap();
    let wb = &loaded.workbook;
    let w = W(wb);
    assert_eq!(wb.spill_range(w.a("C1")), Some(r("C1:C3")));
    assert_eq!(w.input("C1"), "=SEQUENCE(3)*10");
    assert_eq!(w.v("C3"), Value::Number(30.0));
    assert_eq!(w.input("C2"), "");
    assert_eq!(w.input("D2"), "=@A1:A3*2");
    assert_eq!(w.v("D2"), Value::Number(4.0));
    assert_eq!(wb.spill_range(w.a("E1")), Some(r("E1:E3")));
    assert_eq!(w.v("E3"), Value::Number(4.0));
    assert_eq!(w.input("F1"), "=LET(x,2,x*A1)");
    assert_eq!(w.v("F1"), Value::Number(2.0));
    // Saved back unchanged, the legacy formula stays legacy.
    let a3 = w.a("A3");
    let pkg = loaded.package.clone().unwrap();
    let out = tmp("excel-like-out.xlsx");
    let mut wb2 = loaded.workbook;
    wb2.set_input(a3, "7").unwrap();
    xl_io::save(&wb2, &out, wb2.sheet_ids()[0], Some(&pkg)).unwrap();
    let sheet = &parts(&out)["xl/worksheets/sheet1.xml"];
    let d2 = cell_xml(sheet, "D2");
    assert!(d2.contains("<f>A1:A3*2</f>") && !d2.contains("cm="), "{d2}");
    assert!(cell_xml(sheet, "C1").contains("ref=\"C1:C3\""));
    assert!(cell_xml(sheet, "E3").contains("<v>8</v>"));
}
