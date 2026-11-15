//! CSV/TSV import and export (one sheet).

use std::io::Write;
use std::path::Path;

use rayon::prelude::*;
use xl_engine::{Addr, CellRef, SheetId, Value, Workbook};

pub(crate) fn sheet_name_for(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Sheet1");
    let cleaned: String = stem.chars().filter(|c| !matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\')).take(31).collect();
    let cleaned = cleaned.trim_matches('\'').trim().to_string();
    if cleaned.is_empty() { "Sheet1".into() } else { cleaned }
}

/// Rows per parallel formatting batch.
const BATCH_ROWS: u32 = 16_384;

/// CSV holds what you see: formatted values, not formulas. Rows are
/// formatted in parallel batches and written in order.
pub fn write_csv(wb: &Workbook, sheet: SheetId, path: &Path) -> Result<(), String> {
    let tsv = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("tsv"));
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
    let Some(end) = wb.store(sheet).extent() else { return out.flush().map_err(|e| e.to_string()) };
    let batches: Vec<u32> = (0..=end.row).step_by(BATCH_ROWS as usize).collect();
    // Bounded memory: format a few batches per thread at a time.
    for group in batches.chunks(rayon::current_num_threads().max(1) * 2) {
        let bufs: Vec<Result<Vec<u8>, String>> = group.par_iter().map(|&first| format_rows(wb, sheet, first, (first + BATCH_ROWS - 1).min(end.row), end.col, tsv)).collect();
        for b in bufs {
            out.write_all(&b?).map_err(|e| e.to_string())?;
        }
    }
    out.flush().map_err(|e| e.to_string())
}

fn format_rows(wb: &Workbook, sheet: SheetId, first: u32, last: u32, last_col: u32, tsv: bool) -> Result<Vec<u8>, String> {
    let store = wb.store(sheet);
    let mut w = csv::WriterBuilder::new().flexible(true).delimiter(if tsv { b'\t' } else { b',' }).from_writer(Vec::with_capacity(1 << 20));
    let mut row: Vec<String> = Vec::with_capacity(last_col as usize + 1);
    for r in first..=last {
        row.clear();
        for c in 0..=last_col {
            let a = Addr::new(sheet, CellRef::new(r, c));
            // Read values without materializing compact cells.
            let v = store.value_owned(a.cell);
            row.push(match (wb.format(a), &v) {
                (_, Value::Empty) => String::new(),
                (None, Value::Text(t)) => t.to_string(),
                (None, v) => v.display(),
                (Some(f), v) => f.format(v).text,
            });
        }
        w.write_record(&row).map_err(|e| e.to_string())?;
    }
    w.into_inner().map_err(|e| e.to_string())
}
