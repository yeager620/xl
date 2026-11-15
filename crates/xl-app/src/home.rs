//! The Home page: start a blank workbook or one from a template, reopen a
//! recent file, or open one from disk. Shown when xl starts without a file,
//! and any time from the menu, the palette or Alt+Home.
//!
//! Going Home keeps the current workbook as it is; "Back to …" (or Escape)
//! returns to it. Starting or opening something else from Home replaces it,
//! and goes through the unsaved-changes dialog first when it has changes.

use std::path::PathBuf;
use std::sync::OnceLock;

use gpui::{
    AnyElement, Context, ElementInputHandler, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, canvas, div, prelude::FluentBuilder, px, rgb, svg,
};
use xl_engine::{CellRef, Workbook};

use crate::app::Spreadsheet;
use crate::chrome::{icon, tip};
use crate::lifecycle::After;
use crate::recent::{Entry, Recent, short_folder};
use crate::templates::{Preview, TEMPLATES};

const TILE_W: f32 = 184.0;
const THUMB_H: f32 = 116.0;
const PREVIEW_ROW_H: f32 = 12.0;
const PREVIEW_ROWS: u32 = 9;

/// What to start with when replacing the current workbook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Starter {
    Blank,
    Template(usize),
}

/// Something on the Home page that can be selected with the keyboard.
#[derive(Clone, Debug, PartialEq)]
pub enum HomeItem {
    Back,
    Blank,
    Template(usize),
    Open,
    Recent(PathBuf),
}

impl HomeItem {
    /// Rows of the page, for Up/Down.
    fn group(&self) -> u8 {
        match self {
            HomeItem::Back => 0,
            HomeItem::Blank | HomeItem::Template(_) => 1,
            HomeItem::Open => 2,
            HomeItem::Recent(_) => 3,
        }
    }
}

pub struct HomeState {
    /// A workbook is open behind the Home page (offer to go back to it).
    pub back: bool,
    /// Filter over the recent files (typed anywhere on the page).
    pub filter: String,
    pub selected: HomeItem,
}

impl HomeState {
    pub fn new(back: bool) -> Self {
        Self { back, filter: String::new(), selected: HomeItem::Blank }
    }
}

/// Thumbnails of the templates, built once.
fn previews() -> &'static [Preview] {
    static PREVIEWS: OnceLock<Vec<Preview>> = OnceLock::new();
    PREVIEWS.get_or_init(|| {
        TEMPLATES
            .iter()
            .map(|t| {
                let (wb, _) = (t.build)();
                crate::templates::preview(&wb, PREVIEW_ROWS, 520.0)
            })
            .collect()
    })
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Does a recent entry match the filter (case-insensitive, name or folder)?
fn matches(e: &Entry, filter: &str) -> bool {
    let q = filter.trim().to_lowercase();
    q.is_empty() || e.path.to_string_lossy().to_lowercase().contains(&q) || short_folder(&e.path).to_lowercase().contains(&q)
}

impl Spreadsheet {
    /// Show the Home page, keeping the current workbook to come back to.
    pub(crate) fn go_home(&mut self) {
        if self.home.is_some() {
            return;
        }
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        self.menu = None;
        self.palette = None;
        self.context = None;
        self.filter_menu = None;
        self.field = None;
        self.renaming = None;
        self.print = Default::default();
        // Pick up files opened in other windows and files that went missing.
        self.recent = Recent::load(self.recent.dir());
        self.home = Some(HomeState::new(true));
    }

    /// Leave Home for the workbook behind it.
    pub(crate) fn leave_home(&mut self) {
        if self.home.as_ref().is_some_and(|h| h.back) {
            self.home = None;
        }
    }

    /// Replace the workbook with a new blank one or a template (untitled:
    /// Save asks where). Unsaved changes are dealt with before this.
    pub(crate) fn start_new(&mut self, starter: Starter) {
        let (wb, start, message) = match starter {
            Starter::Blank => (Workbook::new(), CellRef::new(0, 0), None),
            Starter::Template(i) => {
                let t = &TEMPLATES[i.min(TEMPLATES.len() - 1)];
                let (wb, start) = (t.build)();
                (wb, start, Some(format!("New workbook from the {} template", t.name)))
            }
        };
        self.wb = wb;
        self.scripts_loaded();
        self.big.forget_workbook();
        self.cur = self.wb.sheet_ids()[0];
        self.views.clear();
        self.last_stats = None;
        self.undo.clear();
        self.redo.clear();
        self.clip = None;
        self.active = start;
        self.anchor = start;
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
        self.edit = None;
        self.renaming = None;
        self.field = None;
        self.menu = None;
        self.context = None;
        self.filter_menu = None;
        self.selected_chart = None;
        self.form = None;
        self.cf_dialog = None;
        self.print = Default::default();
        self.confirm_delete = None;
        self.dirty = false;
        self.unsupported.clear();
        self.package = None;
        self.path = None;
        self.restore_as = None;
        self.message = message.map(|m| (m, false));
        self.home = None;
    }

