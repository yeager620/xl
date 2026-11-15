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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::address::{Addr, CellRef, Range, SheetId};
use crate::eval::{Ctx, Func, eval};
use crate::format::{Formatted, NumberFormat};
use crate::formula::{Expr, Move, ParseError, SheetNames, Shift, parse_formula};
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
    layout: SheetLayout,
}

/// Row/column sizes and visibility, merged cells and frozen panes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SheetLayout {
    /// Custom column widths in pixels.
    pub col_widths: BTreeMap<u32, f64>,
    /// Custom row heights in pixels.
    pub row_heights: BTreeMap<u32, f64>,
    pub hidden_rows: BTreeSet<u32>,
    pub hidden_cols: BTreeSet<u32>,
    pub merges: Vec<Range>,
    /// Frozen (rows, columns) at the top-left.
    pub freeze: (u32, u32),
    pub filter: Option<AutoFilter>,
    /// Rows hidden by the filter (separate from rows hidden by hand).
    pub filtered_rows: BTreeSet<u32>,
    /// Conditional formats, highest priority first.
    pub cond: Vec<crate::cond::CondFormat>,
    /// Charts, back to front.
    pub charts: Vec<crate::chart::Chart>,
    pub sparklines: Vec<crate::chart::Sparkline>,
    pub tables: Vec<Table>,
    /// Pivot tables whose output is on this sheet.
    pub pivots: Vec<crate::pivot::Pivot>,
    /// Notes (Excel's comments) on cells.
    pub notes: BTreeMap<CellRef, Note>,
    /// Hyperlinks on cells.
    pub links: BTreeMap<CellRef, Link>,
    /// Outline (grouping) level of grouped rows and columns, from 1.
    pub row_levels: BTreeMap<u32, u8>,
    pub col_levels: BTreeMap<u32, u8>,
}

/// A hyperlink on a cell: a web/file address, or a place in the workbook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Url { url: String, tooltip: Option<String> },
    /// "Sheet2!A1" or a defined name.
    Place { location: String, tooltip: Option<String> },
}

impl Link {
    pub fn tooltip(&self) -> Option<&str> {
        match self {
            Link::Url { tooltip, .. } | Link::Place { tooltip, .. } => tooltip.as_deref(),
        }
    }

    pub fn target(&self) -> &str {
        match self {
            Link::Url { url, .. } => url,
            Link::Place { location, .. } => location,
        }
    }
}

/// A note attached to a cell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Note {
    pub author: String,
    pub text: String,
}

/// A table ("Format as Table"): a named range whose first row holds the
/// column names (read live from the cells), with optional totals row.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub name: String,
    pub range: Range,
    pub totals: bool,
    pub style: TableStyle,
    pub banded: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TableStyle {
    #[default]
    Blue,
    Green,
    Orange,
    Gray,
}

impl Table {
    /// Rows holding data (between the header and any totals row).
    pub fn data_rows(&self) -> std::ops::RangeInclusive<u32> {
        self.range.start.row + 1..=self.range.end.row - self.totals as u32
    }
}

/// A filter on a range whose first row holds the headers.
#[derive(Clone, Debug, PartialEq)]
pub struct AutoFilter {
    pub range: Range,
    /// Column → the displayed values to show ("" stands for blanks). Columns
    /// without an entry show everything.
    pub columns: BTreeMap<u32, BTreeSet<String>>,
}

impl SheetLayout {
    /// The merged range containing `c`, if any.
    pub fn merge_at(&self, c: CellRef) -> Option<Range> {
        self.merges.iter().copied().find(|m| m.contains(c))
    }

