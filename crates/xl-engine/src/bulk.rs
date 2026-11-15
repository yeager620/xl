//! Compact column storage for large imported data (CSV loads, query
//! results): 8 bytes per cell, with strings packed into a shared arena.
//!
//! A [`Segment`] holds one column's values for a contiguous run of rows.
//! Each value is a NaN-boxed `u64`: a finite `f64` as is, or a tag in the
//! negative-NaN space for empty / text (an index into the segment's string
//! arena) / boolean / error. Imported cells are plain values, never
//! formulas, so this is all a cell needs.
//!
//! The cell store hands out `&Cell`, so compact cells are materialized into
//! real [`Cell`]s on demand, one block of [`BLOCK`] rows at a time, into a
//! per-segment cache. The cache only grows through `&self` and is dropped
//! by `&mut` access ([`crate::CellStore::trim_cache`] or any edit to the
//! block), so references stay valid for as long as the borrow checker says.
//! Painting a screen touches a handful of blocks; a whole-column scan
//! materializes that column until the next trim.

use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

use rustc_hash::{FxHashMap, FxHasher};

use crate::store::{Cell, CellInput};
use crate::value::{ErrorKind, Value};

/// Rows per materialized block.
pub const BLOCK: usize = 256;

/// One compact cell.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Slot(u64);

// Sign bit + all-ones exponent: -inf and negative NaNs. Stored numbers are
// always finite, so this whole space is free for tags. The tag kind lives in
// bits 48..52 and the payload in the low 48 bits; kind 0 / payload 0 is -inf
// itself, used for "empty".
const TAG: u64 = 0xFFF0_0000_0000_0000;
const PAYLOAD: u64 = (1 << 48) - 1;
const K_EMPTY: u64 = 0;
const K_TEXT: u64 = 1;
const K_BOOL: u64 = 2;
const K_ERROR: u64 = 3;

const ERRORS: [ErrorKind; 7] = [ErrorKind::Div0, ErrorKind::Value, ErrorKind::Ref, ErrorKind::Name, ErrorKind::Num, ErrorKind::NA, ErrorKind::Circ];

impl Slot {
    pub const EMPTY: Slot = Slot(TAG);

    pub fn number(n: f64) -> Slot {
        if n.is_finite() { Slot(n.to_bits()) } else { Slot::error(ErrorKind::Num) }
    }

    pub fn boolean(b: bool) -> Slot {
        Slot(TAG | K_BOOL << 48 | b as u64)
    }

    pub fn error(e: ErrorKind) -> Slot {
        Slot(TAG | K_ERROR << 48 | ERRORS.iter().position(|&x| x == e).unwrap_or(1) as u64)
    }

    fn text(index: u32) -> Slot {
        Slot(TAG | K_TEXT << 48 | index as u64)
    }

    pub fn is_empty(self) -> bool {
        self == Slot::EMPTY
    }

    fn kind(self) -> Option<u64> {
        (self.0 & TAG == TAG).then_some((self.0 >> 48) & 0xF)
    }

    /// The number, if this is one.
    pub fn as_number(self) -> Option<f64> {
        self.kind().is_none().then(|| f64::from_bits(self.0))
    }

    fn to_value(self, strings: &Strings) -> Value {
        match self.kind() {
            None => Value::Number(f64::from_bits(self.0)),
            Some(K_EMPTY) => Value::Empty,
            Some(K_TEXT) => Value::text(strings.get((self.0 & PAYLOAD) as u32)),
            Some(K_BOOL) => Value::Bool(self.0 & 1 == 1),
            Some(_) => Value::Error(ERRORS.get((self.0 & PAYLOAD) as usize).copied().unwrap_or(ErrorKind::Value)),
        }
    }
}

/// Packed strings: string `i` is `bytes[ends[i-1]..ends[i]]`.
#[derive(Default, Debug)]
pub struct Strings {
    bytes: String,
    ends: Vec<u32>,
}

impl Strings {
    fn get(&self, i: u32) -> &str {
        let i = i as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.bytes[start..self.ends[i] as usize]
    }

    /// Append; None when the arena is full (4 GiB) or holds 2^32 strings.
    fn push(&mut self, s: &str) -> Option<u32> {
        let end = u32::try_from(self.bytes.len() + s.len()).ok()?;
        let index = u32::try_from(self.ends.len()).ok()?;
        self.bytes.push_str(s);
        self.ends.push(end);
        Some(index)
    }

