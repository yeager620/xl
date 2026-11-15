//! Diffs and workbooks as stable, line-oriented text.

use std::fmt::Write;

use xl_engine::{Addr, CellRef, Style, Value, Workbook, col_name, quote_sheet_name};

use crate::diff::{CellChange, CellInfo, CellKind, LineAxis, SheetStatus, WorkbookDiff};

/// Cells listed under an inserted/deleted line run or a whole sheet.
const LINE_CELLS: usize = 10;

/// A value as shown in diffs: numbers exactly, text quoted.
pub fn value_text(v: &Value) -> String {
    match v {
        Value::Empty => "(empty)".into(),
        Value::Number(n) => xl_engine::precise(*n),
        Value::Text(t) => format!("\"{}\"", escape(t).replace('"', "\\\"")),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Error(e) => e.as_str().into(),
    }
}

/// Backslash-escape characters that would break a line or a field.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// What a cell holds, in one line: `=B4*C4 (43.5)` or `120`.
pub fn content_text(i: &CellInfo) -> String {
    match &i.formula {
        Some(f) => format!("{} ({})", escape(f), value_text(&i.value)),
        None => value_text(&i.value),
    }
}

fn rgb(c: Option<u32>, none: &str) -> String {
    c.map_or_else(|| none.to_string(), |c| format!("#{c:06X}"))
}

/// The differences between two styles, e.g. `bold on, fill none → #FFFF00`.
pub fn style_delta(a: &Style, b: &Style) -> String {
    let mut out: Vec<String> = Vec::new();
    for (name, x, y) in [("bold", a.bold, b.bold), ("italic", a.italic, b.italic), ("underline", a.underline, b.underline), ("strikethrough", a.strike, b.strike), ("wrap", a.wrap, b.wrap)] {
        if x != y {
            out.push(format!("{name} {}", if y { "on" } else { "off" }));
        }
    }
    if a.color != b.color {
        out.push(format!("color {} → {}", rgb(a.color, "automatic"), rgb(b.color, "automatic")));
    }
    if a.fill != b.fill {
        out.push(format!("fill {} → {}", rgb(a.fill, "none"), rgb(b.fill, "none")));
    }
    if a.align != b.align {
        out.push(format!("align {:?} → {:?}", a.align, b.align).to_lowercase());
    }
    if a.valign != b.valign {
        out.push(format!("vertical align {:?} → {:?}", a.valign, b.valign).to_lowercase());
    }
    if a.font_size != b.font_size {
        let pt = |s: Option<u16>| s.map_or("default".to_string(), |t| format!("{}pt", t as f64 / 10.0));
        out.push(format!("font size {} → {}", pt(a.font_size), pt(b.font_size)));
    }
    if a.font_name != b.font_name {
        out.push(format!("font {} → {}", a.font_name.as_deref().unwrap_or("default"), b.font_name.as_deref().unwrap_or("default")));
    }
    for (name, x, y) in [("top", &a.top, &b.top), ("bottom", &a.bottom, &b.bottom), ("left", &a.left, &b.left), ("right", &a.right, &b.right)] {
        if x != y {
            let d = |bd: &xl_engine::Border| if bd.is_none() { "none".to_string() } else { format!("{:?}", bd.style).to_lowercase() };
            out.push(format!("{name} border {} → {}", d(x), d(y)));
        }
    }
    out.join(", ")
}

