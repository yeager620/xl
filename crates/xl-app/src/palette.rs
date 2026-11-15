//! Command palette (Ctrl+K / Ctrl+Shift+P): fuzzy-search every command, jump
//! to a cell by typing its address, or start a formula with a function.

use gpui::{Context, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px};
use xl_engine::{CellRef, Func, HAlign};

use crate::app::{BorderPreset, EditMode, Emphasis, Spreadsheet};

type Run = fn(&mut Spreadsheet, &mut Window, &mut Context<Spreadsheet>);

pub struct Command {
    pub name: &'static str,
    pub keys: &'static str,
    pub run: Run,
}

macro_rules! cmd {
    ($name:literal, $keys:literal, |$s:ident, $w:ident, $cx:ident| $body:expr) => {
        Command { name: $name, keys: $keys, run: |$s, $w, $cx| { $body; } }
    };
}

#[allow(unused_variables)]
pub fn commands() -> Vec<Command> {
    vec![
        cmd!("Open…", "Ctrl+O", |s, w, cx| s.open(cx)),
        cmd!("Save", "Ctrl+S", |s, w, cx| s.save(false, cx)),
        cmd!("Save as…", "Ctrl+Shift+S", |s, w, cx| s.save(true, cx)),
        cmd!("New sheet", "Ctrl+Shift+N", |s, w, cx| s.add_sheet()),
        cmd!("Rename sheet", "Double-click tab", |s, w, cx| s.start_rename(s.cur)),
        cmd!("Delete sheet", "", |s, w, cx| s.delete_sheet(s.cur)),
        cmd!("Next sheet", "Ctrl+PgDn", |s, w, cx| s.cycle_sheet(1)),
        cmd!("Previous sheet", "Ctrl+PgUp", |s, w, cx| s.cycle_sheet(-1)),
        cmd!("Undo", "Ctrl+Z", |s, w, cx| s.undo_redo(true)),
        cmd!("Redo", "Ctrl+Y", |s, w, cx| s.undo_redo(false)),
        cmd!("Copy", "Ctrl+C", |s, w, cx| s.copy(cx)),
        cmd!("Cut", "Ctrl+X", |s, w, cx| s.cut(cx)),
        cmd!("Paste", "Ctrl+V", |s, w, cx| s.paste(cx)),
        cmd!("Paste values", "Ctrl+Shift+V", |s, w, cx| s.paste_with(crate::app::PasteMode::Values, cx)),
        cmd!("Paste formulas", "", |s, w, cx| s.paste_with(crate::app::PasteMode::Formulas, cx)),
        cmd!("Paste formats", "", |s, w, cx| s.paste_with(crate::app::PasteMode::Formats, cx)),
        cmd!("Paste transposed", "", |s, w, cx| s.paste_with(crate::app::PasteMode::Transpose, cx)),
        cmd!("Insert rows above", "Ctrl+Shift+=", |s, w, cx| s.insert_rows(false)),
        cmd!("Insert rows below", "", |s, w, cx| s.insert_rows(true)),
        cmd!("Insert columns left", "", |s, w, cx| s.insert_cols(false)),
        cmd!("Insert columns right", "", |s, w, cx| s.insert_cols(true)),
        cmd!("Delete rows", "Ctrl+-", |s, w, cx| s.delete_rows()),
        cmd!("Delete columns", "", |s, w, cx| s.delete_cols()),
        cmd!("Insert or edit note", "Shift+F2", |s, w, cx| s.edit_note()),
        cmd!("Delete notes in selection", "", |s, w, cx| s.delete_note()),
        cmd!("Go to / name a range…", "Ctrl+G", |s, w, cx| s.focus_name_box()),
        cmd!("Find…", "Ctrl+F", |s, w, cx| s.open_find(false)),
        cmd!("Conditional formatting: greater than…", "", |s, w, cx| s.open_cf_dialog(crate::cond_ui::CfKind::Greater)),
        cmd!("Conditional formatting: less than…", "", |s, w, cx| s.open_cf_dialog(crate::cond_ui::CfKind::Less)),
        cmd!("Conditional formatting: duplicate values…", "", |s, w, cx| s.open_cf_dialog(crate::cond_ui::CfKind::Duplicates)),
        cmd!("Conditional formatting: use a formula…", "", |s, w, cx| s.open_cf_dialog(crate::cond_ui::CfKind::Formula)),
        cmd!("Conditional formatting: clear rules from selection", "", |s, w, cx| s.clear_cond_formats(false)),
        cmd!("Sparklines: line", "", |s, w, cx| s.insert_sparklines(xl_engine::chart::SparkKind::Line)),
        cmd!("Sparklines: column", "", |s, w, cx| s.insert_sparklines(xl_engine::chart::SparkKind::Column)),
        cmd!("Sparklines: win/loss", "", |s, w, cx| s.insert_sparklines(xl_engine::chart::SparkKind::WinLoss)),
        cmd!("Clear sparklines from selection", "", |s, w, cx| s.clear_sparklines()),
        cmd!("Insert column chart", "", |s, w, cx| s.insert_chart(xl_engine::chart::ChartKind::Column)),
        cmd!("Insert bar chart", "", |s, w, cx| s.insert_chart(xl_engine::chart::ChartKind::Bar)),
        cmd!("Insert line chart", "", |s, w, cx| s.insert_chart(xl_engine::chart::ChartKind::Line)),
        cmd!("Insert area chart", "", |s, w, cx| s.insert_chart(xl_engine::chart::ChartKind::Area)),
        cmd!("Insert pie chart", "", |s, w, cx| s.insert_chart(xl_engine::chart::ChartKind::Pie)),
        cmd!("Insert scatter chart", "", |s, w, cx| s.insert_chart(xl_engine::chart::ChartKind::Scatter)),
        cmd!("Goal Seek…", "", |s, w, cx| s.open_form(crate::solver_ui::FormKind::GoalSeek)),
        cmd!("Solver…", "", |s, w, cx| s.open_form(crate::solver_ui::FormKind::Solver)),
        cmd!("Insert pivot table", "", |s, w, cx| s.insert_pivot()),
        cmd!("Refresh pivot table", "", |s, w, cx| s.refresh_pivot()),
        cmd!("Format as table", "Ctrl+T", |s, w, cx| s.format_as_table()),
        cmd!("Convert table to range", "", |s, w, cx| s.convert_table_to_range()),
        cmd!("Table style: blue", "", |s, w, cx| s.update_table(|t| t.style = xl_engine::TableStyle::Blue)),
        cmd!("Table style: green", "", |s, w, cx| s.update_table(|t| t.style = xl_engine::TableStyle::Green)),
        cmd!("Table style: orange", "", |s, w, cx| s.update_table(|t| t.style = xl_engine::TableStyle::Orange)),
        cmd!("Table style: gray", "", |s, w, cx| s.update_table(|t| t.style = xl_engine::TableStyle::Gray)),
        cmd!("Table: banded rows on/off", "", |s, w, cx| s.update_table(|t| t.banded = !t.banded)),
        cmd!("Remove duplicates", "", |s, w, cx| s.remove_duplicates()),
        cmd!("Text to columns (detect delimiter)", "", |s, w, cx| s.text_to_columns(crate::data::Delimiter::Auto)),
        cmd!("Text to columns: comma", "", |s, w, cx| s.text_to_columns(crate::data::Delimiter::Comma)),
        cmd!("Text to columns: tab", "", |s, w, cx| s.text_to_columns(crate::data::Delimiter::Tab)),
        cmd!("Text to columns: semicolon", "", |s, w, cx| s.text_to_columns(crate::data::Delimiter::Semicolon)),
        cmd!("Text to columns: space", "", |s, w, cx| s.text_to_columns(crate::data::Delimiter::Space)),
        cmd!("Sort A → Z", "", |s, w, cx| s.sort_by_active(true)),
        cmd!("Sort Z → A", "", |s, w, cx| s.sort_by_active(false)),
        cmd!("Filter (toggle)", "Ctrl+Shift+L", |s, w, cx| s.toggle_filter()),
        cmd!("Find and replace…", "Ctrl+H", |s, w, cx| s.open_find(true)),
        cmd!("Fill down", "Ctrl+D", |s, w, cx| s.fill(true)),
        cmd!("Fill right", "Ctrl+R", |s, w, cx| s.fill(false)),
        cmd!("Clear contents", "Delete", |s, w, cx| s.clear_selection()),
        cmd!("Clear formatting", "", |s, w, cx| s.clear_formatting()),
        cmd!("Bold", "Ctrl+B", |s, w, cx| s.toggle_emphasis(Emphasis::Bold)),
        cmd!("Italic", "Ctrl+I", |s, w, cx| s.toggle_emphasis(Emphasis::Italic)),
        cmd!("Underline", "Ctrl+U", |s, w, cx| s.toggle_emphasis(Emphasis::Underline)),
        cmd!("Strikethrough", "Ctrl+5", |s, w, cx| s.toggle_emphasis(Emphasis::Strike)),
        cmd!("Align left", "", |s, w, cx| s.set_align(HAlign::Left)),
        cmd!("Align center", "", |s, w, cx| s.set_align(HAlign::Center)),
        cmd!("Align right", "", |s, w, cx| s.set_align(HAlign::Right)),
        cmd!("Wrap text", "", |s, w, cx| s.toggle_wrap()),
        cmd!("Merge and center", "", |s, w, cx| s.toggle_merge(true)),
        cmd!("Merge cells", "", |s, w, cx| s.toggle_merge(false)),
        cmd!("Unmerge cells", "", |s, w, cx| s.toggle_merge(false)),
        cmd!("Align top", "", |s, w, cx| s.set_valign(xl_engine::VAlign::Top)),
        cmd!("Align middle", "", |s, w, cx| s.set_valign(xl_engine::VAlign::Center)),
        cmd!("Align bottom", "", |s, w, cx| s.set_valign(xl_engine::VAlign::Bottom)),
        cmd!("Font size: 8", "", |s, w, cx| s.set_font_size(Some(8.0))),
        cmd!("Font size: 10", "", |s, w, cx| s.set_font_size(Some(10.0))),
        cmd!("Font size: 11 (default)", "", |s, w, cx| s.set_font_size(None)),
        cmd!("Font size: 12", "", |s, w, cx| s.set_font_size(Some(12.0))),
        cmd!("Font size: 14", "", |s, w, cx| s.set_font_size(Some(14.0))),
        cmd!("Font size: 18", "", |s, w, cx| s.set_font_size(Some(18.0))),
        cmd!("Font size: 24", "", |s, w, cx| s.set_font_size(Some(24.0))),
        cmd!("Freeze panes at selection", "", |s, w, cx| s.toggle_freeze()),
        cmd!("Freeze top row", "", |s, w, cx| s.set_freeze((1, 0))),
        cmd!("Freeze first column", "", |s, w, cx| s.set_freeze((0, 1))),
        cmd!("Unfreeze panes", "", |s, w, cx| s.set_freeze((0, 0))),
        cmd!("Group", "Alt+Shift+Right", |s, w, cx| s.group(false)),
        cmd!("Ungroup", "Alt+Shift+Left", |s, w, cx| s.group(true)),
        cmd!("Hide rows", "", |s, w, cx| s.set_hidden(true, true)),
        cmd!("Unhide rows", "", |s, w, cx| s.set_hidden(true, false)),
        cmd!("Hide columns", "", |s, w, cx| s.set_hidden(false, true)),
        cmd!("Unhide columns", "", |s, w, cx| s.set_hidden(false, false)),
        cmd!("Fit row height", "", |s, w, cx| s.autofit_row(s.active.row, w)),
        cmd!("All borders", "", |s, w, cx| s.apply_borders(BorderPreset::All)),
        cmd!("Outside borders", "", |s, w, cx| s.apply_borders(BorderPreset::Outside)),
        cmd!("Bottom border", "", |s, w, cx| s.apply_borders(BorderPreset::Bottom)),
        cmd!("No borders", "", |s, w, cx| s.apply_borders(BorderPreset::None)),
        cmd!("Format: General", "Ctrl+Shift+`", |s, w, cx| s.apply_format(None)),
        cmd!("Format: Number", "Ctrl+Shift+1", |s, w, cx| s.apply_format(Some("#,##0.00"))),
        cmd!("Format: Currency", "Ctrl+Shift+4", |s, w, cx| s.apply_format(Some("$#,##0.00"))),
        cmd!("Format: Accounting", "", |s, w, cx| s.apply_format(Some("_($* #,##0.00_);_($* (#,##0.00);_($* \"-\"??_);_(@_)"))),
        cmd!("Format: Percent", "Ctrl+Shift+5", |s, w, cx| s.apply_format(Some("0%"))),
        cmd!("Format: Date", "Ctrl+Shift+3", |s, w, cx| s.apply_format(Some("m/d/yyyy"))),
        cmd!("Format: Long date", "", |s, w, cx| s.apply_format(Some("dddd, mmmm d, yyyy"))),
        cmd!("Format: Time", "", |s, w, cx| s.apply_format(Some("h:mm AM/PM"))),
        cmd!("Format: Scientific", "", |s, w, cx| s.apply_format(Some("0.00E+00"))),
        cmd!("Format: Text", "", |s, w, cx| s.apply_format(Some("@"))),
        cmd!("Increase decimals", "", |s, w, cx| s.bump_decimals(1)),
        cmd!("Decrease decimals", "", |s, w, cx| s.bump_decimals(-1)),
        cmd!("Autofit column width", "Double-click header edge", |s, w, cx| s.autofit(s.active.col, w)),
        cmd!("Toggle dark mode", "", |s, w, cx| s.toggle_theme()),
        cmd!("Close window", "Ctrl+W", |s, w, cx| s.request(crate::lifecycle::After::Close, w, cx)),
        cmd!("Quit", "Ctrl+Q", |s, w, cx| s.request(crate::lifecycle::After::Quit, w, cx)),
    ]
}

