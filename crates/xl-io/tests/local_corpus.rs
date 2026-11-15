//! Structural edits against local files: set XL_LOCAL_CORPUS to a file
//! listing xlsx paths (one per line). Skipped otherwise.

use std::io::Read;

use xl_engine::{Addr, CellRef, Shift};

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

#[test]
fn insert_rows_and_rename_on_local_files() {
    let Ok(list) = std::env::var("XL_LOCAL_CORPUS") else { return };
    let files: Vec<String> = std::fs::read_to_string(list).unwrap().lines().map(str::to_string).collect();
    let out_dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("xl-local-corpus");
    std::fs::create_dir_all(&out_dir).unwrap();
    let (mut patched, mut fell_back, mut failed) = (0, 0, 0);
    for (i, f) in files.iter().enumerate() {
        let Ok(mut loaded) = xl_io::open(std::path::Path::new(f)) else { continue };
        let Some(pkg) = loaded.package.clone() else { continue };
        let s = loaded.workbook.sheet_ids()[0];
        let probe = loaded.workbook.store(s).iter().next().map(|(c, _)| c);
        let before = probe.map(|c| loaded.workbook.input_text(Addr::new(s, c)));
        loaded.workbook.apply_shift(Shift::InsertRows { sheet: s, at: 0, count: 1 });
        let name = format!("{} (xl)", loaded.workbook.sheet_name(s));
        loaded.workbook.rename_sheet(s, &name).unwrap();
        let plan = xl_io::save_plan(&loaded.workbook, &pkg);
        let out = out_dir.join(format!("{i}.xlsx"));
        if let Err(e) = xl_io::save(&loaded.workbook, &out, s, Some(&pkg)) {
            println!("#{i}: save failed: {e}");
            failed += 1;
            continue;
        }
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        let mut bad = Vec::new();
        for j in 0..zip.len() {
            let mut f = zip.by_index(j).unwrap();
            let n = f.name().unwrap().to_string();
            let mut b = Vec::new();
            f.read_to_end(&mut b).unwrap();
            if n.ends_with(".xml") && !well_formed(&b) {
                bad.push(n);
            }
        }
        let back = xl_io::open(&out);
        let ok_values = back.as_ref().is_ok_and(|b| {
            let bs = b.workbook.sheet_ids()[0];
            b.workbook.sheet_name(bs) == name && probe.zip(before.as_ref()).is_none_or(|(c, _)| !b.workbook.input_text(Addr::new(bs, CellRef::new(c.row + 1, c.col))).is_empty())
        });
        match (&plan, bad.is_empty() && ok_values) {
            (_, false) => {
                failed += 1;
                println!("#{i}: malformed {bad:?}, reopened ok: {ok_values}");
            }
            (xl_io::SavePlan::Patch, true) => patched += 1,
            (xl_io::SavePlan::Rewrite(why), true) => {
                fell_back += 1;
                println!("#{i}: fell back: {}", why.join("; "));
            }
        }
    }
    println!("patched {patched}, fell back {fell_back}, failed {failed}");
    assert_eq!(failed, 0);
}
