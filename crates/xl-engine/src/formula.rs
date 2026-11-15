//! Excel formula lexer + Pratt parser, and a printer that can shift relative
//! references (fill-down / copy-paste). References may be qualified with a
//! sheet (`Sheet2!A1`, `'My Sheet'!A1:B5`); qualified references store the
//! sheet's stable id, so renaming a sheet never breaks a formula.

use std::fmt::Write as _;
use std::sync::Arc;

use crate::address::{A1, CellRef, MAX_ROWS, Range, SheetId, col_name, parse_a1, parse_col};
use crate::eval::Func;
use crate::value::ErrorKind;

/// Sheet name <-> id resolution used while parsing and printing.
pub trait SheetNames {
    fn sheet_id(&self, name: &str) -> Option<SheetId>;
    fn sheet_name(&self, id: SheetId) -> Option<&str>;
}

/// Resolver with no sheets (formulas without sheet-qualified refs).
pub struct NoSheets;

impl SheetNames for NoSheets {
    fn sheet_id(&self, _: &str) -> Option<SheetId> {
        None
    }
    fn sheet_name(&self, _: SheetId) -> Option<&str> {
        None
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct RefAddr {
    /// None = the sheet containing the formula.
    pub sheet: Option<SheetId>,
    pub cell: CellRef,
    pub col_abs: bool,
    pub row_abs: bool,
}

impl RefAddr {
    fn new(a: A1, sheet: Option<SheetId>) -> Self {
        Self { sheet, cell: a.cell, col_abs: a.col_abs, row_abs: a.row_abs }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum RangeKind {
    /// `A1:B5`
    Cells,
    /// `A:C` (whole columns)
    Columns,
    /// `1:3` (whole rows)
    Rows,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl BinOp {
    fn as_str(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Pow => "^",
            BinOp::Concat => "&",
            BinOp::Eq => "=",
            BinOp::Ne => "<>",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
        }
    }

    /// (left bp, right bp). Excel ops are left-associative, including `^`.
    fn binding_power(self) -> (u8, u8) {
        match self {
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => (10, 11),
            BinOp::Concat => (20, 21),
            BinOp::Add | BinOp::Sub => (30, 31),
            BinOp::Mul | BinOp::Div => (40, 41),
            BinOp::Pow => (50, 51),
        }
    }
}

const PERCENT_BP: u8 = 60;
const PREFIX_BP: u8 = 70;

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Number(f64),
    Text(Arc<str>),
    Bool(bool),
    Error(ErrorKind),
    Ref(RefAddr),
    /// The sheet of a range is the first endpoint's sheet.
    Range(RefAddr, RefAddr, RangeKind),
    Neg(Box<Expr>),
    Plus(Box<Expr>),
    Percent(Box<Expr>),
    Paren(Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call(Func, Vec<Expr>),
    UnknownFunc(Arc<str>, Vec<Expr>),
    /// A defined name (`TaxRate`), resolved when evaluated.
    Name(Arc<str>),
    /// A structured reference to part of a table (`Sales[Amount]`, `[@Qty]`).
    Table(Arc<StructRef>),
}

/// Which rows of a table a structured reference means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableArea {
    /// The data rows (the default).
    Data,
    All,
    Headers,
    Totals,
    /// The row of the formula (`[@Col]`).
    ThisRow,
}

/// `Table[...]`: a table (None = the one containing the formula), an area
/// and optionally a column or span of columns, by header name.
#[derive(Clone, Debug, PartialEq)]
pub struct StructRef {
    pub table: Option<Arc<str>>,
    pub area: TableArea,
    pub cols: Option<(Arc<str>, Arc<str>)>,
}

/// Column names with characters that need escaping (or brackets) inside a
/// structured reference.
fn escape_column(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if matches!(ch, '[' | ']' | '#' | '\'') {
            out.push('\'');
        }
        out.push(ch);
    }
    out
}

fn needs_brackets(name: &str) -> bool {
    !name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

impl StructRef {
    /// Parse the text between a reference's outer brackets.
    fn parse(table: Option<Arc<str>>, spec: &str) -> Result<StructRef, ParseError> {
        let bad = || ParseError(format!("bad table reference [{spec}]"));
        let item = |s: &str| -> String {
            // Unescape 'x and strip one level of brackets.
            let s = s.trim();
            let s = s.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(s);
            let mut out = String::new();
            let mut chars = s.chars();
            while let Some(c) = chars.next() {
                if c == '\'' {
                    if let Some(n) = chars.next() {
                        out.push(n);
                    }
                } else {
                    out.push(c);
                }
            }
            out
        };
        let special = |s: &str| -> Option<TableArea> {
            Some(match s.to_ascii_lowercase().as_str() {
                "#all" => TableArea::All,
                "#data" => TableArea::Data,
                "#headers" => TableArea::Headers,
                "#totals" => TableArea::Totals,
                "#this row" => TableArea::ThisRow,
                _ => return None,
            })
        };
        let spec_t = spec.trim();
        if spec_t.is_empty() {
            return Ok(StructRef { table, area: TableArea::Data, cols: None });
        }
        if let Some(rest) = spec_t.strip_prefix('@') {
            let col: Arc<str> = item(rest).into();
            let cols = (!col.is_empty()).then(|| (col.clone(), col));
            return Ok(StructRef { table, area: TableArea::ThisRow, cols });
        }
        if !spec_t.starts_with('[') {
            if let Some(area) = special(spec_t) {
                return Ok(StructRef { table, area, cols: None });
            }
            let col: Arc<str> = item(spec_t).into();
            return Ok(StructRef { table, area: TableArea::Data, cols: Some((col.clone(), col)) });
        }
        // A list of bracketed items: specials, then a column or column span.
        let mut parts: Vec<String> = Vec::new();
        let mut depth = 0;
        let mut cur = String::new();
        let mut chars = spec_t.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\'' => {
                    cur.push(c);
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                }
                '[' => {
                    depth += 1;
                    cur.push(c);
                }
                ']' => {
                    depth -= 1;
                    cur.push(c);
                }
                ',' if depth == 0 => parts.push(std::mem::take(&mut cur)),
                _ => cur.push(c),
            }
        }
        parts.push(cur);
        let mut area = TableArea::Data;
        let mut cols = None;
        for p in parts {
            let p = p.trim();
            if let Some(a) = special(&item(p)) {
                area = a;
            } else if let Some((a, b)) = p.split_once("]:[") {
                cols = Some((item(&format!("{a}]")).into(), item(&format!("[{b}")).into()));
            } else {
                let c: Arc<str> = item(p).into();
                if c.is_empty() {
                    return Err(bad());
                }
                cols = Some((c.clone(), c));
            }
        }
        Ok(StructRef { table, area, cols })
    }

    fn write(&self, out: &mut String) {
        if let Some(t) = &self.table {
            out.push_str(t);
        }
        let col = |c: &str| if needs_brackets(c) { format!("[{}]", escape_column(c)) } else { escape_column(c) };
        let bracketed = |c: &str| format!("[{}]", escape_column(c));
        let area = match self.area {
            TableArea::Data => None,
            TableArea::All => Some("#All"),
            TableArea::Headers => Some("#Headers"),
            TableArea::Totals => Some("#Totals"),
            TableArea::ThisRow => Some("#This Row"),
        };
        out.push('[');
        match (&self.cols, self.area, area) {
            (None, TableArea::Data, _) => out.push_str("#Data"),
            (None, _, Some(a)) => out.push_str(a),
            (Some((a, b)), TableArea::ThisRow, _) if a == b => {
                out.push('@');
                out.push_str(&col(a));
            }
            (Some((a, b)), TableArea::Data, _) if a == b => out.push_str(&escape_column(a)),
            (Some((a, b)), _, area) => {
                if let Some(ar) = area {
                    out.push_str(&format!("[{ar}],"));
                }
                if a == b {
                    out.push_str(&bracketed(a));
                } else {
                    out.push_str(&format!("{}:{}", bracketed(a), bracketed(b)));
                }
            }
            (None, _, None) => {}
        }
        out.push(']');
    }
}

impl Expr {
    /// The referenced range (and its explicit sheet, if any).
    pub fn reference(&self) -> Option<(Option<SheetId>, Range)> {
        match self {
            Expr::Ref(r) => Some((r.sheet, Range::single(r.cell))),
            Expr::Range(a, b, _) => Some((a.sheet, Range::new(a.cell, b.cell))),
            _ => None,
        }
    }

