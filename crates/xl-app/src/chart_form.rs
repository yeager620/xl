//! The "Format chart" form: every chart option as a short line of text, so
//! it works from the keyboard and the command palette. Each field is
//! filled with a description of the chart as it is; editing the text and
//! applying parses it back (one undo step).
//!
//! Series lines read like `line, secondary, color #C00000, width 2, dash,
//! marker square 7, smooth, labels value, trend poly 3 eq r2, errors 5%`.

use gpui::SharedString;
use xl_engine::chart::{Axis, BinMode, Bins, Chart, ChartKind, DataLabels, Dash, ErrorBars, ErrorDir, ErrorKind, LegendPos, Marker, MarkerShape, Paint, SeriesFormat, TrendKind, Trendline};

use crate::app::Spreadsheet;

/// Fields before the per-series ones.
const FIXED: usize = 8;

/// Split on commas outside double quotes, trimmed, empties dropped.
fn items(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut cur, mut quoted) = (String::new(), false);
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' | ';' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

fn quote(s: &str) -> String {
    if s.contains([',', ';', '"']) || s != s.trim() { format!("\"{}\"", s.replace('"', "")) } else { s.to_string() }
}

fn num(v: f64) -> String {
    xl_engine::precise(v)
}

fn parse_num(s: Option<&str>, what: &str) -> Result<f64, String> {
    s.and_then(|s| s.trim().trim_end_matches('%').parse::<f64>().ok()).ok_or_else(|| format!("{what} needs a number"))
}

const NAMED: [(&str, u32); 10] = [("black", 0x000000), ("white", 0xFFFFFF), ("red", 0xC00000), ("orange", 0xED7D31), ("yellow", 0xFFC000), ("green", 0x70AD47), ("blue", 0x4472C4), ("purple", 0x7030A0), ("gray", 0x7F7F7F), ("teal", 0x00838F)];

fn paint_text(p: Paint) -> String {
    match p {
        Paint::Auto => "auto".into(),
        Paint::None => "none".into(),
        Paint::Solid(c) => NAMED.iter().find(|(_, v)| *v == c).map_or_else(|| format!("#{c:06X}"), |(n, _)| n.to_string()),
    }
}

fn parse_paint(s: Option<&str>) -> Result<Paint, String> {
    let s = s.ok_or("color needs a value, like #C00000 or red")?.trim().to_ascii_lowercase();
    match s.as_str() {
        "auto" | "automatic" => Ok(Paint::Auto),
        "none" | "no" => Ok(Paint::None),
        _ => {
            if let Some((_, v)) = NAMED.iter().find(|(n, _)| *n == s) {
                return Ok(Paint::Solid(*v));
            }
            let hex = s.trim_start_matches('#');
            match (hex.len(), u32::from_str_radix(hex, 16)) {
                (6, Ok(v)) => Ok(Paint::Solid(v)),
                (3, Ok(v)) => Ok(Paint::Solid(((v >> 8) & 15) * 0x110000 + ((v >> 4) & 15) * 0x1100 + (v & 15) * 0x11)),
                _ => Err(format!("“{s}” isn't a color (try #C00000 or red)")),
            }
        }
    }
}

fn kind_name(k: ChartKind) -> String {
    k.label().to_ascii_lowercase()
}

fn parse_kind(s: &str) -> Option<ChartKind> {
    ChartKind::ALL.into_iter().find(|k| kind_name(*k) == s)
}

// ---------------------------------------------------------------- series

