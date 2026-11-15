//! Big-data benchmarks: `cargo run -p xl-bench --release -- bigdata [rows]`
//!
//! Generates a CSV (default 10M rows, about 1 GB) under `target/bigdata/`
//! once, then measures: parallel CSV load, memory, simulated scrolling,
//! whole-column SUM, a SQL group-by, and saving back to CSV.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use xl_engine::{Addr, CellRef, Workbook};

fn secs(d: std::time::Duration) -> String {
    format!("{:>8.3} s", d.as_secs_f64())
}

fn mb(bytes: u64) -> String {
    format!("{:.0} MB", bytes as f64 / 1e6)
}

/// (current, peak) resident memory from /proc.
fn rss() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |k: &str| status.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
    (field("VmRSS:"), field("VmHWM:"))
}

const CITIES: [&str; 12] = ["Berlin", "Boston", "Cairo", "Delhi", "Lagos", "Lima", "London", "Madrid", "Osaka", "Paris", "Quito", "Seoul"];
const NAMES: [&str; 8] = ["Ana", "Ben", "Chen", "Dara", "Eli", "Fatima", "Goran", "Hana"];

/// A deterministic ~100-byte-per-row CSV: id, name, city, amount, date,
/// flag, zip (leading zeros), note (unique text).
pub fn generate(path: &Path, rows: u64) -> std::io::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    let mut w = std::io::BufWriter::with_capacity(8 << 20, std::fs::File::create(&tmp)?);
    writeln!(w, "id,name,city,amount,date,flag,zip,note")?;
    let mut x: u64 = 0x2545F4914F6CDD1D;
    for i in 0..rows {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let name = NAMES[(x % 8) as usize];
        let city = CITIES[((x >> 8) % 12) as usize];
        let cents = (x >> 16) % 1_000_000;
        let (y, m, d) = (2015 + (x >> 40) % 10, 1 + (x >> 44) % 12, 1 + (x >> 48) % 28);
        let flag = if (x >> 52) & 1 == 1 { "TRUE" } else { "FALSE" };
        let zip = (x >> 20) % 100_000;
        writeln!(w, "{i},{name} {},{city},{}.{:02},{y}-{m:02}-{d:02},{flag},{zip:05},\"order {i}, ref {:x}\"", (x >> 4) % 1000, cents / 100, cents % 100, x >> 24)?;
    }
    w.flush()?;
    drop(w);
    std::fs::rename(tmp, path)
}