    /// Visit every reference in the expression.
    pub fn visit_refs(&self, f: &mut impl FnMut(Option<SheetId>, Range)) {
        match self {
            Expr::Ref(_) | Expr::Range(..) => {
                let (s, r) = self.reference().unwrap();
                f(s, r)
            }
            Expr::Neg(e) | Expr::Plus(e) | Expr::Percent(e) | Expr::Paren(e) => e.visit_refs(f),
            Expr::Binary(_, a, b) => {
                a.visit_refs(f);
                b.visit_refs(f);
            }
            Expr::Call(_, args) | Expr::UnknownFunc(_, args) => args.iter().for_each(|a| a.visit_refs(f)),
            Expr::Number(_) | Expr::Text(_) | Expr::Bool(_) | Expr::Error(_) | Expr::Name(_) | Expr::Table(_) => {}
        }
    }

    /// Visit every structured (table) reference in the expression.
    pub fn visit_tables(&self, f: &mut impl FnMut(&StructRef)) {
        match self {
            Expr::Table(t) => f(t),
            Expr::Neg(e) | Expr::Plus(e) | Expr::Percent(e) | Expr::Paren(e) => e.visit_tables(f),
            Expr::Binary(_, a, b) => {
                a.visit_tables(f);
                b.visit_tables(f);
            }
            Expr::Call(_, args) | Expr::UnknownFunc(_, args) => args.iter().for_each(|a| a.visit_tables(f)),
            _ => {}
        }
    }

    /// Visit every defined name used in the expression.
    pub fn visit_names(&self, f: &mut impl FnMut(&str)) {
        match self {
            Expr::Name(n) => f(n),
            Expr::Neg(e) | Expr::Plus(e) | Expr::Percent(e) | Expr::Paren(e) => e.visit_names(f),
            Expr::Binary(_, a, b) => {
                a.visit_names(f);
                b.visit_names(f);
            }
            Expr::Call(_, args) | Expr::UnknownFunc(_, args) => args.iter().for_each(|a| a.visit_names(f)),
            _ => {}
        }
    }

    fn any(&self, pred: &impl Fn(&Expr) -> bool) -> bool {
        if pred(self) {
            return true;
        }
        match self {
            Expr::Neg(e) | Expr::Plus(e) | Expr::Percent(e) | Expr::Paren(e) => e.any(pred),
            Expr::Binary(_, a, b) => a.any(pred) || b.any(pred),
            Expr::Call(_, args) | Expr::UnknownFunc(_, args) => args.iter().any(|a| a.any(pred)),
            _ => false,
        }
    }

    pub fn is_volatile(&self) -> bool {
        self.any(&|e| matches!(e, Expr::Call(f, _) if f.is_volatile()))
    }

    pub fn has_unknown_func(&self) -> bool {
        self.any(&|e| matches!(e, Expr::UnknownFunc(..)))
    }

    /// Print as formula source (without `=`), shifting relative refs by (dr, dc).
    pub fn to_source(&self, dr: i64, dc: i64, names: &dyn SheetNames) -> String {
        let mut s = String::new();
        write_expr(self, dr, dc, names, &mut s);
        s
    }
}

/// A structural change to a sheet: rows or columns inserted or deleted.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Shift {
    InsertRows { sheet: SheetId, at: u32, count: u32 },
    DeleteRows { sheet: SheetId, at: u32, count: u32 },
    InsertCols { sheet: SheetId, at: u32, count: u32 },
    DeleteCols { sheet: SheetId, at: u32, count: u32 },
}