/// "line, secondary, color #C00000, …" for a series (in a chart of `chart_kind`).
pub(crate) fn describe_series(f: &SeriesFormat, chart_kind: ChartKind) -> String {
    let mut out: Vec<String> = Vec::new();
    if let Some(k) = f.kind.filter(|k| *k != chart_kind) {
        out.push(kind_name(k));
    }
    if f.secondary {
        out.push("secondary".into());
    }
    if f.fill != Paint::Auto {
        out.push(format!("fill {}", paint_text(f.fill)));
    }
    if f.line != Paint::Auto {
        out.push(format!("line {}", paint_text(f.line)));
    }
    if let Some(w) = f.line_width {
        out.push(format!("width {}", num((w * 100.0).round() / 100.0)));
    }
    if f.dash != Dash::Solid {
        out.push(f.dash.label().into());
    }
    match (f.marker.shape, f.marker.size) {
        (None, None) => {}
        (Some(MarkerShape::None), _) => out.push("no marker".into()),
        (shape, size) => out.push(format!("marker {}{}", shape.map_or("auto", |s| s.name()), size.map_or(String::new(), |s| format!(" {s}")))),
    }
    if f.smooth {
        out.push("smooth".into());
    }
    let l = f.labels;
    if l.any() {
        let parts: Vec<&str> = [(l.value, "value"), (l.category, "category"), (l.series, "series"), (l.percent, "percent")].iter().filter(|p| p.0).map(|p| p.1).collect();
        out.push(format!("labels {}", parts.join(" ")));
    }
    if let Some(t) = &f.trendline {
        let kind = match t.kind {
            TrendKind::Linear => "linear".to_string(),
            TrendKind::Exponential => "exp".into(),
            TrendKind::Logarithmic => "log".into(),
            TrendKind::Power => "power".into(),
            TrendKind::Polynomial(n) => format!("poly {n}"),
            TrendKind::MovingAverage(n) => format!("average {n}"),
        };
        out.push(format!("trend {kind}{}{}", if t.equation { " eq" } else { "" }, if t.r_squared { " r2" } else { "" }));
    }
    if let Some(e) = &f.error_bars {
        let amount = match e.kind {
            ErrorKind::Fixed => num(e.value),
            ErrorKind::Percent => format!("{}%", num(e.value)),
            ErrorKind::StdDev => format!("sd {}", num(e.value)),
            ErrorKind::StdErr => "se".into(),
        };
        let dir = match e.dir {
            ErrorDir::Both => "",
            ErrorDir::Plus => " plus",
            ErrorDir::Minus => " minus",
        };
        out.push(format!("errors {amount}{dir}"));
    }
    out.join(", ")
}

/// Parse a series line onto `base` (keeping what the line can't say, like
/// single points' colors).
pub(crate) fn parse_series(text: &str, base: &SeriesFormat, chart_kind: ChartKind) -> Result<SeriesFormat, String> {
    let mut f = SeriesFormat { points: base.points.clone(), ..SeriesFormat::default() };
    let filled_kind = |k: Option<ChartKind>| !matches!(k.unwrap_or(chart_kind), ChartKind::Line | ChartKind::Scatter | ChartKind::Radar);
    for item in items(text) {
        let lower = item.to_ascii_lowercase();
        let mut words = lower.split_whitespace();
        let head = words.next().unwrap_or("");
        let rest: Vec<&str> = words.collect();
        let arg = rest.first().copied();
        match head {
            k if rest.is_empty() && parse_kind(k).is_some_and(ChartKind::combinable) => {
                let k = parse_kind(k).unwrap_or(chart_kind);
                f.kind = (k != chart_kind).then_some(k);
            }
            "secondary" => f.secondary = true,
            "primary" => f.secondary = false,
            "fill" => f.fill = parse_paint(arg)?,
            "line" | "border" if arg.is_some() => f.line = parse_paint(arg)?,
            "color" | "colour" => {
                let p = parse_paint(arg)?;
                if filled_kind(f.kind) { f.fill = p } else { f.line = p }
            }
            "width" => f.line_width = Some(parse_num(arg, "width")?.clamp(0.25, 20.0)),
            "solid" => f.dash = Dash::Solid,
            d if Dash::ALL.iter().any(|x| x.label() == d) => f.dash = Dash::ALL.into_iter().find(|x| x.label() == d).unwrap_or_default(),
            "no" if arg == Some("marker") => f.marker = Marker { shape: Some(MarkerShape::None), size: None },
            "marker" | "markers" => {
                let mut m = Marker::default();
                for w in &rest {
                    if let Ok(n) = w.parse::<f64>() {
                        m.size = Some(n.clamp(2.0, 72.0) as u8);
                    } else if *w != "auto" {
                        m.shape = Some(MarkerShape::from_name(w).ok_or_else(|| format!("“{w}” isn't a marker (circle, square, diamond, triangle, x, plus, star, dash, dot, none)"))?);
                    }
                }
                f.marker = m;
            }
            "smooth" => f.smooth = true,
            "labels" | "label" => {
                let mut l = DataLabels::default();
                for w in &rest {
                    match *w {
                        "value" | "values" => l.value = true,
                        "category" | "categories" => l.category = true,
                        "series" | "name" => l.series = true,
                        "percent" | "percentage" | "%" => l.percent = true,
                        "none" => l = DataLabels::default(),
                        w => return Err(format!("“{w}” isn't a label part (value, category, series, percent)")),
                    }
                }
                if rest.is_empty() {
                    l.value = true;
                }
                f.labels = l;
            }
            "trend" | "trendline" => {
                let order = || rest.get(1).and_then(|n| n.parse::<f64>().ok());
                let kind = match arg.unwrap_or("linear") {
                    "linear" => TrendKind::Linear,
                    "exp" | "exponential" => TrendKind::Exponential,
                    "log" | "logarithmic" => TrendKind::Logarithmic,
                    "power" => TrendKind::Power,
                    "poly" | "polynomial" => TrendKind::Polynomial(order().unwrap_or(2.0).clamp(2.0, 6.0) as u8),
                    "average" | "avg" | "moving" => TrendKind::MovingAverage(order().unwrap_or(2.0).clamp(2.0, 255.0) as u8),
                    "none" => {
                        f.trendline = None;
                        continue;
                    }
                    w => return Err(format!("“{w}” isn't a trendline (linear, exp, log, power, poly 2–6, average N)")),
                };
                let name = base.trendline.as_ref().filter(|t| t.kind == kind).and_then(|t| t.name.clone());
                f.trendline = Some(Trendline { kind, equation: rest.contains(&"eq"), r_squared: rest.contains(&"r2"), name });
            }
            "errors" | "error" => {
                let dir = if rest.contains(&"plus") {
                    ErrorDir::Plus
                } else if rest.contains(&"minus") {
                    ErrorDir::Minus
                } else {
                    ErrorDir::Both
                };
                let (kind, value) = match arg {
                    Some("none") => {
                        f.error_bars = None;
                        continue;
                    }
                    Some("se") => (ErrorKind::StdErr, 0.0),
                    Some("sd") => (ErrorKind::StdDev, rest.get(1).and_then(|n| n.parse().ok()).unwrap_or(1.0)),
                    Some(a) if a.ends_with('%') => (ErrorKind::Percent, parse_num(Some(a), "errors")?),
                    a => (ErrorKind::Fixed, parse_num(a, "errors")?),
                };
                f.error_bars = Some(ErrorBars { kind, value, dir });
            }
            _ => return Err(format!("“{item}” isn't something a series can have")),
        }
    }
    Ok(f)
}

