//! Formula autocomplete (functions and defined names) and argument hints
//! while typing a formula.

use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};
use xl_engine::eval::Func;

use crate::app::Spreadsheet;

/// One suggestion.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Candidate {
    Function(Func),
    Name(String),
}

impl Candidate {
    pub(crate) fn label(&self) -> &str {
        match self {
            Candidate::Function(f) => f.name(),
            Candidate::Name(n) => n,
        }
    }
}

/// Most suggestions shown at once.
const MAX_CANDIDATES: usize = 8;

/// Is byte `i` of a formula inside a "string literal"?
fn in_string(text: &str, i: usize) -> bool {
    text[..i].bytes().filter(|&b| b == b'"').count() % 2 == 1
}

/// The identifier being typed just before `cursor` in a formula: (start, text).
pub(crate) fn word_at(text: &str, cursor: usize) -> Option<(usize, &str)> {
    if !text.starts_with('=') || cursor > text.len() || in_string(text, cursor) {
        return None;
    }
    let before = &text[..cursor];
    let start = before.char_indices().rev().take_while(|&(_, c)| c.is_ascii_alphanumeric() || c == '.' || c == '_').last().map(|(i, _)| i)?;
    let word = &before[start..];
    let prev = before[..start].chars().last();
    // Part of a sheet name, a cell range or an absolute reference: not a function.
    if !word.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') || matches!(prev, Some('!' | ':' | '$' | '\'')) {
        return None;
    }
    Some((start, word))
}

/// The innermost function call around `cursor`, and which argument it's in.
pub(crate) fn call_at(text: &str, cursor: usize) -> Option<(Func, usize)> {
    if !text.starts_with('=') {
        return None;
    }
    let mut stack: Vec<(Option<Func>, usize)> = Vec::new();
    let mut quoted = false;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate().take(cursor.min(text.len())) {
        match b {
            b'"' => quoted = !quoted,
            _ if quoted => {}
            b'(' => {
                let name_start = text[..i].char_indices().rev().take_while(|&(_, c)| c.is_ascii_alphanumeric() || c == '.' || c == '_').last().map_or(i, |(j, _)| j);
                stack.push((Func::from_name(&text[name_start..i]), 0));
            }
            b',' => {
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
            }
            b')' => {
                stack.pop();
            }
            _ => {}
        }
    }
    stack.into_iter().rev().find_map(|(f, arg)| Some((f?, arg)))
}

impl Spreadsheet {
    /// Suggestions for the word being typed, unless dismissed with Escape.
    pub(crate) fn completions(&self) -> Option<(usize, Vec<Candidate>)> {
        let e = self.edit.as_ref()?;
        let (start, word) = word_at(&e.text, e.cursor)?;
        if self.ac_dismissed == Some(start) {
            return None;
        }
        let up = word.to_ascii_uppercase();
        let mut out: Vec<Candidate> = Func::ALL.iter().filter(|f| f.name().starts_with(&up)).map(|&f| Candidate::Function(f)).collect();
        out.sort_by_key(|c| c.label().len());
        out.sort_by(|a, b| a.label().cmp(b.label()));
        let mut names: Vec<Candidate> = self.wb.names().iter().filter(|d| d.name.to_ascii_uppercase().starts_with(&up)).map(|d| Candidate::Name(d.name.clone())).collect();
        names.sort_by(|a, b| a.label().cmp(b.label()));
        out.extend(names);
        // A word that already is the only match needs no menu.
        if out.is_empty() || (out.len() == 1 && out[0].label().eq_ignore_ascii_case(word)) {
            return None;
        }
        out.truncate(MAX_CANDIDATES);
        Some((start, out))
    }

    /// Replace the word being typed with the chosen suggestion.
    pub(crate) fn accept_completion(&mut self) -> bool {
        let Some((start, items)) = self.completions() else { return false };
        let Some(choice) = items.get(self.ac_selected.min(items.len() - 1)).cloned() else { return false };
        let Some(e) = self.edit.as_mut() else { return false };
        let insert = match &choice {
            Candidate::Function(f) => format!("{}(", f.name()),
            Candidate::Name(n) => n.clone(),
        };
        // Don't double an opening parenthesis that's already there.
        let end = e.cursor;
        let insert = if insert.ends_with('(') && e.text[end..].starts_with('(') { insert.trim_end_matches('(').to_string() } else { insert };
        e.text.replace_range(start..end, &insert);
        e.cursor = (start + insert.len()).min(e.text.len());
        e.point = None;
        self.ac_selected = 0;
        true
    }

    /// Keys that steer the suggestion list while editing. Returns true if used.
    pub(crate) fn key_completion(&mut self, key: &str) -> bool {
        let Some((start, items)) = self.completions() else { return false };
        match key {
            "down" => self.ac_selected = (self.ac_selected + 1) % items.len(),
            "up" => self.ac_selected = (self.ac_selected + items.len() - 1) % items.len(),
            "tab" => return self.accept_completion(),
            "escape" => self.ac_dismissed = Some(start),
            _ => return false,
        }
        true
    }