    /// The selectable items, in reading order.
    fn home_items(&self) -> Vec<HomeItem> {
        let Some(h) = &self.home else { return Vec::new() };
        let mut items = Vec::new();
        if h.back {
            items.push(HomeItem::Back);
        }
        items.push(HomeItem::Blank);
        items.extend((0..TEMPLATES.len()).map(HomeItem::Template));
        items.push(HomeItem::Open);
        items.extend(self.visible_recents().into_iter().map(|e| HomeItem::Recent(e.path.clone())));
        items
    }

    pub(crate) fn visible_recents(&self) -> Vec<&Entry> {
        let filter = self.home.as_ref().map_or("", |h| h.filter.as_str());
        self.recent.entries().iter().filter(|e| matches(e, filter)).collect()
    }

    /// After the filter or the list changed: keep the selection on something shown.
    fn fix_home_selection(&mut self, prefer_first_recent: bool) {
        let items = self.home_items();
        let first_recent = items.iter().find(|i| matches!(i, HomeItem::Recent(_))).cloned();
        let Some(h) = self.home.as_mut() else { return };
        if prefer_first_recent
            && !h.filter.trim().is_empty()
            && let Some(r) = first_recent
        {
            h.selected = r;
            return;
        }
        if !items.contains(&h.selected) {
            h.selected = first_recent.unwrap_or(HomeItem::Blank);
        }
    }

    pub(crate) fn home_filter_push(&mut self, text: &str) {
        if let Some(h) = self.home.as_mut() {
            h.filter.push_str(text);
        }
        self.fix_home_selection(true);
    }

    /// Open, start or go back to an item.
    pub(crate) fn activate_home_item(&mut self, item: HomeItem, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(h) = self.home.as_mut() {
            h.selected = item.clone();
        }
        match item {
            HomeItem::Back => self.leave_home(),
            HomeItem::Blank => self.request(After::New(Starter::Blank), window, cx),
            HomeItem::Template(i) => self.request(After::New(Starter::Template(i)), window, cx),
            HomeItem::Open => self.open(cx),
            HomeItem::Recent(path) => {
                if path.is_file() {
                    self.request(After::Open(path), window, cx);
                } else {
                    self.message = Some((format!("Can’t find “{}”. It may have been moved or deleted.", file_name(&path)), true));
                    self.recent = Recent::load(self.recent.dir());
                }
            }
        }
    }

    pub(crate) fn remove_recent(&mut self, path: &std::path::Path) {
        self.recent.remove(path);
        self.fix_home_selection(false);
    }

    pub(crate) fn toggle_recent_pin(&mut self, path: &std::path::Path) {
        self.recent.toggle_pin(path);
    }