// ---------------------------------------------------------------- axes

pub(crate) fn describe_axis(a: &Axis) -> String {
    let mut out = Vec::new();
    if let Some(t) = &a.title {
        out.push(format!("title {}", quote(t)));
    }
    for (k, v) in [("min", a.min), ("max", a.max), ("unit", a.major)] {
        if let Some(v) = v {
            out.push(format!("{k} {}", num(v)));
        }
    }
    match a.log_base {
        Some(10.0) => out.push("log".into()),
        Some(b) => out.push(format!("log {}", num(b))),
        None => {}
    }
    if let Some(f) = &a.num_fmt {
        out.push(format!("format {}", quote(f)));
    }
    if a.gridlines {
        out.push("grid".into());
    }
    if a.minor_gridlines {
        out.push("minor".into());
    }
    if a.hidden {
        out.push("hidden".into());
    }
    out.join(", ")
}

pub(crate) fn parse_axis(text: &str) -> Result<Axis, String> {
    let mut a = Axis::default();
    for item in items(text) {
        let (head, rest) = item.split_once(char::is_whitespace).map_or((item.as_str(), ""), |(h, r)| (h, r.trim()));
        let value = (!rest.is_empty()).then_some(rest);
        match head.to_ascii_lowercase().as_str() {
            "title" => a.title = value.map(str::to_string),
            "min" => a.min = Some(parse_num(value, "min")?),
            "max" => a.max = Some(parse_num(value, "max")?),
            "unit" | "major" | "step" => a.major = Some(parse_num(value, "unit")?).filter(|v| *v > 0.0),
            "log" => a.log_base = Some(value.map_or(Ok(10.0), |v| parse_num(Some(v), "log"))?.max(2.0)),
            "format" => a.num_fmt = value.map(str::to_string),
            "grid" | "gridlines" => a.gridlines = true,
            "minor" => a.minor_gridlines = true,
            "hidden" | "hide" => a.hidden = true,
            "auto" | "none" => {}
            _ => return Err(format!("“{item}” isn't an axis option (title, min, max, unit, log, format, grid, minor, hidden)")),
        }
    }
    Ok(a)
}

// ---------------------------------------------------------------- the rest

/// Chart styles by name: Excel's style numbers for their colors.
const STYLES: [(&str, u8); 8] = [("gray", 1), ("colorful", 2), ("blue", 3), ("orange", 4), ("silver", 5), ("gold", 6), ("azure", 7), ("green", 8)];

