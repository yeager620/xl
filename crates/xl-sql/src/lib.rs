//! SQL over a workbook: sheets (first row = column names) and tables by
//! name, and CSV files by path, queried with an embedded SQLite.
//!
//! Sources are loaded lazily: the query is prepared, and each "no such
//! table: X" error loads X (a table, a sheet, or a file) into the in-memory
//! database and tries again. Loaded sources are cached until the workbook
//! changes (see [`SqlEngine::run`]'s `generation`) or, for files, until the
//! file's modification time changes.
//!
//! Values: numbers are REAL (so `7/2` is 3.5, as in a spreadsheet), text is
//! TEXT, booleans 1/0, empty cells and errors NULL. Date-formatted columns
//! go in as ISO-8601 text (`2024-03-09`, `2024-03-09 14:30:00`) so SQLite's
//! date functions work on them, and ISO dates coming out become dates again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rusqlite::types::ValueRef;
use rusqlite::Connection;
use xl_engine::bulk::{ColumnBuilder, Segment};
use xl_engine::datetime::datetime_from_serial;
use xl_engine::{Addr, CellRef, Range, SheetId, Value, Workbook, col_name};
use xl_io::infer::{DateOrder, Inferred, infer};

/// The result of a query, as compact columns ready to put on a sheet: the
/// first row holds the column names.
#[derive(Debug)]
pub struct QueryResult {
    pub columns: Vec<String>,
    /// Data rows (not counting the header).
    pub rows: usize,
    /// Columns built at the requested anchor, header first.
    pub data: Vec<Vec<Segment>>,
    /// Number-format runs `(column offset, format, first row, end row)`.
    pub formats: Vec<(u32, &'static str, u32, u32)>,
}

impl QueryResult {
    /// The region the result covers when placed at `anchor`.
    pub fn region(&self, anchor: CellRef) -> Range {
        let cols = self.columns.len().max(1) as u32;
        Range::new(anchor, CellRef::new(anchor.row + self.rows as u32, anchor.col + cols - 1))
    }

    /// Put the result on `sheet` at `anchor` (the region must be empty).
    pub fn place(self, wb: &mut Workbook, sheet: SheetId, anchor: CellRef) -> Result<(), String> {
        for &(c, fmt, a, b) in &self.formats {
            wb.set_format_cells(sheet, Some(fmt), (a..b).map(|r| CellRef::new(r, anchor.col + c)));
        }
        wb.load_columns(sheet, anchor.col, self.data)?;
        Ok(())
    }
}

struct Loaded {
    /// Workbook generation (sheets/tables) or file mtime it was loaded at.
    stamp: Stamp,
}

#[derive(PartialEq, Eq, Clone, Debug)]
enum Stamp {
    Workbook(u64),
    File(PathBuf, Option<SystemTime>),
}

/// An in-memory SQLite database that loads workbook data on demand.
pub struct SqlEngine {
    conn: Connection,
    loaded: HashMap<String, Loaded>,
}

/// Results with more rows than a sheet holds are cut off.
const MAX_RESULT_ROWS: usize = xl_engine::MAX_ROWS as usize - 1;

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

impl SqlEngine {
    pub fn new() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        conn.execute_batch("PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF; PRAGMA threads = 8;").map_err(|e| e.to_string())?;
        Ok(SqlEngine { conn, loaded: HashMap::new() })
    }

    /// Run `sql` against `wb`, with results anchored at `anchor` (their
    /// header row). `generation` identifies the workbook's contents: bump it
    /// on every edit so cached sheets are reloaded. Relative file paths are
    /// resolved against `base_dir`.
    pub fn run(&mut self, wb: &Workbook, sql: &str, generation: u64, base_dir: Option<&Path>, anchor: CellRef) -> Result<QueryResult, String> {
        self.drop_stale(generation);
        let sql = sql.trim().trim_end_matches(';');
        if sql.is_empty() {
            return Err("Write a query, e.g. SELECT * FROM Sheet1".into());
        }
        let mut tried = Vec::new();
        loop {
            let msg = match self.conn.prepare(sql) {
                Ok(stmt) => return collect(stmt, anchor),
                Err(e) => e.to_string(),
            };
            let Some(name) = msg.strip_prefix("no such table: ").map(|n| n.strip_prefix("main.").unwrap_or(n).to_string()) else { return Err(msg) };
            if tried.contains(&name) {
                return Err(msg);
            }
            self.load_source(wb, &name, generation, base_dir)?;
            tried.push(name);
        }
    }

    fn drop_stale(&mut self, generation: u64) {
        let stale: Vec<String> = self
            .loaded
            .iter()
            .filter(|(_, l)| match &l.stamp {
                Stamp::Workbook(g) => *g != generation,
                Stamp::File(p, t) => file_mtime(p) != *t,
            })
            .map(|(n, _)| n.clone())
            .collect();
        for name in stale {
            self.conn.execute_batch(&format!("DROP TABLE IF EXISTS {}", quote_ident(&name))).ok();
            self.loaded.remove(&name);
        }
    }

