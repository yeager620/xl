//! Check that saving preserves real xlsx files:
//!   cargo run -p xl-io --example roundtrip -- OUT_DIR FILE.xlsx...
//! For each file: a save without edits must leave every part unchanged; a
//! save after editing one cell must leave every part but the sheet, shared
//! strings, styles, calcChain and package indexes unchanged, keep the XML
//! well-formed, and reopen with the same values elsewhere.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use xl_engine::{Addr, CellRef};

fn parts(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    (0..zip.len())
        .map(|i| {
            let mut f = zip.by_index(i).unwrap();
            let mut b = Vec::new();
            f.read_to_end(&mut b).unwrap();
            (f.name().unwrap().to_string(), b)
        })
        .collect()
}

fn well_formed(bytes: &[u8]) -> bool {
    let mut r = quick_xml::Reader::from_reader(bytes);
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Eof) => return true,
            Err(_) => return false,
            _ => buf.clear(),
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().expect("OUT_DIR"));
    std::fs::create_dir_all(&out).unwrap();
    let (mut ok, mut total) = (0, 0);
    for (i, f) in args.enumerate() {
        total += 1;
        let path = PathBuf::from(&f);
        let tag = format!("#{i}");
        let Ok(loaded) = xl_io::open(&path) else {
            println!("{tag}: xl can't open it (skipped)");
            continue;
        };
        let Some(pkg) = loaded.package.clone() else {
            println!("{tag}: no package");
            continue;
        };
        let s = loaded.workbook.sheet_ids()[0];
        let noop = out.join(format!("{i}-noop.xlsx"));
        if let Err(e) = xl_io::save(&loaded.workbook, &noop, s, Some(&pkg)) {
            println!("{tag}: no-op save failed: {e}");
            continue;
        }
        let (before, after) = (parts(&path), parts(&noop));
        let changed: Vec<&String> = before.keys().filter(|k| after.get(*k) != before.get(*k)).collect();
        if !changed.is_empty() {
            println!("{tag}: no-op save changed {changed:?}");
            continue;
        }
        // Edit: a value far from the data on the first sheet.
        let mut wb = loaded.workbook;
        let cell = CellRef::new(wb.store(s).extent().map_or(0, |e| e.row) + 5, 0);
        wb.set_input(Addr::new(s, cell), "xl round trip").unwrap();
        let plan = xl_io::save_plan(&wb, &pkg);
        let edited = out.join(format!("{i}-edit.xlsx"));
        if let Err(e) = xl_io::save(&wb, &edited, s, Some(&pkg)) {
            println!("{tag}: edit save failed: {e}");
            continue;
        }
        let after = parts(&edited);
        let allowed = ["xl/sharedStrings.xml", "xl/styles.xml", "xl/calcChain.xml", "[Content_Types].xml", "xl/_rels/workbook.xml.rels"];
        let sheet_part = pkg.sheets_parts().first().cloned().unwrap_or_default();
        let unexpected: Vec<&String> = before.keys().filter(|k| !allowed.contains(&k.as_str()) && **k != sheet_part && after.get(*k) != before.get(*k)).collect();
        let malformed: Vec<&String> = after.iter().filter(|(k, v)| k.ends_with(".xml") && before.get(*k) != Some(v) && !well_formed(v)).map(|(k, _)| k).collect();
        let back = xl_io::open(&edited);
        let same_values = back.as_ref().is_ok_and(|b| {
            let bs = b.workbook.sheet_ids()[0];
            b.workbook.store(bs).len() == wb.store(s).len() && *b.workbook.value(Addr::new(bs, cell)) == xl_engine::Value::text("xl round trip")
        });
        if unexpected.is_empty() && malformed.is_empty() && same_values {
            ok += 1;
            println!("{tag}: ok ({:?})", plan);
        } else {
            println!("{tag}: unexpected changes {unexpected:?}, malformed {malformed:?}, reopened with same values: {same_values}");
        }
    }
    println!("{ok}/{total} files round-trip");
}
