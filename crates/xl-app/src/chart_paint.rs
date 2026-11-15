//! Drawing a chart: frame, title, legend, axes with tidy (or set) ticks,
//! and the marks for each kind — columns, bars, lines, areas, scatter and
//! bubbles (mixed in combo charts, on two value axes), pies and doughnuts,
//! radar, histograms and waterfalls — with per-series colors, line styles,
//! markers, data labels, trendlines and error bars.

use gpui::{Bounds, FontWeight, Hsla, PathBuilder, Pixels, Window, fill, point, px};
use xl_engine::chart::fit::{self, StepKind};
use xl_engine::chart::{Axis, Chart, ChartData, ChartKind, Dash, LegendPos, MarkerShape, Paint, TrendKind};
use xl_engine::precise;

use crate::app::Spreadsheet;
use crate::paint::{Op, cell_font, outline, rect, shape};

/// Series colors, in order (the app's default palette).
pub(crate) fn series_color(sheet: &Spreadsheet, i: usize) -> Hsla {
    let palette = sheet.theme.ref_colors;
    palette[i % palette.len()]
}

fn rgb(c: u32) -> Hsla {
    gpui::rgb(c & 0xFF_FFFF).into()
}

/// Excel's Office accents, for monochrome chart styles.
const ACCENTS: [u32; 6] = [0x4472C4, 0xED7D31, 0xA5A5A5, 0xFFC000, 0x5B9BD5, 0x70AD47];

/// The automatic color of item `i` under the chart's style: Excel styles
/// 1, 9, 17… are grays, 3–8 (and every 8 after) shades of one accent, the
/// rest the colorful default.
pub(crate) fn auto_color(sheet: &Spreadsheet, chart: &Chart, i: usize) -> Hsla {
    let Some(style) = chart.options.style else { return series_color(sheet, i) };
    let col = (style.clamp(1, 48) as usize - 1) % 8;
    let shade = |base: Hsla| {
        // Alternate darker and lighter around the base.
        let steps = [0.0, -0.18, 0.18, -0.32, 0.3, -0.08, 0.1];
        Hsla { l: (base.l + steps[i % steps.len()]).clamp(0.12, 0.88), ..base }
    };
    match col {
        0 => shade(rgb(0x7F7F7F)),
        1 => series_color(sheet, i),
        k => shade(rgb(ACCENTS[k - 2])),
    }
}

/// A series' fill (None: not filled).
fn fill_of(sheet: &Spreadsheet, chart: &Chart, i: usize) -> Option<Hsla> {
    match chart.series.get(i).map_or(Paint::Auto, |s| s.format.fill) {
        Paint::Auto => Some(auto_color(sheet, chart, i)),
        Paint::None => None,
        Paint::Solid(c) => Some(rgb(c)),
    }
}

/// A series' line color (lines fall back to the fill color when automatic).
fn stroke_of(sheet: &Spreadsheet, chart: &Chart, i: usize) -> Option<Hsla> {
    let f = chart.series.get(i).map(|s| &s.format);
    match f.map_or(Paint::Auto, |f| f.line) {
        Paint::Auto => match f.map_or(Paint::Auto, |f| f.fill) {
            Paint::Solid(c) => Some(rgb(c)),
            _ => Some(auto_color(sheet, chart, i)),
        },
        Paint::None => None,
        Paint::Solid(c) => Some(rgb(c)),
    }
}

/// About `target` tick intervals covering [lo, hi], on 1/2/2.5/5 × 10ⁿ steps.
pub(crate) fn nice_ticks(lo: f64, hi: f64, target: usize) -> (f64, f64, f64) {
    let (lo, hi) = if hi > lo { (lo, hi) } else if hi == 0.0 { (0.0, 1.0) } else { (lo.min(0.0), hi.max(0.0) + hi.abs().max(1.0) * 0.1) };
    let raw = (hi - lo) / target.max(1) as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(10.0 * mag);
    ((lo / step).floor() * step, (hi / step).ceil() * step, step)
}

fn label(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 {
        format!("{}B", precise((v / 1e9 * 100.0).round() / 100.0))
    } else if a >= 1e6 {
        format!("{}M", precise((v / 1e6 * 100.0).round() / 100.0))
    } else if a >= 1e4 {
        format!("{}K", precise((v / 1e3 * 100.0).round() / 100.0))
    } else {
        precise((v * 1e6).round() / 1e6)
    }
}

fn axis_label(v: f64, ax: &Axis) -> String {
    match &ax.num_fmt {
        Some(code) => xl_engine::NumberFormat::parse(code).format(&xl_engine::Value::Number(v)).text.trim().to_string(),
        None => label(v),
    }
}

/// A value axis' range and ticks, after the axis' own bounds and scale.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Scale {
    pub lo: f64,
    pub hi: f64,
    pub ticks: Vec<f64>,
    pub log: Option<f64>,
}

impl Scale {
    /// Ticks for data spanning [lo, hi].
    pub(crate) fn new(lo: f64, hi: f64, ax: &Axis) -> Scale {
        if let Some(base) = ax.log_base.filter(|b| *b > 1.0) {
            let lg = |v: f64| v.ln() / base.ln();
            let mn = ax.min.filter(|m| *m > 0.0).unwrap_or(if lo > 0.0 { lo } else { 1.0 });
            let mx = ax.max.filter(|m| *m > mn).unwrap_or(hi.max(mn));
            let (a, mut b) = (lg(mn).floor(), lg(mx).ceil());
            if b <= a {
                b = a + 1.0;
            }
            let lo = ax.min.filter(|m| *m > 0.0).unwrap_or(base.powf(a));
            let hi = ax.max.filter(|m| *m > lo).unwrap_or(base.powf(b));
            let ticks = (a as i32..=b as i32).map(|p| base.powi(p)).filter(|t| *t >= lo * 0.999 && *t <= hi * 1.001).take(40).collect();
            return Scale { lo, hi, ticks, log: Some(base) };
        }
        let (mut vlo, mut vhi, mut step) = nice_ticks(ax.min.unwrap_or(lo), ax.max.unwrap_or(hi), 5);
        if let Some(m) = ax.min {
            vlo = m;
        }
        if let Some(m) = ax.max.filter(|m| *m > vlo) {
            vhi = m;
        }
        if vhi <= vlo {
            vhi = vlo + step.max(1.0);
        }
        if let Some(m) = ax.major.filter(|m| *m > 0.0 && (vhi - vlo) / m <= 200.0) {
            step = m;
        }
        let first = if ax.min.is_some() { vlo } else { (vlo / step).ceil() * step };
        let ticks = (0..201).map(|k| first + step * k as f64).take_while(|t| *t <= vhi + step * 1e-9).collect();
        Scale { lo: vlo, hi: vhi, ticks, log: None }
    }

    /// Where `v` falls, 0 at `lo` and 1 at `hi` (NaN where a log scale can't go).
    pub(crate) fn frac(&self, v: f64) -> f64 {
        match self.log {
            Some(_) if v <= 0.0 => f64::NAN,
            Some(_) => (v.ln() - self.lo.ln()) / (self.hi.ln() - self.lo.ln()),
            None => (v - self.lo) / (self.hi - self.lo),
        }
    }

