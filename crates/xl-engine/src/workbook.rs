//! A workbook: sheets of cells, number formats, a cross-sheet dependency
//! graph and incremental recalculation.
//!
//! Recalc on edit:
//! 1. Walk dependents from the edited cells (plus volatile cells) to collect
//!    the dirty subgraph. Single-cell refs are direct edges; range refs are
//!    found through a column-banded range index, so `SUM(A:A)` costs one index
//!    entry rather than a million edges.
//! 2. Topologically order the dirty subgraph (Kahn's algorithm).
//! 3. Evaluate in order, only cells whose inputs changed ("early cutoff").
//! 4. If Kahn stalls, mark exactly the cycle members (Tarjan SCC) `#CIRC!`
//!    and release cells merely downstream of them.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::address::{Addr, CellRef, Range, SheetId};
use crate::eval::{Ctx, Func, eval};
use crate::format::{Formatted, NumberFormat};
use crate::formula::{Expr, ParseError, SheetNames, parse_formula};
use crate::input::classify_literal;
use crate::store::{Cell, CellInput, CellStore};
use crate::style::Style;
use crate::value::{ErrorKind, Value};

// ------------------------------------------------------------------- sheets

pub struct SheetData {
    pub id: SheetId,
    pub name: String,
    store: CellStore,
    formats: FxHashMap<CellRef, FmtId>,
    styles: FxHashMap<CellRef, StyleId>,
    col_widths: BTreeMap<u32, f64>,
}

/// All sheets, addressable by stable id, plus their display order.
#[derive(Default)]
pub struct Sheets {
    slots: Vec<Option<SheetData>>,
    order: Vec<SheetId>,
}

impl Sheets {
    pub fn store(&self, id: SheetId) -> Option<&CellStore> {
        self.get(id).map(|s| &s.store)
    }

    fn get(&self, id: SheetId) -> Option<&SheetData> {
        self.slots.get(id as usize).and_then(Option::as_ref)
    }

    fn get_mut(&mut self, id: SheetId) -> Option<&mut SheetData> {
        self.slots.get_mut(id as usize).and_then(Option::as_mut)
    }
}

impl SheetNames for Sheets {
    fn sheet_id(&self, name: &str) -> Option<SheetId> {
        self.order.iter().copied().find(|&id| self.get(id).is_some_and(|s| s.name.eq_ignore_ascii_case(name)))
    }

    fn sheet_name(&self, id: SheetId) -> Option<&str> {
        self.get(id).map(|s| s.name.as_str())
    }
}

/// Excel's sheet-name rules.
pub fn validate_sheet_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Sheet name can't be empty".into());
    }
    if name.chars().count() > 31 {
        return Err("Sheet name must be 31 characters or fewer".into());
    }
    if let Some(c) = name.chars().find(|c| matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\')) {
        return Err(format!("Sheet name can't contain '{c}'"));
    }
    if name.starts_with('\'') || name.ends_with('\'') {
        return Err("Sheet name can't start or end with an apostrophe".into());
    }
    Ok(())
}

// ------------------------------------------------------------------ formats

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FmtId(u32);

#[derive(Default)]
struct FormatTable {
    list: Vec<NumberFormat>,
    by_code: FxHashMap<String, FmtId>,
}

impl FormatTable {
    fn intern(&mut self, code: &str) -> FmtId {
        if let Some(id) = self.by_code.get(code) {
            return *id;
        }
        let id = FmtId(self.list.len() as u32);
        self.list.push(NumberFormat::parse(code));
        self.by_code.insert(code.to_string(), id);
        id
    }

