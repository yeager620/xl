//! Scripts in the app: running workbook and user-library scripts (after
//! consent), macros as one undo step, macro shortcuts, and the script editor
//! panel.

use std::sync::Arc;

use gpui::{Context, FontWeight, InteractiveElement, IntoElement, Keystroke, MouseButton, ParentElement, ScrollEvent, SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px};
use xl_engine::udf::Array;
use xl_engine::{Addr, MAX_ROWS, Rgb, Shift, Value};
use xl_script::consent::{Consent, TrustStore};
use xl_script::macros::{block_inputs, cells_of, range_text, read_block, resolve};
use xl_script::{FunctionHost, Limits, MacroHost, ScriptError, ScriptMacro, ScriptModule};

use crate::app::{Field, Spreadsheet, UndoEntry};

/// Lines of the editor shown at once.
const VISIBLE_LINES: usize = 30;
const EDITOR_FONT: f32 = 13.0;

const STARTER: &str = r#"-- Worksheet functions: use them in cells, e.g. =DISCOUNT(A1, 10%)
xl.func("DISCOUNT", function(price, rate)
  return price * (1 - rate)
end, { description = "Price after a discount" })

-- Macros: run from the command palette (Ctrl+K, "Run script").
xl.macro("Bold the selection", function()
  xl.bold(xl.selection(), true)
end, { key = "ctrl-alt-b" })
"#;

pub(crate) struct ScriptState {
    /// The running worksheet functions (library plus allowed workbook scripts).
    pub(crate) host: Option<Arc<FunctionHost>>,
    pub(crate) trust: TrustStore,
    /// The user's own scripts (`~/.config/xl/scripts`).
    pub(crate) library: Vec<ScriptModule>,
    /// May the workbook's scripts run?
    pub(crate) allowed: bool,
    /// Asking whether to run the workbook's scripts.
    pub(crate) consent_prompt: bool,
    pub(crate) editor: Option<ScriptEditor>,
    /// A macro is running: its undo steps are being collected.
    pub(crate) recording: bool,
    /// The last undo removed a sheet a macro added; it can't be redone.
    pub(crate) redo_blocked: bool,
    /// Output of the last macro run (`print`).
    pub(crate) output: Vec<String>,
}

impl ScriptState {
    pub(crate) fn new() -> Self {
        ScriptState {
            host: None,
            trust: TrustStore::load_default(),
            library: xl_script::storage::user_library(),
            allowed: true,
            consent_prompt: false,
            editor: None,
            recording: false,
            redo_blocked: false,
            output: Vec::new(),
        }
    }

    /// No user library and remembered consent only in memory (tests).
    #[cfg(test)]
    pub(crate) fn isolated() -> Self {
        ScriptState { trust: TrustStore::at(None), library: Vec::new(), ..ScriptState::new() }
    }

    pub(crate) fn macros(&self) -> Vec<ScriptMacro> {
        self.host.as_ref().map(|h| h.catalog().macros.clone()).unwrap_or_default()
    }
}

/// The script editor: an editing copy of the workbook's scripts.
pub(crate) struct ScriptEditor {
    pub(crate) modules: Vec<ScriptModule>,
    pub(crate) current: usize,
    /// Byte offset of the caret in the current module.
    pub(crate) cursor: usize,
    /// First line shown.
    pub(crate) top: usize,
    pub(crate) errors: Vec<ScriptError>,
    /// Edited since last applied.
    pub(crate) changed: bool,
    pub(crate) font: SharedString,
}

impl ScriptEditor {
    pub(crate) fn text(&self) -> &str {
        self.modules.get(self.current).map_or("", |m| m.source.as_str())
    }

    fn text_mut(&mut self) -> Option<&mut String> {
        self.modules.get_mut(self.current).map(|m| &mut m.source)
    }

    pub(crate) fn insert(&mut self, s: &str) {
        let at = self.cursor;
        let Some(text) = self.text_mut() else { return };
        let at = at.min(text.len());
        text.insert_str(at, s);
        self.cursor = at + s.len();
        self.changed = true;
        self.scroll_to_caret();
    }

    fn line_col(&self) -> (usize, usize) {
        line_col(self.text(), self.cursor)
    }

    fn scroll_to_caret(&mut self) {
        let (line, _) = self.line_col();
        if line < self.top {
            self.top = line;
        } else if line >= self.top + VISIBLE_LINES {
            self.top = line + 1 - VISIBLE_LINES;
        }
    }