impl Shift {
    pub fn sheet(self) -> SheetId {
        match self {
            Shift::InsertRows { sheet, .. } | Shift::DeleteRows { sheet, .. } | Shift::InsertCols { sheet, .. } | Shift::DeleteCols { sheet, .. } => sheet,
        }
    }

    /// The change that undoes this one (inserting the deleted rows back, …).
    pub fn inverse(self) -> Shift {
        match self {
            Shift::InsertRows { sheet, at, count } => Shift::DeleteRows { sheet, at, count },
            Shift::DeleteRows { sheet, at, count } => Shift::InsertRows { sheet, at, count },
            Shift::InsertCols { sheet, at, count } => Shift::DeleteCols { sheet, at, count },
            Shift::DeleteCols { sheet, at, count } => Shift::InsertCols { sheet, at, count },
        }
    }

    fn is_rows(self) -> bool {
        matches!(self, Shift::InsertRows { .. } | Shift::DeleteRows { .. })
    }

    /// Where position `p` (a row or column index along the shifted axis)
    /// ends up; None if it was deleted.
    pub fn map_pos(self, p: u32) -> Option<u32> {
        let limit = if self.is_rows() { MAX_ROWS } else { crate::address::MAX_COLS };
        match self {
            Shift::InsertRows { at, count, .. } | Shift::InsertCols { at, count, .. } => {
                if p < at {
                    Some(p)
                } else {
                    (p + count < limit).then_some(p + count)
                }
            }
            Shift::DeleteRows { at, count, .. } | Shift::DeleteCols { at, count, .. } => {
                if p < at {
                    Some(p)
                } else if p < at + count {
                    None
                } else {
                    Some(p - count)
                }
            }
        }
    }

    /// [`Shift::map_span`] for callers outside this module.
    pub fn map_span_pub(self, a: u32, b: u32) -> Option<(u32, u32)> {
        self.map_span(a, b)
    }

    /// Where a rectangular range ends up (for merges and other ranges kept
    /// alongside cells); None if it was deleted entirely.
    pub fn map_range(self, r: Range) -> Option<Range> {
        if self.is_rows() {
            let (a, b) = self.map_span(r.start.row, r.end.row)?;
            Some(Range::new(CellRef::new(a, r.start.col), CellRef::new(b, r.end.col)))
        } else {
            let (a, b) = self.map_span(r.start.col, r.end.col)?;
            Some(Range::new(CellRef::new(r.start.row, a), CellRef::new(r.end.row, b)))
        }
    }

    /// Map a span [a, b] along the shifted axis: inserted lines inside the
    /// span grow it, deleted lines shrink it; None if all of it was deleted.
    fn map_span(self, a: u32, b: u32) -> Option<(u32, u32)> {
        match self {
            Shift::InsertRows { .. } | Shift::InsertCols { .. } => Some((self.map_pos(a)?, self.map_pos(b).unwrap_or(b))),
            Shift::DeleteRows { at, count, .. } | Shift::DeleteCols { at, count, .. } => {
                let end_del = at + count;
                if a >= at && b < end_del {
                    return None;
                }
                let na = if a >= end_del { a - count } else { a.min(at) };
                let nb = if b >= end_del { b - count } else if b >= at { at.saturating_sub(1).max(na) } else { b };
                Some((na, nb))
            }
        }
    }
}

impl Expr {
    /// The expression after a structural change on `op.sheet()`, for a
    /// formula that lives on `home`. References to deleted cells become #REF!.
    pub fn shifted(&self, op: Shift, home: SheetId) -> Expr {
        let rows = op.is_rows();
        let on_sheet = |r: &RefAddr| r.sheet.unwrap_or(home) == op.sheet();
        let map_ref = |r: &RefAddr| -> Option<RefAddr> {
            if !on_sheet(r) {
                return Some(*r);
            }
            let mut out = *r;
            if rows {
                out.cell.row = op.map_pos(r.cell.row)?;
            } else {
                out.cell.col = op.map_pos(r.cell.col)?;
            }
            Some(out)
        };
        let recur = |e: &Expr| Box::new(e.shifted(op, home));
        match self {
            Expr::Ref(r) => map_ref(r).map_or(Expr::Error(ErrorKind::Ref), Expr::Ref),
            Expr::Range(a, b, kind) => {
                if !on_sheet(a) || (rows && *kind == RangeKind::Columns) || (!rows && *kind == RangeKind::Rows) {
                    return self.clone();
                }
                let (pa, pb) = if rows { (a.cell.row, b.cell.row) } else { (a.cell.col, b.cell.col) };
                let (lo, hi) = (pa.min(pb), pa.max(pb));
                match op.map_span(lo, hi) {
                    None => Expr::Error(ErrorKind::Ref),
                    Some((nlo, nhi)) => {
                        let (mut na, mut nb) = (*a, *b);
                        let (va, vb) = if pa <= pb { (nlo, nhi) } else { (nhi, nlo) };
                        if rows {
                            na.cell.row = va;
                            nb.cell.row = vb;
                        } else {
                            na.cell.col = va;
                            nb.cell.col = vb;
                        }
                        Expr::Range(na, nb, *kind)
                    }
                }
            }
            Expr::Neg(x) => Expr::Neg(recur(x)),
            Expr::Plus(x) => Expr::Plus(recur(x)),
            Expr::Percent(x) => Expr::Percent(recur(x)),
            Expr::Paren(x) => Expr::Paren(recur(x)),
            Expr::Binary(o, x, y) => Expr::Binary(*o, recur(x), recur(y)),
            Expr::Call(f, args) => Expr::Call(*f, args.iter().map(|a| a.shifted(op, home)).collect()),
            Expr::UnknownFunc(n, args) => Expr::UnknownFunc(n.clone(), args.iter().map(|a| a.shifted(op, home)).collect()),
            Expr::Number(_) | Expr::Text(_) | Expr::Bool(_) | Expr::Error(_) | Expr::Name(_) | Expr::Table(_) => self.clone(),
        }
    }
}

