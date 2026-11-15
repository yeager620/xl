//! Writing chart and drawing XML for charts made or edited in xl, to patch
//! into a package: DrawingML chart parts (combo groups, secondary axes,
//! series formatting, trendlines, error bars) and chartex parts for
//! histograms and waterfalls. Everything written here reads back to the
//! same model in `xlsx_charts`.

use xl_engine::chart::{Axis, BinMode, Chart, ChartKind, ChartSeries, Dash, ErrorDir, ErrorKind, LegendPos, Paint, SeriesFormat, TrendKind};
use xl_engine::formula::Expr;
use xl_engine::{SheetId, Value, Workbook, quote_sheet_name};

use crate::xlsx_patch::esc;

/// "Sheet1!$A$2:$A$9" for a reference (sheet always named, as charts need).
fn chart_ref(wb: &Workbook, home: SheetId, e: &Expr) -> Option<String> {
    let (sheet, r) = e.reference()?;
    let name = quote_sheet_name(wb.sheet_name(sheet.unwrap_or(home)));
    let abs = |c: xl_engine::CellRef| format!("${}${}", xl_engine::col_name(c.col), c.row + 1);
    Some(if r.start == r.end { format!("{name}!{}", abs(r.start)) } else { format!("{name}!{}:{}", abs(r.start), abs(r.end)) })
}

/// A number as XML writes it (no exponent surprises for ordinary values).
fn num(v: f64) -> String {
    xl_engine::precise(v)
}

fn solid(c: u32) -> String {
    format!("<a:solidFill><a:srgbClr val=\"{:06X}\"/></a:solidFill>", c & 0xFF_FFFF)
}

fn paint_xml(p: Paint) -> String {
    match p {
        Paint::Auto => String::new(),
        Paint::None => "<a:noFill/>".into(),
        Paint::Solid(c) => solid(c),
    }
}

/// `<c:spPr>` (or `<cx:spPr>`) for a series' fill and line; empty when automatic.
fn shape_props(f: &SeriesFormat, ns: &str) -> String {
    let mut ln = String::new();
    if f.line != Paint::Auto || f.line_width.is_some() || f.dash != Dash::Solid {
        let w = f.line_width.map_or(String::new(), |w| format!(" w=\"{}\"", (w * 12700.0).round() as i64));
        let dash = match f.dash {
            Dash::Solid => "",
            Dash::Dash => "<a:prstDash val=\"dash\"/>",
            Dash::Dot => "<a:prstDash val=\"sysDot\"/>",
            Dash::DashDot => "<a:prstDash val=\"dashDot\"/>",
            Dash::LongDash => "<a:prstDash val=\"lgDash\"/>",
        };
        ln = format!("<a:ln{w}>{}{dash}</a:ln>", paint_xml(f.line));
    }
    let fill = paint_xml(f.fill);
    if fill.is_empty() && ln.is_empty() { String::new() } else { format!("<{ns}:spPr>{fill}{ln}</{ns}:spPr>") }
}

fn rich_title(text: &str, vertical: bool) -> String {
    let body = if vertical { "<a:bodyPr rot=\"-5400000\" vert=\"horz\"/>" } else { "<a:bodyPr/>" };
    format!("<c:title><c:tx><c:rich>{body}<a:lstStyle/><a:p><a:r><a:t>{}</a:t></a:r></a:p></c:rich></c:tx><c:overlay val=\"0\"/></c:title>", esc(text))
}

fn series_name(wb: &Workbook, home: SheetId, s: &ChartSeries) -> String {
    match s.name.as_deref() {
        Some(Expr::Text(t)) => format!("<c:tx><c:v>{}</c:v></c:tx>", esc(t)),
        Some(e) => chart_ref(wb, home, e).map_or(String::new(), |r| format!("<c:tx><c:strRef><c:f>{}</c:f></c:strRef></c:tx>", esc(&r))),
        None => String::new(),
    }
}

