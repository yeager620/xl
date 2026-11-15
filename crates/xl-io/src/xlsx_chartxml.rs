//! Writing chart and drawing XML for charts made or edited in xl, to patch
//! into an opened package (fresh files use rust_xlsxwriter instead).

use xl_engine::chart::{Chart, ChartKind};
use xl_engine::formula::Expr;
use xl_engine::{SheetId, Workbook, quote_sheet_name};

use crate::xlsx_patch::esc;

/// "Sheet1!$A$2:$A$9" for a reference (sheet always named, as charts need).
fn chart_ref(wb: &Workbook, home: SheetId, e: &Expr) -> Option<String> {
    let (sheet, r) = e.reference()?;
    let name = quote_sheet_name(wb.sheet_name(sheet.unwrap_or(home)));
    let abs = |c: xl_engine::CellRef| format!("${}${}", xl_engine::col_name(c.col), c.row + 1);
    Some(if r.start == r.end { format!("{name}!{}", abs(r.start)) } else { format!("{name}!{}:{}", abs(r.start), abs(r.end)) })
}

/// A chart part (`xl/charts/chartN.xml`).
pub(crate) fn chart_xml(wb: &Workbook, home: SheetId, chart: &Chart) -> String {
    let stacked = chart.stacked && matches!(chart.kind, ChartKind::Column | ChartKind::Bar | ChartKind::Area);
    let mut series = String::new();
    for (i, s) in chart.series.iter().enumerate() {
        let Some(values) = chart_ref(wb, home, &s.values) else { continue };
        let name = match s.name.as_deref() {
            Some(Expr::Text(t)) => format!("<c:tx><c:v>{}</c:v></c:tx>", esc(t)),
            Some(e) => chart_ref(wb, home, e).map_or(String::new(), |r| format!("<c:tx><c:strRef><c:f>{}</c:f></c:strRef></c:tx>", esc(&r))),
            None => String::new(),
        };
        let cats = s.categories.as_deref().and_then(|e| chart_ref(wb, home, e));
        let body = if chart.kind == ChartKind::Scatter {
            let x = cats.map_or(String::new(), |r| format!("<c:xVal><c:numRef><c:f>{}</c:f></c:numRef></c:xVal>", esc(&r)));
            format!("<c:spPr><a:ln w=\"19050\"><a:noFill/></a:ln></c:spPr>{x}<c:yVal><c:numRef><c:f>{}</c:f></c:numRef></c:yVal><c:smooth val=\"0\"/>", esc(&values))
        } else {
            let cat = cats.map_or(String::new(), |r| format!("<c:cat><c:strRef><c:f>{}</c:f></c:strRef></c:cat>", esc(&r)));
            let marker = if chart.kind == ChartKind::Line { "<c:marker><c:symbol val=\"circle\"/></c:marker>" } else { "" };
            format!("{marker}{cat}<c:val><c:numRef><c:f>{}</c:f></c:numRef></c:val>{}", esc(&values), if chart.kind == ChartKind::Line { "<c:smooth val=\"0\"/>" } else { "" })
        };
        series.push_str(&format!("<c:ser><c:idx val=\"{i}\"/><c:order val=\"{i}\"/>{name}{body}</c:ser>"));
    }
    let axes_ids = "<c:axId val=\"50010001\"/><c:axId val=\"50010002\"/>";
    let plot = match chart.kind {
        ChartKind::Column | ChartKind::Bar => format!(
            "<c:barChart><c:barDir val=\"{}\"/><c:grouping val=\"{}\"/><c:varyColors val=\"0\"/>{series}{}{axes_ids}</c:barChart>",
            if chart.kind == ChartKind::Bar { "bar" } else { "col" },
            if stacked { "stacked" } else { "clustered" },
            if stacked { "<c:overlap val=\"100\"/>" } else { "" }
        ),
        ChartKind::Line => format!("<c:lineChart><c:grouping val=\"standard\"/><c:varyColors val=\"0\"/>{series}<c:marker val=\"1\"/>{axes_ids}</c:lineChart>"),
        ChartKind::Area => format!("<c:areaChart><c:grouping val=\"{}\"/><c:varyColors val=\"0\"/>{series}{axes_ids}</c:areaChart>", if stacked { "stacked" } else { "standard" }),
        ChartKind::Pie => format!("<c:pieChart><c:varyColors val=\"1\"/>{series}<c:firstSliceAng val=\"0\"/></c:pieChart>"),
        ChartKind::Scatter => format!("<c:scatterChart><c:scatterStyle val=\"lineMarker\"/><c:varyColors val=\"0\"/>{series}{axes_ids}</c:scatterChart>"),
    };
    let axes = match chart.kind {
        ChartKind::Pie => String::new(),
        ChartKind::Scatter => "<c:valAx><c:axId val=\"50010001\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:delete val=\"0\"/><c:axPos val=\"b\"/><c:numFmt formatCode=\"General\" sourceLinked=\"1\"/><c:tickLblPos val=\"nextTo\"/><c:crossAx val=\"50010002\"/><c:crosses val=\"autoZero\"/><c:crossBetween val=\"midCat\"/></c:valAx><c:valAx><c:axId val=\"50010002\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:delete val=\"0\"/><c:axPos val=\"l\"/><c:majorGridlines/><c:numFmt formatCode=\"General\" sourceLinked=\"1\"/><c:tickLblPos val=\"nextTo\"/><c:crossAx val=\"50010001\"/><c:crosses val=\"autoZero\"/><c:crossBetween val=\"midCat\"/></c:valAx>".to_string(),
        _ => {
            let (cat_pos, val_pos) = if chart.kind == ChartKind::Bar { ("l", "b") } else { ("b", "l") };
            format!(
                "<c:catAx><c:axId val=\"50010001\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:delete val=\"0\"/><c:axPos val=\"{cat_pos}\"/><c:numFmt formatCode=\"General\" sourceLinked=\"1\"/><c:tickLblPos val=\"nextTo\"/><c:crossAx val=\"50010002\"/><c:crosses val=\"autoZero\"/><c:auto val=\"1\"/><c:lblAlgn val=\"ctr\"/><c:lblOffset val=\"100\"/></c:catAx><c:valAx><c:axId val=\"50010002\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:delete val=\"0\"/><c:axPos val=\"{val_pos}\"/><c:majorGridlines/><c:numFmt formatCode=\"General\" sourceLinked=\"1\"/><c:tickLblPos val=\"nextTo\"/><c:crossAx val=\"50010001\"/><c:crosses val=\"autoZero\"/><c:crossBetween val=\"between\"/></c:valAx>"
            )
        }
    };
    let title = chart.title.as_ref().map_or(String::new(), |t| {
        format!("<c:title><c:tx><c:rich><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>{}</a:t></a:r></a:p></c:rich></c:tx><c:overlay val=\"0\"/></c:title>", esc(t))
    });
    let legend = if chart.series.len() > 1 || chart.kind == ChartKind::Pie { "<c:legend><c:legendPos val=\"b\"/><c:overlay val=\"0\"/></c:legend>" } else { "" };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<c:chartSpace xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><c:roundedCorners val=\"0\"/><c:chart>{title}<c:autoTitleDeleted val=\"{}\"/><c:plotArea><c:layout/>{plot}{axes}</c:plotArea>{legend}<c:plotVisOnly val=\"1\"/><c:dispBlanksAs val=\"gap\"/></c:chart></c:chartSpace>",
        if chart.title.is_none() && chart.series.len() > 1 { 1 } else { 0 }
    )
}

/// A two-cell anchor holding a chart frame (to be placed by `reanchor`).
pub(crate) fn chart_anchor(rid: &str, id: u32, name: &str) -> String {
    format!(
        "<xdr:twoCellAnchor editAs=\"oneCell\"><xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from><xdr:to><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>1</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to><xdr:graphicFrame macro=\"\"><xdr:nvGraphicFramePr><xdr:cNvPr id=\"{id}\" name=\"{}\"/><xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr><xdr:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/></xdr:xfrm><a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/chart\"><c:chart xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\" r:id=\"{rid}\"/></a:graphicData></a:graphic></xdr:graphicFrame><xdr:clientData/></xdr:twoCellAnchor>",
        esc(name)
    )
}

/// An empty drawing part.
pub(crate) const EMPTY_DRAWING: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<xdr:wsDr xmlns:xdr=\"http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"></xdr:wsDr>";