/// Moving a block of cells (cut and paste): references into the block follow
/// it; references to cells it lands on (outside the block) break.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Move {
    pub from_sheet: SheetId,
    pub from: Range,
    pub to_sheet: SheetId,
    pub dr: i64,
    pub dc: i64,
}

impl Move {
    pub fn to_range(&self) -> Range {
        let off = |c: CellRef| CellRef::new((c.row as i64 + self.dr) as u32, (c.col as i64 + self.dc) as u32);
        Range::new(off(self.from.start), off(self.from.end))
    }
}

impl Expr {
    /// The formula as if copied `dr` rows and `dc` columns away: relative
    /// references move, absolute ones stay (off-sheet ones become #REF!).
    pub fn offset(&self, dr: i64, dc: i64) -> Expr {
        if (dr, dc) == (0, 0) {
            return self.clone();
        }
        let recur = |e: &Expr| Box::new(e.offset(dr, dc));
        match self {
            Expr::Ref(r) => shift_ref(r, dr, dc).map_or(Expr::Error(ErrorKind::Ref), Expr::Ref),
            Expr::Range(a, b, kind) => match (shift_ref(a, dr, dc), shift_ref(b, dr, dc)) {
                (Some(a), Some(b)) => Expr::Range(a, b, *kind),
                _ => Expr::Error(ErrorKind::Ref),
            },
            Expr::Neg(x) => Expr::Neg(recur(x)),
            Expr::Plus(x) => Expr::Plus(recur(x)),
            Expr::Percent(x) => Expr::Percent(recur(x)),
            Expr::Paren(x) => Expr::Paren(recur(x)),
            Expr::Binary(o, x, y) => Expr::Binary(*o, recur(x), recur(y)),
            Expr::Call(f, args) => Expr::Call(*f, args.iter().map(|a| a.offset(dr, dc)).collect()),
            Expr::UnknownFunc(n, args) => Expr::UnknownFunc(n.clone(), args.iter().map(|a| a.offset(dr, dc)).collect()),
            Expr::Number(_) | Expr::Text(_) | Expr::Bool(_) | Expr::Error(_) | Expr::Name(_) | Expr::Table(_) => self.clone(),
        }
    }

    /// Rewrite references for a move, in a formula on sheet `home`.
    pub fn moved(&self, mv: &Move, home: SheetId) -> Expr {
        let to = mv.to_range();
        let relocate = |r: &RefAddr| {
            let mut out = *r;
            out.cell = CellRef::new((r.cell.row as i64 + mv.dr) as u32, (r.cell.col as i64 + mv.dc) as u32);
            out.sheet = if r.sheet.is_none() && mv.to_sheet == home { None } else { Some(mv.to_sheet) };
            out
        };
        let recur = |e: &Expr| Box::new(e.moved(mv, home));
        match self {
            Expr::Ref(r) => {
                let sheet = r.sheet.unwrap_or(home);
                if sheet == mv.from_sheet && mv.from.contains(r.cell) {
                    Expr::Ref(relocate(r))
                } else if sheet == mv.to_sheet && to.contains(r.cell) {
                    Expr::Error(ErrorKind::Ref)
                } else {
                    self.clone()
                }
            }
            Expr::Range(a, b, kind) => {
                let sheet = a.sheet.unwrap_or(home);
                let inside = |r: Range| r.contains(a.cell) && r.contains(b.cell);
                if *kind == RangeKind::Cells && sheet == mv.from_sheet && inside(mv.from) {
                    Expr::Range(relocate(a), relocate(b), *kind)
                } else if *kind == RangeKind::Cells && sheet == mv.to_sheet && inside(to) {
                    Expr::Error(ErrorKind::Ref)
                } else {
                    self.clone()
                }
            }
            Expr::Neg(x) => Expr::Neg(recur(x)),
            Expr::Plus(x) => Expr::Plus(recur(x)),
            Expr::Percent(x) => Expr::Percent(recur(x)),
            Expr::Paren(x) => Expr::Paren(recur(x)),
            Expr::Binary(o, x, y) => Expr::Binary(*o, recur(x), recur(y)),
            Expr::Call(f, args) => Expr::Call(*f, args.iter().map(|a| a.moved(mv, home)).collect()),
            Expr::UnknownFunc(n, args) => Expr::UnknownFunc(n.clone(), args.iter().map(|a| a.moved(mv, home)).collect()),
            Expr::Number(_) | Expr::Text(_) | Expr::Bool(_) | Expr::Error(_) | Expr::Name(_) | Expr::Table(_) => self.clone(),
        }
    }