/// One `<c:ser>` of a group of `kind`.
fn series_xml(wb: &Workbook, home: SheetId, i: usize, s: &ChartSeries, kind: ChartKind) -> Option<String> {
    let values = chart_ref(wb, home, &s.values)?;
    let f = &s.format;
    let mut out = format!("<c:ser><c:idx val=\"{i}\"/><c:order val=\"{i}\"/>{}{}", series_name(wb, home, s), shape_props(f, "c"));
    let has_marker = matches!(kind, ChartKind::Line | ChartKind::Scatter | ChartKind::Radar);
    if has_marker && (f.marker.shape.is_some() || f.marker.size.is_some()) {
        out.push_str("<c:marker>");
        if let Some(shape) = f.marker.shape {
            out.push_str(&format!("<c:symbol val=\"{}\"/>", shape.name()));
        }
        if let Some(size) = f.marker.size {
            out.push_str(&format!("<c:size val=\"{}\"/>", size.clamp(2, 72)));
        }
        out.push_str("</c:marker>");
    }
    for (p, c) in &f.points {
        out.push_str(&format!("<c:dPt><c:idx val=\"{p}\"/><c:spPr>{}</c:spPr></c:dPt>", solid(*c)));
    }
    if f.labels.any() {
        let b = |v: bool| if v { 1 } else { 0 };
        out.push_str(&format!(
            "<c:dLbls><c:showLegendKey val=\"0\"/><c:showVal val=\"{}\"/><c:showCatName val=\"{}\"/><c:showSerName val=\"{}\"/><c:showPercent val=\"{}\"/><c:showBubbleSize val=\"0\"/></c:dLbls>",
            b(f.labels.value),
            b(f.labels.category),
            b(f.labels.series),
            b(f.labels.percent)
        ));
    }
    let can_trend = !matches!(kind, ChartKind::Pie | ChartKind::Doughnut | ChartKind::Radar);
    if let (Some(t), true) = (&f.trendline, can_trend) {
        let name = t.name.as_ref().map_or(String::new(), |n| format!("<c:name>{}</c:name>", esc(n)));
        let ty = match t.kind {
            TrendKind::Linear => "<c:trendlineType val=\"linear\"/>".to_string(),
            TrendKind::Exponential => "<c:trendlineType val=\"exp\"/>".to_string(),
            TrendKind::Logarithmic => "<c:trendlineType val=\"log\"/>".to_string(),
            TrendKind::Power => "<c:trendlineType val=\"power\"/>".to_string(),
            TrendKind::Polynomial(n) => format!("<c:trendlineType val=\"poly\"/><c:order val=\"{}\"/>", n.clamp(2, 6)),
            TrendKind::MovingAverage(n) => format!("<c:trendlineType val=\"movingAvg\"/><c:period val=\"{}\"/>", n.max(2)),
        };
        out.push_str(&format!("<c:trendline>{name}{ty}<c:dispRSqr val=\"{}\"/><c:dispEq val=\"{}\"/></c:trendline>", t.r_squared as u8, t.equation as u8));
    }
    if let (Some(e), true) = (&f.error_bars, can_trend) {
        let dir = if kind.is_xy() { "<c:errDir val=\"y\"/>" } else { "" };
        let ty = match e.dir {
            ErrorDir::Both => "both",
            ErrorDir::Plus => "plus",
            ErrorDir::Minus => "minus",
        };
        let (vt, val) = match e.kind {
            ErrorKind::Fixed => ("fixedVal", true),
            ErrorKind::Percent => ("percentage", true),
            ErrorKind::StdDev => ("stdDev", true),
            ErrorKind::StdErr => ("stdErr", false),
        };
        let val = if val { format!("<c:val val=\"{}\"/>", num(e.value)) } else { String::new() };
        out.push_str(&format!("<c:errBars>{dir}<c:errBarType val=\"{ty}\"/><c:errValType val=\"{vt}\"/><c:noEndCap val=\"0\"/>{val}</c:errBars>"));
    }
    let cats = s.categories.as_deref().and_then(|e| chart_ref(wb, home, e));
    if kind.is_xy() {
        if let Some(r) = cats {
            out.push_str(&format!("<c:xVal><c:numRef><c:f>{}</c:f></c:numRef></c:xVal>", esc(&r)));
        }
        out.push_str(&format!("<c:yVal><c:numRef><c:f>{}</c:f></c:numRef></c:yVal>", esc(&values)));
    } else {
        if let Some(r) = cats {
            out.push_str(&format!("<c:cat><c:strRef><c:f>{}</c:f></c:strRef></c:cat>", esc(&r)));
        }
        out.push_str(&format!("<c:val><c:numRef><c:f>{}</c:f></c:numRef></c:val>", esc(&values)));
    }
    if kind == ChartKind::Bubble {
        if let Some(r) = s.sizes.as_deref().and_then(|e| chart_ref(wb, home, e)) {
            out.push_str(&format!("<c:bubbleSize><c:numRef><c:f>{}</c:f></c:numRef></c:bubbleSize>", esc(&r)));
        }
        out.push_str("<c:bubble3D val=\"0\"/>");
    }
    if matches!(kind, ChartKind::Line | ChartKind::Scatter) {
        out.push_str(&format!("<c:smooth val=\"{}\"/>", f.smooth as u8));
    }
    out.push_str("</c:ser>");
    Some(out)
}

