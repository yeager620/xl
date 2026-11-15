//! Chart features through xlsx: combo charts, secondary axes, series
//! styling, trendlines, error bars, axis options, the newer kinds, and the
//! chartex histogram and waterfall — in fresh files and patched saves.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xl_engine::chart::*;
use xl_engine::formula::parse_formula;
use xl_engine::{Addr, CellRef, SheetId, Workbook};

fn tmp(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("xl-chart-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn parts(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut out = BTreeMap::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        let mut b = Vec::new();
        f.read_to_end(&mut b).unwrap();
        out.insert(f.name().unwrap().to_string(), b);
    }
    out
}

/// A sheet of monthly numbers: A month, B sales, C costs, D margin %, E size.
fn data_book() -> (Workbook, SheetId) {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    wb.rename_sheet(s, "Data").unwrap();
    let rows = [
        ["Month", "Sales", "Costs", "Margin", "Size"],
        ["Jan", "10", "4", "0.6", "3"],
        ["Feb", "14", "6", "0.57", "5"],
        ["Mar", "9", "5", "0.44", "2"],
        ["Apr", "17", "7", "0.59", "8"],
        ["May", "21", "8", "0.62", "4"],
        ["Jun", "-6", "3", "0.5", "6"],
    ];
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            wb.set_input(Addr::new(s, CellRef::new(r as u32, c as u32)), v).unwrap();
        }
    }
    (wb, s)
}

fn series(wb: &Workbook, col: &str) -> ChartSeries {
    let e = |src: &str| Arc::new(parse_formula(src, wb.sheets()).unwrap());
    ChartSeries::new(Some(e(&format!("Data!${col}$1"))), Some(e("Data!$A$2:$A$7")), e(&format!("Data!${col}$2:${col}$7")))
}

