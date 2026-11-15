//! Data validation in the app: the dropdown on list cells, input prompts,
//! and creating or clearing rules.

use std::sync::Arc;

use gpui::{AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Pixels, Point, SharedString, StatefulInteractiveElement, Styled, div, point, px};
use xl_engine::formula::parse_formula;
use xl_engine::validation::{ErrorStyle, ListSource, Validation, ValidationKind, ValidationOp, ValidationRule};
use xl_engine::Range;

use crate::app::Spreadsheet;

/// Width of the dropdown button beside a list cell.
pub(crate) const DROPDOWN_W: f64 = 18.0;

impl Spreadsheet {
    /// The list rule (with a dropdown) on the active cell.
    pub(crate) fn active_list(&self) -> Option<&Validation> {
        self.wb.validation_at(self.cur, self.active).filter(|v| v.dropdown && matches!(v.rule, ValidationRule::List(_)))
    }

    /// Window rectangle of the active cell's dropdown button (x, y, w, h).
    pub(crate) fn dropdown_rect(&self) -> Option<(f64, f64, f64, f64)> {
        if self.edit.is_some() || self.active_list().is_none() {
            return None;
        }
        let c = self.wb.layout(self.cur).merge_at(self.active).map_or(self.active, |m| m.end);
        let x = f64::from(self.viewport.origin.x) + self.hw() + self.grid_x(c.col) + self.col_w(c.col);
        let y = f64::from(self.viewport.origin.y) + self.hh() + self.grid_y(self.active.row);
        Some((x, y, DROPDOWN_W, self.row_h(self.active.row)))
    }

    pub(crate) fn on_dropdown(&self, pos: Point<Pixels>) -> bool {
        self.dropdown_rect().is_some_and(|(x, y, w, h)| {
            let (px_, py) = (f64::from(pos.x), f64::from(pos.y));
            px_ >= x && px_ <= x + w && py >= y && py <= y + h
        })
    }

    /// Open the list of choices for the active cell (Alt+Down or the button).
    pub(crate) fn open_dropdown(&mut self) {
        let Some(v) = self.active_list() else { return };
        let choices = self.wb.list_choices(self.cur, v);
        let Some((x, y, _, h)) = self.dropdown_rect() else { return };
        let left = x - self.col_w(self.active.col);
        self.dropdown = Some((self.active, choices, point(px(left as f32), px((y + h) as f32))));
        self.menu = None;
        self.context = None;
    }

    pub(crate) fn choose(&mut self, i: usize) {
        let Some((cell, choices, _)) = self.dropdown.take() else { return };
        if let Some(choice) = choices.get(i) {
            let a = self.at(cell);
            self.apply_inputs(vec![(a, choice.clone())]);
        }
    }