    /// Keyboard on the Home page. Typed characters filter the recent list
    /// (they arrive through the input handler, see ime.rs).
    pub(crate) fn key_home(&mut self, key: &str, ctrl: bool, shift: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let items = self.home_items();
        let Some(h) = self.home.as_mut() else { return false };
        let i = items.iter().position(|it| *it == h.selected).unwrap_or(0);
        let last = items.len().saturating_sub(1);
        let group = items[i].group();
        match key {
            "escape" if !h.filter.is_empty() => {
                h.filter.clear();
                self.fix_home_selection(false);
            }
            "escape" => self.leave_home(),
            "backspace" => {
                h.filter.pop();
                self.fix_home_selection(true);
            }
            "enter" | "space" if key == "enter" || h.filter.is_empty() => {
                let item = h.selected.clone();
                self.activate_home_item(item, window, cx);
            }
            "tab" if shift => h.selected = items[i.saturating_sub(1)].clone(),
            "tab" => h.selected = items[(i + 1).min(last)].clone(),
            "left" => h.selected = items[i.saturating_sub(1)].clone(),
            "right" => h.selected = items[(i + 1).min(last)].clone(),
            "down" => {
                let next = if group == 3 { items.get(i + 1) } else { items.iter().find(|it| it.group() > group) };
                if let Some(next) = next {
                    h.selected = next.clone();
                }
            }
            "up" => {
                let prev = if group == 3 && i > 0 && items[i - 1].group() == 3 {
                    Some(&items[i - 1])
                } else {
                    // The first item of the row above.
                    items.iter().rev().find(|it| it.group() < group).and_then(|above| items.iter().find(|it| it.group() == above.group()))
                };
                if let Some(prev) = prev {
                    h.selected = prev.clone();
                }
            }
            "delete" => {
                if let HomeItem::Recent(p) = h.selected.clone() {
                    self.remove_recent(&p);
                }
            }
            "p" if ctrl => {
                if let HomeItem::Recent(p) = h.selected.clone() {
                    self.toggle_recent_pin(&p);
                }
            }
            "o" if ctrl => self.open(cx),
            // Other shortcuts don't apply to the hidden workbook.
            _ if ctrl => {}
            _ => return false,
        }
        true
    }

    // ------------------------------------------------------------ drawing