    /// The baseline bars grow from.
    fn base(&self) -> f64 {
        if self.log.is_some() { self.lo } else { 0.0f64.clamp(self.lo, self.hi) }
    }
}

fn path(build: impl FnOnce(&mut PathBuilder), stroke: Option<f32>, dash: Dash) -> Option<gpui::Path> {
    let mut p = match stroke {
        Some(w) => {
            let p = PathBuilder::stroke(px(w));
            let unit = w.max(1.0);
            match dash {
                Dash::Solid => p,
                Dash::Dash => p.dash_array(&[px(4.0 * unit), px(3.0 * unit)]),
                Dash::Dot => p.dash_array(&[px(unit), px(2.0 * unit)]),
                Dash::DashDot => p.dash_array(&[px(4.0 * unit), px(2.0 * unit), px(unit), px(2.0 * unit)]),
                Dash::LongDash => p.dash_array(&[px(8.0 * unit), px(3.0 * unit)]),
            }
        }
        None => PathBuilder::fill(),
    };
    build(&mut p);
    p.build().ok()
}

fn pt(x: f64, y: f64) -> gpui::Point<Pixels> {
    point(px(x as f32), px(y as f32))
}

/// A polyline through the points (Catmull-Rom curves when smooth).
fn polyline(p: &mut PathBuilder, pts: &[(f64, f64)], smooth: bool) {
    let Some(&(x0, y0)) = pts.first() else { return };
    p.move_to(pt(x0, y0));
    for i in 1..pts.len() {
        let (b, c) = (pts[i - 1], pts[i]);
        if smooth && pts.len() > 2 {
            let a = if i >= 2 { pts[i - 2] } else { b };
            let d = pts.get(i + 1).copied().unwrap_or(c);
            let c1 = (b.0 + (c.0 - a.0) / 6.0, b.1 + (c.1 - a.1) / 6.0);
            let c2 = (c.0 - (d.0 - b.0) / 6.0, c.1 - (d.1 - b.1) / 6.0);
            p.cubic_bezier_to(pt(c.0, c.1), pt(c1.0, c1.1), pt(c2.0, c2.1));
        } else {
            p.line_to(pt(c.0, c.1));
        }
    }
}

/// Draws a point marker.
fn marker(ops: &mut Vec<Op>, shape_: MarkerShape, (x, y): (f64, f64), d: f64, color: Hsla, bg: Hsla, clip: Bounds<Pixels>) {
    let r = d / 2.0;
    let poly = |pts: &[(f64, f64)]| {
        path(
            |p| {
                p.move_to(pt(pts[0].0, pts[0].1));
                for q in &pts[1..] {
                    p.line_to(pt(q.0, q.1));
                }
                p.close();
            },
            None,
            Dash::Solid,
        )
    };
    type Seg = ((f64, f64), (f64, f64));
    let lines = |ops: &mut Vec<Op>, segs: &[Seg]| {
        for (a, b) in segs {
            if let Some(l) = path(
                |p| {
                    p.move_to(pt(a.0, a.1));
                    p.line_to(pt(b.0, b.1));
                },
                Some(1.6),
                Dash::Solid,
            ) {
                ops.push(Op::Path(l, color, clip));
            }
        }
    };
    match shape_ {
        MarkerShape::None => {}
        MarkerShape::Circle => ops.push(Op::Quad(outline(rect(x - r, y - r, d, d), 1.5, color, bg, r as f32), clip)),
        MarkerShape::Dot => ops.push(Op::Quad(outline(rect(x - r * 0.6, y - r * 0.6, d * 0.6, d * 0.6), 1.0, color, color, (r * 0.6) as f32), clip)),
        MarkerShape::Square => ops.push(Op::Quad(outline(rect(x - r, y - r, d, d), 1.5, color, bg, 1.0), clip)),
        MarkerShape::Diamond => {
            if let Some(p) = poly(&[(x, y - r * 1.2), (x + r * 1.2, y), (x, y + r * 1.2), (x - r * 1.2, y)]) {
                ops.push(Op::Path(p, color, clip));
            }
        }
        MarkerShape::Triangle => {
            if let Some(p) = poly(&[(x, y - r * 1.2), (x + r * 1.1, y + r * 0.8), (x - r * 1.1, y + r * 0.8)]) {
                ops.push(Op::Path(p, color, clip));
            }
        }
        MarkerShape::X => lines(ops, &[((x - r, y - r), (x + r, y + r)), ((x - r, y + r), (x + r, y - r))]),
        MarkerShape::Plus => lines(ops, &[((x - r, y), (x + r, y)), ((x, y - r), (x, y + r))]),
        MarkerShape::Star => lines(ops, &[((x - r, y - r), (x + r, y + r)), ((x - r, y + r), (x + r, y - r)), ((x, y - r), (x, y + r))]),
        MarkerShape::Dash => lines(ops, &[((x - r, y), (x + r, y))]),
    }
}

/// Legend entries: series (categories for pies, step kinds for waterfalls).
fn legend_entries(sheet: &Spreadsheet, chart: &Chart, data: &ChartData) -> Vec<(String, Hsla)> {
    match chart.kind {
        k if k.is_round() => data
            .categories
            .iter()
            .enumerate()
            .map(|(i, c)| (c.clone(), point_color(sheet, chart, 0, i).unwrap_or_else(|| auto_color(sheet, chart, i))))
            .collect(),
        ChartKind::Waterfall => ["Increase", "Decrease", "Total"].iter().enumerate().map(|(i, n)| (n.to_string(), auto_color(sheet, chart, i))).collect(),
        _ => data.series.iter().enumerate().map(|(i, s)| (s.0.clone(), fill_of(sheet, chart, i).or_else(|| stroke_of(sheet, chart, i)).unwrap_or(sheet.theme.ink_faint))).collect(),
    }
}

/// A point's own color, if it has one.
fn point_color(_sheet: &Spreadsheet, chart: &Chart, series: usize, i: usize) -> Option<Hsla> {
    chart.series.get(series)?.format.points.iter().find(|(p, _)| *p as usize == i).map(|(_, c)| rgb(*c))
}

struct Painter<'a> {
    sheet: &'a Spreadsheet,
    ops: Vec<Op>,
    clip: Bounds<Pixels>,
}

