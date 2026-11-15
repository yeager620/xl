//! Fast CSV import: the whole file in memory, split into chunks at record
//! boundaries, parsed in parallel straight into compact columns.
//!
//! Splitting needs to know which newlines are inside quoted fields. In
//! RFC 4180 CSV an escaped quote is doubled, so a position is inside quotes
//! exactly when an odd number of `"` precede it. Each chunk counts its
//! quotes (in parallel), a prefix sum gives the parity at every chunk start,
//! and each chunk then starts at its first newline outside quotes. If the
//! total count is odd (malformed quoting) the file is parsed as one chunk.

use std::borrow::Cow;
use std::path::Path;
use std::sync::atomic::Ordering;

use rayon::prelude::*;
use xl_engine::bulk::{ColumnBuilder, Segment, Slot, clip_segments, offset_segments};
use xl_engine::{CellRef, MAX_COLS, MAX_ROWS, SheetId, Workbook};

use crate::Progress;
use crate::infer::{DateOrder, Inferred, OrderEvidence, infer};

/// How to read a CSV file.
#[derive(Clone, Debug, Default)]
pub struct CsvOptions {
    /// Field separator; None detects it from the first lines (`,` `;` tab `|`).
    pub delimiter: Option<u8>,
    /// Day/month order for ambiguous slash dates when the data doesn't
    /// settle it; None uses the locale.
    pub date_order: Option<DateOrder>,
}

const CHUNK: usize = 4 << 20;

/// One parsed chunk: its rows, columns (rows relative to the chunk) and
/// number-format runs `(col, format, first row, end row)`.
struct Parsed {
    rows: u32,
    cols: Vec<Vec<Segment>>,
    formats: Vec<(u32, &'static str, u32, u32)>,
    evidence: Vec<OrderEvidence>,
    wide: bool,
}

/// The result of parsing a buffer: compact columns ready for a sheet.
pub struct CsvData {
    pub cols: Vec<Vec<Segment>>,
    pub formats: Vec<(u32, &'static str, u32, u32)>,
    pub rows: u32,
    pub cells: usize,
    pub warnings: Vec<String>,
}

impl CsvData {
    /// Parse a buffer without progress reporting.
    pub fn parse(data: &[u8], opts: &CsvOptions) -> Result<CsvData, String> {
        parse(data, opts, &Progress::default())
    }