/// An axis element. `cat`: a category axis (else a value axis).
#[allow(clippy::too_many_arguments)]
fn axis_xml(cat: bool, id: u32, cross: u32, pos: &str, ax: &Axis, deleted: bool, crosses_max: bool, vertical_title: bool) -> String {
    let tag = if cat { "catAx" } else { "valAx" };
    let mut scaling = String::from("<c:scaling>");
    if let Some(b) = ax.log_base {
        scaling.push_str(&format!("<c:logBase val=\"{}\"/>", num(b)));
    }
    scaling.push_str("<c:orientation val=\"minMax\"/>");
    if let Some(v) = ax.max {
        scaling.push_str(&format!("<c:max val=\"{}\"/>", num(v)));
    }
    if let Some(v) = ax.min {
        scaling.push_str(&format!("<c:min val=\"{}\"/>", num(v)));
    }
    scaling.push_str("</c:scaling>");
    let grid = format!("{}{}", if ax.gridlines { "<c:majorGridlines/>" } else { "" }, if ax.minor_gridlines { "<c:minorGridlines/>" } else { "" });
    let title = ax.title.as_ref().map_or(String::new(), |t| rich_title(t, vertical_title));
    let fmt = match &ax.num_fmt {
        Some(code) => format!("<c:numFmt formatCode=\"{}\" sourceLinked=\"0\"/>", esc(code)),
        None => "<c:numFmt formatCode=\"General\" sourceLinked=\"1\"/>".to_string(),
    };
    let crosses = if crosses_max { "max" } else { "autoZero" };
    let tail = if cat {
        "<c:auto val=\"1\"/><c:lblAlgn val=\"ctr\"/><c:lblOffset val=\"100\"/>".to_string()
    } else {
        format!("<c:crossBetween val=\"between\"/>{}", ax.major.map_or(String::new(), |m| format!("<c:majorUnit val=\"{}\"/>", num(m))))
    };
    format!(
        "<c:{tag}><c:axId val=\"{id}\"/>{scaling}<c:delete val=\"{}\"/><c:axPos val=\"{pos}\"/>{grid}{title}{fmt}<c:majorTickMark val=\"out\"/><c:minorTickMark val=\"none\"/><c:tickLblPos val=\"nextTo\"/><c:crossAx val=\"{cross}\"/><c:crosses val=\"{crosses}\"/>{tail}</c:{tag}>",
        deleted as u8
    )
}

const AX_X: u32 = 50010001;
const AX_Y: u32 = 50010002;
const AX_X2: u32 = 50010003;
const AX_Y2: u32 = 50010004;