    /// A formula moving from sheet `from` to `to` keeps pointing at the same
    /// cells: implicit-sheet references get `from` spelled out.
    pub fn rehomed(&self, from: SheetId, to: SheetId) -> Expr {
        if from == to {
            return self.clone();
        }
        let fix = |r: &RefAddr| RefAddr { sheet: Some(r.sheet.unwrap_or(from)), ..*r };
        let recur = |e: &Expr| Box::new(e.rehomed(from, to));
        match self {
            Expr::Ref(r) => Expr::Ref(fix(r)),
            Expr::Range(a, b, k) => Expr::Range(fix(a), fix(b), *k),
            Expr::Neg(x) => Expr::Neg(recur(x)),
            Expr::Plus(x) => Expr::Plus(recur(x)),
            Expr::Percent(x) => Expr::Percent(recur(x)),
            Expr::Paren(x) => Expr::Paren(recur(x)),
            Expr::Binary(o, x, y) => Expr::Binary(*o, recur(x), recur(y)),
            Expr::Call(f, args) => Expr::Call(*f, args.iter().map(|a| a.rehomed(from, to)).collect()),
            Expr::UnknownFunc(n, args) => Expr::UnknownFunc(n.clone(), args.iter().map(|a| a.rehomed(from, to)).collect()),
            Expr::Number(_) | Expr::Text(_) | Expr::Bool(_) | Expr::Error(_) | Expr::Name(_) | Expr::Table(_) => self.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

/// Parse a formula body (the text after `=`).
pub fn parse_formula(src: &str, names: &dyn SheetNames) -> Result<Expr, ParseError> {
    let tokens = lex(src)?;
    let mut p = Parser { tokens, pos: 0, names };
    let e = p.expr(0)?;
    match p.peek() {
        None => Ok(e),
        Some(t) => Err(ParseError(format!("unexpected {}", t.describe()))),
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    Sheet(String),
    Err(ErrorKind),
    Op(BinOp),
    Minus,
    PlusSign,
    Percent,
    LParen,
    RParen,
    Comma,
    Colon,
    /// A structured reference: optional table name and the bracket contents.
    Struct(Option<String>, String),
}

impl Tok {
    fn describe(&self) -> String {
        match self {
            Tok::Num(n) => format!("number {n}"),
            Tok::Str(s) => format!("text \"{s}\""),
            Tok::Ident(s) => format!("'{s}'"),
            Tok::Sheet(s) => format!("sheet '{s}'"),
            Tok::Err(e) => e.to_string(),
            Tok::Op(op) => format!("'{}'", op.as_str()),
            Tok::Minus => "'-'".into(),
            Tok::PlusSign => "'+'".into(),
            Tok::Percent => "'%'".into(),
            Tok::LParen => "'('".into(),
            Tok::RParen => "')'".into(),
            Tok::Comma => "','".into(),
            Tok::Colon => "':'".into(),
            Tok::Struct(t, s) => format!("'{}[{s}]'", t.as_deref().unwrap_or("")),
        }
    }
}

/// The contents of the bracket block starting at byte `start` (a '['),
/// and the index just past its closing ']'. Nested brackets and ' escapes
/// are respected.
fn bracket_block(src: &str, start: usize) -> Result<(String, usize), ParseError> {
    let b = src.as_bytes();
    let mut depth = 0;
    let mut i = start;
    while i < b.len() {
        match b[i] {
            b'\'' => i += 1,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Ok((src[start + 1..i].to_string(), i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
    Err(ParseError("unterminated table reference (missing ])".into()))
}

fn lex(src: &str) -> Result<Vec<Tok>, ParseError> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'0'..=b'9' | b'.' => {
                let s = i;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                    i += 1;
                }
                if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                    let save = i;
                    i += 1;
                    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                        i += 1;
                    }
                    if i < b.len() && b[i].is_ascii_digit() {
                        while i < b.len() && b[i].is_ascii_digit() {
                            i += 1;
                        }
                    } else {
                        i = save;
                    }
                }
                let n: f64 = src[s..i].parse().map_err(|_| ParseError(format!("bad number {}", &src[s..i])))?;
                out.push(Tok::Num(n));
            }
            b'"' => {
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= b.len() {
                        return Err(ParseError("unterminated text (missing \")".into()));
                    }
                    if b[i] == b'"' {
                        if b.get(i + 1) == Some(&b'"') {
                            s.push('"');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    let ch = src[i..].chars().next().unwrap();
                    s.push(ch);
                    i += ch.len_utf8();
                }
                out.push(Tok::Str(s));
            }
            b'\'' => {
                // Quoted sheet name: 'My Sheet'!A1 (quotes doubled inside).
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= b.len() {
                        return Err(ParseError("unterminated sheet name (missing ')".into()));
                    }
                    if b[i] == b'\'' {
                        if b.get(i + 1) == Some(&b'\'') {
                            s.push('\'');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    let ch = src[i..].chars().next().unwrap();
                    s.push(ch);
                    i += ch.len_utf8();
                }
                if b.get(i) != Some(&b'!') {
                    return Err(ParseError(format!("expected ! after '{s}'")));
                }
                i += 1;
                out.push(Tok::Sheet(s));
            }
            b'#' => {
                let s = i;
                i += 1;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'/') {
                    i += 1;
                }
                if i < b.len() && (b[i] == b'!' || b[i] == b'?') {
                    i += 1;
                }
                let e = ErrorKind::parse(&src[s..i]).ok_or_else(|| ParseError(format!("unknown error {}", &src[s..i])))?;
                out.push(Tok::Err(e));
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 => {
                let s = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'_' | b'.' | b'$') || b[i] >= 0x80) {
                    i += 1;
                }
                let id = &src[s..i];
                if b.get(i) == Some(&b'!') {
                    i += 1;
                    out.push(Tok::Sheet(id.to_string()));
                } else if b.get(i) == Some(&b'[') {
                    let (spec, end) = bracket_block(src, i)?;
                    i = end;
                    out.push(Tok::Struct(Some(id.to_string()), spec));
                } else {
                    out.push(Tok::Ident(id.to_string()));
                }
            }
            b'[' => {
                let (spec, end) = bracket_block(src, i)?;
                i = end;
                out.push(Tok::Struct(None, spec));
            }
            b'+' => {
                out.push(Tok::PlusSign);
                i += 1;
            }
            b'-' => {
                out.push(Tok::Minus);
                i += 1;
            }
            b'*' => {
                out.push(Tok::Op(BinOp::Mul));
                i += 1;
            }
            b'/' => {
                out.push(Tok::Op(BinOp::Div));
                i += 1;
            }
            b'^' => {
                out.push(Tok::Op(BinOp::Pow));
                i += 1;
            }
            b'&' => {
                out.push(Tok::Op(BinOp::Concat));
                i += 1;
            }
            b'%' => {
                out.push(Tok::Percent);
                i += 1;
            }
            b'=' => {
                out.push(Tok::Op(BinOp::Eq));
                i += 1;
            }
            b'<' => {
                let (op, n) = match b.get(i + 1) {
                    Some(b'=') => (BinOp::Le, 2),
                    Some(b'>') => (BinOp::Ne, 2),
                    _ => (BinOp::Lt, 1),
                };
                out.push(Tok::Op(op));
                i += n;
            }
            b'>' => {
                let (op, n) = if b.get(i + 1) == Some(&b'=') { (BinOp::Ge, 2) } else { (BinOp::Gt, 1) };
                out.push(Tok::Op(op));
                i += n;
            }
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b',' | b';' => {
                out.push(Tok::Comma);
                i += 1;
            }
            b':' => {
                out.push(Tok::Colon);
                i += 1;
            }
            _ => {
                let ch = src[i..].chars().next().unwrap();
                return Err(ParseError(format!("unexpected character '{ch}'")));
            }
        }
    }
    Ok(out)
}

struct Parser<'a> {
    tokens: Vec<Tok>,
    pos: usize,
    names: &'a dyn SheetNames,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn expect(&mut self, t: Tok) -> Result<(), ParseError> {
        match self.next() {
            Some(x) if x == t => Ok(()),
            Some(other) => Err(ParseError(format!("expected {}, found {}", t.describe(), other.describe()))),
            None => Err(ParseError(format!("expected {} before end of formula", t.describe()))),
        }
    }