    fn get(&self, id: FmtId) -> &NumberFormat {
        &self.list[id.0 as usize]
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct StyleId(u32);

#[derive(Default)]
struct StyleTable {
    list: Vec<Style>,
    by_style: FxHashMap<Style, StyleId>,
}

impl StyleTable {
    fn intern(&mut self, s: &Style) -> StyleId {
        if let Some(id) = self.by_style.get(s) {
            return *id;
        }
        let id = StyleId(self.list.len() as u32);
        self.list.push(s.clone());
        self.by_style.insert(s.clone(), id);
        id
    }
}

static DEFAULT_STYLE: std::sync::LazyLock<Style> = std::sync::LazyLock::new(Style::default);

// -------------------------------------------------------------------- graph

/// Ranges wider than this many columns go in a linear "wide" list.
const NARROW_COLS: u32 = 64;

/// (first row, last row, dependent) for ranges covering one column.
type ColumnBands = Vec<(u32, u32, Addr)>;

#[derive(Default)]
struct RangeIndex {
    by_col: FxHashMap<(SheetId, u32), ColumnBands>,
    wide: Vec<(SheetId, Range, Addr)>,
}

impl RangeIndex {
    fn add(&mut self, sheet: SheetId, r: Range, dep: Addr) {
        if r.cols() > NARROW_COLS {
            self.wide.push((sheet, r, dep));
        } else {
            for c in r.start.col..=r.end.col {
                self.by_col.entry((sheet, c)).or_default().push((r.start.row, r.end.row, dep));
            }
        }
    }

    fn remove(&mut self, sheet: SheetId, r: Range, dep: Addr) {
        if r.cols() > NARROW_COLS {
            if let Some(i) = self.wide.iter().position(|e| *e == (sheet, r, dep)) {
                self.wide.swap_remove(i);
            }
        } else {
            for c in r.start.col..=r.end.col {
                if let Some(v) = self.by_col.get_mut(&(sheet, c)) {
                    if let Some(i) = v.iter().position(|e| *e == (r.start.row, r.end.row, dep)) {
                        v.swap_remove(i);
                    }
                    if v.is_empty() {
                        self.by_col.remove(&(sheet, c));
                    }
                }
            }
        }
    }

    fn query(&self, a: Addr, f: &mut impl FnMut(Addr)) {
        if let Some(v) = self.by_col.get(&(a.sheet, a.cell.col)) {
            for &(r0, r1, dep) in v {
                if a.cell.row >= r0 && a.cell.row <= r1 {
                    f(dep);
                }
            }
        }
        for (s, r, dep) in &self.wide {
            if *s == a.sheet && r.contains(a.cell) {
                f(*dep);
            }
        }
    }
}

#[derive(Default)]
struct Precedents {
    cells: SmallVec<[Addr; 4]>,
    ranges: SmallVec<[(SheetId, Range); 2]>,
}

#[derive(Default)]
struct DepGraph {
    precedents: FxHashMap<Addr, Precedents>,
    cell_deps: FxHashMap<Addr, SmallVec<[Addr; 4]>>,
    ranges: RangeIndex,
    volatile: FxHashSet<Addr>,
}

impl DepGraph {
    fn add(&mut self, at: Addr, expr: &Expr) {
        let mut p = Precedents::default();
        expr.visit_refs(&mut |sheet, r| {
            let sheet = sheet.unwrap_or(at.sheet);
            if r.start == r.end {
                p.cells.push(Addr::new(sheet, r.start));
            } else {
                p.ranges.push((sheet, r));
            }
        });
        p.cells.sort_unstable();
        p.cells.dedup();
        p.ranges.dedup();
        for &c in &p.cells {
            self.cell_deps.entry(c).or_default().push(at);
        }
        for &(s, r) in &p.ranges {
            self.ranges.add(s, r, at);
        }
        if expr.is_volatile() {
            self.volatile.insert(at);
        }
        self.precedents.insert(at, p);
    }

    fn remove(&mut self, at: Addr) {
        let Some(p) = self.precedents.remove(&at) else { return };
        for c in p.cells {
            if let Some(v) = self.cell_deps.get_mut(&c) {
                if let Some(i) = v.iter().position(|d| *d == at) {
                    v.swap_remove(i);
                }
                if v.is_empty() {
                    self.cell_deps.remove(&c);
                }
            }
        }
        for (s, r) in p.ranges {
            self.ranges.remove(s, r, at);
        }
        self.volatile.remove(&at);
    }

    fn for_each_dependent(&self, a: Addr, mut f: impl FnMut(Addr)) {
        if let Some(v) = self.cell_deps.get(&a) {
            v.iter().copied().for_each(&mut f);
        }
        self.ranges.query(a, &mut f);
    }
}

// ----------------------------------------------------------------- workbook

#[derive(Debug, Default, Clone, Copy)]
pub struct RecalcStats {
    /// Cells in the dirty subgraph.
    pub dirty: usize,
    /// Formulas actually re-evaluated (after early cutoff).
    pub evaluated: usize,
    pub elapsed: Duration,
}

/// Everything needed to restore a cell (for undo, paste and fill).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct CellSnapshot {
    pub input: String,
    pub format: Option<String>,
    pub style: Style,
}

/// A cell as read from a file, applied without recalculation.
#[derive(Clone, Debug)]
pub enum LoadedCell {
    Value(Value),
    /// Formula text (without '=') and the value the file cached for it.
    Formula { src: String, cached: Value },
}

#[derive(Default)]
pub struct Workbook {
    sheets: Sheets,
    graph: DepGraph,
    formats: FormatTable,
    styles: StyleTable,
}

enum Parsed {
    Clear,
    Input(CellInput, Option<&'static str>),
}

impl Workbook {
    /// A workbook with one empty sheet named "Sheet1".
    pub fn new() -> Self {
        let mut wb = Self::default();
        wb.add_sheet(None).unwrap();
        wb
    }

