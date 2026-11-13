//! A sheet: cell storage + dependency graph + incremental recalculation.
//!
//! Recalc algorithm on edit:
//! 1. Walk dependents from the edited cells (plus volatile cells) to collect
//!    the dirty subgraph. Single-cell refs are direct edges; range refs are
//!    found through a column-banded range index, so `SUM(A:A)` costs one index
//!    entry rather than a million edges.
//! 2. Topologically order the dirty subgraph (Kahn's algorithm).
//! 3. Evaluate in order, but only cells whose inputs actually changed
//!    ("early cutoff"): if a recomputed value is unchanged, its dependents are
//!    skipped.
//! 4. Anything left with unresolved in-degree is part of (or downstream of) a
//!    cycle and gets `#CIRC!`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::address::{CellRef, Range};
use crate::functions::eval;
use crate::parser::{ParseError, parse_formula};
use crate::store::{Cell, CellInput, CellStore};
use crate::value::{ErrorKind, Value, parse_number};

/// Ranges wider than this many columns go in a linear "wide" list instead of
/// being registered per column.
const NARROW_COLS: u32 = 64;

#[derive(Default)]
struct RangeIndex {
    by_col: FxHashMap<u32, Vec<(u32, u32, CellRef)>>,
    wide: Vec<(Range, CellRef)>,
}

impl RangeIndex {
    fn add(&mut self, r: Range, dep: CellRef) {
        if r.cols() > NARROW_COLS {
            self.wide.push((r, dep));
        } else {
            for c in r.start.col..=r.end.col {
                self.by_col.entry(c).or_default().push((r.start.row, r.end.row, dep));
            }
        }
    }

    fn remove(&mut self, r: Range, dep: CellRef) {
        if r.cols() > NARROW_COLS {
            if let Some(i) = self.wide.iter().position(|e| *e == (r, dep)) {
                self.wide.swap_remove(i);
            }
        } else {
            for c in r.start.col..=r.end.col {
                if let Some(v) = self.by_col.get_mut(&c) {
                    if let Some(i) = v.iter().position(|e| *e == (r.start.row, r.end.row, dep)) {
                        v.swap_remove(i);
                    }
                    if v.is_empty() {
                        self.by_col.remove(&c);
                    }
                }
            }
        }
    }

    fn query(&self, c: CellRef, f: &mut impl FnMut(CellRef)) {
        if let Some(v) = self.by_col.get(&c.col) {
            for &(r0, r1, dep) in v {
                if c.row >= r0 && c.row <= r1 {
                    f(dep);
                }
            }
        }
        for (r, dep) in &self.wide {
            if r.contains(c) {
                f(*dep);
            }
        }
    }
}

#[derive(Default)]
struct Precedents {
    cells: SmallVec<[CellRef; 4]>,
    ranges: SmallVec<[Range; 2]>,
}

#[derive(Default)]
struct DepGraph {
    precedents: FxHashMap<CellRef, Precedents>,
    cell_deps: FxHashMap<CellRef, SmallVec<[CellRef; 4]>>,
    ranges: RangeIndex,
    volatile: FxHashSet<CellRef>,
}

impl DepGraph {
    fn add(&mut self, cell: CellRef, expr: &crate::parser::Expr) {
        let mut p = Precedents::default();
        expr.visit_refs(&mut |r| {
            if r.start == r.end {
                p.cells.push(r.start);
            } else {
                p.ranges.push(r);
            }
        });
        p.cells.sort_unstable();
        p.cells.dedup();
        p.ranges.dedup();
        for &c in &p.cells {
            self.cell_deps.entry(c).or_default().push(cell);
        }
        for &r in &p.ranges {
            self.ranges.add(r, cell);
        }
        if expr.is_volatile() {
            self.volatile.insert(cell);
        }
        self.precedents.insert(cell, p);
    }

    fn remove(&mut self, cell: CellRef) {
        let Some(p) = self.precedents.remove(&cell) else { return };
        for c in p.cells {
            if let Some(v) = self.cell_deps.get_mut(&c) {
                if let Some(i) = v.iter().position(|d| *d == cell) {
                    v.swap_remove(i);
                }
                if v.is_empty() {
                    self.cell_deps.remove(&c);
                }
            }
        }
        for r in p.ranges {
            self.ranges.remove(r, cell);
        }
        self.volatile.remove(&cell);
    }

