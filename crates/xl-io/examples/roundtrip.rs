//! Check that saving preserves real xlsx files.
//!
//!   cargo run -p xl-io --release --example roundtrip -- [OPTIONS] PATH...
//!
//! PATH is an .xlsx/.xlsm file or a directory of them. Options:
//!   --out DIR       where saved copies go (default: target/roundtrip-out);
//!                   DIR/outputs.tsv lists them (input, scenario, output)
//!   --json FILE     also write the report as JSON
//!   --summary FILE  also write the plain-text report to FILE
//!   --quiet         print only files that didn't pass, and the totals
//!   --known-failures FILE
//!                   basenames (one per line, '#' comments) whose failures
//!                   are reported but don't fail the run
//!
//! Each file goes through these scenarios, each starting from a fresh open:
//!   noop         save without edits: every part byte-identical, none added
//!   edit-new     type a value below the data on the first sheet with data
//!   edit-middle  change an existing constant in the middle of the data
//!   style        toggle bold on an existing cell
//!   add-sheet    add a sheet with one value
//!   rename       rename a sheet nothing refers to (n/a if every sheet is)
//!   insert-row   insert a row in the middle of the data; a save that has to
//!                rewrite the file (the save plan says why) counts as
//!                "fell back", not failed, until phase 3 patches these
//! After a patched save, only the parts the edit concerns may differ, no
//! original part may disappear (calcChain excepted), changed XML must be
//! well-formed, and the file must reopen in xl with the same values in every
//! cell (volatile formulas aside) — the edited ones included.
//!
//! The report names files by index and basename only, never their contents.
//! Exit status: 0 if nothing failed (known failures aside), 1 if something
//! did, 2 on bad usage.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use xl_engine::{Addr, CellInput, CellRef, SheetId, Shift, Style, Value, Workbook};
use xl_io::{Package, SavePlan};

// ------------------------------------------------------------------ results

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Status {
    Pass,
    NotApplicable,
    FellBack,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::NotApplicable => "n/a",
            Status::FellBack => "fell back",
            Status::Fail => "FAIL",
        }
    }
    fn key(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::NotApplicable => "n/a",
            Status::FellBack => "fell_back",
            Status::Fail => "fail",
        }
    }
}

struct Outcome {
    scenario: &'static str,
    status: Status,
    /// "patch" or "rewrite" when a save happened.
    plan: Option<&'static str>,
    detail: String,
}

impl Outcome {
    fn new(scenario: &'static str, status: Status, plan: Option<&'static str>, detail: impl Into<String>) -> Self {
        Outcome { scenario, status, plan, detail: detail.into() }
    }
}

struct FileReport {
    index: usize,
    name: String,
    status: Status,
    outcomes: Vec<Outcome>,
    /// Listed in --known-failures.
    known: bool,
}

const SCENARIOS: &[&str] = &["noop", "edit-new", "edit-middle", "style", "add-sheet", "rename", "insert-row"];

// ------------------------------------------------------------------ package helpers

type Parts = BTreeMap<String, Vec<u8>>;

fn parts(path: &Path) -> Result<Parts, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("not a zip: {e}"))?;
    let mut out = BTreeMap::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| e.to_string())?;
        let mut b = Vec::new();
        f.read_to_end(&mut b).map_err(|e| e.to_string())?;
        out.insert(f.name().map_err(|e| e.to_string())?.to_string(), b);
    }
    Ok(out)
}

fn is_xml(name: &str) -> bool {
    name.ends_with(".xml") || name.ends_with(".rels") || name.ends_with(".vml")
}

fn well_formed(bytes: &[u8]) -> bool {
    let mut r = quick_xml::Reader::from_reader(bytes);
    r.config_mut().check_end_names = true;
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Eof) => return true,
            Err(_) => return false,
            _ => buf.clear(),
        }
    }
}

fn local(name: &str) -> &str {
    name.rfind(':').map_or(name, |i| &name[i + 1..])
}

