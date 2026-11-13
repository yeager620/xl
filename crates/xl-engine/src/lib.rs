//! xl-engine: the headless spreadsheet core (storage, formulas, incremental recalc).

pub mod address;
pub mod functions;
pub mod parser;
pub mod sheet;
pub mod store;
pub mod value;

pub use address::{CellRef, MAX_COLS, MAX_ROWS, Range, col_name, parse_col};
pub use functions::Func;
pub use parser::ParseError;
pub use sheet::{RecalcStats, Sheet, parse_input};
pub use store::{Cell, CellInput, CellStore};
pub use value::{ErrorKind, Value, format_general};