    fn for_each_dependent(&self, c: CellRef, mut f: impl FnMut(CellRef)) {
        if let Some(v) = self.cell_deps.get(&c) {
            v.iter().copied().for_each(&mut f);
        }
        self.ranges.query(c, &mut f);
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RecalcStats {
    /// Cells in the dirty subgraph.
    pub dirty: usize,
    /// Formulas actually re-evaluated (after early cutoff).
    pub evaluated: usize,
    pub elapsed: Duration,
}

#[derive(Default)]
pub struct Sheet {
    store: CellStore,
    graph: DepGraph,
}

/// Interpret raw user input. `Ok(None)` means "clear the cell".
pub fn parse_input(raw: &str) -> Result<Option<CellInput>, ParseError> {
    if raw.trim().is_empty() {
        return Ok(None);
    }
    if let Some(body) = raw.strip_prefix('=')
        && !body.trim().is_empty()
    {
        let expr = parse_formula(body)?;
        return Ok(Some(CellInput::Formula { src: body.trim().into(), expr: Arc::new(expr) }));
    }
    if let Some(text) = raw.strip_prefix('\'') {
        return Ok(Some(CellInput::Value(Value::text(text))));
    }
    let v = if let Some(n) = parse_number(raw) {
        Value::Number(n)
    } else if raw.trim().eq_ignore_ascii_case("TRUE") {
        Value::Bool(true)
    } else if raw.trim().eq_ignore_ascii_case("FALSE") {
        Value::Bool(false)
    } else if let Some(e) = ErrorKind::parse(raw.trim()) {
        Value::Error(e)
    } else {
        Value::text(raw)
    };
    Ok(Some(CellInput::Value(v)))
}

impl Sheet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn store(&self) -> &CellStore {
        &self.store
    }

    pub fn value(&self, c: CellRef) -> &Value {
        self.store.value(c)
    }

    pub fn cell(&self, c: CellRef) -> Option<&Cell> {
        self.store.get(c)
    }

    /// What the user would see/edit in the formula bar.
    pub fn input_text(&self, c: CellRef) -> String {
        match self.store.get(c).map(|cell| &cell.input) {
            None => String::new(),
            Some(CellInput::Formula { src, .. }) => format!("={src}"),
            Some(CellInput::Value(Value::Text(t))) if parse_number(t).is_some() || ErrorKind::parse(t).is_some() || t.starts_with('=') => {
                format!("'{t}")
            }
            Some(CellInput::Value(v)) => v.display(),
        }
    }

    /// The input of `src` as it would read if moved by (dr, dc): relative
    /// references in formulas shift, everything else is copied verbatim.
    pub fn input_text_shifted(&self, src: CellRef, dr: i64, dc: i64) -> String {
        match self.store.get(src).map(|cell| &cell.input) {
            Some(CellInput::Formula { expr, .. }) => format!("={}", expr.to_source_shifted(dr, dc)),
            _ => self.input_text(src),
        }
    }

    /// Set one cell from raw user input and recalculate incrementally.
    pub fn set_input(&mut self, c: CellRef, raw: &str) -> Result<RecalcStats, ParseError> {
        let input = parse_input(raw)?;
        let mut seeds = Vec::with_capacity(1);
        let mut forced = FxHashSet::default();
        self.apply(c, input, &mut seeds, &mut forced);
        Ok(self.propagate(seeds, forced))
    }

    /// Set many cells at once (paste, fill, load) with a single recalc pass.
    /// Formulas that fail to parse are stored as text.
    pub fn set_inputs(&mut self, items: impl IntoIterator<Item = (CellRef, String)>) -> RecalcStats {
        let mut seeds = Vec::new();
        let mut forced = FxHashSet::default();
        for (c, raw) in items {
            let input = parse_input(&raw).unwrap_or_else(|_| Some(CellInput::Value(Value::text(raw))));
            self.apply(c, input, &mut seeds, &mut forced);
        }
        self.propagate(seeds, forced)
    }

    /// Recompute every formula from scratch.
    pub fn recalc_all(&mut self) -> RecalcStats {
        let formulas: Vec<CellRef> = self.store.iter().filter(|(_, cell)| cell.is_formula()).map(|(c, _)| c).collect();
        let forced = formulas.iter().copied().collect();
        self.propagate(formulas, forced)
    }

    fn apply(&mut self, c: CellRef, input: Option<CellInput>, seeds: &mut Vec<CellRef>, forced: &mut FxHashSet<CellRef>) {
        self.graph.remove(c);
        forced.remove(&c);
        let old_value = self.store.get(c).map(|cell| cell.value.clone()).unwrap_or_default();
        match input {
            None => {
                self.store.remove(c);
                if !old_value.is_empty() {
                    seeds.push(c);
                }
            }
            Some(CellInput::Value(v)) => {
                let changed = v != old_value;
                self.store.insert(c, Cell { input: CellInput::Value(v.clone()), value: v });
                if changed {
                    seeds.push(c);
                }
            }
            Some(input @ CellInput::Formula { .. }) => {
                if let CellInput::Formula { expr, .. } = &input {
                    self.graph.add(c, expr);
                }
                self.store.insert(c, Cell { input, value: old_value });
                seeds.push(c);
                forced.insert(c);
            }
        }
    }

    fn propagate(&mut self, seeds: Vec<CellRef>, mut needs: FxHashSet<CellRef>) -> RecalcStats {
        let start = Instant::now();
        // Seeds whose own value changed push "needs eval" onto their dependents.
        let changed_seeds: FxHashSet<CellRef> = seeds.iter().copied().filter(|c| !needs.contains(c)).collect();
        for &v in &self.graph.volatile {
            needs.insert(v);
        }

        // 1. Collect the dirty subgraph with its adjacency.
        let mut adj: FxHashMap<CellRef, SmallVec<[CellRef; 4]>> = FxHashMap::default();
        let mut stack: Vec<CellRef> = seeds;
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
        let mut indeg: FxHashMap<CellRef, u32> = adj.keys().map(|&c| (c, 0)).collect();
        for deps in adj.values() {
            for d in deps {
                *indeg.get_mut(d).unwrap() += 1;
            }
        }
        for c in &changed_seeds {
            needs.extend(adj[c].iter().copied());
        }

        // 3. Kahn's algorithm with early cutoff.
        let mut queue: VecDeque<CellRef> = indeg.iter().filter(|&(_, &n)| n == 0).map(|(&c, _)| c).collect();
        let mut evaluated = 0;
        let mut processed = 0;
        let mut cycles_resolved = false;
        loop {
            while let Some(c) = queue.pop_front() {
                processed += 1;
                // A dirty cell showing #CIRC! must re-evaluate even if its inputs
                // are unchanged: the marker was imposed by cycle detection, and
                // the cycle may have just been broken.
                if let Some(cell) = self.store.get(c)
                    && (needs.contains(&c) || cell.value == Value::Error(ErrorKind::Circ))
                    && let CellInput::Formula { expr, .. } = &cell.input
                {
                    let expr = expr.clone();
                    let v = eval(&expr, &self.store);
                    evaluated += 1;
                    let cell = self.store.get_mut(c).unwrap();
                    if cell.value != v {
                        cell.value = v;
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
            // 4. Kahn stalled: some cells sit on cycles. Mark exactly the cycle
            //    members #CIRC!, then release the cells merely downstream of
            //    them so they evaluate normally (as Excel does).
            cycles_resolved = true;
            let stuck: Vec<CellRef> = indeg.iter().filter(|&(_, &n)| n > 0).map(|(&c, _)| c).collect();
            let cyclic = cycle_members(&stuck, &adj, &indeg);
            for &c in &cyclic {
                processed += 1;
                if let Some(cell) = self.store.get_mut(c)
                    && cell.is_formula()
                    && cell.value != Value::Error(ErrorKind::Circ)
                {
                    cell.value = Value::Error(ErrorKind::Circ);
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

/// Cells among `nodes` that lie on a cycle: members of a strongly connected
/// component of size > 1, or with a self-edge (iterative Tarjan).
fn cycle_members(
    nodes: &[CellRef],
    adj: &FxHashMap<CellRef, SmallVec<[CellRef; 4]>>,
    indeg: &FxHashMap<CellRef, u32>,
) -> FxHashSet<CellRef> {
    let in_set = |c: &CellRef| indeg.get(c).is_some_and(|&n| n > 0);
    let mut index: FxHashMap<CellRef, usize> = FxHashMap::default();
    let mut low: FxHashMap<CellRef, usize> = FxHashMap::default();
    let mut on_stack: FxHashSet<CellRef> = FxHashSet::default();
    let mut stack: Vec<CellRef> = Vec::new();
    let mut out = FxHashSet::default();
    let mut next = 0;
    for &root in nodes {
        if index.contains_key(&root) {
            continue;
        }
        let mut call: Vec<(CellRef, usize)> = vec![(root, 0)];
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
