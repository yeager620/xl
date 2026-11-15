//! The full-rewrite save path (used when patching isn't possible) keeps
//! every value: checked on corpus files when XL_CORPUS_DIR is set.

use xl_engine::{Addr, Value};

#[test]
fn fresh_rewrite_keeps_values() {
    let Ok(dir) = std::env::var("XL_CORPUS_DIR") else { return };
    let out = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fresh-rewrite");
    std::fs::create_dir_all(&out).unwrap();
    let mut failures = Vec::new();
    for name in ["lo-hyperlinks.xlsx", "poi-FormulaEvalTestData_Copy.xlsx"] {
        let src = std::path::Path::new(&dir).join(name);
        let Ok(mut a) = xl_io::open(&src) else { continue };
        // As the corpus checker's insert-row scenario does.
        let first = a.workbook.sheet_ids()[0];
        a.workbook.apply_shift(xl_engine::Shift::InsertRows { sheet: first, at: 0, count: 1 });
        let dst = out.join(name);
        xl_io::save(&a.workbook, &dst, a.workbook.sheet_ids()[0], None).unwrap();
        let b = xl_io::open(&dst).unwrap();
        for (&sa, &sb) in a.workbook.sheet_ids().iter().zip(b.workbook.sheet_ids()) {
            for (c, _) in a.workbook.store(sa).iter() {
                let (va, vb) = (a.workbook.value(Addr::new(sa, c)), b.workbook.value(Addr::new(sb, c)));
                if va != vb && !matches!(va, Value::Error(_)) {
                    failures.push(format!("{name} {}!{c}: {va:?} → {vb:?} (input {:?} → {:?})", a.workbook.sheet_name(sa), a.workbook.input_text(Addr::new(sa, c)), b.workbook.input_text(Addr::new(sb, c))));
                }
            }
        }
    }
    for f in failures.iter().take(8) {
        println!("{f}");
    }
    assert!(failures.is_empty(), "{} cells changed", failures.len());
}
