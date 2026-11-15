//! Notes on cells: the corner marker, the hover card, and editing.

use gpui::{AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Pixels, Point, SharedString, Styled, div, point, prelude::FluentBuilder, px};
use xl_engine::{CellRef, Note};

use crate::app::{Field, Spreadsheet};

/// Who new notes are by: the login name, which is all a local app knows.
pub(crate) fn note_author() -> String {
    std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "xl".into())
}

impl Spreadsheet {
    pub(crate) fn note_at(&self, cell: CellRef) -> Option<&Note> {
        let l = self.wb.layout(self.cur);
        // A note on a merged range belongs to its top-left cell.
        let cell = l.merge_at(cell).map_or(cell, |m| m.start);
        l.notes.get(&cell)
    }

    /// Start writing (or editing) the active cell's note.
    pub(crate) fn edit_note(&mut self) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let cell = self.active;
        let text = self.note_at(cell).map(|n| n.text.clone()).unwrap_or_default();
        self.note_edit = Some((cell, text));
        self.context = None;
        self.field = Some(Field::Note);
    }

    pub(crate) fn finish_note(&mut self, save: bool) {
        let Some((cell, text)) = self.note_edit.take() else { return };
        if self.field == Some(Field::Note) {
            self.field = None;
        }
        if !save {
            return;
        }
        let existing = self.note_at(cell).cloned();
        let text = text.trim_end().to_string();
        if existing.as_ref().map(|n| n.text.as_str()) == Some(text.as_str()) {
            return;
        }
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                if text.is_empty() {
                    l.notes.remove(&cell);
                } else {
                    let author = existing.map_or_else(note_author, |n| n.author);
                    l.notes.insert(cell, Note { author, text });
                }
            }
        });
    }

    pub(crate) fn delete_note(&mut self) {
        let sel = self.selection();
        self.context = None;
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                l.notes.retain(|c, _| !sel.contains(*c));
            }
        });
    }

    pub(crate) fn key_note(&mut self, key: &str, shift: bool) -> bool {
        match key {
            "enter" if shift => {
                if let Some((_, t)) = self.note_edit.as_mut() {
                    t.push('\n');
                }
            }
            "enter" => self.finish_note(true),
            "escape" => self.finish_note(false),
            "backspace" => {
                if let Some((_, t)) = self.note_edit.as_mut() {
                    t.pop();
                }
            }
            _ => return false,
        }
        true
    }

    /// Window position of a cell's top-right corner.
    fn cell_corner(&self, cell: CellRef) -> Point<Pixels> {
        let x = f64::from(self.viewport.origin.x) + self.hw() + self.grid_x(cell.col) + self.col_w(cell.col) + 4.0;
        let y = f64::from(self.viewport.origin.y) + self.hh() + self.grid_y(cell.row);
        point(px(x as f32), px(y as f32))
    }

    /// The note card: the one being edited, else the one under the pointer.
    pub(crate) fn render_note_card(&self, cx: &mut Context<Self>) -> Option<(Point<Pixels>, AnyElement)> {
        let t = self.theme;
        let card = || {
            div()
                .occlude()
                .w(px(240.))
                .p(px(10.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .rounded(px(6.))
                .bg(t.popover)
                .border_1()
                .border_color(t.divider)
                .shadow_lg()
                .text_size(px(13.5))
                .text_color(t.ink)
        };
        if let Some((cell, text)) = &self.note_edit {
            let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
            let mut body = div().flex().flex_col().min_h(px(48.));
            let last = lines.len() - 1;
            for (i, line) in lines.into_iter().enumerate() {
                let mut row = div().flex().flex_row().min_h(px(18.)).child(SharedString::from(line));
                if i == last {
                    row = row.child(div().w(px(1.5)).h(px(16.)).bg(t.accent));
                }
                body = body.child(row);
            }
            let el = card()
                .border_color(t.accent)
                .child(div().text_size(px(12.)).text_color(t.ink_muted).child(SharedString::from(format!("Note on {cell}"))))
                .child(body)
                .child(div().text_size(px(11.5)).text_color(t.ink_faint).child("Enter to save · Shift+Enter for a new line · Esc to cancel"))
                .into_any_element();
            return Some((self.cell_corner(*cell), el));
        }
        let cell = self.hover_note?;
        let note = self.note_at(cell)?;
        let _ = cx;
        let el = card()
            .when(!note.author.is_empty(), |d| d.child(div().font_weight(FontWeight::SEMIBOLD).child(SharedString::from(note.author.clone()))))
            .children(note.text.split('\n').map(|l| div().child(SharedString::from(l.to_string()))))
            .into_any_element();
        Some((self.cell_corner(cell), el))
    }
}