    /// Load a table, sheet or file named `name` into the database as `name`.
    fn load_source(&mut self, wb: &Workbook, name: &str, generation: u64, base_dir: Option<&Path>) -> Result<(), String> {
        if let Some((sheet, t)) = wb.table_by_name(name) {
            let mut r = t.range;
            if t.totals && r.rows() > 1 {
                r.end.row -= 1;
            }
            self.load_range(wb, sheet, r, name)?;
            self.loaded.insert(name.to_string(), Loaded { stamp: Stamp::Workbook(generation) });
            return Ok(());
        }
        if let Some(sheet) = wb.sheet_ids().iter().copied().find(|&s| wb.sheet_name(s).eq_ignore_ascii_case(name)) {
            let end = wb.store(sheet).extent().unwrap_or_default();
            self.load_range(wb, sheet, Range::new(CellRef::new(0, 0), end), name)?;
            self.loaded.insert(name.to_string(), Loaded { stamp: Stamp::Workbook(generation) });
            return Ok(());
        }
        let path = resolve_path(name, base_dir).ok_or_else(|| format!("No sheet, table or file named “{name}”"))?;
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut opts = xl_io::CsvOptions::default();
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("tsv")) {
            opts.delimiter = Some(b'\t');
        }
        let data = xl_io::CsvData::parse(&bytes, &opts)?;
        drop(bytes);
        let mut tmp = Workbook::empty();
        let s = tmp.add_sheet(None)?;
        data.place(&mut tmp, s)?;
        let end = tmp.store(s).extent().unwrap_or_default();
        self.load_range(&tmp, s, Range::new(CellRef::new(0, 0), end), name)?;
        let mtime = file_mtime(&path);
        self.loaded.insert(name.to_string(), Loaded { stamp: Stamp::File(path, mtime) });
        Ok(())
    }

    /// Create table `name` from a range whose first row holds column names.
    fn load_range(&mut self, wb: &Workbook, sheet: SheetId, r: Range, name: &str) -> Result<(), String> {
        let store = wb.store(sheet);
        let mut cols = Vec::new();
        for c in r.start.col..=r.end.col {
            let header = store.value_owned(CellRef::new(r.start.row, c)).display();
            let mut h = if header.trim().is_empty() { col_name(c) } else { header.trim().to_string() };
            if cols.iter().any(|x: &String| x.eq_ignore_ascii_case(&h)) {
                let base = h.clone();
                h = (2..).map(|i| format!("{base}_{i}")).find(|n| !cols.iter().any(|x: &String| x.eq_ignore_ascii_case(n))).unwrap_or(base);
            }
            cols.push(h);
        }
        // Columns shown as dates go in as ISO text.
        let dates: Vec<Option<bool>> = (r.start.col..=r.end.col)
            .map(|c| {
                let f = wb.format(Addr::new(sheet, CellRef::new((r.start.row + 1).min(r.end.row), c)))?;
                f.has_date().then(|| f.has_time())
            })
            .collect();
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        let defs: Vec<String> = cols.iter().map(|c| quote_ident(c)).collect();
        tx.execute_batch(&format!("DROP TABLE IF EXISTS {0}; CREATE TABLE {0} ({1});", quote_ident(name), defs.join(", "))).map_err(|e| e.to_string())?;
        {
            // Several rows per INSERT: far fewer statement executions.
            let n = cols.len();
            let batch = (400 / n).max(1);
            let one = format!("({})", vec!["?"; n].join(", "));
            let sql = |rows: usize| format!("INSERT INTO {} VALUES {}", quote_ident(name), vec![one.as_str(); rows].join(", "));
            let mut many = tx.prepare(&sql(batch)).map_err(|e| e.to_string())?;
            let mut single = tx.prepare(&sql(1)).map_err(|e| e.to_string())?;
            let (first, last) = (r.start.row + 1, r.end.row);
            let mut rr = first;
            while rr <= last && first <= last {
                let rows = if (last - rr + 1) as usize >= batch { batch } else { 1 };
                let stmt = if rows == batch { &mut many } else { &mut single };
                let mut p = 1;
                for row in rr..rr + rows as u32 {
                    for (i, c) in (r.start.col..=r.end.col).enumerate() {
                        let bound = match store.value_owned(CellRef::new(row, c)) {
                            Value::Number(x) => match dates[i].and_then(|t| iso(x, t)) {
                                Some(s) => stmt.raw_bind_parameter(p, s),
                                None => stmt.raw_bind_parameter(p, x),
                            },
                            Value::Text(t) => stmt.raw_bind_parameter(p, &*t),
                            Value::Bool(b) => stmt.raw_bind_parameter(p, b as i64),
                            Value::Empty | Value::Error(_) => stmt.raw_bind_parameter(p, rusqlite::types::Null),
                        };
                        bound.map_err(|e| e.to_string())?;
                        p += 1;
                    }
                }
                stmt.raw_execute().map_err(|e| e.to_string())?;
                rr += rows as u32;
            }
        }
        tx.commit().map_err(|e| e.to_string())
    }
}