    /// A workbook with no sheets (for file loaders).
    pub fn empty() -> Self {
        Self::default()
    }

    // ------------------------------------------------------------ sheets

    pub fn sheets(&self) -> &Sheets {
        &self.sheets
    }

    pub fn sheet_ids(&self) -> &[SheetId] {
        &self.sheets.order
    }

    pub fn sheet_name(&self, id: SheetId) -> &str {
        self.sheets.get(id).map_or("", |s| s.name.as_str())
    }

    pub fn sheet_id(&self, name: &str) -> Option<SheetId> {
        self.sheets.sheet_id(name)
    }

    fn next_sheet_name(&self) -> String {
        (1..).map(|i| format!("Sheet{i}")).find(|n| self.sheet_id(n).is_none()).unwrap()
    }

    /// Append a sheet; `None` picks the next free "SheetN" name.
    pub fn add_sheet(&mut self, name: Option<&str>) -> Result<SheetId, String> {
        let name = match name {
            Some(n) => {
                validate_sheet_name(n)?;
                if self.sheet_id(n.trim()).is_some() {
                    return Err(format!("A sheet named '{}' already exists", n.trim()));
                }
                n.trim().to_string()
            }
            None => self.next_sheet_name(),
        };
        let id = self.sheets.slots.len() as SheetId;
        self.sheets.slots.push(Some(SheetData { id, name, store: CellStore::default(), formats: FxHashMap::default(), styles: FxHashMap::default(), col_widths: BTreeMap::new() }));
        self.sheets.order.push(id);
        Ok(id)
    }

    pub fn rename_sheet(&mut self, id: SheetId, name: &str) -> Result<(), String> {
        validate_sheet_name(name)?;
        let name = name.trim();
        if let Some(other) = self.sheet_id(name)
            && other != id
        {
            return Err(format!("A sheet named '{name}' already exists"));
        }
        let s = self.sheets.get_mut(id).ok_or("No such sheet")?;
        s.name = name.to_string();
        Ok(())
    }

    pub fn move_sheet(&mut self, id: SheetId, to: usize) {
        if let Some(from) = self.sheets.order.iter().position(|&s| s == id) {
            self.sheets.order.remove(from);
            let to = to.min(self.sheets.order.len());
            self.sheets.order.insert(to, id);
        }
    }

    /// Delete a sheet. Formulas elsewhere that referenced it become `#REF!`.
    pub fn delete_sheet(&mut self, id: SheetId) -> Result<RecalcStats, String> {
        if self.sheets.order.len() <= 1 {
            return Err("A workbook needs at least one sheet".into());
        }
        let Some(sheet) = self.sheets.get(id) else { return Err("No such sheet".into()) };
        let cells: Vec<CellRef> = sheet.store.iter().map(|(c, _)| c).collect();
        let mut seeds = Vec::new();
        let mut forced = FxHashSet::default();
        for c in cells {
            self.apply(Addr::new(id, c), None, &mut seeds, &mut forced);
        }
        // Formulas pointing at the sheet (even at its empty cells) must re-evaluate.
        for (at, p) in &self.graph.precedents {
            if p.cells.iter().any(|a| a.sheet == id) || p.ranges.iter().any(|(s, _)| *s == id) {
                seeds.push(*at);
                forced.insert(*at);
            }
        }
        self.sheets.slots[id as usize] = None;
        self.sheets.order.retain(|&s| s != id);
        seeds.retain(|a| a.sheet != id);
        Ok(self.propagate(seeds, forced))
    }

    // ------------------------------------------------------------ reading

    pub fn store(&self, sheet: SheetId) -> &CellStore {
        static EMPTY_STORE: std::sync::OnceLock<CellStore> = std::sync::OnceLock::new();
        self.sheets.store(sheet).unwrap_or_else(|| EMPTY_STORE.get_or_init(CellStore::default))
    }

    pub fn cell(&self, a: Addr) -> Option<&Cell> {
        self.sheets.store(a.sheet)?.get(a.cell)
    }

