//! xl-io: reading and writing workbooks (xlsx, csv).

mod csv_fast;
mod csv_io;
pub mod infer;
mod package;
mod xlsx_chartxml;
mod xlsx_charts;
mod xlsx_patch;
mod xlsx_read;
mod xlsx_refs;
mod xlsx_write;

pub use csv_fast::{CsvData, CsvOptions, detect_delimiter};
pub use package::{Package, SavePlan, save_plan};

use std::path::Path;

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use xl_engine::{RecalcStats, SheetId, Workbook, XLSX_MAX_ROWS};

pub use xlsx_write::{expand_this_row, prefix_future_functions};

/// A workbook read from disk.
pub struct Loaded {
    pub workbook: Workbook,
    /// Non-empty cells loaded.
    pub cells: usize,
    /// Things that didn't come across perfectly.
    pub warnings: Vec<String>,
    /// The initial full recalculation.
    pub recalc: RecalcStats,
    /// Parts of the file xl can't keep (e.g. "charts", "comments"); saving
    /// over the file would drop them.
    pub unsupported: Vec<&'static str>,
    /// The original file, for saving back into without losing what xl
    /// doesn't model (xlsx only).
    pub package: Option<Package>,
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
    open_with_progress(path, &Progress::default())
}

/// The error a cancelled load returns.
pub const CANCELLED: &str = "Cancelled";

/// Progress of a long load, shared with the UI thread, which can also ask
/// for it to stop (the load then returns `Err(CANCELLED)`).
#[derive(Debug, Default)]
pub struct Progress {
    /// 0 = starting, 1 = reading, 2 = parsing, 3 = building the sheet.
    pub phase: AtomicU8,
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn start(&self, phase: u8, total: u64) {
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
        self.phase.store(phase, Ordering::Relaxed);
    }

    pub fn add(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// What's happening and how far along (0..=1).
    pub fn status(&self) -> (&'static str, f64) {
        let label = match self.phase.load(Ordering::Relaxed) {
            1 => "Reading",
            2 => "Parsing",
            3 => "Building sheet",
            _ => "Opening",
        };
        let total = self.total.load(Ordering::Relaxed);
        let frac = if total == 0 { 0.0 } else { (self.done.load(Ordering::Relaxed) as f64 / total as f64).min(1.0) };
        (label, frac)
    }
}

/// Open a file, reporting progress (CSV only; xlsx reports nothing yet)
/// and honouring cancellation.
pub fn open_with_progress(path: &Path, progress: &Progress) -> Result<Loaded, String> {
    match FileKind::of(path) {
        Some(FileKind::Xlsx) => xlsx_read::read_xlsx(path),
        Some(FileKind::Csv) => csv_fast::read_csv_with(path, &CsvOptions::default(), progress),
        None => Err("Unsupported file type (xl opens .xlsx and .csv)".into()),
    }
}

/// The first sheet with more rows than an xlsx file can hold:
/// (sheet name, rows used).
pub fn xlsx_overflow(wb: &Workbook) -> Option<(String, u32)> {
    wb.sheet_ids().iter().find_map(|&id| {
        let end = wb.store(id).extent()?;
        (end.row >= XLSX_MAX_ROWS).then(|| (wb.sheet_name(id).to_string(), end.row + 1))
    })
}

/// The message for [`xlsx_overflow`].
pub fn xlsx_overflow_message(sheet: &str, rows: u32) -> String {
    format!("“{sheet}” has {rows} rows, but an .xlsx file holds at most {XLSX_MAX_ROWS}. Save as .csv to keep every row.")
}

/// Save the workbook. CSV files hold one sheet: `sheet` picks which.
///
/// With the package of the xlsx the workbook came from, xlsx saves patch
/// that package (when [`save_plan`] allows), keeping everything xl doesn't
/// model. Returns the package of the file just written, to save into next.
pub fn save(wb: &Workbook, path: &Path, sheet: SheetId, package: Option<&Package>) -> Result<Option<Package>, String> {
    match FileKind::of(path) {
        Some(FileKind::Xlsx) => {
            if let Some((sheet, rows)) = xlsx_overflow(wb) {
                return Err(xlsx_overflow_message(&sheet, rows));
            }
            match package {
                Some(pkg) if save_plan(wb, pkg) == SavePlan::Patch => xlsx_patch::write_patched(wb, pkg, path)?,
                _ => xlsx_write::write_xlsx(wb, path)?,
            }
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            Ok(Package::new(bytes, wb).ok())
        }
        Some(FileKind::Csv) => csv_io::write_csv(wb, sheet, path).map(|_| None),
        None => Err("Unsupported file type (xl saves .xlsx and .csv)".into()),
    }
}

/// Our default column is 100px where Excel's is 64px (8.43 chars of
/// Calibri 11), so widths scale by that ratio to keep proportions.
const SCALE: f64 = 100.0 / 64.0;

/// Pixel height of a default row in xl (the app's ROW_H). Excel's default is
/// 15pt, so heights scale by that ratio to keep proportions.
pub const DEFAULT_ROW_PX: f64 = 22.0;

/// Excel row height in points to our pixels.
pub fn from_excel_height(pt: f64) -> f64 {
    pt * DEFAULT_ROW_PX / 15.0
}

/// Our row height in pixels to Excel points.
pub fn to_excel_height(px: f64) -> f64 {
    ((px * 15.0 / DEFAULT_ROW_PX) * 4.0).round() / 4.0
}

/// Excel `<col width>` (characters, padding included) to our pixels, using
/// the OOXML formula for a 7px max digit width (Calibri 11):
/// `px = trunc((256 * w + trunc(128 / 7)) / 256 * 7)`.
pub fn from_excel_width(width: f64) -> f64 {
    let excel_px = (((256.0 * width + 18.0) / 256.0) * 7.0).trunc();
    (excel_px * SCALE).round().max(8.0)
}

/// Excel pixels (horizontal) to ours.
pub fn from_excel_width_px(px: f64) -> f64 {
    px * SCALE
}

/// Our pixels to Excel pixels.
pub fn to_excel_width_px(px: f64) -> u32 {
    (px / SCALE).round().max(1.0) as u32
}
