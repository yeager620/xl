//! xl-engine: the headless spreadsheet core — multi-sheet storage, formulas,
//! number formats and incremental recalculation.

pub mod address;
pub mod chart;
pub mod cond;
pub mod datetime;
pub mod eval;
pub mod format;
pub mod formula;
pub mod input;
pub mod pivot;
pub mod solver;
pub mod store;
pub mod style;
pub mod validation;
pub mod value;
pub mod workbook;

pub use address::{Addr, CellRef, MAX_COLS, MAX_ROWS, Range, SheetId, col_name, parse_col};
pub use eval::Func;
pub use format::{FmtColor, Formatted, NumberFormat};
pub use formula::{Move, ParseError, Shift, quote_sheet_name};
pub use store::{Cell, CellInput, CellStore};
pub use style::{Border, HAlign, LineStyle, Rgb, Style, VAlign};
pub use value::{ErrorKind, Value, format_general};
pub use workbook::{AutoFilter, CellSnapshot, DATE1904_OFFSET, Link, Note, StructOp, Table, TableStyle, LoadedCell, RecalcStats, SheetLayout, Workbook, precise, validate_sheet_name};