impl Painter<'_> {
    fn text(&self, window: &mut Window, s: &str, size: f32, color: Hsla, bold: bool) -> gpui::ShapedLine {
        let f = gpui::Font { weight: if bold { FontWeight::SEMIBOLD } else { FontWeight::NORMAL }, ..cell_font() };
        shape(window, s.to_string(), f, color, size)
    }

    fn put(&mut self, line: gpui::ShapedLine, x: f64, y: f64, lh: f64) {
        self.ops.push(Op::Text { line: Box::new(line), origin: pt(x, y), clip: self.clip, line_height: px(lh as f32) });
    }

    /// Text centered on (cx, y).
    fn put_centered(&mut self, window: &mut Window, s: &str, cx: f64, y: f64, size: f32, color: Hsla) {
        let line = self.text(window, s, size, color, false);
        let w = f64::from(line.width);
        self.put(line, cx - w / 2.0, y, 18.0);
    }

    fn quad(&mut self, x: f64, y: f64, w: f64, h: f64, color: Hsla, clip: Bounds<Pixels>) {
        self.ops.push(Op::Quad(fill(rect(x, y, w, h), color), clip));
    }

    fn stroke(&mut self, pts: &[(f64, f64)], width: f32, dash: Dash, smooth: bool, color: Hsla, clip: Bounds<Pixels>) {
        if pts.len() > 1
            && let Some(p) = path(|p| polyline(p, pts, smooth), Some(width), dash)
        {
            self.ops.push(Op::Path(p, color, clip));
        }
    }
}

pub(crate) fn chart_ops(sheet: &Spreadsheet, chart: &Chart, data: &ChartData, (x, y, w, h): (f64, f64, f64, f64), clip: Bounds<Pixels>, selected: bool, window: &mut Window) -> Vec<Op> {
    let t = sheet.theme;
    let mut p = Painter { sheet, ops: Vec::new(), clip };
    // Frame.
    p.ops.push(Op::Quad(outline(rect(x, y, w, h), if selected { 2.0 } else { 1.0 }, if selected { t.accent } else { t.divider }, t.canvas, 6.0), clip));

    let mut top = y + 10.0;
    let auto_title = (data.series.len() == 1 && !chart.options.auto_title_deleted).then(|| data.series[0].0.clone());
    let title = chart.title.clone().or(auto_title);
    let editing_title = sheet.field == Some(crate::app::Field::ChartTitle) && selected;
    let title = if editing_title { Some(sheet.chart_title_text.clone()) } else { title };
    if let Some(title) = title {
        let line = p.text(window, &title, 15.0, t.ink, true);
        let tw = f64::from(line.width);
        let tx = x + (w - tw) / 2.0;
        p.put(line, tx, top, 22.0);
        if editing_title {
            p.ops.push(Op::Quad(fill(rect(tx + tw + 1.0, top + 3.0, 1.5, 16.0), t.accent), clip));
        }
        top += 28.0;
    }

    // Legend: shrinks the plot from its side.
    let (mut left, mut right, mut bottom) = (x + 10.0, x + w - 10.0, y + h - 10.0);
    if let Some(pos) = chart.options.legend {
        let entries = legend_entries(sheet, chart, data);
        let lines: Vec<_> = entries.iter().map(|(name, _)| p.text(window, name, 12.5, t.ink_muted, false)).collect();
        match pos {
            LegendPos::Bottom | LegendPos::Top | LegendPos::TopRight => {
                let total: f64 = lines.iter().map(|l| f64::from(l.width) + 26.0).sum();
                let mut lx = if pos == LegendPos::TopRight { (right - total).max(left) } else { x + ((w - total) / 2.0).max(10.0) };
                let ly = if pos == LegendPos::Bottom { bottom - 18.0 } else { top };
                for (line, (_, color)) in lines.into_iter().zip(&entries) {
                    if lx > x + w - 30.0 {
                        break;
                    }
                    p.quad(lx, ly + 5.0, 10.0, 10.0, *color, clip);
                    let lw = f64::from(line.width);
                    p.put(line, lx + 14.0, ly, 20.0);
                    lx += lw + 26.0;
                }
                if pos == LegendPos::Bottom {
                    bottom -= 24.0;
                } else {
                    top += 24.0;
                }
            }
            LegendPos::Left | LegendPos::Right => {
                let lw = lines.iter().map(|l| f64::from(l.width)).fold(0.0, f64::max).min(w * 0.35) + 22.0;
                let lx = if pos == LegendPos::Left { left } else { right - lw };
                let mut ly = (top + bottom) / 2.0 - lines.len() as f64 * 10.0;
                ly = ly.max(top);
                for (line, (_, color)) in lines.into_iter().zip(&entries) {
                    if ly > bottom - 18.0 {
                        break;
                    }
                    p.quad(lx, ly + 5.0, 10.0, 10.0, *color, clip);
                    p.put(line, lx + 14.0, ly, 20.0);
                    ly += 20.0;
                }
                if pos == LegendPos::Left {
                    left += lw + 6.0;
                } else {
                    right -= lw + 6.0;
                }
            }
        }
    }

    let area = (left, top, right, bottom);
    match chart.kind {
        k if k.is_round() => round_chart(&mut p, chart, data, area, window),
        ChartKind::Radar => radar_chart(&mut p, chart, data, area, window),
        _ => cartesian_chart(&mut p, chart, data, area, window),
    }
    let mut ops = p.ops;
    if selected {
        // Resize grip at the bottom-right corner.
        ops.push(Op::Quad(outline(rect(x + w - 5.0, y + h - 5.0, 10.0, 10.0), 1.5, t.accent, t.canvas, 2.0), clip));
    }
    ops
}

/// Label text for a point: the parts the series' data labels ask for.
fn point_label(chart: &Chart, data: &ChartData, si: usize, i: usize, v: f64, total: Option<f64>) -> Option<String> {
    let l = chart.series.get(si)?.format.labels;
    if !l.any() {
        return None;
    }
    let mut parts = Vec::new();
    if l.series {
        parts.push(data.series.get(si).map_or(String::new(), |s| s.0.clone()));
    }
    if l.category {
        parts.push(data.categories.get(i).cloned().unwrap_or_default());
    }
    if l.value {
        parts.push(label(v));
    }
    if l.percent
        && let Some(t) = total.filter(|t| *t > 0.0)
    {
        parts.push(format!("{}%", (v / t * 100.0).round()));
    }
    Some(parts.join(", "))
}