    pub(crate) fn render_dropdown(&self, cx: &mut Context<Self>) -> Option<(Point<Pixels>, AnyElement)> {
        let (cell, choices, pos) = self.dropdown.as_ref()?;
        let t = self.theme;
        let current = self.wb.display(self.at(*cell)).text;
        let mut list = div()
            .id("dv-list")
            .occlude()
            .min_w(px(self.col_w(cell.col).max(140.0) as f32))
            .max_h(px(280.))
            .overflow_y_scroll()
            .p(px(4.))
            .flex()
            .flex_col()
            .rounded(px(6.))
            .bg(t.popover)
            .border_1()
            .border_color(t.divider)
            .shadow_lg()
            .text_size(px(14.))
            .text_color(t.ink);
        if choices.is_empty() {
            list = list.child(div().px(px(8.)).py(px(4.)).text_color(t.ink_faint).child("No choices"));
        }
        for (i, c) in choices.iter().enumerate() {
            let on = c.eq_ignore_ascii_case(&current);
            list = list.child(
                div()
                    .id(("dv-choice", i))
                    .h(px(26.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .when_on(on, t.header_selected)
                    .hover(|s| s.bg(t.chrome_hover))
                    .child(SharedString::from(c.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.choose(i);
                        cx.notify();
                    })),
            );
        }
        Some((*pos, list.into_any_element()))
    }

    /// The active cell's input prompt, shown beside it.
    pub(crate) fn render_prompt(&self) -> Option<(Point<Pixels>, AnyElement)> {
        if self.edit.is_some() || self.dropdown.is_some() {
            return None;
        }
        let v = self.wb.validation_at(self.cur, self.active)?;
        let (title, body) = v.prompt.as_ref()?;
        let t = self.theme;
        let x = f64::from(self.viewport.origin.x) + self.hw() + self.grid_x(self.active.col) + 12.0;
        let y = f64::from(self.viewport.origin.y) + self.hh() + self.grid_y(self.active.row) + self.row_h(self.active.row) + 4.0;
        let card = div()
            .max_w(px(260.))
            .p(px(8.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .rounded(px(6.))
            .bg(t.popover)
            .border_1()
            .border_color(t.divider)
            .shadow_lg()
            .text_size(px(13.))
            .text_color(t.ink)
            .when_some_text(title, t.ink)
            .child(div().text_color(t.ink_muted).child(SharedString::from(body.clone())));
        Some((point(px(x as f32), px(y as f32)), card.into_any_element()))
    }

    /// Make a rule for the selection from the dialog's text fields.
    pub(crate) fn apply_validation_form(&mut self, allow: &str, condition: &str, value: &str, second: &str, error: &str) -> Result<(), String> {
        let sel = self.selection();
        let expr = |src: &str| -> Result<Arc<xl_engine::formula::Expr>, String> {
            let src = src.trim();
            let src = src.strip_prefix('=').unwrap_or(src);
            if src.is_empty() {
                return Err("Enter a value".into());
            }
            parse_formula(src, self.wb.sheets()).map(Arc::new).map_err(|e| e.to_string())
        };
        let a = allow.trim().to_ascii_lowercase();
        let kind = |k| {
            let op = parse_op(condition).ok_or_else(|| format!("“{condition}” isn't a condition (between, =, <>, >, <, >=, <=)"))?;
            let b = if op.two_values() { Some(expr(second)?) } else { None };
            Ok::<_, String>(ValidationRule::Compare { kind: k, op, a: expr(value)?, b })
        };
        let rule = match a.as_str() {
            "list" | "" => {
                let v = value.trim();
                if v.starts_with('=') {
                    ValidationRule::List(ListSource::Cells(expr(v)?))
                } else {
                    let items: Vec<String> = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                    if items.is_empty() {
                        return Err("List the choices, separated by commas (or =A1:A9 for cells)".into());
                    }
                    ValidationRule::List(ListSource::Items(items))
                }
            }
            "whole" | "whole number" | "integer" => kind(ValidationKind::Whole)?,
            "decimal" | "number" => kind(ValidationKind::Decimal)?,
            "date" => kind(ValidationKind::Date)?,
            "time" => kind(ValidationKind::Time)?,
            "length" | "text length" => kind(ValidationKind::TextLength)?,
            "custom" | "formula" => ValidationRule::Custom(expr(value)?),
            other => return Err(format!("“{other}” isn't a kind of rule (list, whole, decimal, date, time, length, custom)")),
        };
        let mut v = Validation::new(vec![sel], rule);
        if !error.trim().is_empty() {
            v.error = Some((String::new(), error.trim().to_string()));
        }
        v.error_style = ErrorStyle::Stop;
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                // The new rule replaces old ones on the same cells.
                l.validations.retain(|old| !old.ranges.iter().any(|r| overlaps(*r, sel)));
                l.validations.push(v);
            }
        });
        Ok(())
    }

    pub(crate) fn clear_validation(&mut self) {
        let sel = self.selection();
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                l.validations.retain(|v| !v.ranges.iter().any(|r| overlaps(*r, sel)));
            }
        });
    }
}

fn overlaps(a: Range, b: Range) -> bool {
    a.start.row <= b.end.row && b.start.row <= a.end.row && a.start.col <= b.end.col && b.start.col <= a.end.col
}

fn parse_op(s: &str) -> Option<ValidationOp> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "between" | "" => ValidationOp::Between,
        "not between" => ValidationOp::NotBetween,
        "=" | "equal" | "equal to" => ValidationOp::Equal,
        "<>" | "!=" | "not equal" | "not equal to" => ValidationOp::NotEqual,
        ">" | "greater than" => ValidationOp::Greater,
        "<" | "less than" => ValidationOp::Less,
        ">=" | "at least" => ValidationOp::GreaterEq,
        "<=" | "at most" => ValidationOp::LessEq,
        _ => return None,
    })
}

/// Small styling helpers for optional bits.
trait OptionalStyle: Sized {
    fn when_on(self, on: bool, bg: gpui::Hsla) -> Self;
    fn when_some_text(self, text: &str, color: gpui::Hsla) -> Self;
}

impl<E: ParentElement + Styled> OptionalStyle for E {
    fn when_on(self, on: bool, bg: gpui::Hsla) -> Self {
        if on { self.bg(bg) } else { self }
    }

    fn when_some_text(self, text: &str, color: gpui::Hsla) -> Self {
        if text.is_empty() { self } else { self.child(div().font_weight(gpui::FontWeight::SEMIBOLD).text_color(color).child(SharedString::from(text.to_string()))) }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn conditions() {
        use super::parse_op;
        use xl_engine::validation::ValidationOp;
        assert_eq!(parse_op(">="), Some(ValidationOp::GreaterEq));
        assert_eq!(parse_op("Not Between"), Some(ValidationOp::NotBetween));
        assert_eq!(parse_op("around"), None);
    }
}