    fn goto(&mut self, line: usize, col: usize) {
        self.cursor = offset_of(self.text(), line, col);
        self.scroll_to_caret();
    }

    /// Handle a key; false if it isn't an editor key.
    fn key(&mut self, key: &str, ctrl: bool) -> bool {
        let text = self.text().to_string();
        let (line, col) = self.line_col();
        match key {
            "enter" if !ctrl => {
                let start = text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
                let indent: String = text[start..].chars().take_while(|c| *c == ' ' || *c == '\t').collect();
                self.insert(&format!("\n{indent}"));
            }
            "tab" => self.insert("  "),
            "backspace" if self.cursor > 0 => {
                let prev = text[..self.cursor].char_indices().next_back().map_or(0, |(i, _)| i);
                let at = self.cursor;
                if let Some(t) = self.text_mut() {
                    t.replace_range(prev..at, "");
                }
                self.cursor = prev;
                self.changed = true;
                self.scroll_to_caret();
            }
            "delete" if self.cursor < text.len() => {
                let next = text[self.cursor..].chars().next().map_or(self.cursor, |c| self.cursor + c.len_utf8());
                let at = self.cursor;
                if let Some(t) = self.text_mut() {
                    t.replace_range(at..next, "");
                }
                self.changed = true;
            }
            "left" => {
                self.cursor = text[..self.cursor].char_indices().next_back().map_or(0, |(i, _)| i);
                self.scroll_to_caret();
            }
            "right" => {
                self.cursor = text[self.cursor..].chars().next().map_or(self.cursor, |c| self.cursor + c.len_utf8());
                self.scroll_to_caret();
            }
            "up" if line > 0 => self.goto(line - 1, col),
            "down" => self.goto(line + 1, col),
            "pageup" => self.goto(line.saturating_sub(VISIBLE_LINES), col),
            "pagedown" => self.goto(line + VISIBLE_LINES, col),
            "home" if ctrl => self.goto(0, 0),
            "end" if ctrl => self.goto(usize::MAX, 0),
            "home" => self.goto(line, 0),
            "end" => self.goto(line, usize::MAX),
            "backspace" | "delete" | "up" => {}
            _ => return false,
        }
        true
    }
}

/// (line, column in chars) of a byte offset.
pub(crate) fn line_col(text: &str, at: usize) -> (usize, usize) {
    let at = at.min(text.len());
    let before = &text[..at];
    let line = before.matches('\n').count();
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    (line, before[start..].chars().count())
}

/// Byte offset of (line, column), clamped to the text.
pub(crate) fn offset_of(text: &str, line: usize, col: usize) -> usize {
    let mut start = 0;
    for _ in 0..line {
        match text[start..].find('\n') {
            Some(i) => start += i + 1,
            None => return text.len(),
        }
    }
    let end = text[start..].find('\n').map_or(text.len(), |i| start + i);
    text[start..end].char_indices().nth(col).map_or(end, |(i, _)| start + i)
}

/// Does a shortcut like "ctrl-alt-h" match this keystroke?
pub(crate) fn key_matches(spec: &str, ks: &Keystroke) -> bool {
    let spec = spec.trim().to_ascii_lowercase();
    let (mods, key) = match spec.rsplit_once('-') {
        Some((m, "")) => (m.strip_suffix('-').unwrap_or(m), "-"),
        Some((m, k)) => (m, k),
        None => ("", spec.as_str()),
    };
    let (mut ctrl, mut alt, mut shift, mut platform) = (false, false, false, false);
    for m in mods.split('-').filter(|m| !m.is_empty()) {
        match m {
            "ctrl" | "control" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            "super" | "cmd" | "win" | "platform" => platform = true,
            _ => return false,
        }
    }
    let m = ks.modifiers;
    key == ks.key && (ctrl, alt, shift, platform) == (m.control, m.alt, m.shift, m.platform)
}

fn pick_mono_font(window: &Window) -> SharedString {
    let names = window.text_system().all_font_names();
    ["DejaVu Sans Mono", "Ubuntu Mono", "Noto Sans Mono", "Liberation Mono", "Source Code Pro", "Cascadia Mono", "Fira Mono"]
        .into_iter()
        .find(|f| names.iter().any(|n| n == f))
        .unwrap_or(crate::FONT)
        .into()
}

