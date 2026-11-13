//! xlsx writer built on rust_xlsxwriter: sheets, values, formulas with cached
//! results, number formats and column widths.

use std::collections::HashMap;
use std::path::Path;

use rust_xlsxwriter::{Color, Format, FormatAlign, FormatBorder, FormatUnderline, Formula, Workbook as XlsxBook};
use xl_engine::{Addr, Border, CellInput, CellRef, HAlign, LineStyle, Style, Value, Workbook, precise};

use crate::to_excel_width_px;

/// Functions newer than Excel 2010 must carry the `_xlfn.` prefix in the
/// file or Excel shows #NAME?.
const FUTURE_FUNCTIONS: &[&str] = &["CONCAT", "IFS", "IFNA", "XLOOKUP", "XMATCH", "TEXTJOIN", "SWITCH", "MAXIFS", "MINIFS"];

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
        for (&col, &px) in wb.col_widths(id) {
            ws.set_column_width_pixels(col as u16, to_excel_width_px(px)).map_err(err)?;
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
                    let src = prefix_future_functions(&wb.formula_source(a).unwrap_or_default());
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
    book.save(path).map_err(err)
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
