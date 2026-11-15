//! Workbook entry points for big data: placing compact imported columns,
//! setting literal values without re-parsing them, number formats for many
//! cells at once, and memory housekeeping for compact storage.

use rustc_hash::FxHashSet;

use super::{RecalcStats, Workbook};
use crate::address::{Addr, CellRef, Range, SheetId};
use crate::bulk::Segment;
use crate::store::CellInput;
use crate::value::Value;

impl Workbook {
    /// Put compact columns (from [`crate::bulk::ColumnBuilder`]) on a sheet:
    /// `cols[i]` goes to column `first_col + i`, at the rows its segments
    /// were built for. The cells they cover must be empty. Returns the
    /// number of cells placed; formulas that read them are recalculated.
    pub fn load_columns(&mut self, sheet: SheetId, first_col: u32, cols: Vec<Vec<Segment>>) -> Result<(usize, RecalcStats), String> {
        let sd = self.sheets.get(sheet).ok_or("No such sheet")?;
        for (i, segs) in cols.iter().enumerate() {
            let c = first_col + i as u32;
            if c >= crate::address::MAX_COLS {
                return Err("Too many columns".into());
            }
            for s in segs {
                let r = Range::new(CellRef::new(s.start, c), CellRef::new(s.end() - 1, c));
                if sd.store.iter_range(r).next().is_some() {
                    return Err(format!("{r} isn't empty"));
                }
            }
        }
        let sd = self.sheets.get_mut(sheet).ok_or("No such sheet")?;
        let before = sd.store.len();
        for (i, segs) in cols.into_iter().enumerate() {
            sd.store.add_segments(first_col + i as u32, segs);
        }
        let placed = sd.store.len() - before;
        Ok((placed, self.recalc_readers_of(sheet)))
    }

    /// Recalculate the formulas that read anything on `sheet` (after bulk
    /// changes there that don't go through per-cell seeds).
    fn recalc_readers_of(&mut self, sheet: SheetId) -> RecalcStats {
        let mut seeds = Vec::new();
        let mut forced = FxHashSet::default();
        for (at, p) in &self.graph.precedents {
            if p.cells.iter().any(|a| a.sheet == sheet) || p.ranges.iter().any(|(s, _)| *s == sheet) {
                seeds.push(*at);
                forced.insert(*at);
            }
        }
        if seeds.is_empty() {
            return RecalcStats::default();
        }
        self.propagate(seeds, forced)
    }

    /// Set cells to literal values (no parsing: text stays text, however it
    /// looks) with a single recalc pass. `Value::Empty` clears the cell.
    pub fn set_values(&mut self, items: impl IntoIterator<Item = (Addr, Value)>) -> RecalcStats {
        let mut seeds = Vec::new();
        let mut forced = FxHashSet::default();
        let mut count = 0usize;
        for (a, v) in items {
            if self.sheets.get(a.sheet).is_none() {
                continue;
            }
            let input = (!v.is_empty()).then_some(CellInput::Value(v));
            self.apply(a, input, &mut seeds, &mut forced);
            count += 1;
        }
        if count >= 10_000 && count * 4 >= self.graph.precedents.len() {
            return self.recalc_all();
        }
        self.propagate(seeds, forced)
    }

    /// Give many cells the same number format (None clears it).
    pub fn set_format_cells(&mut self, sheet: SheetId, code: Option<&str>, cells: impl IntoIterator<Item = CellRef>) {
        let id = code.filter(|c| !c.is_empty() && !c.eq_ignore_ascii_case("General")).map(|c| self.formats.intern(c));
        let Some(s) = self.sheets.get_mut(sheet) else { return };
        let cells = cells.into_iter();
        match id {
            Some(id) => {
                s.formats.reserve(cells.size_hint().0);
                for c in cells {
                    s.formats.insert(c, id);
                }
            }
            None => {
                for c in cells {
                    s.formats.remove(&c);
                }
            }
        }
    }

    /// Clear every cell in a region (contents only; formats stay), dropping
    /// compact data wholesale. Dependent formulas are recalculated.
    pub fn clear_region(&mut self, sheet: SheetId, range: Range) -> RecalcStats {
        let Some(sd) = self.sheets.get_mut(sheet) else { return RecalcStats::default() };
        // Spills reaching into the region, or blocked by cells in it, are
        // re-run afterwards: dropping compact data skips the per-cell path
        // that would otherwise tell them.
        let meets = |r: &Range| r.start.row <= range.end.row && range.start.row <= r.end.row && r.start.col <= range.end.col && range.start.col <= r.end.col;
        let anchors: Vec<Addr> = sd.spills.iter().chain(sd.blocked.iter()).filter(|(_, r)| meets(r)).map(|(a, _)| Addr::new(sheet, *a)).collect();
        let dropped = sd.store.clear_compact(range);
        let rest: Vec<Addr> = sd.store.iter_range(range).map(|(c, _)| Addr::new(sheet, c)).collect();
        let stats = self.set_values(rest.into_iter().map(|a| (a, Value::Empty)));
        let stats = if dropped > 0 { self.recalc_readers_of(sheet) } else { stats };
        if anchors.is_empty() {
            return stats;
        }
        let forced: FxHashSet<Addr> = anchors.iter().copied().collect();
        self.propagate(anchors, forced)
    }

    /// Drop cells materialized from compact storage on every sheet.
    pub fn trim_caches(&mut self) {
        for id in self.sheets.order.clone() {
            if let Some(sd) = self.sheets.get_mut(id) {
                sd.store.trim_cache();
            }
        }
    }

