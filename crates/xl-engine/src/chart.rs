//! Charts: a kind, data series given as references, and a place on the
//! sheet. The data is read from the workbook when the chart is drawn.
//!
//! Beyond kind, title and series, a chart carries per-series formatting
//! (combo kinds, secondary axis, colors, markers, labels, trendlines, error
//! bars) and chart-wide options (axes, legend, style colors, histogram bins,
//! waterfall totals). The statistics behind them live in [`fit`].

use std::sync::Arc;

use crate::address::{Addr, CellRef, Range, SheetId};
use crate::formula::{Expr, Move, Shift};
use crate::style::Rgb;
use crate::value::Value;
use crate::workbook::Workbook;

pub mod fit;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChartKind {
    Column,
    Bar,
    Line,
    Area,
    Pie,
    Scatter,
    Doughnut,
    Radar,
    Bubble,
    /// Columns counting how many values fall in each bin (a chartex part).
    Histogram,
    /// Floating columns for a running total (a chartex part).
    Waterfall,
}

impl ChartKind {
    pub const ALL: [ChartKind; 11] = [
        ChartKind::Column,
        ChartKind::Bar,
        ChartKind::Line,
        ChartKind::Area,
        ChartKind::Pie,
        ChartKind::Doughnut,
        ChartKind::Scatter,
        ChartKind::Bubble,
        ChartKind::Radar,
        ChartKind::Histogram,
        ChartKind::Waterfall,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ChartKind::Column => "Column",
            ChartKind::Bar => "Bar",
            ChartKind::Line => "Line",
            ChartKind::Area => "Area",
            ChartKind::Pie => "Pie",
            ChartKind::Scatter => "Scatter",
            ChartKind::Doughnut => "Doughnut",
            ChartKind::Radar => "Radar",
            ChartKind::Bubble => "Bubble",
            ChartKind::Histogram => "Histogram",
            ChartKind::Waterfall => "Waterfall",
        }
    }

    /// Kinds stored as Office 2016 "chartex" parts rather than DrawingML charts.
    pub fn is_chartex(self) -> bool {
        matches!(self, ChartKind::Histogram | ChartKind::Waterfall)
    }

    /// Pie-like kinds without axes.
    pub fn is_round(self) -> bool {
        matches!(self, ChartKind::Pie | ChartKind::Doughnut)
    }

    /// Kinds whose x values are numbers (a value axis along the bottom).
    pub fn is_xy(self) -> bool {
        matches!(self, ChartKind::Scatter | ChartKind::Bubble)
    }

    /// Kinds that can be mixed in one category chart (a combo chart).
    pub fn combinable(self) -> bool {
        matches!(self, ChartKind::Column | ChartKind::Bar | ChartKind::Line | ChartKind::Area)
    }
}

/// How a line or an area is painted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Paint {
    /// The chart's palette color.
    #[default]
    Auto,
    /// Not drawn.
    None,
    Solid(Rgb),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Dash {
    #[default]
    Solid,
    Dash,
    Dot,
    DashDot,
    LongDash,
}

impl Dash {
    pub const ALL: [Dash; 5] = [Dash::Solid, Dash::Dash, Dash::Dot, Dash::DashDot, Dash::LongDash];