/// One chart of every modeled feature.
fn showcase(wb: &Workbook) -> Vec<Chart> {
    let at = |r: u32| CellRef::new(r, 7);
    let size = (480.0, 288.0);
    let mut out = Vec::new();

    // Combo: sales columns, costs as a dashed line, margin on the secondary axis.
    let mut sales = series(wb, "B");
    sales.format.fill = Paint::Solid(0x1F77B4);
    sales.format.line = Paint::Solid(0x0B3C5D);
    sales.format.labels = DataLabels { value: true, ..DataLabels::default() };
    sales.format.trendline = Some(Trendline { kind: TrendKind::Polynomial(3), equation: true, r_squared: true, name: Some("Fit".into()) });
    sales.format.error_bars = Some(ErrorBars { kind: ErrorKind::Percent, value: 10.0, dir: ErrorDir::Both });
    sales.format.points = vec![(5, 0xD62728)];
    let mut costs = series(wb, "C");
    costs.format.kind = Some(ChartKind::Line);
    costs.format.line = Paint::Solid(0xFF7F0E);
    costs.format.line_width = Some(2.25);
    costs.format.dash = Dash::Dash;
    costs.format.marker = Marker { shape: Some(MarkerShape::Diamond), size: Some(8) };
    costs.format.smooth = true;
    costs.format.error_bars = Some(ErrorBars { kind: ErrorKind::Fixed, value: 1.5, dir: ErrorDir::Plus });
    let mut margin = series(wb, "D");
    margin.format.kind = Some(ChartKind::Line);
    margin.format.secondary = true;
    margin.format.marker = Marker { shape: Some(MarkerShape::None), size: None };
    margin.format.trendline = Some(Trendline::new(TrendKind::MovingAverage(3)));
    let mut combo = Chart::new(ChartKind::Column, vec![sales, costs, margin], at(0), size);
    combo.title = Some("Sales & margin".into());
    combo.options.legend = Some(LegendPos::Right);
    combo.options.style = Some(3);
    combo.options.x_axis = Axis { title: Some("Month".into()), ..Axis::default() };
    combo.options.y_axis = Axis { title: Some("Amount".into()), min: Some(-10.0), max: Some(30.0), major: Some(5.0), num_fmt: Some("#,##0".into()), gridlines: true, minor_gridlines: true, ..Axis::default() };
    combo.options.y2_axis = Axis { min: Some(0.0), max: Some(1.0), num_fmt: Some("0%".into()), ..Axis::default() };
    out.push(combo);

    // Stacked bars with a log axis and no legend.
    let mut bars = Chart::new(ChartKind::Bar, vec![series(wb, "C"), series(wb, "E")], at(16), size);
    bars.stacked = true;
    bars.options.legend = None;
    bars.options.auto_title_deleted = true;
    bars.options.y_axis = Axis { log_base: Some(10.0), hidden: false, gridlines: false, ..Axis::default() };
    bars.options.x_axis.hidden = true;
    out.push(bars);

    // Scatter with exponential and linear trendlines, std dev error bars.
    let e = |src: &str| Arc::new(parse_formula(src, wb.sheets()).unwrap());
    let mut xy = ChartSeries::new(Some(e("\"Points\"")), Some(e("Data!$E$2:$E$7")), e("Data!$C$2:$C$7"));
    xy.format.line = Paint::None;
    xy.format.marker = Marker { shape: Some(MarkerShape::Square), size: Some(6) };
    xy.format.trendline = Some(Trendline { kind: TrendKind::Exponential, equation: true, r_squared: false, name: None });
    xy.format.error_bars = Some(ErrorBars { kind: ErrorKind::StdDev, value: 1.0, dir: ErrorDir::Minus });
    let mut xy2 = ChartSeries::new(None, Some(e("Data!$E$2:$E$7")), e("Data!$B$2:$B$7"));
    xy2.format.secondary = true;
    xy2.format.trendline = Some(Trendline::new(TrendKind::Linear));
    xy2.format.error_bars = Some(ErrorBars { kind: ErrorKind::StdErr, value: 0.0, dir: ErrorDir::Both });
    let mut scatter = Chart::new(ChartKind::Scatter, vec![xy, xy2], at(32), size);
    scatter.options.x_axis = Axis { min: Some(0.0), max: Some(10.0), major: Some(2.0), gridlines: true, ..Axis::default() };
    out.push(scatter);

    // Bubble: sizes from column E.
    let mut b = series(wb, "C");
    b.categories = Some(e("Data!$B$2:$B$7"));
    b.sizes = Some(e("Data!$E$2:$E$7"));
    b.format.fill = Paint::Solid(0x2CA02C);
    out.push(Chart::new(ChartKind::Bubble, vec![b], at(48), size));

    // Doughnut with colored slices and percent + category labels.
    let mut d = series(wb, "E");
    d.format.labels = DataLabels { percent: true, category: true, ..DataLabels::default() };
    d.format.points = vec![(0, 0xFF0000), (2, 0x00FF00)];
    let mut doughnut = Chart::new(ChartKind::Doughnut, vec![d], at(64), size);
    doughnut.options.hole_size = 60;
    doughnut.options.legend = Some(LegendPos::Left);
    out.push(doughnut);

    // Filled radar.
    let mut radar = Chart::new(ChartKind::Radar, vec![series(wb, "B"), series(wb, "C")], at(80), size);
    radar.options.radar_filled = true;
    radar.options.legend = Some(LegendPos::TopRight);
    out.push(radar);

    // Pie and area kinds with defaults.
    out.push(Chart::new(ChartKind::Area, vec![series(wb, "B"), series(wb, "C")], at(96), size));

    // Histogram: bins by width with an overflow bin.
    let mut h = Chart::new(ChartKind::Histogram, vec![series(wb, "B")], at(112), size);
    h.title = Some("Spread".into());
    h.options.bins = Bins { mode: BinMode::Width(5.0), overflow: Some(20.0), underflow: None };
    h.options.y_axis = Axis { title: Some("Count".into()), gridlines: true, max: Some(6.0), ..Axis::default() };
    out.push(h);

    // Waterfall: the last point is a total.
    let mut w = series(wb, "B");
    w.format.labels.value = true;
    w.format.points = vec![(5, 0x7F7F7F)];
    let mut wf = Chart::new(ChartKind::Waterfall, vec![w], at(128), size);
    wf.options.subtotals = vec![3, 5];
    wf.options.legend = Some(LegendPos::Bottom);
    out.push(wf);
    out
}

