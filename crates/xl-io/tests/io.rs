use std::io::Write;
use std::path::PathBuf;

use xl_engine::datetime::serial_from_date;
use xl_engine::{Addr, CellRef, ErrorKind, SheetId, Value, Workbook};

fn tmp(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("xl-io-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn a(s: SheetId, a1: &str) -> Addr {
    Addr::new(s, CellRef::parse(a1).unwrap())
}

#[test]
fn xlsx_roundtrip_preserves_everything_we_model() {
    let mut wb = Workbook::new();
    let s1 = wb.sheet_ids()[0];
    wb.rename_sheet(s1, "Summary").unwrap();
    let data = wb.add_sheet(Some("Q4 Data")).unwrap();
    let inputs = [
        (data, "A1", "Item"),
        (data, "B1", "Amount"),
        (data, "A2", "Fish & Chips <large>"),
        (data, "B2", "$12.50"),
        (data, "A3", "Café \"Crème\""),
        (data, "B3", "$7"),
        (data, "C2", "2026-11-13"),
        (data, "C3", "15%"),
        (data, "D2", "TRUE"),
        (data, "D3", "#N/A"),
        (data, "E2", "'0042"),
        (s1, "A1", "='Q4 Data'!B2+'Q4 Data'!B3"),
        (s1, "A2", "=CONCAT('Q4 Data'!A2,\"!\")"),
        (s1, "A3", "=XLOOKUP(\"Item\",'Q4 Data'!A1:A3,'Q4 Data'!B1:B3,\"none\")"),
        (s1, "A4", "=TEXT('Q4 Data'!C2,\"mmm d\")"),
        (s1, "A5", "=IFS(A1>10,\"big\",TRUE,\"small\")"),
    ];
    for (s, cell, raw) in inputs {
        wb.set_input(a(s, cell), raw).unwrap();
    }
    wb.set_format(a(s1, "B1"), Some("0.000")); // formatted but empty
    wb.set_col_width(data, 0, Some(250.0));

    let path = tmp("roundtrip.xlsx");
    xl_io::save(&wb, &path, s1).unwrap();
    let loaded = xl_io::open(&path).unwrap();
    let back = loaded.workbook;
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);

    let names: Vec<&str> = back.sheet_ids().iter().map(|&id| back.sheet_name(id)).collect();
    assert_eq!(names, ["Summary", "Q4 Data"]);
    let (b1, bd) = (back.sheet_ids()[0], back.sheet_ids()[1]);
    for (s, cell, _) in inputs {
        let (orig, new) = (a(s, cell), a(if s == s1 { b1 } else { bd }, cell));
        assert_eq!(back.input_text(new), wb.input_text(orig), "input of {cell}");
        assert_eq!(back.value(new), wb.value(orig), "value of {cell}");
        assert_eq!(back.format_code(new), wb.format_code(orig), "format of {cell}");
        assert_eq!(back.display(new), wb.display(orig), "display of {cell}");
    }
    assert_eq!(*back.value(a(b1, "A1")), Value::Number(19.5));
    assert_eq!(*back.value(a(b1, "A3")), Value::text("Amount"));
    assert_eq!(*back.value(a(b1, "A4")), Value::text("Nov 13"));
    assert_eq!(back.format_code(a(b1, "B1")), Some("0.000"));
    assert_eq!(back.col_width(bd, 0), Some(250.0));
    assert_eq!(*back.value(a(bd, "D3")), Value::Error(ErrorKind::NA));
}