/// Describe a modified cell's changes (without the address), one entry per aspect.
pub fn cell_aspects(c: &CellChange) -> Vec<String> {
    let mut out = Vec::new();
    let (Some(a), Some(b)) = (&c.old, &c.new) else {
        let i = c.old.as_ref().or(c.new.as_ref());
        if let Some(i) = i {
            if i.has_content() {
                out.push(content_text(i));
            }
            if let Some(f) = &i.format {
                out.push(format!("format {}", escape(f)));
            }
            if !i.style.is_default() {
                out.push(format!("style {}", style_delta(&Style::default(), &i.style)));
            }
        }
        return out;
    };
    if c.content {
        match (&a.formula, &b.formula) {
            (None, None) => out.push(format!("{} → {}", value_text(&a.value), value_text(&b.value))),
            (Some(x), Some(y)) if c.value => out.push(format!("{} → {} ({} → {})", escape(x), escape(y), value_text(&a.value), value_text(&b.value))),
            (Some(x), Some(y)) => out.push(format!("{} → {}", escape(x), escape(y))),
            _ => out.push(format!("{} → {}", content_text(a), content_text(b))),
        }
    } else if c.value {
        let f = b.formula.as_deref().map(escape).unwrap_or_default();
        out.push(format!("{} → {} ({f})", value_text(&a.value), value_text(&b.value)));
    }
    if c.format {
        out.push(format!("format {} → {}", a.format.as_deref().map_or("General".into(), escape), b.format.as_deref().map_or("General".into(), escape)));
    }
    if c.style {
        out.push(format!("style {}", style_delta(&a.style, &b.style)));
    }
    out
}

fn addr(sheet: &str, c: CellRef) -> String {
    format!("{}!{c}", quote_sheet_name(sheet))
}

fn plural(n: u32, one: &str, many: &str) -> String {
    if n == 1 { one.to_string() } else { many.to_string() }
}

fn line_label(axis: LineAxis, start: u32, count: u32) -> String {
    match axis {
        LineAxis::Row if count == 1 => format!("row {}", start + 1),
        LineAxis::Row => format!("rows {}–{}", start + 1, start + count),
        LineAxis::Col if count == 1 => format!("column {}", col_name(start)),
        LineAxis::Col => format!("columns {}–{}", col_name(start), col_name(start + count - 1)),
    }
}

/// The diff as text: one change per line, addressed like `Sheet1!B7`, in a
/// stable order. Added things start with `+ `, removed ones with `- `, other
/// sheet-level and layout changes with `~ `; changed cells are bare.
pub fn render_diff(d: &WorkbookDiff) -> String {
    let mut out = String::new();
    for s in &d.sheets {
        let name = s.name();
        match s.status {
            SheetStatus::Added => {
                writeln!(out, "+ sheet {} added ({} {})", quote_sheet_name(name), s.cells.len(), plural(s.cells.len() as u32, "cell", "cells")).ok();
                list_cells(&mut out, "+", name, s.cells.iter().collect());
                continue;
            }
            SheetStatus::Removed => {
                writeln!(out, "- sheet {} removed ({} {})", quote_sheet_name(name), s.cells.len(), plural(s.cells.len() as u32, "cell", "cells")).ok();
                list_cells(&mut out, "-", name, s.cells.iter().collect());
                continue;
            }
            SheetStatus::Matched => {}
        }
        if s.renamed() {
            writeln!(out, "~ sheet {} renamed to {}", quote_sheet_name(s.old_name.as_deref().unwrap_or("")), quote_sheet_name(name)).ok();
        }
        let old_name = s.old_name.as_deref().unwrap_or(name);
        for l in &s.lines {
            let label = line_label(l.axis, l.start, l.count);
            let in_run = |c: &CellChange| -> bool {
                let (pos, want) = if l.inserted { (c.new_at, CellKind::Added) } else { (c.old_at, CellKind::Removed) };
                c.in_line
                    && c.kind == want
                    && pos.is_some_and(|p| {
                        let i = if l.axis == LineAxis::Row { p.row } else { p.col };
                        i >= l.start && i < l.start + l.count
                    })
            };
            let cells: Vec<&CellChange> = s.cells.iter().filter(|c| in_run(c)).collect();
            if l.inserted {
                writeln!(out, "+ {} {label} inserted", quote_sheet_name(name)).ok();
                list_cells(&mut out, "+", name, cells);
            } else {
                let now = match l.axis {
                    LineAxis::Row => format!("row {}", l.at_new + 1),
                    LineAxis::Col => format!("column {}", col_name(l.at_new)),
                };
                writeln!(out, "- {} {label} deleted (old numbering; before {now})", quote_sheet_name(old_name)).ok();
                list_cells(&mut out, "-", old_name, cells);
            }
        }
        for c in s.cells.iter().filter(|c| !c.in_line) {
            let at = match (c.old_at, c.new_at) {
                (Some(o), Some(n)) if o != n && c.kind == CellKind::Modified => format!("{} (was {o})", addr(name, n)),
                (_, Some(n)) => addr(name, n),
                (Some(o), None) => addr(old_name, o),
                (None, None) => continue,
            };
            let mark = match c.kind {
                CellKind::Added => "+ ",
                CellKind::Removed => "- ",
                CellKind::Modified => "",
            };
            for a in cell_aspects(c) {
                writeln!(out, "{mark}{at}: {a}").ok();
            }
        }
        for l in &s.layout {
            writeln!(out, "~ {}: {}", quote_sheet_name(name), l.text).ok();
        }
    }
    if d.sheet_order_changed {
        let order: Vec<String> = d.sheets.iter().filter(|s| s.status == SheetStatus::Matched).map(|s| quote_sheet_name(s.name())).collect();
        writeln!(out, "~ sheet order: {}", order.join(", ")).ok();
    }
    for n in &d.names {
        match (&n.old, &n.new) {
            (None, Some(f)) => writeln!(out, "+ name {}: {}", n.name, escape(f)),
            (Some(f), None) => writeln!(out, "- name {}: {}", n.name, escape(f)),
            (Some(a), Some(b)) => writeln!(out, "~ name {}: {} → {}", n.name, escape(a), escape(b)),
            (None, None) => Ok(()),
        }
        .ok();
    }
    out
}