    pub(crate) fn render_home(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let Some(h) = &self.home else { return div().into_any_element() };
        let this = cx.entity();
        // Typing filters the recent list: register for text input like the grid does.
        let input = canvas(
            |_, _, _| {},
            move |bounds, (), window, cx| {
                let focus = this.read(cx).focus.clone();
                window.handle_input(&focus, ElementInputHandler::new(bounds, this.clone()), cx);
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();

        let section_title = |text: &'static str| div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child(text);

        let mut tiles = div().flex().flex_row().flex_wrap().gap(px(12.)).child(self.blank_tile(h.selected == HomeItem::Blank, cx));
        for (i, tpl) in TEMPLATES.iter().enumerate() {
            let selected = h.selected == HomeItem::Template(i);
            let thumb = self.preview_thumb(&previews()[i]);
            tiles = tiles.child(
                self.tile(("home-template", i), selected, thumb, tpl.name, tpl.blurb).on_click(cx.listener(move |this, _, window, cx| {
                    this.activate_home_item(HomeItem::Template(i), window, cx);
                    cx.notify();
                })),
            );
        }

        let status: Option<(String, bool)> = match (&self.loading, &self.message) {
            (Some(name), _) => Some((format!("Opening {name}…"), false)),
            (None, Some((m, true))) => Some((m.clone(), true)),
            _ => None,
        };

        let page = div()
            .w_full()
            .max_w(px(1080.))
            .px(px(32.))
            .pt(px(24.))
            .pb(px(40.))
            .flex()
            .flex_col()
            .gap(px(28.))
            .when(h.back, |d| d.child(self.back_button(h.selected == HomeItem::Back, cx)))
            .when_some(status, |d, (text, error)| {
                d.child(div().text_size(px(13.5)).text_color(if error { t.error } else { t.ink_muted }).child(SharedString::from(text)))
            })
            .child(div().flex().flex_col().gap(px(12.)).child(section_title("New")).child(tiles))
            .child(self.render_recent_section(h, cx))
            .child(div().text_size(px(12.5)).text_color(t.ink_faint).child("Tip: drop a spreadsheet file anywhere in this window to open it."));

        div()
            .flex_1()
            .min_h(px(0.))
            .relative()
            .bg(t.canvas)
            .child(input)
            .child(div().id("home").size_full().overflow_y_scroll().child(div().w_full().flex().flex_row().justify_center().child(page)))
            .into_any_element()
    }

    fn back_button(&self, selected: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let label = format!("Back to “{}”{}", self.file_name(), if self.dirty { " (edited)" } else { "" });
        div().flex().flex_row().child(
            div()
                .id("home-back")
                .debug_selector(|| "home-back".into())
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .h(px(32.))
                .px(px(10.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(14.))
                .text_color(t.accent)
                .border_2()
                .border_color(if selected { t.accent } else { gpui::transparent_black() })
                .hover(|s| s.bg(t.chrome_hover))
                .child(icon("arrow-left", t.accent))
                .child(SharedString::from(label))
                .tooltip(tip(t, "Back to the workbook", "Esc"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.leave_home();
                    cx.notify();
                })),
        )
    }

    fn tile(&self, id: impl Into<gpui::ElementId>, selected: bool, thumb: AnyElement, name: &'static str, blurb: &'static str) -> gpui::Stateful<gpui::Div> {
        let t = self.theme;
        div()
            .id(id)
            .flex()
            .flex_col()
            .gap(px(4.))
            .w(px(TILE_W))
            .p(px(6.))
            .rounded(px(8.))
            .cursor_pointer()
            .border_2()
            .border_color(if selected { t.accent } else { gpui::transparent_black() })
            .when(selected, |d| d.bg(t.header_selected))
            .when(!selected, |d| d.hover(|s| s.bg(t.chrome_hover)))
            .child(div().mb(px(4.)).h(px(THUMB_H)).w_full().rounded(px(6.)).overflow_hidden().border_1().border_color(t.divider).child(thumb))
            .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child(name))
            .child(div().text_size(px(12.5)).line_height(px(16.)).text_color(t.ink_muted).line_clamp(2).child(blurb))
    }

    fn blank_tile(&self, selected: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let thumb = div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .bg(t.accent)
            .child(svg().path("icons/plus.svg").size(px(36.)).text_color(t.on_accent))
            .into_any_element();
        self.tile("home-blank", selected, thumb, "Blank spreadsheet", "Start from an empty sheet · Ctrl+N")
            .debug_selector(|| "home-blank".into())
            .on_click(cx.listener(|this, _, window, cx| {
                this.activate_home_item(HomeItem::Blank, window, cx);
                cx.notify();
            }))
    }

    /// A small picture of a template's top-left cells: fills, header rows and
    /// a bar for each piece of text (bold text darker), with gridlines.
    fn preview_thumb(&self, p: &Preview) -> AnyElement {
        let inner_w = TILE_W - 16.0;
        let total: f64 = p.widths.iter().sum();
        let scale = (inner_w as f64 / total.max(1.0)) as f32;
        let widths: Vec<f32> = p.widths.iter().map(|w| *w as f32 * scale).collect();
        let mut rows = div().flex().flex_col().pt(px(4.));
        for row in &p.rows {
            let mut cells = div().relative().flex().flex_row().h(px(PREVIEW_ROW_H));
            let mut bars = Vec::new();
            let mut x = 0.0f32;
            for (c, cell) in row.iter().enumerate() {
                let w = widths[c];
                let bg = cell.table_header.or(cell.fill);
                cells = cells.child(div().w(px(w)).h_full().flex_none().border_r_1().border_b_1().border_color(rgb(0xE6E8EB)).when_some(bg, |d, f| d.bg(rgb(f))));
                if cell.len > 0 {
                    // Text spills into empty cells to the right, as on the sheet.
                    let room = if cell.numeric {
                        w
                    } else {
                        w + row[c + 1..].iter().zip(&widths[c + 1..]).take_while(|(n, _)| n.len == 0).map(|(_, w)| *w).sum::<f32>()
                    };
                    let bar_w = (cell.len as f32 * 2.4).min(room - 4.0).max(2.0);
                    let left = if cell.numeric { x + w - 2.0 - bar_w } else { x + 2.0 };
                    let color = if cell.table_header.is_some() {
                        0xFFFFFF
                    } else if cell.bold {
                        0x3A3D44
                    } else {
                        0xB2B7BF
                    };
                    let bar_h = if cell.bold { 3.5 } else { 2.5 };
                    bars.push(div().absolute().left(px(left)).top(px((PREVIEW_ROW_H - bar_h) / 2.0)).w(px(bar_w)).h(px(bar_h)).rounded(px(1.)).bg(rgb(color)));
                }
                x += w;
            }
            rows = rows.child(cells.children(bars));
        }
        div().size_full().bg(rgb(0xFFFFFF)).pl(px(4.)).overflow_hidden().child(rows).into_any_element()
    }

    fn render_recent_section(&self, h: &HomeState, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let open_selected = h.selected == HomeItem::Open;
        let filter_box = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .h(px(32.))
            .w(px(240.))
            .px(px(10.))
            .rounded(px(6.))
            .bg(t.canvas)
            .border_1()
            .border_color(if h.filter.is_empty() { t.divider } else { t.accent })
            .text_size(px(13.5))
            .child(icon("search", t.ink_faint))
            .child(if h.filter.is_empty() {
                div().text_color(t.ink_faint).child("Type to filter recent files")
            } else {
                div().flex().flex_row().items_center().overflow_hidden().whitespace_nowrap().text_color(t.ink).child(SharedString::from(h.filter.clone())).child(div().w(px(1.5)).h(px(16.)).bg(t.accent))
            });
        let open = div()
            .id("home-open")
            .debug_selector(|| "home-open".into())
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .h(px(32.))
            .px(px(12.))
            .rounded(px(6.))
            .cursor_pointer()
            .text_size(px(14.))
            .text_color(t.ink)
            .border_1()
            .border_color(if open_selected { t.accent } else { t.divider })
            .when(open_selected, |d| d.bg(t.header_selected))
            .when(!open_selected, |d| d.hover(|s| s.bg(t.chrome_hover)))
            .child(icon("folder-open", t.ink))
            .child("Open…")
            .tooltip(tip(t, "Open a file", "Ctrl+O"))
            .on_click(cx.listener(|this, _, window, cx| {
                this.activate_home_item(HomeItem::Open, window, cx);
                cx.notify();
            }));
        let header = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child("Recent"))
            .child(div().flex_1())
            .child(filter_box)
            .child(open);

        let entries = self.visible_recents();
        let list = if entries.is_empty() {
            let text = if self.recent.entries().is_empty() {
                "Workbooks you open or save will show up here.".to_string()
            } else {
                format!("No recent files match “{}”.", h.filter.trim())
            };
            div().py(px(16.)).px(px(12.)).text_size(px(14.)).text_color(t.ink_muted).child(SharedString::from(text)).into_any_element()
        } else {
            let mut list = div().flex().flex_col().gap(px(2.));
            for (i, e) in entries.into_iter().enumerate() {
                list = list.child(self.recent_row(i, e, h.selected == HomeItem::Recent(e.path.clone()), cx));
            }
            list.into_any_element()
        };
        div().flex().flex_col().gap(px(10.)).child(header).child(div().border_t_1().border_color(t.divider).pt(px(6.)).child(list))
    }

