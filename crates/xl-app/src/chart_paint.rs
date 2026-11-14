//! Drawing a chart: frame, title, axes with tidy ticks, the marks for each
//! kind, and a legend.

use gpui::{Bounds, FontWeight, Hsla, PathBuilder, Pixels, Window, fill, point, px};
use xl_engine::chart::{Chart, ChartData, ChartKind};
use xl_engine::precise;

use crate::app::Spreadsheet;
use crate::paint::{Op, cell_font, outline, rect, shape};

/// Series colors, in order.
pub(crate) fn series_color(sheet: &Spreadsheet, i: usize) -> Hsla {
    let palette = sheet.theme.ref_colors;
    palette[i % palette.len()]
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

fn path(build: impl FnOnce(&mut PathBuilder), stroke: Option<f32>) -> Option<gpui::Path> {
    let mut p = match stroke {
        Some(w) => PathBuilder::stroke(px(w)),
        None => PathBuilder::fill(),
    };
    build(&mut p);
    p.build().ok()
}

fn pt(x: f64, y: f64) -> gpui::Point<Pixels> {
    point(px(x as f32), px(y as f32))
}

pub(crate) fn chart_ops(sheet: &Spreadsheet, chart: &Chart, data: &ChartData, (x, y, w, h): (f64, f64, f64, f64), clip: Bounds<Pixels>, selected: bool, window: &mut Window) -> Vec<Op> {
    let t = sheet.theme;
    let mut ops = Vec::new();
    let text = |window: &mut Window, s: &str, size: f32, color: Hsla, bold: bool| {
        let f = gpui::Font { weight: if bold { FontWeight::SEMIBOLD } else { FontWeight::NORMAL }, ..cell_font() };
        shape(window, s.to_string(), f, color, size)
    };
    let put_text = |ops: &mut Vec<Op>, line: gpui::ShapedLine, tx: f64, ty: f64, lh: f64| {
        ops.push(Op::Text { line: Box::new(line), origin: pt(tx, ty), clip, line_height: px(lh as f32) });
    };
    // Frame.
    ops.push(Op::Quad(outline(rect(x, y, w, h), if selected { 2.0 } else { 1.0 }, if selected { t.accent } else { t.divider }, t.canvas, 6.0), clip));

    let mut top = y + 10.0;
    let title = chart.title.clone().or_else(|| (data.series.len() == 1).then(|| data.series[0].0.clone()));
    let editing_title = sheet.field == Some(crate::app::Field::ChartTitle) && selected;
    let title = if editing_title { Some(sheet.chart_title_text.clone()) } else { title };
    if let Some(title) = title {
        let line = text(window, &title, 15.0, t.ink, true);
        let tw = f64::from(line.width);
        let tx = x + (w - tw) / 2.0;
        put_text(&mut ops, line, tx, top, 22.0);
        if editing_title {
            ops.push(Op::Quad(fill(rect(tx + tw + 1.0, top + 3.0, 1.5, 16.0), t.accent), clip));
        }
        top += 28.0;
    }

    let pie = chart.kind == ChartKind::Pie;
    // Legend along the bottom: series (or categories, for pies).
    let entries: Vec<(String, Hsla)> = if pie {
        data.categories.iter().enumerate().map(|(i, c)| (c.clone(), series_color(sheet, i))).collect()
    } else {
        data.series.iter().enumerate().map(|(i, s)| (s.0.clone(), series_color(sheet, i))).collect()
    };
    let mut bottom = y + h - 10.0;
    if entries.len() > 1 || pie {
        let lines: Vec<_> = entries.iter().map(|(name, _)| text(window, name, 12.5, t.ink_muted, false)).collect();
        let total: f64 = lines.iter().map(|l| f64::from(l.width) + 26.0).sum();
        let mut lx = x + ((w - total) / 2.0).max(10.0);
        let ly = bottom - 18.0;
        for (line, (_, color)) in lines.into_iter().zip(&entries) {
            if lx > x + w - 30.0 {
                break;
            }
            ops.push(Op::Quad(fill(rect(lx, ly + 5.0, 10.0, 10.0), *color), clip));
            let lw = f64::from(line.width);
            put_text(&mut ops, line, lx + 14.0, ly, 20.0);
            lx += lw + 26.0;
        }
        bottom -= 24.0;
    }

    if pie {
        let values: Vec<f64> = data.series.first().map(|s| s.1.iter().map(|v| v.unwrap_or(0.0).max(0.0)).collect()).unwrap_or_default();
        let total: f64 = values.iter().sum();
        let (cx, cy) = (x + w / 2.0, (top + bottom) / 2.0);
        let r = ((bottom - top).min(w - 20.0) / 2.0 - 4.0).max(4.0);
        if total > 0.0 {
            let mut angle = -std::f64::consts::FRAC_PI_2;
            for (i, v) in values.iter().enumerate() {
                if *v <= 0.0 {
                    continue;
                }
                let sweep = v / total * std::f64::consts::TAU;
                let (a0, a1) = (angle, angle + sweep);
                let color = series_color(sheet, i);
                let wedge = path(
                    |p| {
                        p.move_to(pt(cx, cy));
                        p.line_to(pt(cx + r * a0.cos(), cy + r * a0.sin()));
                        if sweep >= std::f64::consts::TAU - 1e-9 {
                            // A full circle: two half arcs.
                            p.arc_to(pt(r, r), px(0.), false, true, pt(cx - r * a0.cos(), cy - r * a0.sin()));
                            p.arc_to(pt(r, r), px(0.), false, true, pt(cx + r * a0.cos(), cy + r * a0.sin()));
                        } else {
                            p.arc_to(pt(r, r), px(0.), sweep > std::f64::consts::PI, true, pt(cx + r * a1.cos(), cy + r * a1.sin()));
                        }
                        p.close();
                    },
                    None,
                );
                if let Some(wedge) = wedge {
                    ops.push(Op::Path(wedge, color, clip));
                }
                // Percent labels on slices big enough to hold one.
                if sweep > 0.35 {
                    let mid = (a0 + a1) / 2.0;
                    let line = text(window, &format!("{}%", (v / total * 100.0).round()), 12.0, gpui::white(), true);
                    let lw = f64::from(line.width);
                    put_text(&mut ops, line, cx + r * 0.62 * mid.cos() - lw / 2.0, cy + r * 0.62 * mid.sin() - 9.0, 18.0);
                }
                angle = a1;
            }
        }
        return ops;
    }

    // Value range.
    let n = data.categories.len().max(data.series.iter().map(|s| s.1.len()).max().unwrap_or(0));
    let stacked = chart.stacked && matches!(chart.kind, ChartKind::Column | ChartKind::Bar | ChartKind::Area);
    let (mut lo, mut hi) = (0.0f64, 0.0f64);
    if stacked {
        for i in 0..n {
            let (mut pos, mut neg) = (0.0, 0.0);
            for s in &data.series {
                match s.1.get(i).copied().flatten() {
                    Some(v) if v >= 0.0 => pos += v,
                    Some(v) => neg += v,
                    None => {}
                }
            }
            hi = hi.max(pos);
            lo = lo.min(neg);
        }
    } else {
        for v in data.series.iter().flat_map(|s| s.1.iter().flatten()) {
            hi = hi.max(*v);
            lo = lo.min(*v);
        }
        if matches!(chart.kind, ChartKind::Line | ChartKind::Scatter) {
            // Lines needn't start at zero when all the values are far from it.
            let vals: Vec<f64> = data.series.iter().flat_map(|s| s.1.iter().flatten().copied()).collect();
            if let (Some(mn), Some(mx)) = (vals.iter().copied().reduce(f64::min), vals.iter().copied().reduce(f64::max))
                && mn > 0.0
                && mn > (mx - mn) * 2.0
            {
                lo = mn;
                hi = mx;
            }
        }
    }
    let (vlo, vhi, step) = nice_ticks(lo, hi, 5);
    let horizontal = chart.kind == ChartKind::Bar;

    // Plot area: room for value labels on the left (or bottom for bars).
    let value_labels: Vec<(f64, gpui::ShapedLine)> = {
        let mut v = vlo;
        let mut out = Vec::new();
        while v <= vhi + step * 1e-9 && out.len() < 50 {
            out.push((v, text(window, &label(v), 12.0, t.ink_faint, false)));
            v += step;
        }
        out
    };
    let label_w = value_labels.iter().map(|(_, l)| f64::from(l.width)).fold(0.0, f64::max);
    let cat_w = if horizontal { data.categories.iter().map(|c| f64::from(text(window, c, 12.0, t.ink_muted, false).width)).fold(0.0, f64::max).min(w * 0.3) } else { 0.0 };
    let (px0, px1) = (x + 14.0 + if horizontal { cat_w + 6.0 } else { label_w + 6.0 }, x + w - 16.0);
    let (py0, py1) = (top + 4.0, bottom - 20.0);
    if px1 - px0 < 20.0 || py1 - py0 < 20.0 {
        return ops;
    }
    let scale = |v: f64| (v - vlo) / (vhi - vlo);
    let to_y = |v: f64| py1 - scale(v) * (py1 - py0);
    let to_x = |v: f64| px0 + scale(v) * (px1 - px0);
    // Gridlines and value labels.
    for (v, line) in value_labels {
        let lw = f64::from(line.width);
        if horizontal {
            let gx = to_x(v);
            ops.push(Op::Quad(fill(rect(gx, py0, 1.0, py1 - py0), t.rule), clip));
            put_text(&mut ops, line, gx - lw / 2.0, py1 + 2.0, 18.0);
        } else {
            let gy = to_y(v);
            ops.push(Op::Quad(fill(rect(px0, gy, px1 - px0, 1.0), t.rule), clip));
            put_text(&mut ops, line, px0 - 6.0 - lw, gy - 9.0, 18.0);
        }
    }
    // Zero line.
    if vlo < 0.0 && vhi > 0.0 {
        if horizontal {
            ops.push(Op::Quad(fill(rect(to_x(0.0), py0, 1.0, py1 - py0), t.ink_faint), clip));
        } else {
            ops.push(Op::Quad(fill(rect(px0, to_y(0.0), px1 - px0, 1.0), t.ink_faint), clip));
        }
    }

    // Category positions: band centers (scatter: numeric x).
    let scatter_x = (chart.kind == ChartKind::Scatter).then(|| data.x.clone()).flatten();
    let (xlo, xhi, xstep) = match &scatter_x {
        Some(xs) => {
            let mn = xs.iter().copied().fold(f64::INFINITY, f64::min);
            let mx = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            nice_ticks(mn.min(0.0).max(if mn > 0.0 && mn > (mx - mn) { mn } else { f64::NEG_INFINITY }), mx, 5)
        }
        None => (0.0, 1.0, 1.0),
    };
    let band = if horizontal { (py1 - py0) / n.max(1) as f64 } else { (px1 - px0) / n.max(1) as f64 };
    let center = |i: usize| -> f64 {
        if let Some(xs) = &scatter_x {
            let xv = xs.get(i).copied().unwrap_or(0.0);
            px0 + (xv - xlo) / (xhi - xlo) * (px1 - px0)
        } else if horizontal {
            py0 + band * (i as f64 + 0.5)
        } else {
            px0 + band * (i as f64 + 0.5)
        }
    };
    // Category labels (skipping some when crowded).
    if let Some(_xs) = &scatter_x {
        let mut v = xlo;
        while v <= xhi + xstep * 1e-9 {
            let line = text(window, &label(v), 12.0, t.ink_faint, false);
            let lw = f64::from(line.width);
            put_text(&mut ops, line, px0 + (v - xlo) / (xhi - xlo) * (px1 - px0) - lw / 2.0, py1 + 2.0, 18.0);
            v += xstep;
        }
    } else {
        let every = if horizontal { ((18.0 / band).ceil() as usize).max(1) } else { ((data.categories.iter().map(|c| c.len()).max().unwrap_or(1) as f64 * 7.0 + 8.0) / band).ceil().max(1.0) as usize };
        for (i, c) in data.categories.iter().enumerate().step_by(every) {
            let line = text(window, c, 12.0, t.ink_muted, false);
            let lw = f64::from(line.width);
            if horizontal {
                put_text(&mut ops, line, px0 - 6.0 - lw, center(i) - 9.0, 18.0);
            } else {
                put_text(&mut ops, line, center(i) - lw / 2.0, py1 + 2.0, 18.0);
            }
        }
    }
    // Axis line.
    if horizontal {
        ops.push(Op::Quad(fill(rect(px0, py0, 1.0, py1 - py0), t.ink_faint), clip));
    } else {
        ops.push(Op::Quad(fill(rect(px0, py1, px1 - px0, 1.0), t.ink_faint), clip));
    }

    let ns = data.series.len().max(1);
    match chart.kind {
        ChartKind::Column | ChartKind::Bar => {
            let group = band * 0.72;
            let bar = if stacked { group } else { group / ns as f64 };
            let mut base_pos = vec![0.0; n];
            let mut base_neg = vec![0.0; n];
            for (si, s) in data.series.iter().enumerate() {
                let color = series_color(sheet, si);
                for (i, v) in s.1.iter().enumerate() {
                    let Some(v) = *v else { continue };
                    let (from, to) = if stacked {
                        let base = if v >= 0.0 { &mut base_pos[i] } else { &mut base_neg[i] };
                        let from = *base;
                        *base += v;
                        (from, *base)
                    } else {
                        (0.0f64.clamp(vlo, vhi), v)
                    };
                    let offset = if stacked { -group / 2.0 } else { -group / 2.0 + bar * si as f64 };
                    let r = if horizontal {
                        let (a, b) = (to_x(from.min(to)), to_x(from.max(to)));
                        rect(a, center(i) + offset, (b - a).max(1.0), bar - 1.0)
                    } else {
                        let (a, b) = (to_y(from.max(to)), to_y(from.min(to)));
                        rect(center(i) + offset, a, bar - 1.0, (b - a).max(1.0))
                    };
                    ops.push(Op::Quad(gpui::quad(r, gpui::Corners::all(px(1.5)), color, gpui::Edges::default(), color, gpui::BorderStyle::Solid), clip));
                }
            }
        }
        ChartKind::Line | ChartKind::Area | ChartKind::Scatter => {
            let mut base = vec![0.0; n];
            for (si, s) in data.series.iter().enumerate() {
                let color = series_color(sheet, si);
                let pts: Vec<(f64, f64)> = s
                    .1
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| {
                        let v = (*v)?;
                        let v = if stacked {
                            base[i] += v;
                            base[i]
                        } else {
                            v
                        };
                        Some((center(i), to_y(v)))
                    })
                    .collect();
                if pts.is_empty() {
                    continue;
                }
                if chart.kind == ChartKind::Area {
                    let mut soft = color;
                    soft.a = 0.28;
                    let floor = to_y(0.0f64.clamp(vlo, vhi));
                    if let Some(area) = path(
                        |p| {
                            p.move_to(pt(pts[0].0, floor));
                            for &(px_, py_) in &pts {
                                p.line_to(pt(px_, py_));
                            }
                            p.line_to(pt(pts[pts.len() - 1].0, floor));
                            p.close();
                        },
                        None,
                    ) {
                        ops.push(Op::Path(area, soft, clip));
                    }
                }
                if chart.kind != ChartKind::Scatter
                    && pts.len() > 1
                    && let Some(line) = path(
                        |p| {
                            p.move_to(pt(pts[0].0, pts[0].1));
                            for &(px_, py_) in &pts[1..] {
                                p.line_to(pt(px_, py_));
                            }
                        },
                        Some(2.0),
                    )
                {
                    ops.push(Op::Path(line, color, clip));
                }
                if chart.kind != ChartKind::Area {
                    let d = if chart.kind == ChartKind::Scatter { 7.0 } else { 6.0 };
                    for &(px_, py_) in &pts {
                        ops.push(Op::Quad(outline(rect(px_ - d / 2.0, py_ - d / 2.0, d, d), 1.5, color, t.canvas, (d / 2.0) as f32), clip));
                    }
                }
            }
        }
        ChartKind::Pie => {}
    }
    if selected {
        // Resize grip at the bottom-right corner.
        ops.push(Op::Quad(outline(rect(x + w - 5.0, y + h - 5.0, 10.0, 10.0), 1.5, t.accent, t.canvas, 2.0), clip));
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
                && let Some(line) = path(
                    |p| {
                        p.move_to(pt(pts[0].0, pts[0].1));
                        for &(px_, py_) in &pts[1..] {
                            p.line_to(pt(px_, py_));
                        }
                    },
                    Some(1.5),
                )
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