impl Spreadsheet {
    /// The workbook was replaced (opened, new, restored): decide whether its
    /// scripts may run and load them.
    pub(crate) fn scripts_loaded(&mut self) {
        self.scripts.editor = None;
        match self.scripts.trust.consent(self.wb.scripts()) {
            Consent::NoScripts | Consent::Trusted => {
                self.scripts.allowed = true;
                self.scripts.consent_prompt = false;
            }
            Consent::Ask => {
                self.scripts.allowed = false;
                self.scripts.consent_prompt = true;
            }
        }
        self.reload_scripts();
        if self.unsupported.contains(&"macros") {
            self.message = Some(("This file contains VBA macros, which xl can't run (they're kept when you save)".into(), false));
        }
    }

    /// The scripts that may run now.
    fn runnable_modules(&self) -> Vec<ScriptModule> {
        let mut modules = self.scripts.library.clone();
        if self.scripts.allowed {
            modules.extend(self.wb.scripts().iter().cloned());
        }
        modules
    }

    /// Rebuild the worksheet functions from the runnable scripts.
    pub(crate) fn reload_scripts(&mut self) -> Vec<ScriptError> {
        let modules = self.runnable_modules();
        let had = self.scripts.host.is_some();
        if modules.iter().all(|m| m.source.trim().is_empty()) {
            self.scripts.host = None;
            if had {
                self.last_stats = Some(self.wb.set_user_functions(None));
            }
            return Vec::new();
        }
        match FunctionHost::new(&modules, Limits::FUNCTIONS) {
            Ok(host) => {
                let host = Arc::new(host);
                let errors = host.catalog().errors.clone();
                self.scripts.host = Some(host.clone());
                self.last_stats = Some(self.wb.set_user_functions(Some(host)));
                errors
            }
            Err(e) => {
                self.scripts.host = None;
                self.last_stats = Some(self.wb.set_user_functions(None));
                vec![e]
            }
        }
    }

    pub(crate) fn allow_scripts(&mut self) {
        if let Err(e) = self.scripts.trust.trust(self.wb.scripts()) {
            self.message = Some((format!("Couldn't remember the answer: {e}"), true));
        }
        self.scripts.allowed = true;
        self.scripts.consent_prompt = false;
        let errors = self.reload_scripts();
        if let Some(e) = errors.first() {
            self.message = Some((format!("Script error: {e}"), true));
        }
    }

    pub(crate) fn deny_scripts(&mut self) {
        self.scripts.consent_prompt = false;
        self.message = Some(("The workbook's scripts are off. Cells using them show the values saved in the file.".into(), false));
    }

    fn key_consent(&mut self, key: &str) -> bool {
        match key {
            "escape" => self.deny_scripts(),
            "enter" => self.allow_scripts(),
            _ => {}
        }
        true
    }

    /// Why the active cell's script function failed, for the status bar.
    pub(crate) fn script_error_status(&self) -> Option<(SharedString, bool)> {
        let at = self.at(self.active);
        if !matches!(self.wb.value(at), Value::Error(_)) {
            return None;
        }
        let msg = self.scripts.host.as_ref()?.cell_error(at)?;
        Some((format!("Script error: {msg}").into(), true))
    }

    // ----------------------------------------------------------- macros

    /// Run the macro bound to this keystroke, if there is one.
    pub(crate) fn try_macro_key(&mut self, ks: &Keystroke) -> bool {
        let Some(m) = self.scripts.macros().into_iter().find(|m| m.key.as_deref().is_some_and(|k| key_matches(k, ks))) else { return false };
        self.run_macro_named(&m.name);
        true
    }

