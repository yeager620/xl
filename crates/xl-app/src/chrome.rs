//! Everything around the grid: title bar (toolbar), formula bar, bottom bar
//! (sheet tabs + selection summary), menus, tooltips and the window frame.

use gpui::{
    AnyElement, AnyView, App, AppContext, Context, CursorStyle, Decorations, ExternalPaths, FontWeight, Hsla, HitboxBehavior, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Pixels, Point, Render, ResizeEdge, SharedString, Size, StatefulInteractiveElement, Styled, Window,
    canvas, deferred, div, prelude::FluentBuilder, px, rgb, svg,
};
use xl_engine::{HAlign, NumberFormat, Rgb, Value, quote_sheet_name};

use crate::app::{BorderPreset, ContextKind, EditMode, Emphasis, Field, Menu, Spreadsheet};
use crate::paint::paint_ops;
use crate::refs::scan_refs;
use crate::theme::Theme;

const TITLE_H: f32 = 40.0;
const FORMULA_H: f32 = 30.0;
const BOTTOM_H: f32 = 30.0;
const ICON: f32 = 16.0;
const RADIUS: f32 = 4.0;

/// Swatches: strong colors, then light tints.
const SWATCHES: [[Rgb; 8]; 2] = [
    [0x000000, 0x6A6F78, 0xC4271B, 0xD9730D, 0xB7950B, 0x2E7D32, 0x1F5FBF, 0x6A3FA0],
    [0xFFFFFF, 0xE4E6E9, 0xF6D5D2, 0xFBE3CC, 0xFFF1C2, 0xD9EDDA, 0xD6E3F7, 0xE6DCF2],
];

/// Number formats offered in the format menu: (name, code, sample value is a date?).
const NUMBER_FORMATS: [(&str, Option<&str>, bool); 10] = [
    ("General", None, false),
    ("Number", Some("#,##0.00"), false),
    ("Currency", Some("$#,##0.00"), false),
    ("Accounting", Some("_($* #,##0.00_);_($* (#,##0.00);_($* \"-\"??_);_(@_)"), false),
    ("Percent", Some("0.00%"), false),
    ("Scientific", Some("0.00E+00"), false),
    ("Short date", Some("m/d/yyyy"), true),
    ("Long date", Some("dddd, mmmm d, yyyy"), true),
    ("Time", Some("h:mm AM/PM"), true),
    ("Text", Some("@"), false),
];

/// Width of the invisible border used for resizing (and the shadow) when we
/// draw our own window frame.
const FRAME: f32 = 10.0;
const CORNER: f32 = 10.0;

fn resize_edge(pos: Point<Pixels>, inset: Pixels, size: Size<Pixels>) -> Option<ResizeEdge> {
    let (left, right) = (pos.x < inset, pos.x > size.width - inset);
    let (top, bottom) = (pos.y < inset, pos.y > size.height - inset);
    Some(match (top, bottom, left, right) {
        (true, _, true, _) => ResizeEdge::TopLeft,
        (true, _, _, true) => ResizeEdge::TopRight,
        (_, true, true, _) => ResizeEdge::BottomLeft,
        (_, true, _, true) => ResizeEdge::BottomRight,
        (true, ..) => ResizeEdge::Top,
        (_, true, ..) => ResizeEdge::Bottom,
        (_, _, true, _) => ResizeEdge::Left,
        (_, _, _, true) => ResizeEdge::Right,
        _ => return None,
    })
}

fn resize_cursor(edge: ResizeEdge) -> CursorStyle {
    match edge {
        ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
        ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
    }
}

/// Keep clicks on interactive toolbar content from reaching the title bar:
/// otherwise a mouse-down arms "drag to move", and the slightest wobble during
/// the click hands the pointer to the compositor (swallowing the click), and
/// two quick clicks on different buttons count as a title-bar double-click.
fn no_drag<E: InteractiveElement>(e: E) -> E {
    e.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()).on_mouse_move(|_, _, cx| cx.stop_propagation())
}

pub(crate) fn icon(name: &'static str, color: Hsla) -> impl IntoElement {
    svg().path(format!("icons/{name}.svg")).size(px(ICON)).flex_none().text_color(color)
}

// ---------------------------------------------------------------- tooltips

struct Tip {
    text: SharedString,
    keys: Option<SharedString>,
    theme: Theme,
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        div()
            .flex()
            .flex_row()
            .gap(px(8.))
            .px(px(8.))
            .py(px(4.))
            .rounded(px(RADIUS))
            .bg(t.popover)
            .border_1()
            .border_color(t.divider)
            .shadow_md()
            .font_family(crate::FONT)
            .text_size(px(12.5))
            .text_color(t.ink)
            .child(self.text.clone())
            .when_some(self.keys.clone(), |d, k| d.child(div().text_color(t.ink_muted).child(k)))
    }
}

pub(crate) fn tip(t: Theme, text: impl Into<SharedString>, keys: &'static str) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |_, cx| cx.new(|_| Tip { text: text.clone(), keys: (!keys.is_empty()).then(|| keys.into()), theme: t }).into()
}

// ----------------------------------------------------------------- buttons

fn tool_button(t: &Theme, id: &'static str, active: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .debug_selector(|| id.to_string())
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(2.))
        .h(px(28.))
        .min_w(px(28.))
        .px(px(6.))
        .rounded(px(RADIUS))
        .cursor_pointer()
        .when(active, |d| d.bg(t.chrome_active))
        .when(!active, |d| d.hover(|s| s.bg(t.chrome_hover)))
}

fn divider(t: &Theme) -> impl IntoElement {
    div().w(px(1.)).h(px(18.)).mx(px(6.)).bg(t.divider)
}

fn window_button(t: &Theme, id: &'static str, name: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .debug_selector(|| id.to_string())
        .flex()
        .items_center()
        .justify_center()
        .size(px(24.))
        .rounded_full()
        .bg(t.chrome_hover)
        .cursor_pointer()
        .hover(|s| s.bg(t.chrome_active))
        .child(svg().path(format!("icons/{name}.svg")).size(px(14.)).text_color(t.ink))
}