    fn expr(&mut self, min_bp: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.prefix()?;
        loop {
            match self.peek() {
                Some(Tok::Percent) if PERCENT_BP >= min_bp => {
                    self.pos += 1;
                    lhs = Expr::Percent(Box::new(lhs));
                }
                Some(Tok::Op(_) | Tok::Minus | Tok::PlusSign) => {
                    let op = match self.peek().unwrap() {
                        Tok::Op(op) => *op,
                        Tok::Minus => BinOp::Sub,
                        _ => BinOp::Add,
                    };
                    let (l, r) = op.binding_power();
                    if l < min_bp {
                        break;
                    }
                    self.pos += 1;
                    let rhs = self.expr(r)?;
                    lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
                }
                _ => break,
            }
        }
        Ok(lhs)
    }

    /// `1:3` / `$1:$1` after the first row number (`abs` = it had a `$`).
    fn row_range(&mut self, first: f64, abs: bool, sheet: Option<SheetId>) -> Option<Result<Expr, ParseError>> {
        let whole = |n: f64| (n >= 1.0 && n.fract() == 0.0 && n <= MAX_ROWS as f64).then(|| n as u32 - 1);
        let r1 = whole(first)?;
        if self.peek() != Some(&Tok::Colon) {
            return None;
        }
        let (r2, abs2) = match self.tokens.get(self.pos + 1) {
            Some(Tok::Num(n)) => (whole(*n)?, false),
            Some(Tok::Ident(id)) => (whole(id.strip_prefix('$')?.parse::<f64>().ok()?)?, true),
            _ => return None,
        };
        self.pos += 2;
        let a = RefAddr { sheet, cell: CellRef::new(r1, 0), col_abs: true, row_abs: abs };
        let b = RefAddr { sheet, cell: CellRef::new(r2, crate::address::MAX_COLS - 1), col_abs: true, row_abs: abs2 };
        Some(Ok(Expr::Range(a, b, RangeKind::Rows)))
    }

    fn prefix(&mut self) -> Result<Expr, ParseError> {
        match self.next() {
            Some(Tok::Num(n)) => match self.row_range(n, false, None) {
                Some(r) => r,
                None => Ok(Expr::Number(n)),
            },
            Some(Tok::Str(s)) => Ok(Expr::Text(s.into())),
            Some(Tok::Err(e)) => Ok(Expr::Error(e)),
            Some(Tok::Minus) => Ok(Expr::Neg(Box::new(self.expr(PREFIX_BP)?))),
            Some(Tok::PlusSign) => Ok(Expr::Plus(Box::new(self.expr(PREFIX_BP)?))),
            Some(Tok::LParen) => {
                let e = self.expr(0)?;
                self.expect(Tok::RParen)?;
                Ok(Expr::Paren(Box::new(e)))
            }
            Some(Tok::Sheet(name)) => {
                let id = self.names.sheet_id(&name).ok_or_else(|| ParseError(format!("no sheet named '{name}'")))?;
                match self.next() {
                    // What Excel writes for a reference that was deleted: Sheet1!#REF!
                    Some(Tok::Err(e)) => Ok(Expr::Error(e)),
                    Some(Tok::Num(n)) => self.row_range(n, false, Some(id)).ok_or_else(|| ParseError(format!("bad reference after {name}!")))?,
                    Some(Tok::Ident(r)) if r.starts_with('$') && r[1..].bytes().all(|b| b.is_ascii_digit()) && !r[1..].is_empty() => {
                        self.row_range(r[1..].parse().unwrap_or(0.0), true, Some(id)).ok_or_else(|| ParseError(format!("bad reference after {name}!")))?
                    }
                    Some(Tok::Ident(r)) => self.reference(r, Some(id)).ok_or_else(|| ParseError(format!("bad reference {name}!{}", self.tokens[self.pos - 1].describe())))?,
                    _ => Err(ParseError(format!("expected a cell reference after {name}!"))),
                }
            }
            Some(Tok::Ident(id)) => self.ident(id),
            Some(Tok::Struct(table, spec)) => Ok(Expr::Table(Arc::new(StructRef::parse(table.map(Into::into), &spec)?))),
            Some(t) => Err(ParseError(format!("unexpected {}", t.describe()))),
            None => Err(ParseError("formula ends too early".into())),
        }
    }