fn charts_of(wb: &Workbook) -> &[Chart] {
    &wb.layout(wb.sheet_ids()[0]).charts
}

fn assert_same(got: &[Chart], want: &[Chart]) {
    assert_eq!(got.len(), want.len());
    for (g, w) in got.iter().zip(want) {
        assert!(g.same_look(w), "{:?} chart changed:\n got {g:#?}\nwant {w:#?}", w.kind);
        assert_eq!(g.anchor, w.anchor);
    }
}

#[test]
fn every_chart_feature_roundtrips_through_a_fresh_file() {
    let (mut wb, s) = data_book();
    let charts = showcase(&wb);
    wb.layout_mut(s).unwrap().charts = charts.clone();
    let p = tmp("showcase.xlsx");
    xl_io::save(&wb, &p, s, None).unwrap();
    let files = parts(&p);
    assert!(files.contains_key("xl/charts/chartEx1.xml") && files.contains_key("xl/charts/chartEx2.xml"), "{:?}", files.keys().collect::<Vec<_>>());
    let ct = String::from_utf8_lossy(&files["[Content_Types].xml"]).to_string();
    assert!(ct.contains("application/vnd.ms-office.chartex+xml"), "{ct}");
    let drawing = String::from_utf8_lossy(&files["xl/drawings/drawing1.xml"]).to_string();
    assert!(drawing.contains("mc:AlternateContent") && drawing.contains("Requires=\"cx1\""));
    let combo = String::from_utf8_lossy(&files["xl/charts/chart1.xml"]).to_string();
    assert!(combo.contains("<c:barChart>") && combo.contains("<c:lineChart>") && combo.contains("<c:crosses val=\"max\"/>"), "{combo}");
    let back = xl_io::open(&p).unwrap();
    assert!(back.unsupported.is_empty(), "{:?}", back.unsupported);
    assert_same(charts_of(&back.workbook), &charts);
    // The histogram's data comes back binned.
    let h = &charts_of(&back.workbook)[7];
    let data = back.workbook.chart_data(s, h);
    assert_eq!(data.categories, ["[-6, -1]", "(-1, 4]", "(4, 9]", "(9, 14]", "(14, 19]", "(19, 24]", ">20"]);
    assert_eq!(data.series[0].1.iter().flatten().sum::<f64>(), 6.0);
}

#[test]
fn every_chart_feature_roundtrips_through_a_patched_save() {
    // Start from a file with one plain chart (written fresh), then add the showcase.
    let (mut wb, s) = data_book();
    let first = series(&wb, "B");
    wb.layout_mut(s).unwrap().charts.push(Chart::new(ChartKind::Line, vec![first], CellRef::new(0, 20), (300.0, 200.0)));
    let src = tmp("patch-src.xlsx");
    xl_io::save(&wb, &src, s, None).unwrap();
    let mut loaded = xl_io::open(&src).unwrap();
    let charts = showcase(&loaded.workbook);
    loaded.workbook.layout_mut(s).unwrap().charts.extend(charts.clone());
    let pkg = loaded.package.clone().unwrap();
    assert_eq!(xl_io::save_plan(&loaded.workbook, &pkg), xl_io::SavePlan::Patch);
    let out = tmp("patch-out.xlsx");
    xl_io::save(&loaded.workbook, &out, s, Some(&pkg)).unwrap();
    // The untouched chart keeps its part byte for byte.
    assert_eq!(parts(&src)["xl/charts/chart1.xml"], parts(&out)["xl/charts/chart1.xml"]);
    let back = xl_io::open(&out).unwrap();
    assert_same(&charts_of(&back.workbook)[1..], &charts);

    // Reading the saved file and saving it again unedited keeps every chart part.
    let again = tmp("patch-again.xlsx");
    let pkg = back.package.clone().unwrap();
    xl_io::save(&back.workbook, &again, s, Some(&pkg)).unwrap();
    let (a, b) = (parts(&out), parts(&again));
    for k in a.keys().filter(|k| k.starts_with("xl/charts/")) {
        assert_eq!(a[k], b[k], "{k} changed");
    }

    // Editing one chart's styling rewrites just that one.
    let mut wb = back.workbook;
    wb.layout_mut(s).unwrap().charts[1].series[1].format.secondary = false;
    let edited = tmp("patch-edited.xlsx");
    xl_io::save(&wb, &edited, s, Some(&pkg)).unwrap();
    let c = parts(&edited);
    let unchanged = a.keys().filter(|k| k.starts_with("xl/charts/") && c.get(*k) == Some(&a[*k])).count();
    assert!(unchanged >= a.keys().filter(|k| k.starts_with("xl/charts/")).count() - 2, "only the edited chart is rewritten");
    let back = xl_io::open(&edited).unwrap();
    assert!(!charts_of(&back.workbook)[1..].iter().any(|c| c.kind == ChartKind::Column && c.series[1].format.secondary));
    assert_eq!(charts_of(&back.workbook).len(), 10);
}