fn round_chart(p: &mut Painter, chart: &Chart, data: &ChartData, (left, top, right, bottom): (f64, f64, f64, f64), window: &mut Window) {
    let (sheet, clip) = (p.sheet, p.clip);
    let values: Vec<f64> = data.series.first().map(|s| s.1.iter().map(|v| v.unwrap_or(0.0).max(0.0)).collect()).unwrap_or_default();
    let total: f64 = values.iter().sum();
    let (cx, cy) = ((left + right) / 2.0, (top + bottom) / 2.0);
    let r = ((bottom - top).min(right - left) / 2.0 - 4.0).max(4.0);
    let hole = if chart.kind == ChartKind::Doughnut { r * f64::from(chart.options.hole_size.clamp(10, 90)) / 100.0 } else { 0.0 };
    if total <= 0.0 {
        return;
    }
    let tau = std::f64::consts::TAU;
    let mut angle = -std::f64::consts::FRAC_PI_2;
    for (i, v) in values.iter().enumerate() {
        if *v <= 0.0 {
            continue;
        }
        let sweep = v / total * tau;
        let (a0, a1) = (angle, angle + sweep);
        let color = point_color(sheet, chart, 0, i).unwrap_or_else(|| auto_color(sheet, chart, i));
        let at = |rad: f64, a: f64| pt(cx + rad * a.cos(), cy + rad * a.sin());
        let wedge = path(
            |b| {
                let full = sweep >= tau - 1e-9;
                let mid = (a0 + a1) / 2.0;
                if hole > 0.0 {
                    b.move_to(at(r, a0));
                    // Two arcs each way keep every arc under half a turn.
                    b.arc_to(pt(r, r), px(0.), false, true, at(r, mid));
                    b.arc_to(pt(r, r), px(0.), false, true, at(r, if full { a0 + tau - 1e-6 } else { a1 }));
                    b.line_to(at(hole, if full { a0 + tau - 1e-6 } else { a1 }));
                    b.arc_to(pt(hole, hole), px(0.), false, false, at(hole, mid));
                    b.arc_to(pt(hole, hole), px(0.), false, false, at(hole, a0));
                } else {
                    b.move_to(pt(cx, cy));
                    b.line_to(at(r, a0));
                    b.arc_to(pt(r, r), px(0.), false, true, at(r, mid));
                    b.arc_to(pt(r, r), px(0.), false, true, at(r, if full { a0 + tau - 1e-6 } else { a1 }));
                }
                b.close();
            },
            None,
            Dash::Solid,
        );
        if let Some(wedge) = wedge {
            p.ops.push(Op::Path(wedge, color, clip));
        }
        if sweep > 0.3
            && let Some(text) = point_label(chart, data, 0, i, *v, Some(total))
        {
            let mid = (a0 + a1) / 2.0;
            let rr = if hole > 0.0 { (r + hole) / 2.0 } else { r * 0.62 };
            let line = p.text(window, &text, 12.0, gpui::white(), true);
            let lw = f64::from(line.width);
            p.put(line, cx + rr * mid.cos() - lw / 2.0, cy + rr * mid.sin() - 9.0, 18.0);
        }
        angle = a1;
    }
}

fn radar_chart(p: &mut Painter, chart: &Chart, data: &ChartData, (left, top, right, bottom): (f64, f64, f64, f64), window: &mut Window) {
    let (sheet, clip) = (p.sheet, p.clip);
    let t = sheet.theme;
    let n = data.categories.len().max(data.series.iter().map(|s| s.1.len()).max().unwrap_or(0));
    if n < 3 {
        return;
    }
    let (cx, cy) = ((left + right) / 2.0, (top + bottom) / 2.0 + 4.0);
    let r = ((bottom - top - 30.0).min(right - left - 60.0) / 2.0).max(10.0);
    let hi = data.series.iter().flat_map(|s| s.1.iter().flatten()).copied().fold(0.0, f64::max);
    let lo = data.series.iter().flat_map(|s| s.1.iter().flatten()).copied().fold(0.0, f64::min);
    let scale = Scale::new(lo, hi, &chart.options.y_axis);
    let spoke = |i: usize| -std::f64::consts::FRAC_PI_2 + i as f64 / n as f64 * std::f64::consts::TAU;
    let at = |i: usize, f: f64| (cx + r * f * spoke(i).cos(), cy + r * f * spoke(i).sin());
    // Rings at the ticks, spokes, category labels.
    for tick in &scale.ticks {
        let f = scale.frac(*tick);
        if !(0.0..=1.0001).contains(&f) {
            continue;
        }
        let ring: Vec<(f64, f64)> = (0..=n).map(|i| at(i % n, f)).collect();
        p.stroke(&ring, 1.0, Dash::Solid, false, t.rule, clip);
        if !chart.options.y_axis.hidden {
            let line = p.text(window, &axis_label(*tick, &chart.options.y_axis), 11.0, t.ink_faint, false);
            p.put(line, cx + 3.0, cy - r * f - 9.0, 16.0);
        }
    }
    for i in 0..n {
        p.stroke(&[(cx, cy), at(i, 1.0)], 1.0, Dash::Solid, false, t.rule, clip);
        let (lx, ly) = at(i, 1.0 + 14.0 / r);
        if let Some(c) = data.categories.get(i) {
            let line = p.text(window, c, 12.0, t.ink_muted, false);
            let lw = f64::from(line.width);
            let ax = if (lx - cx).abs() < 2.0 { lw / 2.0 } else if lx < cx { lw } else { 0.0 };
            p.put(line, lx - ax, ly - 9.0, 18.0);
        }
    }
    for (si, s) in data.series.iter().enumerate() {
        let pts: Vec<(f64, f64)> = (0..n).map(|i| at(i, s.1.get(i).copied().flatten().map_or(0.0, |v| scale.frac(v).clamp(0.0, 1.0)))).collect();
        let f = &chart.series[si].format;
        if chart.options.radar_filled {
            if let Some(mut c) = fill_of(sheet, chart, si)
                && let Some(area) = path(
                    |b| {
                        polyline(b, &pts, false);
                        b.close();
                    },
                    None,
                    Dash::Solid,
                )
            {
                c.a = 0.45;
                p.ops.push(Op::Path(area, c, clip));
            }
        } else if let Some(c) = stroke_of(sheet, chart, si) {
            let mut ring = pts.clone();
            ring.push(pts[0]);
            p.stroke(&ring, f.line_width.map_or(2.0, |w| (w * 4.0 / 3.0) as f32), f.dash, false, c, clip);
            let shape_ = f.marker.shape.unwrap_or(MarkerShape::Circle);
            let d = f.marker.size.map_or(6.0, |s| f64::from(s) * 4.0 / 3.0);
            for q in &pts {
                marker(&mut p.ops, shape_, *q, d, c, t.canvas, clip);
            }
        }
    }
}