/// A chart part (`xl/charts/chartN.xml`). Histograms and waterfalls are
/// chartex parts instead: see [`chartex_xml`].
pub(crate) fn chart_xml(wb: &Workbook, home: SheetId, chart: &Chart) -> String {
    let kind = chart.kind;
    let stackable = |k: ChartKind| chart.stacked && matches!(k, ChartKind::Column | ChartKind::Bar | ChartKind::Area);
    let secondary_on = chart.has_secondary();
    // Series grouped by (kind, secondary axis): the chart's own kind first.
    let mut groups: Vec<(ChartKind, bool, Vec<usize>)> = Vec::new();
    for i in 0..chart.series.len() {
        let key = (chart.series_kind(i), secondary_on && chart.series[i].format.secondary);
        match groups.iter_mut().find(|g| (g.0, g.1) == key) {
            Some(g) => g.2.push(i),
            None => groups.push((key.0, key.1, vec![i])),
        }
    }
    groups.sort_by_key(|g| (g.1, g.0 != kind));
    let mut plot = String::new();
    for (gk, secondary, members) in &groups {
        let series: String = members.iter().filter_map(|&i| series_xml(wb, home, i, &chart.series[i], *gk)).collect();
        let ids = if *secondary { format!("<c:axId val=\"{AX_X2}\"/><c:axId val=\"{AX_Y2}\"/>") } else { format!("<c:axId val=\"{AX_X}\"/><c:axId val=\"{AX_Y}\"/>") };
        let stacked = stackable(*gk);
        plot.push_str(&match gk {
            ChartKind::Column | ChartKind::Bar => format!(
                "<c:barChart><c:barDir val=\"{}\"/><c:grouping val=\"{}\"/><c:varyColors val=\"0\"/>{series}<c:gapWidth val=\"150\"/>{}{ids}</c:barChart>",
                if *gk == ChartKind::Bar { "bar" } else { "col" },
                if stacked { "stacked" } else { "clustered" },
                if stacked { "<c:overlap val=\"100\"/>" } else { "" }
            ),
            ChartKind::Line => format!("<c:lineChart><c:grouping val=\"standard\"/><c:varyColors val=\"0\"/>{series}<c:marker val=\"1\"/>{ids}</c:lineChart>"),
            ChartKind::Area => format!("<c:areaChart><c:grouping val=\"{}\"/><c:varyColors val=\"0\"/>{series}{ids}</c:areaChart>", if stacked { "stacked" } else { "standard" }),
            ChartKind::Pie => format!("<c:pieChart><c:varyColors val=\"1\"/>{series}<c:firstSliceAng val=\"0\"/></c:pieChart>"),
            ChartKind::Doughnut => format!("<c:doughnutChart><c:varyColors val=\"1\"/>{series}<c:firstSliceAng val=\"0\"/><c:holeSize val=\"{}\"/></c:doughnutChart>", chart.options.hole_size.clamp(10, 90)),
            ChartKind::Scatter => format!("<c:scatterChart><c:scatterStyle val=\"lineMarker\"/><c:varyColors val=\"0\"/>{series}{ids}</c:scatterChart>"),
            ChartKind::Bubble => format!("<c:bubbleChart><c:varyColors val=\"0\"/>{series}<c:bubbleScale val=\"100\"/><c:showNegBubbles val=\"0\"/>{ids}</c:bubbleChart>"),
            ChartKind::Radar => format!("<c:radarChart><c:radarStyle val=\"{}\"/><c:varyColors val=\"0\"/>{series}{ids}</c:radarChart>", if chart.options.radar_filled { "filled" } else { "marker" }),
            ChartKind::Histogram | ChartKind::Waterfall => String::new(),
        });
    }
    let o = &chart.options;
    let axes = if kind.is_round() {
        String::new()
    } else {
        let xy = kind.is_xy();
        // Horizontal bars swap where the axes go.
        let (xpos, ypos, y2pos, x2pos) = if kind == ChartKind::Bar { ("l", "b", "t", "r") } else { ("b", "l", "r", "t") };
        let mut a = axis_xml(!xy, AX_X, AX_Y, xpos, &o.x_axis, o.x_axis.hidden, false, kind == ChartKind::Bar);
        a.push_str(&axis_xml(false, AX_Y, AX_X, ypos, &o.y_axis, o.y_axis.hidden, false, kind != ChartKind::Bar));
        if secondary_on {
            a.push_str(&axis_xml(!xy, AX_X2, AX_Y2, x2pos, &Axis::default(), true, false, false));
            a.push_str(&axis_xml(false, AX_Y2, AX_X2, y2pos, &o.y2_axis, o.y2_axis.hidden, true, kind != ChartKind::Bar));
        }
        a
    };
    let title = chart.title.as_ref().map_or(String::new(), |t| {
        format!("<c:title><c:tx><c:rich><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>{}</a:t></a:r></a:p></c:rich></c:tx><c:overlay val=\"0\"/></c:title>", esc(t))
    });
    let legend = o.legend.map_or(String::new(), |p| {
        let pos = match p {
            LegendPos::Bottom => "b",
            LegendPos::Top => "t",
            LegendPos::Left => "l",
            LegendPos::Right => "r",
            LegendPos::TopRight => "tr",
        };
        format!("<c:legend><c:legendPos val=\"{pos}\"/><c:overlay val=\"0\"/></c:legend>")
    });
    let style = o.style.map_or(String::new(), |s| format!("<c:style val=\"{}\"/>", s.clamp(1, 48)));
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<c:chartSpace xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><c:roundedCorners val=\"0\"/>{style}<c:chart>{title}<c:autoTitleDeleted val=\"{}\"/><c:plotArea><c:layout/>{plot}{axes}</c:plotArea>{legend}<c:plotVisOnly val=\"1\"/><c:dispBlanksAs val=\"gap\"/></c:chart></c:chartSpace>",
        o.auto_title_deleted as u8
    )
}