    pub fn label(self) -> &'static str {
        match self {
            Dash::Solid => "solid",
            Dash::Dash => "dash",
            Dash::Dot => "dot",
            Dash::DashDot => "dashdot",
            Dash::LongDash => "longdash",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarkerShape {
    None,
    Circle,
    Square,
    Diamond,
    Triangle,
    X,
    Plus,
    Star,
    Dash,
    Dot,
}

impl MarkerShape {
    pub const ALL: [MarkerShape; 10] = [
        MarkerShape::None,
        MarkerShape::Circle,
        MarkerShape::Square,
        MarkerShape::Diamond,
        MarkerShape::Triangle,
        MarkerShape::X,
        MarkerShape::Plus,
        MarkerShape::Star,
        MarkerShape::Dash,
        MarkerShape::Dot,
    ];

    /// The name DrawingML (and the chart form) uses.
    pub fn name(self) -> &'static str {
        match self {
            MarkerShape::None => "none",
            MarkerShape::Circle => "circle",
            MarkerShape::Square => "square",
            MarkerShape::Diamond => "diamond",
            MarkerShape::Triangle => "triangle",
            MarkerShape::X => "x",
            MarkerShape::Plus => "plus",
            MarkerShape::Star => "star",
            MarkerShape::Dash => "dash",
            MarkerShape::Dot => "dot",
        }
    }

    pub fn from_name(s: &str) -> Option<MarkerShape> {
        MarkerShape::ALL.into_iter().find(|m| m.name() == s)
    }
}

/// A series' point markers; `None` fields mean automatic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Marker {
    pub shape: Option<MarkerShape>,
    /// 2–72 points.
    pub size: Option<u8>,
}

/// What a series' data labels show (all false: no labels).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DataLabels {
    pub value: bool,
    pub category: bool,
    pub series: bool,
    pub percent: bool,
}

