//! CSV/TSV import and export (one sheet).

use std::path::Path;

use xl_engine::{Addr, CellRef, MAX_COLS, MAX_ROWS, SheetId, Workbook};

use crate::Loaded;

fn sheet_name_for(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Sheet1");
    let cleaned: String = stem.chars().filter(|c| !matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\')).take(31).collect();
    let cleaned = cleaned.trim_matches('\'').trim().to_string();
    if cleaned.is_empty() { "Sheet1".into() } else { cleaned }
}

pub fn read_csv(path: &Path) -> Result<Loaded, String> {
    let tsv = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("tsv"));
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(if tsv { b'\t' } else { b',' })
        .from_path(path)
        .map_err(|e| e.to_string())?;
    let mut wb = Workbook::empty();
    let sheet = wb.add_sheet(Some(&sheet_name_for(path))).or_else(|_| wb.add_sheet(None))?;
    let mut items = Vec::new();
    let mut warnings = Vec::new();
    for (r, rec) in rdr.records().enumerate() {
        let rec = rec.map_err(|e| e.to_string())?;
        if r as u32 >= MAX_ROWS {
            warnings.push(format!("Only the first {MAX_ROWS} rows were loaded"));
            break;
        }
        for (c, field) in rec.iter().enumerate() {
            let field = if r == 0 && c == 0 { field.trim_start_matches('\u{feff}') } else { field };
            if !field.is_empty() && (c as u32) < MAX_COLS {
                items.push((Addr::new(sheet, CellRef::new(r as u32, c as u32)), field.to_string()));
            }
        }
    }
    let cells = items.len();
    let recalc = wb.set_inputs(items);
    Ok(Loaded { workbook: wb, cells, warnings, recalc, unsupported: Vec::new(), package: None })
}

/// CSV holds what you see: formatted values, not formulas.
pub fn write_csv(wb: &Workbook, sheet: SheetId, path: &Path) -> Result<(), String> {
    let tsv = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("tsv"));
    let mut w = csv::WriterBuilder::new().flexible(true).delimiter(if tsv { b'\t' } else { b',' }).from_path(path).map_err(|e| e.to_string())?;
    if let Some(end) = wb.store(sheet).extent() {
        for r in 0..=end.row {
            let row: Vec<String> = (0..=end.col).map(|c| wb.display(Addr::new(sheet, CellRef::new(r, c))).text).collect();
            w.write_record(&row).map_err(|e| e.to_string())?;
        }
    }
    w.flush().map_err(|e| e.to_string())
}