    fn recent_row(&self, i: usize, e: &Entry, selected: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let name = file_name(&e.path);
        let folder = if e.missing { format!("Not found · {}", short_folder(&e.path)) } else { short_folder(&e.path) };
        let when = crate::lifecycle::ago(e.opened);
        let small_button = |id: (&'static str, usize), name: &'static str, color: gpui::Hsla| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .flex_none()
                .size(px(28.))
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|s| s.bg(t.chrome_active))
                .child(icon(name, color))
        };
        let open_path = e.path.clone();
        let pin_path = e.path.clone();
        let remove_path = e.path.clone();
        div()
            .id(("home-recent", i))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .h(px(48.))
            .px(px(10.))
            .rounded(px(6.))
            .cursor_pointer()
            .border_2()
            .border_color(if selected { t.accent } else { gpui::transparent_black() })
            .when(selected, |d| d.bg(t.header_selected))
            .when(!selected, |d| d.hover(|s| s.bg(t.chrome_hover)))
            .child(div().when(e.missing, |d| d.opacity(0.5)).child(icon("sheet", if e.missing { t.ink_faint } else { t.accent })))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .when(e.missing, |d| d.opacity(0.55))
                    .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).truncate().child(SharedString::from(name)))
                    .child(div().text_size(px(12.5)).text_color(t.ink_muted).truncate().child(SharedString::from(folder))),
            )
            .child(div().flex_none().text_size(px(12.5)).text_color(t.ink_muted).when(e.missing, |d| d.opacity(0.55)).child(SharedString::from(when)))
            .child(
                small_button(("home-pin", i), "pin", if e.pinned { t.accent } else { t.ink_faint })
                    .tooltip(tip(t, if e.pinned { "Unpin" } else { "Pin to top" }, "Ctrl+P"))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_recent_pin(&pin_path);
                        cx.notify();
                    })),
            )
            .child(
                small_button(("home-remove", i), "x", if e.missing { t.ink } else { t.ink_faint })
                    .tooltip(tip(t, "Remove from list", "Delete"))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.remove_recent(&remove_path);
                        cx.notify();
                    })),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.activate_home_item(HomeItem::Recent(open_path.clone()), window, cx);
                cx.notify();
            }))
    }
}