    /// Run a macro: every change it makes is one undo step.
    pub(crate) fn run_macro_named(&mut self, name: &str) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        if !self.scripts.allowed && !self.wb.scripts().is_empty() {
            self.scripts.consent_prompt = true;
            return;
        }
        let modules = self.runnable_modules();
        let start = self.undo.len();
        self.scripts.recording = true;
        let mut host = AppHost { s: self, messages: Vec::new() };
        let outcome = xl_script::run_macro(&modules, name, &mut host, Limits::MACROS);
        let messages = std::mem::take(&mut host.messages);
        self.scripts.recording = false;
        let steps = self.undo.split_off(start.min(self.undo.len()));
        if !steps.is_empty() {
            self.push_undo(UndoEntry::Group(steps));
            self.mark_dirty();
        }
        self.scripts.output = outcome.output;
        self.message = Some(match (&outcome.error, messages.last()) {
            (Some(e), _) => (format!("Macro “{name}” stopped: {e}"), true),
            (None, Some(m)) => (m.clone(), false),
            (None, None) => (format!("Ran “{name}”"), false),
        });
        if let Some(ed) = self.scripts.editor.as_mut() {
            ed.errors = outcome.error.into_iter().collect();
        }
    }

    // ----------------------------------------------------------- editor

    pub(crate) fn open_script_editor(&mut self, window: &mut Window) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        if self.scripts.editor.is_none() {
            let mut modules = self.wb.scripts().to_vec();
            if modules.is_empty() {
                modules.push(ScriptModule { name: "Module1".into(), source: STARTER.into() });
            }
            let errors = self.scripts.host.as_ref().map(|h| h.catalog().errors.clone()).unwrap_or_default();
            self.scripts.editor = Some(ScriptEditor { modules, current: 0, cursor: 0, top: 0, errors, changed: false, font: pick_mono_font(window) });
        }
        self.field = Some(Field::Script);
    }

    /// Save the editor's scripts into the workbook and load them. Scripts
    /// written here are the user's own, so they're trusted.
    pub(crate) fn apply_scripts(&mut self) {
        let Some(ed) = self.scripts.editor.as_mut() else { return };
        let modules: Vec<ScriptModule> = ed.modules.iter().filter(|m| !m.source.trim().is_empty()).cloned().collect();
        ed.changed = false;
        if modules.as_slice() != self.wb.scripts() {
            self.wb.set_scripts(modules);
            self.mark_dirty();
        }
        if let Err(e) = self.scripts.trust.trust(self.wb.scripts()) {
            self.message = Some((format!("Couldn't remember the scripts as trusted: {e}"), true));
        }
        self.scripts.allowed = true;
        self.scripts.consent_prompt = false;
        let errors = self.reload_scripts();
        let n = self.scripts.host.as_ref().map_or((0, 0), |h| (h.catalog().functions.len(), h.catalog().macros.len()));
        self.message = Some(match errors.first() {
            Some(e) => (format!("Script error: {e}"), true),
            None => (format!("Scripts saved: {} functions, {} macros", n.0, n.1), false),
        });
        if let Some(ed) = self.scripts.editor.as_mut() {
            ed.errors = errors;
        }
    }

    pub(crate) fn close_script_editor(&mut self) {
        if self.scripts.editor.as_ref().is_some_and(|e| e.changed) {
            self.apply_scripts();
        }
        self.scripts.editor = None;
        if self.field == Some(Field::Script) {
            self.field = None;
        }
    }

    /// The first macro of the module being edited (what "Run" runs).
    fn editor_macro(&self) -> Option<String> {
        let ed = self.scripts.editor.as_ref()?;
        let module = &ed.modules.get(ed.current)?.name;
        let macros = self.scripts.macros();
        macros.iter().find(|m| &m.module == module).or(macros.first()).map(|m| m.name.clone())
    }

    fn apply_and_run(&mut self) {
        self.apply_scripts();
        if self.scripts.editor.as_ref().is_some_and(|e| !e.errors.is_empty()) {
            return;
        }
        match self.editor_macro() {
            Some(name) => self.run_macro_named(&name),
            None => self.message = Some(("These scripts define no macros to run (xl.macro(\"Name\", function() … end))".into(), false)),
        }
    }

    pub(crate) fn key_script(&mut self, key: &str, ctrl: bool) -> bool {
        match key {
            "escape" => {
                self.field = None;
                return true;
            }
            "s" if ctrl => {
                self.apply_scripts();
                return true;
            }
            "enter" if ctrl => {
                self.apply_and_run();
                return true;
            }
            _ => {}
        }
        self.scripts.editor.as_mut().is_some_and(|ed| ed.key(key, ctrl))
    }

    fn add_module(&mut self) {
        let Some(ed) = self.scripts.editor.as_mut() else { return };
        let mut n = ed.modules.len() + 1;
        while ed.modules.iter().any(|m| m.name == format!("Module{n}")) {
            n += 1;
        }
        ed.modules.push(ScriptModule { name: format!("Module{n}"), source: String::new() });
        ed.current = ed.modules.len() - 1;
        ed.cursor = 0;
        ed.top = 0;
        ed.changed = true;
    }

    fn delete_module(&mut self) {
        let Some(ed) = self.scripts.editor.as_mut() else { return };
        if ed.current < ed.modules.len() {
            ed.modules.remove(ed.current);
            ed.current = ed.current.saturating_sub(1);
            ed.cursor = 0;
            ed.top = 0;
            ed.changed = true;
        }
    }

    pub(crate) fn render_script_panel(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let ed = self.scripts.editor.as_ref()?;
        let t = self.theme;
        let focused = self.field == Some(Field::Script);
        let text = ed.text();
        let (cur_line, cur_col) = line_col(text, ed.cursor);
        let module_name = ed.modules.get(ed.current).map(|m| m.name.clone()).unwrap_or_default();
        let error_lines: Vec<usize> = ed.errors.iter().filter(|e| e.module.as_deref() == Some(module_name.as_str())).filter_map(|e| e.line.map(|l| l as usize - 1)).collect();
        let small_button = |id: &'static str, label: &'static str| {
            div().id(id).px(px(8.)).h(px(24.)).flex().items_center().rounded(px(4.)).cursor_pointer().text_size(px(12.5)).border_1().border_color(t.divider).hover(|s| s.bg(t.chrome_hover)).child(label)
        };

        let tabs = div().flex().flex_row().flex_wrap().gap(px(4.)).children(ed.modules.iter().enumerate().map(|(i, m)| {
            let on = i == ed.current;
            div()
                .id(("script-tab", i))
                .px(px(8.))
                .h(px(22.))
                .flex()
                .items_center()
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(px(12.5))
                .when(on, |d| d.bg(t.accent).text_color(t.on_accent))
                .when(!on, |d| d.text_color(t.ink).hover(|s| s.bg(t.chrome_hover)))
                .child(SharedString::from(m.name.clone()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(ed) = this.scripts.editor.as_mut() {
                        ed.current = i;
                        ed.cursor = 0;
                        ed.top = 0;
                    }
                    this.field = Some(Field::Script);
                    cx.notify();
                }))
        }));
        let tabs = tabs.child(small_button("script-add-module", "+").on_click(cx.listener(|this, _, _, cx| {
            this.add_module();
            this.field = Some(Field::Script);
            cx.notify();
        })));

        let lines: Vec<&str> = text.split('\n').collect();
        let rows = lines.iter().enumerate().skip(ed.top).take(VISIBLE_LINES).map(|(i, line)| {
            let is_err = error_lines.contains(&i);
            let mut row = div()
                .id(("script-line", i))
                .flex()
                .flex_row()
                .h(px(EDITOR_FONT + 5.))
                .items_center()
                .whitespace_nowrap()
                .child(div().w(px(36.)).flex_none().pr(px(8.)).flex().justify_end().text_color(if is_err { t.error } else { t.ink_faint }).child(SharedString::from((i + 1).to_string())))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        if let Some(ed) = this.scripts.editor.as_mut() {
                            ed.goto(i, usize::MAX);
                        }
                        this.field = Some(Field::Script);
                        cx.stop_propagation();
                        cx.notify();
                    }),
                );
            let display = line.replace('\t', "  ");
            if i == cur_line && focused {
                let split = display.char_indices().nth(cur_col).map_or(display.len(), |(b, _)| b);
                row = row
                    .child(SharedString::from(display[..split].to_string()))
                    .child(div().w(px(1.5)).h(px(EDITOR_FONT + 2.)).bg(t.accent))
                    .child(SharedString::from(display[split..].to_string()));
            } else {
                row = row.child(SharedString::from(display));
            }
            row.when(is_err, |d| d.bg(gpui::hsla(0., 0.7, 0.5, 0.12)))
        });
        let editor = div()
            .id("script-editor")
            .flex_1()
            .min_h(px(200.))
            .py(px(4.))
            .rounded(px(4.))
            .bg(t.canvas)
            .border_1()
            .border_color(if focused { t.accent } else { t.divider })
            .font_family(ed.font.clone())
            .text_size(px(EDITOR_FONT))
            .text_color(t.ink)
            .overflow_hidden()
            .cursor_text()
            .children(rows)
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                this.field = Some(Field::Script);
                cx.notify();
            }))
            .on_scroll(cx.listener(|this, ev: &ScrollEvent, _, cx| {
                if let Some(ed) = this.scripts.editor.as_mut() {
                    let dy = f32::from(ev.delta.pixel_delta(px(EDITOR_FONT + 5.)).y);
                    let n = ed.text().matches('\n').count();
                    let lines = (dy.abs() / (EDITOR_FONT + 5.)).ceil().max(1.0) as usize;
                    ed.top = if dy > 0.0 { ed.top.saturating_sub(lines) } else { (ed.top + lines).min(n) };
                }
                cx.notify();
            }));

        let errors = div().flex().flex_col().gap(px(2.)).children(ed.errors.iter().enumerate().map(|(i, e)| {
            let target = e.module.as_ref().and_then(|m| ed.modules.iter().position(|x| &x.name == m)).zip(e.line);
            div()
                .id(("script-error", i))
                .text_size(px(12.5))
                .text_color(t.error)
                .cursor_pointer()
                .child(SharedString::from(e.to_string()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let (Some(ed), Some((m, line))) = (this.scripts.editor.as_mut(), target) {
                        ed.current = m;
                        ed.goto(line as usize - 1, 0);
                    }
                    this.field = Some(Field::Script);
                    cx.notify();
                }))
        }));

        let catalog = self.scripts.host.as_ref().map(|h| h.catalog().clone()).unwrap_or_default();
        let functions = catalog.functions.iter().map(|f| {
            let label = format!("={}(){}", f.name, if f.volatile { "  · volatile" } else { "" });
            div().text_size(px(12.5)).text_color(t.ink).child(SharedString::from(label)).children(f.description.clone().map(|d| div().text_size(px(11.5)).text_color(t.ink_muted).child(SharedString::from(d))))
        });
        let macros = catalog.macros.iter().enumerate().map(|(i, m)| {
            let name = m.name.clone();
            div()
                .id(("script-macro", i))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .px(px(4.))
                .rounded(px(3.))
                .cursor_pointer()
                .text_size(px(12.5))
                .hover(|s| s.bg(t.chrome_hover))
                .child(div().text_color(t.accent).child("▶"))
                .child(div().flex_1().text_color(t.ink).child(SharedString::from(m.name.clone())))
                .children(m.key.clone().map(|k| div().text_size(px(11.)).text_color(t.ink_muted).child(SharedString::from(k))))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.run_macro_named(&name);
                    cx.notify();
                }))
        });
        let output = self.scripts.output.iter().rev().take(6).rev().map(|l| div().text_size(px(12.)).text_color(t.ink_muted).child(SharedString::from(l.clone())));

        Some(
            div()
                .id("script-panel")
                .w(px(560.))
                .h_full()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(12.))
                .bg(t.chrome)
                .border_l_1()
                .border_color(t.divider)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(div().flex_1().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child("Scripts"))
                        .child(small_button("script-apply", "Save").on_click(cx.listener(|this, _, _, cx| {
                            this.apply_scripts();
                            cx.notify();
                        })))
                        .child(small_button("script-run", "Run").on_click(cx.listener(|this, _, _, cx| {
                            this.apply_and_run();
                            cx.notify();
                        })))
                        .child(small_button("script-delete-module", "Delete script").on_click(cx.listener(|this, _, _, cx| {
                            this.delete_module();
                            cx.notify();
                        })))
                        .child(small_button("script-close", "Close").on_click(cx.listener(|this, _, _, cx| {
                            this.close_script_editor();
                            cx.notify();
                        }))),
                )
                .child(tabs)
                .child(editor)
                .child(div().text_size(px(11.5)).text_color(t.ink_muted).child(SharedString::from(format!(
                    "Line {}, column {}{} · Ctrl+S save · Ctrl+Enter save and run · Esc back to the sheet",
                    cur_line + 1,
                    cur_col + 1,
                    if ed.changed { " · unsaved" } else { "" }
                ))))
                .when(!ed.errors.is_empty(), |d| d.child(errors))
                .child(
                    div()
                        .id("script-catalog")
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .max_h(px(220.))
                        .overflow_y_scroll()
                        .when(!catalog.functions.is_empty(), |d| d.child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink_muted).child("Functions")))
                        .children(functions)
                        .when(!catalog.macros.is_empty(), |d| d.child(div().mt(px(4.)).text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink_muted).child("Macros")))
                        .children(macros)
                        .when(!self.scripts.output.is_empty(), |d| d.child(div().mt(px(4.)).text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink_muted).child("Output")))
                        .children(output),
                )
                .into_any_element(),
        )
    }

    pub(crate) fn render_consent_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let n = self.wb.scripts().len();
        let button = |id: &'static str, label: &'static str| div().id(id).flex().items_center().justify_center().h(px(32.)).px(px(14.)).rounded(px(4.)).cursor_pointer().text_size(px(14.)).child(label);
        div()
            .id("consent-scrim")
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
                    .id("consent")
                    .w(px(460.))
                    .p(px(20.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .rounded(px(8.))
                    .bg(t.popover)
                    .border_1()
                    .border_color(t.divider)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child(SharedString::from(format!("“{}” contains {n} script{}", self.file_name(), if n == 1 { "" } else { "s" }))))
                    .child(div().text_size(px(14.)).text_color(t.ink_muted).child(
                        "Scripts can define worksheet functions and macros. They run sandboxed — no access to your files, the network or other programs — but only run them if you trust where this workbook came from.",
                    ))
                    .child(
                        div()
                            .mt(px(12.))
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .child(div().flex_1())
                            .child(button("consent-deny", "Don’t Run").text_color(t.ink).border_1().border_color(t.divider).hover(|s| s.bg(t.chrome_hover)).on_click(cx.listener(|this, _, _, cx| {
                                this.deny_scripts();
                                cx.notify();
                            })))
                            .child(button("consent-allow", "Run Scripts").bg(t.accent).text_color(t.on_accent).font_weight(FontWeight::SEMIBOLD).on_click(cx.listener(|this, _, _, cx| {
                                this.allow_scripts();
                                cx.notify();
                            }))),
                    ),
            )
    }

    pub(crate) fn handle_consent_key(&mut self, key: &str) -> bool {
        self.key_consent(key)
    }
}