/// Build an xlsx the way Excel lays it out, independent of our writer.
fn write_excel_style_xlsx(path: &PathBuf) {
    let file = std::fs::File::create(path).unwrap();
    let mut z = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut put = |name: &str, body: &str| {
        z.start_file(name, opts).unwrap();
        z.write_all(body.as_bytes()).unwrap();
    };
    put("[Content_Types].xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#);
    put(
        "xl/workbook.xml",
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Sales" sheetId="1" r:id="rId1"/><sheet name="Lookup" sheetId="2" r:id="rId2"/></sheets></workbook>"#,
    );
    put(
        "xl/_rels/workbook.xml.rels",
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/>
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="/xl/worksheets/sheet1.xml"/>
</Relationships>"#,
    );
    put(
        "xl/sharedStrings.xml",
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="4" uniqueCount="4">
<si><t>Region</t></si>
<si><r><rPr><b/></rPr><t>Rich </t></r><r><t xml:space="preserve">text &amp; more</t></r><rPh><t>IGNORED</t></rPh></si>
<si><t>East</t></si>
<si><t>West &#x2192; &lt;b&gt;</t></si>
</sst>"#,
    );
    put(
        "xl/styles.xml",
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<numFmts count="1"><numFmt numFmtId="164" formatCode="&quot;$&quot;#,##0.00"/></numFmts>
<cellStyleXfs count="1"><xf numFmtId="0"/></cellStyleXfs>
<cellXfs count="4"><xf numFmtId="0"/><xf numFmtId="164" applyNumberFormat="1"/><xf numFmtId="14" applyNumberFormat="1"/><xf numFmtId="10" applyNumberFormat="1"/></cellXfs>
</styleSheet>"#,
    );
    put(
        "xl/worksheets/sheet1.xml",
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<cols><col min="1" max="1" width="20.7109375" customWidth="1"/><col min="2" max="16384" width="9.140625"/></cols>
<sheetData>
<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="C1" t="inlineStr"><is><t>Inline</t></is></c></row>
<row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2" s="1"><v>1200.5</v></c><c r="C2" s="2"><v>46339</v></c><c r="D2"><f t="shared" ref="D2:D4" si="0">B2*2</f><v>2401</v></c></row>
<row r="3"><c t="s"><v>3</v></c><c s="1"><v>300</v></c><c s="2"><v>46340</v></c><c><f t="shared" si="0"/><v>600</v></c></row>
<row r="4"><c r="A4" t="b"><v>1</v></c><c r="B4" t="e"><v>#DIV/0!</v></c><c r="D4"><f t="shared" si="0"/><v>0</v></c>
<c r="E4"><f>_xlfn.LAMBDA(x,x*3)(B2)</f><v>3601.5</v></c><c r="F4" t="str"><f>_xlfn.CONCAT(A2,"-",A3)</f><v>East-West</v></c></row>
<row r="5"><c r="B5" s="3"><v>0.256</v></c><c r="C5"><f>SUM(B2:B3)+Lookup!A1</f><v>1510.5</v></c><c r="D5" s="1"/></row>
</sheetData></worksheet>"#,
    );
    put(
        "xl/worksheets/sheet2.xml",
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>10</v></c></row></sheetData></worksheet>"#,
    );
    z.finish().unwrap();
}

#[test]
fn reads_excel_style_xlsx() {
    let path = tmp("excel_style.xlsx");
    write_excel_style_xlsx(&path);
    let loaded = xl_io::open(&path).unwrap();
    let wb = &loaded.workbook;
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    let s = wb.sheet_id("Sales").unwrap();
    let v = |c: &str| wb.value(a(s, c)).clone();
    let shown = |c: &str| wb.display(a(s, c)).text;

    // Shared strings: rich runs concatenated, phonetic runs skipped, entities decoded.
    assert_eq!(v("A1"), Value::text("Region"));
    assert_eq!(v("B1"), Value::text("Rich text & more"));
    assert_eq!(v("A3"), Value::text("West → <b>"), "cell without r= attribute follows its row");
    assert_eq!(v("C1"), Value::text("Inline"));
    // Number formats: custom, built-in date, built-in percent.
    assert_eq!(shown("B2"), "$1,200.50");
    assert_eq!(shown("C2"), "11/13/2026");
    assert_eq!(v("C3"), Value::Number(serial_from_date(2026, 11, 14)));
    assert_eq!(shown("B5"), "25.60%");
    assert_eq!(wb.format_code(a(s, "D5")), Some("\"$\"#,##0.00"), "formatted empty cell");
    // Shared formulas expand with shifted references and recalc.
    assert_eq!(wb.input_text(a(s, "D3")), "=B3*2");
    assert_eq!(wb.input_text(a(s, "D4")), "=B4*2");
    assert_eq!(v("D3"), Value::Number(600.0));
    assert_eq!(v("D4"), Value::Error(ErrorKind::Div0), "recalculated, not the stale cached 0");
    // Booleans and errors.
    assert_eq!(v("A4"), Value::Bool(true));
    assert_eq!(v("B4"), Value::Error(ErrorKind::Div0));
    // Unsupported function keeps the cached value; supported _xlfn functions run.
    assert_eq!(v("E4"), Value::Number(3601.5));
    assert_eq!(v("F4"), Value::text("East-West → <b>"));
    // Cross-sheet reference.
    assert_eq!(v("C5"), Value::Number(1510.5));
    // Column widths: custom only.
    assert_eq!(wb.col_width(s, 0), Some(xl_io::from_excel_width(20.7109375)));
    assert_eq!(wb.col_width(s, 1), None);
}

#[test]
fn csv_roundtrip_detects_types() {
    let path = tmp("people.csv");
    std::fs::write(&path, "\u{feff}Name,Joined,Score,Rate\nAda,2026-01-05,91,12.5%\n\"Lovelace, A\",1/2/2025,\"1,204\",3%\n").unwrap();
    let loaded = xl_io::open(&path).unwrap();
    let wb = &loaded.workbook;
    let s = wb.sheet_ids()[0];
    assert_eq!(wb.sheet_name(s), "people");
    assert_eq!(*wb.value(a(s, "A1")), Value::text("Name"), "BOM stripped");
    assert_eq!(*wb.value(a(s, "B2")), Value::Number(serial_from_date(2026, 1, 5)));
    assert_eq!(wb.display(a(s, "C3")).text, "1,204");
    assert_eq!(*wb.value(a(s, "A3")), Value::text("Lovelace, A"));
    let out = tmp("people_out.csv");
    xl_io::save(wb, &out, s).unwrap();
    let text = std::fs::read_to_string(&out).unwrap();
    assert_eq!(text, "Name,Joined,Score,Rate\nAda,2026-01-05,91,12.50%\n\"Lovelace, A\",1/2/2025,\"1,204\",3%\n");
}

#[test]
fn rejects_non_xlsx() {
    let path = tmp("fake.xlsx");
    std::fs::write(&path, "not a zip").unwrap();
    assert!(xl_io::open(&path).is_err());
    assert!(xl_io::open(&tmp("missing.xlsx")).is_err());
}

#[test]
fn styles_roundtrip_through_xlsx() {
    use xl_engine::{Border, HAlign, LineStyle, Style};
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let header = Style { bold: true, color: Some(0xFFFFFF), fill: Some(0x305496), align: HAlign::Center, bottom: Border { style: LineStyle::Medium, color: Some(0x1F3864) }, ..Style::default() };
    let note = Style { italic: true, underline: true, strike: true, color: Some(0xC00000), align: HAlign::Right, ..Style::default() };
    let boxed = Style { top: Border::THIN, bottom: Border::THIN, left: Border::THIN, right: Border { style: LineStyle::Double, color: None }, ..Style::default() };
    wb.set_input(a(s, "A1"), "Header").unwrap();
    wb.set_style(a(s, "A1"), &header);
    wb.set_input(a(s, "A2"), "12.5%").unwrap();
    wb.set_style(a(s, "A2"), &note);
    wb.set_style(a(s, "C3"), &boxed); // styled but empty
    let path = tmp("styles.xlsx");
    xl_io::save(&wb, &path, s).unwrap();
    let back = xl_io::open(&path).unwrap().workbook;
    let bs = back.sheet_ids()[0];
    assert_eq!(back.style(a(bs, "A1")), &header);
    assert_eq!(back.style(a(bs, "A2")), &note);
    assert_eq!(back.format_code(a(bs, "A2")), Some("0.00%"), "number format and style share one xf");
    assert_eq!(back.style(a(bs, "C3")), &boxed);
    assert!(back.style(a(bs, "B1")).is_default());
}

#[test]
fn reads_theme_colors_and_excel_styles() {
    let path = tmp("excel_styles.xlsx");
    let file = std::fs::File::create(&path).unwrap();
    let mut z = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    let mut put = |name: &str, body: &str| {
        z.start_file(name, opts).unwrap();
        z.write_all(body.as_bytes()).unwrap();
    };
    put("xl/workbook.xml", r#"<workbook xmlns:r="r"><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>"#);
    put("xl/_rels/workbook.xml.rels", r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#);
    put(
        "xl/theme/theme1.xml",
        r#"<a:theme xmlns:a="a"><a:themeElements><a:clrScheme name="Office">
<a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1>
<a:dk2><a:srgbClr val="44546A"/></a:dk2><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>
<a:accent1><a:srgbClr val="4472C4"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
</a:clrScheme></a:themeElements></a:theme>"#,
    );
    put(
        "xl/styles.xml",
        r#"<styleSheet>
<fonts count="3"><font><sz val="11"/><color theme="1"/><name val="Calibri"/></font>
<font><b/><sz val="11"/><color theme="0"/><name val="Calibri"/></font>
<font><i/><u/><b val="0"/><color rgb="FFC00000"/></font></fonts>
<fills count="4"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill>
<fill><patternFill patternType="solid"><fgColor theme="4"/><bgColor indexed="64"/></patternFill></fill>
<fill><patternFill patternType="solid"><fgColor theme="4" tint="0.79998168889431442"/></patternFill></fill></fills>
<borders count="2"><border><left/><right/><top/><bottom/><diagonal/></border>
<border><left style="thin"><color indexed="64"/></left><right style="thin"><color indexed="10"/></right><top/><bottom style="medium"><color auto="1"/></bottom></border></borders>
<cellXfs count="4"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/>
<xf numFmtId="0" fontId="1" fillId="2" borderId="1" applyFont="1"><alignment horizontal="center" vertical="center"/></xf>
<xf numFmtId="3" fontId="2" fillId="3" borderId="0"/>
<xf numFmtId="0" fontId="0" fillId="0" borderId="0"><alignment horizontal="right"/></xf></cellXfs>
</styleSheet>"#,
    );
    put(
        "xl/worksheets/sheet1.xml",
        r#"<worksheet><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>Head</t></is></c><c r="B1" s="2"><v>1234</v></c><c r="C1" s="3"><v>1</v></c><c r="D1" s="0"><v>2</v></c></row></sheetData></worksheet>"#,
    );
    z.finish().unwrap();

    let wb = xl_io::open(&path).unwrap().workbook;
    let s = wb.sheet_ids()[0];
    let head = wb.style(a(s, "A1"));
    assert!(head.bold);
    assert_eq!(head.color, Some(0xFFFFFF), "theme 0 is lt1 (white)");
    assert_eq!(head.fill, Some(0x4472C4), "theme 4 is accent1");
    assert_eq!(head.align, xl_engine::HAlign::Center);
    assert_eq!(head.left.style, xl_engine::LineStyle::Thin);
    assert_eq!(head.left.color, None, "indexed 64 = automatic");
    assert_eq!(head.right.color, Some(0xFF0000), "indexed 10 is red");
    assert_eq!(head.bottom.style, xl_engine::LineStyle::Medium);
    let b1 = wb.style(a(s, "B1"));
    assert!(b1.italic && b1.underline && !b1.bold);
    assert_eq!(b1.color, Some(0xC00000));
    let tinted = b1.fill.unwrap();
    assert!((tinted >> 16) >= 0xD7 && (tinted >> 16) <= 0xDB, "accent1 lighter 80% ~ D9E1F2, got {tinted:06X}");
    assert_eq!(wb.display(a(s, "B1")).text, "1,234");
    assert_eq!(wb.style(a(s, "C1")).align, xl_engine::HAlign::Right);
    assert!(wb.style(a(s, "A1")).color.is_some());
    assert!(wb.style(a(s, "D1")).is_default(), "default font color (dk1) stays automatic");
}

#[test]
fn defined_names_roundtrip() {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let other = wb.add_sheet(Some("Rates")).unwrap();
    wb.set_input(a(other, "A1"), "0.25").unwrap();
    wb.define_name("Rate", None, "Rates!$A$1").unwrap();
    wb.define_name("Local", Some(s), "Sheet1!$B$1:$B$3").unwrap();
    wb.set_input(a(s, "B1"), "4").unwrap();
    wb.set_input(a(s, "C1"), "=SUM(Local)*Rate").unwrap();
    let path = tmp("names.xlsx");
    xl_io::save(&wb, &path, s).unwrap();
    let back = xl_io::open(&path).unwrap();
    assert!(back.warnings.is_empty(), "{:?}", back.warnings);
    let wb2 = back.workbook;
    let s2 = wb2.sheet_ids()[0];
    assert_eq!(*wb2.value(a(s2, "C1")), Value::Number(1.0));
    let names: Vec<(String, bool)> = wb2.names().iter().map(|d| (d.name.clone(), d.scope.is_some())).collect();
    assert!(names.contains(&("Rate".into(), false)) && names.contains(&("Local".into(), true)), "{names:?}");
}

#[test]
fn layout_roundtrips_through_xlsx() {
    use xl_engine::{Range, Style, VAlign};
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    wb.set_input(a(s, "A1"), "Quarterly report").unwrap();
    wb.set_input(a(s, "A3"), "A long note that wraps").unwrap();
    wb.merge(s, Range::new(CellRef::parse("A1").unwrap(), CellRef::parse("D1").unwrap()));
    wb.set_style(a(s, "A3"), &Style { wrap: true, valign: VAlign::Top, font_size: Some(140), font_name: Some("DejaVu Serif".into()), ..Style::default() });
    wb.set_row_height(s, 2, Some(44.0));
    {
        let l = wb.layout_mut(s).unwrap();
        l.hidden_rows.insert(5);
        l.hidden_cols.insert(4);
        l.freeze = (1, 2);
    }
    let p = tmp("layout.xlsx");
    xl_io::save(&wb, &p, s).unwrap();
    let back = xl_io::open(&p).unwrap().workbook;
    let s2 = back.sheet_ids()[0];
    let l = back.layout(s2);
    assert_eq!(l.merges, vec![Range::new(CellRef::parse("A1").unwrap(), CellRef::parse("D1").unwrap())]);
    assert_eq!(l.freeze, (1, 2));
    assert!(l.hidden_rows.contains(&5) && l.hidden_cols.contains(&4));
    assert_eq!(back.row_height(s2, 2), Some(44.0));
    let st = back.style(a(s2, "A3"));
    assert!(st.wrap);
    assert_eq!(st.valign, VAlign::Top);
    assert_eq!(st.font_size, Some(140));
    assert_eq!(st.font_name.as_deref(), Some("DejaVu Serif"));
    let plain = back.style(a(s2, "A1"));
    assert_eq!((plain.font_size, plain.font_name.clone()), (None, None), "the default font stays implicit");
    assert_eq!(*back.value(a(s2, "A1")), Value::text("Quarterly report"));
}

#[test]
fn reports_parts_it_cannot_keep() {
    use rust_xlsxwriter::{Chart, ChartType, DataValidation, Note, Workbook as XlsxBook};
    let p = tmp("lossy.xlsx");
    let mut book = XlsxBook::new();
    let ws = book.add_worksheet();
    ws.write_number(0, 0, 1).unwrap();
    ws.write_number(1, 0, 2).unwrap();
    let mut chart = Chart::new(ChartType::Column);
    chart.add_series().set_values("Sheet1!$A$1:$A$2");
    ws.insert_chart(3, 3, &chart).unwrap();
    ws.insert_note(0, 1, &Note::new("check this")).unwrap();
    ws.add_data_validation(0, 2, 5, 2, &DataValidation::new().allow_whole_number(rust_xlsxwriter::DataValidationRule::Between(1, 10))).unwrap();
    book.save(&p).unwrap();
    let loaded = xl_io::open(&p).unwrap();
    for part in ["charts", "comments", "data validation"] {
        assert!(loaded.unsupported.contains(&part), "{part} missing from {:?}", loaded.unsupported);
    }
    assert_eq!(*loaded.workbook.value(a(loaded.workbook.sheet_ids()[0], "A2")), Value::Number(2.0));
    // A file xl wrote itself has nothing to lose.
    let p2 = tmp("plain.xlsx");
    xl_io::save(&loaded.workbook, &p2, loaded.workbook.sheet_ids()[0]).unwrap();
    assert!(xl_io::open(&p2).unwrap().unsupported.is_empty());
}

#[test]
fn autofilter_roundtrips_through_xlsx() {
    use xl_engine::{AutoFilter, Range};
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let rows = [["Fruit", "Qty"], ["Apple", "3"], ["Pear", "5"], ["Apple", "7"], ["", "1"]];
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            if !v.is_empty() {
                wb.set_input(Addr::new(s, CellRef::new(r as u32, c as u32)), v).unwrap();
            }
        }
    }
    let mut f = AutoFilter { range: Range::new(CellRef::new(0, 0), CellRef::new(4, 1)), columns: Default::default() };
    f.columns.insert(0, ["Apple".to_string(), String::new()].into_iter().collect());
    wb.layout_mut(s).unwrap().filter = Some(f.clone());
    wb.reapply_filter(s);
    assert_eq!(wb.layout(s).filtered_rows.iter().copied().collect::<Vec<_>>(), vec![2], "only Pear is hidden");
    let p = tmp("filter.xlsx");
    xl_io::save(&wb, &p, s).unwrap();
    let back = xl_io::open(&p).unwrap();
    assert!(!back.unsupported.contains(&"advanced filters"));
    let l = back.workbook.layout(back.workbook.sheet_ids()[0]);
    assert_eq!(l.filter.as_ref(), Some(&f));
    assert_eq!(l.filtered_rows.iter().copied().collect::<Vec<_>>(), vec![2]);
    assert!(l.hidden_rows.is_empty(), "filtered rows aren't hand-hidden rows");
}

#[test]
fn conditional_formats_roundtrip_through_xlsx() {
    use std::sync::Arc;
    use xl_engine::Range;
    use xl_engine::cond::{CfOp, CfRule, CfStyle, CfValue, CondFormat, TextMatch};
    use xl_engine::formula::parse_formula;
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    for r in 0..5 {
        wb.set_input(Addr::new(s, CellRef::new(r, 0)), &format!("{}", r * 10)).unwrap();
    }
    let rg = |a: &str, b: &str| Range::new(CellRef::parse(a).unwrap(), CellRef::parse(b).unwrap());
    let red = CfStyle { fill: Some(0xFFC7CE), color: Some(0x9C0006), bold: true, italic: false };
    let e = |src: &str| Arc::new(parse_formula(src, wb.sheets()).unwrap());
    let rules = vec![
        CondFormat { ranges: vec![rg("A1", "A5")], rule: CfRule::Cell { op: CfOp::Between, a: e("10"), b: Some(e("$B$1*2")), style: red.clone() }, stop_if_true: true },
        CondFormat::new(vec![rg("A1", "A5"), rg("C1", "C5")], CfRule::Text { kind: TextMatch::BeginsWith, text: "ab".into(), style: CfStyle { italic: true, ..CfStyle::default() } }),
        CondFormat::new(vec![rg("A1", "A5")], CfRule::Top { n: 3, percent: true, bottom: true, style: red.clone() }),
        CondFormat::new(vec![rg("A1", "A5")], CfRule::Average { above: false, style: red.clone() }),
        CondFormat::new(vec![rg("A1", "A5")], CfRule::Duplicates { unique: true, style: red.clone() }),
        CondFormat::new(vec![rg("A1", "A5")], CfRule::Formula { expr: e("MOD(A1,2)=0"), style: red.clone() }),
        CondFormat::new(vec![rg("A1", "A5")], CfRule::ColorScale { stops: vec![(CfValue::Min, 0xF8696B), (CfValue::Percentile(50.0), 0xFFEB84), (CfValue::Max, 0x63BE7B)] }),
        CondFormat::new(vec![rg("A1", "A5")], CfRule::DataBar { color: 0x638EC6, min: CfValue::Number(0.0), max: CfValue::Percent(90.0) }),
    ];
    wb.layout_mut(s).unwrap().cond = rules.clone();
    let p = tmp("cond.xlsx");
    xl_io::save(&wb, &p, s).unwrap();
    let back = xl_io::open(&p).unwrap();
    assert!(back.unsupported.is_empty(), "{:?}", back.unsupported);
    let got = &back.workbook.layout(back.workbook.sheet_ids()[0]).cond;
    assert_eq!(got.len(), rules.len());
    for (g, want) in got.iter().zip(&rules) {
        assert_eq!(g, want);
    }
}