    /// Heap held by cells materialized from compact storage (dropped by
    /// [`Self::trim_caches`]).
    pub fn compact_cache_bytes(&self) -> usize {
        self.sheets.order.iter().filter_map(|&id| self.sheets.store(id)).map(|s| s.cached_bytes()).sum()
    }

    /// Rough heap use of compact storage across sheets.
    pub fn compact_heap_bytes(&self) -> usize {
        self.sheets.order.iter().filter_map(|&id| self.sheets.store(id)).map(|s| s.compact_heap_bytes()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bulk::ColumnBuilder;

    #[test]
    fn dynamic_arrays_over_compact_data() {
        let mut wb = Workbook::new();
        let s = wb.sheet_ids()[0];
        let at = |a1: &str| Addr::new(s, CellRef::parse(a1).unwrap());
        let (mut a, mut b) = (ColumnBuilder::new(0), ColumnBuilder::new(0));
        for i in 0..50_000 {
            a.push_number(i as f64);
            b.push_text(["red", "green", "blue"][i % 3]);
        }
        wb.load_columns(s, 0, vec![a.finish(), b.finish()]).unwrap();
        // Spilling formulas read compact columns.
        wb.set_input(at("D1"), "=UNIQUE(B1:B50000)").unwrap();
        assert_eq!(wb.value(at("D1")), &Value::text("red"));
        assert_eq!(wb.value(at("D3")), &Value::text("blue"));
        assert_eq!(wb.value(at("D4")), &Value::Empty);
        wb.set_input(at("E1"), "=ROWS(FILTER(A1:A50000, A1:A50000>=49990))").unwrap();
        assert_eq!(wb.value(at("E1")), &Value::Number(10.0));
        wb.set_input(at("F1"), "=SUM(--(D1#=\"red\"))+COUNTA(D1#)").unwrap();
        assert_eq!(wb.value(at("F1")), &Value::Number(4.0));
        // A spill aimed at compact cells is blocked; clearing them frees it.
        let mut h = ColumnBuilder::new(0);
        for i in 0..100 {
            h.push_number(i as f64);
        }
        wb.load_columns(s, 7, vec![h.finish()]).unwrap();
        wb.set_input(at("G10"), "=SEQUENCE(1, 3)").unwrap();
        assert_eq!(wb.value(at("G10")), &Value::Error(crate::value::ErrorKind::Spill));
        wb.clear_region(s, Range::new(CellRef::new(0, 7), CellRef::new(99, 7)));
        assert_eq!(wb.value(at("I10")), &Value::Number(3.0), "freed spill fills in");
    }

    #[test]
    fn compact_columns_feed_formulas() {
        let mut wb = Workbook::new();
        let s = wb.sheet_ids()[0];
        wb.set_input(Addr::new(s, CellRef::new(0, 3)), "=SUM(A:A)").unwrap();
        wb.set_input(Addr::new(s, CellRef::new(1, 3)), "=COUNTA(B:B)").unwrap();
        let mut a = ColumnBuilder::new(0);
        let mut b = ColumnBuilder::new(0);
        for i in 0..100_000 {
            a.push_number(i as f64);
            b.push_text(if i % 2 == 0 { "even" } else { "odd" });
        }
        let (placed, _) = wb.load_columns(s, 0, vec![a.finish(), b.finish()]).unwrap();
        assert_eq!(placed, 200_000);
        assert_eq!(wb.value(Addr::new(s, CellRef::new(0, 3))), &Value::Number(4_999_950_000.0));
        assert_eq!(wb.value(Addr::new(s, CellRef::new(1, 3))), &Value::Number(100_000.0));
        // Editing a compact cell recalculates dependents.
        wb.set_input(Addr::new(s, CellRef::new(10, 0)), "1010").unwrap();
        assert_eq!(wb.value(Addr::new(s, CellRef::new(0, 3))), &Value::Number(4_999_951_000.0));
        // Loading over existing cells is refused.
        let mut c = ColumnBuilder::new(5);
        c.push_number(1.0);
        assert!(wb.load_columns(s, 0, vec![c.finish()]).is_err());
        wb.trim_caches();
        assert!(wb.compact_heap_bytes() > 0);
        // Clearing drops compact data and recalculates.
        wb.clear_region(s, Range::new(CellRef::new(0, 0), CellRef::new(49_999, 1)));
        assert_eq!(wb.value(Addr::new(s, CellRef::new(1, 3))), &Value::Number(50_000.0));
        assert_eq!(wb.value(Addr::new(s, CellRef::new(0, 3))), &Value::Number((50_000..100_000u64).sum::<u64>() as f64));
    }

    #[test]
    fn set_values_keeps_text_as_text() {
        let mut wb = Workbook::new();
        let s = wb.sheet_ids()[0];
        wb.set_input(Addr::new(s, CellRef::new(0, 1)), "=A1&\"!\"").unwrap();
        wb.set_values([(Addr::new(s, CellRef::new(0, 0)), Value::text("007"))]);
        assert_eq!(wb.value(Addr::new(s, CellRef::new(0, 1))), &Value::text("007!"));
        wb.set_values([(Addr::new(s, CellRef::new(0, 0)), Value::Empty)]);
        assert_eq!(wb.value(Addr::new(s, CellRef::new(0, 1))), &Value::text("!"));
    }
}
