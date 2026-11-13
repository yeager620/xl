//! Everything around the grid: toolbar, formula bar, sheet tabs, status bar.

use gpui::{
    AnyElement, Context, ExternalPaths, FontWeight, Hsla, InteractiveElement, IntoElement, MouseButton, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, canvas, div, prelude::FluentBuilder, px,
};
use xl_engine::quote_sheet_name;

use crate::app::{EditMode, Spreadsheet};
use crate::paint::paint_ops;
use crate::refs::scan_refs;
use crate::theme::Theme;

fn kbd(t: &Theme, keys: &'static str, label: &'static str) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(5.))
        .text_size(px(12.))
        .text_color(t.muted)
        .child(div().px(px(5.)).py(px(1.)).rounded(px(4.)).border_1().border_color(t.border).text_size(px(10.5)).child(keys))
        .child(label)
}

fn segment(t: &Theme, id: &'static str, label: &'static str, active: bool, tip_left: bool, tip_right: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(26.))
        .px(px(9.))
        .text_size(px(12.))
        .cursor_pointer()
        .when(tip_left, |d| d.rounded_l(px(6.)))
        .when(tip_right, |d| d.rounded_r(px(6.)))
        .when(!tip_left, |d| d.border_l_1().border_color(t.border))
        .when(active, |d| d.bg(t.header_active_bg).text_color(t.header_active_fg).font_weight(FontWeight::MEDIUM))
        .when(!active, |d| d.text_color(t.muted).hover(|s| s.bg(t.header_bg)))
        .child(label)
}

