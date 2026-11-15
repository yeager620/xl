//! Columnar sparse cell storage: one ordered map per column, so range scans
//! (e.g. `SUM(A:A)`) only touch cells that actually exist.
//!
//! Each column can also hold compact segments of imported values (see
//! [`crate::bulk`]): 8 bytes per cell instead of a map entry, for sheets with
//! millions of rows. A row's cell lives in exactly one of the two places;
//! editing a compact cell moves it to the map.

use std::collections::BTreeMap;
use std::collections::btree_map;
use std::iter::Peekable;
use std::sync::Arc;

use crate::address::{CellRef, Range};
use crate::bulk::{self, Segment};
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

#[derive(Default, Clone, Debug)]
struct Column {
    sparse: BTreeMap<u32, Cell>,
    /// Compact segments, sorted by row and non-overlapping.
    bulk: Vec<Segment>,
}

impl Column {
    fn get(&self, row: u32) -> Option<&Cell> {
        if let Some(c) = self.sparse.get(&row) {
            return Some(c);
        }
        if self.bulk.is_empty() {
            return None;
        }
        let (i, o) = bulk::locate(&self.bulk, row)?;
        self.bulk[i].get(o)
    }

    /// Remove a compact cell (if any) at `row`.
    fn take_bulk(&mut self, row: u32) -> Option<Cell> {
        if self.bulk.is_empty() {
            return None;
        }
        let (i, o) = bulk::locate(&self.bulk, row)?;
        self.bulk[i].take(o)
    }

    fn range(&self, lo: u32, hi: u32, col: u32) -> ColIter<'_> {
        let segs = &self.bulk[self.bulk.partition_point(|s| s.end() <= lo)..];
        let mut it = ColIter { col, sparse: self.sparse.range(lo..=hi).peekable(), segs, offset: 0, hi, next_bulk: None };
        if let Some(s) = segs.first() {
            it.offset = lo.saturating_sub(s.start) as usize;
        }
        it.next_bulk = it.advance_bulk();
        it
    }

    fn last_row(&self) -> Option<u32> {
        let s = self.sparse.last_key_value().map(|(&r, _)| r);
        let b = bulk::last_filled(&self.bulk);
        s.max(b)
    }

    fn drain_into(self, col: u32, out: &mut Vec<(CellRef, Cell)>) {
        out.extend(self.sparse.into_iter().map(|(r, cell)| (CellRef::new(r, col), cell)));
        for s in &self.bulk {
            s.drain_into(col, out);
        }
    }
}

/// Non-empty cells of one column within rows `..=hi`, in row order: the
/// sparse map merged with the compact segments.
struct ColIter<'a> {
    col: u32,
    sparse: Peekable<btree_map::Range<'a, u32, Cell>>,
    segs: &'a [Segment],
    offset: usize,
    hi: u32,
    next_bulk: Option<(u32, &'a Cell)>,
}

impl<'a> ColIter<'a> {
    fn advance_bulk(&mut self) -> Option<(u32, &'a Cell)> {
        while let Some(s) = self.segs.first() {
            if s.start > self.hi {
                self.segs = &[];
                return None;
            }
            if let Some(o) = s.next_filled(self.offset) {
                let row = s.start + o as u32;
                if row > self.hi {
                    self.segs = &[];
                    return None;
                }
                self.offset = o + 1;
                return s.get(o).map(|c| (row, c));
            }
            self.segs = &self.segs[1..];
            self.offset = 0;
        }
        None
    }
}

impl<'a> Iterator for ColIter<'a> {
    type Item = (CellRef, &'a Cell);

    fn next(&mut self) -> Option<Self::Item> {
        let take_sparse = match (self.sparse.peek(), self.next_bulk) {
            (None, None) => return None,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(&(&s, _)), Some((b, _))) => {
                if s == b {
                    // Shouldn't happen (a row lives in one place); the map wins.
                    self.next_bulk = self.advance_bulk();
                }
                s <= b
            }
        };
        if take_sparse {
            self.sparse.next().map(|(&r, c)| (CellRef::new(r, self.col), c))
        } else {
            let (r, c) = self.next_bulk.take()?;
            self.next_bulk = self.advance_bulk();
            Some((CellRef::new(r, self.col), c))
        }
    }
}