    pub fn value(&self, a: Addr) -> &Value {
        static EMPTY: Value = Value::Empty;
        self.cell(a).map_or(&EMPTY, |c| &c.value)
    }

    pub fn format(&self, a: Addr) -> Option<&NumberFormat> {
        let id = self.sheets.get(a.sheet)?.formats.get(&a.cell)?;
        Some(self.formats.get(*id))
    }

    pub fn format_code(&self, a: Addr) -> Option<&str> {
        self.format(a).map(NumberFormat::code)
    }

    /// The value as displayed, with its number format applied.
    pub fn display(&self, a: Addr) -> Formatted {
        let v = self.value(a);
        match self.format(a) {
            Some(f) => f.format(v),
            None => Formatted { text: v.display(), color: None },
        }
    }

    /// Formula text without '=' (also for opaque formulas), if the cell has one.
    pub fn formula_source(&self, a: Addr) -> Option<String> {
        match &self.cell(a)?.input {
            CellInput::Formula(e) => Some(e.to_source(0, 0, &self.sheets)),
            CellInput::Opaque(src) => Some(src.to_string()),
            CellInput::Value(_) => None,
        }
    }

    /// What the user sees and edits in the formula bar.
    pub fn input_text(&self, a: Addr) -> String {
        let Some(cell) = self.cell(a) else { return String::new() };
        match &cell.input {
            CellInput::Formula(_) | CellInput::Opaque(_) => format!("={}", self.formula_source(a).unwrap()),
            CellInput::Value(Value::Text(t)) => {
                if t.starts_with('=') || t.starts_with('\'') || classify_literal(t).is_some() {
                    format!("'{t}")
                } else {
                    t.to_string()
                }
            }
            CellInput::Value(Value::Number(n)) => edit_number(*n, self.format(a)),
            CellInput::Value(v) => v.display(),
        }
    }

    /// The input of `src` as it would read if moved by (dr, dc): relative
    /// references in formulas shift, everything else is copied verbatim.
    pub fn input_text_shifted(&self, src: Addr, dr: i64, dc: i64) -> String {
        match self.cell(src).map(|c| &c.input) {
            Some(CellInput::Formula(e)) => format!("={}", e.to_source(dr, dc, &self.sheets)),
            _ => self.input_text(src),
        }
    }

    pub fn snapshot(&self, a: Addr) -> CellSnapshot {
        CellSnapshot { input: self.input_text(a), format: self.format_code(a).map(str::to_string), style: self.style(a).clone() }
    }

    /// Snapshot of `src` adjusted for placement at an offset (copy/fill).
    pub fn snapshot_shifted(&self, src: Addr, dr: i64, dc: i64) -> CellSnapshot {
        CellSnapshot { input: self.input_text_shifted(src, dr, dc), format: self.format_code(src).map(str::to_string), style: self.style(src).clone() }
    }

    // ------------------------------------------------------------ writing

    fn parse(&self, raw: &str) -> Result<Parsed, ParseError> {
        if raw.trim().is_empty() {
            return Ok(Parsed::Clear);
        }
        if let Some(body) = raw.strip_prefix('=')
            && !body.trim().is_empty()
        {
            let expr = parse_formula(body, &self.sheets)?;
            let fmt = match &expr {
                Expr::Call(Func::Today | Func::Date | Func::EDate | Func::EOMonth, _) => Some("m/d/yyyy"),
                Expr::Call(Func::Now, _) => Some("m/d/yyyy h:mm"),
                Expr::Call(Func::Time, _) => Some("h:mm AM/PM"),
                _ => None,
            };
            return Ok(Parsed::Input(CellInput::Formula(Arc::new(expr)), fmt));
        }
        if let Some(text) = raw.strip_prefix('\'') {
            return Ok(Parsed::Input(CellInput::Value(Value::text(text)), None));
        }
        Ok(match classify_literal(raw) {
            Some(l) => Parsed::Input(CellInput::Value(l.value), l.format),
            None => Parsed::Input(CellInput::Value(Value::text(raw)), None),
        })
    }

    /// Apply an auto-detected format unless the cell already has a
    /// compatible one (a typed date replaces a non-date format).
    fn auto_format(&mut self, a: Addr, fmt: Option<&'static str>) {
        let Some(code) = fmt else { return };
        let replace = match self.format(a) {
            None => true,
            Some(cur) => cur.is_general() || (NumberFormat::parse(code).is_date() && !cur.is_date()),
        };
        if replace {
            self.set_format(a, Some(code));
        }
    }

