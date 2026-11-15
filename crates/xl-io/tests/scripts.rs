//! Scripts stored in xlsx files: fresh writes, patched saves, and leaving
//! files without script changes alone.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use xl_engine::{Addr, CellRef, ScriptModule, Value, Workbook};
use xl_io::SavePlan;

fn tmp(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("xl-script-tests");
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

fn text(p: &BTreeMap<String, Vec<u8>>, name: &str) -> String {
    String::from_utf8_lossy(&p[name]).into_owned()
}

fn scripts() -> Vec<ScriptModule> {
    vec![
        ScriptModule { name: "Pricing".into(), source: "xl.func(\"DISCOUNT\", function(p, r)\r\n  return p * (1 - r) -- p < 0 && r > 1?\r\nend)\n".into() },
        ScriptModule { name: "Tools".into(), source: "xl.macro('Hi', function() xl.message('hi') end)".into() },
    ]
}

fn book() -> Workbook {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    wb.set_input(Addr::new(s, CellRef::parse("A1").unwrap()), "100").unwrap();
    wb.set_input(Addr::new(s, CellRef::parse("B1").unwrap()), "=A1*2").unwrap();
    wb
}

/// Content types and relationships point at the scripts item.
fn check_wiring(p: &BTreeMap<String, Vec<u8>>) -> String {
    let item = p.keys().find(|k| k.starts_with("customXml/item") && !k.contains("Props")).expect("scripts item").clone();
    let n = item.trim_start_matches("customXml/item").trim_end_matches(".xml").to_string();
    let ct = text(p, "[Content_Types].xml");
    assert!(ct.contains(&format!("PartName=\"/customXml/itemProps{n}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.customXmlProperties+xml\"")), "{ct}");
    let rels = text(p, "xl/_rels/workbook.xml.rels");
    assert!(rels.contains(&format!("relationships/customXml\" Target=\"../customXml/item{n}.xml\"")), "{rels}");
    let item_rels = text(p, &format!("customXml/_rels/item{n}.xml.rels"));
    assert!(item_rels.contains(&format!("Target=\"itemProps{n}.xml\"")));
    assert!(text(p, &item).contains("urn:x-xl:scripts:1"));
    item
}

#[test]
fn fresh_files_carry_scripts() {
    let path = tmp("fresh.xlsx");
    let mut wb = book();
    wb.set_scripts(scripts());
    xl_io::save(&wb, &path, wb.sheet_ids()[0], None).unwrap();
    check_wiring(&parts(&path));
    let loaded = xl_io::open(&path).unwrap();
    assert_eq!(loaded.workbook.scripts(), &scripts()[..]);
    // Our own Custom XML item isn't something "xl can't keep".
    assert!(!loaded.unsupported.contains(&"custom XML"), "{:?}", loaded.unsupported);
    // A file without scripts gets no Custom XML at all.
    let plain = tmp("plain.xlsx");
    xl_io::save(&book(), &plain, 0, None).unwrap();
    assert!(!parts(&plain).keys().any(|k| k.starts_with("customXml/")));
    assert!(xl_io::open(&plain).unwrap().workbook.scripts().is_empty());
}

#[test]
fn patched_saves_add_replace_and_keep_scripts() {
    let src = tmp("patch-src.xlsx");
    xl_io::save(&book(), &src, 0, None).unwrap();
    let before = parts(&src);

    // Adding scripts to a file that had none.
    let mut loaded = xl_io::open(&src).unwrap();
    let pkg = loaded.package.take().unwrap();
    loaded.workbook.set_scripts(scripts());
    assert_eq!(xl_io::save_plan(&loaded.workbook, &pkg), SavePlan::Patch);
    let out = tmp("patch-added.xlsx");
    let pkg2 = xl_io::save(&loaded.workbook, &out, 0, Some(&pkg)).unwrap().unwrap();
    let after = parts(&out);
    let item = check_wiring(&after);
    // Only the wiring changed; every sheet part is byte for byte the same.
    for (name, bytes) in &before {
        if name != "[Content_Types].xml" && name != "xl/_rels/workbook.xml.rels" {
            assert_eq!(after.get(name), Some(bytes), "{name} changed");
        }
    }
    let reread = xl_io::open(&out).unwrap();
    assert_eq!(reread.workbook.scripts(), &scripts()[..]);
    assert_eq!(*reread.workbook.value(Addr::new(0, CellRef::parse("B1").unwrap())), Value::Number(200.0));

    // Editing a script replaces the item in place: no new parts.
    let mut wb = reread.workbook;
    let mut edited = scripts();
    edited[1].source.push_str("\n-- edited");
    wb.set_scripts(edited.clone());
    let out2 = tmp("patch-edited.xlsx");
    xl_io::save(&wb, &out2, 0, Some(&pkg2)).unwrap();
    let edited_parts = parts(&out2);
    assert_eq!(edited_parts.keys().collect::<Vec<_>>(), after.keys().collect::<Vec<_>>());
    assert_ne!(edited_parts[&item], after[&item]);
    for (name, bytes) in &after {
        if *name != item {
            assert_eq!(edited_parts.get(name), Some(bytes), "{name} changed");
        }
    }
    assert_eq!(xl_io::open(&out2).unwrap().workbook.scripts(), &edited[..]);

    // Saving with no script changes leaves the item (and everything) alone.
    let mut again = xl_io::open(&out2).unwrap();
    let pkg3 = again.package.take().unwrap();
    let out3 = tmp("patch-untouched.xlsx");
    xl_io::save(&again.workbook, &out3, 0, Some(&pkg3)).unwrap();
    assert_eq!(parts(&out3), edited_parts);

    // Cell edits keep the scripts item byte for byte.
    let s = again.workbook.sheet_ids()[0];
    again.workbook.set_input(Addr::new(s, CellRef::parse("A1").unwrap()), "7").unwrap();
    let out4 = tmp("patch-cells.xlsx");
    xl_io::save(&again.workbook, &out4, 0, Some(&pkg3)).unwrap();
    let p4 = parts(&out4);
    assert_eq!(p4[&item], edited_parts[&item]);
    assert_eq!(xl_io::open(&out4).unwrap().workbook.scripts(), &edited[..]);
}

#[test]
fn other_custom_xml_is_left_alone() {
    // A file with someone else's Custom XML item: not mistaken for scripts,
    // still reported as something xl doesn't model.
    let src = tmp("other-cx.xlsx");
    xl_io::save(&book(), &src, 0, None).unwrap();
    let mut p = parts(&src);
    p.insert("customXml/item1.xml".into(), b"<?xml version=\"1.0\"?><b:Sources xmlns:b=\"http://schemas.openxmlformats.org/officeDocument/2006/bibliography\"/>".to_vec());
    let rels = text(&p, "xl/_rels/workbook.xml.rels").replace("</Relationships>", "<Relationship Id=\"rIdCx\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml\" Target=\"../customXml/item1.xml\"/></Relationships>");
    p.insert("xl/_rels/workbook.xml.rels".into(), rels.into_bytes());
    {
        let f = std::fs::File::create(&src).unwrap();
        let mut z = zip::ZipWriter::new(f);
        for (name, b) in &p {
            z.start_file(name.as_str(), zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut z, b).unwrap();
        }
        z.finish().unwrap();
    }
    let mut loaded = xl_io::open(&src).unwrap();
    assert!(loaded.workbook.scripts().is_empty());
    assert!(loaded.unsupported.contains(&"custom XML"));
    // Adding scripts picks the next free item number and keeps the other item.
    let pkg = loaded.package.take().unwrap();
    loaded.workbook.set_scripts(scripts());
    let out = tmp("other-cx-out.xlsx");
    xl_io::save(&loaded.workbook, &out, 0, Some(&pkg)).unwrap();
    let after = parts(&out);
    assert_eq!(after["customXml/item1.xml"], p["customXml/item1.xml"]);
    assert!(after.contains_key("customXml/item2.xml"));
    assert_eq!(xl_io::open(&out).unwrap().workbook.scripts(), &scripts()[..]);
}