    /// The popup under the cell being edited: suggestions, or the signature
    /// of the function the cursor is in.
    pub(crate) fn render_completion(&self, cx: &mut Context<Self>) -> Option<(gpui::Point<gpui::Pixels>, AnyElement)> {
        let e = self.edit.as_ref().filter(|e| e.at.sheet == self.cur)?;
        let t = self.theme;
        let c = e.at.cell;
        let x = f64::from(self.viewport.origin.x) + crate::app::HEADER_W + self.grid_x(c.col);
        let y = f64::from(self.viewport.origin.y) + crate::app::HEADER_H + self.grid_y(c.row) + self.row_h(c.row).max(crate::app::ROW_H) + 3.0;
        let pos = gpui::point(px(x as f32), px(y as f32));
        let panel = || {
            div()
                .occlude()
                .flex()
                .flex_col()
                .min_w(px(260.))
                .max_w(px(420.))
                .p(px(4.))
                .rounded(px(6.))
                .bg(t.popover)
                .border_1()
                .border_color(t.divider)
                .shadow_lg()
                .text_size(px(13.5))
                .text_color(t.ink)
        };
        if let Some((_, items)) = self.completions() {
            let sel = self.ac_selected.min(items.len() - 1);
            let mut list = panel();
            for (i, item) in items.iter().enumerate() {
                let detail = match item {
                    Candidate::Function(f) => f.signature().to_string(),
                    Candidate::Name(_) => "name".to_string(),
                };
                list = list.child(
                    div()
                        .id(("completion", i))
                        .flex()
                        .flex_row()
                        .gap(px(6.))
                        .h(px(24.))
                        .px(px(8.))
                        .items_center()
                        .rounded(px(4.))
                        .cursor_pointer()
                        .when(i == sel, |d| d.bg(t.header_selected))
                        .hover(|s| s.bg(t.chrome_hover))
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(SharedString::from(item.label().to_string())))
                        .child(div().text_color(t.ink_faint).overflow_hidden().whitespace_nowrap().child(SharedString::from(detail)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.ac_selected = i;
                            this.accept_completion();
                            cx.notify();
                        })),
                );
            }
            if let Candidate::Function(f) = &items[sel] {
                list = list.child(div().mt(px(4.)).px(px(8.)).py(px(6.)).border_t_1().border_color(t.divider).text_color(t.ink_muted).whitespace_normal().child(f.description()));
            }
            list = list.child(div().px(px(8.)).pb(px(2.)).text_size(px(12.)).text_color(t.ink_faint).child("Tab to insert · ↑↓ to choose · Esc to hide"));
            return Some((pos, list.into_any_element()));
        }
        let (f, arg) = call_at(&e.text, e.cursor)?;
        let sig = f.signature();
        let inner = sig.trim_start_matches('(').trim_end_matches(')');
        let params: Vec<&str> = if inner.is_empty() { Vec::new() } else { inner.split(", ").collect() };
        // Past the listed parameters, a trailing "…" means the last one repeats.
        let current = if arg < params.len() { arg } else if params.last() == Some(&"…") { params.len().saturating_sub(2) } else { usize::MAX };
        let mut line = div().flex().flex_row().flex_wrap().child(div().font_weight(FontWeight::SEMIBOLD).child(SharedString::from(format!("{}(", f.name()))));
        for (i, p) in params.iter().enumerate() {
            let text = if i + 1 < params.len() { format!("{p}, ") } else { p.to_string() };
            line = line.child(if i == current { div().font_weight(FontWeight::BOLD).text_color(t.accent).child(SharedString::from(text)) } else { div().child(SharedString::from(text)) });
        }
        line = line.child(div().child(")"));
        Some((pos, panel().px(px(10.)).py(px(6.)).gap(px(2.)).child(line).child(div().text_color(t.ink_muted).whitespace_normal().child(f.description())).into_any_element()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_word_being_typed() {
        assert_eq!(word_at("=SU", 3), Some((1, "SU")));
        assert_eq!(word_at("=A1+vlo", 7), Some((4, "vlo")));
        assert_eq!(word_at("=SUM(A1:B", 9), None, "the end of a range");
        assert_eq!(word_at("=Sheet2!A", 9), None);
        assert_eq!(word_at("=\"SU", 4), None, "inside a string");
        assert_eq!(word_at("SU", 2), None, "not a formula");
        assert_eq!(word_at("=SUM(1,", 7), None);
    }

    #[test]
    fn finds_the_call_and_argument() {
        assert_eq!(call_at("=IF(A1>0,", 9), Some((Func::If, 1)));
        assert_eq!(call_at("=ROUND(SUM(A1,B1),", 18), Some((Func::Round, 1)));
        assert_eq!(call_at("=ROUND(SUM(A1,", 14), Some((Func::Sum, 1)));
        assert_eq!(call_at("=IF(\"a,b\",", 10), Some((Func::If, 1)), "commas in strings don't count");
        assert_eq!(call_at("=(1+2", 5), None);
        assert_eq!(call_at("=SUM(1)", 7), None);
    }
}
