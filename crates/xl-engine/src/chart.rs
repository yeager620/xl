//! Charts: a kind, data series given as references, and a place on the
//! sheet. The data is read from the workbook when the chart is drawn.

use std::sync::Arc;

use crate::address::{Addr, CellRef, Range, SheetId};
use crate::formula::{Expr, Move, Shift};
use crate::value::Value;
use crate::workbook::Workbook;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChartKind {
    Column,
    Bar,
    Line,
    Area,
    Pie,
    Scatter,
}

impl ChartKind {
    pub const ALL: [ChartKind; 6] = [ChartKind::Column, ChartKind::Bar, ChartKind::Line, ChartKind::Area, ChartKind::Pie, ChartKind::Scatter];

    pub fn label(self) -> &'static str {
        match self {
            ChartKind::Column => "Column",
            ChartKind::Bar => "Bar",
            ChartKind::Line => "Line",
            ChartKind::Area => "Area",
            ChartKind::Pie => "Pie",
            ChartKind::Scatter => "Scatter",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChartSeries {
    /// A cell holding the name, or a text literal.
    pub name: Option<Arc<Expr>>,
    /// Category labels (x values for scatter charts).
    pub categories: Option<Arc<Expr>>,
    pub values: Arc<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Chart {
    pub kind: ChartKind,
    pub title: Option<String>,
    pub series: Vec<ChartSeries>,
    /// The cell the chart's top-left corner sits in, and the offset within it.
    pub anchor: CellRef,
    pub offset: (f64, f64),
    /// Width and height in pixels.
    pub size: (f64, f64),
    /// Column, bar and area charts: stack the series instead of side by side.
    pub stacked: bool,
}

/// A chart's data, read from the workbook.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChartData {
    pub categories: Vec<String>,
    /// Numeric categories (scatter x values), when they all are numbers.
    pub x: Option<Vec<f64>>,
    pub series: Vec<(String, Vec<Option<f64>>)>,
}

impl Chart {
    fn exprs_mut(&mut self) -> impl Iterator<Item = &mut Arc<Expr>> {
        self.series.iter_mut().flat_map(|s| s.name.iter_mut().chain(s.categories.iter_mut()).chain(std::iter::once(&mut s.values)))
    }

    /// Follow inserted/deleted lines: data references on any sheet; the
    /// chart's own position when `on_sheet` is the shifted sheet.
    pub fn apply_shift(&mut self, op: Shift, home: SheetId) {
        for e in self.exprs_mut() {
            *e = Arc::new(e.shifted(op, home));
        }
        if op.sheet() == home {
            let rows = matches!(op, Shift::InsertRows { .. } | Shift::DeleteRows { .. });
            let pos = if rows { self.anchor.row } else { self.anchor.col };
            // A chart whose anchor line is deleted stays where the lines closed up.
            let new = op.map_pos(pos).unwrap_or(match op {
                Shift::DeleteRows { at, .. } | Shift::DeleteCols { at, .. } => at,
                _ => pos,
            });
            if rows {
                self.anchor.row = new;
            } else {
                self.anchor.col = new;
            }
        }
    }

    pub(crate) fn apply_move(&mut self, mv: &Move, home: SheetId) {
        for e in self.exprs_mut() {
            *e = Arc::new(e.moved(mv, home));
        }
    }
}

/// Cells of a reference, row by row (a single row or column in practice):
/// values and how they're displayed.
fn cells_of(wb: &Workbook, home: SheetId, e: &Expr) -> Vec<(Value, String)> {
    let Some((sheet, r)) = e.reference() else {
        return match e {
            Expr::Text(t) => vec![(Value::Text(t.clone()), t.to_string())],
            Expr::Number(n) => vec![(Value::Number(*n), crate::workbook::precise(*n))],
            _ => Vec::new(),
        };
    };
    let sheet = sheet.unwrap_or(home);
    // Whole columns: stop at the used area.
    let end_row = wb.store(sheet).extent().map_or(r.end.row, |x| r.end.row.min(x.row));
    let r = Range::new(r.start, CellRef::new(end_row.max(r.start.row), r.end.col));
    r.iter().map(|c| (wb.value(Addr::new(sheet, c)).clone(), wb.display(Addr::new(sheet, c)).text)).collect()
}

impl Workbook {
    /// Read a chart's data on sheet `home`.
    pub fn chart_data(&self, home: SheetId, chart: &Chart) -> ChartData {
        let mut out = ChartData::default();
        for (i, s) in chart.series.iter().enumerate() {
            let values: Vec<Option<f64>> = cells_of(self, home, &s.values).iter().map(|(v, _)| if let Value::Number(n) = v { Some(*n) } else { None }).collect();
            let name = s
                .name
                .as_ref()
                .and_then(|n| cells_of(self, home, n).into_iter().next())
                .map(|(_, shown)| shown)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("Series {}", i + 1));
            if out.categories.is_empty()
                && let Some(c) = &s.categories
            {
                let cats = cells_of(self, home, c);
                out.x = cats.iter().map(|(v, _)| if let Value::Number(n) = v { Some(*n) } else { None }).collect();
                out.categories = cats.into_iter().map(|(_, shown)| shown).collect();
            }
            out.series.push((name, values));
        }
        let n = out.series.iter().map(|s| s.1.len()).max().unwrap_or(0);
        if out.categories.is_empty() {
            out.categories = (1..=n).map(|i| i.to_string()).collect();
        }
        out
    }
}

/// A tiny chart drawn inside one cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SparkKind {
    Line,
    Column,
    WinLoss,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sparkline {
    pub cell: CellRef,
    pub kind: SparkKind,
    /// The data: a row or column of cells.
    pub data: Arc<Expr>,
    pub color: Option<crate::style::Rgb>,
}

impl Sparkline {
    /// False if its cell was deleted.
    pub(crate) fn apply_shift(&mut self, op: Shift, home: SheetId) -> bool {
        self.data = Arc::new(self.data.shifted(op, home));
        if op.sheet() != home {
            return true;
        }
        let rows = matches!(op, Shift::InsertRows { .. } | Shift::DeleteRows { .. });
        let pos = if rows { self.cell.row } else { self.cell.col };
        match op.map_pos(pos) {
            Some(p) if rows => self.cell.row = p,
            Some(p) => self.cell.col = p,
            None => return false,
        }
        true
    }

    pub(crate) fn apply_move(&mut self, mv: &Move, home: SheetId) {
        self.data = Arc::new(self.data.moved(mv, home));
    }
}

impl Workbook {
    /// A sparkline's numbers (blanks and text skipped as gaps).
    pub fn sparkline_values(&self, home: SheetId, s: &Sparkline) -> Vec<Option<f64>> {
        cells_of(self, home, &s.data).iter().map(|(v, _)| if let Value::Number(n) = v { Some(*n) } else { None }).collect()
    }
}