#[derive(Default, Clone)]
pub struct CellStore {
    cols: Vec<Column>,
    len: usize,
}

impl CellStore {
    pub fn get(&self, c: CellRef) -> Option<&Cell> {
        self.cols.get(c.col as usize)?.get(c.row)
    }

    pub fn get_mut(&mut self, c: CellRef) -> Option<&mut Cell> {
        let col = self.cols.get_mut(c.col as usize)?;
        if !col.sparse.contains_key(&c.row) {
            // Editing a compact cell moves it to the map.
            let cell = col.take_bulk(c.row)?;
            col.sparse.insert(c.row, cell);
        }
        col.sparse.get_mut(&c.row)
    }

    pub fn value(&self, c: CellRef) -> &Value {
        self.get(c).map_or(&EMPTY, |cell| &cell.value)
    }

    /// The value at `c`, built without materializing compact cells (for
    /// exporters and other one-pass readers of big sheets).
    pub fn value_owned(&self, c: CellRef) -> Value {
        let Some(col) = self.cols.get(c.col as usize) else { return Value::Empty };
        if let Some(cell) = col.sparse.get(&c.row) {
            return cell.value.clone();
        }
        match bulk::locate(&col.bulk, c.row) {
            Some((i, o)) => col.bulk[i].value(o),
            None => Value::Empty,
        }
    }

    pub fn insert(&mut self, c: CellRef, cell: Cell) -> Option<Cell> {
        let col = c.col as usize;
        if self.cols.len() <= col {
            self.cols.resize_with(col + 1, Column::default);
        }
        let col = &mut self.cols[col];
        let old = col.sparse.insert(c.row, cell).or_else(|| col.take_bulk(c.row));
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    pub fn remove(&mut self, c: CellRef) -> Option<Cell> {
        let col = self.cols.get_mut(c.col as usize)?;
        let old = col.sparse.remove(&c.row).or_else(|| col.take_bulk(c.row));
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
        self.cols[first..last].iter().enumerate().flat_map(move |(i, col)| col.range(r.start.row, r.end.row, (first + i) as u32))
    }

    pub fn iter(&self) -> impl Iterator<Item = (CellRef, &Cell)> + '_ {
        self.cols.iter().enumerate().flat_map(|(c, col)| col.range(0, u32::MAX, c as u32))
    }