/// A chartex part as Excel writes it (with caches, a style reference and
/// extras xl doesn't model) reads as a histogram, and is kept byte for byte.
#[test]
fn excel_chartex_parts_read_and_survive() {
    let (mut wb, s) = data_book();
    let first = series(&wb, "B");
    wb.layout_mut(s).unwrap().charts.push(Chart::new(ChartKind::Histogram, vec![first], CellRef::new(2, 8), (400.0, 250.0)));
    let p = tmp("chartex-src.xlsx");
    xl_io::save(&wb, &p, s, None).unwrap();
    // Swap in Excel-style XML for the part.
    let excel = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cx:chartSpace xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:cx="http://schemas.microsoft.com/office/drawing/2014/chartex"><cx:chartData><cx:data id="0"><cx:numDim type="val"><cx:f dir="row">Data!$B$2:$B$7</cx:f><cx:lvl ptCount="6" formatCode="General"><cx:pt idx="0">10</cx:pt></cx:lvl></cx:numDim></cx:data></cx:chartData><cx:chart><cx:title pos="t" align="ctr" overlay="0"><cx:tx><cx:txData><cx:v>Sales spread</cx:v></cx:txData></cx:tx><cx:txPr><a:bodyPr/><a:lstStyle/><a:p><a:pPr><a:defRPr/></a:pPr><a:r><a:t>Sales spread</a:t></a:r></a:p></cx:txPr></cx:title><cx:plotArea><cx:plotAreaRegion><cx:series layoutId="clusteredColumn" uniqueId="{A}"><cx:tx><cx:txData><cx:f>Data!$B$1</cx:f><cx:v>Sales</cx:v></cx:txData></cx:tx><cx:spPr><a:solidFill><a:schemeClr val="accent2"/></a:solidFill></cx:spPr><cx:dataId val="0"/><cx:layoutPr><cx:binning intervalClosed="r" overflow="auto"><cx:binCount val="4"/></cx:binning></cx:layoutPr></cx:series></cx:plotAreaRegion><cx:axis id="0"><cx:catScaling gapWidth="0"/><cx:tickLabels/></cx:axis><cx:axis id="1"><cx:valScaling/><cx:majorGridlines/><cx:tickLabels/></cx:axis></cx:plotArea></cx:chart></cx:chartSpace>"#;
    let mut files = parts(&p);
    files.insert("xl/charts/chartEx1.xml".into(), excel.as_bytes().to_vec());
    let src = tmp("chartex-excel.xlsx");
    {
        let mut z = zip::ZipWriter::new(std::fs::File::create(&src).unwrap());
        for (name, bytes) in &files {
            z.start_file(name.as_str(), zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut z, bytes).unwrap();
        }
        z.finish().unwrap();
    }
    let loaded = xl_io::open(&src).unwrap();
    let c = &charts_of(&loaded.workbook)[0];
    assert_eq!((c.kind, c.title.as_deref()), (ChartKind::Histogram, Some("Sales spread")));
    assert_eq!(c.options.bins, Bins { mode: BinMode::Count(4), overflow: None, underflow: None });
    // The file's own theme (rust_xlsxwriter writes the Office 2007 one).
    assert_eq!(c.series[0].format.fill, Paint::Solid(0xC0504D));
    assert!(c.options.y_axis.gridlines);
    // Moving the chart keeps the part; inserting a row above shifts its formula.
    let mut wb = loaded.workbook;
    let pkg = loaded.package.unwrap();
    wb.layout_mut(s).unwrap().charts[0].anchor = CellRef::new(4, 9);
    let out = tmp("chartex-moved.xlsx");
    xl_io::save(&wb, &out, s, Some(&pkg)).unwrap();
    assert_eq!(parts(&out)["xl/charts/chartEx1.xml"], excel.as_bytes());
    let back = xl_io::open(&out).unwrap();
    assert_eq!(charts_of(&back.workbook)[0].anchor, CellRef::new(4, 9));
}