    /// Set one cell from raw user input and recalculate incrementally.
    pub fn set_input(&mut self, a: Addr, raw: &str) -> Result<RecalcStats, ParseError> {
        let Parsed::Input(input, fmt) = self.parse(raw)? else {
            return Ok(self.set_inputs([(a, String::new())]));
        };
        let mut seeds = Vec::with_capacity(1);
        let mut forced = FxHashSet::default();
        self.auto_format(a, fmt);
        self.apply(a, Some(input), &mut seeds, &mut forced);
        Ok(self.propagate(seeds, forced))
    }

    /// Set many cells at once (paste, fill, CSV) with a single recalc pass.
    /// Formulas that fail to parse are stored as text.
    pub fn set_inputs(&mut self, items: impl IntoIterator<Item = (Addr, String)>) -> RecalcStats {
        let mut seeds = Vec::new();
        let mut forced = FxHashSet::default();
        let mut count = 0usize;
        for (a, raw) in items {
            if self.sheets.get(a.sheet).is_none() {
                continue;
            }
            let input = match self.parse(&raw) {
                Ok(Parsed::Clear) => None,
                Ok(Parsed::Input(input, fmt)) => {
                    self.auto_format(a, fmt);
                    Some(input)
                }
                Err(_) => Some(CellInput::Value(Value::text(raw))),
            };
            self.apply(a, input, &mut seeds, &mut forced);
            count += 1;
        }
        // Big batches (loads, huge pastes) are cheaper as one full recalc than
        // as an incremental walk from every changed cell.
        if count >= 10_000 && count * 4 >= self.graph.precedents.len() {
            return self.recalc_all();
        }
        self.propagate(seeds, forced)
    }

    /// Restore exact snapshots (formats first, no auto-formatting).
    pub fn restore(&mut self, items: Vec<(Addr, CellSnapshot)>) -> RecalcStats {
        let mut seeds = Vec::new();
        let mut forced = FxHashSet::default();
        for (a, snap) in items {
            if self.sheets.get(a.sheet).is_none() {
                continue;
            }
            self.set_format(a, snap.format.as_deref());
            self.set_style(a, &snap.style);
            let input = match self.parse(&snap.input) {
                Ok(Parsed::Clear) => None,
                Ok(Parsed::Input(input, _)) => Some(input),
                Err(_) => Some(CellInput::Value(Value::text(snap.input))),
            };
            self.apply(a, input, &mut seeds, &mut forced);
        }
        self.propagate(seeds, forced)
    }

    /// Set (or with None, clear) a cell's number format. No recalc needed.
    pub fn set_format(&mut self, a: Addr, code: Option<&str>) {
        let id = code.filter(|c| !c.is_empty() && !c.eq_ignore_ascii_case("General")).map(|c| self.formats.intern(c));
        if let Some(s) = self.sheets.get_mut(a.sheet) {
            match id {
                Some(id) => {
                    s.formats.insert(a.cell, id);
                }
                None => {
                    s.formats.remove(&a.cell);
                }
            }
        }
    }

    pub fn style(&self, a: Addr) -> &Style {
        match self.sheets.get(a.sheet).and_then(|s| s.styles.get(&a.cell)) {
            Some(id) => &self.styles.list[id.0 as usize],
            None => &DEFAULT_STYLE,
        }
    }

    /// Set a cell's style (the default style clears it). No recalc needed.
    pub fn set_style(&mut self, a: Addr, style: &Style) {
        let id = (!style.is_default()).then(|| self.styles.intern(style));
        if let Some(s) = self.sheets.get_mut(a.sheet) {
            match id {
                Some(id) => {
                    s.styles.insert(a.cell, id);
                }
                None => {
                    s.styles.remove(&a.cell);
                }
            }
        }
    }

    /// Every cell with a non-default style (including empty ones).
    pub fn styled_cells(&self, sheet: SheetId) -> impl Iterator<Item = (CellRef, &Style)> + '_ {
        self.sheets.get(sheet).into_iter().flat_map(move |s| s.styles.iter().map(move |(c, id)| (*c, &self.styles.list[id.0 as usize])))
    }

    /// Every cell with an explicit number format (including empty ones).
    pub fn formatted_cells(&self, sheet: SheetId) -> impl Iterator<Item = (CellRef, &str)> + '_ {
        self.sheets.get(sheet).into_iter().flat_map(move |s| s.formats.iter().map(move |(c, id)| (*c, self.formats.get(*id).code())))
    }

    pub fn col_width(&self, sheet: SheetId, col: u32) -> Option<f64> {
        self.sheets.get(sheet)?.col_widths.get(&col).copied()
    }