    /// Put the data on `sheet` (which must be empty).
    pub fn place(self, wb: &mut Workbook, sheet: SheetId) -> Result<(), String> {
        for &(col, fmt, a, b) in &self.formats {
            wb.set_format_cells(sheet, Some(fmt), (a..b).map(|r| CellRef::new(r, col)));
        }
        wb.load_columns(sheet, 0, self.cols)?;
        Ok(())
    }
}

/// Read the file into memory, reporting progress.
pub fn read_file(path: &Path, progress: &Progress) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    progress.start(1, len);
    let mut buf = Vec::with_capacity(len as usize + 1);
    let mut chunk = vec![0u8; 16 << 20];
    loop {
        if progress.cancelled() {
            return Err(crate::CANCELLED.into());
        }
        let n = f.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        progress.add(n as u64);
    }
    Ok(buf)
}

/// Field separator that splits the sample's lines most consistently.
pub fn detect_delimiter(sample: &[u8]) -> u8 {
    let cut = sample.iter().rposition(|&b| b == b'\n').map_or(sample.len(), |p| p + 1);
    let sample = &sample[..cut];
    let mut best = (b',', 0usize, 0.0f64);
    for &d in b",\t;|" {
        let mut rdr = csv::ReaderBuilder::new().has_headers(false).flexible(true).delimiter(d).from_reader(sample);
        let mut counts = Vec::new();
        let mut rec = csv::ByteRecord::new();
        while counts.len() < 200 && rdr.read_byte_record(&mut rec).unwrap_or(false) {
            counts.push(rec.len());
        }
        if counts.is_empty() {
            continue;
        }
        let mut freq = std::collections::HashMap::new();
        for &c in &counts {
            *freq.entry(c).or_insert(0usize) += 1;
        }
        let (&mode, &n) = freq.iter().max_by_key(|(c, n)| (**n, **c)).unwrap();
        let consistency = n as f64 / counts.len() as f64;
        if mode > 1 && (consistency > best.2 + 0.05 || (consistency >= best.2 - 0.05 && mode > best.1)) {
            best = (d, mode, consistency);
        }
    }
    best.0
}

/// Chunk start offsets (each at a record boundary), plus the end.
fn split(data: &[u8]) -> Vec<usize> {
    if data.len() <= CHUNK {
        return vec![0, data.len()];
    }
    let n = data.len().div_ceil(CHUNK);
    let quotes: Vec<usize> = (0..n).into_par_iter().map(|i| memchr::memchr_iter(b'"', &data[i * CHUNK..((i + 1) * CHUNK).min(data.len())]).count()).collect();
    if quotes.iter().sum::<usize>() % 2 == 1 {
        return vec![0, data.len()];
    }
    let mut parity = Vec::with_capacity(n);
    let mut acc = 0usize;
    for q in &quotes {
        parity.push(acc % 2 == 1);
        acc += q;
    }
    let mut starts: Vec<usize> = (1..n)
        .into_par_iter()
        .map(|i| {
            let mut inside = parity[i];
            for (k, &b) in data[i * CHUNK..].iter().enumerate() {
                match b {
                    b'"' => inside = !inside,
                    b'\n' if !inside => return i * CHUNK + k + 1,
                    _ => {}
                }
            }
            data.len()
        })
        .collect();
    starts.insert(0, 0);
    starts.push(data.len());
    starts.dedup();
    starts
}

fn decode(field: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(field) {
        Ok(s) => Cow::Borrowed(s),
        // Not UTF-8: read it as Latin-1 (every byte is a character) rather
        // than replacing bytes.
        Err(_) => Cow::Owned(field.iter().map(|&b| b as char).collect()),
    }
}

fn parse_chunk(data: &[u8], delim: u8, orders: &[DateOrder], default: DateOrder, progress: &Progress) -> Result<Parsed, String> {
    let mut rdr = csv::ReaderBuilder::new().has_headers(false).flexible(true).delimiter(delim).from_reader(data);
    let mut rec = csv::ByteRecord::new();
    let mut cols: Vec<ColumnBuilder> = Vec::new();
    let mut evidence: Vec<OrderEvidence> = Vec::new();
    let mut formats: Vec<(u32, &'static str, u32, u32)> = Vec::new();
    let mut last_run: Vec<Option<usize>> = Vec::new();
    let mut row = 0u32;
    let mut wide = false;
    loop {
        match rdr.read_byte_record(&mut rec) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => return Err(format!("line {}: {e}", rec.position().map_or(0, |p| p.line()))),
        }
        if row.is_multiple_of(65_536) && progress.cancelled() {
            return Err(crate::CANCELLED.into());
        }
        if rec.len() > cols.len() {
            let n = rec.len().min(MAX_COLS as usize);
            wide |= rec.len() > n;
            while cols.len() < n {
                cols.push(ColumnBuilder::new(0));
                evidence.push(OrderEvidence::default());
                last_run.push(None);
            }
        }
        for (c, field) in rec.iter().enumerate().take(cols.len()) {
            if field.is_empty() {
                continue;
            }
            let text = decode(field);
            let order = orders.get(c).copied().unwrap_or(default);
            let b = &mut cols[c];
            match infer(&text, order) {
                Inferred::Empty => continue,
                Inferred::Number(n) => {
                    b.pad_to(row);
                    b.push_number(n);
                }
                Inferred::Bool(v) => {
                    b.pad_to(row);
                    b.push_slot(Slot::boolean(v));
                }
                Inferred::Text(s) => {
                    if s.contains('/') {
                        evidence[c].observe(s);
                    }
                    b.pad_to(row);
                    b.push_text(s);
                }
                Inferred::Formatted(n, fmt) => {
                    if text.contains('/') {
                        evidence[c].observe(&text);
                    }
                    b.pad_to(row);
                    b.push_number(n);
                    match last_run[c].map(|i| &mut formats[i]) {
                        Some(run) if run.1 == fmt && run.3 == row => run.3 = row + 1,
                        _ => {
                            last_run[c] = Some(formats.len());
                            formats.push((c as u32, fmt, row, row + 1));
                        }
                    }
                }
            }
        }
        row += 1;
    }
    progress.add(data.len() as u64);
    Ok(Parsed { rows: row, cols: cols.into_iter().map(ColumnBuilder::finish).collect(), formats, evidence, wide })
}

/// Parse a whole CSV buffer (BOM allowed) into compact columns.
pub fn parse(data: &[u8], opts: &CsvOptions, progress: &Progress) -> Result<CsvData, String> {
    let data = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data);
    let delim = opts.delimiter.unwrap_or_else(|| detect_delimiter(&data[..data.len().min(64 << 10)]));
    let default = opts.date_order.unwrap_or_else(DateOrder::from_env);
    let bounds = split(data);
    let mut orders: Vec<DateOrder> = Vec::new();
    // Slash-date order is per column and decided by the data: parse with
    // the default, then once more if some column's dates turned out to be
    // the other way round (so no date is ever read with its day and month
    // swapped).
    for attempt in 0..2 {
        progress.start(2, data.len() as u64);
        let parts: Vec<Result<Parsed, String>> = bounds.par_windows(2).map(|w| parse_chunk(&data[w[0]..w[1]], delim, &orders, default, progress)).collect();
        let parts: Vec<Parsed> = parts.into_iter().collect::<Result<_, _>>()?;
        let ncols = parts.iter().map(|p| p.cols.len()).max().unwrap_or(0);
        let mut evidence = vec![OrderEvidence::default(); ncols];
        for p in &parts {
            for (e, x) in evidence.iter_mut().zip(&p.evidence) {
                e.day_first |= x.day_first;
                e.month_first |= x.month_first;
            }
        }
        let decided: Vec<DateOrder> = evidence.iter().map(|e| e.decide(default)).collect();
        let used: Vec<DateOrder> = (0..ncols).map(|c| orders.get(c).copied().unwrap_or(default)).collect();
        if attempt == 0 && decided != used {
            orders = decided;
            continue;
        }
        return Ok(assemble(parts, ncols));
    }
    unreachable!("the second attempt always returns")
}