fn list_cells(out: &mut String, mark: &str, sheet: &str, cells: Vec<&CellChange>) {
    for c in cells.iter().take(LINE_CELLS) {
        let Some(at) = c.new_at.or(c.old_at) else { continue };
        let Some(info) = c.new.as_ref().or(c.old.as_ref()) else { continue };
        writeln!(out, "{mark}   {}: {}", addr(sheet, at), content_text(info)).ok();
    }
    if cells.len() > LINE_CELLS {
        let more = cells.len() - LINE_CELLS;
        writeln!(out, "{mark}   … {more} more {}", plural(more as u32, "cell", "cells")).ok();
    }
}

/// A stable text dump of a workbook for `git diff` (textconv): a `# Sheet`
/// header per sheet, then one cell per line in row-major order as
/// `Sheet!A1<TAB>value[<TAB>=formula]`. Values are exact (numbers in full
/// precision) with tabs, newlines and backslashes escaped.
pub fn textconv(wb: &Workbook) -> String {
    let mut out = String::new();
    for &s in wb.sheet_ids() {
        let name = quote_sheet_name(wb.sheet_name(s));
        writeln!(out, "# {name}").ok();
        let mut cells: Vec<CellRef> = wb.store(s).iter().map(|(c, _)| c).collect();
        cells.sort_unstable();
        for c in cells {
            let info = crate::diff::cell_info(wb, Addr::new(s, c));
            if !info.has_content() {
                continue;
            }
            let value = match &info.value {
                Value::Empty => String::new(),
                Value::Number(n) => xl_engine::precise(*n),
                Value::Text(t) => escape(t),
                v => value_text(v),
            };
            match &info.formula {
                Some(f) => writeln!(out, "{name}!{c}\t{value}\t{}", escape(f)),
                None => writeln!(out, "{name}!{c}\t{value}"),
            }
            .ok();
        }
    }
    let mut names: Vec<(String, String)> = wb
        .names()
        .iter()
        .map(|d| {
            let scope = d.scope.map(|s| format!("{}!", quote_sheet_name(wb.sheet_name(s)))).unwrap_or_default();
            (format!("{scope}{}", d.name), wb.name_formula(d))
        })
        .collect();
    names.sort();
    for (n, f) in names {
        writeln!(out, "# name {n}\t={}", escape(&f)).ok();
    }
    out
}