pub(crate) fn menu_panel(t: &Theme, width: f32) -> gpui::Stateful<gpui::Div> {
    div()
        .id("menu-panel")
        .occlude()
        .mt(px(4.))
        .w(px(width))
        .p(px(4.))
        .flex()
        .flex_col()
        .rounded(px(6.))
        .bg(t.popover)
        .border_1()
        .border_color(t.divider)
        .shadow_lg()
        .text_size(px(14.))
        .text_color(t.ink)
        .whitespace_nowrap()
}

pub(crate) fn menu_item(t: &Theme, id: impl Into<gpui::ElementId>, label: impl Into<SharedString>, detail: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(16.))
        .h(px(28.))
        .px(px(8.))
        .rounded(px(RADIUS))
        .cursor_pointer()
        .hover(|s| s.bg(t.chrome_hover))
        .child(div().child(label.into()))
        .child(div().text_color(t.ink_muted).child(detail.into()))
}

pub(crate) fn menu_separator(t: &Theme) -> impl IntoElement {
    div().h(px(1.)).mx(px(4.)).my(px(4.)).bg(t.divider)
}

impl Spreadsheet {
    // ----------------------------------------------------------- title bar

    fn render_window_controls(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let controls = window.window_controls();
        no_drag(div())
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .ml(px(10.))
            .when(controls.minimize, |d| {
                d.child(window_button(&t, "win-min", "minus").on_click(|_, window, cx| {
                    cx.stop_propagation();
                    window.minimize_window();
                }))
            })
            .when(controls.maximize, |d| {
                let name = if window.is_maximized() { "copy" } else { "square" };
                d.child(window_button(&t, "win-max", name).on_click(|_, window, cx| {
                    cx.stop_propagation();
                    window.zoom_window();
                }))
            })
            .child(window_button(&t, "win-close", "x").on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.request(crate::lifecycle::After::Close, window, cx);
            })))
    }

    /// A toolbar button that opens a menu, with the menu attached below it.
    fn menu_button(&self, id: &'static str, m: Menu, tooltip: &'static str, content: AnyElement, cx: &mut Context<Self>) -> impl IntoElement {
        self.menu_button_inner(id, m, tooltip, content, true, cx)
    }

    fn menu_button_inner(&self, id: &'static str, m: Menu, tooltip: &'static str, content: AnyElement, chevron: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let open = self.menu == Some(m);
        div()
            .relative()
            .child(
                tool_button(&t, id, open)
                    .child(content)
                    .when(chevron, |d| d.child(icon("chevron-down", t.ink_faint)))
                    .when(!open, |d| d.tooltip(tip(t, tooltip, "")))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = if this.menu == Some(m) { None } else { Some(m) };
                        cx.notify();
                    })),
            )
            .when(open, |d| {
                let panel = div().absolute().top(px(30.)).child(self.render_menu(m, cx));
                let panel = if m == Menu::Main { panel.right_0() } else { panel.left_0() };
                d.child(deferred(panel).with_priority(2))
            })
    }

    /// The selection's font, in its own face; opens the font picker.
    fn font_button(&self, st: &xl_engine::Style, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let (label, family, missing): (SharedString, SharedString, bool) = match st.font_name.as_deref() {
            None => ("Default".into(), crate::FONT.into(), false),
            Some(n) => {
                let r = crate::fonts::resolve(n);
                (n.to_string().into(), r.family, r.substituted)
            }
        };
        let tooltip: SharedString = if missing { format!("Font: {label} isn't installed (shown as {family})").into() } else { "Font".into() };
        tool_button(&t, "font-name", false)
            .child(
                div()
                    .w(px(112.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_family(family)
                    .text_size(px(14.))
                    .text_color(if missing { t.ink_muted } else { t.ink })
                    .when(missing, |d| d.italic())
                    .child(label),
            )
            .child(icon("chevron-down", t.ink_faint))
            .tooltip(tip(t, tooltip, "Ctrl+Shift+F"))
            .on_click(cx.listener(|this, _, _, cx| {
                this.open_font_picker();
                cx.notify();
            }))
    }

    fn render_toolbar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let client_frame = matches!(window.window_decorations(), Decorations::Client { .. });
        let st = self.wb.style(self.at(self.active)).clone();

        let emph = |id: &'static str, name: &'static str, on: bool, e: Emphasis, label: &'static str, keys: &'static str, cx: &mut Context<Self>| {
            tool_button(&t, id, on).child(icon(name, t.ink)).tooltip(tip(t, label, keys)).on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_emphasis(e);
                cx.notify();
            }))
        };
        let format_label = self.format_label();
        let text_color = st.color.map_or(t.ink, |c| rgb(c).into());

        let tools = no_drag(div())
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.))
            .child(tool_button(&t, "undo", false).child(icon("undo-2", t.ink)).tooltip(tip(t, "Undo", "Ctrl+Z")).on_click(cx.listener(|this, _, _, cx| {
                this.undo_redo(true);
                cx.notify();
            })))
            .child(tool_button(&t, "redo", false).child(icon("redo-2", t.ink)).tooltip(tip(t, "Redo", "Ctrl+Y")).on_click(cx.listener(|this, _, _, cx| {
                this.undo_redo(false);
                cx.notify();
            })))
            .child(divider(&t))
            .child(self.menu_button(
                "number-format",
                Menu::NumberFormat,
                "Number format",
                div().min_w(px(72.)).text_size(px(14.)).text_color(t.ink).child(format_label).into_any_element(),
                cx,
            ))
            .child(tool_button(&t, "fmt-currency", format_label == "Currency").child(icon("dollar-sign", t.ink)).tooltip(tip(t, "Currency", "Ctrl+Shift+4")).on_click(
                cx.listener(|this, _, _, cx| {
                    this.apply_format(Some("$#,##0.00"));
                    cx.notify();
                }),
            ))
            .child(tool_button(&t, "fmt-percent", format_label == "Percent").child(icon("percent", t.ink)).tooltip(tip(t, "Percent", "Ctrl+Shift+5")).on_click(
                cx.listener(|this, _, _, cx| {
                    this.apply_format(Some("0%"));
                    cx.notify();
                }),
            ))
            .child(divider(&t))
            .child(self.font_button(&st, cx))
            .child(self.menu_button(
                "font-size",
                Menu::FontSize,
                "Font size",
                div().min_w(px(20.)).text_size(px(14.)).text_color(t.ink).child(SharedString::from(format!("{}", st.font_size.map_or(11.0, |t| t as f64 / 10.0)))).into_any_element(),
                cx,
            ))
            .child(emph("bold", "bold", st.bold, Emphasis::Bold, "Bold", "Ctrl+B", cx))
            .child(emph("italic", "italic", st.italic, Emphasis::Italic, "Italic", "Ctrl+I", cx))
            .child(emph("underline", "underline", st.underline, Emphasis::Underline, "Underline", "Ctrl+U", cx))
            .child(divider(&t))
            .child(self.menu_button(
                "text-color",
                Menu::TextColor,
                "Text color",
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(icon("baseline", t.ink))
                    .child(div().mt(px(-3.)).w(px(14.)).h(px(3.)).bg(text_color))
                    .into_any_element(),
                cx,
            ))
            .child(self.menu_button(
                "fill-color",
                Menu::Fill,
                "Fill color",
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(icon("paint-bucket", t.ink))
                    .child(div().mt(px(-3.)).w(px(14.)).h(px(3.)).when_some(st.fill, |d, f| d.bg(rgb(f))).when(st.fill.is_none(), |d| d.border_1().border_color(t.ink_faint)))
                    .into_any_element(),
                cx,
            ))
            .child(divider(&t))
            .child(self.menu_button(
                "align",
                Menu::Align,
                "Alignment",
                icon(
                    match st.align {
                        HAlign::Center => "align-center",
                        HAlign::Right => "align-right",
                        _ => "align-left",
                    },
                    t.ink,
                )
                .into_any_element(),
                cx,
            ))
            .child(self.menu_button("borders", Menu::Borders, "Borders", icon("grid-2x2", t.ink).into_any_element(), cx))
            .child(self.menu_button("cond-format", Menu::CondFormat, "Conditional formatting", icon("highlighter", t.ink).into_any_element(), cx))
            .child(self.menu_button("chart", Menu::Chart, "Insert chart", icon("chart-column", t.ink).into_any_element(), cx));

        let file = no_drag(div()).flex().flex_row().items_center().gap(px(2.)).child(
            tool_button(&t, "palette", false).child(icon("search", t.ink)).tooltip(tip(t, "Search commands", "Ctrl+K")).on_click(cx.listener(|this, _, _, cx| {
                this.open_palette();
                cx.notify();
            })),
        );
        let main_menu = no_drag(div()).child(self.menu_button_inner("main-menu", Menu::Main, "Menu", icon("menu", t.ink).into_any_element(), false, cx));

        div()
            .id("titlebar")
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .h(px(TITLE_H))
            .pl(px(12.))
            .pr(px(8.))
            .gap(px(4.))
            .bg(t.chrome)
            .border_b_1()
            .border_color(t.divider)
            // Title-bar behavior: drag to move, double-click to maximize,
            // right-click for the window menu. A move only starts once the
            // mouse actually moves, so clicking buttons never drags.
            .when(client_frame, |d| {
                d.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| {
                        this.should_move = true;
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| {
                        this.should_move = false;
                    }),
                )
                .on_mouse_down_out(cx.listener(|this, _, _, _| {
                    this.should_move = false;
                }))
                .on_mouse_move(cx.listener(|this, _, window, _| {
                    if this.should_move {
                        this.should_move = false;
                        window.start_window_move();
                    }
                }))
                .on_click(|ev, window, _| {
                    if ev.is_right_click() {
                        window.show_window_menu(ev.position());
                    } else if ev.click_count() == 2 {
                        window.zoom_window();
                    }
                })
            })
            .child(icon(if self.home.is_some() { "house" } else { "sheet" }, t.ink_muted))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .ml(px(4.))
                    .mr(px(8.))
                    .max_w(px(220.))
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.ink)
                    .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(if self.home.is_some() { "Home".to_string() } else { self.file_name() }))
                    .when(self.dirty && self.home.is_none(), |d| {
                        // A quiet dot marks unsaved changes.
                        d.child(div().id("unsaved").flex_none().ml(px(6.)).size(px(7.)).rounded_full().bg(t.ink_faint).tooltip(tip(t, "Unsaved changes", "Ctrl+S")))
                    }),
            )
            .when(self.home.is_none(), |d| d.child(tools))
            .child(div().flex_1().h_full())
            .child(file)
            .child(main_menu)
            .when(client_frame, |d| d.child(self.render_window_controls(window, cx)))
    }

    fn render_menu(&self, m: Menu, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        match m {
            Menu::TextColor | Menu::Fill => {
                let is_fill = m == Menu::Fill;
                let reset = menu_item(&t, "reset-color", if is_fill { "No fill" } else { "Automatic" }, "").on_click(cx.listener(move |this, _, _, cx| {
                    if is_fill { this.set_fill(None) } else { this.set_text_color(None) }
                    cx.notify();
                }));
                let mut rows = Vec::new();
                for (ri, row) in SWATCHES.iter().enumerate() {
                    let mut swatches = Vec::new();
                    for (ci, &c) in row.iter().enumerate() {
                        swatches.push(
                            div()
                                .id(("swatch", ri * 8 + ci))
                                .size(px(20.))
                                .rounded(px(3.))
                                .bg(rgb(c))
                                .border_1()
                                .border_color(t.divider)
                                .cursor_pointer()
                                .hover(|s| s.border_color(t.accent))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if is_fill { this.set_fill(Some(c)) } else { this.set_text_color(Some(c)) }
                                    cx.notify();
                                })),
                        );
                    }
                    rows.push(div().flex().flex_row().gap(px(4.)).px(px(8.)).py(px(2.)).children(swatches));
                }
                menu_panel(&t, 208.).child(reset).child(div().h(px(4.))).children(rows).child(div().h(px(4.))).into_any_element()
            }
            Menu::Borders => {
                let presets = [
                    ("All borders", BorderPreset::All),
                    ("Outside borders", BorderPreset::Outside),
                    ("Bottom border", BorderPreset::Bottom),
                    ("Thick bottom border", BorderPreset::ThickBottom),
                    ("Top border", BorderPreset::Top),
                    ("No borders", BorderPreset::None),
                ];
                menu_panel(&t, 196.)
                    .children(presets.into_iter().enumerate().map(|(i, (label, p))| {
                        menu_item(&t, ("border", i), label, "").on_click(cx.listener(move |this, _, _, cx| {
                            this.apply_borders(p);
                            cx.notify();
                        }))
                    }))
                    .into_any_element()
            }
            Menu::Align => {
                let st = self.wb.style(self.at(self.active)).clone();
                let merged = self.wb.layout(self.cur).merge_at(self.active).is_some();
                let check = |on: bool| if on { "✓" } else { "" };
                let mut panel = menu_panel(&t, 200.);
                for (i, (label, a)) in [("Align left", HAlign::Left), ("Center", HAlign::Center), ("Align right", HAlign::Right)].into_iter().enumerate() {
                    panel = panel.child(menu_item(&t, ("align", i), label, check(st.align == a)).on_click(cx.listener(move |this, _, _, cx| {
                        this.set_align(a);
                        this.menu = None;
                        cx.notify();
                    })));
                }
                panel
                    .child(menu_separator(&t))
                    .child(menu_item(&t, "wrap", "Wrap text", check(st.wrap)).on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_wrap();
                        this.menu = None;
                        cx.notify();
                    })))
                    .child(menu_item(&t, "merge", if merged { "Unmerge cells" } else { "Merge and center" }, "").on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_merge(true);
                        this.menu = None;
                        cx.notify();
                    })))
                    .into_any_element()
            }
            Menu::FontSize => {
                let cur = self.wb.style(self.at(self.active)).font_size.map_or(11.0, |t| t as f64 / 10.0);
                menu_panel(&t, 120.)
                    .id("font-sizes")
                    .max_h(px(420.))
                    .overflow_y_scroll()
                    .children(crate::palette::FONT_SIZES.into_iter().enumerate().map(|(i, size)| {
                        menu_item(&t, ("font-size", i), format!("{size}"), if (size - cur).abs() < 0.05 { "✓" } else { "" }).on_click(cx.listener(move |this, _, _, cx| {
                            this.set_font_size(if size == 11.0 { None } else { Some(size) });
                            this.menu = None;
                            cx.notify();
                        }))
                    }))
                    .child(menu_separator(&t))
                    .child(menu_item(&t, "font-size-other", "Other size…", "").on_click(cx.listener(|this, _, _, cx| {
                        this.open_font_picker();
                        cx.notify();
                    })))
                    .into_any_element()
            }
            Menu::NumberFormat => {
                // Each format previews the active cell's value (or a sample).
                let a = self.at(self.active);
                let v = self.wb.value(a).clone();
                let is_date = self.wb.format(a).is_some_and(|f| f.is_date());
                let number = if v.is_number() { v.clone() } else { Value::Number(1234.5) };
                // Date formats preview the cell's date if it holds one, else now.
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64()) / 86_400.0 + 25_569.0;
                let date = if is_date { v } else { Value::Number(now) };
                menu_panel(&t, 300.)
                    .children(NUMBER_FORMATS.into_iter().enumerate().map(|(i, (name, code, is_date))| {
                        let sample = match code {
                            None => xl_engine::format_general(match &number {
                                Value::Number(n) => *n,
                                _ => 0.0,
                            }),
                            Some(c) => NumberFormat::parse(c).format(if is_date { &date } else { &number }).text,
                        };
                        menu_item(&t, ("numfmt", i), name, sample.trim().to_string()).on_click(cx.listener(move |this, _, _, cx| {
                            this.apply_format(code);
                            this.menu = None;
                            cx.notify();
                        }))
                    }))
                    .child(menu_separator(&t))
                    .child(menu_item(&t, "dec-more", "More decimal places", "").on_click(cx.listener(|this, _, _, cx| {
                        this.bump_decimals(1);
                        cx.notify();
                    })))
                    .child(menu_item(&t, "dec-less", "Fewer decimal places", "").on_click(cx.listener(|this, _, _, cx| {
                        this.bump_decimals(-1);
                        cx.notify();
                    })))
                    .into_any_element()
            }
            Menu::CondFormat => self.render_cf_menu(cx).into_any_element(),
            Menu::Chart => {
                use xl_engine::chart::SparkKind;
                let mut panel = menu_panel(&t, 200.);
                for (i, k) in xl_engine::chart::ChartKind::ALL.into_iter().enumerate() {
                    panel = panel.child(menu_item(&t, ("insert-chart", i), format!("{} chart", k.label()), "").on_click(cx.listener(move |this, _, _, cx| {
                        this.insert_chart(k);
                        cx.notify();
                    })));
                }
                panel = panel.child(menu_separator(&t)).child(menu_item(&t, "insert-pivot", "Pivot table", "").on_click(cx.listener(|this, _, _, cx| {
                    this.menu = None;
                    this.insert_pivot();
                    cx.notify();
                })));
                panel = panel.child(menu_separator(&t));
                for (i, (k, label)) in [(SparkKind::Line, "Line sparklines"), (SparkKind::Column, "Column sparklines"), (SparkKind::WinLoss, "Win/loss sparklines")].into_iter().enumerate() {
                    panel = panel.child(menu_item(&t, ("insert-spark", i), label, "").on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = None;
                        this.insert_sparklines(k);
                        cx.notify();
                    })));
                }
                panel.into_any_element()
            }
            Menu::Main => {
                let dark = t.dark;
                let close = |this: &mut Spreadsheet| this.menu = None;
                menu_panel(&t, 230.)
                    .child(menu_item(&t, "m-home", "Home", "Alt+Home").on_click(cx.listener(move |this, _, _, cx| {
                        close(this);
                        this.go_home();
                        cx.notify();
                    })))
                    .child(menu_item(&t, "m-new", "New blank workbook", "Ctrl+N").on_click(cx.listener(|this, _, window, cx| {
                        this.menu = None;
                        this.request(crate::lifecycle::After::New(crate::home::Starter::Blank), window, cx);
                        cx.notify();
                    })))
                    .child(menu_item(&t, "m-open", "Open…", "Ctrl+O").on_click(cx.listener(move |this, _, _, cx| {
                        close(this);
                        this.open(cx);
                        cx.notify();
                    })))
                    .child(menu_item(&t, "m-save", "Save", "Ctrl+S").on_click(cx.listener(move |this, _, _, cx| {
                        close(this);
                        this.save(false, cx);
                        cx.notify();
                    })))
                    .child(menu_item(&t, "m-save-as", "Save as…", "Ctrl+Shift+S").on_click(cx.listener(move |this, _, _, cx| {
                        close(this);
                        this.save(true, cx);
                        cx.notify();
                    })))
                    .child(menu_item(&t, "m-history", "Version history…", "").on_click(cx.listener(move |this, _, _, cx| {
                        close(this);
                        this.open_history();
                        cx.notify();
                    })))
                    .child(menu_separator(&t))
                    .child(menu_item(&t, "m-palette", "Search commands…", "Ctrl+K").on_click(cx.listener(move |this, _, _, cx| {
                        close(this);
                        this.open_palette();
                        cx.notify();
                    })))
                    .child(menu_item(&t, "m-theme", if dark { "Light appearance" } else { "Dark appearance" }, "").on_click(cx.listener(move |this, _, _, cx| {
                        close(this);
                        this.toggle_theme();
                        cx.notify();
                    })))
                    .child(menu_separator(&t))
                    .child(menu_item(&t, "m-quit", "Quit", "Ctrl+Q").on_click(cx.listener(|this, _, window, cx| {
                        this.menu = None;
                        this.request(crate::lifecycle::After::Quit, window, cx);
                    })))
                    .into_any_element()
            }
        }
    }

    // -------------------------------------------------------- context menu

    fn render_context_menu(&self, kind: ContextKind, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let sel = self.selection();
        let plural = |n: u32, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
        let rows = plural(sel.rows(), "row", "rows");
        let cols = plural(sel.cols(), "column", "columns");
        type Action = fn(&mut Spreadsheet, &mut Window, &mut Context<Spreadsheet>);
        let mut items: Vec<Option<(String, &'static str, Action)>> = Vec::new();
        let clipboard: [Option<(String, &'static str, Action)>; 7] = [
            Some(("Cut".into(), "Ctrl+X", |s, _, cx| s.cut(cx))),
            Some(("Copy".into(), "Ctrl+C", |s, _, cx| s.copy(cx))),
            Some(("Paste".into(), "Ctrl+V", |s, _, cx| s.paste(cx))),
            Some(("Paste values".into(), "Ctrl+Shift+V", |s, _, cx| s.paste_with(crate::app::PasteMode::Values, cx))),
            Some(("Paste formats".into(), "", |s, _, cx| s.paste_with(crate::app::PasteMode::Formats, cx))),
            Some(("Paste transposed".into(), "", |s, _, cx| s.paste_with(crate::app::PasteMode::Transpose, cx))),
            None,
        ];
        items.extend(clipboard);
        if kind == ContextKind::Chart {
            let current = self.selected_chart.and_then(|i| self.wb.layout(self.cur).charts.get(i)).map(|c| (c.kind, c.stacked));
            let mut panel = menu_panel(&t, 220.).mt(px(0.));
            for (i, k) in xl_engine::chart::ChartKind::ALL.into_iter().enumerate() {
                let on = current.is_some_and(|c| c.0 == k);
                panel = panel.child(menu_item(&t, ("chart-kind", i), format!("{}{}", k.label(), if on { "  ✓" } else { "" }), "").on_click(cx.listener(move |this, _, _, cx| {
                    this.update_chart(|c| crate::charts::switch_kind(c, k));
                    cx.notify();
                })));
            }
            let stacked = current.is_some_and(|c| c.1);
            return panel
                .child(menu_separator(&t))
                .child(menu_item(&t, "chart-stacked", if stacked { "Side by side" } else { "Stacked" }, "").on_click(cx.listener(|this, _, _, cx| {
                    this.update_chart(|c| c.stacked = !c.stacked);
                    cx.notify();
                })))
                .child(menu_item(&t, "chart-title", "Edit title…", "Double-click").on_click(cx.listener(|this, _, _, cx| {
                    this.edit_chart_title();
                    cx.notify();
                })))
                .child(menu_item(&t, "chart-format", "Format chart…", "").on_click(cx.listener(|this, _, _, cx| {
                    this.open_form(crate::solver_ui::FormKind::Chart);
                    cx.notify();
                })))
                .child(menu_item(&t, "chart-combo", "Last series as line on 2nd axis", "").on_click(cx.listener(|this, _, _, cx| {
                    this.chart_combo_last_series();
                    cx.notify();
                })))
                .child(menu_separator(&t))
                .child(menu_item(&t, "chart-delete", "Delete chart", "Delete").on_click(cx.listener(|this, _, _, cx| {
                    this.delete_chart();
                    cx.notify();
                })));
        }
        match kind {
            ContextKind::Chart => {}
            ContextKind::Rows => {
                items.push(Some((format!("Insert {rows} above"), "Ctrl+Shift+=", |s, _, _| s.insert_rows(false))));
                items.push(Some((format!("Insert {rows} below"), "", |s, _, _| s.insert_rows(true))));
                items.push(Some((format!("Delete {rows}"), "Ctrl+-", |s, _, _| s.delete_rows())));
                items.push(None);
                items.push(Some(("Group".into(), "Alt+Shift+Right", |s, _, _| s.group(false))));
                items.push(Some(("Ungroup".into(), "Alt+Shift+Left", |s, _, _| s.group(true))));
                items.push(Some(("Hide".into(), "", |s, _, _| s.set_hidden(true, true))));
                items.push(Some(("Unhide".into(), "", |s, _, _| s.set_hidden(true, false))));
                items.push(Some(("Fit row height".into(), "", |s, w, _| s.autofit_row(s.active.row, w))));
            }
            ContextKind::Cols => {
                items.push(Some((format!("Insert {cols} left"), "Ctrl+Shift+=", |s, _, _| s.insert_cols(false))));
                items.push(Some((format!("Insert {cols} right"), "", |s, _, _| s.insert_cols(true))));
                items.push(Some((format!("Delete {cols}"), "Ctrl+-", |s, _, _| s.delete_cols())));
                items.push(None);
                items.push(Some(("Group".into(), "Alt+Shift+Right", |s, _, _| s.group(false))));
                items.push(Some(("Ungroup".into(), "Alt+Shift+Left", |s, _, _| s.group(true))));
                items.push(Some(("Hide".into(), "", |s, _, _| s.set_hidden(false, true))));
                items.push(Some(("Unhide".into(), "", |s, _, _| s.set_hidden(false, false))));
                items.push(Some(("Autofit column width".into(), "", |s, w, _| s.autofit(s.active.col, w))));
            }
            ContextKind::Cells => {
                items.push(Some((format!("Insert {rows} above"), "", |s, _, _| s.insert_rows(false))));
                items.push(Some((format!("Insert {cols} left"), "", |s, _, _| s.insert_cols(false))));
                items.push(Some((format!("Delete {rows}"), "", |s, _, _| s.delete_rows())));
                items.push(Some((format!("Delete {cols}"), "", |s, _, _| s.delete_cols())));
            }
        }
        if kind == ContextKind::Cells {
            items.push(None);
            let has_link = self.link_at(self.active).is_some();
            items.push(Some((if has_link { "Edit link…".into() } else { "Insert link…".into() }, "", |s, _, _| s.open_form(crate::solver_ui::FormKind::Link))));
            if has_link {
                items.push(Some(("Remove link".into(), "", |s, _, _| s.remove_link())));
            }
            let has_note = self.note_at(self.active).is_some();
            items.push(Some((if has_note { "Edit note…".into() } else { "Insert note…".into() }, "Shift+F2", |s, _, _| s.edit_note())));
            if has_note {
                items.push(Some(("Delete note".into(), "", |s, _, _| s.delete_note())));
            }
            items.push(None);
            items.push(Some(("Sort A → Z".into(), "", |s, _, _| s.sort_by_active(true))));
            items.push(Some(("Sort Z → A".into(), "", |s, _, _| s.sort_by_active(false))));
            items.push(Some(("Filter".into(), "Ctrl+Shift+L", |s, _, _| s.toggle_filter())));
        }
        items.push(None);
        items.push(Some(("Clear contents".into(), "Delete", |s, _, _| s.clear_selection())));
        items.push(Some(("Clear formatting".into(), "", |s, _, _| s.clear_formatting())));
        let mut panel = menu_panel(&t, 236.).mt(px(0.));
        for (i, item) in items.into_iter().enumerate() {
            panel = match item {
                None => panel.child(menu_separator(&t)),
                Some((label, keys, action)) => panel.child(menu_item(&t, ("ctx", i), label, keys).on_click(cx.listener(move |this, _, window, cx| {
                    this.context = None;
                    action(this, window, cx);
                    cx.notify();
                }))),
            };
        }
        panel
    }

    // --------------------------------------------------------- formula bar

    fn render_formula_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let sel = self.selection();
        let typing = (self.field == Some(Field::NameBox)).then(|| self.name_text.clone());
        let name_box = match (&typing, &self.edit) {
            (Some(t), _) => t.clone(),
            (None, Some(e)) if e.at.sheet != self.cur => format!("{}!{}", quote_sheet_name(self.wb.sheet_name(e.at.sheet)), e.at.cell),
            (None, Some(e)) => e.at.cell.to_string(),
            (None, None) => match self.wb.name_for_range(self.cur, sel) {
                Some(n) => n.to_string(),
                None if sel.start == sel.end => self.active.to_string(),
                None => format!("{} × {}", sel.rows(), sel.cols()),
            },
        };
        // A spilled cell shows its anchor's formula, greyed out (it isn't
        // this cell's input).
        let mut ghost = false;
        let (text, caret) = match &self.edit {
            Some(e) => (e.text.clone(), Some(e.cursor)),
            None => {
                let here = self.at(self.active);
                match self.wb.spill_anchor(here) {
                    Some(anchor) if anchor != here.cell => {
                        ghost = true;
                        (self.wb.input_text(self.at(anchor)), None)
                    }
                    _ => (self.wb.input_text(here), None),
                }
            }
        };

        // Color references; split at the caret.
        let mut segments: Vec<(String, Hsla)> = Vec::new();
        let mut pos = 0;
        let mut spans: Vec<(usize, usize, Hsla)> = scan_refs(&text).iter().map(|r| (r.span.start, r.span.end, t.ref_colors[r.color % t.ref_colors.len()])).collect();
        spans.sort_by_key(|s| s.0);
        for (s, e, col) in spans {
            if s > pos {
                segments.push((text[pos..s].to_string(), t.ink));
            }
            segments.push((text[s..e].to_string(), col));
            pos = e;
        }
        if pos < text.len() {
            segments.push((text[pos..].to_string(), t.ink));
        }
        if ghost {
            for s in &mut segments {
                s.1 = t.ink_faint;
            }
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
            .flex_none()
            .h(px(FORMULA_H))
            .bg(t.canvas)
            .border_b_1()
            .border_color(t.divider)
            .child(
                div()
                    .w(px(96.))
                    .h_full()
                    .flex()
                    .items_center()
                    .px(px(10.))
                    .border_r_1()
                    .border_color(t.divider)
                    .text_size(px(14.))
                    .text_color(t.ink)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .id("name-box")
                    .cursor_text()
                    .when(typing.is_some(), |d| d.bg(t.canvas).border_2().border_color(t.accent))
                    .tooltip(tip(t, "Go to a cell, range or name — or type a new name for the selection", "Ctrl+G"))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                        this.focus.focus(window, cx);
                        this.focus_name_box();
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .child(name_box)
                    .when(typing.is_some(), |d| d.child(div().w(px(1.5)).h(px(16.)).bg(t.accent))),
            )
            .child(div().px(px(8.)).child(icon("square-function", if self.edit.is_some() { t.accent } else { t.ink_faint })))
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
                    .text_size(px(14.))
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

    // ---------------------------------------------------------- bottom bar

    fn render_bottom_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child(SharedString::from(name))
                    .child(div().w(px(1.5)).h(px(15.)).bg(t.accent))
                    .into_any_element(),
                None => div().child(SharedString::from(self.wb.sheet_name(id).to_string())).into_any_element(),
            };
            div()
                .id(("sheet-tab", id as usize))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .h_full()
                .px(px(14.))
                .text_size(px(14.))
                .cursor_pointer()
                .border_r_1()
                .border_color(t.divider)
                // The active tab is part of the sheet: canvas-colored, with an
                // accent rule along its bottom edge.
                .when(active, |d| d.bg(t.canvas).text_color(t.ink).font_weight(FontWeight::SEMIBOLD).border_b_2().border_color(t.accent))
                .when(!active, |d| d.text_color(t.ink_muted).hover(|s| s.bg(t.chrome_hover).text_color(t.ink)))
                .child(label)
                .when(active && many && self.renaming.is_none(), |d| {
                    d.child(
                        div()
                            .id(("sheet-del", id as usize))
                            .flex()
                            .items_center()
                            .justify_center()
                            .h(px(18.))
                            .px(px(if confirming { 6. } else { 1. }))
                            .rounded(px(3.))
                            .text_size(px(12.5))
                            .font_weight(FontWeight::NORMAL)
                            .when(confirming, |d| d.bg(t.error).text_color(t.on_accent).child("Delete sheet?"))
                            .when(!confirming, |d| d.hover(|s| s.bg(t.chrome_hover)).child(svg().path("icons/x.svg").size(px(13.)).text_color(t.ink_faint)))
                            .tooltip(tip(t, "Delete sheet", ""))
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

        let mode = match &self.edit {
            None if self.loading.is_some() => Some("Opening…"),
            None => None,
            Some(e) if e.can_point() && e.mode == EditMode::Enter => Some("Point"),
            Some(e) if e.mode == EditMode::Enter => Some("Enter"),
            Some(_) => Some("Edit"),
        };
        let summary = self.selection_summary();

        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .h(px(BOTTOM_H))
            .bg(t.chrome)
            .border_t_1()
            .border_color(t.divider)
            .text_size(px(13.5))
            .child(div().flex().flex_row().h_full().border_r_1().border_color(t.divider).children(tabs))
            .child(
                tool_button(&t, "add-sheet", false)
                    .mx(px(4.))
                    .h(px(24.))
                    .child(icon("plus", t.ink_muted))
                    .tooltip(tip(t, "New sheet", "Ctrl+Shift+N"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.focus.focus(window, cx);
                        this.add_sheet();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .px(px(12.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .when_some(self.message.clone().map(|(m, e)| (SharedString::from(m), e)).or_else(|| self.script_error_status()), |d, (msg, err)| d.text_color(if err { t.error } else { t.ink_muted }).child(msg)),
            )
            .when_some(mode, |d, m| d.child(div().px(px(12.)).text_color(t.ink_muted).child(m)))
            .when_some(summary, |d, parts| {
                d.child(div().flex().flex_row().gap(px(18.)).pr(px(14.)).children(parts.into_iter().map(|(label, value)| {
                    div().flex().flex_row().gap(px(6.)).child(div().text_color(t.ink_muted).child(label)).child(div().text_color(t.ink).child(value))
                })))
            })
    }
}

impl Render for Spreadsheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.home.is_some() { "xl".to_string() } else { self.title() };
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
                let focus = this.read(cx).focus.clone();
                window.handle_input(&focus, gpui::ElementInputHandler::new(bounds, this.clone()), cx);
                let ops = this.read(cx).build_frame(bounds, window);
                paint_ops(ops, window, cx);
                crate::pictures::PictureCache::prune(&this, cx);
            },
        )
        .size_full();

        let content = div()
            .key_context("Spreadsheet")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| this.on_drop_files(paths, window, cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(t.canvas)
            .text_color(t.ink)
            .font_family(crate::FONT)
            .text_size(px(14.))
            .child(self.render_toolbar(window, cx))
            .when(self.home.is_some(), |d| d.children(self.render_recovery_bar(cx)).child(self.render_home(cx)))
            .when(self.home.is_none(), |d| d
            .child(self.render_formula_bar(cx))
            .when(self.find.open, |d| d.child(self.render_find_bar(cx)))
            .children(self.render_recovery_bar(cx))
            .child(
                div().flex_1().min_h(px(0.)).flex().flex_row().child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .relative()
                    .overflow_hidden()
                    .child(grid)
                    .children(self.render_floating_filters(cx))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_scroll(cx.listener(Self::on_scroll)),
                )
                .children(self.render_pivot_panel(cx))
                .children(self.render_script_panel(cx))
                .children(self.render_history_panel(cx)),
            )
            .child(self.render_bottom_bar(cx)))
            .relative()
            .when(self.menu.is_some() || self.palette.is_some() || self.context.is_some() || self.filter_menu.is_some() || self.dropdown.is_some(), |d| {
                // Transparent backdrop: a click anywhere outside the open menu or palette closes it.
                d.child(
                    deferred(div().id("menu-backdrop").absolute().top_0().left_0().size_full().occlude().on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.menu = None;
                            this.palette = None;
                            this.context = None;
                            this.filter_menu = None;
                            this.dropdown = None;
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ))
                    .with_priority(1),
                )
            })
            .when_some(self.filter_menu.as_ref().map(|m| m.pos), |d, pos| {
                d.child(deferred(gpui::anchored().position(pos).snap_to_window_with_margin(px(8.)).child(self.render_filter_menu(cx))).with_priority(2))
            })
            .when_some(if self.menu.is_none() && self.palette.is_none() { self.render_completion(cx) } else { None }, |d, (pos, el)| {
                d.child(deferred(gpui::anchored().position(pos).snap_to_window_with_margin(px(8.)).child(el)).with_priority(2))
            })
            .when_some(self.render_dropdown(cx), |d, (pos, el)| d.child(deferred(gpui::anchored().position(pos).snap_to_window_with_margin(px(8.)).child(el)).with_priority(2)))
            .when_some(if self.menu.is_none() && self.context.is_none() { self.render_note_card(cx).or_else(|| self.render_prompt()).or_else(|| self.render_diff_card()) } else { None }, |d, (pos, el)| {
                d.child(deferred(gpui::anchored().position(pos).snap_to_window_with_margin(px(8.)).child(el)).with_priority(2))
            })
            .when_some(self.context, |d, (pos, kind)| {
                d.child(deferred(gpui::anchored().position(pos).snap_to_window_with_margin(px(8.)).child(self.render_context_menu(kind, cx))).with_priority(2))
            })
            .when(self.palette.is_some(), |d| {
                d.child(deferred(div().absolute().top(px(64.)).left_0().right_0().flex().justify_center().child(self.render_palette(cx))).with_priority(2))
            });
        let content = content.when(self.pending.is_some(), |d| d.child(deferred(self.render_dialog(cx)).with_priority(3)));
        let content = content.when(self.cf_dialog.is_some(), |d| d.child(deferred(self.render_cf_dialog(cx)).with_priority(3)));
        let content = content.when(self.print_ui_open(), |d| d.child(deferred(self.render_print_ui(cx)).with_priority(3)));
        let content = content.when(self.form.is_some(), |d| d.child(deferred(self.render_form(cx)).with_priority(3)));
        let content = content.when(self.lossy_prompt, |d| d.child(deferred(self.render_lossy_dialog(cx)).with_priority(4)));
        let content = content.when(self.scripts.consent_prompt && self.home.is_none(), |d| d.child(deferred(self.render_consent_dialog(cx)).with_priority(4)));
        let content = content.when(self.loading.is_some() && self.big.progress.is_some(), |d| d.child(deferred(self.render_load_progress(cx)).with_priority(2)));
        let content = content.when(self.big.xlsx_refused.is_some(), |d| d.child(deferred(self.render_xlsx_refused(cx)).with_priority(4)));
        self.window_frame(content, window)
    }
}

impl Spreadsheet {
    /// When we draw our own decorations: an invisible resize border with
    /// cursors, rounded corners, a hairline border and a soft shadow — all
    /// dropped on the sides where the window is tiled or maximized.
    fn window_frame(&self, content: impl IntoElement, window: &mut Window) -> impl IntoElement {
        let t = self.theme;
        let Decorations::Client { tiling } = window.window_decorations() else {
            window.set_client_inset(px(0.));
            return div().size_full().child(content).into_any_element();
        };
        let inset = px(FRAME);
        window.set_client_inset(inset);
        let rounding = px(CORNER);
        let frame = div()
            .id("window-frame")
            .size_full()
            .bg(gpui::transparent_black())
            .child(
                canvas(
                    |_, window, _| window.insert_hitbox(gpui::Bounds::new(gpui::point(px(0.), px(0.)), window.window_bounds().get_bounds().size), HitboxBehavior::Normal),
                    move |_, hitbox, window, _| {
                        let size = window.window_bounds().get_bounds().size;
                        if let Some(edge) = resize_edge(window.mouse_position(), inset, size) {
                            window.set_cursor_style(resize_cursor(edge), &hitbox);
                        }
                    },
                )
                .size_full()
                .absolute(),
            )
            .when(!tiling.top, |d| d.pt(inset))
            .when(!tiling.bottom, |d| d.pb(inset))
            .when(!tiling.left, |d| d.pl(inset))
            .when(!tiling.right, |d| d.pr(inset))
            .on_mouse_move(|_, window, _| window.refresh())
            .on_mouse_down(MouseButton::Left, move |ev, window, _| {
                let size = window.window_bounds().get_bounds().size;
                if let Some(edge) = resize_edge(ev.position, inset, size) {
                    window.start_window_resize(edge);
                }
            });
        let inner = div()
            .size_full()
            .overflow_hidden()
            .cursor(CursorStyle::Arrow)
            .bg(t.canvas)
            .border_color(gpui::rgb(if t.dark { 0x3A3C42 } else { 0xC4C7CD }))
            .when(!(tiling.top || tiling.left), |d| d.rounded_tl(rounding))
            .when(!(tiling.top || tiling.right), |d| d.rounded_tr(rounding))
            .when(!(tiling.bottom || tiling.left), |d| d.rounded_bl(rounding))
            .when(!(tiling.bottom || tiling.right), |d| d.rounded_br(rounding))
            .when(!tiling.top, |d| d.border_t_1())
            .when(!tiling.bottom, |d| d.border_b_1())
            .when(!tiling.left, |d| d.border_l_1())
            .when(!tiling.right, |d| d.border_r_1())
            .when(!tiling.is_tiled(), |d| {
                d.shadow(vec![gpui::BoxShadow::new(px(0.), px(2.), gpui::hsla(0., 0., 0., if t.dark { 0.5 } else { 0.22 })).blur_radius(inset / 1.5)])
            })
            .on_mouse_move(|_, _, cx| cx.stop_propagation())
            .child(content);
        frame.child(inner).into_any_element()
    }
}