    fn apply_shift(&mut self, op: Shift, sheet: SheetId) {
        self.cond.retain_mut(|c| c.apply_shift(op, sheet));
        let rows = matches!(op, Shift::InsertRows { .. } | Shift::DeleteRows { .. });
        let shift_keys = |m: &mut BTreeMap<u32, f64>| *m = std::mem::take(m).into_iter().filter_map(|(k, v)| Some((op.map_pos(k)?, v))).collect();
        let shift_set = |m: &mut BTreeSet<u32>| *m = std::mem::take(m).into_iter().filter_map(|k| op.map_pos(k)).collect();
        let shift_levels = |m: &mut BTreeMap<u32, u8>| *m = std::mem::take(m).into_iter().filter_map(|(k, v)| Some((op.map_pos(k)?, v))).collect();
        if rows {
            shift_keys(&mut self.row_heights);
            shift_set(&mut self.hidden_rows);
            shift_levels(&mut self.row_levels);
        } else {
            shift_keys(&mut self.col_widths);
            shift_set(&mut self.hidden_cols);
            shift_levels(&mut self.col_levels);
        }
        self.merges = self.merges.iter().filter_map(|m| op.map_range(*m)).filter(|m| m.cell_count() > 1).collect();
        let move_cell = |c: CellRef| -> Option<CellRef> { Some(if rows { CellRef::new(op.map_pos(c.row)?, c.col) } else { CellRef::new(c.row, op.map_pos(c.col)?) }) };
        self.notes = std::mem::take(&mut self.notes).into_iter().filter_map(|(c, n)| Some((move_cell(c)?, n))).collect();
        self.links = std::mem::take(&mut self.links).into_iter().filter_map(|(c, n)| Some((move_cell(c)?, n))).collect();
        self.tables.retain_mut(|t| match op.map_range(t.range) {
            // A table keeps its header row and at least one data row.
            Some(r) if r.rows() > 1 => {
                t.range = r;
                true
            }
            _ => false,
        });
        if rows {
            shift_set(&mut self.filtered_rows);
        }
        self.filter = self.filter.take().and_then(|mut f| {
            f.range = op.map_range(f.range)?;
            if !rows {
                f.columns = std::mem::take(&mut f.columns).into_iter().filter_map(|(c, v)| Some((op.map_pos(c)?, v))).collect();
            }
            Some(f)
        });
        // Lines inserted or deleted inside the frozen area grow or shrink it.
        let frozen = if rows { &mut self.freeze.0 } else { &mut self.freeze.1 };
        if *frozen > 0 {
            *frozen = match op.map_span_pub(0, *frozen - 1) {
                Some((_, last)) => last + 1,
                None => 0,
            };
        }
    }
}

/// All sheets, addressable by stable id, plus their display order.
#[derive(Default)]
pub struct Sheets {
    slots: Vec<Option<SheetData>>,
    order: Vec<SheetId>,
}

impl Sheets {
    /// The table a structured reference names (or, without a name, the one
    /// containing `cell`), and the part of it the reference covers.
    pub(crate) fn resolve_table(&self, sr: &crate::formula::StructRef, sheet: SheetId, cell: CellRef) -> Result<(SheetId, Range), ErrorKind> {
        use crate::formula::TableArea;
        let found = self.order.iter().filter_map(|&id| self.get(id)).find_map(|sd| {
            sd.layout.tables.iter().find(|t| match &sr.table {
                Some(name) => t.name.eq_ignore_ascii_case(name),
                None => sd.id == sheet && t.range.contains(cell),
            }).map(|t| (sd.id, t))
        });
        let (id, t) = found.ok_or(ErrorKind::Ref)?;
        let r = t.range;
        let (r0, r1) = match sr.area {
            TableArea::All => (r.start.row, r.end.row),
            TableArea::Data => (*t.data_rows().start(), *t.data_rows().end()),
            TableArea::Headers => (r.start.row, r.start.row),
            TableArea::Totals if t.totals => (r.end.row, r.end.row),
            TableArea::Totals => return Err(ErrorKind::Ref),
            TableArea::ThisRow if id == sheet && t.data_rows().contains(&cell.row) => (cell.row, cell.row),
            TableArea::ThisRow => return Err(ErrorKind::Value),
        };
        let (c0, c1) = match &sr.cols {
            None => (r.start.col, r.end.col),
            Some((a, b)) => {
                let store = self.store(id).ok_or(ErrorKind::Ref)?;
                let find = |name: &str| (r.start.col..=r.end.col).find(|&c| store.value(CellRef::new(r.start.row, c)).display().eq_ignore_ascii_case(name.trim()));
                let (ca, cb) = (find(a).ok_or(ErrorKind::Ref)?, find(b).ok_or(ErrorKind::Ref)?);
                (ca.min(cb), ca.max(cb))
            }
        };
        Ok((id, Range::new(CellRef::new(r0, c0), CellRef::new(r1, c1))))
    }

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

/// (first row, last row, dependent) for a range covering one column.
type Band = (u32, u32, Addr);

const BLOCK_ROWS: u32 = 1024;
/// Ranges spanning more blocks than this go in the column's `tall` list.
const MAX_BLOCKS: u32 = 16;

/// Ranges touching one column, bucketed by 1024-row block so a lookup only
/// scans ranges near the changed row.
#[derive(Default)]
struct ColumnBands {
    blocks: FxHashMap<u32, Vec<Band>>,
    tall: Vec<Band>,
}

impl ColumnBands {
    fn blocks_of(r0: u32, r1: u32) -> Option<std::ops::RangeInclusive<u32>> {
        let (b0, b1) = (r0 / BLOCK_ROWS, r1 / BLOCK_ROWS);
        (b1 - b0 < MAX_BLOCKS).then_some(b0..=b1)
    }