/// Columns, bars, lines, areas, scatter and bubbles on value axes (and
/// histograms and waterfalls, which are columns).
fn cartesian_chart(p: &mut Painter, chart: &Chart, data: &ChartData, (left, top, right, bottom): (f64, f64, f64, f64), window: &mut Window) {
    let (sheet, clip) = (p.sheet, p.clip);
    let t = sheet.theme;
    let o = &chart.options;
    let ns = data.series.len();
    let n = data.categories.len().max(data.series.iter().map(|s| s.1.len()).max().unwrap_or(0));
    let kind_of = |i: usize| match chart.kind {
        ChartKind::Histogram | ChartKind::Waterfall => ChartKind::Column,
        _ => chart.series_kind(i),
    };
    let secondary = |i: usize| chart.has_secondary() && chart.series.get(i).is_some_and(|s| s.format.secondary);
    let stackable = |k: ChartKind| chart.stacked && matches!(k, ChartKind::Column | ChartKind::Bar | ChartKind::Area);
    let horizontal = chart.kind == ChartKind::Bar;
    let xy = chart.kind.is_xy();
    let steps = (chart.kind == ChartKind::Waterfall).then(|| fit::waterfall(data.series.first().map_or(&[][..], |s| &s.1[..]), &o.subtotals));

    // Each series' plotted values: stacked running totals where stacked.
    let mut tops: Vec<Vec<Option<(f64, f64)>>> = vec![Vec::new(); ns];
    {
        let mut bases: std::collections::HashMap<(ChartKind, bool), (Vec<f64>, Vec<f64>)> = std::collections::HashMap::new();
        for (si, s) in data.series.iter().enumerate() {
            let k = kind_of(si);
            let key = (if k == ChartKind::Bar { ChartKind::Column } else { k }, secondary(si));
            let (pos, neg) = bases.entry(key).or_insert_with(|| (vec![0.0; n], vec![0.0; n]));
            tops[si] = (0..n)
                .map(|i| {
                    if let (Some(st), 0) = (&steps, si) {
                        return st.get(i).copied().flatten().map(|s| (s.start, s.end));
                    }
                    let v = s.1.get(i).copied().flatten()?;
                    if stackable(k) {
                        let base = if v >= 0.0 { &mut pos[i] } else { &mut neg[i] };
                        let from = *base;
                        *base += v;
                        Some((from, *base))
                    } else {
                        Some((0.0, v))
                    }
                })
                .collect();
        }
    }
    let error_ranges: Vec<Vec<Option<(f64, f64)>>> = (0..ns).map(|si| chart.series[si].format.error_bars.map(|e| fit::error_ranges(&data.series[si].1, &e)).unwrap_or_default()).collect();
    // Value ranges per axis.
    let range_of = |sec: bool| -> (f64, f64, bool) {
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        let mut any_bar = false;
        for si in (0..ns).filter(|&si| secondary(si) == sec) {
            let k = kind_of(si);
            any_bar |= matches!(k, ChartKind::Column | ChartKind::Bar | ChartKind::Area);
            for (a, b) in tops[si].iter().flatten() {
                for v in [*a, *b] {
                    if v != 0.0 || matches!(k, ChartKind::Column | ChartKind::Bar | ChartKind::Area) || steps.is_some() {
                        lo = lo.min(v);
                        hi = hi.max(v);
                    }
                }
            }
            for (a, b) in error_ranges[si].iter().flatten() {
                lo = lo.min(*a);
                hi = hi.max(*b);
            }
        }
        if !lo.is_finite() {
            return (0.0, 1.0, any_bar);
        }
        (lo, hi, any_bar)
    };
    let value_scale = |sec: bool, ax: &Axis| -> Scale {
        let (lo, hi, any_bar) = range_of(sec);
        // Bars start at zero; lines needn't when all the values are far from it.
        let (lo, hi) = if any_bar || !(lo > 0.0 && lo > (hi - lo) * 2.0) { (lo.min(0.0), hi.max(0.0)) } else { (lo, hi) };
        Scale::new(lo, hi, ax)
    };
    let ys = value_scale(false, &o.y_axis);
    let y2s = chart.has_secondary().then(|| value_scale(true, &o.y2_axis));
    // Scatter x axis.
    let xs_of = |si: usize| -> Vec<Option<f64>> {
        match data.series_x.get(si).filter(|v| !v.is_empty()) {
            Some(v) => v.clone(),
            None => match &data.x {
                Some(x) => x.iter().map(|v| Some(*v)).collect(),
                None => (1..=n).map(|i| Some(i as f64)).collect(),
            },
        }
    };
    let xscale = xy.then(|| {
        let all: Vec<f64> = (0..ns).flat_map(xs_of).flatten().collect();
        let mn = all.iter().copied().fold(f64::INFINITY, f64::min);
        let mx = all.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let (mn, mx) = if mn.is_finite() { (mn, mx) } else { (0.0, 1.0) };
        let lo = if mn > 0.0 && mn > (mx - mn) { mn } else { mn.min(0.0) };
        Scale::new(lo, mx, &o.x_axis)
    });

    // Labels and room for them.
    let label_lines = |p: &Painter, window: &mut Window, s: &Scale, ax: &Axis| -> Vec<(f64, gpui::ShapedLine)> { s.ticks.iter().map(|v| (*v, p.text(window, &axis_label(*v, ax), 12.0, t.ink_faint, false))).collect() };
    let vlabels = if o.y_axis.hidden { Vec::new() } else { label_lines(p, window, &ys, &o.y_axis) };
    let v2labels = match (&y2s, o.y2_axis.hidden) {
        (Some(s), false) => label_lines(p, window, s, &o.y2_axis),
        _ => Vec::new(),
    };
    let width_of = |ls: &[(f64, gpui::ShapedLine)]| ls.iter().map(|(_, l)| f64::from(l.width)).fold(0.0, f64::max);
    let cat_w = if horizontal && !o.x_axis.hidden { data.categories.iter().map(|c| f64::from(p.text(window, c, 12.0, t.ink_muted, false).width)).fold(0.0, f64::max).min((right - left) * 0.3) } else { 0.0 };
    let has_ytitle = o.y_axis.title.is_some() || o.y2_axis.title.is_some();
    let top = top + if has_ytitle && !horizontal { 18.0 } else { 0.0 };
    let bottom = bottom - if o.x_axis.title.is_some() { 18.0 } else { 0.0 };
    let (px0, px1) = if horizontal { (left + 4.0 + cat_w + 6.0, right - 8.0) } else { (left + 4.0 + width_of(&vlabels) + 6.0, right - 6.0 - if v2labels.is_empty() { 0.0 } else { width_of(&v2labels) + 8.0 }) };
    let (py0, py1) = (top + 4.0 + if horizontal && !v2labels.is_empty() { 18.0 } else { 0.0 }, bottom - if (horizontal && vlabels.is_empty()) || (!horizontal && o.x_axis.hidden) { 4.0 } else { 20.0 });
    if px1 - px0 < 20.0 || py1 - py0 < 20.0 {
        return;
    }
    let plot = rect(px0, py0, px1 - px0, py1 - py0);
    let mclip = clip.intersect(&rect(px0 - 6.0, py0 - 6.0, px1 - px0 + 12.0, py1 - py0 + 12.0));
    // Value → pixel along the value direction.
    let vpos = |s: &Scale, v: f64| -> f64 {
        let f = s.frac(v);
        if horizontal { px0 + f * (px1 - px0) } else { py1 - f * (py1 - py0) }
    };
    // Gridlines and value labels.
    let minor = |s: &Scale| -> Vec<f64> {
        if s.log.is_some() || s.ticks.len() < 2 {
            return Vec::new();
        }
        s.ticks.windows(2).map(|w| (w[0] + w[1]) / 2.0).collect()
    };
    let mut faint = t.rule;
    faint.a *= 0.5;
    if o.y_axis.minor_gridlines {
        for v in minor(&ys) {
            let g = vpos(&ys, v);
            if horizontal { p.quad(g, py0, 1.0, py1 - py0, faint, clip) } else { p.quad(px0, g, px1 - px0, 1.0, faint, clip) }
        }
    }
    for (v, line) in vlabels {
        let g = vpos(&ys, v);
        if !g.is_finite() {
            continue;
        }
        let lw = f64::from(line.width);
        if horizontal {
            if o.y_axis.gridlines {
                p.quad(g, py0, 1.0, py1 - py0, t.rule, clip);
            }
            p.put(line, g - lw / 2.0, py1 + 2.0, 18.0);
        } else {
            if o.y_axis.gridlines {
                p.quad(px0, g, px1 - px0, 1.0, t.rule, clip);
            }
            p.put(line, px0 - 6.0 - lw, g - 9.0, 18.0);
        }
    }
    if o.y_axis.hidden && o.y_axis.gridlines {
        for v in ys.ticks.clone() {
            let g = vpos(&ys, v);
            if horizontal { p.quad(g, py0, 1.0, py1 - py0, t.rule, clip) } else { p.quad(px0, g, px1 - px0, 1.0, t.rule, clip) }
        }
    }
    if let Some(s2) = &y2s {
        for (v, line) in v2labels {
            let g = vpos(s2, v);
            if !g.is_finite() {
                continue;
            }
            if horizontal {
                let lw = f64::from(line.width);
                p.put(line, g - lw / 2.0, py0 - 20.0, 18.0);
            } else {
                p.put(line, px1 + 8.0, g - 9.0, 18.0);
            }
        }
    }
    // Axis titles: value titles above their labels, the category title below.
    if let Some(title) = &o.y_axis.title {
        let line = p.text(window, title, 12.0, t.ink_muted, true);
        if horizontal {
            let lw = f64::from(line.width);
            p.put(line, (px0 + px1 - lw) / 2.0, py1 + 18.0, 18.0);
        } else {
            p.put(line, left + 2.0, top - 18.0, 18.0);
        }
    }
    if let (Some(title), false) = (&o.y2_axis.title, horizontal) {
        let line = p.text(window, title, 12.0, t.ink_muted, true);
        let lw = f64::from(line.width);
        p.put(line, right - lw - 2.0, top - 18.0, 18.0);
    }
    if let Some(title) = &o.x_axis.title {
        let line = p.text(window, title, 12.0, t.ink_muted, true);
        let lw = f64::from(line.width);
        if horizontal {
            p.put(line, left + 2.0, py0 - 18.0, 18.0);
        } else {
            p.put(line, (px0 + px1 - lw) / 2.0, bottom, 18.0);
        }
    }
    // Zero line.
    if ys.log.is_none() && ys.lo < 0.0 && ys.hi > 0.0 {
        let z = vpos(&ys, 0.0);
        if horizontal { p.quad(z, py0, 1.0, py1 - py0, t.ink_faint, clip) } else { p.quad(px0, z, px1 - px0, 1.0, t.ink_faint, clip) }
    }

    // Category positions: band centers (scatter: numeric x).
    let band = if horizontal { (py1 - py0) / n.max(1) as f64 } else { (px1 - px0) / n.max(1) as f64 };
    let center = |i: usize| if horizontal { py0 + band * (i as f64 + 0.5) } else { px0 + band * (i as f64 + 0.5) };
    let xpos = |si: usize, i: usize| -> Option<f64> {
        match &xscale {
            Some(xs) => xs_of(si).get(i).copied().flatten().map(|v| px0 + xs.frac(v) * (px1 - px0)),
            None => Some(center(i)),
        }
    };
    if let Some(xs) = &xscale {
        for v in &xs.ticks {
            let gx = px0 + xs.frac(*v) * (px1 - px0);
            if o.x_axis.gridlines {
                p.quad(gx, py0, 1.0, py1 - py0, t.rule, clip);
            }
            if !o.x_axis.hidden {
                p.put_centered(window, &axis_label(*v, &o.x_axis), gx, py1 + 2.0, 12.0, t.ink_faint);
            }
        }
    } else if !o.x_axis.hidden {
        let every = if horizontal { ((18.0 / band).ceil() as usize).max(1) } else { ((data.categories.iter().map(|c| c.chars().count()).max().unwrap_or(1) as f64 * 7.0 + 8.0) / band).ceil().max(1.0) as usize };
        for (i, c) in data.categories.iter().enumerate().step_by(every) {
            let line = p.text(window, c, 12.0, t.ink_muted, false);
            let lw = f64::from(line.width);
            if horizontal {
                p.put(line, px0 - 6.0 - lw, center(i) - 9.0, 18.0);
            } else {
                p.put(line, center(i) - lw / 2.0, py1 + 2.0, 18.0);
            }
        }
        if o.x_axis.gridlines {
            for i in 0..=n {
                let g = if horizontal { py0 + band * i as f64 } else { px0 + band * i as f64 };
                if horizontal { p.quad(px0, g, px1 - px0, 1.0, t.rule, clip) } else { p.quad(g, py0, 1.0, py1 - py0, t.rule, clip) }
            }
        }
    }
    let _ = plot;

    let scale_of = |si: usize| if secondary(si) { y2s.as_ref().unwrap_or(&ys) } else { &ys };
    let mut labels: Vec<(String, f64, f64)> = Vec::new();

    // Areas first (behind everything).
    for si in (0..ns).filter(|&si| kind_of(si) == ChartKind::Area) {
        let s = scale_of(si);
        let pts: Vec<(f64, f64, f64)> = tops[si].iter().enumerate().filter_map(|(i, v)| v.map(|(a, b)| (center(i), vpos(s, b), vpos(s, a)))).collect();
        if pts.is_empty() {
            continue;
        }
        if let Some(mut c) = fill_of(sheet, chart, si) {
            if chart.series[si].format.fill == Paint::Auto {
                c.a = 0.35;
            }
            if let Some(area) = path(
                |b| {
                    b.move_to(pt(pts[0].0, pts[0].1));
                    for q in &pts[1..] {
                        b.line_to(pt(q.0, q.1));
                    }
                    for q in pts.iter().rev() {
                        b.line_to(pt(q.0, q.2));
                    }
                    b.close();
                },
                None,
                Dash::Solid,
            ) {
                p.ops.push(Op::Path(area, c, mclip));
            }
        }
        if let (Some(c), Paint::Solid(_) | Paint::Auto) = (stroke_of(sheet, chart, si), chart.series[si].format.line) {
            let line: Vec<(f64, f64)> = pts.iter().map(|q| (q.0, q.1)).collect();
            let f = &chart.series[si].format;
            p.stroke(&line, f.line_width.map_or(1.5, |w| (w * 4.0 / 3.0) as f32), f.dash, false, c, mclip);
        }
    }

    // Columns and bars: clustered across every column series, or stacked.
    let col_series: Vec<usize> = (0..ns).filter(|&si| matches!(kind_of(si), ChartKind::Column | ChartKind::Bar)).collect();
    let stacked_cols = stackable(chart.kind) || steps.is_some();
    let slots = if stacked_cols { 1 } else { col_series.len().max(1) };
    let group = band * if chart.kind == ChartKind::Histogram { 0.96 } else { 0.72 };
    let bar = group / slots as f64;
    let mut prev_step_end: Option<(f64, f64)> = None;
    for (slot, &si) in col_series.iter().enumerate() {
        let s = scale_of(si);
        for (i, v) in tops[si].iter().enumerate() {
            let Some((from, to)) = *v else { continue };
            let (from, to) = if stacked_cols || steps.is_some() { (from, to) } else { (s.base().max(s.lo), to) };
            let offset = -group / 2.0 + if stacked_cols { 0.0 } else { bar * slot as f64 };
            let (a, b) = (vpos(s, from), vpos(s, to));
            if !a.is_finite() || !b.is_finite() {
                continue;
            }
            let color = match steps.as_ref().and_then(|st| st.get(i).copied().flatten()) {
                Some(step) => point_color(sheet, chart, si, i).unwrap_or_else(|| {
                    auto_color(
                        sheet,
                        chart,
                        match step.kind {
                            StepKind::Increase => 0,
                            StepKind::Decrease => 1,
                            StepKind::Total => 2,
                        },
                    )
                }),
                None => match point_color(sheet, chart, si, i).or_else(|| fill_of(sheet, chart, si)) {
                    Some(c) => c,
                    None => continue,
                },
            };
            let r = if horizontal { rect(a.min(b), center(i) + offset, (b - a).abs().max(1.0), bar - 1.0) } else { rect(center(i) + offset, a.min(b), bar - 1.0, (b - a).abs().max(1.0)) };
            let border = match chart.series[si].format.line {
                Paint::Solid(c) => rgb(c),
                _ => color,
            };
            p.ops.push(Op::Quad(gpui::quad(r, gpui::Corners::all(px(1.5)), color, gpui::Edges::all(px(if border == color { 0.0 } else { 1.0 })), border, gpui::BorderStyle::Solid), mclip));
            // Waterfall connectors from one column's end to the next.
            if steps.is_some() {
                if let Some((px_, py_)) = prev_step_end {
                    p.stroke(&[(px_, py_), (center(i) + offset, b)], 1.0, Dash::Solid, false, t.ink_faint, mclip);
                }
                prev_step_end = Some((center(i) + offset + bar - 1.0, b));
            }
            let raw = data.series[si].1.get(i).copied().flatten().unwrap_or(to - from);
            if let Some(text) = point_label(chart, data, si, i, raw, None) {
                let (lx, ly) = if horizontal { (a.max(b) + 4.0, center(i) + offset + bar / 2.0 - 9.0) } else { (center(i) + offset + bar / 2.0, a.min(b) - 17.0) };
                labels.push((text, lx, ly));
            }
            if let Some(Some((lo, hi))) = error_ranges[si].get(i) {
                let c = center(i) + offset + bar / 2.0;
                error_bar(p, (vpos(s, *lo), vpos(s, *hi)), c, horizontal, mclip);
            }
        }
    }

    // Lines, scatter and bubbles.
    for si in (0..ns).filter(|&si| matches!(kind_of(si), ChartKind::Line | ChartKind::Scatter | ChartKind::Bubble)) {
        let k = kind_of(si);
        let s = scale_of(si);
        let f = &chart.series[si].format;
        let pts: Vec<(usize, f64, f64)> = tops[si].iter().enumerate().filter_map(|(i, v)| Some((i, xpos(si, i)?, vpos(s, (*v)?.1)))).filter(|q| q.2.is_finite()).collect();
        if k == ChartKind::Bubble {
            let sizes = data.sizes.get(si).cloned().unwrap_or_default();
            let max = sizes.iter().flatten().copied().fold(0.0f64, |m, v| m.max(v.abs()));
            let rmax = ((px1 - px0).min(py1 - py0) / 9.0).max(4.0);
            for &(i, bx, by) in &pts {
                let z = sizes.get(i).copied().flatten().unwrap_or(max);
                let rr = if max > 0.0 { (z.abs() / max).sqrt() * rmax } else { rmax / 2.0 }.max(2.0);
                let mut c = point_color(sheet, chart, si, i).or_else(|| fill_of(sheet, chart, si)).unwrap_or(t.accent);
                let edge = c;
                c.a = 0.65;
                p.ops.push(Op::Quad(outline(rect(bx - rr, by - rr, rr * 2.0, rr * 2.0), 1.0, edge, c, rr as f32), mclip));
            }
        } else if let Some(c) = stroke_of(sheet, chart, si) {
            let line: Vec<(f64, f64)> = pts.iter().map(|q| (q.1, q.2)).collect();
            p.stroke(&line, f.line_width.map_or(2.0, |w| (w * 4.0 / 3.0) as f32), f.dash, f.smooth, c, mclip);
        }
        let mc = stroke_of(sheet, chart, si).or_else(|| fill_of(sheet, chart, si)).unwrap_or_else(|| auto_color(sheet, chart, si));
        let shape_ = f.marker.shape.unwrap_or(if k == ChartKind::Bubble { MarkerShape::None } else { MarkerShape::Circle });
        let d = f.marker.size.map_or(if k == ChartKind::Scatter { 7.0 } else { 6.0 }, |s| f64::from(s) * 4.0 / 3.0);
        for &(i, mx, my) in &pts {
            let c = point_color(sheet, chart, si, i).unwrap_or(mc);
            marker(&mut p.ops, shape_, (mx, my), d, c, t.canvas, mclip);
            let raw = data.series[si].1.get(i).copied().flatten().unwrap_or(0.0);
            if let Some(text) = point_label(chart, data, si, i, raw, None) {
                labels.push((text, mx, my - d / 2.0 - 17.0));
            }
            if let Some(Some((lo, hi))) = error_ranges[si].get(i) {
                error_bar(p, (vpos(s, *lo), vpos(s, *hi)), mx, false, mclip);
            }
        }
    }

    // Trendlines (fitted on x = category number, or the scatter x values).
    for si in 0..ns {
        let Some(tl) = &chart.series[si].format.trendline else { continue };
        let s = scale_of(si);
        let k = kind_of(si);
        let xs = if xy { xs_of(si) } else { (1..=n).map(|i| Some(i as f64)).collect() };
        let pairs: Vec<(f64, f64)> = data.series[si].1.iter().zip(&xs).filter_map(|(v, x)| Some(((*x)?, (*v)?))).collect();
        let color = stroke_of(sheet, chart, si).or_else(|| fill_of(sheet, chart, si)).unwrap_or(t.ink_faint);
        let to_px = |x: f64, v: f64| -> (f64, f64) {
            let along = match &xscale {
                Some(xsc) => px0 + xsc.frac(x) * (px1 - px0),
                None => {
                    let c = if horizontal { py0 } else { px0 };
                    c + band * (x - 0.5)
                }
            };
            let val = vpos(s, v);
            if horizontal { (val, along) } else { (along, val) }
        };
        let line: Vec<(f64, f64)> = match tl.kind {
            TrendKind::MovingAverage(period) => {
                let avg = fit::moving_average(&data.series[si].1, period);
                avg.iter().zip(&xs).filter_map(|(v, x)| Some(to_px((*x)?, (*v)?))).collect()
            }
            kind => {
                let (xv, yv): (Vec<f64>, Vec<f64>) = pairs.iter().copied().unzip();
                let Some(fitted) = fit::fit_trend(kind, &xv, &yv) else { continue };
                let (mn, mx) = xv.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), x| (a.min(*x), b.max(*x)));
                let samples = if kind == TrendKind::Linear { 1 } else { 60 };
                let line: Vec<(f64, f64)> = (0..=samples).map(|j| mn + (mx - mn) * j as f64 / samples as f64).map(|x| to_px(x, fitted.eval(x))).filter(|q| q.0.is_finite() && q.1.is_finite()).collect();
                let mut text = Vec::new();
                if tl.equation {
                    text.push(fitted.equation());
                }
                if tl.r_squared {
                    text.push(format!("R² = {}", precise((fitted.r_squared * 1e4).round() / 1e4)));
                }
                if !text.is_empty()
                    && let Some(&(ex, ey)) = line.last()
                {
                    let line_ = p.text(window, &text.join("   "), 11.5, color, false);
                    let lw = f64::from(line_.width);
                    p.put(line_, (ex - lw).max(px0 + 2.0), (ey - 20.0).clamp(py0, py1 - 16.0), 16.0);
                }
                line
            }
        };
        let _ = k;
        p.stroke(&line, 1.6, Dash::Dash, false, color, rect(px0, py0, px1 - px0, py1 - py0).intersect(&clip));
    }

    // Data labels on top.
    for (text, lx, ly) in labels {
        let line = p.text(window, &text, 11.5, t.ink_muted, false);
        let lw = f64::from(line.width);
        let x = if horizontal { lx } else { lx - lw / 2.0 };
        p.put(line, x, ly, 16.0);
    }
    // Axis lines.
    if horizontal {
        p.quad(px0, py0, 1.0, py1 - py0, t.ink_faint, clip);
    } else {
        p.quad(px0, py1, px1 - px0, 1.0, t.ink_faint, clip);
    }
}

