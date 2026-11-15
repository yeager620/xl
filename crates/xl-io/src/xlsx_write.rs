//! xlsx writer built on rust_xlsxwriter: sheets, values, formulas with cached
//! results, number formats and column widths.

use std::collections::HashMap;
use std::path::Path;

use rust_xlsxwriter::{
    Color, ConditionalFormat2ColorScale, ConditionalFormat3ColorScale, ConditionalFormatAverage, ConditionalFormatAverageRule, ConditionalFormatBlank,
    ConditionalFormatCell, ConditionalFormatCellRule, ConditionalFormatDataBar, ConditionalFormatDuplicate, ConditionalFormatFormula, ConditionalFormatText,
    ConditionalFormatTextRule, ConditionalFormatTop, ConditionalFormatTopRule, ConditionalFormatType, FilterCondition, Format, FormatAlign, FormatBorder,
    FormatUnderline, Formula, Workbook as XlsxBook, Worksheet,
};
use xl_engine::cond::{CfOp, CfRule, CfStyle, CfValue, CondFormat, TextMatch};
use xl_engine::{Addr, Border, CellInput, CellRef, HAlign, LineStyle, Style, VAlign, Value, Workbook, precise};

use crate::{to_excel_height, to_excel_width_px};

/// Functions newer than Excel 2010 must carry the `_xlfn.` prefix in the
/// file or Excel shows #NAME?.
const FUTURE_FUNCTIONS: &[&str] = &[
    "CONCAT", "IFS", "IFNA", "XLOOKUP", "XMATCH", "TEXTJOIN", "SWITCH", "MAXIFS", "MINIFS", "TEXTBEFORE", "TEXTAFTER", "DAYS", "STDEV.S", "STDEV.P", "VAR.S",
    "VAR.P", "PERCENTILE.INC", "QUARTILE.INC", "RANK.EQ", "MODE.SNGL",
];