    fn add(&mut self, band: Band) {
        match Self::blocks_of(band.0, band.1) {
            Some(blocks) => blocks.for_each(|b| self.blocks.entry(b).or_default().push(band)),
            None => self.tall.push(band),
        }
    }

    fn remove(&mut self, band: Band) {
        let take = |v: &mut Vec<Band>| {
            if let Some(i) = v.iter().position(|e| *e == band) {
                v.swap_remove(i);
            }
        };
        match Self::blocks_of(band.0, band.1) {
            Some(blocks) => {
                for b in blocks {
                    if let Some(v) = self.blocks.get_mut(&b) {
                        take(v);
                        if v.is_empty() {
                            self.blocks.remove(&b);
                        }
                    }
                }
            }
            None => take(&mut self.tall),
        }
    }

    fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.tall.is_empty()
    }

    fn query(&self, row: u32, f: &mut impl FnMut(Addr)) {
        let hits = |v: &Vec<Band>, f: &mut dyn FnMut(Addr)| {
            for &(r0, r1, dep) in v {
                if row >= r0 && row <= r1 {
                    f(dep);
                }
            }
        };
        if let Some(v) = self.blocks.get(&(row / BLOCK_ROWS)) {
            hits(v, f);
        }
        hits(&self.tall, f);
    }
}

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
                self.by_col.entry((sheet, c)).or_default().add((r.start.row, r.end.row, dep));
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
                if let Some(bands) = self.by_col.get_mut(&(sheet, c)) {
                    bands.remove((r.start.row, r.end.row, dep));
                    if bands.is_empty() {
                        self.by_col.remove(&(sheet, c));
                    }
                }
            }
        }
    }

    fn query(&self, a: Addr, f: &mut impl FnMut(Addr)) {
        if let Some(bands) = self.by_col.get(&(a.sheet, a.cell.col)) {
            bands.query(a.cell.row, f);
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

/// Every reference an expression depends on, following defined names.
fn collect_refs(expr: &Expr, sheets: &Sheets, names: &Names, at: Addr, depth: u8, out: &mut impl FnMut(SheetId, Range)) {
    expr.visit_refs(&mut |sheet, r| out(sheet.unwrap_or(at.sheet), r));
    // Structured references depend on the table's current extent.
    expr.visit_tables(&mut |t| {
        if let Ok((sheet, r)) = sheets.resolve_table(t, at.sheet, at.cell) {
            out(sheet, r);
        }
    });
    if depth < 16 {
        expr.visit_names(&mut |n| {
            if let Some(def) = names.lookup(n, at.sheet) {
                collect_refs(&def.expr, sheets, names, at, depth + 1, out);
            }
        });
    }
}

impl DepGraph {
    fn add(&mut self, at: Addr, expr: &Expr, sheets: &Sheets, names: &Names) {
        let mut p = Precedents::default();
        collect_refs(expr, sheets, names, at, 0, &mut |sheet, r| {
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

// -------------------------------------------------------------------- names

/// A defined name: `TaxRate` → `Rates!$B$2`, workbook-wide or local to a sheet.
#[derive(Clone, Debug)]
pub struct DefinedName {
    pub name: String,
    /// None = workbook scope.
    pub scope: Option<SheetId>,
    pub expr: Arc<Expr>,
}

#[derive(Default, Clone)]
pub struct Names {
    list: Vec<DefinedName>,
}

impl Names {
    /// Sheet-local names take precedence over workbook-wide ones.
    pub fn lookup(&self, name: &str, sheet: SheetId) -> Option<&DefinedName> {
        let matches = |d: &&DefinedName| d.name.eq_ignore_ascii_case(name);
        self.list.iter().filter(matches).find(|d| d.scope == Some(sheet)).or_else(|| self.list.iter().filter(matches).find(|d| d.scope.is_none()))
    }

    pub fn iter(&self) -> impl Iterator<Item = &DefinedName> {
        self.list.iter()
    }
}

/// Excel's rules for defined names.
pub fn validate_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let first = chars.next().ok_or("A name can't be empty")?;
    if !(first.is_alphabetic() || first == '_' || first == '\\') {
        return Err("A name must start with a letter or underscore".into());
    }
    if let Some(c) = name.chars().find(|c| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '\\'))) {
        return Err(format!("A name can't contain '{c}'"));
    }
    let upper = name.to_ascii_uppercase();
    let r1c1 = upper.trim_start_matches('R').trim_start_matches(|c: char| c.is_ascii_digit()).trim_start_matches('C').chars().all(|c| c.is_ascii_digit())
        && (upper.starts_with('R') || upper.starts_with('C'));
    if crate::address::parse_a1(name).is_some() || r1c1 || upper == "TRUE" || upper == "FALSE" {
        return Err(format!("'{name}' looks like a cell reference; pick another name"));
    }
    if name.len() > 255 {
        return Err("Names are limited to 255 characters".into());
    }
    Ok(())
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
    names: Names,
    /// Per sheet, how many structural edits (inserted/deleted lines, moved
    /// cells) it has had: tells savers when cell addresses have shifted.
    structure: FxHashMap<SheetId, u64>,
    /// Every structural edit in order, for savers that patch files written
    /// before them.
    structure_log: Vec<StructOp>,
    /// The file this came from counts dates from 1904 (old Mac Excel). xl
    /// always works in the 1900 system; savers convert back.
    date1904: bool,
}

/// Days between the 1900 and 1904 date systems' day zero.
pub const DATE1904_OFFSET: f64 = 1462.0;

/// A structural edit, as logged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StructOp {
    Shift(Shift),
    Move(Move),
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

    pub fn date1904(&self) -> bool {
        self.date1904
    }

    /// Mark the workbook as read from a 1904-based file, converting its
    /// date-formatted constants (not pure times) to the 1900 system.
    pub fn convert_from_1904(&mut self) {
        self.date1904 = true;
        for id in self.sheets.order.clone() {
            let Some(sd) = self.sheets.get(id) else { continue };
            let dates: Vec<CellRef> = sd
                .store
                .iter()
                .filter(|(c, cell)| matches!(cell.input, CellInput::Value(Value::Number(n)) if n >= 1.0) && sd.formats.get(c).is_some_and(|f| self.formats.get(*f).has_date()))
                .map(|(c, _)| c)
                .collect();
            let Some(sd) = self.sheets.get_mut(id) else { continue };
            for c in dates {
                if let Some(cell) = sd.store.get_mut(c)
                    && let CellInput::Value(Value::Number(n)) = cell.input
                {
                    cell.input = CellInput::Value(Value::Number(n + DATE1904_OFFSET));
                    cell.value = Value::Number(n + DATE1904_OFFSET);
                }
            }
        }
    }

    /// All structural edits so far, oldest first.
    pub fn structure_log(&self) -> &[StructOp] {
        &self.structure_log
    }

    /// Structural edits so far on a sheet (see `structure`).
    pub fn structure_version(&self, sheet: SheetId) -> u64 {
        self.structure.get(&sheet).copied().unwrap_or(0)
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
        self.sheets.slots.push(Some(SheetData { id, name, store: CellStore::default(), formats: FxHashMap::default(), styles: FxHashMap::default(), layout: SheetLayout::default() }));
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

    pub fn names(&self) -> &Names {
        &self.names
    }

    /// The formula a name stands for (without '=').
    /// Evaluate an expression as if it were a formula in `cell` on `sheet`.
    pub fn eval_at(&self, sheet: SheetId, cell: CellRef, expr: &Expr) -> Value {
        crate::eval::eval(expr, &crate::eval::Ctx::new(&self.sheets, &self.names, sheet, cell))
    }

    /// Resolve "B7", "Sheet2!A1:C9" or a defined name to a sheet and range,
    /// relative to `sheet`.
    pub fn resolve_reference(&self, text: &str, sheet: SheetId) -> Option<(SheetId, Range)> {
        let expr = parse_formula(text.trim().trim_start_matches('='), &self.sheets).ok()?;
        match expr {
            Expr::Name(n) => {
                let d = self.names.lookup(&n, sheet)?;
                let (s, r) = d.expr.reference()?;
                Some((s.or(d.scope).unwrap_or(sheet), r))
            }
            e => e.reference().map(|(s, r)| (s.unwrap_or(sheet), r)),
        }
    }

    /// The defined name that refers to exactly this range, if any.
    pub fn name_for_range(&self, sheet: SheetId, range: Range) -> Option<&str> {
        self.names.iter().find(|d| d.expr.reference().is_some_and(|(s, r)| s.or(d.scope) == Some(sheet) && r == range)).map(|d| d.name.as_str())
    }

    pub fn name_formula(&self, d: &DefinedName) -> String {
        d.expr.to_source(0, 0, &self.sheets)
    }

    /// Define (or redefine) a name. `formula` is like `Rates!$B$2` or `0.0825`.
    pub fn define_name(&mut self, name: &str, scope: Option<SheetId>, formula: &str) -> Result<RecalcStats, String> {
        let name = name.trim();
        validate_name(name)?;
        let expr = parse_formula(formula.trim().trim_start_matches('='), &self.sheets).map_err(|e| e.to_string())?;
        if let Expr::Name(n) = &expr
            && n.eq_ignore_ascii_case(name)
        {
            return Err("A name can't refer to itself".into());
        }
        self.names.list.retain(|d| !(d.name.eq_ignore_ascii_case(name) && d.scope == scope));
        self.names.list.push(DefinedName { name: name.to_string(), scope, expr: Arc::new(expr) });
        Ok(self.rebuild())
    }

    /// Load a name from a file without recalculating (formulas come later).
    pub fn load_name(&mut self, name: &str, scope: Option<SheetId>, formula: &str) -> bool {
        match parse_formula(formula, &self.sheets) {
            Ok(expr) if validate_name(name).is_ok() => {
                self.names.list.retain(|d| !(d.name.eq_ignore_ascii_case(name) && d.scope == scope));
                self.names.list.push(DefinedName { name: name.to_string(), scope, expr: Arc::new(expr) });
                true
            }
            _ => false,
        }
    }

    pub fn delete_name(&mut self, name: &str, scope: Option<SheetId>) -> Option<RecalcStats> {
        let before = self.names.list.len();
        self.names.list.retain(|d| !(d.name.eq_ignore_ascii_case(name) && d.scope == scope));
        (self.names.list.len() != before).then(|| self.rebuild())
    }

    /// Insert or delete rows or columns. Cells, formats, styles and widths
    /// move; formulas on every sheet and defined names are rewritten (refs to
    /// deleted cells become #REF!); then everything recalculates.
    pub fn apply_shift(&mut self, op: Shift) -> RecalcStats {
        let target = op.sheet();
        let rows = matches!(op, Shift::InsertRows { .. } | Shift::DeleteRows { .. });
        let remap = |c: CellRef| -> Option<CellRef> {
            if rows { Some(CellRef::new(op.map_pos(c.row)?, c.col)) } else { Some(CellRef::new(c.row, op.map_pos(c.col)?)) }
        };
        *self.structure.entry(target).or_default() += 1;
        self.structure_log.push(StructOp::Shift(op));
        let Some(sd) = self.sheets.get_mut(target) else { return RecalcStats::default() };
        sd.store.apply_shift(op);
        sd.formats = sd.formats.drain().filter_map(|(c, f)| Some((remap(c)?, f))).collect();
        sd.styles = sd.styles.drain().filter_map(|(c, f)| Some((remap(c)?, f))).collect();
        sd.layout.apply_shift(op, target);
        for id in self.sheets.order.clone() {
            let Some(sd) = self.sheets.get_mut(id) else { continue };
            for (_, cell) in sd.store.cells_mut() {
                if let CellInput::Formula(e) = &cell.input {
                    let moved = e.shifted(op, id);
                    if moved != **e {
                        cell.input = CellInput::Formula(Arc::new(moved));
                    }
                }
            }
        }
        for d in &mut self.names.list {
            d.expr = Arc::new(d.expr.shifted(op, d.scope.unwrap_or(target)));
        }
        for id in self.sheets.order.clone() {
            if let Some(sd) = self.sheets.get_mut(id) {
                for chart in &mut sd.layout.charts {
                    chart.apply_shift(op, id);
                }
                sd.layout.sparklines.retain_mut(|s| s.apply_shift(op, id));
                for p in &mut sd.layout.pivots {
                    p.apply_shift(op, id);
                }
            }
        }
        self.rebuild()
    }

    /// Cut and paste: move a block of cells (contents, formats, styles) so
    /// that every reference to it — from any sheet, from names, from inside
    /// the block — follows. References to cells it overwrites become #REF!.
    pub fn move_range(&mut self, mv: Move) -> RecalcStats {
        *self.structure.entry(mv.from_sheet).or_default() += 1;
        *self.structure.entry(mv.to_sheet).or_default() += 1;
        self.structure_log.push(StructOp::Move(mv));
        let to = mv.to_range();
        // What's moving, captured before anything is rewritten.
        let mut moving: Vec<(CellRef, Option<Cell>, Option<String>, Style)> = Vec::new();
        for r in mv.from.start.row..=mv.from.end.row {
            for c in mv.from.start.col..=mv.from.end.col {
                let cell = CellRef::new(r, c);
                let a = Addr::new(mv.from_sheet, cell);
                let input = self.cell(a).cloned();
                let (format, style) = (self.format_code(a).map(str::to_string), self.style(a).clone());
                if input.is_some() || format.is_some() || !style.is_default() {
                    moving.push((cell, input, format, style));
                }
            }
        }
        for id in self.sheets.order.clone() {
            let Some(sd) = self.sheets.get_mut(id) else { continue };
            for (_, cell) in sd.store.cells_mut() {
                if let CellInput::Formula(e) = &cell.input {
                    let moved = e.moved(&mv, id);
                    if moved != **e {
                        cell.input = CellInput::Formula(Arc::new(moved));
                    }
                }
            }
        }
        // Notes go with their cells; notes where the block lands are replaced.
        let moving_notes: Vec<(CellRef, Note)> = self.sheets.get(mv.from_sheet).map(|sd| sd.layout.notes.iter().filter(|(c, _)| mv.from.contains(**c)).map(|(c, n)| (*c, n.clone())).collect()).unwrap_or_default();
        if let Some(sd) = self.sheets.get_mut(mv.from_sheet) {
            sd.layout.notes.retain(|c, _| !mv.from.contains(*c));
        }
        let moving_links: Vec<(CellRef, Link)> = self.sheets.get(mv.from_sheet).map(|sd| sd.layout.links.iter().filter(|(c, _)| mv.from.contains(**c)).map(|(c, n)| (*c, n.clone())).collect()).unwrap_or_default();
        if let Some(sd) = self.sheets.get_mut(mv.from_sheet) {
            sd.layout.links.retain(|c, _| !mv.from.contains(*c));
        }
        if let Some(sd) = self.sheets.get_mut(mv.to_sheet) {
            let to = mv.to_range();
            let land = |c: CellRef| CellRef::new((c.row as i64 + mv.dr) as u32, (c.col as i64 + mv.dc) as u32);
            sd.layout.notes.retain(|c, _| !to.contains(*c));
            sd.layout.links.retain(|c, _| !to.contains(*c));
            for (c, n) in moving_notes {
                sd.layout.notes.insert(land(c), n);
            }
            for (c, l) in moving_links {
                sd.layout.links.insert(land(c), l);
            }
        }
        for id in self.sheets.order.clone() {
            if let Some(sd) = self.sheets.get_mut(id) {
                for chart in &mut sd.layout.charts {
                    chart.apply_move(&mv, id);
                }
                for s in &mut sd.layout.sparklines {
                    s.apply_move(&mv, id);
                }
            }
        }
        for d in &mut self.names.list {
            // Names are written as if on the first sheet; their refs carry sheets.
            let home = d.scope.unwrap_or(mv.from_sheet);
            d.expr = Arc::new(d.expr.moved(&mv, home));
        }
        let moved_input = |cell: &Option<Cell>| {
            let mut cell = cell.clone()?;
            if let CellInput::Formula(e) = &cell.input {
                cell.input = CellInput::Formula(Arc::new(e.moved(&mv, mv.from_sheet).rehomed(mv.from_sheet, mv.to_sheet)));
            }
            Some(cell)
        };
        let landed: Vec<(CellRef, Option<Cell>, Option<String>, Style)> =
            moving.iter().map(|(c, input, f, st)| (CellRef::new((c.row as i64 + mv.dr) as u32, (c.col as i64 + mv.dc) as u32), moved_input(input), f.clone(), st.clone())).collect();
        // Clear the source (where the block doesn't land on itself) and the
        // destination, then write the block.
        let mut clear: Vec<Addr> = Vec::new();
        for (c, ..) in &moving {
            clear.push(Addr::new(mv.from_sheet, *c));
        }
        if let Some(sd) = self.sheets.get(mv.to_sheet) {
            clear.extend(sd.store.iter_range(to).map(|(c, _)| Addr::new(mv.to_sheet, c)));
            clear.extend(sd.formats.keys().filter(|c| to.contains(**c)).map(|c| Addr::new(mv.to_sheet, *c)));
            clear.extend(sd.styles.keys().filter(|c| to.contains(**c)).map(|c| Addr::new(mv.to_sheet, *c)));
        }
        for a in clear {
            self.set_format(a, None);
            self.set_style(a, &Style::default());
            if let Some(sd) = self.sheets.get_mut(a.sheet) {
                sd.store.remove(a.cell);
            }
        }
        for (c, input, f, st) in landed {
            let a = Addr::new(mv.to_sheet, c);
            self.set_format(a, f.as_deref());
            self.set_style(a, &st);
            if let (Some(cell), Some(sd)) = (input, self.sheets.get_mut(mv.to_sheet)) {
                sd.store.insert(c, cell);
            }
        }
        self.rebuild()
    }

    /// Input text of every formula in the workbook (for undoing structural edits).
    pub fn formula_texts(&self) -> Vec<(Addr, String)> {
        let mut out = Vec::new();
        for &id in &self.sheets.order {
            for (c, cell) in self.sheets.store(id).unwrap().iter() {
                if cell.is_formula() {
                    let a = Addr::new(id, c);
                    out.push((a, self.input_text(a)));
                }
            }
        }
        out
    }

    /// Snapshots of every cell in `range` that holds anything (a value, a
    /// number format or a style).
    pub fn region_snapshots(&self, sheet: SheetId, range: Range) -> Vec<(Addr, CellSnapshot)> {
        let Some(sd) = self.sheets.get(sheet) else { return Vec::new() };
        let mut cells: Vec<CellRef> = sd.store.iter_range(range).map(|(c, _)| c).collect();
        cells.extend(sd.formats.keys().copied().filter(|c| range.contains(*c)));
        cells.extend(sd.styles.keys().copied().filter(|c| range.contains(*c)));
        cells.sort_unstable();
        cells.dedup();
        cells.into_iter().map(|c| (Addr::new(sheet, c), self.snapshot(Addr::new(sheet, c)))).collect()
    }

    /// Rebuild the dependency graph from scratch and recalculate everything
    /// (after structural changes: names, inserted/deleted rows, moves).
    pub fn rebuild(&mut self) -> RecalcStats {
        self.graph = DepGraph::default();
        let mut formulas = Vec::new();
        for &id in &self.sheets.order {
            for (c, cell) in self.sheets.store(id).unwrap().iter() {
                if let CellInput::Formula(e) = &cell.input {
                    formulas.push((Addr::new(id, c), e.clone()));
                }
            }
        }
        for (a, e) in formulas {
            self.graph.add(a, &e, &self.sheets, &self.names);
        }
        self.recalc_all()
    }

    /// Every cell with an explicit number format (including empty ones).
    pub fn formatted_cells(&self, sheet: SheetId) -> impl Iterator<Item = (CellRef, &str)> + '_ {
        self.sheets.get(sheet).into_iter().flat_map(move |s| s.formats.iter().map(move |(c, id)| (*c, self.formats.get(*id).code())))
    }

    pub fn layout(&self, sheet: SheetId) -> &SheetLayout {
        static EMPTY: std::sync::LazyLock<SheetLayout> = std::sync::LazyLock::new(SheetLayout::default);
        self.sheets.get(sheet).map_or(&EMPTY, |s| &s.layout)
    }

    pub fn layout_mut(&mut self, sheet: SheetId) -> Option<&mut SheetLayout> {
        self.sheets.get_mut(sheet).map(|s| &mut s.layout)
    }

    /// Merge a range. Only the top-left cell's contents are kept (the others
    /// are cleared, like Excel); merges it overlaps are absorbed.
    pub fn merge(&mut self, sheet: SheetId, range: Range) -> RecalcStats {
        if range.cell_count() < 2 {
            return RecalcStats::default();
        }
        let others: Vec<(Addr, String)> =
            self.store(sheet).iter_range(range).filter(|(c, _)| *c != range.start).map(|(c, _)| (Addr::new(sheet, c), String::new())).collect();
        let stats = self.set_inputs(others);
        if let Some(l) = self.layout_mut(sheet) {
            l.merges.retain(|m| !(m.start.row <= range.end.row && range.start.row <= m.end.row && m.start.col <= range.end.col && range.start.col <= m.end.col));
            l.merges.push(range);
        }
        stats
    }

    /// A table name not used by any table or defined name ("Table1", …).
    pub fn next_table_name(&self) -> String {
        (1..).map(|i| format!("Table{i}")).find(|n| self.table_by_name(n).is_none() && self.names.lookup(n, 0).is_none()).unwrap_or_default()
    }

    pub fn table_by_name(&self, name: &str) -> Option<(SheetId, &Table)> {
        self.sheets.order.iter().filter_map(|&id| self.sheets.get(id)).find_map(|sd| sd.layout.tables.iter().find(|t| t.name.eq_ignore_ascii_case(name)).map(|t| (sd.id, t)))
    }

    /// The table containing a cell.
    pub fn table_at(&self, sheet: SheetId, cell: CellRef) -> Option<&Table> {
        self.layout(sheet).tables.iter().find(|t| t.range.contains(cell))
    }

    /// Add a table. Its name must be a valid, unused name; it mustn't overlap
    /// another table.
    pub fn add_table(&mut self, sheet: SheetId, table: Table) -> Result<RecalcStats, String> {
        validate_name(&table.name)?;
        if self.table_by_name(&table.name).is_some() || self.names.lookup(&table.name, sheet).is_some() {
            return Err(format!("The name {} is already used", table.name));
        }
        if table.range.rows() < 2 {
            return Err("A table needs a header row and at least one row of data".into());
        }
        let overlaps = |a: Range, b: Range| a.start.row <= b.end.row && b.start.row <= a.end.row && a.start.col <= b.end.col && b.start.col <= a.end.col;
        if self.layout(sheet).tables.iter().any(|t| overlaps(t.range, table.range)) {
            return Err("Tables can't overlap".into());
        }
        let Some(l) = self.layout_mut(sheet) else { return Err("No such sheet".into()) };
        l.tables.push(table);
        Ok(self.rebuild())
    }

    /// Grow tables over data typed in the row just below them (or the column
    /// just right of them), like Excel. True if any table changed.
    pub fn grow_tables(&mut self, sheet: SheetId, edited: &[CellRef]) -> bool {
        let Some(sd) = self.sheets.get_mut(sheet) else { return false };
        let mut changed = false;
        for t in &mut sd.layout.tables {
            let r = t.range;
            let below = !t.totals && edited.iter().any(|c| c.row == r.end.row + 1 && (r.start.col..=r.end.col).contains(&c.col) && !sd.store.value(*c).is_empty());
            let right = edited.iter().any(|c| c.col == r.end.col + 1 && (r.start.row..=r.end.row).contains(&c.row) && !sd.store.value(*c).is_empty());
            if below {
                t.range.end.row += 1;
                changed = true;
            }
            if right {
                t.range.end.col += 1;
                changed = true;
            }
        }
        if changed {
            self.rebuild();
        }
        changed
    }

    /// Recompute which rows the sheet's filter hides. The range first grows
    /// down over rows of data added right below it, like Excel's.
    pub fn reapply_filter(&mut self, sheet: SheetId) {
        let Some(mut f) = self.layout(sheet).filter.clone() else {
            if let Some(l) = self.layout_mut(sheet) {
                l.filtered_rows.clear();
            }
            return;
        };
        let store = self.store(sheet);
        let row_has_data = |r: u32| (f.range.start.col..=f.range.end.col).any(|c| !store.value(CellRef::new(r, c)).is_empty());
        while f.range.end.row + 1 < crate::address::MAX_ROWS && row_has_data(f.range.end.row + 1) {
            f.range.end.row += 1;
        }
        let mut hidden = BTreeSet::new();
        for r in f.range.start.row + 1..=f.range.end.row {
            let shown = f.columns.iter().all(|(&c, allowed)| allowed.contains(&self.display(Addr::new(sheet, CellRef::new(r, c))).text));
            if !shown {
                hidden.insert(r);
            }
        }
        if let Some(l) = self.layout_mut(sheet) {
            l.filter = Some(f);
            l.filtered_rows = hidden;
        }
    }

    /// Unmerge every merged range overlapping `range`.
    pub fn unmerge(&mut self, sheet: SheetId, range: Range) {
        if let Some(l) = self.layout_mut(sheet) {
            l.merges.retain(|m| !(m.start.row <= range.end.row && range.start.row <= m.end.row && m.start.col <= range.end.col && range.start.col <= m.end.col));
        }
    }

    pub fn row_height(&self, sheet: SheetId, row: u32) -> Option<f64> {
        self.sheets.get(sheet)?.layout.row_heights.get(&row).copied()
    }

    /// Set (Some) or reset (None) a row's height in pixels.
    pub fn set_row_height(&mut self, sheet: SheetId, row: u32, h: Option<f64>) {
        if let Some(l) = self.layout_mut(sheet) {
            match h {
                Some(h) => {
                    l.row_heights.insert(row, h);
                }
                None => {
                    l.row_heights.remove(&row);
                }
            }
        }
    }

    pub fn col_width(&self, sheet: SheetId, col: u32) -> Option<f64> {
        self.sheets.get(sheet)?.layout.col_widths.get(&col).copied()
    }

    pub fn col_widths(&self, sheet: SheetId) -> &BTreeMap<u32, f64> {
        static EMPTY: BTreeMap<u32, f64> = BTreeMap::new();
        self.sheets.get(sheet).map_or(&EMPTY, |s| &s.layout.col_widths)
    }

    /// Column width in pixels; None resets to the default.
    pub fn set_col_width(&mut self, sheet: SheetId, col: u32, px: Option<f64>) {
        if let Some(s) = self.sheets.get_mut(sheet) {
            match px {
                Some(w) => {
                    s.layout.col_widths.insert(col, w);
                }
                None => {
                    s.layout.col_widths.remove(&col);
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
                    self.graph.add(a, &e, &self.sheets, &self.names);
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
                self.graph.add(a, &expr, &self.sheets, &self.names);
                seeds.push(a);
                forced.insert(a);
            }
        }
    }

    fn eval_cell(&self, a: Addr) -> Option<Value> {
        let cell = self.sheets.store(a.sheet)?.get(a.cell)?;
        let CellInput::Formula(expr) = &cell.input else { return None };
        Some(eval(expr, &Ctx::new(&self.sheets, &self.names, a.sheet, a.cell)))
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
