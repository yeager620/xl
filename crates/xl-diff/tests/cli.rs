//! `xl diff` / `xl textconv` output (golden text) and exit codes.

use std::path::{Path, PathBuf};

use xl_engine::{Addr, CellRef, Style, Workbook};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xl-diff-cli-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn book(rows: &[&[&str]]) -> Workbook {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let mut items = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            if !v.is_empty() {
                items.push((Addr::new(s, CellRef::new(r as u32, c as u32)), v.to_string()));
            }
        }
    }
    wb.set_inputs(items);
    wb
}

fn save(wb: &Workbook, path: &Path) {
    xl_io::save(wb, path, wb.sheet_ids()[0], None).unwrap();
}

fn diff(args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = xl_diff::cli::diff_main(&args, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

const OLD: &[&[&str]] = &[
    &["Item", "Qty", "Price", "Total"],
    &["Coffee", "3", "14.5", "=B2*C2"],
    &["Milk", "6", "2.25", "=B3*C3"],
    &["Bread", "1", "3", "=B4*C4"],
    &["", "", "Sum", "=SUM(D2:D4)"],
];

const NEW: &[&[&str]] = &[
    &["Item", "Qty", "Price", "Total"],
    &["Coffee", "3", "15", "=B2*C2"],
    &["Tea", "2", "4", "=B3*C3"],
    &["Milk", "6", "2.25", "=B4*C4"],
    &["Bread", "1", "3", "=B5*C5*1.1"],
    &["", "", "Sum", "=SUM(D2:D5)"],
];

#[test]
fn golden_diff_output_and_exit_codes() {
    let dir = scratch("golden");
    let (a, b) = (dir.join("old.xlsx"), dir.join("new.xlsx"));
    save(&book(OLD), &a);
    let mut new = book(NEW);
    let s = new.sheet_ids()[0];
    new.set_style(Addr::new(s, CellRef::parse("A1").unwrap()), &Style { bold: true, ..Style::default() });
    new.set_format(Addr::new(s, CellRef::parse("D6").unwrap()), Some("$#,##0.00"));
    new.add_sheet(Some("Notes")).unwrap();
    new.set_input(Addr::new(new.sheet_id("Notes").unwrap(), CellRef::new(0, 0)), "Checked").unwrap();
    save(&new, &b);

    let (code, out, err) = diff(&[a.to_str().unwrap(), b.to_str().unwrap()]);
    assert_eq!(code, 1, "{err}");
    let body: String = out.lines().skip(2).map(|l| format!("{l}\n")).collect();
    let expected = "\
+ Sheet1 row 3 inserted
+   Sheet1!A3: \"Tea\"
+   Sheet1!B3: 2
+   Sheet1!C3: 4
+   Sheet1!D3: =B3*C3 (8)
Sheet1!A1: style bold on
Sheet1!C2: 14.5 → 15
Sheet1!D2: 43.5 → 45 (=B2*C2)
Sheet1!D5 (was D4): =B4*C4 → =B5*C5*1.1 (3 → 3.3)
Sheet1!D6 (was D5): 60 → 69.8 (=SUM(D2:D5))
Sheet1!D6 (was D5): format General → $#,##0.00
+ sheet Notes added (1 cell)
+   Notes!A1: \"Checked\"
";
    assert_eq!(body, expected, "got:\n{body}");
    assert!(out.starts_with(&format!("--- {}\n+++ {}\n", a.display(), b.display())));

    // Same files: exit 0, no output.
    assert_eq!(diff(&[a.to_str().unwrap(), a.to_str().unwrap()]), (0, String::new(), String::new()));
    // Quiet: only the status.
    let (code, out, _) = diff(&["-q", a.to_str().unwrap(), b.to_str().unwrap()]);
    assert_eq!((code, out.as_str()), (1, ""));
    // Trouble: exit 2.
    let (code, _, err) = diff(&[a.to_str().unwrap(), dir.join("missing.xlsx").to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(err.contains("missing.xlsx"), "{err}");
    assert_eq!(diff(&["only-one"]).0, 2);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn git_external_diff_mode() {
    let dir = scratch("git");
    // git's temporary copies have no meaningful extension.
    let (a, b) = (dir.join("old.xlsx"), dir.join("new.xlsx"));
    save(&book(OLD), &a);
    save(&book(NEW), &b);
    let (ta, tb) = (dir.join("XXXX_old"), dir.join("XXXX_new"));
    std::fs::copy(&a, &ta).unwrap();
    std::fs::copy(&b, &tb).unwrap();
    let (code, out, err) = diff(&["Book.xlsx", ta.to_str().unwrap(), "abc123", "100644", tb.to_str().unwrap(), "def456", "100644"]);
    assert_eq!(code, 0, "git expects 0 from an external diff: {err}");
    assert!(out.starts_with("diff --xl a/Book.xlsx b/Book.xlsx\n--- a/Book.xlsx\n+++ b/Book.xlsx\n"), "{out}");
    assert!(out.contains("Sheet1!C2: 14.5 → 15"), "{out}");
    // A new file: the old side is /dev/null.
    let (code, out, _) = diff(&["Book.xlsx", "/dev/null", ".", ".", tb.to_str().unwrap(), "def456", "100644"]);
    assert_eq!(code, 0);
    assert!(out.contains("+ sheet Sheet1 added (22 cells)"), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn textconv_dump_is_stable() {
    let dir = scratch("textconv");
    let path = dir.join("book.xlsx");
    let mut wb = book(&[&["Name", "Note"], &["tab\there", "line\nbreak"], &["=1/3", "=A2&\"!\""]]);
    wb.define_name("Third", None, "Sheet1!$A$3").unwrap();
    save(&wb, &path);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(xl_diff::cli::textconv_main(&[path.to_string_lossy().into_owned()], &mut out, &mut err), 0);
    let text = String::from_utf8(out).unwrap();
    let expected = "\
# Sheet1
Sheet1!A1\tName
Sheet1!B1\tNote
Sheet1!A2\ttab\\there
Sheet1!B2\tline\\nbreak
Sheet1!A3\t0.333333333333333\t=1/3
Sheet1!B3\ttab\\there!\t=A2&\"!\"
# name Third\t=Sheet1!$A$3
";
    assert_eq!(text, expected, "got:\n{text}");
    // CSV works too.
    let csv = dir.join("data.csv");
    std::fs::write(&csv, "a,b\n1,2\n").unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(xl_diff::cli::textconv_main(&[csv.to_string_lossy().into_owned()], &mut out, &mut err), 0);
    assert!(String::from_utf8(out).unwrap().contains("!B2\t2\n"));
    // Missing file: exit 2.
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(xl_diff::cli::textconv_main(&["/nonexistent/x.xlsx".into()], &mut out, &mut err), 2);
    std::fs::remove_dir_all(&dir).ok();
}