/// Attributes of every element with the given local name.
fn elements(bytes: &[u8], want: &str) -> Vec<BTreeMap<String, String>> {
    let mut r = quick_xml::Reader::from_reader(bytes);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        match r.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e) | quick_xml::events::Event::Empty(e)) if local(e.name().as_ref()) == want => {
                let attrs = e
                    .attributes()
                    .with_checks(false)
                    .flatten()
                    .map(|a| {
                        let k = local(a.key.as_ref()).to_string();
                        let v = a.normalized_value(quick_xml::XmlVersion::Implicit1_0).map(|v| v.into_owned()).unwrap_or_default();
                        (k, v)
                    })
                    .collect();
                out.push(attrs);
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => return out,
            _ => {}
        }
        buf.clear();
    }
}

fn rels_of(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

fn resolve(from: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut path: Vec<&str> = from.split('/').collect();
    path.pop();
    for seg in target.split('/') {
        match seg {
            ".." => {
                path.pop();
            }
            "." | "" => {}
            s => path.push(s),
        }
    }
    path.join("/")
}

/// The workbook part and, per sheet name, its worksheet part.
fn sheet_parts(parts: &Parts) -> (String, BTreeMap<String, String>) {
    let rel = |p: &Parts, owner: &str| -> BTreeMap<String, (String, String)> {
        p.get(&rels_of(owner))
            .map(|b| {
                elements(b, "Relationship")
                    .into_iter()
                    .filter_map(|a| Some((a.get("Id")?.clone(), (a.get("Type").cloned().unwrap_or_default(), resolve(owner, a.get("Target")?)))))
                    .collect()
            })
            .unwrap_or_default()
    };
    let root = rel(parts, "");
    let workbook = root
        .values()
        .find(|(t, _)| t.ends_with("/officeDocument"))
        .map(|(_, p)| p.clone())
        .unwrap_or_else(|| "xl/workbook.xml".into());
    let wb_rels = rel(parts, &workbook);
    let mut map = BTreeMap::new();
    if let Some(bytes) = parts.get(&workbook) {
        for a in elements(bytes, "sheet") {
            if let (Some(name), Some(id)) = (a.get("name"), a.get("id"))
                && let Some((_, p)) = wb_rels.get(id)
            {
                map.insert(name.clone(), p.clone());
            }
        }
    }
    (workbook, map)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// Save plan reasons quote sheet names; keep those out of the report.
fn redact(s: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for ch in s.chars() {
        match ch {
            '“' => {
                inside = true;
                out.push_str("“…");
            }
            '”' => {
                inside = false;
                out.push('”');
            }
            _ if !inside => out.push(ch),
            _ => {}
        }
    }
    out
}

// ------------------------------------------------------------------ values

/// Every non-empty value, per sheet (by name, in order), skipping volatile
/// formulas whose value legitimately changes between opens.
type Snapshot = Vec<(String, BTreeMap<CellRef, Value>)>;

fn snapshot(wb: &Workbook) -> Snapshot {
    wb.sheet_ids()
        .iter()
        .map(|&s| {
            let cells = wb
                .store(s)
                .iter()
                .filter(|(_, c)| !matches!(c.value, Value::Empty))
                .filter(|(c, _)| !volatile(wb, Addr::new(s, *c)))
                .map(|(c, cell)| (c, cell.value.clone()))
                .collect();
            (wb.sheet_name(s).to_string(), cells)
        })
        .collect()
}

fn volatile(wb: &Workbook, a: Addr) -> bool {
    wb.formula_source(a).is_some_and(|f| {
        let f = f.to_ascii_uppercase();
        ["RAND", "NOW(", "TODAY(", "OFFSET(", "INDIRECT(", "CELL(", "INFO("].iter().any(|v| f.contains(v))
    })
}

fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x == y || (x - y).abs() <= 1e-9 * x.abs().max(y.abs()),
        _ => a == b,
    }
}