/// ISO text for a date serial (with the time when `time` or when it has one).
fn iso(serial: f64, time: bool) -> Option<String> {
    let d = datetime_from_serial(serial)?;
    Some(if time || d.hour + d.minute + d.second > 0 {
        format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", d.year, d.month, d.day, d.hour, d.minute, d.second)
    } else {
        format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
    })
}

fn file_mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn resolve_path(name: &str, base_dir: Option<&Path>) -> Option<PathBuf> {
    let p = Path::new(name);
    let p = if p.is_relative() { base_dir.map_or_else(|| p.to_path_buf(), |b| b.join(p)) } else { p.to_path_buf() };
    p.is_file().then_some(p)
}

fn collect(mut stmt: rusqlite::Statement<'_>, anchor: CellRef) -> Result<QueryResult, String> {
    let columns: Vec<String> = stmt.column_names().into_iter().map(str::to_string).collect();
    let n = columns.len();
    let mut data: Vec<ColumnBuilder> = (0..n).map(|_| ColumnBuilder::new(anchor.row)).collect();
    for (b, name) in data.iter_mut().zip(&columns) {
        b.push_text(name);
    }
    let mut formats: Vec<(u32, &'static str, u32, u32)> = Vec::new();
    let mut rows = 0usize;
    let mut q = stmt.query([]).map_err(|e| e.to_string())?;
    while let Some(row) = q.next().map_err(|e| e.to_string())? {
        if rows >= MAX_RESULT_ROWS - anchor.row as usize {
            break;
        }
        let r = anchor.row + 1 + rows as u32;
        for (i, b) in data.iter_mut().enumerate() {
            match row.get_ref(i).map_err(|e| e.to_string())? {
                ValueRef::Null => b.push_empty(),
                ValueRef::Integer(v) => b.push_number(v as f64),
                ValueRef::Real(v) => b.push_number(v),
                ValueRef::Text(t) => {
                    let t = String::from_utf8_lossy(t);
                    match iso_date(&t) {
                        Some((serial, fmt)) => {
                            b.push_number(serial);
                            match formats.last_mut() {
                                Some(run) if run.0 == i as u32 && run.1 == fmt && run.3 == r => run.3 = r + 1,
                                _ => formats.push((i as u32, fmt, r, r + 1)),
                            }
                        }
                        None => b.push_text(&t),
                    }
                }
                ValueRef::Blob(x) => b.push_text(&format!("(blob, {} bytes)", x.len())),
            }
        }
        rows += 1;
    }
    Ok(QueryResult { columns, rows, data: data.into_iter().map(ColumnBuilder::finish).collect(), formats })
}

/// ISO dates (and date-times) coming out of SQLite become dates again.
fn iso_date(t: &str) -> Option<(f64, &'static str)> {
    let b = t.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    match infer(t, DateOrder::Mdy) {
        Inferred::Formatted(n, fmt) if fmt.starts_with("yyyy-mm-dd") => Some((n, fmt)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> (Workbook, SheetId) {
        let mut wb = Workbook::new();
        let s = wb.sheet_ids()[0];
        let rows = [["region", "rep", "amount", "when"], ["East", "Ann", "100", "2024-01-05"], ["West", "Bob", "250", "2024-01-06"], ["East", "Cy", "50", "2024-02-01"], ["West", "Di", "7", "2024-02-02"], ["North", "007", "", "2024-03-01"]];
        let mut items = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            for (c, v) in row.iter().enumerate() {
                items.push((Addr::new(s, CellRef::new(r as u32, c as u32)), v.to_string()));
            }
        }
        wb.set_inputs(items);
        // An ID typed as text (typing 007 would give the number 7).
        wb.set_values([(Addr::new(s, CellRef::new(5, 1)), Value::text("007"))]);
        (wb, s)
    }

    fn cell(r: &QueryResult, row: usize, col: usize) -> Value {
        let segs = &r.data[col];
        segs.iter().find_map(|s| (s.start_row() as usize..s.start_row() as usize + s.len()).contains(&row).then(|| s.value(row - s.start_row() as usize))).unwrap_or(Value::Empty)
    }

    #[test]
    fn group_by_over_a_sheet() {
        let (wb, _) = book();
        let mut e = SqlEngine::new().unwrap();
        let r = e.run(&wb, "SELECT region, SUM(amount) AS total, COUNT(*) AS n FROM Sheet1 GROUP BY region ORDER BY total DESC", 1, None, CellRef::new(0, 0)).unwrap();
        assert_eq!(r.columns, ["region", "total", "n"]);
        assert_eq!(r.rows, 3);
        assert_eq!(cell(&r, 0, 0), Value::text("region"));
        assert_eq!((cell(&r, 1, 0), cell(&r, 1, 1), cell(&r, 1, 2)), (Value::text("West"), Value::Number(257.0), Value::Number(2.0)));
        assert_eq!(cell(&r, 3, 0), Value::text("North"));
        assert_eq!(cell(&r, 3, 1), Value::Empty, "SUM of only NULLs is NULL");
        // Text that looks like a number stays text; arithmetic is real-valued.
        let r = e.run(&wb, "SELECT rep, amount / 3 FROM Sheet1 WHERE region = 'North' OR rep = 'Di'", 1, None, CellRef::new(0, 0)).unwrap();
        assert_eq!(cell(&r, 1, 1), Value::Number(7.0 / 3.0));
        assert_eq!(cell(&r, 2, 0), Value::text("007"));
    }

    #[test]
    fn dates_round_trip_as_dates() {
        let (wb, _) = book();
        let mut e = SqlEngine::new().unwrap();
        let r = e.run(&wb, "SELECT strftime('%m', \"when\") AS m, MIN(\"when\") AS first FROM Sheet1 GROUP BY m ORDER BY m", 1, None, CellRef::new(2, 3)).unwrap();
        assert_eq!(r.rows, 3);
        assert_eq!(cell(&r, 3, 0), Value::text("01"));
        assert_eq!(cell(&r, 3, 1), Value::Number(xl_engine::datetime::serial_from_date(2024, 1, 5)));
        assert!(r.formats.iter().any(|&(c, f, a, b)| c == 1 && f == "yyyy-mm-dd" && a == 3 && b == 6), "{:?}", r.formats);
        let region = r.region(CellRef::new(2, 3));
        assert_eq!(region.to_string(), "D3:E6");
    }

    #[test]
    fn tables_and_csv_files() {
        let (mut wb, s) = book();
        wb.add_table(s, xl_engine::Table { name: "Sales".into(), range: Range::new(CellRef::new(0, 0), CellRef::new(4, 2)), totals: false, style: Default::default(), banded: true }).unwrap();
        let dir = std::env::temp_dir().join(format!("xl-sql-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("targets.csv"), "region,target\nEast,120\nWest,300\n").unwrap();
        let mut e = SqlEngine::new().unwrap();
        let sql = "SELECT s.region, SUM(s.amount) AS total, t.target FROM Sales s JOIN 'targets.csv' t ON t.region = s.region GROUP BY s.region ORDER BY s.region";
        let r = e.run(&wb, sql, 1, Some(&dir), CellRef::new(0, 0)).unwrap();
        assert_eq!(r.rows, 2);
        assert_eq!((cell(&r, 1, 0), cell(&r, 1, 1), cell(&r, 1, 2)), (Value::text("East"), Value::Number(150.0), Value::Number(120.0)));
        // Absolute paths work too.
        let abs = dir.join("targets.csv");
        let r = e.run(&wb, &format!("SELECT COUNT(*) FROM \"{}\"", abs.display()), 1, None, CellRef::new(0, 0)).unwrap();
        assert_eq!(cell(&r, 1, 0), Value::Number(2.0));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edits_are_seen_after_a_generation_bump() {
        let (mut wb, s) = book();
        let mut e = SqlEngine::new().unwrap();
        let q = "SELECT SUM(amount) FROM Sheet1";
        assert_eq!(cell(&e.run(&wb, q, 1, None, CellRef::new(0, 0)).unwrap(), 1, 0), Value::Number(407.0));
        wb.set_input(Addr::new(s, CellRef::new(1, 2)), "1000").unwrap();
        assert_eq!(cell(&e.run(&wb, q, 1, None, CellRef::new(0, 0)).unwrap(), 1, 0), Value::Number(407.0), "cached until the generation changes");
        assert_eq!(cell(&e.run(&wb, q, 2, None, CellRef::new(0, 0)).unwrap(), 1, 0), Value::Number(1307.0));
    }

    #[test]
    fn errors_are_readable() {
        let (wb, _) = book();
        let mut e = SqlEngine::new().unwrap();
        assert!(e.run(&wb, "SELECT * FROM Nope", 1, None, CellRef::new(0, 0)).unwrap_err().contains("Nope"));
        assert!(e.run(&wb, "SELEC 1", 1, None, CellRef::new(0, 0)).unwrap_err().contains("syntax"));
        assert!(e.run(&wb, "  ", 1, None, CellRef::new(0, 0)).is_err());
    }
}
