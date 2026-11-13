//! xl-io: reading and writing workbooks (xlsx, csv).

mod csv_io;
mod xlsx_read;
mod xlsx_write;

use std::path::Path;

use xl_engine::{RecalcStats, SheetId, Workbook};

pub use xlsx_write::prefix_future_functions;

/// A workbook read from disk.
pub struct Loaded {
    pub workbook: Workbook,
    /// Non-empty cells loaded.
    pub cells: usize,
    /// Things that didn't come across perfectly.
    pub warnings: Vec<String>,
    /// The initial full recalculation.
    pub recalc: RecalcStats,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FileKind {
    Xlsx,
    Csv,
}

impl FileKind {
    pub fn of(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "xlsx" | "xlsm" => Some(FileKind::Xlsx),
            "csv" | "tsv" | "txt" => Some(FileKind::Csv),
            _ => None,
        }
    }
}

pub fn open(path: &Path) -> Result<Loaded, String> {
    match FileKind::of(path) {
        Some(FileKind::Xlsx) => xlsx_read::read_xlsx(path),
        Some(FileKind::Csv) => csv_io::read_csv(path),
        None => Err("Unsupported file type (xl opens .xlsx and .csv)".into()),
    }
}

/// Save the workbook. CSV files hold one sheet: `sheet` picks which.
pub fn save(wb: &Workbook, path: &Path, sheet: SheetId) -> Result<(), String> {
    match FileKind::of(path) {
        Some(FileKind::Xlsx) => xlsx_write::write_xlsx(wb, path),
        Some(FileKind::Csv) => csv_io::write_csv(wb, sheet, path),
        None => Err("Unsupported file type (xl saves .xlsx and .csv)".into()),
    }
}

/// Our default column is 100px where Excel's is 64px (8.43 chars of
/// Calibri 11), so widths scale by that ratio to keep proportions.
const SCALE: f64 = 100.0 / 64.0;

/// Excel `<col width>` (characters, padding included) to our pixels, using
/// the OOXML formula for a 7px max digit width (Calibri 11):
/// `px = trunc((256 * w + trunc(128 / 7)) / 256 * 7)`.
pub fn from_excel_width(width: f64) -> f64 {
    let excel_px = (((256.0 * width + 18.0) / 256.0) * 7.0).trunc();
    (excel_px * SCALE).round().max(8.0)
}

/// Our pixels to Excel pixels.
pub fn to_excel_width_px(px: f64) -> u32 {
    (px / SCALE).round().max(1.0) as u32
}