fn describe_style(s: Option<u8>) -> String {
    match s {
        None => "colorful".into(),
        Some(n) => STYLES.iter().find(|(_, v)| *v == n).map_or_else(|| n.to_string(), |(name, _)| name.to_string()),
    }
}

fn parse_style(s: &str, before: Option<u8>) -> Result<Option<u8>, String> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() || (s == "colorful" && before.is_none()) {
        return Ok(None);
    }
    if let Some((_, v)) = STYLES.iter().find(|(n, _)| *n == s) {
        return Ok(Some(*v));
    }
    match s.parse::<u8>() {
        Ok(n) if (1..=48).contains(&n) => Ok(Some(n)),
        _ => Err(format!("“{s}” isn't a color style (colorful, gray, blue, orange, silver, gold, azure, green or 1–48)")),
    }
}

fn describe_extra(c: &Chart) -> String {
    match c.kind {
        ChartKind::Histogram => {
            let b = &c.options.bins;
            let mut out = vec![match b.mode {
                BinMode::Auto => "auto".to_string(),
                BinMode::Width(w) => format!("width {}", num(w)),
                BinMode::Count(n) => format!("count {n}"),
            }];
            if let Some(v) = b.overflow {
                out.push(format!("overflow {}", num(v)));
            }
            if let Some(v) = b.underflow {
                out.push(format!("underflow {}", num(v)));
            }
            out.join(", ")
        }
        ChartKind::Waterfall if !c.options.subtotals.is_empty() => format!("totals {}", c.options.subtotals.iter().map(|i| (i + 1).to_string()).collect::<Vec<_>>().join(" ")),
        ChartKind::Doughnut => format!("hole {}", c.options.hole_size),
        ChartKind::Radar if c.options.radar_filled => "filled".into(),
        _ => String::new(),
    }
}

fn parse_extra(text: &str, c: &mut Chart) -> Result<(), String> {
    let mut bins = Bins::default();
    for item in items(text) {
        let lower = item.to_ascii_lowercase();
        let words: Vec<&str> = lower.split_whitespace().collect();
        match words.first().copied().unwrap_or("") {
            "auto" => bins.mode = BinMode::Auto,
            "width" => bins.mode = BinMode::Width(parse_num(words.get(1).copied(), "width")?.abs()),
            "count" => bins.mode = BinMode::Count(parse_num(words.get(1).copied(), "count")?.clamp(1.0, 1000.0) as u32),
            "overflow" => bins.overflow = Some(parse_num(words.get(1).copied(), "overflow")?),
            "underflow" => bins.underflow = Some(parse_num(words.get(1).copied(), "underflow")?),
            "totals" | "total" => {
                let mut v = Vec::new();
                for w in &words[1..] {
                    let n = w.parse::<u32>().map_err(|_| format!("“{w}” isn't a point number"))?;
                    v.push(n.saturating_sub(1));
                }
                c.options.subtotals = v;
            }
            "hole" => c.options.hole_size = parse_num(words.get(1).copied(), "hole")?.clamp(10.0, 90.0) as u8,
            "filled" => c.options.radar_filled = true,
            "lines" => c.options.radar_filled = false,
            _ => return Err(format!("“{item}” isn't a bin, total or shape option")),
        }
    }
    if c.kind == ChartKind::Histogram {
        c.options.bins = bins;
    }
    if c.kind == ChartKind::Radar && !text.to_ascii_lowercase().contains("filled") {
        c.options.radar_filled = false;
    }
    if c.kind == ChartKind::Waterfall && !text.to_ascii_lowercase().contains("total") {
        c.options.subtotals.clear();
    }
    Ok(())
}

pub(crate) type FormFields = Vec<(SharedString, SharedString, String)>;