impl Spreadsheet {
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let fmt = self.format_label();
        let formats: [(&'static str, &'static str, Option<&'static str>, &[&str]); 5] = [
            ("fmt-general", "General", None, &["General"]),
            ("fmt-number", "1,234", Some("#,##0.00"), &["Number", "Scientific"]),
            ("fmt-currency", "$", Some("$#,##0.00"), &["Currency"]),
            ("fmt-percent", "%", Some("0%"), &["Percent"]),
            ("fmt-date", "Date", Some("m/d/yyyy"), &["Date", "Time"]),
        ];
        let n = formats.len();
        let group = div().flex().flex_row().items_center().rounded(px(6.)).border_1().border_color(t.border).children(formats.into_iter().enumerate().map(
            |(i, (id, label, code, kinds))| {
                segment(&t, id, label, kinds.contains(&fmt), i == 0, i == n - 1).on_click(cx.listener(move |this, _, _, cx| {
                    this.apply_format(code);
                    cx.notify();
                }))
            },
        ));
        let decimals = div()
            .flex()
            .flex_row()
            .items_center()
            .rounded(px(6.))
            .border_1()
            .border_color(t.border)
            .child(segment(&t, "dec-less", ".0←", false, true, false).on_click(cx.listener(|this, _, _, cx| {
                this.bump_decimals(-1);
                cx.notify();
            })))
            .child(segment(&t, "dec-more", ".00→", false, false, true).on_click(cx.listener(|this, _, _, cx| {
                this.bump_decimals(1);
                cx.notify();
            })));

        div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(44.))
            .px(px(14.))
            .gap(px(10.))
            .bg(t.surface)
            .border_b_1()
            .border_color(t.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(22.))
                    .rounded(px(6.))
                    .bg(t.accent)
                    .text_color(t.accent_fg)
                    .text_size(px(11.))
                    .font_weight(FontWeight::BOLD)
                    .child("xl"),
            )
            .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(13.5)).child(self.file_name()))
            .when(self.dirty, |d| d.child(div().size(px(6.)).rounded_full().bg(t.muted)))
            .child(div().w(px(10.)))
            .child(group)
            .child(decimals)
            .child(div().flex_1())
            .child(kbd(&t, "Ctrl+O", "Open"))
            .child(kbd(&t, "Ctrl+S", "Save"))
            .child(
                div()
                    .id("theme-toggle")
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(26.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(t.border)
                    .text_size(px(12.))
                    .text_color(t.muted)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.header_bg))
                    .child(if t.dark { "Light" } else { "Dark" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_theme();
                        cx.notify();
                    })),
            )
    }

    fn render_formula_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let sel = self.selection();
        let name_box = match &self.edit {
            Some(e) if e.at.sheet != self.cur => format!("{}!{}", quote_sheet_name(self.wb.sheet_name(e.at.sheet)), e.at.cell),
            Some(e) => e.at.cell.to_string(),
            None if sel.start == sel.end => self.active.to_string(),
            None => format!("{}R × {}C", sel.rows(), sel.cols()),
        };
        let (text, caret) = match &self.edit {
            Some(e) => (e.text.clone(), Some(e.cursor)),
            None => (self.wb.input_text(self.at(self.active)), None),
        };

        // Color references; split at the caret.
        let mut segments: Vec<(String, Hsla)> = Vec::new();
        let mut pos = 0;
        let mut spans: Vec<(usize, usize, Hsla)> = scan_refs(&text).iter().map(|r| (r.span.start, r.span.end, t.ref_colors[r.color % t.ref_colors.len()])).collect();
        spans.sort_by_key(|s| s.0);
        for (s, e, col) in spans {
            if s > pos {
                segments.push((text[pos..s].to_string(), t.text));
            }
            segments.push((text[s..e].to_string(), col));
            pos = e;
        }
        if pos < text.len() {
            segments.push((text[pos..].to_string(), t.text));
        }
        let mut children: Vec<AnyElement> = Vec::new();
        let caret_el = || div().w(px(1.5)).h(px(16.)).bg(t.accent).into_any_element();
        let seg_el = |s: &str, color: Hsla| div().text_color(color).child(s.replace('\u{2028}', "↵")).into_any_element();
        let mut offset = 0;
        let mut caret_done = caret.is_none();
        for (s, color) in &segments {
            let end = offset + s.len();
            match caret {
                Some(c) if !caret_done && c >= offset && c < end => {
                    let (a, b) = s.split_at(c - offset);
                    if !a.is_empty() {
                        children.push(seg_el(a, *color));
                    }
                    children.push(caret_el());
                    children.push(seg_el(b, *color));
                    caret_done = true;
                }
                _ => children.push(seg_el(s, *color)),
            }
            offset = end;
        }
        if !caret_done {
            children.push(caret_el());
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(36.))
            .bg(t.bg)
            .border_b_1()
            .border_color(t.border)
            .child(
                div()
                    .w(px(110.))
                    .h_full()
                    .flex()
                    .items_center()
                    .px(px(12.))
                    .border_r_1()
                    .border_color(t.border)
                    .text_size(px(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.muted)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(name_box),
            )
            .child(div().px(px(12.)).text_size(px(12.)).italic().text_color(if self.edit.is_some() { t.accent } else { t.muted }).child("fx"))
            .child(
                div()
                    .id("formula-bar")
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(13.))
                    .cursor_text()
                    .children(children)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.focus.focus(window, cx);
                            if this.edit.is_none() {
                                this.begin_edit(None, EditMode::Edit);
                            }
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            )
    }

    fn render_sheet_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let ids = self.wb.sheet_ids().to_vec();
        let many = ids.len() > 1;
        let tabs = ids.into_iter().map(|id| {
            let active = id == self.cur;
            let renaming = self.renaming.as_ref().filter(|(r, _)| *r == id).map(|(_, n)| n.clone());
            let confirming = self.confirm_delete == Some(id);
            let label: AnyElement = match renaming {
                Some(name) => div()
                    .flex()
                    .items_center()
                    .min_w(px(40.))
                    .px(px(4.))
                    .rounded(px(3.))
                    .bg(t.editor_bg)
                    .border_1()
                    .border_color(t.accent)
                    .child(SharedString::from(name))
                    .child(div().w(px(1.5)).h(px(14.)).bg(t.accent))
                    .into_any_element(),
                None => div().child(SharedString::from(self.wb.sheet_name(id).to_string())).into_any_element(),
            };
            div()
                .id(("sheet-tab", id as usize))
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(26.))
                .px(px(12.))
                .rounded(px(6.))
                .text_size(px(12.5))
                .cursor_pointer()
                .when(active, |d| d.bg(t.bg).border_1().border_color(t.border).text_color(t.text).font_weight(FontWeight::MEDIUM))
                .when(!active, |d| d.text_color(t.muted).hover(|s| s.bg(t.header_bg)))
                .child(label)
                .when(active && many && self.renaming.is_none(), |d| {
                    d.child(
                        div()
                            .id(("sheet-del", id as usize))
                            .flex()
                            .items_center()
                            .justify_center()
                            .h(px(16.))
                            .px(px(if confirming { 6. } else { 3. }))
                            .rounded(px(4.))
                            .text_size(px(11.))
                            .when(confirming, |d| d.bg(t.error).text_color(t.accent_fg))
                            .when(!confirming, |d| d.text_color(t.muted).hover(|s| s.bg(t.header_bg).text_color(t.error)))
                            .child(if confirming { "Delete?" } else { "×" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.confirm_delete == Some(id) {
                                    this.delete_sheet(id);
                                } else {
                                    this.confirm_delete = Some(id);
                                }
                                cx.stop_propagation();
                                cx.notify();
                            })),
                    )
                })
                .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                    this.focus.focus(window, cx);
                    if this.renaming.as_ref().is_some_and(|(r, _)| *r != id) {
                        this.finish_rename(true);
                    }
                    if ev.click_count() >= 2 {
                        this.switch_sheet(id);
                        this.start_rename(id);
                    } else if this.renaming.is_none() {
                        this.switch_sheet(id);
                    }
                    cx.notify();
                }))
        });

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .h(px(36.))
            .px(px(8.))
            .bg(t.surface)
            .border_t_1()
            .border_color(t.border)
            .children(tabs)
            .child(
                div()
                    .id("add-sheet")
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(26.))
                    .rounded(px(6.))
                    .text_size(px(16.))
                    .text_color(t.muted)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.header_bg).text_color(t.text))
                    .child("+")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.focus.focus(window, cx);
                        this.add_sheet();
                        cx.notify();
                    })),
            )
    }

    fn render_status_bar(&self) -> impl IntoElement {
        let t = self.theme;
        let mode = match &self.edit {
            None if self.renaming.is_some() => "Rename",
            None => "Ready",
            Some(e) if e.can_point() && e.mode == EditMode::Enter => "Point",
            Some(e) if e.mode == EditMode::Enter => "Enter",
            Some(_) => "Edit",
        };
        let recalc = self.last_stats.map(|s| {
            let us = s.elapsed.as_secs_f64() * 1e6;
            let time = if us < 1000.0 { format!("{us:.0} µs") } else { format!("{:.1} ms", us / 1000.0) };
            format!("Recalc {time} · {} cells", s.evaluated)
        });
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.))
            .h(px(28.))
            .px(px(12.))
            .bg(t.surface)
            .border_t_1()
            .border_color(t.border)
            .text_size(px(11.5))
            .text_color(t.muted)
            .child(div().px(px(7.)).py(px(1.)).rounded(px(4.)).bg(t.header_active_bg).text_color(t.header_active_fg).font_weight(FontWeight::MEDIUM).child(mode))
            .when_some(self.message.clone(), |d, (msg, err)| d.child(div().text_color(if err { t.error } else { t.muted }).child(msg)))
            .child(div().flex_1())
            .when_some(self.selection_summary(), |d, s| d.child(div().text_color(t.text).child(s)))
            .child(self.format_label())
            .when_some(recalc, |d, s| d.child(s))
    }
}

impl Render for Spreadsheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.title();
        if title != self.last_title {
            window.set_window_title(&title);
            self.last_title = title;
        }
        let t = self.theme;
        let this = cx.entity();
        let grid = canvas(
            {
                let this = this.clone();
                move |bounds, _window, cx| {
                    this.update(cx, |s, _| {
                        s.viewport = bounds;
                        s.scroll_x = s.scroll_x.min(s.max_scroll_x());
                        s.scroll_y = s.scroll_y.min(s.max_scroll_y());
                    });
                }
            },
            move |bounds, (), window, cx| {
                let ops = this.read(cx).build_frame(bounds, window);
                paint_ops(ops, window, cx);
            },
        )
        .size_full();

        div()
            .key_context("Spreadsheet")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| this.on_drop_files(paths, window, cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .text_color(t.text)
            .font_family("Inter")
            .text_size(px(13.))
            .child(self.render_toolbar(cx))
            .child(self.render_formula_bar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .relative()
                    .overflow_hidden()
                    .child(grid)
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_scroll(cx.listener(Self::on_scroll)),
            )
            .child(self.render_sheet_tabs(cx))
            .child(self.render_status_bar())
    }
}
