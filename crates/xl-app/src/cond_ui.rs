//! Creating conditional formats: the toolbar menu of presets and the small
//! dialog for rules that need a value.

use std::sync::Arc;

use gpui::{Context, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px};
use xl_engine::Range;
use xl_engine::cond::{CfOp, CfRule, CfStyle, CfValue, CondFormat, TextMatch};
use xl_engine::formula::parse_formula;

use crate::app::{Field, Spreadsheet};
use crate::chrome::{menu_item, menu_panel, menu_separator};

/// Highlight styles offered for value rules: (label, fill, text color, bold).
pub(crate) const PRESETS: [(&str, Option<u32>, Option<u32>, bool); 4] = [
    ("Light red fill with dark red text", Some(0xFFC7CE), Some(0x9C0006), false),
    ("Yellow fill with dark yellow text", Some(0xFFEB9C), Some(0x9C5700), false),
    ("Green fill with dark green text", Some(0xC6EFCE), Some(0x006100), false),
    ("Bold red text", None, Some(0xC00000), true),
];

const BARS: [(&str, u32); 4] = [("Blue", 0x638EC6), ("Green", 0x63C384), ("Red", 0xFF555A), ("Orange", 0xFFB628)];

const SCALES: [(&str, &[u32]); 4] = [
    ("Green – yellow – red", &[0x63BE7B, 0xFFEB84, 0xF8696B]),
    ("Red – yellow – green", &[0xF8696B, 0xFFEB84, 0x63BE7B]),
    ("White – green", &[0xFCFCFF, 0x63BE7B]),
    ("Blue – white – red", &[0x5A8AC6, 0xFCFCFF, 0xF8696B]),
];

/// Rules made through the dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CfKind {
    Greater,
    Less,
    Between,
    Equal,
    TextContains,
    Duplicates,
    Top,
    Bottom,
    Formula,
}

impl CfKind {
    fn title(self) -> &'static str {
        match self {
            CfKind::Greater => "Format cells greater than",
            CfKind::Less => "Format cells less than",
            CfKind::Between => "Format cells between",
            CfKind::Equal => "Format cells equal to",
            CfKind::TextContains => "Format cells that contain the text",
            CfKind::Duplicates => "Format duplicate values",
            CfKind::Top => "Format the top items",
            CfKind::Bottom => "Format the bottom items",
            CfKind::Formula => "Format cells where this formula is true",
        }
    }

    fn fields(self) -> usize {
        match self {
            CfKind::Duplicates => 0,
            CfKind::Between => 2,
            _ => 1,
        }
    }
}

pub(crate) struct CfDialog {
    pub kind: CfKind,
    pub range: Range,
    pub value1: String,
    pub value2: String,
    pub preset: usize,
}

impl Spreadsheet {
    pub(crate) fn open_cf_dialog(&mut self, kind: CfKind) {
        self.menu = None;
        let range = self.selection();
        let value1 = match kind {
            CfKind::Top | CfKind::Bottom => "10".to_string(),
            CfKind::Formula => "=".to_string(),
            _ => String::new(),
        };
        self.cf_dialog = Some(CfDialog { kind, range, value1, value2: String::new(), preset: 0 });
        self.field = (kind.fields() > 0).then_some(Field::Cf1);
    }