/// Compare what we expect with a reopened file; Err says where they differ.
fn compare(expected: &Snapshot, back: &Workbook) -> Result<(), String> {
    let got = snapshot(back);
    if got.len() != expected.len() {
        return Err(format!("reopened with {} sheets, expected {}", got.len(), expected.len()));
    }
    for (i, ((en, ev), (gn, gv))) in expected.iter().zip(&got).enumerate() {
        if en != gn {
            return Err(format!("sheet #{} reopened with a different name", i + 1));
        }
        let keys: BTreeSet<&CellRef> = ev.keys().chain(gv.keys()).collect();
        let diffs: Vec<&CellRef> = keys
            .into_iter()
            .filter(|k| match (ev.get(k), gv.get(k)) {
                (Some(a), Some(b)) => !same_value(a, b),
                _ => true,
            })
            .collect();
        if let Some(first) = diffs.first() {
            return Err(format!("sheet #{} reopened with {} cell(s) different, first {first}", i + 1, diffs.len()));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ scenarios

struct Ctx<'a> {
    path: &'a Path,
    out_dir: PathBuf,
    original: Parts,
    /// Worksheet part per original sheet name.
    sheet_parts: BTreeMap<String, String>,
    workbook_part: String,
    outputs: Vec<(String, PathBuf)>,
}

/// Which parts an edit may change.
enum Allowed {
    /// These parts, plus the package indexes / caches every edit may touch.
    Parts(Vec<String>),
    /// Anything but parts that never hold cell addresses or values.
    AnyAddressed,
}

const ALWAYS_ALLOWED: &[&str] = &["xl/sharedStrings.xml", "xl/styles.xml", "xl/calcChain.xml", "[Content_Types].xml", "xl/_rels/workbook.xml.rels"];
const NEVER_CHANGED: &[&str] = &["xl/theme/", "xl/media/", "xl/embeddings/", "xl/printerSettings/", "customXml/", "xl/vbaProject.bin", "docProps/core.xml", "docProps/custom.xml", "_rels/.rels"];

type Check = Box<dyn Fn(&Workbook) -> Result<(), String>>;

struct Edit {
    allowed: Allowed,
    /// A Rewrite plan counts as "fell back" rather than failed.
    rewrite_ok: bool,
    /// Further checks on the reopened workbook.
    check: Option<Check>,
}

fn open(path: &Path) -> Result<(Workbook, Package), String> {
    let loaded = xl_io::open(path)?;
    let pkg = loaded.package.ok_or("no package after open")?;
    Ok((loaded.workbook, pkg))
}

/// The first sheet with cells (else the first sheet).
fn target_sheet(wb: &Workbook) -> SheetId {
    wb.sheet_ids().iter().copied().find(|&s| !wb.store(s).is_empty()).unwrap_or(wb.sheet_ids()[0])
}

/// An existing constant (number or text) in the middle of the target sheet.
fn middle_constant(wb: &Workbook, s: SheetId) -> Option<CellRef> {
    let mut cells: Vec<CellRef> = wb
        .store(s)
        .iter()
        .filter(|(_, c)| matches!(&c.input, CellInput::Value(Value::Number(_) | Value::Text(_))))
        .map(|(c, _)| c)
        .collect();
    cells.sort_by_key(|c| (c.row, c.col));
    cells.get(cells.len() / 2).copied()
}

fn noop(ctx: &mut Ctx) -> Outcome {
    const N: &str = "noop";
    let (wb, pkg) = match open(ctx.path) {
        Ok(x) => x,
        Err(e) => return Outcome::new(N, Status::Fail, None, format!("reopen failed: {e}")),
    };
    let plan = xl_io::save_plan(&wb, &pkg);
    if let SavePlan::Rewrite(why) = &plan {
        return Outcome::new(N, Status::Fail, Some("rewrite"), format!("a save without edits would rewrite the file: {}", redact(&why.join("; "))));
    }
    let out = ctx.out_dir.join("noop.xlsx");
    if let Err(e) = xl_io::save(&wb, &out, wb.sheet_ids()[0], Some(&pkg)) {
        return Outcome::new(N, Status::Fail, Some("patch"), format!("save failed: {e}"));
    }
    ctx.outputs.push((N.into(), out.clone()));
    let after = match parts(&out) {
        Ok(p) => p,
        Err(e) => return Outcome::new(N, Status::Fail, Some("patch"), format!("output unreadable: {e}")),
    };
    let changed: Vec<&String> = ctx.original.keys().filter(|k| after.get(*k) != ctx.original.get(*k)).collect();
    let added: Vec<&String> = after.keys().filter(|k| !ctx.original.contains_key(*k)).collect();
    if !changed.is_empty() || !added.is_empty() {
        return Outcome::new(N, Status::Fail, Some("patch"), format!("parts changed {changed:?}, added {added:?}"));
    }
    match xl_io::open(&out) {
        Ok(back) => match compare(&snapshot(&wb), &back.workbook) {
            Ok(()) => Outcome::new(N, Status::Pass, Some("patch"), ""),
            Err(e) => Outcome::new(N, Status::Fail, Some("patch"), e),
        },
        Err(e) => Outcome::new(N, Status::Fail, Some("patch"), format!("doesn't reopen: {e}")),
    }
}

/// Open, apply `edit`, save, and check the result.
fn scenario(ctx: &mut Ctx, name: &'static str, edit: impl FnOnce(&mut Workbook, &Ctx) -> Result<Edit, String>) -> Outcome {
    let (mut wb, pkg) = match open(ctx.path) {
        Ok(x) => x,
        Err(e) => return Outcome::new(name, Status::Fail, None, format!("reopen failed: {e}")),
    };
    let edit = match edit(&mut wb, ctx) {
        Ok(e) => e,
        Err(why) => return Outcome::new(name, Status::NotApplicable, None, why),
    };
    let expected = snapshot(&wb);
    let plan = xl_io::save_plan(&wb, &pkg);
    let plan_label = if plan == SavePlan::Patch { "patch" } else { "rewrite" };
    let out = ctx.out_dir.join(format!("{name}.xlsx"));
    if let Err(e) = xl_io::save(&wb, &out, wb.sheet_ids()[0], Some(&pkg)) {
        return Outcome::new(name, Status::Fail, Some(plan_label), format!("save failed: {e}"));
    }
    ctx.outputs.push((name.into(), out.clone()));
    let values = match xl_io::open(&out) {
        Ok(b) => compare(&expected, &b.workbook).and_then(|()| edit.check.as_ref().map_or(Ok(()), |c| c(&b.workbook))),
        Err(e) => Err(format!("doesn't reopen: {e}")),
    };
    if let SavePlan::Rewrite(why) = plan {
        let why = redact(&why.join("; "));
        return match (edit.rewrite_ok, values) {
            (true, Ok(())) => Outcome::new(name, Status::FellBack, Some("rewrite"), why),
            (true, Err(e)) => Outcome::new(name, Status::Fail, Some("rewrite"), format!("{e} (after falling back: {why})")),
            (false, _) => Outcome::new(name, Status::Fail, Some("rewrite"), format!("would rewrite the file: {why}")),
        };
    }
    let after = match parts(&out) {
        Ok(p) => p,
        Err(e) => return Outcome::new(name, Status::Fail, Some("patch"), format!("output unreadable: {e}")),
    };
    let mut problems = Vec::new();
    let unexpected: Vec<&String> = ctx
        .original
        .keys()
        .filter(|k| after.get(*k) != ctx.original.get(*k) && after.contains_key(*k))
        .filter(|k| match &edit.allowed {
            Allowed::Parts(list) => !ALWAYS_ALLOWED.contains(&k.as_str()) && !list.contains(k),
            Allowed::AnyAddressed => NEVER_CHANGED.iter().any(|p| k.starts_with(p)),
        })
        .collect();
    if !unexpected.is_empty() {
        problems.push(format!("unexpected changes {unexpected:?}"));
    }
    let removed: Vec<&String> = ctx.original.keys().filter(|k| !after.contains_key(*k) && k.as_str() != "xl/calcChain.xml").collect();
    if !removed.is_empty() {
        problems.push(format!("parts dropped {removed:?}"));
    }
    let malformed: Vec<&String> = after.iter().filter(|(k, v)| is_xml(k) && ctx.original.get(*k) != Some(v) && !well_formed(v)).map(|(k, _)| k).collect();
    if !malformed.is_empty() {
        problems.push(format!("malformed XML {malformed:?}"));
    }
    if let Err(e) = values {
        problems.push(e);
    }
    if problems.is_empty() {
        Outcome::new(name, Status::Pass, Some("patch"), "")
    } else {
        Outcome::new(name, Status::Fail, Some("patch"), problems.join("; "))
    }
}

/// Worksheet parts of sheets whose values differ between two snapshots.
fn changed_sheet_parts(ctx: &Ctx, before: &Snapshot, after: &Snapshot) -> Vec<String> {
    before
        .iter()
        .filter(|(name, cells)| after.iter().find(|(n, _)| n == name).is_none_or(|(_, c)| c != cells))
        .filter_map(|(name, _)| ctx.sheet_parts.get(name).cloned())
        .collect()
}

fn part_of(ctx: &Ctx, wb: &Workbook, s: SheetId) -> Vec<String> {
    ctx.sheet_parts.get(wb.sheet_name(s)).cloned().into_iter().collect()
}

fn edit_new(wb: &mut Workbook, ctx: &Ctx) -> Result<Edit, String> {
    let s = target_sheet(wb);
    let cell = CellRef::new(wb.store(s).extent().map_or(0, |e| e.row) + 5, 0);
    wb.set_input(Addr::new(s, cell), "xl round trip").map_err(|e| format!("{e:?}"))?;
    Ok(Edit { allowed: Allowed::Parts(part_of(ctx, wb, s)), rewrite_ok: false, check: None })
}

fn edit_middle(wb: &mut Workbook, ctx: &Ctx) -> Result<Edit, String> {
    let s = target_sheet(wb);
    let cell = middle_constant(wb, s).ok_or("no constant cells")?;
    let before = snapshot(wb);
    let input = match wb.value(Addr::new(s, cell)) {
        Value::Number(n) => format!("{}", n + 1.0),
        _ => "xl edited".to_string(),
    };
    wb.set_input(Addr::new(s, cell), &input).map_err(|e| format!("{e:?}"))?;
    let mut allowed = part_of(ctx, wb, s);
    allowed.extend(changed_sheet_parts(ctx, &before, &snapshot(wb)));
    Ok(Edit { allowed: Allowed::Parts(allowed), rewrite_ok: false, check: None })
}

fn style(wb: &mut Workbook, ctx: &Ctx) -> Result<Edit, String> {
    let s = target_sheet(wb);
    let cell = middle_constant(wb, s).or_else(|| wb.store(s).iter().map(|(c, _)| c).min_by_key(|c| (c.row, c.col))).unwrap_or(CellRef::new(0, 0));
    let a = Addr::new(s, cell);
    let bold = !wb.style(a).bold;
    let new = Style { bold, ..wb.style(a).clone() };
    wb.set_style(a, &new);
    let sheet_name = wb.sheet_name(s).to_string();
    let check = move |back: &Workbook| {
        let bs = back.sheet_id(&sheet_name).ok_or("styled sheet missing")?;
        if back.style(Addr::new(bs, cell)).bold == bold { Ok(()) } else { Err(format!("bold on {cell} didn't survive")) }
    };
    Ok(Edit { allowed: Allowed::Parts(part_of(ctx, wb, s)), rewrite_ok: false, check: Some(Box::new(check)) })
}

fn add_sheet(wb: &mut Workbook, ctx: &Ctx) -> Result<Edit, String> {
    let name = (1..).map(|i| format!("xl check {i}")).find(|n| wb.sheet_id(n).is_none()).unwrap();
    let s = wb.add_sheet(Some(&name))?;
    wb.set_input(Addr::new(s, CellRef::new(0, 0)), "added by xl").map_err(|e| format!("{e:?}"))?;
    let allowed = vec![ctx.workbook_part.clone(), rels_of(&ctx.workbook_part), "docProps/app.xml".into()];
    Ok(Edit { allowed: Allowed::Parts(allowed), rewrite_ok: false, check: None })
}

fn rename(wb: &mut Workbook, ctx: &Ctx) -> Result<Edit, String> {
    // Referenced: named in a formula, or in any part but the shared strings
    // and app.xml (which lists sheet names as plain titles). A sheet's own
    // part mentioning itself counts too.
    let formulas = wb.formula_texts();
    let referenced = |name: &str| {
        let quoted = format!("'{}'!", name.replace('\'', "''"));
        let plain = format!("{name}!");
        formulas.iter().any(|(_, f)| f.contains(&quoted) || f.contains(&plain))
            || ctx
                .original
                .iter()
                .filter(|(k, _)| is_xml(k) && k.as_str() != "docProps/app.xml" && k.as_str() != "xl/sharedStrings.xml")
                // Pivot caches name their source sheet in an attribute.
                .any(|(_, v)| contains(v, quoted.as_bytes()) || contains(v, plain.as_bytes()) || contains(v, format!("sheet=\"{name}\"").as_bytes()))
    };
    let s = wb.sheet_ids().iter().copied().find(|&s| !referenced(wb.sheet_name(s))).ok_or("every sheet is referenced by name")?;
    let new = (1..).map(|i| format!("xl renamed {i}")).find(|n| wb.sheet_id(n).is_none()).unwrap();
    wb.rename_sheet(s, &new)?;
    let allowed = vec![ctx.workbook_part.clone(), "docProps/app.xml".into()];
    Ok(Edit { allowed: Allowed::Parts(allowed), rewrite_ok: false, check: None })
}

fn insert_row(wb: &mut Workbook, _ctx: &Ctx) -> Result<Edit, String> {
    let s = target_sheet(wb);
    let last = wb.store(s).extent().map_or(0, |e| e.row);
    let first = wb.store(s).iter().map(|(c, _)| c.row).min().unwrap_or(0);
    let at = first + (last - first).div_ceil(2);
    wb.apply_shift(Shift::InsertRows { sheet: s, at, count: 1 });
    Ok(Edit { allowed: Allowed::AnyAddressed, rewrite_ok: true, check: None })
}

// ------------------------------------------------------------------ driver

static PANIC: Mutex<Option<String>> = Mutex::new(None);

fn check_file(index: usize, path: &Path, out_root: &Path, outputs: &mut Vec<(PathBuf, String, PathBuf)>) -> FileReport {
    let name = path.file_name().map_or_else(|| format!("#{index}"), |n| n.to_string_lossy().into_owned());
    let stem = path.file_stem().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let out_dir = out_root.join(format!("{index:03}-{stem}"));
    let fail = |detail: String| FileReport { index, name: name.clone(), status: Status::Fail, outcomes: vec![Outcome::new("open", Status::Fail, None, detail)], known: false };
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        return fail(format!("can't create output dir: {e}"));
    }
    let original = match parts(path) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    if let Err(e) = xl_io::open(path) {
        return fail(format!("xl can't open it: {e}"));
    }
    let (workbook_part, sheet_parts) = sheet_parts(&original);
    let mut ctx = Ctx { path, out_dir, original, sheet_parts, workbook_part, outputs: Vec::new() };
    type Run = fn(&mut Ctx) -> Outcome;
    let runs: [(&'static str, Run); 7] = [
        ("noop", noop),
        ("edit-new", |c| scenario(c, "edit-new", edit_new)),
        ("edit-middle", |c| scenario(c, "edit-middle", edit_middle)),
        ("style", |c| scenario(c, "style", style)),
        ("add-sheet", |c| scenario(c, "add-sheet", add_sheet)),
        ("rename", |c| scenario(c, "rename", rename)),
        ("insert-row", |c| scenario(c, "insert-row", insert_row)),
    ];
    let mut outcomes = Vec::new();
    for (scenario_name, run) in runs {
        let outcome = catch_unwind(AssertUnwindSafe(|| run(&mut ctx))).unwrap_or_else(|_| {
            let msg = PANIC.lock().ok().and_then(|mut m| m.take()).unwrap_or_default();
            Outcome::new(scenario_name, Status::Fail, None, format!("panicked: {msg}"))
        });
        outcomes.push(outcome);
    }
    for (scenario_name, out) in ctx.outputs {
        outputs.push((path.to_path_buf(), scenario_name, out));
    }
    let status = outcomes.iter().map(|o| if o.status == Status::NotApplicable { Status::Pass } else { o.status }).max().unwrap_or(Status::Pass);
    FileReport { index, name, status, outcomes, known: false }
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn to_json(reports: &[FileReport]) -> String {
    let mut j = String::from("{\n  \"files\": [\n");
    for (i, r) in reports.iter().enumerate() {
        let _ = write!(j, "    {{\"index\": {}, \"file\": {}, \"status\": {}, \"known_failure\": {}, \"scenarios\": [", r.index, json_str(&r.name), json_str(r.status.key()), r.known);
        for (k, o) in r.outcomes.iter().enumerate() {
            let plan = o.plan.map_or("null".to_string(), json_str);
            let _ = write!(j, "{}{{\"name\": {}, \"status\": {}, \"plan\": {plan}, \"detail\": {}}}", if k > 0 { ", " } else { "" }, json_str(o.scenario), json_str(o.status.key()), json_str(&o.detail));
        }
        let _ = writeln!(j, "]}}{}", if i + 1 < reports.len() { "," } else { "" });
    }
    let count = |st: Status| reports.iter().filter(|r| r.status == st).count();
    let _ = write!(j, "  ],\n  \"summary\": {{\"files\": {}, \"pass\": {}, \"fell_back\": {}, \"fail\": {}, \"known_failures\": {}, \"scenarios\": {{", reports.len(), count(Status::Pass), count(Status::FellBack), count(Status::Fail), known_failures(reports));
    for (k, s) in SCENARIOS.iter().enumerate() {
        let n = |st: Status| reports.iter().flat_map(|r| &r.outcomes).filter(|o| o.scenario == *s && o.status == st).count();
        let _ = write!(j, "{}{}: {{\"pass\": {}, \"fell_back\": {}, \"fail\": {}, \"n/a\": {}}}", if k > 0 { ", " } else { "" }, json_str(s), n(Status::Pass), n(Status::FellBack), n(Status::Fail), n(Status::NotApplicable));
    }
    j.push_str("}}\n}\n");
    j
}

fn known_failures(reports: &[FileReport]) -> usize {
    reports.iter().filter(|r| r.known && r.status == Status::Fail).count()
}

fn file_line(r: &FileReport) -> String {
    let known = match (r.known, r.status) {
        (true, Status::Fail) => " (known failure)",
        (true, _) => " (listed as a known failure, but didn't fail: remove it from the list)",
        _ => "",
    };
    let mut s = format!("#{} {}: {}{known}", r.index, r.name, r.status.label());
    if r.outcomes.len() > 1 {
        let tags: Vec<String> = r.outcomes.iter().map(|o| format!("{}={}", o.scenario, o.status.label())).collect();
        let _ = write!(s, "  [{}]", tags.join(" "));
    }
    for o in r.outcomes.iter().filter(|o| matches!(o.status, Status::Fail | Status::FellBack)) {
        let _ = write!(s, "\n    {} {}: {}", o.scenario, o.status.label(), o.detail);
    }
    s
}

fn summary_lines(reports: &[FileReport]) -> String {
    let count = |st: Status| reports.iter().filter(|r| r.status == st).count();
    let mut s = format!("\n{} files: {} pass, {} fell back, {} fail ({} known)\n", reports.len(), count(Status::Pass), count(Status::FellBack), count(Status::Fail), known_failures(reports));
    for sc in SCENARIOS {
        let n = |st: Status| reports.iter().flat_map(|r| &r.outcomes).filter(|o| o.scenario == *sc && o.status == st).count();
        let _ = writeln!(s, "  {sc:<12} {:>4} pass {:>4} fell back {:>4} fail {:>4} n/a", n(Status::Pass), n(Status::FellBack), n(Status::Fail), n(Status::NotApplicable));
    }
    s
}

fn usage() -> ! {
    eprintln!("usage: roundtrip [--out DIR] [--json FILE] [--summary FILE] [--known-failures FILE] [--quiet] PATH...\n  PATH: .xlsx/.xlsm files or directories of them");
    std::process::exit(2)
}

fn main() {
    let mut out = PathBuf::from("target/roundtrip-out");
    let (mut json, mut summary, mut quiet) = (None::<PathBuf>, None::<PathBuf>, false);
    let mut known = BTreeSet::new();
    let mut inputs = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" => out = args.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "--json" => json = Some(args.next().map(PathBuf::from).unwrap_or_else(|| usage())),
            "--summary" => summary = Some(args.next().map(PathBuf::from).unwrap_or_else(|| usage())),
            "--quiet" => quiet = true,
            "--known-failures" => {
                let p = args.next().unwrap_or_else(|| usage());
                let text = std::fs::read_to_string(&p).unwrap_or_else(|e| {
                    eprintln!("can't read {p}: {e}");
                    std::process::exit(2)
                });
                known.extend(text.lines().map(|l| l.split('#').next().unwrap_or("").trim().to_string()).filter(|l| !l.is_empty()));
            }
            "-h" | "--help" => usage(),
            s if s.starts_with("--") => usage(),
            _ => inputs.push(PathBuf::from(a)),
        }
    }
    let mut files = Vec::new();
    for p in inputs {
        if p.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(&p)
                .map(|d| d.flatten().map(|e| e.path()).filter(|f| f.extension().is_some_and(|x| x.eq_ignore_ascii_case("xlsx") || x.eq_ignore_ascii_case("xlsm"))).collect())
                .unwrap_or_default();
            found.sort();
            files.extend(found);
        } else {
            files.push(p);
        }
    }
    if files.is_empty() {
        usage();
    }
    if let Err(e) = std::fs::create_dir_all(&out) {
        eprintln!("can't create {}: {e}", out.display());
        std::process::exit(2);
    }
    std::panic::set_hook(Box::new(|info| {
        let msg = info.payload().downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| info.payload().downcast_ref::<String>().cloned()).unwrap_or_default();
        let msg: String = msg.lines().next().unwrap_or("").chars().take(200).collect();
        let at = info.location().map(|l| format!(" at {}:{}", l.file(), l.line())).unwrap_or_default();
        if let Ok(mut m) = PANIC.lock() {
            *m = Some(format!("{msg}{at}"));
        }
    }));

    let mut reports = Vec::new();
    let mut outputs = Vec::new();
    let mut text = String::new();
    for (i, f) in files.iter().enumerate() {
        let mut r = check_file(i, f, &out, &mut outputs);
        r.known = known.contains(&r.name);
        let line = file_line(&r);
        if !quiet || r.status != Status::Pass || r.known {
            println!("{line}");
        }
        text.push_str(&line);
        text.push('\n');
        reports.push(r);
    }
    let tail = summary_lines(&reports);
    print!("{tail}");
    text.push_str(&tail);

    let abs = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let tsv: String = outputs.iter().map(|(input, sc, o)| format!("{}\t{sc}\t{}\n", abs(input).display(), abs(o).display())).collect();
    let mut io_ok = std::fs::write(out.join("outputs.tsv"), tsv).is_ok();
    if let Some(p) = json {
        io_ok &= std::fs::write(p, to_json(&reports)).is_ok();
    }
    if let Some(p) = summary {
        io_ok &= std::fs::write(p, text).is_ok();
    }
    if !io_ok {
        eprintln!("couldn't write a report file");
        std::process::exit(2);
    }
    std::process::exit(if reports.iter().any(|r| r.status == Status::Fail && !r.known) { 1 } else { 0 });
}