/// The cells of a reference as a chartex cache level.
fn cache_level(wb: &Workbook, home: SheetId, e: &Expr, numeric: bool) -> String {
    let Some((sheet, r)) = e.reference() else { return String::new() };
    let sheet = sheet.unwrap_or(home);
    let end_row = wb.store(sheet).extent().map_or(r.end.row, |x| r.end.row.min(x.row).max(r.start.row));
    let cells: Vec<xl_engine::CellRef> = xl_engine::Range::new(r.start, xl_engine::CellRef::new(end_row, r.end.col)).iter().take(100_000).collect();
    let mut pts = String::new();
    for (i, c) in cells.iter().enumerate() {
        let a = xl_engine::Addr::new(sheet, *c);
        if numeric {
            if let Value::Number(n) = wb.value(a) {
                pts.push_str(&format!("<cx:pt idx=\"{i}\">{}</cx:pt>", num(*n)));
            }
        } else {
            pts.push_str(&format!("<cx:pt idx=\"{i}\">{}</cx:pt>", esc(&wb.display(a).text)));
        }
    }
    let fmt = if numeric { " formatCode=\"General\"" } else { "" };
    format!("<cx:lvl ptCount=\"{}\"{fmt}>{pts}</cx:lvl>", cells.len())
}

fn cx_text(text: &str) -> String {
    format!("<cx:tx><cx:txData><cx:v>{}</cx:v></cx:txData></cx:tx>", esc(text))
}

fn cx_axis(id: u32, ax: &Axis, scaling: String) -> String {
    let title = ax.title.as_ref().map_or(String::new(), |t| format!("<cx:title>{}</cx:title>", cx_text(t)));
    let grid = format!("{}{}", if ax.gridlines { "<cx:majorGridlines/>" } else { "" }, if ax.minor_gridlines { "<cx:minorGridlines/>" } else { "" });
    let fmt = ax.num_fmt.as_ref().map_or(String::new(), |c| format!("<cx:numFmt formatCode=\"{}\" sourceLinked=\"0\"/>", esc(c)));
    let hidden = if ax.hidden { " hidden=\"1\"" } else { "" };
    format!("<cx:axis id=\"{id}\"{hidden}>{scaling}{title}{grid}<cx:tickLabels/>{fmt}</cx:axis>")
}

