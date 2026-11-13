//! xl-engine: the headless spreadsheet core — multi-sheet storage, formulas,
//! number formats and incremental recalculation.

pub mod address;
pub mod datetime;
pub mod eval;
pub mod format;
pub mod formula;
pub mod input;
pub mod store;
pub mod value;
pub mod workbook;

pub use address::{Addr, CellRef, MAX_COLS, MAX_ROWS, Range, SheetId, col_name, parse_col};
pub use eval::Func;
pub use format::{FmtColor, Formatted, NumberFormat};
pub use formula::{ParseError, quote_sheet_name};
pub use store::{Cell, CellInput, CellStore};
pub use value::{ErrorKind, Value, format_general};
pub use workbook::{CellSnapshot, LoadedCell, RecalcStats, Workbook, precise, validate_sheet_name};
