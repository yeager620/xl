//! Platform text input (IME): dead keys, compose sequences and CJK input
//! methods deliver text here rather than through key events. GPUI ranges are
//! in UTF-16 code units; our edit buffer is UTF-8.

use std::ops::Range;

use gpui::{Bounds, Context, EntityInputHandler, Pixels, UTF16Selection, Window, point, px, size};

use crate::app::{CELL_FONT, CELL_PAD, EditMode, HEADER_H, HEADER_W, ROW_H, Spreadsheet};

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn byte_to_utf16(s: &str, byte: usize) -> usize {
    utf16_len(&s[..byte.min(s.len())])
}

fn utf16_to_byte(s: &str, units: usize) -> usize {
    let mut n = 0;
    for (i, c) in s.char_indices() {
        if n >= units {
            return i;
        }
        n += c.len_utf16();
    }
    s.len()
}

fn range_to_bytes(s: &str, r: &Range<usize>) -> Range<usize> {
    utf16_to_byte(s, r.start)..utf16_to_byte(s, r.end)
}

fn range_to_utf16(s: &str, r: &Range<usize>) -> Range<usize> {
    byte_to_utf16(s, r.start)..byte_to_utf16(s, r.end)
}

impl Spreadsheet {
    /// Text the input method is working on: the tab being renamed, the cell
    /// being edited, or nothing (typing will start an edit).
    fn ime_text(&self) -> &str {
        if let Some(p) = &self.palette {
            return &p.query;
        }
        match (&self.renaming, &self.edit) {
            (Some((_, name)), _) => name,
            (None, Some(e)) => &e.text,
            (None, None) => "",
        }
    }
}

impl EntityInputHandler for Spreadsheet {
    fn text_for_range(&mut self, range: Range<usize>, adjusted: &mut Option<Range<usize>>, _: &mut Window, _: &mut Context<Self>) -> Option<String> {
        let text = self.ime_text();
        let r = range_to_bytes(text, &range);
        *adjusted = Some(range_to_utf16(text, &r));
        Some(text[r].to_string())
    }

    fn selected_text_range(&mut self, _ignore_disabled: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        if let Some(p) = &self.palette {
            let end = utf16_len(&p.query);
            return Some(UTF16Selection { range: end..end, reversed: false });
        }
        let caret = match (&self.renaming, &self.edit) {
            (Some((_, name)), _) => utf16_len(name),
            (None, Some(e)) => byte_to_utf16(&e.text, e.cursor),
            (None, None) => 0,
        };
        Some(UTF16Selection { range: caret..caret, reversed: false })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        let e = self.edit.as_ref()?;
        e.marked.as_ref().map(|m| range_to_utf16(&e.text, m))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(e) = self.edit.as_mut() {
            e.marked = None;
            cx.notify();
        }
    }

    fn replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        if text.is_empty() || text.chars().all(char::is_control) {
            return;
        }
        if let Some(p) = self.palette.as_mut() {
            p.query.push_str(text);
            p.selected = 0;
            cx.notify();
            return;
        }
        if let Some((_, name)) = self.renaming.as_mut() {
            name.push_str(text);
            cx.notify();
            return;
        }
        let Some(e) = self.edit.as_mut() else {
            // Typing on a selected cell starts an edit that replaces it.
            self.begin_edit(Some(text.to_string()), EditMode::Enter);
            cx.notify();
            return;
        };
        let r = range.map(|r| range_to_bytes(&e.text, &r)).or_else(|| e.marked.clone()).unwrap_or(e.cursor..e.cursor);
        e.point = None;
        e.text.replace_range(r.clone(), text);
        e.cursor = r.start + text.len();
        e.marked = None;
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.renaming.is_some() || self.palette.is_some() {
            return;
        }
        if self.edit.is_none() {
            if new_text.is_empty() {
                return;
            }
            self.begin_edit(Some(String::new()), EditMode::Enter);
        }
        let Some(e) = self.edit.as_mut() else { return };
        let r = range.map(|r| range_to_bytes(&e.text, &r)).or_else(|| e.marked.clone()).unwrap_or(e.cursor..e.cursor);
        e.point = None;
        e.text.replace_range(r.clone(), new_text);
        let end = r.start + new_text.len();
        e.marked = (!new_text.is_empty()).then_some(r.start..end);
        e.cursor = match new_selected {
            Some(sel) => r.start + utf16_to_byte(new_text, sel.end),
            None => end,
        };
        cx.notify();
    }

    /// Where the caret is, so the input method's candidate window sits next to it.
    fn bounds_for_range(&mut self, range: Range<usize>, element: Bounds<Pixels>, window: &mut Window, _: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        let e = self.edit.as_ref().filter(|e| e.at.sheet == self.cur)?;
        let prefix = &e.text[..utf16_to_byte(&e.text, range.start)];
        let width = crate::paint::shape(window, prefix.to_string(), crate::paint::cell_font(), self.theme.ink, CELL_FONT).width;
        let x = f64::from(element.origin.x) + HEADER_W + self.col_x(e.at.cell.col) - self.scroll_x + CELL_PAD;
        let y = f64::from(element.origin.y) + HEADER_H + e.at.cell.row as f64 * ROW_H - self.scroll_y;
        Some(Bounds::new(point(px(x as f32) + width, px(y as f32)), size(px(1.), px(ROW_H as f32))))
    }

    fn character_index_for_point(&mut self, _: gpui::Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_conversions() {
        let s = "a😀é";
        assert_eq!(utf16_len(s), 4);
        assert_eq!(byte_to_utf16(s, 5), 3); // after the emoji
        assert_eq!(utf16_to_byte(s, 3), 5);
        assert_eq!(utf16_to_byte(s, 99), s.len());
        assert_eq!(range_to_bytes(s, &(1..3)), 1..5);
    }
}