    pub fn col_widths(&self, sheet: SheetId) -> &BTreeMap<u32, f64> {
        static EMPTY: BTreeMap<u32, f64> = BTreeMap::new();
        self.sheets.get(sheet).map_or(&EMPTY, |s| &s.col_widths)
    }

    /// Column width in pixels; None resets to the default.
    pub fn set_col_width(&mut self, sheet: SheetId, col: u32, px: Option<f64>) {
        if let Some(s) = self.sheets.get_mut(sheet) {
            match px {
                Some(w) => {
                    s.col_widths.insert(col, w);
                }
                None => {
                    s.col_widths.remove(&col);
                }
            }
        }
    }

    // ------------------------------------------------------------ loading

    /// Put a cell read from a file in place without recalculating. Formulas
    /// we can't parse or evaluate are kept as opaque text with their cached
    /// value. Call [`Workbook::recalc_all`] after loading everything.
    pub fn load_cell(&mut self, a: Addr, cell: LoadedCell) {
        let (input, value) = match cell {
            LoadedCell::Value(v) => (CellInput::Value(v.clone()), v),
            LoadedCell::Formula { src, cached } => match parse_formula(&src, &self.sheets) {
                // `=#N/A` is just a constant (writers use it for error values).
                Ok(Expr::Error(e)) => (CellInput::Value(Value::Error(e)), Value::Error(e)),
                Ok(e) if !e.has_unknown_func() => {
                    let e = Arc::new(e);
                    self.graph.add(a, &e);
                    (CellInput::Formula(e), cached)
                }
                _ => (CellInput::Opaque(src.into()), cached),
            },
        };
        if let Some(s) = self.sheets.get_mut(a.sheet) {
            s.store.insert(a.cell, Cell { input, value });
        }
    }

    /// Recompute every formula from scratch.
    ///
    /// Orders formulas with a precedent-first DFS (formula cells inside range
    /// references are found through a per-column index of formula rows), then
    /// evaluates each exactly once. Cycles fall back to the incremental path,
    /// which marks exactly the cycle members.
    pub fn recalc_all(&mut self) -> RecalcStats {
        let start = Instant::now();
        let mut formulas = Vec::new();
        // Formula rows per (sheet, column), ascending (store iteration is
        // column-major with rows in order).
        let mut rows: FxHashMap<(SheetId, u32), Vec<u32>> = FxHashMap::default();
        for &id in &self.sheets.order {
            for (c, cell) in self.sheets.store(id).unwrap().iter() {
                if cell.is_formula() {
                    formulas.push(Addr::new(id, c));
                    rows.entry((id, c.col)).or_default().push(c.row);
                }
            }
        }
        let n = formulas.len();
        let index: FxHashMap<Addr, u32> = formulas.iter().enumerate().map(|(i, a)| (*a, i as u32)).collect();
        // 0 = unvisited, 1 = on the DFS stack, 2 = ordered.
        let mut state = vec![0u8; n];
        let mut order: Vec<u32> = Vec::with_capacity(n);
        let mut arena: Vec<u32> = Vec::new();
        let mut stack: Vec<(u32, usize, usize)> = Vec::new();
        let deps_of = |i: u32, arena: &mut Vec<u32>| {
            let at = formulas[i as usize];
            let Some(p) = self.graph.precedents.get(&at) else { return };
            for c in &p.cells {
                if let Some(&j) = index.get(c) {
                    arena.push(j);
                }
            }
            for (s, r) in &p.ranges {
                for col in r.start.col..=r.end.col {
                    let Some(v) = rows.get(&(*s, col)) else { continue };
                    let lo = v.partition_point(|&row| row < r.start.row);
                    let hi = v.partition_point(|&row| row <= r.end.row);
                    for &row in &v[lo..hi] {
                        arena.push(index[&Addr::new(*s, CellRef::new(row, col))]);
                    }
                }
            }
        };
        for root in 0..n as u32 {
            if state[root as usize] != 0 {
                continue;
            }
            state[root as usize] = 1;
            let s0 = arena.len();
            deps_of(root, &mut arena);
            stack.push((root, s0, arena.len()));
            while let Some(top) = stack.last_mut() {
                if top.1 < top.2 {
                    let d = arena[top.1];
                    top.1 += 1;
                    match state[d as usize] {
                        0 => {
                            state[d as usize] = 1;
                            let s = arena.len();
                            deps_of(d, &mut arena);
                            let e = arena.len();
                            stack.push((d, s, e));
                        }
                        1 => {
                            // Cycle: the incremental path marks exactly its members.
                            let forced = formulas.iter().copied().collect();
                            return self.propagate(formulas, forced);
                        }
                        _ => {}
                    }
                } else {
                    let (node, s, _) = stack.pop().unwrap();
                    arena.truncate(s);
                    state[node as usize] = 2;
                    order.push(node);
                }
            }
        }
        for &i in &order {
            let a = formulas[i as usize];
            if let Some(v) = self.eval_cell(a) {
                self.set_value(a, v);
            }
        }
        RecalcStats { dirty: n, evaluated: n, elapsed: start.elapsed() }
    }

