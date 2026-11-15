//! Comparing two workbooks: sheets added, removed or renamed; rows and
//! columns inserted or deleted (aligned by content, see [`crate::align`]);
//! cells changed (content, computed value, number format, style); and
//! layout changes (widths, merges, charts, notes, validations, names…).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};

use xl_engine::formula::Expr;
use xl_engine::{Addr, CellInput, CellRef, Range, SheetId, SheetLayout, Shift, Style, Value, Workbook, col_name};

use crate::align::{Step, align, maps};

/// What a cell holds, for comparing and showing.
#[derive(Clone, Debug, PartialEq)]
pub struct CellInfo {
    /// Formula text with the leading '=', if the cell has a formula.
    pub formula: Option<String>,
    /// The constant, or the formula's computed result.
    pub value: Value,
    pub format: Option<String>,
    pub style: Style,
    /// The formula uses a volatile function (its result changes on its own).
    pub volatile: bool,
}

impl CellInfo {
    pub fn has_content(&self) -> bool {
        self.formula.is_some() || self.value != Value::Empty
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellKind {
    Added,
    Removed,
    Modified,
}

/// One changed cell.
#[derive(Clone, Debug, PartialEq)]
pub struct CellChange {
    pub kind: CellKind,
    /// Where the cell is in the old workbook (None: in an inserted line).
    pub old_at: Option<CellRef>,
    /// Where the cell is (or would be) in the new workbook (None: in a deleted line).
    pub new_at: Option<CellRef>,
    pub old: Option<CellInfo>,
    pub new: Option<CellInfo>,
    /// The formula or constant changed.
    pub content: bool,
    /// The value changed (for an unchanged formula: its result was recomputed).
    pub value: bool,
    pub format: bool,
    pub style: bool,
    /// Part of an inserted or deleted row/column (or sheet).
    pub in_line: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineAxis {
    Row,
    Col,
}

/// A run of inserted or deleted rows or columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineChange {
    pub axis: LineAxis,
    pub inserted: bool,
    /// First line: a new index if inserted, an old index if deleted.
    pub start: u32,
    pub count: u32,
    /// Where in the new sheet the run is (deleted lines: the line now after them).
    pub at_new: u32,
}

/// A change to a sheet's layout or objects, described in words.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutChange {
    pub text: String,
    /// A cell in the new sheet to go to, if it's about one.
    pub at: Option<CellRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SheetStatus {
    Added,
    Removed,
    /// In both (renamed if the names differ).
    Matched,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SheetDiff {
    pub status: SheetStatus,
    pub old_name: Option<String>,
    pub new_name: Option<String>,
    pub lines: Vec<LineChange>,
    pub cells: Vec<CellChange>,
    pub layout: Vec<LayoutChange>,
}

impl SheetDiff {
    pub fn renamed(&self) -> bool {
        self.status == SheetStatus::Matched && self.old_name != self.new_name
    }

    pub fn is_empty(&self) -> bool {
        self.status == SheetStatus::Matched && !self.renamed() && self.lines.is_empty() && self.cells.is_empty() && self.layout.is_empty()
    }

    /// The sheet's name in the new workbook, else the old one.
    pub fn name(&self) -> &str {
        self.new_name.as_deref().or(self.old_name.as_deref()).unwrap_or("")
    }
}

/// A defined name added, removed or changed.
#[derive(Clone, Debug, PartialEq)]
pub struct NameChange {
    /// `Name`, or `Sheet!Name` for sheet-scoped names.
    pub name: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorkbookDiff {
    /// Every sheet (unchanged ones too), in the new workbook's order, then
    /// removed sheets in their old order.
    pub sheets: Vec<SheetDiff>,
    pub names: Vec<NameChange>,
    /// The sheets present in both are in a different order.
    pub sheet_order_changed: bool,
}

impl WorkbookDiff {
    pub fn is_empty(&self) -> bool {
        self.sheets.iter().all(SheetDiff::is_empty) && self.names.is_empty() && !self.sheet_order_changed
    }

    /// Number of individual changes (cells, lines, layout items, sheets, names).
    pub fn change_count(&self) -> usize {
        let sheets: usize = self
            .sheets
            .iter()
            .map(|s| match s.status {
                SheetStatus::Added | SheetStatus::Removed => 1,
                SheetStatus::Matched => usize::from(s.renamed()) + s.lines.len() + s.cells.iter().filter(|c| !c.in_line).count() + s.layout.len(),
            })
            .sum();
        sheets + self.names.len() + usize::from(self.sheet_order_changed)
    }
}

// ------------------------------------------------------------------ grids

fn hash_of(v: impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

fn hash_value(v: &Value, h: &mut DefaultHasher) {
    match v {
        Value::Empty => 0u8.hash(h),
        Value::Number(n) => {
            1u8.hash(h);
            n.to_bits().hash(h);
        }
        Value::Text(t) => {
            2u8.hash(h);
            t.hash(h);
        }
        Value::Bool(b) => {
            3u8.hash(h);
            b.hash(h);
        }
        Value::Error(e) => {
            4u8.hash(h);
            e.hash(h);
        }
    }
}

/// A formula with its numbers blanked out, so `=A4*2` and `=A5*2` (the same
/// formula one row down) look alike when aligning rows.
fn formula_shape(f: &str) -> String {
    let mut out = String::with_capacity(f.len());
    let mut last_digit = false;
    for ch in f.chars() {
        if ch.is_ascii_digit() {
            if !last_digit {
                out.push('#');
            }
            last_digit = true;
        } else {
            out.push(ch);
            last_digit = false;
        }
    }
    out
}

/// What a cell's content looks like for alignment (None if it has none).
fn content_key(info: &CellInfo) -> Option<u64> {
    let mut h = DefaultHasher::new();
    if let Some(f) = &info.formula {
        b'f'.hash(&mut h);
        formula_shape(f).hash(&mut h);
    } else if info.value != Value::Empty {
        b'v'.hash(&mut h);
        hash_value(&info.value, &mut h);
    } else {
        return None;
    }
    Some(h.finish())
}

pub(crate) fn cell_info(wb: &Workbook, a: Addr) -> CellInfo {
    let (formula, volatile) = match wb.cell(a).map(|c| &c.input) {
        Some(CellInput::Formula(e)) => (wb.formula_source(a).map(|s| format!("={s}")), e.is_volatile()),
        Some(CellInput::Opaque(s)) => (Some(format!("={s}")), false),
        _ => (None, false),
    };
    CellInfo { formula, value: wb.value(a).clone(), format: wb.format_code(a).map(str::to_string), style: wb.style(a).clone(), volatile }
}

/// One sheet's cells, indexed for alignment.
struct Grid {
    sheet: SheetId,
    cells: HashMap<CellRef, CellInfo>,
    rows: usize,
    cols: usize,
    /// Per row of the content's extent, (column, content key) of the cells
    /// with content, by column.
    by_row: Vec<Vec<(u32, u64)>>,
    /// Per column, (row, content key), by row.
    by_col: Vec<Vec<(u32, u64)>>,
}

impl Grid {
    fn new(wb: &Workbook, sheet: SheetId) -> Grid {
        let mut refs: Vec<CellRef> = wb.store(sheet).iter().map(|(c, _)| c).collect();
        refs.extend(wb.styled_cells(sheet).filter(|(_, s)| !s.is_default()).map(|(c, _)| c));
        refs.extend(wb.formatted_cells(sheet).map(|(c, _)| c));
        let mut cells = HashMap::with_capacity(refs.len());
        for c in refs {
            cells.entry(c).or_insert_with(|| cell_info(wb, Addr::new(sheet, c)));
        }
        cells.retain(|_, i: &mut CellInfo| i.has_content() || i.format.is_some() || !i.style.is_default());
        // Lines are aligned within the extent of the content; formatting
        // past it follows the overall shift.
        let content = || cells.iter().filter(|(_, i)| i.has_content()).map(|(c, _)| *c);
        let rows = content().map(|c| c.row as usize + 1).max().unwrap_or(0);
        let cols = content().map(|c| c.col as usize + 1).max().unwrap_or(0);
        let mut by_row = vec![Vec::new(); rows];
        let mut by_col = vec![Vec::new(); cols];
        for (c, info) in &cells {
            if let Some(k) = content_key(info) {
                by_row[c.row as usize].push((c.col, k));
                by_col[c.col as usize].push((c.row, k));
            }
        }
        by_row.iter_mut().for_each(|v| v.sort_unstable());
        by_col.iter_mut().for_each(|v| v.sort_unstable());
        Grid { sheet, cells, rows, cols, by_row, by_col }
    }

    fn content_keys(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.by_row.iter().flatten().map(|&(_, k)| k).collect();
        v.sort_unstable();
        v
    }
}

/// Size of the intersection of two sorted multisets over the larger one.
fn overlap(a: &[u64], b: &[u64]) -> f64 {
    let (mut i, mut j, mut same) = (0, 0, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                same += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    let n = a.len().max(b.len());
    if n == 0 { 1.0 } else { same as f64 / n as f64 }
}

/// Old line -> new line (and back), extended past the end of the cells by
/// the overall shift.
#[derive(Clone, Debug)]
struct LineMap {
    fwd: Vec<Option<u32>>,
    back: Vec<Option<u32>>,
}

impl LineMap {
    fn new(steps: &[Step], old_len: usize, new_len: usize) -> Self {
        let (fwd, back) = maps(steps, old_len, new_len);
        LineMap { fwd, back }
    }

    fn delta(&self) -> i64 {
        self.back.len() as i64 - self.fwd.len() as i64
    }

    fn to_new(&self, i: u32) -> Option<u32> {
        match self.fwd.get(i as usize) {
            Some(m) => *m,
            None => u32::try_from(i as i64 + self.delta()).ok(),
        }
    }

    fn to_old(&self, i: u32) -> Option<u32> {
        match self.back.get(i as usize) {
            Some(m) => *m,
            None => u32::try_from(i as i64 - self.delta()).ok(),
        }
    }
}

/// Two versions of one sheet, aligned.
struct Aligned {
    old: Grid,
    new: Grid,
    row_steps: Vec<Step>,
    col_steps: Vec<Step>,
    rows: LineMap,
    cols: LineMap,
}

impl Aligned {
    fn new(old: Grid, new: Grid) -> Self {
        // Columns first, by their content in row order (ignoring row numbers,
        // so inserted rows disturb them as little as possible).
        let col_keys = |g: &Grid| -> (Vec<u64>, Vec<Vec<u64>>) {
            let hashes = g.by_col.iter().map(|v| hash_of(v.iter().map(|&(_, k)| k).collect::<Vec<_>>())).collect();
            let sets = g
                .by_col
                .iter()
                .map(|v| {
                    let mut s: Vec<u64> = v.iter().map(|&(_, k)| k).collect();
                    s.sort_unstable();
                    s
                })
                .collect();
            (hashes, sets)
        };
        let (oh, os) = col_keys(&old);
        let (nh, ns) = col_keys(&new);
        let col_steps = align(&oh, &nh, &|i, j| overlap(&os[i], &ns[j]));
        let cols = LineMap::new(&col_steps, old.cols, new.cols);

        // Rows by their content in the paired columns.
        let mut pair_of_old = vec![None; old.cols];
        let mut pair_of_new = vec![None; new.cols];
        for (k, s) in col_steps.iter().filter_map(|s| if let Step::Both(i, j) = s { Some((*i, *j)) } else { None }).enumerate() {
            pair_of_old[s.0] = Some(k as u32);
            pair_of_new[s.1] = Some(k as u32);
        }
        let row_keys = |g: &Grid, pair: &[Option<u32>]| -> Vec<Vec<(u32, u64)>> {
            g.by_row
                .iter()
                .map(|cells| {
                    let mut v: Vec<(u32, u64)> = cells.iter().filter_map(|&(c, k)| pair[c as usize].map(|p| (p, k))).collect();
                    v.sort_unstable();
                    v
                })
                .collect()
        };
        let ok = row_keys(&old, &pair_of_old);
        let nk = row_keys(&new, &pair_of_new);
        let oh: Vec<u64> = ok.iter().map(hash_of).collect();
        let nh: Vec<u64> = nk.iter().map(hash_of).collect();
        let row_sim = |i: usize, j: usize| {
            let (a, b) = (&ok[i], &nk[j]);
            let n = a.len().max(b.len());
            if n == 0 {
                return 0.0;
            }
            let (mut x, mut y, mut same) = (0, 0, 0usize);
            while x < a.len() && y < b.len() {
                match a[x].0.cmp(&b[y].0) {
                    std::cmp::Ordering::Equal => {
                        same += usize::from(a[x].1 == b[y].1);
                        x += 1;
                        y += 1;
                    }
                    std::cmp::Ordering::Less => x += 1,
                    std::cmp::Ordering::Greater => y += 1,
                }
            }
            same as f64 / n as f64
        };
        let row_steps = align(&oh, &nh, &row_sim);
        let rows = LineMap::new(&row_steps, old.rows, new.rows);
        Aligned { old, new, row_steps, col_steps, rows, cols }
    }

    fn map_cell(&self, c: CellRef) -> Option<CellRef> {
        Some(CellRef::new(self.rows.to_new(c.row)?, self.cols.to_new(c.col)?))
    }

    fn unmap_cell(&self, c: CellRef) -> Option<CellRef> {
        Some(CellRef::new(self.rows.to_old(c.row)?, self.cols.to_old(c.col)?))
    }

    fn map_range(&self, r: Range) -> Option<Range> {
        Some(Range::new(self.map_cell(r.start)?, self.map_cell(r.end)?))
    }

    /// Runs of inserted / deleted lines.
    fn line_changes(steps: &[Step], axis: LineAxis) -> Vec<LineChange> {
        let mut out: Vec<LineChange> = Vec::new();
        let mut next_new = 0u32;
        for s in steps {
            match *s {
                Step::Both(_, j) => next_new = j as u32 + 1,
                Step::New(j) => {
                    let j = j as u32;
                    match out.last_mut() {
                        Some(l) if l.inserted && l.axis == axis && l.start + l.count == j => l.count += 1,
                        _ => out.push(LineChange { axis, inserted: true, start: j, count: 1, at_new: j }),
                    }
                    next_new = j + 1;
                }
                Step::Old(i) => {
                    let i = i as u32;
                    match out.last_mut() {
                        Some(l) if !l.inserted && l.axis == axis && l.start + l.count == i => l.count += 1,
                        _ => out.push(LineChange { axis, inserted: false, start: i, count: 1, at_new: next_new }),
                    }
                }
            }
        }
        out
    }

    /// The structural edits (in the old sheet's terms) that turn old line
    /// numbers into new ones: deletions bottom-up, then insertions top-down.
    fn shifts(&self) -> Vec<Shift> {
        let sheet = self.old.sheet;
        let mut out = Vec::new();
        for (steps, rows) in [(&self.row_steps, true), (&self.col_steps, false)] {
            let lines = Self::line_changes(steps, if rows { LineAxis::Row } else { LineAxis::Col });
            for l in lines.iter().rev().filter(|l| !l.inserted) {
                out.push(if rows { Shift::DeleteRows { sheet, at: l.start, count: l.count } } else { Shift::DeleteCols { sheet, at: l.start, count: l.count } });
            }
            for l in lines.iter().filter(|l| l.inserted) {
                out.push(if rows { Shift::InsertRows { sheet, at: l.start, count: l.count } } else { Shift::InsertCols { sheet, at: l.start, count: l.count } });
            }
        }
        out
    }
}

// --------------------------------------------------------------- matching

/// Pair the sheets of two workbooks: by name, then (renames) by content.
fn match_sheets(old: &Workbook, new: &Workbook) -> Vec<(Option<SheetId>, Option<SheetId>)> {
    let mut old_left: Vec<SheetId> = old.sheet_ids().to_vec();
    let mut pairs: HashMap<SheetId, SheetId> = HashMap::new(); // new -> old
    for &n in new.sheet_ids() {
        let name = new.sheet_name(n);
        let found = old_left.iter().position(|&o| old.sheet_name(o) == name).or_else(|| old_left.iter().position(|&o| old.sheet_name(o).eq_ignore_ascii_case(name)));
        if let Some(i) = found {
            pairs.insert(n, old_left.remove(i));
        }
    }
    let new_left: Vec<SheetId> = new.sheet_ids().iter().copied().filter(|n| !pairs.contains_key(n)).collect();
    if !old_left.is_empty() && !new_left.is_empty() {
        let keys = |wb: &Workbook, s: SheetId| Grid::new(wb, s).content_keys();
        let ok: Vec<Vec<u64>> = old_left.iter().map(|&s| keys(old, s)).collect();
        let nk: Vec<Vec<u64>> = new_left.iter().map(|&s| keys(new, s)).collect();
        let mut cands: Vec<(f64, usize, usize)> = Vec::new();
        for (i, a) in ok.iter().enumerate() {
            for (j, b) in nk.iter().enumerate() {
                cands.push((overlap(a, b), i, j));
            }
        }
        cands.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        let one_each = old_left.len() == 1 && new_left.len() == 1;
        let pos = |wb: &Workbook, s: SheetId| wb.sheet_ids().iter().position(|&x| x == s);
        let (mut used_o, mut used_n) = (vec![false; ok.len()], vec![false; nk.len()]);
        for (score, i, j) in cands {
            if used_o[i] || used_n[j] {
                continue;
            }
            // A lone renamed sheet in the same place may have been edited a lot too.
            let same_place = one_each && pos(old, old_left[i]) == pos(new, new_left[j]);
            if score >= 0.5 || (same_place && score >= 0.2) || (same_place && ok[i].is_empty() && nk[j].is_empty()) {
                used_o[i] = true;
                used_n[j] = true;
                pairs.insert(new_left[j], old_left[i]);
            }
        }
        old_left = old_left.into_iter().enumerate().filter(|(i, _)| !used_o[*i]).map(|(_, s)| s).collect();
    }
    let mut out: Vec<(Option<SheetId>, Option<SheetId>)> = new.sheet_ids().iter().map(|n| (pairs.get(n).copied(), Some(*n))).collect();
    out.extend(old_left.into_iter().map(|o| (Some(o), None)));
    out
}

// ------------------------------------------------------------------- diff

/// Compare two workbooks.
pub fn diff_workbooks(old: &Workbook, new: &Workbook) -> WorkbookDiff {
    let pairs = match_sheets(old, new);
    // Align every sheet first: comparing formulas needs every sheet's shifts.
    let mut aligned: HashMap<SheetId, Aligned> = HashMap::new();
    for &(o, n) in &pairs {
        if let (Some(o), Some(n)) = (o, n) {
            aligned.insert(n, Aligned::new(Grid::new(old, o), Grid::new(new, n)));
        }
    }
    let mut shifts: Vec<Shift> = Vec::new();
    for &(_, n) in &pairs {
        if let Some(a) = n.and_then(|n| aligned.get(&n)) {
            shifts.extend(a.shifts());
        }
    }

    let mut sheets = Vec::new();
    for &(o, n) in &pairs {
        sheets.push(match (o, n) {
            (Some(o), Some(n)) => matched_sheet(old, new, &aligned[&n], &shifts, o, n),
            (None, Some(n)) => whole_sheet(new, n, true),
            (Some(o), None) => whole_sheet(old, o, false),
            (None, None) => continue,
        });
    }
    let kept_order_old: Vec<SheetId> = old.sheet_ids().iter().copied().filter(|o| pairs.iter().any(|p| p.0 == Some(*o) && p.1.is_some())).collect();
    let kept_order_new: Vec<SheetId> = pairs.iter().filter_map(|p| p.0.filter(|_| p.1.is_some())).collect();
    WorkbookDiff { sheets, names: name_changes(old, new), sheet_order_changed: kept_order_old != kept_order_new }
}

/// A sheet only one side has: every cell added or removed.
fn whole_sheet(wb: &Workbook, sheet: SheetId, added: bool) -> SheetDiff {
    let g = Grid::new(wb, sheet);
    let mut cells: Vec<(CellRef, CellInfo)> = g.cells.into_iter().filter(|(_, i)| i.has_content()).collect();
    cells.sort_by_key(|(c, _)| *c);
    let cells = cells
        .into_iter()
        .map(|(c, info)| CellChange {
            kind: if added { CellKind::Added } else { CellKind::Removed },
            old_at: (!added).then_some(c),
            new_at: added.then_some(c),
            old: (!added).then(|| info.clone()),
            new: added.then_some(info),
            content: true,
            value: true,
            format: false,
            style: false,
            in_line: true,
        })
        .collect();
    let name = Some(wb.sheet_name(sheet).to_string());
    SheetDiff {
        status: if added { SheetStatus::Added } else { SheetStatus::Removed },
        old_name: if added { None } else { name.clone() },
        new_name: if added { name } else { None },
        lines: Vec::new(),
        cells,
        layout: Vec::new(),
    }
}

/// Is the old formula, carried through the row/column changes, the new one?
fn same_formula(old_wb: &Workbook, a: Addr, old_f: &str, new_f: &str, shifts: &[Shift]) -> bool {
    if old_f == new_f {
        return true;
    }
    let Some(CellInput::Formula(e)) = old_wb.cell(a).map(|c| &c.input) else { return false };
    if shifts.is_empty() {
        return false;
    }
    let mut moved: Expr = (**e).clone();
    for op in shifts {
        moved = moved.shifted(*op, a.sheet);
    }
    format!("={}", moved.to_source(0, 0, old_wb.sheets())) == new_f
}

fn compare_cells(old_wb: &Workbook, al: &Aligned, shifts: &[Shift], old_at: CellRef, new_at: CellRef, a: &CellInfo, b: &CellInfo) -> Option<CellChange> {
    let content = match (&a.formula, &b.formula) {
        (Some(x), Some(y)) => !same_formula(old_wb, Addr::new(al.old.sheet, old_at), x, y, shifts),
        (None, None) => a.value != b.value,
        _ => true,
    };
    // A recomputed result counts, except for volatile formulas (RAND, NOW…).
    let value = a.value != b.value && !(a.volatile && b.volatile && !content);
    let format = a.format != b.format;
    let style = a.style != b.style;
    (content || value || format || style).then(|| CellChange {
        kind: CellKind::Modified,
        old_at: Some(old_at),
        new_at: Some(new_at),
        old: Some(a.clone()),
        new: Some(b.clone()),
        content,
        value,
        format,
        style,
        in_line: false,
    })
}

fn matched_sheet(old: &Workbook, new: &Workbook, al: &Aligned, shifts: &[Shift], o: SheetId, n: SheetId) -> SheetDiff {
    let mut cells = Vec::new();
    for (&oc, a) in &al.old.cells {
        let to = al.map_cell(oc);
        match to.and_then(|nc| al.new.cells.get(&nc).map(|b| (nc, b))) {
            Some((nc, b)) => cells.extend(compare_cells(old, al, shifts, oc, nc, a, b)),
            None => {
                // Cleared, or in a deleted line (only content is worth reporting there).
                let in_line = to.is_none();
                if in_line && !a.has_content() {
                    continue;
                }
                cells.push(CellChange {
                    kind: CellKind::Removed,
                    old_at: Some(oc),
                    new_at: to,
                    old: Some(a.clone()),
                    new: None,
                    content: a.has_content(),
                    value: a.value != Value::Empty,
                    format: a.format.is_some(),
                    style: !a.style.is_default(),
                    in_line,
                });
            }
        }
    }
    for (&nc, b) in &al.new.cells {
        let from = al.unmap_cell(nc);
        if from.is_some_and(|oc| al.old.cells.contains_key(&oc)) {
            continue;
        }
        let in_line = from.is_none();
        if in_line && !b.has_content() {
            continue;
        }
        cells.push(CellChange {
            kind: CellKind::Added,
            old_at: from,
            new_at: Some(nc),
            old: None,
            new: Some(b.clone()),
            content: b.has_content(),
            value: b.value != Value::Empty,
            format: b.format.is_some(),
            style: !b.style.is_default(),
            in_line,
        });
    }
    cells.sort_by_key(|c| (c.new_at.map(|p| (p.row, p.col)), c.old_at.map(|p| (p.row, p.col))));
    let mut lines = Aligned::line_changes(&al.row_steps, LineAxis::Row);
    lines.extend(Aligned::line_changes(&al.col_steps, LineAxis::Col));
    SheetDiff {
        status: SheetStatus::Matched,
        old_name: Some(old.sheet_name(o).to_string()),
        new_name: Some(new.sheet_name(n).to_string()),
        lines,
        cells,
        layout: layout_changes(old.layout(o), new.layout(n), al),
    }
}

// ----------------------------------------------------------------- layout

fn px(w: Option<&f64>) -> String {
    w.map_or_else(|| "default".to_string(), |w| format!("{}px", w.round()))
}

/// Sizes that differ by less than a saved file can represent (Excel stores
/// widths in characters and heights in quarter points) are the same.
fn same_size(a: Option<&f64>, b: Option<&f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => (x - y).abs() < 2.0,
        (None, None) => true,
        _ => false,
    }
}

fn quoted(s: &str) -> String {
    let short: String = s.chars().take(60).collect();
    let more = if short.len() < s.len() { "…" } else { "" };
    format!("“{}{more}”", short.replace('\n', " "))
}

fn ranges(rs: &[Range]) -> String {
    rs.iter().map(Range::to_string).collect::<Vec<_>>().join(" ")
}

/// Lines (columns or rows) whose size or visibility may have changed, in new terms.
fn line_keys<T>(old: &BTreeMap<u32, T>, new: &BTreeMap<u32, T>, map: &LineMap) -> BTreeSet<u32> {
    old.keys().filter_map(|&i| map.to_new(i)).chain(new.keys().copied()).collect()
}

fn layout_changes(a: &SheetLayout, b: &SheetLayout, al: &Aligned) -> Vec<LayoutChange> {
    let mut out = Vec::new();
    let mut push = |text: String, at: Option<CellRef>| out.push(LayoutChange { text, at });

    for n in line_keys(&a.col_widths, &b.col_widths, &al.cols) {
        let Some(o) = al.cols.to_old(n) else { continue };
        let (x, y) = (a.col_widths.get(&o), b.col_widths.get(&n));
        if !same_size(x, y) {
            push(format!("column {} width {} → {}", col_name(n), px(x), px(y)), Some(CellRef::new(0, n)));
        }
    }
    for n in line_keys(&a.row_heights, &b.row_heights, &al.rows) {
        let Some(o) = al.rows.to_old(n) else { continue };
        let (x, y) = (a.row_heights.get(&o), b.row_heights.get(&n));
        if !same_size(x, y) {
            push(format!("row {} height {} → {}", n + 1, px(x), px(y)), Some(CellRef::new(n, 0)));
        }
    }
    let as_map = |s: &BTreeSet<u32>| s.iter().map(|&i| (i, ())).collect::<BTreeMap<u32, ()>>();
    for (old_set, new_set, map, rows) in [(&a.hidden_rows, &b.hidden_rows, &al.rows, true), (&a.hidden_cols, &b.hidden_cols, &al.cols, false)] {
        for n in line_keys(&as_map(old_set), &as_map(new_set), map) {
            let Some(o) = map.to_old(n) else { continue };
            let (was, is) = (old_set.contains(&o), new_set.contains(&n));
            if was != is {
                let (what, at) = if rows { (format!("row {}", n + 1), CellRef::new(n, 0)) } else { (format!("column {}", col_name(n)), CellRef::new(0, n)) };
                push(format!("{what} {}", if is { "hidden" } else { "shown" }), Some(at));
            }
        }
    }

    let old_merges: BTreeSet<(u32, u32, u32, u32)> = a.merges.iter().map(|m| al.map_range(*m).unwrap_or(*m)).map(|r| (r.start.row, r.start.col, r.end.row, r.end.col)).collect();
    let new_merges: BTreeSet<(u32, u32, u32, u32)> = b.merges.iter().map(|r| (r.start.row, r.start.col, r.end.row, r.end.col)).collect();
    for m in old_merges.difference(&new_merges) {
        let r = Range::new(CellRef::new(m.0, m.1), CellRef::new(m.2, m.3));
        push(format!("unmerged {r}"), Some(r.start));
    }
    for m in new_merges.difference(&old_merges) {
        let r = Range::new(CellRef::new(m.0, m.1), CellRef::new(m.2, m.3));
        push(format!("merged {r}"), Some(r.start));
    }

    if a.freeze != b.freeze {
        let f = |(r, c): (u32, u32)| match (r, c) {
            (0, 0) => "none".to_string(),
            (r, 0) => format!("{r} row(s)"),
            (0, c) => format!("{c} column(s)"),
            (r, c) => format!("{r} row(s), {c} column(s)"),
        };
        push(format!("frozen panes {} → {}", f(a.freeze), f(b.freeze)), None);
    }
    match (&a.filter, &b.filter) {
        (None, Some(f)) => push(format!("filter added on {}", f.range), Some(f.range.start)),
        (Some(f), None) => push(format!("filter removed from {}", al.map_range(f.range).unwrap_or(f.range)), None),
        (Some(x), Some(y)) if al.map_range(x.range) != Some(y.range) || x.columns != y.columns => push(format!("filter on {} changed", y.range), Some(y.range.start)),
        _ => {}
    }

    // Notes and links, by cell.
    let keyed = |old: Vec<(CellRef, String)>, new: Vec<(CellRef, String)>, what: &str, push: &mut dyn FnMut(String, Option<CellRef>)| {
        let new: BTreeMap<CellRef, String> = new.into_iter().collect();
        let mut seen = BTreeSet::new();
        for (oc, text) in old {
            match al.map_cell(oc).and_then(|nc| new.get(&nc).map(|t| (nc, t))) {
                Some((nc, t)) => {
                    seen.insert(nc);
                    if *t != text {
                        push(format!("{what} on {nc} changed: {} → {}", quoted(&text), quoted(t)), Some(nc));
                    }
                }
                None => push(format!("{what} on {} removed: {}", al.map_cell(oc).unwrap_or(oc), quoted(&text)), al.map_cell(oc)),
            }
        }
        for (nc, t) in &new {
            if !seen.contains(nc) {
                push(format!("{what} on {nc} added: {}", quoted(t)), Some(*nc));
            }
        }
    };
    keyed(a.notes.iter().map(|(c, n)| (*c, n.text.clone())).collect(), b.notes.iter().map(|(c, n)| (*c, n.text.clone())).collect(), "note", &mut push);
    keyed(a.links.iter().map(|(c, l)| (*c, l.target().to_string())).collect(), b.links.iter().map(|(c, l)| (*c, l.target().to_string())).collect(), "link", &mut push);

    // Validations and conditional formats: equal after moving their ranges.
    let moved = |rs: &[Range]| -> Vec<Range> { rs.iter().map(|r| al.map_range(*r).unwrap_or(*r)).collect() };
    let mut unmatched: Vec<bool> = vec![true; b.validations.len()];
    for v in &a.validations {
        let mut v2 = v.clone();
        v2.ranges = moved(&v.ranges);
        match b.validations.iter().enumerate().position(|(j, w)| unmatched[j] && *w == v2) {
            Some(j) => unmatched[j] = false,
            None => match b.validations.iter().enumerate().position(|(j, w)| unmatched[j] && w.ranges == v2.ranges) {
                Some(j) => {
                    unmatched[j] = false;
                    push(format!("data validation on {} changed", ranges(&v2.ranges)), v2.ranges.first().map(|r| r.start));
                }
                None => push(format!("data validation on {} removed", ranges(&v2.ranges)), None),
            },
        }
    }
    for (j, w) in b.validations.iter().enumerate().filter(|(j, _)| unmatched[*j]) {
        let _ = j;
        push(format!("data validation on {} added", ranges(&w.ranges)), w.ranges.first().map(|r| r.start));
    }
    let mut unmatched: Vec<bool> = vec![true; b.cond.len()];
    for c in &a.cond {
        let mut c2 = c.clone();
        c2.ranges = moved(&c.ranges);
        match b.cond.iter().enumerate().position(|(j, w)| unmatched[j] && *w == c2) {
            Some(j) => unmatched[j] = false,
            None => push(format!("conditional format on {} removed", ranges(&c2.ranges)), None),
        }
    }
    for w in b.cond.iter().enumerate().filter(|(j, _)| unmatched[*j]).map(|(_, w)| w) {
        push(format!("conditional format on {} added", ranges(&w.ranges)), w.ranges.first().map(|r| r.start));
    }

    // Charts, in order.
    let chart_name = |c: &xl_engine::chart::Chart| match &c.title {
        Some(t) if !t.is_empty() => format!("chart {} ({})", quoted(t), c.kind.label().to_lowercase()),
        _ => format!("{} chart", c.kind.label().to_lowercase()),
    };
    for i in 0..a.charts.len().max(b.charts.len()) {
        match (a.charts.get(i), b.charts.get(i)) {
            (Some(x), Some(y)) => {
                let mut what = Vec::new();
                if x.kind != y.kind {
                    what.push(format!("type {} → {}", x.kind.label().to_lowercase(), y.kind.label().to_lowercase()));
                }
                if x.title != y.title {
                    what.push(format!("title {} → {}", quoted(x.title.as_deref().unwrap_or("")), quoted(y.title.as_deref().unwrap_or(""))));
                }
                if x.series != y.series {
                    what.push("data".into());
                }
                if al.map_cell(x.anchor) != Some(y.anchor) || x.offset != y.offset {
                    what.push(format!("moved to {}", y.anchor));
                }
                if x.size != y.size {
                    what.push("resized".into());
                }
                if x.stacked != y.stacked {
                    what.push(if y.stacked { "stacked".into() } else { "side by side".into() });
                }
                if !what.is_empty() {
                    push(format!("{} changed: {}", chart_name(y), what.join(", ")), Some(y.anchor));
                }
            }
            (None, Some(y)) => push(format!("{} added at {}", chart_name(y), y.anchor), Some(y.anchor)),
            (Some(x), None) => push(format!("{} removed", chart_name(x)), None),
            (None, None) => {}
        }
    }

    // Tables, by name.
    for t in &a.tables {
        match b.tables.iter().find(|u| u.name == t.name) {
            None => push(format!("table {} removed", t.name), None),
            Some(u) => {
                if al.map_range(t.range) != Some(u.range) {
                    push(format!("table {} now covers {}", u.name, u.range), Some(u.range.start));
                } else if *t != (xl_engine::Table { range: t.range, ..u.clone() }) {
                    push(format!("table {} style changed", u.name), Some(u.range.start));
                }
            }
        }
    }
    for u in b.tables.iter().filter(|u| !a.tables.iter().any(|t| t.name == u.name)) {
        push(format!("table {} added on {}", u.name, u.range), Some(u.range.start));
    }

    let counted = |n: usize, m: usize, what: &str, push: &mut dyn FnMut(String, Option<CellRef>)| {
        if n != m {
            push(format!("{what}: {n} → {m}"), None);
        }
    };
    counted(a.pictures.len(), b.pictures.len(), "pictures", &mut push);
    counted(a.sparklines.len(), b.sparklines.len(), "sparklines", &mut push);
    counted(a.pivots.len(), b.pivots.len(), "pivot tables", &mut push);
    if a.row_levels.len() != b.row_levels.len() || a.col_levels.len() != b.col_levels.len() {
        push("row/column grouping changed".into(), None);
    }
    out
}

fn name_changes(old: &Workbook, new: &Workbook) -> Vec<NameChange> {
    let collect = |wb: &Workbook| -> BTreeMap<String, String> {
        wb.names()
            .iter()
            .map(|d| {
                let key = match d.scope {
                    Some(s) => format!("{}!{}", xl_engine::quote_sheet_name(wb.sheet_name(s)), d.name),
                    None => d.name.clone(),
                };
                (key, format!("={}", wb.name_formula(d)))
            })
            .collect()
    };
    let (a, b) = (collect(old), collect(new));
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .map(|k| NameChange { name: k.clone(), old: a.get(k).cloned(), new: b.get(k).cloned() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(sheets: &[(&str, &[&[&str]])]) -> Workbook {
        let mut wb = Workbook::empty();
        for (name, rows) in sheets {
            let s = wb.add_sheet(Some(name)).unwrap();
            let mut items = Vec::new();
            for (r, row) in rows.iter().enumerate() {
                for (c, v) in row.iter().enumerate() {
                    if !v.is_empty() {
                        items.push((Addr::new(s, CellRef::new(r as u32, c as u32)), v.to_string()));
                    }
                }
            }
            wb.set_inputs(items);
        }
        wb
    }

    fn at(s: &str) -> CellRef {
        CellRef::parse(s).unwrap()
    }

    const BASE: &[&[&str]] = &[&["Item", "Qty", "Price"], &["Coffee", "3", "14.5"], &["Milk", "6", "2.25"], &["Bread", "1", "3"], &["Total", "=SUM(B2:B4)", ""]];

    #[test]
    fn identical_workbooks_have_no_changes() {
        let d = diff_workbooks(&book(&[("S", BASE)]), &book(&[("S", BASE)]));
        assert!(d.is_empty());
        assert_eq!(d.change_count(), 0);
    }

    #[test]
    fn inserted_row_is_one_change_not_everything_below() {
        let new: &[&[&str]] =
            &[&["Item", "Qty", "Price"], &["Coffee", "3", "14.5"], &["Tea", "2", "4"], &["Milk", "6", "2.25"], &["Bread", "1", "3"], &["Total", "=SUM(B2:B5)", ""]];
        let d = diff_workbooks(&book(&[("S", BASE)]), &book(&[("S", new)]));
        let s = &d.sheets[0];
        assert_eq!(s.lines, vec![LineChange { axis: LineAxis::Row, inserted: true, start: 2, count: 1, at_new: 2 }]);
        // Only the inserted row's cells, plus the total (its range grew, so
        // the formula and its result changed).
        let outside: Vec<&CellChange> = s.cells.iter().filter(|c| !c.in_line).collect();
        assert_eq!(outside.len(), 1, "{outside:#?}");
        assert_eq!(outside[0].old_at, Some(at("B5")));
        assert_eq!(outside[0].new_at, Some(at("B6")));
        assert!(outside[0].value);
        assert_eq!(s.cells.iter().filter(|c| c.in_line && c.kind == CellKind::Added).count(), 3);
    }

    #[test]
    fn formula_carried_through_an_insertion_is_unchanged() {
        // B5 sums B2:B4; a row inserted above row 2 moves everything down
        // and the formula becomes =SUM(B3:B5): not a change.
        let new: &[&[&str]] = &[&["Item", "Qty", "Price"], &["New", "", ""], &["Coffee", "3", "14.5"], &["Milk", "6", "2.25"], &["Bread", "1", "3"], &["Total", "=SUM(B3:B5)", ""]];
        let d = diff_workbooks(&book(&[("S", BASE)]), &book(&[("S", new)]));
        let s = &d.sheets[0];
        assert_eq!(s.lines.len(), 1);
        assert!(s.cells.iter().all(|c| c.in_line), "{:#?}", s.cells);
    }

    #[test]
    fn deleted_row() {
        let new: &[&[&str]] = &[&["Item", "Qty", "Price"], &["Coffee", "3", "14.5"], &["Bread", "1", "3"], &["Total", "=SUM(B2:B3)", ""]];
        let d = diff_workbooks(&book(&[("S", BASE)]), &book(&[("S", new)]));
        let s = &d.sheets[0];
        assert_eq!(s.lines, vec![LineChange { axis: LineAxis::Row, inserted: false, start: 2, count: 1, at_new: 2 }]);
        // The deleted row's cells, then the total's recomputed result (the
        // formula itself just followed the deletion).
        let outside: Vec<&CellChange> = s.cells.iter().filter(|c| !c.in_line).collect();
        assert_eq!(outside.len(), 1, "{outside:#?}");
        assert!(!outside[0].content && outside[0].value);
    }

    #[test]
    fn inserted_column() {
        let new: &[&[&str]] = &[&["Item", "SKU", "Qty", "Price"], &["Coffee", "c1", "3", "14.5"], &["Milk", "m1", "6", "2.25"], &["Bread", "b1", "1", "3"], &["Total", "", "=SUM(C2:C4)", ""]];
        let d = diff_workbooks(&book(&[("S", BASE)]), &book(&[("S", new)]));
        let s = &d.sheets[0];
        assert_eq!(s.lines, vec![LineChange { axis: LineAxis::Col, inserted: true, start: 1, count: 1, at_new: 1 }]);
        assert!(s.cells.iter().all(|c| c.in_line), "{:#?}", s.cells);
    }

    #[test]
    fn value_and_formula_changes() {
        let new: &[&[&str]] = &[&["Item", "Qty", "Price"], &["Coffee", "4", "14.5"], &["Milk", "6", "2.25"], &["Bread", "1", "3"], &["Total", "=SUM(B2:B3)", ""]];
        let d = diff_workbooks(&book(&[("S", BASE)]), &book(&[("S", new)]));
        let s = &d.sheets[0];
        assert!(s.lines.is_empty());
        assert_eq!(s.cells.len(), 2);
        let b2 = &s.cells[0];
        assert_eq!((b2.new_at, b2.content, b2.value), (Some(at("B2")), true, true));
        let b5 = &s.cells[1];
        assert_eq!((b5.new_at, b5.content), (Some(at("B5")), true));
        assert_eq!(b5.new.as_ref().unwrap().formula.as_deref(), Some("=SUM(B2:B3)"));
    }

    #[test]
    fn result_only_change() {
        let new: &[&[&str]] = &[&["Item", "Qty", "Price"], &["Coffee", "30", "14.5"], &["Milk", "6", "2.25"], &["Bread", "1", "3"], &["Total", "=SUM(B2:B4)", ""]];
        let d = diff_workbooks(&book(&[("S", BASE)]), &book(&[("S", new)]));
        let total = d.sheets[0].cells.iter().find(|c| c.new_at == Some(at("B5"))).unwrap();
        assert!(!total.content && total.value);
    }

    #[test]
    fn renamed_sheet_is_matched_by_content() {
        let d = diff_workbooks(&book(&[("Data", BASE)]), &book(&[("Sales", BASE)]));
        assert_eq!(d.sheets.len(), 1);
        assert!(d.sheets[0].renamed());
        assert!(d.sheets[0].cells.is_empty());
        assert_eq!(d.change_count(), 1);
    }

    #[test]
    fn added_and_removed_sheets() {
        let other: &[&[&str]] = &[&["x", "y"], &["1", "2"]];
        let d = diff_workbooks(&book(&[("A", BASE), ("B", other)]), &book(&[("A", BASE), ("C", &[&["completely"], &["different"]])]));
        let st: Vec<(SheetStatus, &str)> = d.sheets.iter().map(|s| (s.status, s.name())).collect();
        assert_eq!(st, vec![(SheetStatus::Matched, "A"), (SheetStatus::Added, "C"), (SheetStatus::Removed, "B")]);
    }

    #[test]
    fn style_and_format_changes() {
        let old = book(&[("S", BASE)]);
        let mut new = book(&[("S", BASE)]);
        let s = new.sheet_ids()[0];
        new.set_style(Addr::new(s, at("A1")), &Style { bold: true, ..Style::default() });
        new.set_format(Addr::new(s, at("C2")), Some("$#,##0.00"));
        new.set_style(Addr::new(s, at("E9")), &Style { fill: Some(0xFFFF00), ..Style::default() });
        let d = diff_workbooks(&old, &new);
        let cells = &d.sheets[0].cells;
        assert_eq!(cells.len(), 3, "{cells:#?}");
        assert!(cells[0].style && !cells[0].content && !cells[0].format);
        assert!(cells[1].format && !cells[1].style && !cells[1].content);
        assert_eq!((cells[2].kind, cells[2].style, cells[2].content), (CellKind::Added, true, false));
    }

    #[test]
    fn layout_changes_follow_moved_lines() {
        let old = book(&[("S", BASE)]);
        let new_rows: &[&[&str]] = &[&["Title"], &["Item", "Qty", "Price"], &["Coffee", "3", "14.5"], &["Milk", "6", "2.25"], &["Bread", "1", "3"], &["Total", "=SUM(B3:B5)", ""]];
        let mut old = old;
        let mut new = book(&[("S", new_rows)]);
        let (os, ns) = (old.sheet_ids()[0], new.sheet_ids()[0]);
        // Same merge, width and note, one row lower: no layout change.
        old.merge(os, Range::new(at("A2"), at("B2")));
        new.merge(ns, Range::new(at("A3"), at("B3")));
        old.set_col_width(os, 1, Some(150.0));
        new.set_col_width(ns, 1, Some(150.0));
        old.layout_mut(os).unwrap().notes.insert(at("C3"), xl_engine::Note { author: String::new(), text: "check".into() });
        new.layout_mut(ns).unwrap().notes.insert(at("C4"), xl_engine::Note { author: String::new(), text: "check again".into() });
        new.set_col_width(ns, 2, Some(80.0));
        let d = diff_workbooks(&old, &new);
        let texts: Vec<&str> = d.sheets[0].layout.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["column C width default → 80px", "note on C4 changed: “check” → “check again”"]);
    }

    #[test]
    fn defined_names() {
        let mut old = book(&[("S", BASE)]);
        let mut new = book(&[("S", BASE)]);
        old.define_name("Total", None, "S!$B$5").unwrap();
        new.define_name("Total", None, "S!$B$6").unwrap();
        new.define_name("Rate", None, "0.5").unwrap();
        let d = diff_workbooks(&old, &new);
        let names: Vec<(&str, Option<&str>, Option<&str>)> = d.names.iter().map(|n| (n.name.as_str(), n.old.as_deref(), n.new.as_deref())).collect();
        assert_eq!(names, vec![("Rate", None, Some("=0.5")), ("Total", Some("=S!$B$5"), Some("=S!$B$6"))]);
    }

    #[test]
    fn big_sheet_with_an_inserted_row_and_column() {
        let build = |insert: bool| {
            let mut wb = Workbook::new();
            let s = wb.sheet_ids()[0];
            let mut items = Vec::new();
            for r in 0..20_000u32 {
                let row = if insert && r >= 10_000 { r + 1 } else { r };
                for c in 0..5u32 {
                    let col = if insert && c >= 2 { c + 1 } else { c };
                    items.push((Addr::new(s, CellRef::new(row, col)), format!("{}", r * 7 + c)));
                }
            }
            if insert {
                items.push((Addr::new(s, CellRef::new(10_000, 0)), "new".into()));
                items.push((Addr::new(s, CellRef::new(0, 2)), "new col".into()));
            }
            wb.set_inputs(items);
            wb
        };
        let start = std::time::Instant::now();
        let d = diff_workbooks(&build(false), &build(true));
        let s = &d.sheets[0];
        assert_eq!(s.lines.len(), 2, "{:?}", s.lines);
        assert!(s.cells.iter().all(|c| c.in_line));
        assert!(start.elapsed().as_secs() < 20, "{:?}", start.elapsed());
    }

    #[test]
    fn shape_blanks_numbers() {
        assert_eq!(formula_shape("=SUM(A1:A10)*2.5"), "=SUM(A#:A#)*#.#");
    }
}
