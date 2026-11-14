//! Find and replace: the find bar, searching the current sheet in row order,
//! and replacing in cell inputs (formulas included).

use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement, Styled, div, px,
    prelude::FluentBuilder,
};
use xl_engine::{Addr, CellRef};

use crate::app::{Field, Spreadsheet};
use crate::chrome::{icon, tip};

#[derive(Default)]
pub struct FindState {
    pub open: bool,
    pub show_replace: bool,
    pub query: String,
    pub replacement: String,
    pub match_case: bool,
    /// Match the whole cell, not a part of it.
    pub whole_cell: bool,
    /// Matches for (query, options, sheet, edit generation), in row order.
    cache: Option<(CacheKey, Vec<CellRef>)>,
}

type CacheKey = (String, bool, bool, xl_engine::SheetId, u64);

impl FindState {
    fn matches_text(&self, text: &str) -> bool {
        if self.query.is_empty() {
            return false;
        }
        match (self.whole_cell, self.match_case) {
            (true, true) => text == self.query,
            (true, false) => text.to_lowercase() == self.query.to_lowercase(),
            (false, true) => text.contains(&self.query),
            (false, false) => text.to_lowercase().contains(&self.query.to_lowercase()),
        }
    }

    /// `text` with every occurrence of the query replaced.
    fn replaced(&self, text: &str) -> String {
        if self.whole_cell {
            return self.replacement.clone();
        }
        if self.match_case {
            return text.replace(&self.query, &self.replacement);
        }
        // Case-insensitive: find matches in a lowercased copy. Lowercasing can
        // change byte lengths, so map positions through char boundaries.
        let lower_q = self.query.to_lowercase();
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while !rest.is_empty() {
            let found = rest.char_indices().find(|&(i, _)| rest[i..].to_lowercase().starts_with(&lower_q));
            let Some((i, _)) = found else { break };
            // Consume chars from i until the lowercased prefix covers the query.
            let mut end = i;
            for (j, ch) in rest[i..].char_indices() {
                end = i + j + ch.len_utf8();
                if rest[i..end].to_lowercase().len() >= lower_q.len() {
                    break;
                }
            }
            out.push_str(&rest[..i]);
            out.push_str(&self.replacement);
            rest = &rest[end..];
        }
        out.push_str(rest);
        out
    }
}