    /// Formula cells only, without visiting (or materializing) compact
    /// imported values, which are never formulas.
    pub fn iter_formulas(&self) -> impl Iterator<Item = (CellRef, &Cell)> + '_ {
        self.cols.iter().enumerate().flat_map(|(c, col)| col.sparse.iter().filter(|(_, cell)| cell.is_formula()).map(move |(&row, cell)| (CellRef::new(row, c as u32), cell)))
    }

    /// Move cells for inserted/deleted rows or columns. Returns the cells
    /// that were deleted.
    pub fn apply_shift(&mut self, op: crate::formula::Shift) -> Vec<(CellRef, Cell)> {
        use crate::formula::Shift;
        let mut removed = Vec::new();
        match op {
            Shift::InsertRows { .. } | Shift::DeleteRows { .. } => {
                for (c, col) in self.cols.iter_mut().enumerate() {
                    let old = std::mem::take(&mut col.sparse);
                    for (row, cell) in old {
                        match op.map_pos(row) {
                            Some(r) => {
                                col.sparse.insert(r, cell);
                            }
                            None => removed.push((CellRef::new(row, c as u32), cell)),
                        }
                    }
                    if !col.bulk.is_empty() {
                        bulk::shift_rows(&mut col.bulk, op, c as u32, &mut removed);
                    }
                }
            }
            Shift::InsertCols { at, count, .. } => {
                if (at as usize) < self.cols.len() {
                    let tail = self.cols.split_off(at as usize);
                    self.cols.resize_with(at as usize + count as usize, Column::default);
                    self.cols.extend(tail);
                    // Columns pushed past the sheet edge are lost.
                    let max = crate::address::MAX_COLS as usize;
                    if self.cols.len() > max {
                        for (i, col) in self.cols.drain(max..).enumerate() {
                            col.drain_into((max + i) as u32, &mut removed);
                        }
                    }
                }
            }
            Shift::DeleteCols { at, count, .. } => {
                let (a, b) = ((at as usize).min(self.cols.len()), ((at + count) as usize).min(self.cols.len()));
                for (i, col) in self.cols.drain(a..b).enumerate() {
                    col.drain_into((a + i) as u32, &mut removed);
                }
            }
        }
        self.len -= removed.len();
        removed
    }

    /// Mutable access to every cell that can hold a formula (for rewriting
    /// formulas in place). Compact imported values are plain values and
    /// aren't visited.
    pub fn cells_mut(&mut self) -> impl Iterator<Item = (CellRef, &mut Cell)> + '_ {
        self.cols
            .iter_mut()
            .enumerate()
            .flat_map(|(c, col)| col.sparse.iter_mut().map(move |(&row, cell)| (CellRef::new(row, c as u32), cell)))
    }

    /// Next non-empty row in `col` strictly after (or before) `row`.
    pub fn next_filled_in_col(&self, col: u32, row: u32, forward: bool) -> Option<u32> {
        let m = self.cols.get(col as usize)?;
        let s = if forward { m.sparse.range(row.saturating_add(1)..).next().map(|(&r, _)| r) } else { m.sparse.range(..row).next_back().map(|(&r, _)| r) };
        if m.bulk.is_empty() {
            return s;
        }
        let b = bulk::next_filled(&m.bulk, row, forward);
        match (s, b) {
            (Some(x), Some(y)) => Some(if forward { x.min(y) } else { x.max(y) }),
            (x, y) => x.or(y),
        }
    }

    /// Is there a non-empty value at `c`? (Doesn't materialize anything.)
    pub fn is_filled(&self, c: CellRef) -> bool {
        let Some(col) = self.cols.get(c.col as usize) else { return false };
        if let Some(cell) = col.sparse.get(&c.row) {
            return !cell.value.is_empty();
        }
        bulk::locate(&col.bulk, c.row).is_some_and(|(i, o)| !col.bulk[i].slot(o).is_empty())
    }

    /// The last row of the run of filled cells that starts at `row` in
    /// `col`, going down (or up). Skips through compact data a segment at a
    /// time, so Ctrl+Arrow over millions of rows is instant.
    pub fn run_end(&self, col: u32, row: u32, forward: bool) -> u32 {
        let Some(column) = self.cols.get(col as usize) else { return row };
        let mut r = row;
        loop {
            let next = if forward { r.checked_add(1).filter(|&n| n < crate::address::MAX_ROWS) } else { r.checked_sub(1) };
            let Some(n) = next else { return r };
            if let Some((i, o)) = bulk::locate(&column.bulk, n)
                && !column.bulk[i].slot(o).is_empty()
            {
                r = column.bulk[i].start + column.bulk[i].run_from(o, forward) as u32;
                continue;
            }
            if !self.is_filled(CellRef::new(n, col)) {
                return r;
            }
            r = n;
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
            if let Some(r) = col.last_row() {
                max_row = Some(max_row.map_or(r, |m| m.max(r)));
                max_col = c as u32;
            }
        }
        max_row.map(|r| CellRef::new(r, max_col))
    }

    // ------------------------------------------------------- compact data

    /// Put compact segments into column `col`. The rows they cover must be
    /// empty in that column (callers check with [`Self::iter_range`]).
    pub fn add_segments(&mut self, col: u32, segs: Vec<Segment>) {
        let c = col as usize;
        if self.cols.len() <= c {
            self.cols.resize_with(c + 1, Column::default);
        }
        let column = &mut self.cols[c];
        for s in segs {
            if s.filled() == 0 {
                continue;
            }
            debug_assert!(column.sparse.range(s.start..s.end()).next().is_none(), "rows must be empty");
            self.len += s.filled();
            let at = column.bulk.partition_point(|x| x.start < s.start);
            debug_assert!(at == 0 || column.bulk[at - 1].end() <= s.start, "segments overlap");
            debug_assert!(column.bulk.get(at).is_none_or(|x| x.start >= s.end()), "segments overlap");
            column.bulk.insert(at, s);
        }
    }

    /// Remove every compact cell in `r` (whole segments at a time where
    /// possible). Returns how many were removed.
    pub fn clear_compact(&mut self, r: Range) -> usize {
        let mut n = 0;
        let last = (r.end.col as usize + 1).min(self.cols.len());
        for col in self.cols.iter_mut().take(last).skip(r.start.col as usize) {
            col.bulk.retain_mut(|s| {
                if s.start > r.end.row || s.end() <= r.start.row {
                    return true;
                }
                if s.start >= r.start.row && s.end() - 1 <= r.end.row {
                    n += s.filled();
                    return false;
                }
                let lo = r.start.row.max(s.start) - s.start;
                let hi = r.end.row.min(s.end() - 1) - s.start;
                for i in lo..=hi {
                    n += usize::from(s.take(i as usize).is_some());
                }
                s.filled() > 0
            });
        }
        self.len -= n;
        n
    }

    /// Compact segments of a column (for fast one-pass readers).
    pub fn segments(&self, col: u32) -> &[Segment] {
        self.cols.get(col as usize).map_or(&[], |c| &c.bulk)
    }

    /// Cells held compactly.
    pub fn compact_len(&self) -> usize {
        self.cols.iter().flat_map(|c| &c.bulk).map(Segment::filled).sum()
    }

    /// Drop cells materialized from compact storage (memory goes back to 8
    /// bytes per cell until they're read again).
    pub fn trim_cache(&mut self) {
        for s in self.cols.iter_mut().flat_map(|c| c.bulk.iter_mut()) {
            s.trim();
        }
    }

    /// (sum of numbers, how many numbers, how many non-empty cells) in a
    /// range, reading compact cells in place (nothing is materialized).
    pub fn summarize(&self, r: Range) -> (f64, u64, u64) {
        let (mut sum, mut nums, mut count) = (0.0, 0u64, 0u64);
        let last = (r.end.col as usize + 1).min(self.cols.len());
        for col in self.cols.iter().take(last).skip(r.start.col as usize) {
            for cell in col.sparse.range(r.start.row..=r.end.row).map(|(_, c)| c) {
                match cell.value {
                    Value::Number(n) => {
                        sum += n;
                        nums += 1;
                        count += 1;
                    }
                    Value::Empty => {}
                    _ => count += 1,
                }
            }
            for s in col.bulk.iter().filter(|s| s.start <= r.end.row && s.end() > r.start.row) {
                let lo = (r.start.row.max(s.start) - s.start) as usize;
                let hi = (r.end.row.min(s.end() - 1) - s.start) as usize;
                for i in lo..=hi {
                    let slot = s.slot(i);
                    if slot.is_empty() {
                        continue;
                    }
                    count += 1;
                    if let Some(n) = slot.as_number() {
                        sum += n;
                        nums += 1;
                    }
                }
            }
        }
        (sum, nums, count)
    }

    /// Heap held by cells materialized from compact storage.
    pub fn cached_bytes(&self) -> usize {
        self.cols.iter().flat_map(|c| &c.bulk).map(|s| s.cached_blocks() * bulk::BLOCK * std::mem::size_of::<Option<Cell>>()).sum()
    }

    /// Rough heap use of the compact part: slots, string arenas (each
    /// counted once even when shared after a split) and materialized blocks.
    pub fn compact_heap_bytes(&self) -> usize {
        let mut seen = rustc_hash::FxHashSet::default();
        let mut n = 0;
        for s in self.cols.iter().flat_map(|c| &c.bulk) {
            n += s.heap_bytes() + s.cached_blocks() * bulk::BLOCK * std::mem::size_of::<Option<Cell>>();
            if seen.insert(Arc::as_ptr(s.strings())) {
                n += s.strings().heap_bytes();
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bulk::ColumnBuilder;
    use crate::formula::Shift;

    fn store_with_bulk() -> CellStore {
        let mut st = CellStore::default();
        let mut b = ColumnBuilder::new(0);
        for i in 0..1000 {
            if i % 10 == 3 {
                b.push_empty();
            } else {
                b.push_number(i as f64);
            }
        }
        st.add_segments(0, b.finish());
        st
    }

    fn plain(v: f64) -> Cell {
        Cell { input: CellInput::Value(Value::Number(v)), value: Value::Number(v) }
    }

    #[test]
    fn compact_and_sparse_merge() {
        let mut st = store_with_bulk();
        assert_eq!(st.len(), 900);
        assert_eq!(st.value(CellRef::new(5, 0)), &Value::Number(5.0));
        assert!(st.get(CellRef::new(3, 0)).is_none());
        // Edits: fill a gap, overwrite a compact cell, delete one.
        st.insert(CellRef::new(3, 0), plain(-3.0));
        assert!(st.insert(CellRef::new(4, 0), plain(-4.0)).is_some());
        assert!(st.remove(CellRef::new(5, 0)).is_some());
        assert_eq!(st.len(), 900);
        let first: Vec<(u32, Value)> = st.iter_range(Range::new(CellRef::new(0, 0), CellRef::new(7, 0))).map(|(c, cell)| (c.row, cell.value.clone())).collect();
        let n = Value::Number;
        assert_eq!(first, vec![(0, n(0.0)), (1, n(1.0)), (2, n(2.0)), (3, n(-3.0)), (4, n(-4.0)), (6, n(6.0)), (7, n(7.0))]);
        assert_eq!(st.value_owned(CellRef::new(4, 0)), n(-4.0));
        assert_eq!(st.value_owned(CellRef::new(999, 0)), n(999.0));
        assert_eq!(st.iter().count(), 900);
        assert_eq!(st.extent(), Some(CellRef::new(999, 0)));
        assert_eq!(st.next_filled_in_col(0, 2, true), Some(3));
        assert_eq!(st.next_filled_in_col(0, 4, true), Some(6));
        assert_eq!(st.next_filled_in_col(0, 6, false), Some(4));
        assert_eq!(st.run_end(0, 0, true), 4, "row 5 was removed");
        assert_eq!(st.run_end(0, 6, true), 12);
        assert_eq!(st.run_end(0, 12, false), 6);
        assert_eq!(st.run_end(0, 990, true), 992);
        assert_eq!(st.run_end(0, 994, true), 999);
        assert!(st.is_filled(CellRef::new(3, 0)) && !st.is_filled(CellRef::new(5, 0)));
        if let Some(c) = st.get_mut(CellRef::new(8, 0)) {
            c.value = n(80.0);
        }
        assert_eq!(st.value(CellRef::new(8, 0)), &n(80.0));
        assert_eq!(st.compact_len(), 897);
        let all = Range::new(CellRef::new(0, 0), CellRef::new(crate::MAX_ROWS - 1, 3));
        let slow = st.iter_range(all).fold((0.0, 0u64, 0u64), |(s, n, c), (_, cell)| match cell.value {
            Value::Number(x) => (s + x, n + 1, c + 1),
            _ => (s, n, c + 1),
        });
        assert_eq!(st.summarize(all), slow);
    }

    #[test]
    fn compact_rows_shift() {
        let mut st = store_with_bulk();
        st.insert(CellRef::new(500, 1), plain(1.0));
        st.apply_shift(Shift::InsertRows { sheet: 0, at: 100, count: 5 });
        assert_eq!(st.value(CellRef::new(105, 0)), &Value::Number(100.0));
        assert_eq!(st.value(CellRef::new(505, 1)), &Value::Number(1.0));
        let removed = st.apply_shift(Shift::DeleteRows { sheet: 0, at: 0, count: 10 });
        assert_eq!(removed.len(), 9);
        assert_eq!(st.value(CellRef::new(0, 0)), &Value::Number(10.0));
        assert_eq!(st.len(), 892);
        let removed = st.apply_shift(Shift::DeleteCols { sheet: 0, at: 0, count: 1 });
        assert_eq!(removed.len(), 891);
        assert_eq!(st.len(), 1);
    }

    #[test]
    fn trim_drops_materialized_blocks() {
        let mut st = store_with_bulk();
        let before = st.compact_heap_bytes();
        let sum: f64 = st.iter_range(Range::new(CellRef::new(0, 0), CellRef::new(crate::MAX_ROWS - 1, 0))).filter_map(|(_, c)| c.value.to_number().ok()).sum();
        assert_eq!(sum, (0..1000).filter(|i| i % 10 != 3).sum::<i32>() as f64);
        assert!(st.compact_heap_bytes() > before);
        st.trim_cache();
        assert_eq!(st.compact_heap_bytes(), before);
        assert_eq!(st.iter_formulas().count(), 0);
    }
}
