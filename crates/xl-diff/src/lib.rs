//! xl-diff: cell-level comparison of workbooks.
//!
//! [`diff_workbooks`] compares two [`xl_engine::Workbook`]s, independent of
//! any UI: sheets added, removed or renamed; rows and columns inserted or
//! deleted (aligned by content so an inserted row doesn't make everything
//! below it "changed"); cells whose formula, value, number format or style
//! changed; and layout changes (widths, merges, charts, notes, validations,
//! defined names…). [`render_diff`] and [`textconv`] turn diffs and
//! workbooks into stable text, used by the `xl diff` and `xl textconv`
//! command-line modes ([`cli`]).

pub mod align;
pub mod cli;
mod diff;
mod render;

pub use diff::{CellChange, CellInfo, CellKind, LayoutChange, LineAxis, LineChange, NameChange, SheetDiff, SheetStatus, WorkbookDiff, diff_workbooks};
pub use render::{cell_aspects, content_text, escape, render_diff, style_delta, textconv, value_text};

/// Give a one-sheet `old` workbook the name of `new`'s only sheet. A CSV
/// file's sheet is named after the file, so comparing two CSV files (or a
/// CSV with a one-sheet workbook) would otherwise report a rename.
pub fn same_sheet_name(old: &mut xl_engine::Workbook, new: &xl_engine::Workbook) {
    if let ([o], [n]) = (old.sheet_ids(), new.sheet_ids()) {
        let (o, name) = (*o, new.sheet_name(*n).to_string());
        if old.sheet_name(o) != name {
            old.rename_sheet(o, &name).ok();
        }
    }
}