fn assemble(parts: Vec<Parsed>, ncols: usize) -> CsvData {
    let mut warnings = Vec::new();
    let mut cols: Vec<Vec<Segment>> = (0..ncols).map(|_| Vec::new()).collect();
    let mut formats = Vec::new();
    let mut offset = 0u64;
    let mut truncated = false;
    let mut wide = false;
    for p in parts {
        wide |= p.wide;
        if offset >= MAX_ROWS as u64 {
            truncated |= p.rows > 0;
            continue;
        }
        let at = offset as u32;
        let end = offset + p.rows as u64;
        for (c, mut segs) in p.cols.into_iter().enumerate() {
            offset_segments(&mut segs, at);
            if end > MAX_ROWS as u64 {
                clip_segments(&mut segs, MAX_ROWS);
            }
            cols[c].extend(segs);
        }
        for (c, f, a, b) in p.formats {
            let (a, b) = ((a + at).min(MAX_ROWS), (b as u64 + offset).min(MAX_ROWS as u64) as u32);
            if a < b {
                formats.push((c, f, a, b));
            }
        }
        truncated |= end > MAX_ROWS as u64;
        offset = end;
    }
    if truncated {
        warnings.push(format!("the file has {offset}+ rows; only the first {MAX_ROWS} were loaded"));
    }
    if wide {
        warnings.push(format!("only the first {MAX_COLS} columns were loaded"));
    }
    let cells = cols.iter().flatten().map(Segment::filled).sum();
    CsvData { cols, formats, rows: offset.min(MAX_ROWS as u64) as u32, cells, warnings }
}