/// A chartex part (`xl/charts/chartExN.xml`) for a histogram or waterfall.
pub(crate) fn chartex_xml(wb: &Workbook, home: SheetId, chart: &Chart) -> String {
    let o = &chart.options;
    let mut data = String::new();
    let mut series = String::new();
    for (i, s) in chart.series.iter().enumerate() {
        let Some(values) = chart_ref(wb, home, &s.values) else { continue };
        let cats = s.categories.as_deref().and_then(|e| chart_ref(wb, home, e).map(|r| (r, e)));
        let cat_dim = cats.map_or(String::new(), |(r, e)| format!("<cx:strDim type=\"cat\"><cx:f>{}</cx:f>{}</cx:strDim>", esc(&r), cache_level(wb, home, e, false)));
        data.push_str(&format!("<cx:data id=\"{i}\">{cat_dim}<cx:numDim type=\"val\"><cx:f>{}</cx:f>{}</cx:numDim></cx:data>", esc(&values), cache_level(wb, home, &s.values, true)));
        let name = match s.name.as_deref() {
            Some(Expr::Text(t)) => cx_text(t),
            Some(e) => chart_ref(wb, home, e).map_or(String::new(), |r| {
                let shown = wb.resolve_reference(&r, home).map(|(sh, rg)| wb.display(xl_engine::Addr::new(sh, rg.start)).text).unwrap_or_default();
                format!("<cx:tx><cx:txData><cx:f>{}</cx:f><cx:v>{}</cx:v></cx:txData></cx:tx>", esc(&r), esc(&shown))
            }),
            None => String::new(),
        };
        let f = &s.format;
        let points: String = f.points.iter().map(|(p, c)| format!("<cx:dataPt idx=\"{p}\"><cx:spPr>{}</cx:spPr></cx:dataPt>", solid(*c))).collect();
        let labels = if f.labels.any() {
            format!("<cx:dataLabels><cx:visibility seriesName=\"{}\" categoryName=\"{}\" value=\"{}\"/></cx:dataLabels>", f.labels.series as u8, f.labels.category as u8, f.labels.value as u8)
        } else {
            String::new()
        };
        let (layout_id, layout) = if chart.kind == ChartKind::Histogram {
            let b = &o.bins;
            let edge = |k: &str, v: Option<f64>| v.map_or(String::new(), |v| format!(" {k}=\"{}\"", num(v)));
            let size = match b.mode {
                BinMode::Auto => String::new(),
                BinMode::Width(w) => format!("<cx:binSize val=\"{}\"/>", num(w)),
                BinMode::Count(c) => format!("<cx:binCount val=\"{c}\"/>"),
            };
            ("clusteredColumn", format!("<cx:layoutPr><cx:binning intervalClosed=\"r\"{}{}>{size}</cx:binning></cx:layoutPr>", edge("underflow", b.underflow), edge("overflow", b.overflow)))
        } else {
            let totals: String = o.subtotals.iter().map(|i| format!("<cx:idx val=\"{i}\"/>")).collect();
            ("waterfall", format!("<cx:layoutPr><cx:visibility connectorLines=\"1\"/>{}</cx:layoutPr>", if totals.is_empty() { String::new() } else { format!("<cx:subtotals>{totals}</cx:subtotals>") }))
        };
        series.push_str(&format!(
            "<cx:series layoutId=\"{layout_id}\" uniqueId=\"{{2A7E5F00-0000-4000-8000-{:012X}}}\" formatIdx=\"{i}\">{name}{}{points}{labels}<cx:dataId val=\"{i}\"/>{layout}</cx:series>",
            i + 1,
            shape_props(f, "cx")
        ));
    }
    let gap = if chart.kind == ChartKind::Histogram { "0" } else { "0.5" };
    let val_scaling = {
        let y = &o.y_axis;
        let a = |k: &str, v: Option<f64>| v.map_or(String::new(), |v| format!(" {k}=\"{}\"", num(v)));
        format!("<cx:valScaling{}{}{}/>", a("max", y.max), a("min", y.min), a("majorUnit", y.major))
    };
    let axes = format!("{}{}", cx_axis(0, &o.x_axis, format!("<cx:catScaling gapWidth=\"{gap}\"/>")), cx_axis(1, &o.y_axis, val_scaling));
    let title = chart.title.as_ref().map_or(String::new(), |t| format!("<cx:title pos=\"t\" align=\"ctr\" overlay=\"0\">{}</cx:title>", cx_text(t)));
    let legend = o.legend.map_or(String::new(), |p| {
        let (pos, align) = match p {
            LegendPos::Bottom => ("b", "ctr"),
            LegendPos::Top => ("t", "ctr"),
            LegendPos::Left => ("l", "ctr"),
            LegendPos::Right => ("r", "ctr"),
            LegendPos::TopRight => ("t", "max"),
        };
        format!("<cx:legend pos=\"{pos}\" align=\"{align}\" overlay=\"0\"/>")
    });
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<cx:chartSpace xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" xmlns:cx=\"http://schemas.microsoft.com/office/drawing/2014/chartex\"><cx:chartData>{data}</cx:chartData><cx:chart>{title}<cx:plotArea><cx:plotAreaRegion>{series}</cx:plotAreaRegion>{axes}</cx:plotArea>{legend}</cx:chart></cx:chartSpace>"
    )
}

