//! Command-line modes of the `xl` binary that never start the GUI:
//!
//! - `xl diff [-q] OLD NEW` prints a cell-level diff; exits 0 if the
//!   workbooks are the same, 1 if they differ, 2 on trouble (like diff(1)).
//! - `xl diff PATH OLD OLD-HEX OLD-MODE NEW NEW-HEX NEW-MODE` is the same,
//!   called by git as an external diff driver (`diff.xl.command`); it exits
//!   0 when the files differ, as git expects.
//! - `xl textconv FILE` prints a stable one-cell-per-line dump, for git's
//!   `diff.xl.textconv`.

use std::io::Write;
use std::path::{Path, PathBuf};

use xl_engine::Workbook;

use crate::{diff_workbooks, render_diff, textconv};

const USAGE: &str = "usage: xl diff [-q|--quiet] OLD NEW\n       xl textconv FILE\n";

/// Run a command-line mode if `args` (the whole argv) asks for one; returns
/// the exit code. None means: start the GUI as usual.
pub fn run(args: &[String]) -> Option<i32> {
    let mode = args.get(1)?.as_str();
    let rest = &args[2..];
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    let code = match mode {
        "diff" => diff_main(rest, &mut stdout, &mut stderr),
        "textconv" => textconv_main(rest, &mut stdout, &mut stderr),
        _ => return None,
    };
    stdout.flush().ok();
    Some(code)
}

/// `xl diff …`, writing to the given streams.
pub fn diff_main(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let quiet = args.iter().any(|a| a == "-q" || a == "--quiet");
    let files: Vec<&String> = args.iter().filter(|a| !matches!(a.as_str(), "-q" | "--quiet")).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        out.write_all(USAGE.as_bytes()).ok();
        return 0;
    }
    let (old, new, git_path) = match files.as_slice() {
        [old, new] => (Path::new(old.as_str()), Path::new(new.as_str()), None),
        [path, old, _, _, new, _, _] => (Path::new(old.as_str()), Path::new(new.as_str()), Some(path.as_str())),
        _ => {
            err.write_all(USAGE.as_bytes()).ok();
            return 2;
        }
    };
    let (mut a, b) = match (open_any(old), open_any(new)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            writeln!(err, "xl diff: {e}").ok();
            return 2;
        }
    };
    if is_csv(old) || is_csv(new) {
        crate::same_sheet_name(&mut a, &b);
    }
    let d = diff_workbooks(&a, &b);
    if d.is_empty() {
        return 0;
    }
    if !quiet {
        let text = render_diff(&d);
        let header = match git_path {
            Some(p) => format!("diff --xl a/{p} b/{p}\n--- a/{p}\n+++ b/{p}\n"),
            None => format!("--- {}\n+++ {}\n", old.display(), new.display()),
        };
        if out.write_all(header.as_bytes()).and_then(|_| out.write_all(text.as_bytes())).is_err() {
            // A closed pipe (e.g. `| head`) isn't trouble.
            return if git_path.is_some() { 0 } else { 1 };
        }
    }
    // git treats a failing external diff as an error.
    if git_path.is_some() { 0 } else { 1 }
}

/// `xl textconv FILE`.
pub fn textconv_main(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let [file] = args else {
        err.write_all(USAGE.as_bytes()).ok();
        return 2;
    };
    match open_any(Path::new(file)) {
        Ok(wb) => {
            out.write_all(textconv(&wb).as_bytes()).ok();
            0
        }
        Err(e) => {
            writeln!(err, "xl textconv: {e}").ok();
            2
        }
    }
}

/// Open a workbook for comparing. `/dev/null` (git's stand-in for a file
/// that doesn't exist on one side) is an empty workbook; files without a
/// known extension (git's temporary copies) are recognized by content.
pub fn open_any(path: &Path) -> Result<Workbook, String> {
    if path == Path::new("/dev/null") {
        return Ok(Workbook::empty());
    }
    if xl_io::FileKind::of(path).is_some() {
        return xl_io::open(path).map(|l| l.workbook).map_err(|e| format!("{}: {e}", path.display()));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let ext = if bytes.starts_with(b"PK\x03\x04") { "xlsx" } else { "csv" };
    let tmp = temp_copy(&bytes, ext).map_err(|e| format!("{}: {e}", path.display()))?;
    let result = xl_io::open(&tmp).map(|l| l.workbook).map_err(|e| format!("{}: {e}", path.display()));
    std::fs::remove_file(&tmp).ok();
    result
}

/// CSV files hold one sheet, named after the file.
fn is_csv(path: &Path) -> bool {
    xl_io::FileKind::of(path) == Some(xl_io::FileKind::Csv) || (xl_io::FileKind::of(path).is_none() && std::fs::read(path).is_ok_and(|b| !b.starts_with(b"PK")))
}

fn temp_copy(bytes: &[u8], ext: &str) -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("xl-diff-{}-{n}.{ext}", std::process::id()));
    std::fs::write(&path, bytes)?;
    Ok(path)
}