impl Spreadsheet {
    pub(crate) fn open_find(&mut self, replace: bool) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        self.find.open = true;
        self.find.show_replace |= replace;
        self.field = Some(if replace { Field::Replace } else { Field::Find });
    }

    pub(crate) fn close_find(&mut self) {
        self.find.open = false;
        self.find.show_replace = false;
        if matches!(self.field, Some(Field::Find | Field::Replace)) {
            self.field = None;
        }
    }

    /// Cells on the current sheet whose input matches, in row order.
    pub(crate) fn find_matches(&mut self) -> &[CellRef] {
        let key: CacheKey = (self.find.query.clone(), self.find.match_case, self.find.whole_cell, self.cur, self.edit_generation);
        if self.find.cache.as_ref().is_none_or(|(k, _)| *k != key) {
            let mut hits: Vec<CellRef> =
                self.wb.store(self.cur).iter().map(|(c, _)| c).filter(|&c| self.find.matches_text(&self.wb.input_text(Addr::new(self.cur, c)))).collect();
            hits.sort_unstable_by_key(|c| (c.row, c.col));
            self.find.cache = Some((key, hits));
        }
        self.find.cache.as_ref().map_or(&[], |(_, v)| v.as_slice())
    }

    /// Select the next (or previous) match after the active cell, wrapping.
    pub(crate) fn find_next(&mut self, forward: bool) {
        let here = (self.active.row, self.active.col);
        let hits = self.find_matches().to_vec();
        if hits.is_empty() {
            if !self.find.query.is_empty() {
                self.message = Some((format!("No cells contain “{}”", self.find.query), true));
            }
            return;
        }
        let next = if forward {
            hits.iter().find(|c| (c.row, c.col) > here).or(hits.first())
        } else {
            hits.iter().rev().find(|c| (c.row, c.col) < here).or(hits.last())
        };
        if let Some(&c) = next {
            self.message = None;
            self.select(c, false);
        }
    }

    /// Replace in the active cell if it matches, then move to the next match.
    pub(crate) fn replace_one(&mut self) {
        let a = self.at(self.active);
        let text = self.wb.input_text(a);
        if self.find.matches_text(&text) {
            let new = self.find.replaced(&text);
            self.apply_inputs(vec![(a, new)]);
        }
        self.find_next(true);
    }

    pub(crate) fn replace_all(&mut self) {
        let hits = self.find_matches().to_vec();
        let changes: Vec<(Addr, String)> = hits
            .iter()
            .map(|&c| {
                let a = self.at(c);
                (a, self.find.replaced(&self.wb.input_text(a)))
            })
            .collect();
        let n = changes.len();
        self.apply_inputs(changes);
        self.message = Some((if n == 1 { "Replaced 1 cell".to_string() } else { format!("Replaced {n} cells") }, false));
    }

    pub(crate) fn key_find(&mut self, key: &str, shift: bool) -> bool {
        let Some(field) = self.field else { return false };
        match key {
            "enter" if field == Field::Replace => self.replace_one(),
            "enter" => self.find_next(!shift),
            "tab" if self.find.show_replace => self.field = Some(if field == Field::Find { Field::Replace } else { Field::Find }),
            "escape" => self.close_find(),
            "backspace" => {
                let text = if field == Field::Find { &mut self.find.query } else { &mut self.find.replacement };
                text.pop();
            }
            _ => return false,
        }
        true
    }

    pub(crate) fn render_find_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let count = self.find_matches().len();
        let active = self.active;
        let pos = self.find_matches().iter().position(|&c| c == active);
        let status = match (self.find.query.is_empty(), count, pos) {
            (true, _, _) => String::new(),
            (false, 0, _) => "No results".into(),
            (false, n, Some(i)) => format!("{} of {n}", i + 1),
            (false, n, None) => if n == 1 { "1 result".into() } else { format!("{n} results") },
        };
        let input = |id: &'static str, kind: Field, text: &str, placeholder: &'static str, focused: bool| {
            div()
                .id(id)
                .w(px(220.))
                .h(px(26.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(px(4.))
                .bg(t.canvas)
                .border_1()
                .border_color(if focused { t.accent } else { t.divider })
                .cursor_text()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(px(14.))
                .when(text.is_empty() && !focused, |d| d.text_color(t.ink_faint).child(placeholder))
                .when(!text.is_empty(), |d| d.text_color(t.ink).child(SharedString::from(text.to_string())))
                .when(focused, |d| d.child(div().w(px(1.5)).h(px(16.)).bg(t.accent)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        this.focus.focus(window, cx);
                        this.field = Some(kind);
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
        };
        let toggle = |id: &'static str, label: &'static str, on: bool, hint: &'static str| {
            div()
                .id(id)
                .h(px(26.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(px(13.))
                .text_color(if on { t.accent } else { t.ink_muted })
                .when(on, |d| d.bg(t.header_selected))
                .hover(|s| s.bg(t.chrome_hover))
                .tooltip(tip(t, hint, ""))
                .child(label)
        };
        let text_button = |id: &'static str, label: &'static str| {
            div().id(id).h(px(26.)).px(px(10.)).flex().items_center().rounded(px(4.)).cursor_pointer().text_size(px(13.5)).text_color(t.ink).hover(|s| s.bg(t.chrome_hover)).child(label)
        };
        let icon_button = |id: &'static str, name: &'static str, hint: &'static str| {
            div().id(id).size(px(26.)).flex().items_center().justify_center().rounded(px(4.)).cursor_pointer().hover(|s| s.bg(t.chrome_hover)).tooltip(tip(t, hint, "")).child(icon(name, t.ink))
        };
        let focused = self.field;
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .gap(px(6.))
            .h(px(38.))
            .px(px(10.))
            .bg(t.chrome)
            .border_b_1()
            .border_color(t.divider)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(icon("search", t.ink_muted))
            .child(input("find-query", Field::Find, &self.find.query, "Find", focused == Some(Field::Find)))
            .when(self.find.show_replace, |d| d.child(input("find-replacement", Field::Replace, &self.find.replacement, "Replace with", focused == Some(Field::Replace))))
            .child(toggle("find-case", "Aa", self.find.match_case, "Match case").on_click(cx.listener(|this, _, _, cx| {
                this.find.match_case = !this.find.match_case;
                cx.notify();
            })))
            .child(toggle("find-whole", "Whole cell", self.find.whole_cell, "Match the entire cell contents").on_click(cx.listener(|this, _, _, cx| {
                this.find.whole_cell = !this.find.whole_cell;
                cx.notify();
            })))
            .child(div().min_w(px(70.)).text_size(px(13.)).text_color(t.ink_muted).child(SharedString::from(status)))
            .child(icon_button("find-prev", "chevron-up", "Previous (Shift+Enter)").on_click(cx.listener(|this, _, _, cx| {
                this.find_next(false);
                cx.notify();
            })))
            .child(icon_button("find-next", "chevron-down", "Next (Enter)").on_click(cx.listener(|this, _, _, cx| {
                this.find_next(true);
                cx.notify();
            })))
            .when(self.find.show_replace, |d| {
                d.child(text_button("find-replace-one", "Replace").on_click(cx.listener(|this, _, _, cx| {
                    this.replace_one();
                    cx.notify();
                })))
                .child(text_button("find-replace-all", "Replace all").font_weight(FontWeight::SEMIBOLD).on_click(cx.listener(|this, _, _, cx| {
                    this.replace_all();
                    cx.notify();
                })))
            })
            .when(!self.find.show_replace, |d| {
                d.child(text_button("find-show-replace", "Replace…").text_color(t.ink_muted).on_click(cx.listener(|this, _, _, cx| {
                    this.find.show_replace = true;
                    this.field = Some(Field::Replace);
                    cx.notify();
                })))
            })
            .child(div().flex_1())
            .child(icon_button("find-close", "x", "Close (Esc)").on_click(cx.listener(|this, _, _, cx| {
                this.close_find();
                cx.notify();
            })))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::FindState;

    fn state(q: &str, r: &str, case: bool, whole: bool) -> FindState {
        FindState { query: q.into(), replacement: r.into(), match_case: case, whole_cell: whole, ..FindState::default() }
    }

    #[test]
    fn replaces_case_insensitively_and_keeps_the_rest() {
        assert_eq!(state("oat", "soy", false, false).replaced("Oat milk, OAT bran"), "soy milk, soy bran");
        assert_eq!(state("oat", "soy", true, false).replaced("Oat milk, oat bran"), "Oat milk, soy bran");
        assert_eq!(state("Straße", "Weg", false, false).replaced("Hauptstraße 5"), "HauptWeg 5");
        assert_eq!(state("x", "y", false, true).replaced("X"), "y");
        assert!(!state("milk", "", false, true).matches_text("Oat milk"));
        assert!(state("MILK", "", false, false).matches_text("Oat milk"));
    }
}