/// Files spell "this row" references in full: `[@Qty]` inside table T is
/// written `T[[#This Row],[Qty]]`. String literals are left alone.
pub fn expand_this_row(src: &str, table: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len() + 16);
    let mut i = 0;
    let mut quoted = false;
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            quoted = !quoted;
        }
        if !quoted && c == b'[' && b.get(i + 1) == Some(&b'@') {
            // Find the end of this bracket block, honouring ' escapes.
            let (mut depth, mut j) = (0, i);
            while j < b.len() {
                match b[j] {
                    b'\'' => j += 1,
                    b'[' => depth += 1,
                    b']' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            let inner = &src[i + 2..j.min(src.len())];
            let col = inner.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(inner);
            // A table name right before the bracket stays; otherwise use ours.
            let named = out.chars().last().is_some_and(|ch| ch.is_alphanumeric() || ch == '_' || ch == '.');
            if !named {
                out.push_str(table);
            }
            out.push_str(&format!("[[#This Row],[{col}]]"));
            i = j + 1;
            continue;
        }
        let ch = src[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Add `_xlfn.` to future functions, skipping string literals.
pub fn prefix_future_functions(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len() + 8);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            let start = i;
            i += 1;
            while i < b.len() {
                if b[i] == b'"' {
                    if b.get(i + 1) == Some(&b'"') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            out.push_str(&src[start..i]);
            continue;
        }
        if c.is_ascii_alphabetic() && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || matches!(b[i - 1], b'_' | b'.'))) {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'_' | b'.')) {
                i += 1;
            }
            let word = &src[start..i];
            if b.get(i) == Some(&b'(') && FUTURE_FUNCTIONS.iter().any(|f| f.eq_ignore_ascii_case(word)) {
                out.push_str("_xlfn.");
            }
            out.push_str(word);
            continue;
        }
        let ch = src[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn border_kind(b: &Border) -> FormatBorder {
    match b.style {
        LineStyle::None => FormatBorder::None,
        LineStyle::Thin => FormatBorder::Thin,
        LineStyle::Medium => FormatBorder::Medium,
        LineStyle::Thick => FormatBorder::Thick,
        LineStyle::Dashed => FormatBorder::Dashed,
        LineStyle::Dotted => FormatBorder::Dotted,
        LineStyle::Double => FormatBorder::Double,
    }
}

fn build_format(code: Option<&str>, st: &Style) -> Format {
    let mut f = Format::new();
    if let Some(c) = code {
        f = f.set_num_format(c);
    }
    if st.bold {
        f = f.set_bold();
    }
    if st.italic {
        f = f.set_italic();
    }
    if st.underline {
        f = f.set_underline(FormatUnderline::Single);
    }
    if st.strike {
        f = f.set_font_strikethrough();
    }
    if let Some(c) = st.color {
        f = f.set_font_color(Color::RGB(c));
    }
    if let Some(c) = st.fill {
        f = f.set_background_color(Color::RGB(c));
    }
    f = match st.align {
        HAlign::General => f,
        HAlign::Left => f.set_align(FormatAlign::Left),
        HAlign::Center => f.set_align(FormatAlign::Center),
        HAlign::Right => f.set_align(FormatAlign::Right),
    };
    f = match st.valign {
        VAlign::Bottom => f,
        VAlign::Center => f.set_align(FormatAlign::VerticalCenter),
        VAlign::Top => f.set_align(FormatAlign::Top),
    };
    if st.wrap {
        f = f.set_text_wrap();
    }
    if let Some(t) = st.font_size {
        f = f.set_font_size(t as f64 / 10.0);
    }
    if let Some(name) = &st.font_name {
        f = f.set_font_name(name.to_string());
    }
    if !st.top.is_none() {
        f = f.set_border_top(border_kind(&st.top));
        if let Some(c) = st.top.color {
            f = f.set_border_top_color(Color::RGB(c));
        }
    }
    if !st.bottom.is_none() {
        f = f.set_border_bottom(border_kind(&st.bottom));
        if let Some(c) = st.bottom.color {
            f = f.set_border_bottom_color(Color::RGB(c));
        }
    }
    if !st.left.is_none() {
        f = f.set_border_left(border_kind(&st.left));
        if let Some(c) = st.left.color {
            f = f.set_border_left_color(Color::RGB(c));
        }
    }
    if !st.right.is_none() {
        f = f.set_border_right(border_kind(&st.right));
        if let Some(c) = st.right.color {
            f = f.set_border_right_color(Color::RGB(c));
        }
    }
    f
}

fn dxf_format(st: &CfStyle) -> Format {
    let mut f = Format::new();
    if let Some(c) = st.fill {
        f = f.set_background_color(Color::RGB(c));
    }
    if let Some(c) = st.color {
        f = f.set_font_color(Color::RGB(c));
    }
    if st.bold {
        f = f.set_bold();
    }
    if st.italic {
        f = f.set_italic();
    }
    f
}

fn cf_point(v: CfValue) -> (ConditionalFormatType, f64) {
    match v {
        CfValue::Min => (ConditionalFormatType::Lowest, 0.0),
        CfValue::Max => (ConditionalFormatType::Highest, 0.0),
        CfValue::Number(x) => (ConditionalFormatType::Number, x),
        CfValue::Percent(p) => (ConditionalFormatType::Percent, p),
        CfValue::Percentile(p) => (ConditionalFormatType::Percentile, p),
    }
}

/// Write one conditional format (rules go in priority order).
fn write_cond(ws: &mut Worksheet, wb: &Workbook, cf: &CondFormat) -> Result<(), rust_xlsxwriter::XlsxError> {
    let Some(first) = cf.ranges.first() else { return Ok(()) };
    let multi = cf.ranges.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(" ");
    let (r0, c0, r1, c1) = (first.start.row, first.start.col as u16, first.end.row, first.end.col as u16);
    let src = |e: &xl_engine::formula::Expr| Formula::new(prefix_future_functions(&e.to_source(0, 0, wb.sheets())));
    macro_rules! add {
        ($cf:expr) => {
            ws.add_conditional_format(r0, c0, r1, c1, &$cf.set_multi_range(multi.as_str()).set_stop_if_true(cf.stop_if_true))?
        };
    }
    match &cf.rule {
        CfRule::Cell { op, a, b, style } => {
            let (a, b) = (src(a), b.as_ref().map(|b| src(b)).unwrap_or_else(|| Formula::new("0")));
            let rule = match op {
                CfOp::Between => ConditionalFormatCellRule::Between(a, b),
                CfOp::NotBetween => ConditionalFormatCellRule::NotBetween(a, b),
                CfOp::Equal => ConditionalFormatCellRule::EqualTo(a),
                CfOp::NotEqual => ConditionalFormatCellRule::NotEqualTo(a),
                CfOp::Greater => ConditionalFormatCellRule::GreaterThan(a),
                CfOp::Less => ConditionalFormatCellRule::LessThan(a),
                CfOp::GreaterEq => ConditionalFormatCellRule::GreaterThanOrEqualTo(a),
                CfOp::LessEq => ConditionalFormatCellRule::LessThanOrEqualTo(a),
            };
            add!(ConditionalFormatCell::new().set_rule(rule).set_format(dxf_format(style)))
        }
        CfRule::Text { kind, text, style } => {
            let rule = match kind {
                TextMatch::Contains => ConditionalFormatTextRule::Contains(text.clone()),
                TextMatch::NotContains => ConditionalFormatTextRule::DoesNotContain(text.clone()),
                TextMatch::BeginsWith => ConditionalFormatTextRule::BeginsWith(text.clone()),
                TextMatch::EndsWith => ConditionalFormatTextRule::EndsWith(text.clone()),
            };
            add!(ConditionalFormatText::new().set_rule(rule).set_format(dxf_format(style)))
        }
        CfRule::Top { n, percent, bottom, style } => {
            let n = (*n).min(u16::MAX as u32) as u16;
            let rule = match (percent, bottom) {
                (false, false) => ConditionalFormatTopRule::Top(n),
                (false, true) => ConditionalFormatTopRule::Bottom(n),
                (true, false) => ConditionalFormatTopRule::TopPercent(n),
                (true, true) => ConditionalFormatTopRule::BottomPercent(n),
            };
            add!(ConditionalFormatTop::new().set_rule(rule).set_format(dxf_format(style)))
        }
        CfRule::Average { above, style } => {
            let rule = if *above { ConditionalFormatAverageRule::AboveAverage } else { ConditionalFormatAverageRule::BelowAverage };
            add!(ConditionalFormatAverage::new().set_rule(rule).set_format(dxf_format(style)))
        }
        CfRule::Duplicates { unique, style } => {
            let d = ConditionalFormatDuplicate::new().set_format(dxf_format(style));
            add!(if *unique { d.invert() } else { d })
        }
        CfRule::Blanks { blanks, style } => {
            let d = ConditionalFormatBlank::new().set_format(dxf_format(style));
            add!(if *blanks { d } else { d.invert() })
        }
        CfRule::Formula { expr, style } => add!(ConditionalFormatFormula::new().set_rule(src(expr)).set_format(dxf_format(style))),
        CfRule::ColorScale { stops } => match stops.as_slice() {
            [(lo, lc), (hi, hc)] => {
                let ((lt, lv), (ht, hv)) = (cf_point(*lo), cf_point(*hi));
                add!(ConditionalFormat2ColorScale::new().set_minimum(lt, lv).set_maximum(ht, hv).set_minimum_color(Color::RGB(*lc)).set_maximum_color(Color::RGB(*hc)))
            }
            [(lo, lc), (mid, mc), (hi, hc)] => {
                let ((lt, lv), (mt, mv), (ht, hv)) = (cf_point(*lo), cf_point(*mid), cf_point(*hi));
                add!(ConditionalFormat3ColorScale::new()
                    .set_minimum(lt, lv)
                    .set_midpoint(mt, mv)
                    .set_maximum(ht, hv)
                    .set_minimum_color(Color::RGB(*lc))
                    .set_midpoint_color(Color::RGB(*mc))
                    .set_maximum_color(Color::RGB(*hc)))
            }
            _ => ws,
        },
        CfRule::DataBar { color, min, max } => {
            let ((lt, lv), (ht, hv)) = (cf_point(*min), cf_point(*max));
            add!(ConditionalFormatDataBar::new().set_minimum(lt, lv).set_maximum(ht, hv).set_fill_color(Color::RGB(*color)).set_solid_fill(true).set_border_off(true))
        }
    };
    Ok(())
}

/// Our pixels to Excel's (default column 64px, row 20px).
fn excel_px_x(px: f64) -> u32 {
    crate::to_excel_width_px(px)
}

fn excel_px_y(px: f64) -> u32 {
    (crate::to_excel_height(px) * 4.0 / 3.0).round() as u32
}

/// A picture at its place and size as read. Images rust_xlsxwriter can't
/// embed are left out.
fn write_picture(ws: &mut Worksheet, p: &xl_engine::picture::Picture) -> Result<(), rust_xlsxwriter::XlsxError> {
    let Ok(mut image) = rust_xlsxwriter::Image::new_from_buffer(&p.data) else { return Ok(()) };
    image = image.set_scale_to_size(f64::from(excel_px_x(p.size.0)), f64::from(excel_px_y(p.size.1)), false);
    if let Some(alt) = &p.alt {
        image = image.set_alt_text(alt);
    }
    // No offset stays none (the conversions round up to a pixel).
    let off = |v: f64, f: fn(f64) -> u32| if v > 0.0 { f(v) } else { 0 };
    ws.insert_image_with_offset(p.anchor.row, p.anchor.col as u16, &image, off(p.offset.0, excel_px_x), off(p.offset.1, excel_px_y))?;
    Ok(())
}

fn write_chart(ws: &mut Worksheet, wb: &Workbook, home: xl_engine::SheetId, chart: &xl_engine::chart::Chart) -> Result<(), rust_xlsxwriter::XlsxError> {
    use rust_xlsxwriter::{Chart as XChart, ChartType};
    use xl_engine::chart::ChartKind;
    let ty = match (chart.kind, chart.stacked) {
        (ChartKind::Column, false) => ChartType::Column,
        (ChartKind::Column, true) => ChartType::ColumnStacked,
        (ChartKind::Bar, false) => ChartType::Bar,
        (ChartKind::Bar, true) => ChartType::BarStacked,
        (ChartKind::Line, _) => ChartType::Line,
        (ChartKind::Area, false) => ChartType::Area,
        (ChartKind::Area, true) => ChartType::AreaStacked,
        (ChartKind::Pie, _) => ChartType::Pie,
        (ChartKind::Scatter, _) => ChartType::Scatter,
    };
    let mut c = XChart::new(ty);
    // A reference as (sheet name, rows, columns).
    let span = |e: &xl_engine::formula::Expr| -> Option<(String, u32, u16, u32, u16)> {
        let (sheet, r) = e.reference()?;
        Some((wb.sheet_name(sheet.unwrap_or(home)).to_string(), r.start.row, r.start.col as u16, r.end.row, r.end.col as u16))
    };
    for s in &chart.series {
        let Some(values) = span(&s.values) else { continue };
        let ser = c.add_series();
        ser.set_values((values.0.as_str(), values.1, values.2, values.3, values.4));
        if let Some(cats) = s.categories.as_deref().and_then(span) {
            ser.set_categories((cats.0.as_str(), cats.1, cats.2, cats.3, cats.4));
        }
        if let Some(e) = s.name.as_deref() {
            match (span(e), e) {
                (Some(n), _) => {
                    ser.set_name((n.0.as_str(), n.1, n.2));
                }
                (None, xl_engine::formula::Expr::Text(t)) => {
                    ser.set_name(t.as_ref());
                }
                _ => {}
            }
        }
    }
    if let Some(title) = &chart.title {
        c.title().set_name(title);
    }
    c.set_width(excel_px_x(chart.size.0));
    c.set_height(excel_px_y(chart.size.1));
    ws.insert_chart_with_offset(chart.anchor.row, chart.anchor.col as u16, &c, excel_px_x(chart.offset.0), excel_px_y(chart.offset.1))?;
    Ok(())
}

/// Runs of consecutive lines at each outline level, outer levels first:
/// grouping each run once per level reproduces the levels.
fn outline_runs(levels: &std::collections::BTreeMap<u32, u8>) -> Vec<(u32, u32)> {
    let deepest = levels.values().copied().max().unwrap_or(0);
    let mut out = Vec::new();
    for level in 1..=deepest {
        let mut run: Option<(u32, u32)> = None;
        for (&i, _) in levels.iter().filter(|(_, l)| **l >= level) {
            run = match run {
                Some((a, b)) if i == b + 1 => Some((a, i)),
                Some(r) => {
                    out.push(r);
                    Some((i, i))
                }
                None => Some((i, i)),
            };
        }
        out.extend(run);
    }
    out
}

fn write_validation(ws: &mut Worksheet, wb: &Workbook, v: &xl_engine::validation::Validation) -> Result<(), rust_xlsxwriter::XlsxError> {
    use rust_xlsxwriter::{DataValidation, DataValidationErrorStyle, DataValidationRule as R};
    use xl_engine::validation::{ErrorStyle, ListSource, ValidationKind, ValidationOp, ValidationRule};
    let Some(first) = v.ranges.first() else { return Ok(()) };
    let f = |e: &xl_engine::formula::Expr| Formula::new(prefix_future_functions(&e.to_source(0, 0, wb.sheets())));
    let mut dv = DataValidation::new();
    dv = match &v.rule {
        ValidationRule::List(ListSource::Items(items)) => dv.allow_list_strings(&items.iter().map(String::as_str).collect::<Vec<_>>())?,
        ValidationRule::List(ListSource::Cells(e)) => dv.allow_list_formula(f(e)),
        ValidationRule::Custom(e) => dv.allow_custom(f(e)),
        ValidationRule::Compare { kind, op, a, b } => {
            let (a, b) = (f(a), b.as_deref().map(f).unwrap_or_else(|| Formula::new("0")));
            let rule = match op {
                ValidationOp::Between => R::Between(a, b),
                ValidationOp::NotBetween => R::NotBetween(a, b),
                ValidationOp::Equal => R::EqualTo(a),
                ValidationOp::NotEqual => R::NotEqualTo(a),
                ValidationOp::Greater => R::GreaterThan(a),
                ValidationOp::Less => R::LessThan(a),
                ValidationOp::GreaterEq => R::GreaterThanOrEqualTo(a),
                ValidationOp::LessEq => R::LessThanOrEqualTo(a),
            };
            match kind {
                ValidationKind::Whole => dv.allow_whole_number_formula(rule),
                ValidationKind::Decimal => dv.allow_decimal_number_formula(rule),
                ValidationKind::Date => dv.allow_date_formula(rule),
                ValidationKind::Time => dv.allow_time_formula(rule),
                ValidationKind::TextLength => dv.allow_text_length_formula(rule),
            }
        }
    };
    dv = dv.ignore_blank(v.allow_blank).show_dropdown(v.dropdown).set_error_style(match v.error_style {
        ErrorStyle::Stop => DataValidationErrorStyle::Stop,
        ErrorStyle::Warning => DataValidationErrorStyle::Warning,
        ErrorStyle::Information => DataValidationErrorStyle::Information,
    });
    if let Some((t, b)) = &v.prompt {
        dv = dv.set_input_title(t)?.set_input_message(b)?;
    }
    if let Some((t, b)) = &v.error {
        dv = dv.set_error_title(t)?.set_error_message(b)?;
    }
    if v.ranges.len() > 1 {
        dv = dv.set_multi_range(v.ranges.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(" "));
    }
    ws.add_data_validation(first.start.row, first.start.col as u16, first.end.row, first.end.col as u16, &dv)?;
    Ok(())
}

fn result_text(v: &Value) -> String {
    match v {
        Value::Number(n) => precise(*n),
        Value::Bool(true) => "TRUE".into(),
        Value::Bool(false) => "FALSE".into(),
        Value::Empty => String::new(),
        other => other.display(),
    }
}

pub fn write_xlsx(wb: &Workbook, path: &Path) -> Result<(), String> {
    let err = |e: rust_xlsxwriter::XlsxError| e.to_string();
    let mut book = XlsxBook::new();
    let mut formats: HashMap<(Option<String>, Style), Format> = HashMap::new();
    let mut fmt = |code: Option<&str>, st: &Style| -> Option<Format> {
        if code.is_none() && st.is_default() {
            return None;
        }
        Some(formats.entry((code.map(str::to_string), st.clone())).or_insert_with(|| build_format(code, st)).clone())
    };

    for &id in wb.sheet_ids() {
        let ws = book.add_worksheet();
        ws.set_name(wb.sheet_name(id)).map_err(err)?;
        let layout = wb.layout(id);
        for (&col, &px) in wb.col_widths(id) {
            ws.set_column_width_pixels(col as u16, to_excel_width_px(px)).map_err(err)?;
        }
        for (&row, &px) in &layout.row_heights {
            ws.set_row_height(row, to_excel_height(px)).map_err(err)?;
        }
        for &row in layout.hidden_rows.iter().chain(&layout.filtered_rows) {
            ws.set_row_hidden(row).map_err(err)?;
        }
        for t in &layout.tables {
            use rust_xlsxwriter::{Table as XTable, TableColumn, TableStyle as XStyle};
            let r = t.range;
            let columns: Vec<TableColumn> = (r.start.col..=r.end.col).map(|c| TableColumn::new().set_header(wb.display(Addr::new(id, CellRef::new(r.start.row, c))).text)).collect();
            let style = match t.style {
                xl_engine::TableStyle::Blue => XStyle::Medium2,
                xl_engine::TableStyle::Orange => XStyle::Medium3,
                xl_engine::TableStyle::Gray => XStyle::Medium4,
                xl_engine::TableStyle::Green => XStyle::Medium7,
            };
            let table = XTable::new().set_name(&t.name).set_columns(&columns).set_style(style).set_banded_rows(t.banded).set_total_row(t.totals);
            ws.add_table(r.start.row, r.start.col as u16, r.end.row, r.end.col as u16, &table).map_err(err)?;
        }
        // A table carries its own filter buttons; a sheet filter on the same cells would clash.
        if let Some(f) = layout.filter.as_ref().filter(|f| !layout.tables.iter().any(|t| t.range == f.range)) {
            let r = f.range;
            ws.autofilter(r.start.row, r.start.col as u16, r.end.row, r.end.col as u16).map_err(err)?;
            // We hide the filtered rows ourselves (above), matching xl's view.
            ws.filter_automatic_off();
            for (&col, allowed) in &f.columns {
                if allowed.is_empty() {
                    continue;
                }
                let mut cond = FilterCondition::new();
                for v in allowed {
                    cond = if v.is_empty() { cond.add_list_blanks_filter() } else { cond.add_list_filter(v.as_str()) };
                }
                ws.filter_column(col as u16, &cond).map_err(err)?;
            }
        }
        for &col in &layout.hidden_cols {
            ws.set_column_hidden(col as u16).map_err(err)?;
        }
        for (a, b) in outline_runs(&layout.row_levels) {
            ws.group_rows(a, b).map_err(err)?;
        }
        for (a, b) in outline_runs(&layout.col_levels) {
            ws.group_columns(a as u16, b as u16).map_err(err)?;
        }
        if layout.freeze != (0, 0) {
            ws.set_freeze_panes(layout.freeze.0, layout.freeze.1 as u16).map_err(err)?;
        }
        for cf in &layout.cond {
            write_cond(ws, wb, cf).map_err(err)?;
        }
        for v in &layout.validations {
            write_validation(ws, wb, v).map_err(err)?;
        }
        for (cell, link) in &layout.links {
            let a = Addr::new(id, *cell);
            let url = match link {
                xl_engine::Link::Url { url, .. } => rust_xlsxwriter::Url::new(url.as_str()),
                xl_engine::Link::Place { location, .. } => rust_xlsxwriter::Url::new(format!("internal:{location}")),
            };
            let mut url = url.set_text(wb.display(a).text);
            if let Some(t) = link.tooltip() {
                url = url.set_tip(t);
            }
            match fmt(wb.format_code(a), wb.style(a)) {
                Some(f) => ws.write_url_with_format(cell.row, cell.col as u16, url, &f),
                None => ws.write_url(cell.row, cell.col as u16, url),
            }
            .map_err(err)?;
        }
        for (cell, note) in &layout.notes {
            let n = rust_xlsxwriter::Note::new(&note.text).set_author(&note.author).add_author_prefix(false);
            ws.insert_note(cell.row, cell.col as u16, &n).map_err(err)?;
        }
        for p in &layout.pictures {
            write_picture(ws, p).map_err(err)?;
        }
        for chart in &layout.charts {
            write_chart(ws, wb, id, chart).map_err(err)?;
        }
        for sp in &layout.sparklines {
            use rust_xlsxwriter::{Sparkline, SparklineType};
            use xl_engine::chart::SparkKind;
            let Some((sheet, r)) = sp.data.reference() else { continue };
            let name = wb.sheet_name(sheet.unwrap_or(id)).to_string();
            let ty = match sp.kind {
                SparkKind::Line => SparklineType::Line,
                SparkKind::Column => SparklineType::Column,
                SparkKind::WinLoss => SparklineType::WinLose,
            };
            let mut line = Sparkline::new().set_range((name.as_str(), r.start.row, r.start.col as u16, r.end.row, r.end.col as u16)).set_type(ty);
            if let Some(c) = sp.color {
                line = line.set_sparkline_color(Color::RGB(c));
            }
            ws.add_sparkline(sp.cell.row, sp.cell.col as u16, &line).map_err(err)?;
        }
        // Merges first: the top-left cell's real contents are written below.
        for m in &layout.merges {
            let a = Addr::new(id, m.start);
            let f = fmt(wb.format_code(a), wb.style(a)).unwrap_or_default();
            ws.merge_range(m.start.row, m.start.col as u16, m.end.row, m.end.col as u16, "", &f).map_err(err)?;
        }
        // Empty cells that carry a format or style keep it.
        let mut blanks: Vec<CellRef> = wb.formatted_cells(id).map(|(c, _)| c).chain(wb.styled_cells(id).map(|(c, _)| c)).collect();
        blanks.sort_unstable();
        blanks.dedup();
        for cell in blanks {
            let a = Addr::new(id, cell);
            if wb.cell(a).is_none()
                && let Some(f) = fmt(wb.format_code(a), wb.style(a))
            {
                ws.write_blank(cell.row, cell.col as u16, &f).map_err(err)?;
            }
        }
        for (cell, c) in wb.store(id).iter() {
            let a = Addr::new(id, cell);
            let (row, col) = (cell.row, cell.col as u16);
            let f = fmt(wb.format_code(a), wb.style(a));
            match &c.input {
                CellInput::Formula(_) | CellInput::Opaque(_) => {
                    let mut src = prefix_future_functions(&wb.formula_source(a).unwrap_or_default());
                    if src.contains("[@") {
                        let table = wb.table_at(id, cell).map_or("", |t| t.name.as_str());
                        src = expand_this_row(&src, table);
                    }
                    let formula = Formula::new(format!("={src}")).set_result(result_text(&c.value));
                    match &f {
                        Some(f) => ws.write_formula_with_format(row, col, formula, f),
                        None => ws.write_formula(row, col, formula),
                    }
                    .map_err(err)?;
                }
                CellInput::Value(v) => {
                    match (v, &f) {
                        (Value::Number(n), Some(f)) => ws.write_number_with_format(row, col, *n, f),
                        (Value::Number(n), None) => ws.write_number(row, col, *n),
                        (Value::Text(t), Some(f)) => ws.write_string_with_format(row, col, t.as_ref(), f),
                        (Value::Text(t), None) => ws.write_string(row, col, t.as_ref()),
                        (Value::Bool(b), Some(f)) => ws.write_boolean_with_format(row, col, *b, f),
                        (Value::Bool(b), None) => ws.write_boolean(row, col, *b),
                        (Value::Error(e), _) => ws.write_formula(row, col, Formula::new(format!("={e}")).set_result(e.as_str())),
                        (Value::Empty, _) => continue,
                    }
                    .map_err(err)?;
                }
            }
        }
    }
    for d in wb.names().iter() {
        let name = match d.scope {
            Some(s) => format!("{}!{}", xl_engine::quote_sheet_name(wb.sheet_name(s)), d.name),
            None => d.name.clone(),
        };
        book.define_name(name, &format!("={}", prefix_future_functions(&wb.name_formula(d)))).map_err(err)?;
    }
    book.save(path).map_err(err)?;
    if !wb.scripts().is_empty() {
        crate::xlsx_scripts::add_to_file(path, wb.scripts())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_future_functions_only() {
        assert_eq!(prefix_future_functions("CONCAT(A1,B1)&CONCATENATE(\"IFS(\",1)"), "_xlfn.CONCAT(A1,B1)&CONCATENATE(\"IFS(\",1)");
        assert_eq!(prefix_future_functions("COUNTIFS(A:A,1)+IFS(A1,1)"), "COUNTIFS(A:A,1)+_xlfn.IFS(A1,1)");
        assert_eq!(prefix_future_functions("xlookup(1,A:A,B:B)"), "_xlfn.xlookup(1,A:A,B:B)");
    }
}