    // ------------------------------------------------------------ internals

    fn apply(&mut self, a: Addr, input: Option<CellInput>, seeds: &mut Vec<Addr>, forced: &mut FxHashSet<Addr>) {
        self.graph.remove(a);
        forced.remove(&a);
        let Some(sheet) = self.sheets.get_mut(a.sheet) else { return };
        let old_value = sheet.store.get(a.cell).map(|c| c.value.clone()).unwrap_or_default();
        match input {
            None => {
                sheet.store.remove(a.cell);
                if !old_value.is_empty() {
                    seeds.push(a);
                }
            }
            Some(CellInput::Value(v)) => {
                let changed = v != old_value;
                sheet.store.insert(a.cell, Cell { input: CellInput::Value(v.clone()), value: v });
                if changed {
                    seeds.push(a);
                }
            }
            Some(CellInput::Opaque(src)) => {
                sheet.store.insert(a.cell, Cell { input: CellInput::Opaque(src), value: old_value });
            }
            Some(CellInput::Formula(expr)) => {
                sheet.store.insert(a.cell, Cell { input: CellInput::Formula(expr.clone()), value: old_value });
                self.graph.add(a, &expr);
                seeds.push(a);
                forced.insert(a);
            }
        }
    }

    fn eval_cell(&self, a: Addr) -> Option<Value> {
        let cell = self.sheets.store(a.sheet)?.get(a.cell)?;
        let CellInput::Formula(expr) = &cell.input else { return None };
        Some(eval(expr, &Ctx { sheets: &self.sheets, sheet: a.sheet }))
    }

    fn set_value(&mut self, a: Addr, v: Value) -> bool {
        match self.sheets.get_mut(a.sheet).and_then(|s| s.store.get_mut(a.cell)) {
            Some(cell) if cell.value != v => {
                cell.value = v;
                true
            }
            _ => false,
        }
    }

    fn propagate(&mut self, seeds: Vec<Addr>, mut needs: FxHashSet<Addr>) -> RecalcStats {
        let start = Instant::now();
        let changed_seeds: FxHashSet<Addr> = seeds.iter().copied().filter(|c| !needs.contains(c)).collect();
        needs.extend(self.graph.volatile.iter().copied());

        // 1. Collect the dirty subgraph with its adjacency.
        let mut adj: FxHashMap<Addr, SmallVec<[Addr; 4]>> = FxHashMap::default();
        let mut stack: Vec<Addr> = seeds;
        stack.extend(self.graph.volatile.iter().copied());
        while let Some(c) = stack.pop() {
            if adj.contains_key(&c) {
                continue;
            }
            let mut deps = SmallVec::new();
            self.graph.for_each_dependent(c, |d| deps.push(d));
            stack.extend(deps.iter().copied().filter(|d| !adj.contains_key(d)));
            adj.insert(c, deps);
        }

        // 2. In-degrees within the dirty subgraph.
        let mut indeg: FxHashMap<Addr, u32> = adj.keys().map(|&c| (c, 0)).collect();
        for deps in adj.values() {
            for d in deps {
                *indeg.get_mut(d).unwrap() += 1;
            }
        }
        for c in &changed_seeds {
            needs.extend(adj[c].iter().copied());
        }

        // 3. Kahn's algorithm with early cutoff.
        let mut queue: VecDeque<Addr> = indeg.iter().filter(|&(_, &n)| n == 0).map(|(&c, _)| c).collect();
        let mut evaluated = 0;
        let mut processed = 0;
        let mut cycles_resolved = false;
        loop {
            while let Some(c) = queue.pop_front() {
                processed += 1;
                // A dirty #CIRC! cell must re-evaluate even if its inputs are
                // unchanged: the cycle that caused it may have been broken.
                let circ = *self.value(c) == Value::Error(ErrorKind::Circ);
                if (needs.contains(&c) || circ)
                    && let Some(v) = self.eval_cell(c)
                {
                    evaluated += 1;
                    if self.set_value(c, v) {
                        needs.extend(adj[&c].iter().copied());
                    }
                }
                for d in &adj[&c] {
                    let n = indeg.get_mut(d).unwrap();
                    *n -= 1;
                    if *n == 0 {
                        queue.push_back(*d);
                    }
                }
            }
            if processed == adj.len() || cycles_resolved {
                break;
            }
            // 4. Mark exactly the cycle members, then release their dependents.
            cycles_resolved = true;
            let stuck: Vec<Addr> = indeg.iter().filter(|&(_, &n)| n > 0).map(|(&c, _)| c).collect();
            let cyclic = cycle_members(&stuck, &adj, &indeg);
            for &c in &cyclic {
                processed += 1;
                if self.cell(c).is_some_and(Cell::is_formula) {
                    self.set_value(c, Value::Error(ErrorKind::Circ));
                }
                needs.extend(adj[&c].iter().copied());
            }
            for &c in &cyclic {
                for d in &adj[&c] {
                    if cyclic.contains(d) {
                        continue;
                    }
                    let n = indeg.get_mut(d).unwrap();
                    *n -= 1;
                    if *n == 0 {
                        queue.push_back(*d);
                    }
                }
            }
        }

        RecalcStats { dirty: adj.len(), evaluated, elapsed: start.elapsed() }
    }
}