/// Fuzzy subsequence score (higher is better); None if `query` doesn't match.
pub fn fuzzy_score(query: &str, target: &str) -> Option<i32> {
    let q: Vec<char> = query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = target.to_lowercase().chars().collect();
    let mut score = 0;
    let mut qi = 0;
    let mut prev: Option<usize> = None;
    for (ti, &c) in t.iter().enumerate() {
        if qi < q.len() && c == q[qi] {
            let word_start = ti == 0 || !t[ti - 1].is_alphanumeric();
            score += 1 + if word_start { 8 } else { 0 } + if prev == Some(ti.wrapping_sub(1)) { 5 } else { 0 };
            prev = Some(ti);
            qi += 1;
        }
    }
    (qi == q.len()).then_some(score - t.len() as i32 / 8)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Item {
    Command(usize),
    GoTo(CellRef),
    Function(Func),
}

pub struct Palette {
    pub query: String,
    pub selected: usize,
    commands: Vec<Command>,
}

impl Palette {
    pub fn new() -> Self {
        Self { query: String::new(), selected: 0, commands: commands() }
    }

    pub fn items(&self) -> Vec<Item> {
        let q = self.query.trim();
        let mut out = Vec::new();
        if let Some(c) = CellRef::parse(q) {
            out.push(Item::GoTo(c));
        }
        let mut scored: Vec<(i32, Item)> =
            self.commands.iter().enumerate().filter_map(|(i, c)| fuzzy_score(q, c.name).map(|s| (s, Item::Command(i)))).collect();
        if q.len() >= 2 {
            let fq = q.trim_start_matches('=').to_ascii_uppercase();
            for f in Func::ALL {
                if f.name().starts_with(&fq) {
                    // Exact-prefix function matches rank near the top.
                    scored.push((40 - f.name().len() as i32, Item::Function(*f)));
                }
            }
        }
        scored.sort_by_key(|s| std::cmp::Reverse(s.0));
        out.extend(scored.into_iter().map(|(_, i)| i));
        out.truncate(12);
        out
    }

    fn label(&self, item: Item) -> (String, String) {
        match item {
            Item::Command(i) => (self.commands[i].name.to_string(), self.commands[i].keys.to_string()),
            Item::GoTo(c) => (format!("Go to {c}"), "Enter".into()),
            Item::Function(f) => (format!("Insert ={}()", f.name()), f.signature().to_string()),
        }
    }
}

impl Spreadsheet {
    pub(crate) fn open_palette(&mut self) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        self.menu = None;
        self.palette = Some(Palette::new());
    }

    pub(crate) fn run_palette_item(&mut self, item: Item, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.palette.take() else { return };
        match item {
            Item::Command(i) => (p.commands[i].run)(self, window, cx),
            Item::GoTo(c) => self.select(c, false),
            Item::Function(f) => self.begin_edit(Some(format!("={}(", f.name())), EditMode::Enter),
        }
    }

    /// Key handling while the palette is open. Returns true if handled.
    pub(crate) fn key_palette(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(p) = self.palette.as_mut() else { return false };
        let n = p.items().len();
        match key {
            "escape" => self.palette = None,
            "up" => p.selected = p.selected.saturating_sub(1),
            "down" => p.selected = (p.selected + 1).min(n.saturating_sub(1)),
            "backspace" => {
                p.query.pop();
                p.selected = 0;
            }
            "enter" => {
                if let Some(&item) = p.items().get(p.selected) {
                    self.run_palette_item(item, window, cx);
                }
            }
            // Typed characters arrive through the input handler.
            _ => return false,
        }
        true
    }

    pub(crate) fn render_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let p = self.palette.as_ref().unwrap();
        let items = p.items();
        let rows = items.iter().enumerate().map(|(i, &item)| {
            let (label, hint) = p.label(item);
            let selected = i == p.selected;
            div()
                .id(("palette-item", i))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(12.))
                .px(px(12.))
                .py(px(7.))
                .rounded(px(6.))
                .cursor_pointer()
                .when(selected, |d| d.bg(t.header_selected).text_color(t.ink))
                .when(!selected, |d| d.hover(|s| s.bg(t.chrome_hover)))
                .child(div().child(SharedString::from(label)))
                .child(div().text_size(px(11.)).text_color(t.ink_muted).child(SharedString::from(hint)))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.run_palette_item(item, window, cx);
                    cx.notify();
                }))
        });
        let query: SharedString = if p.query.is_empty() { "Type a command, a cell like B12, or a function…".into() } else { p.query.clone().into() };
        div()
            .id("palette")
            .occlude()
            .w(px(560.))
            .p(px(8.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .rounded(px(10.))
            .bg(t.popover)
            .border_1()
            .border_color(t.divider)
            .shadow_lg()
            .text_size(px(13.))
            .text_color(t.ink)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(36.))
                    .px(px(10.))
                    .mb(px(4.))
                    .rounded(px(6.))
                    .bg(t.canvas)
                    .border_1()
                    .border_color(t.divider)
                    .child(div().text_color(if p.query.is_empty() { t.ink_muted } else { t.ink }).child(query))
                    .when(!p.query.is_empty(), |d| d.child(div().w(px(1.5)).h(px(16.)).bg(t.accent))),
            )
            .children(rows)
            .when(items.is_empty(), |d| d.child(div().px(px(12.)).py(px(8.)).text_color(t.ink_muted).font_weight(FontWeight::NORMAL).child("No matches")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_ranking() {
        assert!(fuzzy_score("bold", "Bold").unwrap() > fuzzy_score("bold", "Bottom border (old)").unwrap_or(-100));
        assert!(fuzzy_score("fmtcur", "Format: Currency").is_some());
        assert!(fuzzy_score("xyz", "Format: Currency").is_none());
        assert!(fuzzy_score("ns", "New sheet").unwrap() > fuzzy_score("ns", "Insert function").unwrap_or(-100));
    }

    #[test]
    fn items_include_goto_and_functions() {
        let mut p = Palette::new();
        p.query = "b12".into();
        assert_eq!(p.items()[0], Item::GoTo(CellRef::parse("B12").unwrap()));
        p.query = "xlo".into();
        assert!(p.items().contains(&Item::Function(Func::XLookup)));
        p.query = "save".into();
        let first = p.items()[0];
        assert!(matches!(first, Item::Command(i) if p.commands[i].name == "Save"));
    }
}