    pub fn heap_bytes(&self) -> usize {
        self.bytes.capacity() + self.ends.capacity() * 4
    }
}

type Block = Box<[Option<Cell>]>;

/// One column's compact values for rows `start..start + len`.
#[derive(Clone, Debug)]
pub struct Segment {
    pub(crate) start: u32,
    slots: Vec<Slot>,
    strings: Arc<Strings>,
    /// Non-empty slots.
    filled: usize,
    cache: Box<[OnceLock<Block>]>,
}

fn empty_cache(len: usize) -> Box<[OnceLock<Block>]> {
    (0..len.div_ceil(BLOCK)).map(|_| OnceLock::new()).collect()
}

fn plain(v: Value) -> Cell {
    Cell { input: CellInput::Value(v.clone()), value: v }
}

impl Segment {
    fn new(start: u32, slots: Vec<Slot>, strings: Arc<Strings>) -> Self {
        let filled = slots.iter().filter(|s| !s.is_empty()).count();
        let cache = empty_cache(slots.len());
        Segment { start, slots, strings, filled, cache }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// First row covered.
    pub fn start_row(&self) -> u32 {
        self.start
    }

    /// One past the last row covered.
    pub fn end(&self) -> u32 {
        self.start + self.slots.len() as u32
    }

    pub fn filled(&self) -> usize {
        self.filled
    }

    pub fn slot(&self, i: usize) -> Slot {
        self.slots[i]
    }

    /// The value at offset `i`, built fresh (doesn't touch the cache).
    pub fn value(&self, i: usize) -> Value {
        self.slots[i].to_value(&self.strings)
    }

    /// The text at offset `i` without allocating, if it is text.
    pub fn text(&self, i: usize) -> Option<&str> {
        let s = self.slots[i];
        (s.kind() == Some(K_TEXT)).then(|| self.strings.get((s.0 & PAYLOAD) as u32))
    }

    fn materialize(&self, block: usize) -> Block {
        let lo = block * BLOCK;
        let hi = (lo + BLOCK).min(self.slots.len());
        self.slots[lo..hi].iter().map(|s| (!s.is_empty()).then(|| plain(s.to_value(&self.strings)))).collect()
    }

    /// The cell at offset `i` (materializing its block).
    pub fn get(&self, i: usize) -> Option<&Cell> {
        if self.slots.get(i)?.is_empty() {
            return None;
        }
        let block = i / BLOCK;
        self.cache[block].get_or_init(|| self.materialize(block))[i % BLOCK].as_ref()
    }

    /// Empty offset `i`, returning what was there.
    pub fn take(&mut self, i: usize) -> Option<Cell> {
        let s = *self.slots.get(i)?;
        if s.is_empty() {
            return None;
        }
        self.slots[i] = Slot::EMPTY;
        self.filled -= 1;
        self.cache[i / BLOCK].take();
        Some(plain(s.to_value(&self.strings)))
    }

    /// Drop materialized blocks.
    pub fn trim(&mut self) {
        for c in self.cache.iter_mut() {
            c.take();
        }
    }

    /// Materialized blocks (for memory accounting and tests).
    pub fn cached_blocks(&self) -> usize {
        self.cache.iter().filter(|c| c.get().is_some()).count()
    }

    /// Split at offset `at`: self keeps `..at`, the result holds `at..`.
    pub(crate) fn split_off(&mut self, at: usize) -> Segment {
        let tail = self.slots.split_off(at);
        let tail = Segment::new(self.start + at as u32, tail, self.strings.clone());
        self.filled -= tail.filled;
        self.cache = empty_cache(self.slots.len());
        tail
    }

    /// Drop everything from offset `at` on, appending the non-empty cells to `out`.
    fn truncate_into(&mut self, at: usize, col: u32, out: &mut Vec<(crate::CellRef, Cell)>) {
        let tail = self.split_off(at);
        tail.drain_into(col, out);
    }

    pub(crate) fn drain_into(&self, col: u32, out: &mut Vec<(crate::CellRef, Cell)>) {
        for (i, s) in self.slots.iter().enumerate() {
            if !s.is_empty() {
                out.push((crate::CellRef::new(self.start + i as u32, col), plain(s.to_value(&self.strings))));
            }
        }
    }

    /// Offset of the first non-empty slot at or after `from`.
    pub fn next_filled(&self, from: usize) -> Option<usize> {
        self.slots.get(from..)?.iter().position(|s| !s.is_empty()).map(|p| p + from)
    }

    /// Last offset of the run of non-empty slots that starts at `o` (going
    /// forward or backward).
    pub fn run_from(&self, o: usize, forward: bool) -> usize {
        if forward {
            self.slots[o..].iter().position(|s| s.is_empty()).map_or(self.slots.len() - 1, |p| o + p - 1)
        } else {
            self.slots[..=o].iter().rposition(|s| s.is_empty()).map_or(0, |p| p + 1)
        }
    }

    /// Offset of the last non-empty slot strictly before `before`.
    pub fn prev_filled(&self, before: usize) -> Option<usize> {
        self.slots[..before.min(self.slots.len())].iter().rposition(|s| !s.is_empty())
    }

    pub fn heap_bytes(&self) -> usize {
        self.slots.capacity() * 8 + self.cache.len() * std::mem::size_of::<OnceLock<Block>>()
    }

    pub fn strings(&self) -> &Arc<Strings> {
        &self.strings
    }
}

// ------------------------------------------------------------ column ops

/// Segment and offset holding `row`.
pub(crate) fn locate(segs: &[Segment], row: u32) -> Option<(usize, usize)> {
    let i = segs.partition_point(|s| s.end() <= row);
    let s = segs.get(i)?;
    (s.start <= row).then(|| (i, (row - s.start) as usize))
}

/// Last non-empty row in a column's segments.
pub(crate) fn last_filled(segs: &[Segment]) -> Option<u32> {
    segs.iter().rev().find_map(|s| s.prev_filled(s.len()).map(|i| s.start + i as u32))
}

/// Next non-empty row after (forward) or before `row`.
pub(crate) fn next_filled(segs: &[Segment], row: u32, forward: bool) -> Option<u32> {
    if forward {
        let from = row.checked_add(1)?;
        let i = segs.partition_point(|s| s.end() <= from);
        segs[i..].iter().find_map(|s| s.next_filled(from.saturating_sub(s.start) as usize).map(|o| s.start + o as u32))
    } else {
        let i = segs.partition_point(|s| s.start < row);
        segs[..i].iter().rev().find_map(|s| s.prev_filled((row - s.start) as usize).map(|o| s.start + o as u32))
    }
}

/// Insert or delete rows in one column's segments. Cells deleted (or pushed
/// past the last row) go to `removed`.
pub(crate) fn shift_rows(segs: &mut Vec<Segment>, op: crate::formula::Shift, col: u32, removed: &mut Vec<(crate::CellRef, Cell)>) {
    use crate::formula::Shift;
    let old = std::mem::take(segs);
    match op {
        Shift::InsertRows { at, count, .. } => {
            for mut s in old {
                if s.end() <= at {
                    segs.push(s);
                } else if s.start >= at {
                    s.start += count;
                    segs.push(s);
                } else {
                    let mut tail = s.split_off((at - s.start) as usize);
                    tail.start = at + count;
                    segs.push(s);
                    segs.push(tail);
                }
            }
            let max = crate::address::MAX_ROWS;
            segs.retain_mut(|s| {
                if s.start >= max {
                    s.drain_into(col, removed);
                    false
                } else {
                    if s.end() > max {
                        s.truncate_into((max - s.start) as usize, col, removed);
                    }
                    true
                }
            });
        }
        Shift::DeleteRows { at, count, .. } => {
            let end = at.saturating_add(count);
            for mut s in old {
                if s.end() <= at {
                    segs.push(s);
                } else if s.start >= end {
                    s.start -= count;
                    segs.push(s);
                } else {
                    let suffix = (s.end() > end).then(|| s.split_off((end - s.start) as usize));
                    let middle = s.split_off((s.start.max(at) - s.start) as usize);
                    middle.drain_into(col, removed);
                    if !s.is_empty() {
                        segs.push(s);
                    }
                    if let Some(mut x) = suffix {
                        x.start = at;
                        segs.push(x);
                    }
                }
            }
        }
        _ => *segs = old,
    }
    segs.retain(|s| s.filled > 0);
}

/// Move segments down by `by` rows (for columns built relative to a
/// chunk's first row).
pub fn offset_segments(segs: &mut [Segment], by: u32) {
    for s in segs {
        s.start += by;
    }
}

/// Drop everything at or past row `end`.
pub fn clip_segments(segs: &mut Vec<Segment>, end: u32) {
    let mut dropped = Vec::new();
    segs.retain_mut(|s| {
        if s.start >= end {
            return false;
        }
        if s.end() > end {
            s.truncate_into((end - s.start) as usize, 0, &mut dropped);
            dropped.clear();
        }
        s.filled > 0
    });
}

// ------------------------------------------------------------ building

/// Builds one column's segments from values pushed row by row.
pub struct ColumnBuilder {
    start: u32,
    slots: Vec<Slot>,
    strings: Strings,
    /// Hash of a string -> its index, to share repeated strings. Dropped
    /// (interning stops) once a column turns out to be mostly unique.
    intern: Option<FxHashMap<u64, u32>>,
    done: Vec<Segment>,
}

const INTERN_LIMIT: usize = 4096;

impl ColumnBuilder {
    /// A column whose first pushed value lands on row `start`.
    pub fn new(start: u32) -> Self {
        ColumnBuilder { start, slots: Vec::new(), strings: Strings::default(), intern: Some(FxHashMap::default()), done: Vec::new() }
    }