/// The form's fields for a chart (label, hint, text).
pub(crate) fn fields_for(c: &Chart, series_names: &[String]) -> FormFields {
    let axis_hint = "title Sales, min 0, max 100, unit 10, log, format 0%, grid, minor, hidden";
    let mut f: FormFields = vec![
        ("Title".into(), "None".into(), c.title.clone().unwrap_or_default()),
        (
            "Type".into(),
            "column · bar · line · area · pie · doughnut · scatter · bubble · radar · histogram · waterfall (+ stacked)".into(),
            format!("{}{}", kind_name(c.kind), if c.stacked { " stacked" } else { "" }),
        ),
        ("Legend".into(), "bottom · top · left · right · topright · none".into(), c.options.legend.map_or("none".into(), |l| l.label().to_string())),
        ("Colors".into(), "colorful · gray · blue · orange · silver · gold · azure · green · 1–48".into(), describe_style(c.options.style)),
        ("Category (x) axis".into(), axis_hint.into(), describe_axis(&c.options.x_axis)),
        ("Value (y) axis".into(), axis_hint.into(), describe_axis(&c.options.y_axis)),
        ("Secondary value axis".into(), "For series marked “secondary”".into(), describe_axis(&c.options.y2_axis)),
        ("Bins · totals · shape".into(), "auto · width 5 · count 8, overflow 100, underflow 0 · totals 3 7 · hole 50 · filled".into(), describe_extra(c)),
    ];
    for (i, s) in c.series.iter().enumerate() {
        let name = series_names.get(i).cloned().unwrap_or_else(|| format!("Series {}", i + 1));
        f.push((format!("Series: {name}").into(), "line, secondary, color #C00000, width 2, dash, marker square 7, smooth, labels value, trend poly 3 eq r2, errors 5%".into(), describe_series(&s.format, c.kind)));
    }
    f
}

/// Apply the form's texts to a copy of the chart.
pub(crate) fn apply_fields(c: &Chart, texts: &[String]) -> Result<Chart, String> {
    let text = |i: usize| texts.get(i).map_or("", |s| s.as_str());
    let mut out = c.clone();
    out.title = Some(text(0).trim().to_string()).filter(|t| !t.is_empty());
    let ty = text(1).to_ascii_lowercase();
    let mut kind = None;
    let mut stacked = false;
    for w in ty.split([' ', ',']).filter(|w| !w.is_empty()) {
        match w {
            "stacked" => stacked = true,
            w => kind = Some(parse_kind(w).ok_or_else(|| format!("“{w}” isn't a chart type"))?),
        }
    }
    crate::charts::switch_kind(&mut out, kind.unwrap_or(c.kind));
    out.stacked = stacked;
    out.options.legend = match text(2).trim().to_ascii_lowercase().as_str() {
        "none" | "" | "hidden" | "off" => None,
        l => Some(LegendPos::ALL.into_iter().find(|p| p.label() == l).ok_or_else(|| format!("“{l}” isn't a legend position"))?),
    };
    out.options.style = parse_style(text(3), c.options.style)?;
    out.options.x_axis = parse_axis(text(4))?;
    out.options.y_axis = parse_axis(text(5))?;
    out.options.y2_axis = parse_axis(text(6))?;
    parse_extra(text(7), &mut out)?;
    for i in 0..out.series.len() {
        let base = out.series[i].format.clone();
        out.series[i].format = parse_series(text(FIXED + i), &base, out.kind)?;
    }
    Ok(out)
}

impl Spreadsheet {
    /// The form's fields for the selected chart.
    pub(crate) fn chart_form_fields(&self) -> Option<FormFields> {
        let c = self.selected_chart.and_then(|i| self.wb.layout(self.cur).charts.get(i))?;
        let names: Vec<String> = self.wb.chart_data(self.cur, c).series.into_iter().map(|s| s.0).collect();
        Some(fields_for(c, &names))
    }

    /// Apply the chart form; Err with what's wrong.
    pub(crate) fn apply_chart_form(&mut self, texts: &[String]) -> Result<(), String> {
        let c = self.selected_chart.and_then(|i| self.wb.layout(self.cur).charts.get(i)).ok_or("Select a chart first")?;
        let new = apply_fields(c, texts)?;
        self.update_chart(move |c| *c = new);
        Ok(())
    }

    /// Change every series of the selected chart (palette shortcuts).
    pub(crate) fn chart_series_edit(&mut self, f: impl Fn(&mut Chart, usize)) {
        if self.selected_chart.is_none() {
            self.select_some_chart();
        }
        if self.selected_chart.is_none() {
            self.message = Some(("There's no chart on this sheet".into(), true));
            return;
        }
        self.update_chart(|c| {
            for i in 0..c.series.len() {
                f(c, i);
            }
        });
    }

    /// Select the sheet's only (or first) chart when none is selected.
    pub(crate) fn select_some_chart(&mut self) {
        if self.selected_chart.is_none() && !self.wb.layout(self.cur).charts.is_empty() {
            self.selected_chart = Some(0);
        }
    }