    /// Add a rule for the selection (one undo step).
    pub(crate) fn add_cond_format(&mut self, rule: CfRule) {
        let range = self.selection();
        self.menu = None;
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                // New rules take priority over older ones, like Excel.
                l.cond.insert(0, CondFormat::new(vec![range], rule));
            }
        });
    }

    /// Remove rules from the selection (shrinking none: rules that touch it go).
    pub(crate) fn clear_cond_formats(&mut self, whole_sheet: bool) {
        let sel = self.selection();
        self.menu = None;
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                l.cond.retain(|cf| {
                    !whole_sheet && !cf.ranges.iter().any(|r| r.start.row <= sel.end.row && sel.start.row <= r.end.row && r.start.col <= sel.end.col && sel.start.col <= r.end.col)
                });
            }
        });
    }

    /// A value typed in the dialog as an expression: numbers stay numbers,
    /// "=…" is a formula, anything else is text.
    fn cf_expr(&self, text: &str) -> Result<Arc<xl_engine::formula::Expr>, String> {
        let t = text.trim();
        let src = if let Some(f) = t.strip_prefix('=') {
            f.to_string()
        } else if t.parse::<f64>().is_ok() {
            t.to_string()
        } else {
            format!("\"{}\"", t.replace('"', "\"\""))
        };
        parse_formula(&src, self.wb.sheets()).map(Arc::new).map_err(|e| e.to_string())
    }

    pub(crate) fn apply_cf_dialog(&mut self) {
        let Some(d) = self.cf_dialog.take() else { return };
        self.field = None;
        let (_, fill, color, bold) = PRESETS[d.preset.min(PRESETS.len() - 1)];
        let style = CfStyle { fill, color, bold, italic: false };
        let rule = (|| -> Result<CfRule, String> {
            Ok(match d.kind {
                CfKind::Greater => CfRule::Cell { op: CfOp::Greater, a: self.cf_expr(&d.value1)?, b: None, style },
                CfKind::Less => CfRule::Cell { op: CfOp::Less, a: self.cf_expr(&d.value1)?, b: None, style },
                CfKind::Equal => CfRule::Cell { op: CfOp::Equal, a: self.cf_expr(&d.value1)?, b: None, style },
                CfKind::Between => CfRule::Cell { op: CfOp::Between, a: self.cf_expr(&d.value1)?, b: Some(self.cf_expr(&d.value2)?), style },
                CfKind::TextContains => CfRule::Text { kind: TextMatch::Contains, text: d.value1.clone(), style },
                CfKind::Duplicates => CfRule::Duplicates { unique: false, style },
                CfKind::Top | CfKind::Bottom => {
                    let n = d.value1.trim().parse::<u32>().map_err(|_| "Enter a whole number of items".to_string())?;
                    CfRule::Top { n: n.max(1), percent: false, bottom: d.kind == CfKind::Bottom, style }
                }
                CfKind::Formula => {
                    let src = d.value1.trim();
                    let src = src.strip_prefix('=').unwrap_or(src);
                    CfRule::Formula { expr: parse_formula(src, self.wb.sheets()).map(Arc::new).map_err(|e| e.to_string())?, style }
                }
            })
        })();
        match rule {
            Ok(rule) => {
                let keep = (self.anchor, self.active);
                self.anchor = d.range.start;
                self.active = d.range.end;
                self.add_cond_format(rule);
                (self.anchor, self.active) = keep;
            }
            Err(e) => {
                self.message = Some((e, true));
                self.cf_dialog = Some(d);
                self.field = Some(Field::Cf1);
            }
        }
    }

    pub(crate) fn key_cf_dialog(&mut self, key: &str) -> bool {
        let Some(d) = self.cf_dialog.as_mut() else { return false };
        match key {
            "enter" => self.apply_cf_dialog(),
            "escape" => {
                self.cf_dialog = None;
                self.field = None;
            }
            "tab" if d.kind.fields() == 2 => self.field = Some(if self.field == Some(Field::Cf1) { Field::Cf2 } else { Field::Cf1 }),
            "backspace" => {
                match self.field {
                    Some(Field::Cf1) => d.value1.pop(),
                    Some(Field::Cf2) => d.value2.pop(),
                    _ => None,
                };
            }
            // Characters reach the focused field through the input handler.
            _ if self.field.is_some() && (key.chars().count() == 1 || key == "space") => return false,
            _ => {}
        }
        // The dialog is modal: nothing else reaches the grid.
        true
    }

    pub(crate) fn render_cf_menu(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let t = self.theme;
        let item = |id: &'static str, label: &'static str, kind: CfKind| {
            menu_item(&t, id, label, "").on_click(cx.listener(move |this, _, _, cx| {
                this.open_cf_dialog(kind);
                cx.notify();
            }))
        };
        let preset_rule = |id: &'static str, label: &'static str, rule: fn() -> CfRule| {
            menu_item(&t, id, label, "").on_click(cx.listener(move |this, _, _, cx| {
                this.add_cond_format(rule());
                cx.notify();
            }))
        };
        let heading = |text: &'static str| div().px(px(8.)).pt(px(6.)).pb(px(2.)).text_size(px(12.)).text_color(t.ink_faint).child(text);
        let mut bars = div().flex().flex_row().gap(px(6.)).px(px(8.)).py(px(4.));
        for (i, (name, color)) in BARS.iter().enumerate() {
            let color = *color;
            bars = bars.child(
                div()
                    .id(("cf-bar", i))
                    .w(px(44.))
                    .h(px(18.))
                    .rounded(px(3.))
                    .border_1()
                    .border_color(t.divider)
                    .cursor_pointer()
                    .hover(|s| s.border_color(t.accent))
                    .child(div().w(px(30.)).h_full().rounded(px(2.)).bg(gpui::rgb(color)))
                    .tooltip(crate::chrome::tip(t, name, ""))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.add_cond_format(CfRule::DataBar { color, min: CfValue::Min, max: CfValue::Max });
                        cx.notify();
                    })),
            );
        }
        let mut scales = div().flex().flex_row().gap(px(6.)).px(px(8.)).py(px(4.));
        for (i, (name, colors)) in SCALES.iter().enumerate() {
            let colors: Vec<u32> = colors.to_vec();
            let mut swatch = div().id(("cf-scale", i)).flex().flex_row().w(px(44.)).h(px(18.)).rounded(px(3.)).overflow_hidden().border_1().border_color(t.divider).cursor_pointer();
            for &c in &colors {
                swatch = swatch.child(div().flex_1().h_full().bg(gpui::rgb(c)));
            }
            scales = scales.child(swatch.hover(|s| s.border_color(t.accent)).tooltip(crate::chrome::tip(t, name, "")).on_click(cx.listener(move |this, _, _, cx| {
                let stops = match colors.as_slice() {
                    [lo, hi] => vec![(CfValue::Min, *lo), (CfValue::Max, *hi)],
                    [lo, mid, hi, ..] => vec![(CfValue::Min, *lo), (CfValue::Percentile(50.0), *mid), (CfValue::Max, *hi)],
                    _ => Vec::new(),
                };
                this.add_cond_format(CfRule::ColorScale { stops });
                cx.notify();
            })));
        }
        menu_panel(&t, 250.)
            .child(heading("Highlight cells"))
            .child(item("cf-gt", "Greater than…", CfKind::Greater))
            .child(item("cf-lt", "Less than…", CfKind::Less))
            .child(item("cf-between", "Between…", CfKind::Between))
            .child(item("cf-eq", "Equal to…", CfKind::Equal))
            .child(item("cf-text", "Text that contains…", CfKind::TextContains))
            .child(item("cf-dup", "Duplicate values…", CfKind::Duplicates))
            .child(menu_separator(&t))
            .child(heading("Top and bottom"))
            .child(item("cf-top", "Top items…", CfKind::Top))
            .child(item("cf-bottom", "Bottom items…", CfKind::Bottom))
            .child(preset_rule("cf-above", "Above average", || CfRule::Average { above: true, style: CfStyle { fill: Some(0xC6EFCE), color: Some(0x006100), ..CfStyle::default() } }))
            .child(preset_rule("cf-below", "Below average", || CfRule::Average { above: false, style: CfStyle { fill: Some(0xFFC7CE), color: Some(0x9C0006), ..CfStyle::default() } }))
            .child(menu_separator(&t))
            .child(heading("Data bars"))
            .child(bars)
            .child(heading("Color scales"))
            .child(scales)
            .child(menu_separator(&t))
            .child(item("cf-formula", "Use a formula…", CfKind::Formula))
            .child(menu_item(&t, "cf-clear", "Clear rules from selection", "").on_click(cx.listener(|this, _, _, cx| {
                this.clear_cond_formats(false);
                cx.notify();
            })))
            .child(menu_item(&t, "cf-clear-all", "Clear rules from sheet", "").on_click(cx.listener(|this, _, _, cx| {
                this.clear_cond_formats(true);
                cx.notify();
            })))
    }

    pub(crate) fn render_cf_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let Some(d) = &self.cf_dialog else { return div().into_any_element() };
        let input = |id: &'static str, field: Field, text: &str| {
            let focused = self.field == Some(field);
            div()
                .id(id)
                .flex_1()
                .h(px(30.))
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
                .text_color(t.ink)
                .child(SharedString::from(text.to_string()))
                .when(focused, |d| d.child(div().w(px(1.5)).h(px(16.)).bg(t.accent)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        this.focus.focus(window, cx);
                        this.field = Some(field);
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
        };
        let mut fields = div().flex().flex_row().items_center().gap(px(8.));
        if d.kind.fields() >= 1 {
            fields = fields.child(input("cf-value1", Field::Cf1, &d.value1));
        }
        if d.kind.fields() == 2 {
            fields = fields.child(div().text_color(t.ink_muted).child("and")).child(input("cf-value2", Field::Cf2, &d.value2));
        }
        let mut presets = div().flex().flex_col().gap(px(2.));
        for (i, (label, fill, color, bold)) in PRESETS.iter().enumerate() {
            let on = d.preset == i;
            presets = presets.child(
                div()
                    .id(("cf-preset", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .h(px(28.))
                    .px(px(6.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(t.header_selected))
                    .hover(|s| s.bg(t.chrome_hover))
                    .child(
                        div()
                            .w(px(64.))
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.))
                            .border_1()
                            .border_color(t.divider)
                            .when_some(*fill, |d, f| d.bg(gpui::rgb(f)))
                            .text_size(px(12.))
                            .text_color(color.map_or(t.ink, |c| gpui::rgb(c).into()))
                            .when(*bold, |d| d.font_weight(FontWeight::BOLD))
                            .child("AaBb"),
                    )
                    .child(div().text_size(px(13.5)).text_color(t.ink).child(*label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(d) = this.cf_dialog.as_mut() {
                            d.preset = i;
                        }
                        cx.notify();
                    })),
            );
        }
        let button = |id: &'static str, label: &'static str| {
            div().id(id).flex().items_center().justify_center().h(px(32.)).px(px(14.)).rounded(px(4.)).cursor_pointer().text_size(px(14.)).child(label)
        };
        div()
            .id("cf-scrim")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::hsla(0., 0., 0., if t.dark { 0.45 } else { 0.25 }))
            .occlude()
            .child(
                div()
                    .id("cf-dialog")
                    .w(px(440.))
                    .p(px(20.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .rounded(px(8.))
                    .bg(t.popover)
                    .border_1()
                    .border_color(t.divider)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child(d.kind.title()))
                    .child(div().text_size(px(13.)).text_color(t.ink_muted).child(SharedString::from(format!("Applies to {}", d.range))))
                    .when(d.kind.fields() > 0, |el| el.child(fields))
                    .child(div().text_size(px(13.)).text_color(t.ink_muted).child("with"))
                    .child(presets)
                    .child(
                        div()
                            .mt(px(6.))
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .child(div().flex_1())
                            .child(button("cf-cancel", "Cancel").text_color(t.ink).border_1().border_color(t.divider).hover(|s| s.bg(t.chrome_hover)).on_click(cx.listener(|this, _, _, cx| {
                                this.cf_dialog = None;
                                this.field = None;
                                cx.notify();
                            })))
                            .child(button("cf-ok", "OK").bg(t.accent).text_color(t.on_accent).font_weight(FontWeight::SEMIBOLD).on_click(cx.listener(|this, _, _, cx| {
                                this.apply_cf_dialog();
                                cx.notify();
                            }))),
                    ),
            )
            .into_any_element()
    }
}