/// Open a CSV file as a one-sheet workbook.
pub fn read_csv_with(path: &Path, opts: &CsvOptions, progress: &Progress) -> Result<crate::Loaded, String> {
    let mut opts = opts.clone();
    if opts.delimiter.is_none() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("tsv")) {
        opts.delimiter = Some(b'\t');
    }
    let bytes = read_file(path, progress)?;
    let data = parse(&bytes, &opts, progress)?;
    drop(bytes);
    progress.start(3, 1);
    let mut wb = Workbook::empty();
    let sheet = wb.add_sheet(Some(&crate::csv_io::sheet_name_for(path))).or_else(|_| wb.add_sheet(None))?;
    let (cells, warnings) = (data.cells, data.warnings.clone());
    data.place(&mut wb, sheet)?;
    progress.done.store(1, Ordering::Relaxed);
    Ok(crate::Loaded { workbook: wb, cells, warnings, recalc: Default::default(), unsupported: Vec::new(), package: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use xl_engine::{Addr, Value};

    fn load(text: &str) -> (Workbook, SheetId, CsvData) {
        load_with(text, CsvOptions { delimiter: None, date_order: Some(DateOrder::Mdy) })
    }

    fn load_with(text: &str, opts: CsvOptions) -> (Workbook, SheetId, CsvData) {
        let data = parse(text.as_bytes(), &opts, &Progress::default()).unwrap();
        let summary = CsvData { cols: Vec::new(), formats: data.formats.clone(), rows: data.rows, cells: data.cells, warnings: data.warnings.clone() };
        let mut wb = Workbook::empty();
        let s = wb.add_sheet(None).unwrap();
        data.place(&mut wb, s).unwrap();
        (wb, s, summary)
    }

    fn cell(wb: &Workbook, s: SheetId, a1: &str) -> Value {
        wb.value(Addr::new(s, CellRef::parse(a1).unwrap())).clone()
    }

    #[test]
    fn quotes_newlines_and_bom() {
        let (wb, s, d) = load("\u{feff}name,note\n\"Smith, J\",\"said \"\"hi\"\"\nthen left\"\nx,\n");
        assert_eq!(cell(&wb, s, "A1"), Value::text("name"));
        assert_eq!(cell(&wb, s, "A2"), Value::text("Smith, J"));
        assert_eq!(cell(&wb, s, "B2"), Value::text("said \"hi\"\nthen left"));
        assert_eq!(cell(&wb, s, "A3"), Value::text("x"));
        assert_eq!(cell(&wb, s, "B3"), Value::Empty);
        assert_eq!(d.rows, 3);
        assert_eq!(d.cells, 5);
    }

    #[test]
    fn delimiters_detected() {
        assert_eq!(detect_delimiter(b"a;b;c\n1;2;3\n4;5;6\n"), b';');
        assert_eq!(detect_delimiter(b"a\tb\tc\n1\t2,5\t3\n"), b'\t');
        assert_eq!(detect_delimiter(b"a|b\n1|2\n"), b'|');
        assert_eq!(detect_delimiter(b"a,b,c\n1,2,3\n"), b',');
        assert_eq!(detect_delimiter(b"\"x;y\",b\n\"1;2\",3\n"), b',', "separators inside quotes don't count");
        let (wb, s, _) = load("a;b\n1,5;2\n");
        assert_eq!(cell(&wb, s, "A2"), Value::text("1,5"), "decimal comma stays text, never 15");
        assert_eq!(cell(&wb, s, "B2"), Value::Number(2.0));
        let (wb, s, _) = load_with("a,b\n1,2\n", CsvOptions { delimiter: Some(b'|'), date_order: None });
        assert_eq!(cell(&wb, s, "A1"), Value::text("a,b"));
    }

    #[test]
    fn types_inferred_without_mangling() {
        let (wb, s, d) = load("id,zip,card,amount,when,flag,pct\n007,02134,4111111111111111,12.5,2024-03-09,TRUE,5%\n8,90210,12,-3,2024-03-10,false,10%\n");
        assert_eq!(cell(&wb, s, "A2"), Value::text("007"));
        assert_eq!(cell(&wb, s, "A3"), Value::Number(8.0));
        assert_eq!(cell(&wb, s, "B2"), Value::text("02134"));
        assert_eq!(cell(&wb, s, "B3"), Value::Number(90210.0));
        assert_eq!(cell(&wb, s, "C2"), Value::text("4111111111111111"));
        assert_eq!(cell(&wb, s, "D2"), Value::Number(12.5));
        assert_eq!(cell(&wb, s, "E2"), Value::Number(xl_engine::datetime::serial_from_date(2024, 3, 9)));
        assert_eq!(wb.display(Addr::new(s, CellRef::parse("E2").unwrap())).text, "2024-03-09");
        assert_eq!(cell(&wb, s, "F2"), Value::Bool(true));
        assert_eq!(cell(&wb, s, "G3"), Value::Number(0.1));
        assert_eq!(wb.display(Addr::new(s, CellRef::parse("G3").unwrap())).text, "10%");
        assert!(d.formats.iter().any(|&(c, f, a, b)| c == 4 && f == "yyyy-mm-dd" && a == 1 && b == 3), "date formats come in runs: {:?}", d.formats);
    }

    #[test]
    fn slash_date_order_follows_the_data() {
        // Ambiguous at first; a later row settles it day-first, so the whole
        // column is read day-first even though the default is month-first.
        let mut text = String::from("d\n");
        for _ in 0..10 {
            text.push_str("01/02/2024\n");
        }
        text.push_str("25/12/2024\n");
        let (wb, s, _) = load(&text);
        assert_eq!(cell(&wb, s, "A2"), Value::Number(xl_engine::datetime::serial_from_date(2024, 2, 1)));
        assert_eq!(cell(&wb, s, "A12"), Value::Number(xl_engine::datetime::serial_from_date(2024, 12, 25)));
        let (wb, s, _) = load("d\n01/02/2024\n");
        assert_eq!(cell(&wb, s, "A2"), Value::Number(xl_engine::datetime::serial_from_date(2024, 1, 2)), "no evidence: the default order");
    }

    #[test]
    fn latin1_and_ragged_rows() {
        let mut bytes = b"caf\xe9,x\n1\n2,3,4\n".to_vec();
        bytes.extend_from_slice(b"\r\n");
        let data = parse(&bytes, &CsvOptions::default(), &Progress::default()).unwrap();
        let mut wb = Workbook::empty();
        let s = wb.add_sheet(None).unwrap();
        data.place(&mut wb, s).unwrap();
        assert_eq!(cell(&wb, s, "A1"), Value::text("café"));
        assert_eq!(cell(&wb, s, "C3"), Value::Number(4.0));
        assert_eq!(cell(&wb, s, "B2"), Value::Empty);
    }

    #[test]
    fn parallel_chunks_match_sequential() {
        // > CHUNK bytes with quoted newlines straddling chunk edges.
        let mut text = String::from("n,txt,v\n");
        let mut i = 0u64;
        while text.len() < 3 * CHUNK + 1000 {
            text.push_str(&format!("{i},\"line one\nline \"\"two\"\", {i}\",{}\n", i * 3));
            i += 1;
        }
        let d = parse(text.as_bytes(), &CsvOptions::default(), &Progress::default()).unwrap();
        assert_eq!(d.rows as u64, i + 1);
        let mut wb = Workbook::empty();
        let s = wb.add_sheet(None).unwrap();
        d.place(&mut wb, s).unwrap();
        for r in [1u64, i / 3, i / 2, i] {
            let n = r - 1;
            assert_eq!(cell(&wb, s, &format!("A{}", r + 1)), Value::Number(n as f64));
            assert_eq!(cell(&wb, s, &format!("B{}", r + 1)), Value::text(format!("line one\nline \"two\", {n}")));
            assert_eq!(cell(&wb, s, &format!("C{}", r + 1)), Value::Number((n * 3) as f64));
        }
    }

    #[test]
    fn cancel_stops_loading() {
        let p = Progress::default();
        p.cancel();
        let text = "a\n".repeat(100_000);
        assert_eq!(parse(text.as_bytes(), &CsvOptions::default(), &p).err().as_deref(), Some(crate::CANCELLED));
    }
}
