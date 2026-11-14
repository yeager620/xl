//! Everything around the grid: title bar (toolbar), formula bar, bottom bar
//! (sheet tabs + selection summary), menus, tooltips and the window frame.

use gpui::{
    AnyElement, AnyView, App, AppContext, Context, CursorStyle, Decorations, ExternalPaths, FontWeight, Hsla, HitboxBehavior, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Pixels, Point, Render, ResizeEdge, SharedString, Size, StatefulInteractiveElement, Styled, Window,
    canvas, deferred, div, prelude::FluentBuilder, px, rgb, svg,
};
use xl_engine::{HAlign, NumberFormat, Rgb, Value, quote_sheet_name};

use crate::app::{BorderPreset, ContextKind, EditMode, Emphasis, Menu, Spreadsheet};
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

fn icon(name: &'static str, color: Hsla) -> impl IntoElement {
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

fn tip(t: Theme, text: &'static str, keys: &'static str) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_, cx| cx.new(|_| Tip { text: text.into(), keys: (!keys.is_empty()).then(|| keys.into()), theme: t }).into()
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

fn menu_panel(t: &Theme, width: f32) -> gpui::Stateful<gpui::Div> {
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

fn menu_item(t: &Theme, id: impl Into<gpui::ElementId>, label: impl Into<SharedString>, detail: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
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

fn menu_separator(t: &Theme) -> impl IntoElement {
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
            .child(tool_button(&t, "dec-less", false).child(icon("decimals-arrow-left", t.ink)).tooltip(tip(t, "Fewer decimal places", "")).on_click(
                cx.listener(|this, _, _, cx| {
                    this.bump_decimals(-1);
                    cx.notify();
                }),
            ))
            .child(tool_button(&t, "dec-more", false).child(icon("decimals-arrow-right", t.ink)).tooltip(tip(t, "More decimal places", "")).on_click(
                cx.listener(|this, _, _, cx| {
                    this.bump_decimals(1);
                    cx.notify();
                }),
            ))
            .child(divider(&t))
            .child(emph("bold", "bold", st.bold, Emphasis::Bold, "Bold", "Ctrl+B", cx))
            .child(emph("italic", "italic", st.italic, Emphasis::Italic, "Italic", "Ctrl+I", cx))
            .child(emph("underline", "underline", st.underline, Emphasis::Underline, "Underline", "Ctrl+U", cx))
            .child(emph("strike", "strikethrough", st.strike, Emphasis::Strike, "Strikethrough", "Ctrl+5", cx))
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
            .children([("align-left", "align-left", HAlign::Left, "Align left"), ("align-center", "align-center", HAlign::Center, "Center"), (
                "align-right",
                "align-right",
                HAlign::Right,
                "Align right",
            )]
            .map(|(id, name, a, label)| {
                tool_button(&t, id, st.align == a).child(icon(name, t.ink)).tooltip(tip(t, label, "")).on_click(cx.listener(move |this, _, _, cx| {
                    this.set_align(a);
                    cx.notify();
                }))
            }))
            .child(tool_button(&t, "wrap", st.wrap).child(icon("wrap-text", t.ink)).tooltip(tip(t, "Wrap text", "")).on_click(cx.listener(|this, _, _, cx| {
                this.toggle_wrap();
                cx.notify();
            })))
            .child({
                let merged = self.wb.layout(self.cur).merge_at(self.active).is_some();
                tool_button(&t, "merge", merged).child(icon("table-cells-merge", t.ink)).tooltip(tip(t, if merged { "Unmerge cells" } else { "Merge and center" }, "")).on_click(cx.listener(|this, _, _, cx| {
                    this.toggle_merge(true);
                    cx.notify();
                }))
            })
            .child(self.menu_button("borders", Menu::Borders, "Borders", icon("grid-2x2", t.ink).into_any_element(), cx));

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
            .child(icon("sheet", t.ink_muted))
            .child(
                div()
                    .ml(px(4.))
                    .mr(px(8.))
                    .max_w(px(220.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.ink)
                    .child(self.file_name())
                    .when(self.dirty, |d| d.child(div().ml(px(4.)).text_color(t.ink_faint).font_weight(FontWeight::NORMAL).child("— edited"))),
            )
            .child(tools)
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
                    .into_any_element()
            }
            Menu::Main => {
                let dark = t.dark;
                let close = |this: &mut Spreadsheet| this.menu = None;
                menu_panel(&t, 230.)
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
        let clipboard: [Option<(String, &'static str, Action)>; 4] = [
            Some(("Cut".into(), "Ctrl+X", |s, _, cx| s.cut(cx))),
            Some(("Copy".into(), "Ctrl+C", |s, _, cx| s.copy(cx))),
            Some(("Paste".into(), "Ctrl+V", |s, _, cx| s.paste(cx))),
            None,
        ];
        items.extend(clipboard);
        match kind {
            ContextKind::Rows => {
                items.push(Some((format!("Insert {rows} above"), "Ctrl+Shift+=", |s, _, _| s.insert_rows(false))));
                items.push(Some((format!("Insert {rows} below"), "", |s, _, _| s.insert_rows(true))));
                items.push(Some((format!("Delete {rows}"), "Ctrl+-", |s, _, _| s.delete_rows())));
                items.push(None);
                items.push(Some(("Hide".into(), "", |s, _, _| s.set_hidden(true, true))));
                items.push(Some(("Unhide".into(), "", |s, _, _| s.set_hidden(true, false))));
                items.push(Some(("Fit row height".into(), "", |s, w, _| s.autofit_row(s.active.row, w))));
            }
            ContextKind::Cols => {
                items.push(Some((format!("Insert {cols} left"), "Ctrl+Shift+=", |s, _, _| s.insert_cols(false))));
                items.push(Some((format!("Insert {cols} right"), "", |s, _, _| s.insert_cols(true))));
                items.push(Some((format!("Delete {cols}"), "Ctrl+-", |s, _, _| s.delete_cols())));
                items.push(None);
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
        let name_box = match &self.edit {
            Some(e) if e.at.sheet != self.cur => format!("{}!{}", quote_sheet_name(self.wb.sheet_name(e.at.sheet)), e.at.cell),
            Some(e) => e.at.cell.to_string(),
            None if sel.start == sel.end => self.active.to_string(),
            None => format!("{} × {}", sel.rows(), sel.cols()),
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
                segments.push((text[pos..s].to_string(), t.ink));
            }
            segments.push((text[s..e].to_string(), col));
            pos = e;
        }
        if pos < text.len() {
            segments.push((text[pos..].to_string(), t.ink));
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
                    .child(name_box),
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
                    .when_some(self.message.clone(), |d, (msg, err)| d.text_color(if err { t.error } else { t.ink_muted }).child(msg)),
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
                let focus = this.read(cx).focus.clone();
                window.handle_input(&focus, gpui::ElementInputHandler::new(bounds, this.clone()), cx);
                let ops = this.read(cx).build_frame(bounds, window);
                paint_ops(ops, window, cx);
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
            .child(self.render_formula_bar(cx))
            .children(self.render_recovery_bar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .relative()
                    .overflow_hidden()
                    .child(grid)
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_scroll(cx.listener(Self::on_scroll)),
            )
            .child(self.render_bottom_bar(cx))
            .relative()
            .when(self.menu.is_some() || self.palette.is_some() || self.context.is_some(), |d| {
                // Transparent backdrop: a click anywhere outside the open menu or palette closes it.
                d.child(
                    deferred(div().id("menu-backdrop").absolute().top_0().left_0().size_full().occlude().on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.menu = None;
                            this.palette = None;
                            this.context = None;
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ))
                    .with_priority(1),
                )
            })
            .when_some(self.context, |d, (pos, kind)| {
                d.child(deferred(gpui::anchored().position(pos).snap_to_window_with_margin(px(8.)).child(self.render_context_menu(kind, cx))).with_priority(2))
            })
            .when(self.palette.is_some(), |d| {
                d.child(deferred(div().absolute().top(px(64.)).left_0().right_0().flex().justify_center().child(self.render_palette(cx))).with_priority(2))
            });
        let content = content.when(self.pending.is_some(), |d| d.child(deferred(self.render_dialog(cx)).with_priority(3)));
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