/// A two-cell anchor holding a chart frame (to be placed by `reanchor`).
pub(crate) fn chart_anchor(rid: &str, id: u32, name: &str) -> String {
    format!(
        "<xdr:twoCellAnchor editAs=\"oneCell\"><xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from><xdr:to><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>1</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to><xdr:graphicFrame macro=\"\"><xdr:nvGraphicFramePr><xdr:cNvPr id=\"{id}\" name=\"{}\"/><xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr><xdr:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/></xdr:xfrm><a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/chart\"><c:chart xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\" r:id=\"{rid}\"/></a:graphicData></a:graphic></xdr:graphicFrame><xdr:clientData/></xdr:twoCellAnchor>",
        esc(name)
    )
}

/// A two-cell anchor holding a chartex frame: the frame for Excel 2016 and
/// later, and a "not available" box for older readers.
pub(crate) fn chartex_anchor(rid: &str, id: u32, name: &str) -> String {
    format!(
        "<xdr:twoCellAnchor editAs=\"oneCell\"><xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from><xdr:to><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>1</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>\
<mc:AlternateContent xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\"><mc:Choice xmlns:cx1=\"http://schemas.microsoft.com/office/drawing/2015/9/8/chartex\" Requires=\"cx1\">\
<xdr:graphicFrame macro=\"\"><xdr:nvGraphicFramePr><xdr:cNvPr id=\"{id}\" name=\"{}\"/><xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr><xdr:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/></xdr:xfrm><a:graphic><a:graphicData uri=\"http://schemas.microsoft.com/office/drawing/2014/chartex\"><cx:chart xmlns:cx=\"http://schemas.microsoft.com/office/drawing/2014/chartex\" r:id=\"{rid}\"/></a:graphicData></a:graphic></xdr:graphicFrame></mc:Choice>\
<mc:Fallback><xdr:sp macro=\"\" textlink=\"\"><xdr:nvSpPr><xdr:cNvPr id=\"{}\" name=\"\"/><xdr:cNvSpPr><a:spLocks noTextEdit=\"1\"/></xdr:cNvSpPr></xdr:nvSpPr><xdr:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"4572000\" cy=\"2743200\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom><a:solidFill><a:prstClr val=\"white\"/></a:solidFill><a:ln w=\"1\"><a:solidFill><a:prstClr val=\"green\"/></a:solidFill></a:ln></xdr:spPr><xdr:txBody><a:bodyPr vertOverflow=\"clip\" horzOverflow=\"clip\"/><a:lstStyle/><a:p><a:r><a:rPr lang=\"en-US\" sz=\"1100\"/><a:t>This chart isn't available in your version of Excel.</a:t></a:r></a:p></xdr:txBody></xdr:sp></mc:Fallback></mc:AlternateContent>\
<xdr:clientData/></xdr:twoCellAnchor>",
        esc(name),
        id + 50_000
    )
}

/// An empty drawing part.
pub(crate) const EMPTY_DRAWING: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<xdr:wsDr xmlns:xdr=\"http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"></xdr:wsDr>";