/// An error bar spanning `span` (pixels along the value direction) at `at`.
fn error_bar(p: &mut Painter, (a, b): (f64, f64), at: f64, horizontal: bool, clip: Bounds<Pixels>) {
    if !a.is_finite() || !b.is_finite() {
        return;
    }
    let c = p.sheet.theme.ink_muted;
    let cap = 4.0;
    if horizontal {
        p.stroke(&[(a, at), (b, at)], 1.2, Dash::Solid, false, c, clip);
        p.stroke(&[(a, at - cap), (a, at + cap)], 1.2, Dash::Solid, false, c, clip);
        p.stroke(&[(b, at - cap), (b, at + cap)], 1.2, Dash::Solid, false, c, clip);
    } else {
        p.stroke(&[(at, a), (at, b)], 1.2, Dash::Solid, false, c, clip);
        p.stroke(&[(at - cap, a), (at + cap, a)], 1.2, Dash::Solid, false, c, clip);
        p.stroke(&[(at - cap, b), (at + cap, b)], 1.2, Dash::Solid, false, c, clip);
    }
}

/// A sparkline inside a cell's rectangle.
pub(crate) fn sparkline_ops(sheet: &Spreadsheet, kind: xl_engine::chart::SparkKind, values: &[Option<f64>], color: Option<u32>, (x, y, w, h): (f64, f64, f64, f64), clip: Bounds<Pixels>) -> Vec<Op> {
    use xl_engine::chart::SparkKind;
    let t = sheet.theme;
    let mut ops = Vec::new();
    let color: Hsla = color.map_or(t.accent, |c| gpui::rgb(c).into());
    let negative = t.error;
    let nums: Vec<f64> = values.iter().flatten().copied().collect();
    let (Some(mn), Some(mx)) = (nums.iter().copied().reduce(f64::min), nums.iter().copied().reduce(f64::max)) else { return ops };
    let n = values.len().max(1);
    let (x0, y0, w0, h0) = (x + 4.0, y + 3.0, (w - 8.0).max(4.0), (h - 6.0).max(4.0));
    match kind {
        SparkKind::Line => {
            let span = if mx > mn { mx - mn } else { 1.0 };
            let to_y = |v: f64| y0 + h0 - (v - mn) / span * h0;
            let step = if n > 1 { w0 / (n - 1) as f64 } else { 0.0 };
            let pts: Vec<(f64, f64)> = values.iter().enumerate().filter_map(|(i, v)| Some((x0 + step * i as f64, to_y((*v)?)))).collect();
            if pts.len() > 1
                && let Some(line) = path(|p| polyline(p, &pts, false), Some(1.5), Dash::Solid)
            {
                ops.push(Op::Path(line, color, clip));
            }
            // Mark the last point, like Excel's "high point" emphasis.
            if let Some(&(lx, ly)) = pts.last() {
                ops.push(Op::Quad(outline(rect(lx - 2.0, ly - 2.0, 4.0, 4.0), 1.0, color, color, 2.0), clip));
            }
        }
        SparkKind::Column | SparkKind::WinLoss => {
            let band = w0 / n as f64;
            let bar = (band * 0.75).max(1.0);
            let (lo, hi) = (mn.min(0.0), mx.max(0.0));
            let span = if hi > lo { hi - lo } else { 1.0 };
            for (i, v) in values.iter().enumerate() {
                let Some(v) = *v else { continue };
                let bx = x0 + band * i as f64 + (band - bar) / 2.0;
                let c = if v < 0.0 { negative } else { color };
                let r = if kind == SparkKind::WinLoss {
                    if v == 0.0 {
                        continue;
                    }
                    let half = h0 / 2.0;
                    if v > 0.0 { rect(bx, y0, bar, half - 1.0) } else { rect(bx, y0 + half + 1.0, bar, half - 1.0) }
                } else {
                    let zero = y0 + h0 - (0.0 - lo) / span * h0;
                    let top = y0 + h0 - (v - lo) / span * h0;
                    let (a, b) = (top.min(zero), top.max(zero));
                    rect(bx, a, bar, (b - a).max(1.0))
                };
                ops.push(Op::Quad(fill(r, c), clip));
            }
        }
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tidy_ticks() {
        assert_eq!(nice_ticks(0.0, 87.0, 5), (0.0, 100.0, 20.0));
        assert_eq!(nice_ticks(-3.0, 12.0, 5), (-5.0, 15.0, 5.0));
        assert_eq!(nice_ticks(0.0, 0.0, 5), (0.0, 1.0, 0.2));
        assert_eq!(label(12_500.0), "12.5K");
        assert_eq!(label(0.3), "0.3");
    }

    #[test]
    fn axis_bounds_and_log_scales() {
        let s = Scale::new(0.0, 87.0, &Axis::default());
        assert_eq!((s.lo, s.hi, s.ticks.len()), (0.0, 100.0, 6));
        let s = Scale::new(0.0, 87.0, &Axis { min: Some(10.0), max: Some(50.0), major: Some(10.0), ..Axis::default() });
        assert_eq!(s.ticks, vec![10.0, 20.0, 30.0, 40.0, 50.0]);
        assert_eq!(s.frac(30.0), 0.5);
        let s = Scale::new(3.0, 4200.0, &Axis { log_base: Some(10.0), ..Axis::default() });
        assert_eq!((s.lo, s.hi), (1.0, 10_000.0));
        assert_eq!(s.ticks, vec![1.0, 10.0, 100.0, 1000.0, 10_000.0]);
        assert!((s.frac(100.0) - 0.5).abs() < 1e-12);
        assert!(s.frac(-1.0).is_nan());
        assert_eq!(axis_label(0.25, &Axis { num_fmt: Some("0%".into()), ..Axis::default() }), "25%");
    }
}