/// Number as shown in the formula bar for editing, honoring date/percent formats.
fn edit_number(n: f64, fmt: Option<&NumberFormat>) -> String {
    match fmt {
        Some(f) if f.is_date() => {
            let whole_day = n.fract() == 0.0;
            let code = if !f.has_date() && n < 1.0 {
                "h:mm:ss"
            } else if whole_day {
                "m/d/yyyy"
            } else {
                "m/d/yyyy h:mm:ss"
            };
            NumberFormat::parse(code).format(&Value::Number(n)).text
        }
        Some(f) if f.is_percent() => format!("{}%", precise(n * 100.0)),
        _ => precise(n),
    }
}

/// Up to 15 significant digits (Excel's precision), shortest form.
pub fn precise(n: f64) -> String {
    if n == 0.0 {
        return "0".into();
    }
    let r: f64 = format!("{n:.14e}").parse().unwrap_or(n);
    let a = r.abs();
    if !(1e-9..1e15).contains(&a) { format!("{r:E}") } else { format!("{r}") }
}

/// Cells among `nodes` that lie on a cycle: members of a strongly connected
/// component of size > 1, or with a self-edge (iterative Tarjan).
fn cycle_members(nodes: &[Addr], adj: &FxHashMap<Addr, SmallVec<[Addr; 4]>>, indeg: &FxHashMap<Addr, u32>) -> FxHashSet<Addr> {
    let in_set = |c: &Addr| indeg.get(c).is_some_and(|&n| n > 0);
    let mut index: FxHashMap<Addr, usize> = FxHashMap::default();
    let mut low: FxHashMap<Addr, usize> = FxHashMap::default();
    let mut on_stack: FxHashSet<Addr> = FxHashSet::default();
    let mut stack: Vec<Addr> = Vec::new();
    let mut out = FxHashSet::default();
    let mut next = 0;
    for &root in nodes {
        if index.contains_key(&root) {
            continue;
        }
        let mut call: Vec<(Addr, usize)> = vec![(root, 0)];
        index.insert(root, next);
        low.insert(root, next);
        next += 1;
        stack.push(root);
        on_stack.insert(root);
        while let Some(&(v, i)) = call.last() {
            let succ = &adj[&v];
            if i < succ.len() {
                call.last_mut().unwrap().1 += 1;
                let w = succ[i];
                if !in_set(&w) {
                    continue;
                }
                if let std::collections::hash_map::Entry::Vacant(e) = index.entry(w) {
                    e.insert(next);
                    low.insert(w, next);
                    next += 1;
                    stack.push(w);
                    on_stack.insert(w);
                    call.push((w, 0));
                } else if on_stack.contains(&w) {
                    let lw = index[&w].min(low[&v]);
                    low.insert(v, lw);
                }
            } else {
                call.pop();
                if let Some(&(p, _)) = call.last() {
                    let lp = low[&p].min(low[&v]);
                    low.insert(p, lp);
                }
                if low[&v] == index[&v] {
                    let mut comp = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack.remove(&w);
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    if comp.len() > 1 || adj[&v].contains(&v) {
                        out.extend(comp);
                    }
                }
            }
        }
    }
    out
}