    /// Row the next pushed value lands on.
    pub fn next_row(&self) -> u32 {
        self.start + self.slots.len() as u32
    }

    pub fn reserve(&mut self, n: usize) {
        self.slots.reserve(n);
    }

    pub fn push_slot(&mut self, s: Slot) {
        self.slots.push(s);
    }

    pub fn push_empty(&mut self) {
        self.slots.push(Slot::EMPTY);
    }

    /// Pad with empties up to (not including) `row`.
    pub fn pad_to(&mut self, row: u32) {
        let n = row.saturating_sub(self.next_row()) as usize;
        self.slots.resize(self.slots.len() + n, Slot::EMPTY);
    }

    pub fn push_number(&mut self, n: f64) {
        self.slots.push(Slot::number(n));
    }

    pub fn push_text(&mut self, s: &str) {
        if s.is_empty() {
            return self.push_empty();
        }
        let hash = self.intern.as_ref().map(|_| {
            let mut h = FxHasher::default();
            s.hash(&mut h);
            h.finish()
        });
        if let (Some(map), Some(h)) = (&self.intern, hash)
            && let Some(&i) = map.get(&h)
            && self.strings.get(i) == s
        {
            self.slots.push(Slot::text(i));
            return;
        }
        let index = match self.strings.push(s) {
            Some(i) => i,
            None => {
                // Arena full: close this segment and start a new one here.
                self.cut();
                self.strings.push(s).expect("a string under 4 GiB")
            }
        };
        if let (Some(map), Some(h)) = (&mut self.intern, hash) {
            map.entry(h).or_insert(index);
            if map.len() > INTERN_LIMIT {
                self.intern = None;
            }
        }
        self.slots.push(Slot::text(index));
    }

