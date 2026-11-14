//! Columnar sparse cell storage: one ordered map per column, so range scans
//! (e.g. `SUM(A:A)`) only touch cells that actually exist.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::address::{CellRef, Range};
use crate::formula::Expr;
use crate::value::Value;

#[derive(Clone, Debug)]
pub enum CellInput {
    Value(Value),
    Formula(Arc<Expr>),
    /// A formula from a file that we can't evaluate (unsupported syntax or
    /// functions). Kept verbatim with the value cached in the file.
    Opaque(Arc<str>),
}

#[derive(Clone, Debug)]
pub struct Cell {
    pub input: CellInput,
    pub value: Value,
}

impl Cell {
    pub fn is_formula(&self) -> bool {
        matches!(self.input, CellInput::Formula(_))
    }
}

static EMPTY: Value = Value::Empty;

#[derive(Default, Clone)]
pub struct CellStore {
    cols: Vec<BTreeMap<u32, Cell>>,
    len: usize,
}

impl CellStore {
    pub fn get(&self, c: CellRef) -> Option<&Cell> {
        self.cols.get(c.col as usize)?.get(&c.row)
    }

    pub fn get_mut(&mut self, c: CellRef) -> Option<&mut Cell> {
        self.cols.get_mut(c.col as usize)?.get_mut(&c.row)
    }

    pub fn value(&self, c: CellRef) -> &Value {
        self.get(c).map_or(&EMPTY, |cell| &cell.value)
    }

    pub fn insert(&mut self, c: CellRef, cell: Cell) -> Option<Cell> {
        let col = c.col as usize;
        if self.cols.len() <= col {
            self.cols.resize_with(col + 1, BTreeMap::new);
        }
        let old = self.cols[col].insert(c.row, cell);
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    pub fn remove(&mut self, c: CellRef) -> Option<Cell> {
        let old = self.cols.get_mut(c.col as usize)?.remove(&c.row);
        if old.is_some() {
            self.len -= 1;
        }
        old
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Non-empty cells within a range, column by column.
    pub fn iter_range(&self, r: Range) -> impl Iterator<Item = (CellRef, &Cell)> + '_ {
        let last = (r.end.col as usize + 1).min(self.cols.len());
        let first = (r.start.col as usize).min(last);
        self.cols[first..last].iter().enumerate().flat_map(move |(i, col)| {
            let c = (first + i) as u32;
            col.range(r.start.row..=r.end.row).map(move |(&row, cell)| (CellRef::new(row, c), cell))
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = (CellRef, &Cell)> + '_ {
        self.cols
            .iter()
            .enumerate()
            .flat_map(|(c, col)| col.iter().map(move |(&row, cell)| (CellRef::new(row, c as u32), cell)))
    }

    /// Move cells for inserted/deleted rows or columns. Returns the cells
    /// that were deleted.
    pub fn apply_shift(&mut self, op: crate::formula::Shift) -> Vec<(CellRef, Cell)> {
        use crate::formula::Shift;
        let mut removed = Vec::new();
        match op {
            Shift::InsertRows { .. } | Shift::DeleteRows { .. } => {
                for (c, col) in self.cols.iter_mut().enumerate() {
                    let old = std::mem::take(col);
                    for (row, cell) in old {
                        match op.map_pos(row) {
                            Some(r) => {
                                col.insert(r, cell);
                            }
                            None => removed.push((CellRef::new(row, c as u32), cell)),
                        }
                    }
                }
            }
            Shift::InsertCols { at, count, .. } => {
                if (at as usize) < self.cols.len() {
                    let tail = self.cols.split_off(at as usize);
                    self.cols.resize_with(at as usize + count as usize, BTreeMap::new);
                    self.cols.extend(tail);
                    // Columns pushed past the sheet edge are lost.
                    let max = crate::address::MAX_COLS as usize;
                    if self.cols.len() > max {
                        for (i, col) in self.cols.drain(max..).enumerate() {
                            removed.extend(col.into_iter().map(|(r, cell)| (CellRef::new(r, (max + i) as u32), cell)));
                        }
                    }
                }
            }
            Shift::DeleteCols { at, count, .. } => {
                let (a, b) = ((at as usize).min(self.cols.len()), ((at + count) as usize).min(self.cols.len()));
                for (i, col) in self.cols.drain(a..b).enumerate() {
                    removed.extend(col.into_iter().map(|(r, cell)| (CellRef::new(r, (a + i) as u32), cell)));
                }
            }
        }
        self.len -= removed.len();
        removed
    }

    /// Mutable access to every cell (for rewriting formulas in place).
    pub fn cells_mut(&mut self) -> impl Iterator<Item = (CellRef, &mut Cell)> + '_ {
        self.cols
            .iter_mut()
            .enumerate()
            .flat_map(|(c, col)| col.iter_mut().map(move |(&row, cell)| (CellRef::new(row, c as u32), cell)))
    }

    /// Next non-empty row in `col` strictly after (or before) `row`.
    pub fn next_filled_in_col(&self, col: u32, row: u32, forward: bool) -> Option<u32> {
        let m = self.cols.get(col as usize)?;
        if forward {
            m.range(row.saturating_add(1)..).next().map(|(&r, _)| r)
        } else {
            m.range(..row).next_back().map(|(&r, _)| r)
        }
    }

    pub fn col_count(&self) -> u32 {
        self.cols.len() as u32
    }

    /// (max row, max col) of any non-empty cell.
    pub fn extent(&self) -> Option<CellRef> {
        let mut max_row = None::<u32>;
        let mut max_col = 0;
        for (c, col) in self.cols.iter().enumerate() {
            if let Some((&r, _)) = col.last_key_value() {
                max_row = Some(max_row.map_or(r, |m| m.max(r)));
                max_col = c as u32;
            }
        }
        max_row.map(|r| CellRef::new(r, max_col))
    }
}