    /// Try to parse a cell / range / column-range reference starting with `id`.
    fn reference(&mut self, id: String, sheet: Option<SheetId>) -> Option<Result<Expr, ParseError>> {
        if let Some(a) = parse_a1(&id) {
            if self.peek() == Some(&Tok::Colon) {
                self.pos += 1;
                return Some(match self.next() {
                    Some(Tok::Ident(id2)) => match parse_a1(&id2) {
                        Some(b) => Ok(Expr::Range(RefAddr::new(a, sheet), RefAddr::new(b, sheet), RangeKind::Cells)),
                        None => Err(ParseError(format!("bad range end {id2}"))),
                    },
                    other => Err(ParseError(format!("bad range end {}", other.map_or("(none)".into(), |t| t.describe())))),
                });
            }
            return Some(Ok(Expr::Ref(RefAddr::new(a, sheet))));
        }
        if let Some((c1, abs1)) = parse_col_abs(&id)
            && self.peek() == Some(&Tok::Colon)
        {
            self.pos += 1;
            if let Some(Tok::Ident(id2)) = self.next()
                && let Some((c2, abs2)) = parse_col_abs(&id2)
            {
                let a = RefAddr { sheet, cell: CellRef::new(0, c1), col_abs: abs1, row_abs: true };
                let b = RefAddr { sheet, cell: CellRef::new(MAX_ROWS - 1, c2), col_abs: abs2, row_abs: true };
                return Some(Ok(Expr::Range(a, b, RangeKind::Columns)));
            }
            return Some(Err(ParseError(format!("bad column range after {id}:"))));
        }
        None
    }

    fn ident(&mut self, id: String) -> Result<Expr, ParseError> {
        if let Some(digits) = id.strip_prefix('$').filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
            && let Some(r) = self.row_range(digits.parse().unwrap_or(0.0), true, None)
        {
            return r;
        }
        if self.peek() == Some(&Tok::LParen) {
            self.pos += 1;
            let mut args = Vec::new();
            if self.peek() != Some(&Tok::RParen) {
                loop {
                    args.push(self.expr(0)?);
                    match self.next() {
                        Some(Tok::Comma) => continue,
                        Some(Tok::RParen) => break,
                        Some(t) => return Err(ParseError(format!("expected , or ) but found {}", t.describe()))),
                        None => return Err(ParseError("missing )".into())),
                    }
                }
            } else {
                self.pos += 1;
            }
            // Newer functions are stored with a prefix in xlsx files.
            let name = id.strip_prefix("_xlfn.").or_else(|| id.strip_prefix("_xlws.")).unwrap_or(&id);
            return Ok(match Func::from_name(name) {
                Some(f) => Expr::Call(f, args),
                None => Expr::UnknownFunc(name.to_ascii_uppercase().into(), args),
            });
        }
        if let Some(r) = self.reference(id.clone(), None) {
            return r;
        }
        match id.to_ascii_uppercase().as_str() {
            "TRUE" => Ok(Expr::Bool(true)),
            "FALSE" => Ok(Expr::Bool(false)),
            _ => Ok(Expr::Name(id.into())),
        }
    }
}

fn parse_col_abs(s: &str) -> Option<(u32, bool)> {
    match s.strip_prefix('$') {
        Some(rest) => parse_col(rest).map(|c| (c, true)),
        None => parse_col(s).map(|c| (c, false)),
    }
}

fn shift_ref(r: &RefAddr, dr: i64, dc: i64) -> Option<RefAddr> {
    let row = if r.row_abs { r.cell.row as i64 } else { r.cell.row as i64 + dr };
    let col = if r.col_abs { r.cell.col as i64 } else { r.cell.col as i64 + dc };
    let cell = CellRef::new(0, 0).offset(row, col)?;
    Some(RefAddr { cell, ..*r })
}

/// Quote a sheet name for use in a formula if needed.
pub fn quote_sheet_name(name: &str) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        && parse_a1(name).is_none()
        && !name.eq_ignore_ascii_case("TRUE")
        && !name.eq_ignore_ascii_case("FALSE");
    if plain { name.to_string() } else { format!("'{}'", name.replace('\'', "''")) }
}

/// Returns false if the sheet no longer exists.
fn write_sheet(sheet: Option<SheetId>, names: &dyn SheetNames, out: &mut String) -> bool {
    match sheet {
        None => true,
        Some(id) => match names.sheet_name(id) {
            Some(n) => {
                out.push_str(&quote_sheet_name(n));
                out.push('!');
                true
            }
            None => false,
        },
    }
}

fn write_ref(r: &RefAddr, out: &mut String) {
    let _ = write!(
        out,
        "{}{}{}{}",
        if r.col_abs { "$" } else { "" },
        col_name(r.cell.col),
        if r.row_abs { "$" } else { "" },
        r.cell.row + 1
    );
}

fn write_number(n: f64, out: &mut String) {
    // Shortest round-trip representation, but avoid long exponent-free forms.
    let a = n.abs();
    if a != 0.0 && !(1e-9..1e15).contains(&a) {
        let _ = write!(out, "{n:E}");
    } else {
        let _ = write!(out, "{n}");
    }
}