    pub fn push_value(&mut self, v: &Value) {
        match v {
            Value::Empty => self.push_empty(),
            Value::Number(n) => self.push_number(*n),
            Value::Text(t) => self.push_text(t),
            Value::Bool(b) => self.slots.push(Slot::boolean(*b)),
            Value::Error(e) => self.slots.push(Slot::error(*e)),
        }
    }

    fn cut(&mut self) {
        let start = self.next_row();
        let slots = std::mem::take(&mut self.slots);
        let strings = std::mem::take(&mut self.strings);
        self.done.push(Segment::new(self.start, slots, Arc::new(strings)));
        self.start = start;
        if let Some(m) = &mut self.intern {
            m.clear();
        }
    }

    /// The finished segments, with leading/trailing empties trimmed and
    /// empty segments dropped.
    pub fn finish(mut self) -> Vec<Segment> {
        self.cut();
        let mut out = Vec::with_capacity(self.done.len());
        for mut s in self.done {
            let Some(last) = s.prev_filled(s.len()) else { continue };
            s.slots.truncate(last + 1);
            let first = s.next_filled(0).unwrap_or(0);
            if first > 0 {
                s = s.split_off(first);
            }
            s.slots.shrink_to_fit();
            s.cache = empty_cache(s.slots.len());
            out.push(s);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formula::Shift;

    #[test]
    fn slots_round_trip() {
        let mut b = ColumnBuilder::new(0);
        b.push_number(-0.0);
        b.push_number(1e300);
        b.push_number(f64::NAN);
        b.push_text("hi");
        b.push_value(&Value::Bool(true));
        b.push_value(&Value::Error(ErrorKind::NA));
        b.push_empty();
        b.push_text("hi");
        let segs = b.finish();
        assert_eq!(segs.len(), 1);
        let s = &segs[0];
        assert_eq!(s.len(), 8);
        assert_eq!(s.value(0), Value::Number(-0.0));
        assert_eq!(s.value(1), Value::Number(1e300));
        assert_eq!(s.value(2), Value::Error(ErrorKind::Num));
        assert_eq!(s.value(3), Value::text("hi"));
        assert_eq!(s.value(4), Value::Bool(true));
        assert_eq!(s.value(5), Value::Error(ErrorKind::NA));
        assert_eq!(s.value(6), Value::Empty);
        assert!(s.get(6).is_none());
        assert_eq!(s.get(7).unwrap().value, Value::text("hi"));
        assert_eq!(s.strings.ends.len(), 1, "repeated strings are shared");
        assert_eq!(s.filled(), 7);
    }

    #[test]
    fn finish_trims_and_splits() {
        let mut b = ColumnBuilder::new(10);
        b.push_empty();
        b.push_empty();
        b.push_number(1.0);
        b.push_empty();
        let segs = b.finish();
        assert_eq!((segs[0].start, segs[0].len()), (12, 1));
    }

    fn column(start: u32, n: u32) -> Vec<Segment> {
        let mut b = ColumnBuilder::new(start);
        for i in 0..n {
            b.push_number(i as f64);
        }
        b.finish()
    }

    #[test]
    fn shifting_rows() {
        let mut segs = column(0, 10);
        let mut removed = Vec::new();
        shift_rows(&mut segs, Shift::InsertRows { sheet: 0, at: 4, count: 3 }, 0, &mut removed);
        assert!(removed.is_empty());
        assert_eq!(locate(&segs, 3).map(|(i, o)| segs[i].value(o)), Some(Value::Number(3.0)));
        assert_eq!(locate(&segs, 5), None);
        assert_eq!(locate(&segs, 7).map(|(i, o)| segs[i].value(o)), Some(Value::Number(4.0)));
        shift_rows(&mut segs, Shift::DeleteRows { sheet: 0, at: 2, count: 6 }, 0, &mut removed);
        let got: Vec<_> = removed.iter().map(|(c, cell)| (c.row, cell.value.clone())).collect();
        assert_eq!(got, vec![(2, Value::Number(2.0)), (3, Value::Number(3.0)), (7, Value::Number(4.0))]);
        let vals: Vec<_> = (0..8).map(|r| locate(&segs, r).map(|(i, o)| segs[i].value(o))).collect();
        assert_eq!(vals[..3], [Some(Value::Number(0.0)), Some(Value::Number(1.0)), Some(Value::Number(5.0))]);
        assert_eq!(last_filled(&segs), Some(6));
        assert_eq!(next_filled(&segs, 0, true), Some(1));
        assert_eq!(next_filled(&segs, 6, false), Some(5));
        assert_eq!(next_filled(&segs, 6, true), None);
    }

    #[test]
    fn cache_materializes_by_block() {
        let segs = column(0, 10 * BLOCK as u32);
        let s = &segs[0];
        assert_eq!(s.cached_blocks(), 0);
        assert_eq!(s.get(BLOCK * 3 + 5).unwrap().value, Value::Number((BLOCK * 3 + 5) as f64));
        assert_eq!(s.cached_blocks(), 1);
        let mut s = s.clone();
        s.trim();
        assert_eq!(s.cached_blocks(), 0);
        assert_eq!(s.take(7).unwrap().value, Value::Number(7.0));
        assert!(s.get(7).is_none());
        assert_eq!(s.filled(), 10 * BLOCK - 1);
    }
}