pub fn run(rows: u64) {
    let path: PathBuf = std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| PathBuf::from("target"), PathBuf::from).join("bigdata").join(format!("big-{rows}.csv"));
    if !path.exists() {
        let t = Instant::now();
        generate(&path, rows).expect("generate CSV");
        println!("generated {} in {}", path.display(), secs(t.elapsed()));
    }
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!("file: {} ({}, {} rows + header)", path.display(), mb(size), rows);

    // 1. Load.
    let t = Instant::now();
    let loaded = xl_io::open_with_progress(&path, &xl_io::Progress::default()).expect("load");
    let load = t.elapsed();
    let mut wb = loaded.workbook;
    let s = wb.sheet_ids()[0];
    let (now, peak) = rss();
    println!(
        "csv load (parallel)               {}  ({:.0} MB/s, {:.1} M rows/s, {} cells)",
        secs(load),
        size as f64 / 1e6 / load.as_secs_f64(),
        rows as f64 / 1e6 / load.as_secs_f64(),
        loaded.cells
    );
    println!("memory after load                 RSS {}, peak {}, compact store {} ({:.1} bytes/cell)", mb(now), mb(peak), mb(wb.compact_heap_bytes() as u64), wb.compact_heap_bytes() as f64 / loaded.cells as f64);

    // 2. Scrolling: format a 40x10 window at many positions (cold blocks).
    let t = Instant::now();
    let frames = 200u32;
    let mut chars = 0usize;
    for f in 0..frames {
        let top = (f as u64 * 48_271 % rows) as u32;
        for r in top..top + 40 {
            for c in 0..10 {
                chars += wb.display(Addr::new(s, CellRef::new(r, c))).text.len();
            }
        }
    }
    let per = t.elapsed() / frames;
    println!("random-jump frame (40x10 cells)   {:>8.3} ms  ({chars} chars formatted)", per.as_secs_f64() * 1e3);

    // 3. Whole-column SUM over 10M rows.
    let t = Instant::now();
    let st = wb.set_input(Addr::new(s, CellRef::new(0, 10)), "=SUM(D:D)").unwrap();
    println!("=SUM(D:D) first eval              {}  (= {})", secs(t.elapsed()), wb.display(Addr::new(s, CellRef::new(0, 10))).text);
    let _ = st;
    let t = Instant::now();
    wb.set_input(Addr::new(s, CellRef::new(5, 3)), "1000").unwrap();
    println!("edit D6 -> SUM(D:D) recalculates  {}", secs(t.elapsed()));
    let t = Instant::now();
    wb.set_input(Addr::new(s, CellRef::new(1, 10)), "=COUNTIF(C:C,\"Paris\")").unwrap();
    println!("=COUNTIF(C:C,\"Paris\")             {}  (= {})", secs(t.elapsed()), wb.display(Addr::new(s, CellRef::new(1, 10))).text);
    println!("after column scans                RSS {}, compact store {} (incl. materialized cells)", mb(rss().0), mb(wb.compact_heap_bytes() as u64));
    wb.trim_caches();
    println!("after trim_caches                 compact store {} (the allocator keeps the freed pages for reuse)", mb(wb.compact_heap_bytes() as u64));

    // 4. SQL group-by.
    let mut sql = xl_sql::SqlEngine::new().unwrap();
    let q = "SELECT city, COUNT(*) AS n, SUM(amount) AS total, AVG(amount) AS mean FROM big GROUP BY city ORDER BY total DESC";
    let name = wb.sheet_name(s).to_string();
    let q = q.replace("FROM big", &format!("FROM \"{name}\""));
    let t = Instant::now();
    sql.run(&wb, &format!("SELECT COUNT(*) FROM \"{name}\""), 1, None, CellRef::new(0, 0)).unwrap();
    println!("SQL: load sheet into SQLite       {}", secs(t.elapsed()));
    for extra in ["SELECT SUM(amount) FROM {t}", "SELECT COUNT(*) FROM {t} WHERE city = 'Paris'", "SELECT city, COUNT(*) FROM {t} GROUP BY city"] {
        let t = Instant::now();
        sql.run(&wb, &extra.replace("{t}", &format!("\"{name}\"")), 1, None, CellRef::new(0, 0)).unwrap();
        println!("  {extra:<45} {}", secs(t.elapsed()));
    }
    let t = Instant::now();
    let r = sql.run(&wb, &q, 1, None, CellRef::new(0, 0)).unwrap();
    println!("SQL group-by (sheet cached)       {}  ({} groups)", secs(t.elapsed()), r.rows);
    let t = Instant::now();
    let out = wb.add_sheet(Some("Query1")).unwrap();
    r.place(&mut wb, out, CellRef::new(0, 0)).unwrap();
    println!("place result on a new sheet       {}", secs(t.elapsed()));

    // 5. Save as CSV.
    let out = path.with_file_name("big-out.csv");
    let t = Instant::now();
    xl_io::save(&wb, &out, s, None).unwrap();
    println!("save as CSV                       {}  ({})", secs(t.elapsed()), mb(std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0)));
    std::fs::remove_file(&out).ok();
    // 6. xlsx refuses sheets past its row limit.
    if rows >= xl_engine::XLSX_MAX_ROWS as u64 {
        let err = xl_io::save(&wb, &path.with_file_name("big.xlsx"), s, None).unwrap_err();
        println!("save as xlsx                      refused: {err}");
    }
}

/// A small, fast version for CI-style smoke runs.
#[allow(dead_code)]
pub fn sum_whole_column(rows: u32) -> (f64, std::time::Duration) {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let mut b = xl_engine::bulk::ColumnBuilder::new(0);
    for i in 0..rows {
        b.push_number(i as f64);
    }
    wb.load_columns(s, 0, vec![b.finish()]).unwrap();
    let t = Instant::now();
    wb.set_input(Addr::new(s, CellRef::new(0, 2)), "=SUM(A:A)").unwrap();
    let el = t.elapsed();
    (wb.value(Addr::new(s, CellRef::new(0, 2))).to_number().unwrap_or(0.0), el)
}