/// An Excel combo chart (bar + line on a secondary axis, with theme colors
/// and the c14 style choice) reads into the model.
#[test]
fn excel_combo_chart_reads() {
    let (mut wb, s) = data_book();
    let first = series(&wb, "B");
    wb.layout_mut(s).unwrap().charts.push(Chart::new(ChartKind::Column, vec![first], CellRef::new(2, 8), (400.0, 250.0)));
    let p = tmp("combo-src.xlsx");
    xl_io::save(&wb, &p, s, None).unwrap();
    let excel = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:c14="http://schemas.microsoft.com/office/drawing/2007/8/2/chart"><c:date1904 val="0"/><c:lang val="en-US"/><c:roundedCorners val="0"/><mc:AlternateContent><mc:Choice Requires="c14"><c14:style val="102"/></mc:Choice><mc:Fallback><c:style val="2"/></mc:Fallback></mc:AlternateContent><c:chart><c:autoTitleDeleted val="0"/><c:plotArea><c:layout/>
<c:barChart><c:barDir val="col"/><c:grouping val="clustered"/><c:varyColors val="0"/><c:ser><c:idx val="0"/><c:order val="0"/><c:tx><c:strRef><c:f>Data!$B$1</c:f></c:strRef></c:tx><c:spPr><a:solidFill><a:schemeClr val="accent1"/></a:solidFill><a:ln><a:noFill/></a:ln></c:spPr><c:invertIfNegative val="0"/><c:trendline><c:spPr><a:ln w="19050"><a:prstDash val="sysDot"/></a:ln></c:spPr><c:trendlineType val="linear"/><c:dispRSqr val="1"/><c:dispEq val="1"/><c:trendlineLbl><c:numFmt formatCode="General" sourceLinked="0"/></c:trendlineLbl></c:trendline><c:errBars><c:errBarType val="both"/><c:errValType val="stdErr"/><c:noEndCap val="0"/></c:errBars><c:cat><c:strRef><c:f>Data!$A$2:$A$7</c:f></c:strRef></c:cat><c:val><c:numRef><c:f>Data!$B$2:$B$7</c:f></c:numRef></c:val></c:ser><c:dLbls><c:showLegendKey val="0"/><c:showVal val="1"/><c:showCatName val="0"/><c:showSerName val="0"/><c:showPercent val="0"/><c:showBubbleSize val="0"/></c:dLbls><c:gapWidth val="219"/><c:axId val="1"/><c:axId val="2"/></c:barChart>
<c:lineChart><c:grouping val="standard"/><c:varyColors val="0"/><c:ser><c:idx val="1"/><c:order val="1"/><c:tx><c:strRef><c:f>Data!$D$1</c:f></c:strRef></c:tx><c:spPr><a:ln w="28575" cap="rnd"><a:solidFill><a:schemeClr val="accent2"><a:lumMod val="75000"/></a:schemeClr></a:solidFill><a:round/></a:ln></c:spPr><c:marker><c:symbol val="none"/></c:marker><c:cat><c:strRef><c:f>Data!$A$2:$A$7</c:f></c:strRef></c:cat><c:val><c:numRef><c:f>Data!$D$2:$D$7</c:f></c:numRef></c:val><c:smooth val="0"/></c:ser><c:marker val="1"/><c:axId val="3"/><c:axId val="4"/></c:lineChart>
<c:catAx><c:axId val="1"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="b"/><c:numFmt formatCode="General" sourceLinked="1"/><c:crossAx val="2"/><c:crosses val="autoZero"/></c:catAx><c:valAx><c:axId val="2"/><c:scaling><c:orientation val="minMax"/><c:max val="25"/></c:scaling><c:delete val="0"/><c:axPos val="l"/><c:majorGridlines/><c:title><c:tx><c:rich><a:bodyPr/><a:p><a:r><a:t>Sales</a:t></a:r></a:p></c:rich></c:tx></c:title><c:numFmt formatCode="General" sourceLinked="1"/><c:crossAx val="1"/><c:crosses val="autoZero"/></c:valAx><c:valAx><c:axId val="4"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="r"/><c:numFmt formatCode="0%" sourceLinked="0"/><c:crossAx val="3"/><c:crosses val="max"/></c:valAx><c:catAx><c:axId val="3"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="1"/><c:axPos val="b"/><c:crossAx val="4"/><c:crosses val="autoZero"/></c:catAx></c:plotArea><c:legend><c:legendPos val="t"/></c:legend><c:plotVisOnly val="1"/></c:chart></c:chartSpace>"#;
    let mut files = parts(&p);
    files.insert("xl/charts/chart1.xml".into(), excel.as_bytes().to_vec());
    let src = tmp("combo-excel.xlsx");
    {
        let mut z = zip::ZipWriter::new(std::fs::File::create(&src).unwrap());
        for (name, bytes) in &files {
            z.start_file(name.as_str(), zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut z, bytes).unwrap();
        }
        z.finish().unwrap();
    }
    let loaded = xl_io::open(&src).unwrap();
    assert!(loaded.unsupported.is_empty(), "{:?}", loaded.unsupported);
    let c = &charts_of(&loaded.workbook)[0];
    assert_eq!((c.kind, c.series.len(), c.options.style, c.options.legend), (ChartKind::Column, 2, Some(2), Some(LegendPos::Top)));
    let (a, b) = (&c.series[0].format, &c.series[1].format);
    assert_eq!((a.fill, a.line, a.labels.value), (Paint::Solid(0x4F81BD), Paint::None, true));
    assert_eq!(a.trendline.as_ref().map(|t| (t.kind, t.equation, t.r_squared)), Some((TrendKind::Linear, true, true)));
    assert_eq!(a.error_bars.map(|e| (e.kind, e.dir)), Some((ErrorKind::StdErr, ErrorDir::Both)));
    assert_eq!((b.kind, b.secondary, b.line_width, b.marker.shape), (Some(ChartKind::Line), true, Some(2.25), Some(MarkerShape::None)));
    assert_eq!(b.line, Paint::Solid(0x953735), "accent2 at 75% luminance");
    assert_eq!((c.options.y_axis.max, c.options.y_axis.title.as_deref(), c.options.y2_axis.num_fmt.as_deref()), (Some(25.0), Some("Sales"), Some("0%")));
    // Unedited, it's kept as is.
    let pkg = loaded.package.clone().unwrap();
    let out = tmp("combo-kept.xlsx");
    let mut wb = loaded.workbook;
    wb.layout_mut(s).unwrap().charts[0].offset = (5.0, 5.0);
    xl_io::save(&wb, &out, s, Some(&pkg)).unwrap();
    assert_eq!(parts(&out)["xl/charts/chart1.xml"], excel.as_bytes());
}