/// The macro API on the open workbook. Each change pushes its own undo
/// step; [`Spreadsheet::run_macro_named`] groups them into one.
struct AppHost<'a> {
    s: &'a mut Spreadsheet,
    messages: Vec<String>,
}

impl AppHost<'_> {
    fn sheet_named(&self, name: &str) -> Result<xl_engine::SheetId, String> {
        self.s.wb.sheet_id(name).ok_or_else(|| format!("there's no sheet named “{name}”"))
    }

    fn restyle(&mut self, reference: &str, f: impl Fn(&mut xl_engine::Style)) -> Result<(), String> {
        let (sheet, r) = resolve(&self.s.wb, self.s.cur, reference)?;
        let cells = cells_of(sheet, r)?;
        let undo = self.s.snapshots(cells.iter().copied());
        for a in cells {
            let mut st = self.s.wb.style(a).clone();
            f(&mut st);
            self.s.wb.set_style(a, &st);
        }
        self.s.push_undo(UndoEntry::Cells(undo));
        Ok(())
    }

    fn shift(&mut self, op: Shift) {
        let undo = self.s.structural(op);
        self.s.push_undo(undo);
    }
}

impl MacroHost for AppHost<'_> {
    fn get(&mut self, reference: &str) -> Result<Array, String> {
        let (sheet, r) = resolve(&self.s.wb, self.s.cur, reference)?;
        read_block(&self.s.wb, sheet, r)
    }

    fn set(&mut self, reference: &str, values: &Array) -> Result<(), String> {
        let (sheet, r) = resolve(&self.s.wb, self.s.cur, reference)?;
        self.s.apply_inputs(block_inputs(sheet, r, values)?);
        Ok(())
    }

    fn formula(&mut self, reference: &str) -> Result<Option<String>, String> {
        let (sheet, r) = resolve(&self.s.wb, self.s.cur, reference)?;
        let a = Addr::new(sheet, r.start);
        Ok(self.s.wb.formula_source(a).map(|_| self.s.wb.input_text(a)))
    }

    fn set_formula(&mut self, reference: &str, input: &str) -> Result<(), String> {
        let (sheet, r) = resolve(&self.s.wb, self.s.cur, reference)?;
        if let Some(body) = input.strip_prefix('=')
            && let Err(e) = xl_engine::formula::parse_formula(body, self.s.wb.sheets())
        {
            return Err(format!("formula error: {e}"));
        }
        self.s.apply_inputs(vec![(Addr::new(sheet, r.start), input.to_string())]);
        Ok(())
    }

    fn selection(&mut self) -> String {
        range_text(self.s.selection())
    }

    fn select(&mut self, reference: &str) -> Result<(), String> {
        let (sheet, r) = resolve(&self.s.wb, self.s.cur, reference)?;
        self.s.switch_sheet(sheet);
        self.s.anchor = r.end;
        self.s.active = r.start;
        self.s.ensure_visible(r.start);
        Ok(())
    }

    fn sheets(&mut self) -> Vec<String> {
        self.s.wb.sheet_ids().iter().map(|&id| self.s.wb.sheet_name(id).to_string()).collect()
    }

    fn active_sheet(&mut self) -> String {
        self.s.wb.sheet_name(self.s.cur).to_string()
    }

    fn add_sheet(&mut self, name: Option<&str>) -> Result<String, String> {
        let id = self.s.wb.add_sheet(name)?;
        self.s.push_undo(UndoEntry::AddedSheet(id));
        Ok(self.s.wb.sheet_name(id).to_string())
    }

    fn rename_sheet(&mut self, from: &str, to: &str) -> Result<(), String> {
        let id = self.sheet_named(from)?;
        self.s.wb.rename_sheet(id, to)?;
        self.s.push_undo(UndoEntry::RenamedSheet { id, name: from.to_string() });
        Ok(())
    }

    fn activate_sheet(&mut self, name: &str) -> Result<(), String> {
        let id = self.sheet_named(name)?;
        self.s.switch_sheet(id);
        Ok(())
    }

    fn set_bold(&mut self, reference: &str, on: bool) -> Result<(), String> {
        self.restyle(reference, |st| st.bold = on)
    }

    fn set_fill(&mut self, reference: &str, color: Option<Rgb>) -> Result<(), String> {
        self.restyle(reference, |st| st.fill = color)
    }

    fn set_number_format(&mut self, reference: &str, code: Option<&str>) -> Result<(), String> {
        let (sheet, r) = resolve(&self.s.wb, self.s.cur, reference)?;
        let cells = cells_of(sheet, r)?;
        let undo = self.s.snapshots(cells.iter().copied());
        for a in cells {
            self.s.wb.set_format(a, code);
        }
        self.s.push_undo(UndoEntry::Cells(undo));
        Ok(())
    }

    fn insert_rows(&mut self, at: u32, count: u32) -> Result<(), String> {
        self.shift(Shift::InsertRows { sheet: self.s.cur, at, count: count.min(MAX_ROWS - at) });
        Ok(())
    }

    fn delete_rows(&mut self, at: u32, count: u32) -> Result<(), String> {
        self.shift(Shift::DeleteRows { sheet: self.s.cur, at, count: count.min(MAX_ROWS - at) });
        Ok(())
    }

    fn message(&mut self, text: &str) {
        self.messages.push(text.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_positions() {
        let t = "ab\ncdé\n\nx";
        assert_eq!(line_col(t, 0), (0, 0));
        assert_eq!(line_col(t, 4), (1, 1));
        assert_eq!(offset_of(t, 1, 3), 7);
        assert_eq!(offset_of(t, 1, 99), 7);
        assert_eq!(offset_of(t, 2, 5), 8);
        assert_eq!(offset_of(t, 99, 0), t.len());
    }

    #[test]
    fn shortcuts_match() {
        let ks = Keystroke::parse("ctrl-alt-h").unwrap();
        assert!(key_matches("ctrl-alt-h", &ks));
        assert!(key_matches("Alt-Ctrl-H", &ks));
        assert!(!key_matches("ctrl-h", &ks));
        assert!(!key_matches("ctrl-alt-j", &ks));
        assert!(key_matches("ctrl--", &Keystroke::parse("ctrl--").unwrap()));
    }

    #[test]
    fn editor_keys() {
        let mut ed = ScriptEditor { modules: vec![ScriptModule { name: "M".into(), source: "  if x then".into() }], current: 0, cursor: 11, top: 0, errors: vec![], changed: false, font: "x".into() };
        ed.key("enter", false);
        ed.insert("y = 1");
        assert_eq!(ed.text(), "  if x then\n  y = 1");
        ed.key("backspace", false);
        ed.key("up", false);
        ed.key("end", false);
        ed.insert(" -- c");
        assert_eq!(ed.text(), "  if x then -- c\n  y = ");
        assert!(ed.changed);
    }
}