    /// Combo: draw the last series as a line on the secondary axis.
    pub(crate) fn chart_combo_last_series(&mut self) {
        self.select_some_chart();
        let Some(n) = self.selected_chart.and_then(|i| self.wb.layout(self.cur).charts.get(i)).map(|c| c.series.len()) else {
            self.message = Some(("There's no chart on this sheet".into(), true));
            return;
        };
        if n < 2 {
            self.message = Some(("A combo chart needs at least two series".into(), true));
            return;
        }
        self.update_chart(|c| {
            if !c.kind.combinable() {
                crate::charts::switch_kind(c, ChartKind::Column);
            }
            let f = &mut c.series[n - 1].format;
            f.kind = (c.kind != ChartKind::Line).then_some(ChartKind::Line);
            f.secondary = true;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_lines_roundtrip() {
        let f = SeriesFormat {
            kind: Some(ChartKind::Line),
            secondary: true,
            fill: Paint::Solid(0x123456),
            line: Paint::Solid(0xC00000),
            line_width: Some(2.5),
            dash: Dash::DashDot,
            marker: Marker { shape: Some(MarkerShape::Diamond), size: Some(9) },
            smooth: true,
            labels: DataLabels { value: true, percent: true, ..DataLabels::default() },
            trendline: Some(Trendline { kind: TrendKind::Polynomial(4), equation: true, r_squared: false, name: None }),
            error_bars: Some(ErrorBars { kind: ErrorKind::Percent, value: 7.5, dir: ErrorDir::Minus }),
            points: vec![(1, 0xFF0000)],
        };
        let text = describe_series(&f, ChartKind::Column);
        assert_eq!(text, "line, secondary, fill #123456, line red, width 2.5, dashdot, marker diamond 9, smooth, labels value percent, trend poly 4 eq, errors 7.5% minus");
        assert_eq!(parse_series(&text, &f, ChartKind::Column).unwrap(), f);
        let plain = SeriesFormat::default();
        assert_eq!(describe_series(&plain, ChartKind::Column), "");
        let g = parse_series("color blue, trend average 3, errors sd 2, no marker", &plain, ChartKind::Line).unwrap();
        assert_eq!((g.line, g.fill), (Paint::Solid(0x4472C4), Paint::Auto), "color on a line series is its line");
        assert_eq!(g.trendline.unwrap().kind, TrendKind::MovingAverage(3));
        assert_eq!(g.error_bars.unwrap().kind, ErrorKind::StdDev);
        assert_eq!(g.marker.shape, Some(MarkerShape::None));
        assert!(parse_series("sparkles", &plain, ChartKind::Line).is_err());
        assert!(parse_series("color mauvish", &plain, ChartKind::Line).is_err());
    }

    #[test]
    fn axis_lines_roundtrip() {
        let a = Axis { title: Some("Sales, net".into()), min: Some(0.0), max: Some(1.0), major: Some(0.25), log_base: None, num_fmt: Some("0%".into()), gridlines: true, minor_gridlines: true, hidden: false };
        let text = describe_axis(&a);
        assert_eq!(text, "title \"Sales, net\", min 0, max 1, unit 0.25, format 0%, grid, minor");
        assert_eq!(parse_axis(&text).unwrap(), a);
        assert_eq!(parse_axis("log, hidden").unwrap(), Axis { log_base: Some(10.0), hidden: true, ..Axis::default() });
        assert!(parse_axis("sideways").is_err());
    }

    #[test]
    fn the_whole_form_roundtrips() {
        let e = std::sync::Arc::new(xl_engine::formula::Expr::Number(1.0));
        let mut c = Chart::new(ChartKind::Histogram, vec![xl_engine::chart::ChartSeries::new(None, None, e)], xl_engine::CellRef::new(0, 0), (100.0, 100.0));
        c.options.bins = Bins { mode: BinMode::Count(7), overflow: Some(90.0), underflow: None };
        c.options.style = Some(4);
        let texts: Vec<String> = fields_for(&c, &[]).into_iter().map(|f| f.2).collect();
        assert_eq!(apply_fields(&c, &texts).unwrap(), c);
        // Change the type and the legend.
        let mut t = texts.clone();
        t[1] = "column stacked".into();
        t[2] = "right".into();
        let d = apply_fields(&c, &t).unwrap();
        assert_eq!((d.kind, d.stacked, d.options.legend), (ChartKind::Column, true, Some(LegendPos::Right)));
        t[1] = "pyramid".into();
        assert!(apply_fields(&c, &t).is_err());
    }
}