fn write_expr(e: &Expr, dr: i64, dc: i64, names: &dyn SheetNames, out: &mut String) {
    match e {
        Expr::Number(n) => write_number(*n, out),
        Expr::Text(s) => {
            out.push('"');
            out.push_str(&s.replace('"', "\"\""));
            out.push('"');
        }
        Expr::Bool(b) => out.push_str(if *b { "TRUE" } else { "FALSE" }),
        Expr::Error(err) => out.push_str(err.as_str()),
        Expr::Name(n) => out.push_str(n),
        Expr::Table(t) => t.write(out),
        Expr::Ref(r) => {
            let mark = out.len();
            match shift_ref(r, dr, dc) {
                Some(r) if write_sheet(r.sheet, names, out) => write_ref(&r, out),
                _ => {
                    out.truncate(mark);
                    out.push_str("#REF!")
                }
            }
        }
        Expr::Range(a, b, kind) => {
            let (sa, sb) = match kind {
                RangeKind::Cells => (shift_ref(a, dr, dc), shift_ref(b, dr, dc)),
                RangeKind::Columns => (shift_ref(a, 0, dc), shift_ref(b, 0, dc)),
                RangeKind::Rows => (shift_ref(a, dr, 0), shift_ref(b, dr, 0)),
            };
            let mark = out.len();
            match (sa, sb) {
                (Some(a), Some(b)) if write_sheet(a.sheet, names, out) => match kind {
                    RangeKind::Cells => {
                        write_ref(&a, out);
                        out.push(':');
                        write_ref(&b, out);
                    }
                    RangeKind::Columns => {
                        let _ = write!(
                            out,
                            "{}{}:{}{}",
                            if a.col_abs { "$" } else { "" },
                            col_name(a.cell.col),
                            if b.col_abs { "$" } else { "" },
                            col_name(b.cell.col)
                        );
                    }
                    RangeKind::Rows => {
                        let _ = write!(out, "{}{}:{}{}", if a.row_abs { "$" } else { "" }, a.cell.row + 1, if b.row_abs { "$" } else { "" }, b.cell.row + 1);
                    }
                },
                _ => {
                    out.truncate(mark);
                    out.push_str("#REF!")
                }
            }
        }
        Expr::Neg(x) => {
            out.push('-');
            write_expr(x, dr, dc, names, out);
        }
        Expr::Plus(x) => {
            out.push('+');
            write_expr(x, dr, dc, names, out);
        }
        Expr::Percent(x) => {
            write_expr(x, dr, dc, names, out);
            out.push('%');
        }
        Expr::Paren(x) => {
            out.push('(');
            write_expr(x, dr, dc, names, out);
            out.push(')');
        }
        Expr::Binary(op, a, b) => {
            write_expr(a, dr, dc, names, out);
            out.push_str(op.as_str());
            write_expr(b, dr, dc, names, out);
        }
        Expr::Call(_, args) | Expr::UnknownFunc(_, args) => {
            let name = match e {
                Expr::Call(f, _) => f.name(),
                Expr::UnknownFunc(n, _) => n,
                _ => unreachable!(),
            };
            out.push_str(name);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_expr(a, dr, dc, names, out);
            }
            out.push(')');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Names(Vec<(SheetId, &'static str)>);
    impl SheetNames for Names {
        fn sheet_id(&self, name: &str) -> Option<SheetId> {
            self.0.iter().find(|(_, n)| n.eq_ignore_ascii_case(name)).map(|(i, _)| *i)
        }
        fn sheet_name(&self, id: SheetId) -> Option<&str> {
            self.0.iter().find(|(i, _)| *i == id).map(|(_, n)| *n)
        }
    }

    fn names() -> Names {
        Names(vec![(0, "Sheet1"), (1, "Data"), (2, "My Sheet"), (3, "it's")])
    }

    fn roundtrip(s: &str) -> String {
        let n = names();
        parse_formula(s, &n).unwrap().to_source(0, 0, &n)
    }

    #[test]
    fn parses_and_prints() {
        assert_eq!(roundtrip("1+2*3"), "1+2*3");
        assert_eq!(roundtrip("SUM( A1:B2 , $C$3)"), "SUM(A1:B2,$C$3)");
        assert_eq!(roundtrip("-(A1)%"), "-(A1)%");
        assert_eq!(roundtrip("\"a\"\"b\"&A1"), "\"a\"\"b\"&A1");
        assert_eq!(roundtrip("sum(a:a)"), "SUM(A:A)");
        assert_eq!(roundtrip("A1<>B1"), "A1<>B1");
        assert_eq!(roundtrip("_xlfn.CONCAT(A1,B1)"), "CONCAT(A1,B1)");
        assert_eq!(roundtrip("0.1+1E-12"), "0.1+1E-12");
    }

    #[test]
    fn sheet_references() {
        assert_eq!(roundtrip("Data!A1+data!B2"), "Data!A1+Data!B2");
        assert_eq!(roundtrip("'My Sheet'!A1:B3"), "'My Sheet'!A1:B3");
        assert_eq!(roundtrip("SUM('it''s'!C:C)"), "SUM('it''s'!C:C)");
        assert_eq!(roundtrip("'Data'!A1"), "Data!A1");
        let n = names();
        assert!(parse_formula("Nope!A1", &n).is_err());
        let e = parse_formula("Data!A1", &n).unwrap();
        assert_eq!(e.reference().unwrap().0, Some(1));
    }

    #[test]
    fn renamed_and_deleted_sheets() {
        let e = parse_formula("Data!A1*2", &names()).unwrap();
        let renamed = Names(vec![(1, "Q4 Data")]);
        assert_eq!(e.to_source(0, 0, &renamed), "'Q4 Data'!A1*2");
        assert_eq!(e.to_source(0, 0, &NoSheets), "#REF!*2");
    }

    #[test]
    fn precedence() {
        let n = NoSheets;
        assert!(matches!(parse_formula("-2^2", &n).unwrap(), Expr::Binary(BinOp::Pow, ..)));
        assert!(matches!(parse_formula("1+2*3", &n).unwrap(), Expr::Binary(BinOp::Add, ..)));
        assert!(matches!(parse_formula("1&2=3", &n).unwrap(), Expr::Binary(BinOp::Eq, ..)));
    }

    #[test]
    fn shifting() {
        let n = names();
        let e = parse_formula("A1+$B$1+C$1+$D1+SUM(A1:A3)+Data!A1", &n).unwrap();
        assert_eq!(e.to_source(2, 1, &n), "B3+$B$1+D$1+$D3+SUM(B3:B5)+Data!B3");
        let e = parse_formula("A1", &n).unwrap();
        assert_eq!(e.to_source(-1, 0, &n), "#REF!");
    }

    #[test]
    fn errors() {
        let n = NoSheets;
        assert!(parse_formula("1+", &n).is_err());
        assert!(parse_formula("SUM(1,2", &n).is_err());
        assert!(parse_formula("\"abc", &n).is_err());
        assert!(parse_formula("Table1[Col", &n).is_err(), "unterminated table reference");
        assert!(parse_formula("Table1[Col]", &n).is_ok());
    }
}