impl DataLabels {
    pub fn any(&self) -> bool {
        self.value || self.category || self.series || self.percent
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrendKind {
    Linear,
    Exponential,
    Logarithmic,
    /// Order 2–6.
    Polynomial(u8),
    Power,
    /// Period 2 or more.
    MovingAverage(u8),
}

impl TrendKind {
    pub fn label(self) -> String {
        match self {
            TrendKind::Linear => "linear".into(),
            TrendKind::Exponential => "exponential".into(),
            TrendKind::Logarithmic => "logarithmic".into(),
            TrendKind::Polynomial(n) => format!("poly {n}"),
            TrendKind::Power => "power".into(),
            TrendKind::MovingAverage(n) => format!("average {n}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Trendline {
    pub kind: TrendKind,
    /// Show the fitted equation / R² on the chart.
    pub equation: bool,
    pub r_squared: bool,
    pub name: Option<String>,
}

impl Trendline {
    pub fn new(kind: TrendKind) -> Trendline {
        Trendline { kind, equation: false, r_squared: false, name: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// The same amount at every point.
    Fixed,
    /// A percentage of each value.
    Percent,
    /// A number of standard deviations, around the series mean.
    StdDev,
    /// The standard error of the mean, at every point.
    StdErr,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ErrorDir {
    #[default]
    Both,
    Plus,
    Minus,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ErrorBars {
    pub kind: ErrorKind,
    /// The amount, percentage or number of deviations (unused for StdErr).
    pub value: f64,
    pub dir: ErrorDir,
}

/// Everything about how one series is drawn; the default is "automatic".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SeriesFormat {
    /// Combo charts: this series' own kind (None: the chart's).
    pub kind: Option<ChartKind>,
    /// Plotted against the secondary value axis on the right.
    pub secondary: bool,
    pub fill: Paint,
    pub line: Paint,
    /// Line width in points.
    pub line_width: Option<f64>,
    pub dash: Dash,
    pub marker: Marker,
    pub smooth: bool,
    pub labels: DataLabels,
    pub trendline: Option<Trendline>,
    pub error_bars: Option<ErrorBars>,
    /// Fill colors of single points (pie slices, waterfall bars…).
    pub points: Vec<(u32, Rgb)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChartSeries {
    /// A cell holding the name, or a text literal.
    pub name: Option<Arc<Expr>>,
    /// Category labels (x values for scatter charts).
    pub categories: Option<Arc<Expr>>,
    pub values: Arc<Expr>,
    /// Bubble charts: the bubble sizes.
    pub sizes: Option<Arc<Expr>>,
    pub format: SeriesFormat,
}

impl ChartSeries {
    pub fn new(name: Option<Arc<Expr>>, categories: Option<Arc<Expr>>, values: Arc<Expr>) -> ChartSeries {
        ChartSeries { name, categories, values, sizes: None, format: SeriesFormat::default() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LegendPos {
    Bottom,
    Top,
    Left,
    Right,
    TopRight,
}

impl LegendPos {
    pub const ALL: [LegendPos; 5] = [LegendPos::Bottom, LegendPos::Top, LegendPos::Left, LegendPos::Right, LegendPos::TopRight];

    pub fn label(self) -> &'static str {
        match self {
            LegendPos::Bottom => "bottom",
            LegendPos::Top => "top",
            LegendPos::Left => "left",
            LegendPos::Right => "right",
            LegendPos::TopRight => "topright",
        }
    }
}

/// One axis: bounds and scale (value axes), title, labels and gridlines.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Axis {
    pub title: Option<String>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub major: Option<f64>,
    /// Logarithmic scale with this base.
    pub log_base: Option<f64>,
    /// Number format of the labels (None: General / the data's).
    pub num_fmt: Option<String>,
    pub gridlines: bool,
    pub minor_gridlines: bool,
    /// Not shown (labels and line).
    pub hidden: bool,
}

/// How a histogram groups its values.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum BinMode {
    #[default]
    Auto,
    Width(f64),
    Count(u32),
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Bins {
    pub mode: BinMode,
    /// Values above this go in one "> overflow" bin.
    pub overflow: Option<f64>,
    /// Values at or below this go in one "≤ underflow" bin.
    pub underflow: Option<f64>,
}

/// Chart-wide options beyond kind, title and series.
#[derive(Clone, Debug, PartialEq)]
pub struct ChartOptions {
    pub legend: Option<LegendPos>,
    /// Category axis (x values for scatter and bubble charts).
    pub x_axis: Axis,
    /// The value axis, and the secondary one (used by series marked secondary).
    pub y_axis: Axis,
    pub y2_axis: Axis,
    /// Excel's chart style number (1–48), which picks its colors. None: default.
    pub style: Option<u8>,
    /// No title is shown, not even the single series' name.
    pub auto_title_deleted: bool,
    /// Doughnut hole, percent of the radius.
    pub hole_size: u8,
    /// Radar charts: filled areas instead of lines.
    pub radar_filled: bool,
    pub bins: Bins,
    /// Waterfall charts: points (0-based) that are totals, not changes.
    pub subtotals: Vec<u32>,
}

impl Default for ChartOptions {
    /// What a new chart looks like.
    fn default() -> ChartOptions {
        ChartOptions {
            legend: Some(LegendPos::Bottom),
            x_axis: Axis::default(),
            y_axis: Axis { gridlines: true, ..Axis::default() },
            y2_axis: Axis::default(),
            style: None,
            auto_title_deleted: false,
            hole_size: 50,
            radar_filled: false,
            bins: Bins::default(),
            subtotals: Vec::new(),
        }
    }
}

impl ChartOptions {
    /// Nothing set: what reading a file starts from.
    pub fn blank() -> ChartOptions {
        ChartOptions { legend: None, y_axis: Axis::default(), ..ChartOptions::default() }
    }
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
    /// A pivot chart: the pivot table (by name) whose output it plots; its
    /// series are rewritten whenever the pivot is.
    pub pivot: Option<String>,
    pub options: ChartOptions,
}

/// A chart's data, read from the workbook.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChartData {
    pub categories: Vec<String>,
    /// Numeric categories (scatter x values), when they all are numbers.
    pub x: Option<Vec<f64>>,
    pub series: Vec<(String, Vec<Option<f64>>)>,
    /// Scatter and bubble charts: each series' own x values (empty if none).
    pub series_x: Vec<Vec<Option<f64>>>,
    /// Bubble charts: each series' bubble sizes (empty if none).
    pub sizes: Vec<Vec<Option<f64>>>,
}

impl Chart {
    /// A chart with default options at `anchor` (top-left of that cell).
    pub fn new(kind: ChartKind, series: Vec<ChartSeries>, anchor: CellRef, size: (f64, f64)) -> Chart {
        let mut options = ChartOptions::default();
        if series.len() < 2 && !kind.is_round() {
            options.legend = None;
        }
        Chart { kind, title: None, series, anchor, offset: (0.0, 0.0), size, stacked: false, options, pivot: None }
    }

    /// The kind a series is drawn as.
    pub fn series_kind(&self, i: usize) -> ChartKind {
        match self.series.get(i).and_then(|s| s.format.kind) {
            Some(k) if self.kind.combinable() && k.combinable() => k,
            _ => self.kind,
        }
    }

    /// Does any series use the secondary axis?
    pub fn has_secondary(&self) -> bool {
        !self.kind.is_round() && !self.kind.is_chartex() && self.kind != ChartKind::Radar && self.series.iter().any(|s| s.format.secondary)
    }

    /// Everything but where the chart is: equal looks mean an unedited chart.
    pub fn same_look(&self, other: &Chart) -> bool {
        (self.kind, &self.title, &self.series, self.stacked, &self.options) == (other.kind, &other.title, &other.series, other.stacked, &other.options)
    }

    fn exprs_mut(&mut self) -> impl Iterator<Item = &mut Arc<Expr>> {
        self.series.iter_mut().flat_map(|s| s.name.iter_mut().chain(s.categories.iter_mut()).chain(s.sizes.iter_mut()).chain(std::iter::once(&mut s.values)))
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

    pub fn apply_move(&mut self, mv: &Move, home: SheetId) {
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

fn numbers(cells: &[(Value, String)]) -> Vec<Option<f64>> {
    cells.iter().map(|(v, _)| if let Value::Number(n) = v { Some(*n) } else { None }).collect()
}

impl Workbook {
    /// Read a chart's data on sheet `home`. Histograms come back binned:
    /// one category per bin and the counts as the series.
    pub fn chart_data(&self, home: SheetId, chart: &Chart) -> ChartData {
        let mut out = ChartData::default();
        for (i, s) in chart.series.iter().enumerate() {
            let values = numbers(&cells_of(self, home, &s.values));
            let name = s
                .name
                .as_ref()
                .and_then(|n| cells_of(self, home, n).into_iter().next())
                .map(|(_, shown)| shown)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("Series {}", i + 1));
            let cats = s.categories.as_ref().map(|c| cells_of(self, home, c));
            if out.categories.is_empty()
                && let Some(cats) = &cats
            {
                out.x = cats.iter().map(|(v, _)| if let Value::Number(n) = v { Some(*n) } else { None }).collect();
                out.categories = cats.iter().map(|(_, shown)| shown.clone()).collect();
            }
            out.series_x.push(if chart.kind.is_xy() { cats.as_deref().map(numbers).unwrap_or_default() } else { Vec::new() });
            out.sizes.push(s.sizes.as_ref().map(|e| numbers(&cells_of(self, home, e))).unwrap_or_default());
            out.series.push((name, values));
        }
        if chart.kind == ChartKind::Histogram {
            let values: Vec<f64> = out.series.first().map(|s| s.1.iter().flatten().copied().collect()).unwrap_or_default();
            let name = out.series.first().map_or_else(|| "Series 1".to_string(), |s| s.0.clone());
            let bins = fit::histogram(&values, &chart.options.bins);
            return ChartData {
                categories: bins.iter().map(|b| b.label.clone()).collect(),
                x: None,
                series: vec![(name, bins.iter().map(|b| Some(b.count as f64)).collect())],
                series_x: vec![Vec::new()],
                sizes: vec![Vec::new()],
            };
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
    pub fn apply_shift(&mut self, op: Shift, home: SheetId) -> bool {
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
        numbers(&cells_of(self, home, &s.data))
    }
}
