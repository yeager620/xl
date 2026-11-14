//! Keeping work safe: the unsaved-changes dialog, periodic autosave of a
//! recovery copy, and offering that copy back after a crash.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gpui::{Context, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div, px, svg};

use crate::app::Spreadsheet;

/// What to do once unsaved changes are dealt with.
#[derive(Clone, Debug, PartialEq)]
pub enum After {
    Close,
    Quit,
    Open(PathBuf),
}

/// A recovery copy left behind by an instance that didn't exit cleanly.
#[derive(Clone, Debug)]
pub struct Recovery {
    pub file: PathBuf,
    /// The document it was a copy of (None if it was never saved).
    pub original: Option<PathBuf>,
    pub modified: SystemTime,
}

const AUTOSAVE_EVERY: Duration = Duration::from_secs(30);
/// Writing a recovery copy happens on the UI thread; skip very large books.
const AUTOSAVE_MAX_CELLS: usize = 300_000;

pub fn recovery_dir() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    state.join("xl/recovery")
}

/// Is `pid` a running xl process? (Recovery files of live instances aren't crashes.)
fn pid_is_running_xl(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == "xl")
}

/// The newest recovery copy left by an instance that is no longer running.
pub fn find_recovery(dir: &Path) -> Option<Recovery> {
    let mut best: Option<Recovery> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let file = entry.path();
        if file.extension().is_none_or(|e| e != "xlsx") {
            continue;
        }
        let Some(pid) = file.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u32>().ok()) else { continue };
        if pid == std::process::id() || pid_is_running_xl(pid) {
            continue;
        }
        let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        let original = std::fs::read_to_string(file.with_extension("path")).ok().map(|s| PathBuf::from(s.trim())).filter(|p| !p.as_os_str().is_empty());
        if best.as_ref().is_none_or(|b| modified > b.modified) {
            best = Some(Recovery { file, original, modified });
        }
    }
    best
}

pub fn remove_recovery(file: &Path) {
    // Missing files are fine: there may never have been an autosave.
    std::fs::remove_file(file).ok();
    std::fs::remove_file(file.with_extension("path")).ok();
}

fn ago(t: SystemTime) -> String {
    let secs = SystemTime::now().duration_since(t).map_or(0, |d| d.as_secs());
    match secs {
        0..=59 => "less than a minute ago".into(),
        60..=119 => "1 minute ago".into(),
        120..=3599 => format!("{} minutes ago", secs / 60),
        3600..=7199 => "1 hour ago".into(),
        7200..=172_799 => format!("{} hours ago", secs / 3600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

impl Spreadsheet {
    /// This instance's recovery copy.
    pub(crate) fn recovery_file(&self) -> PathBuf {
        self.recovery_dir.join(format!("{}.xlsx", std::process::id()))
    }

    pub(crate) fn start_autosave(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(AUTOSAVE_EVERY).await;
                if this.update(cx, |this, _| this.autosave()).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    pub(crate) fn autosave(&mut self) {
        if !self.dirty || self.autosaved_generation == Some(self.edit_generation) {
            return;
        }
        let cells: usize = self.wb.sheet_ids().iter().map(|&s| self.wb.store(s).len()).sum();
        if cells > AUTOSAVE_MAX_CELLS {
            return;
        }
        let file = self.recovery_file();
        if std::fs::create_dir_all(&self.recovery_dir).is_err() {
            return;
        }
        if xl_io::save(&self.wb, &file, self.cur).is_ok() {
            let original = self.path.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            std::fs::write(file.with_extension("path"), original).ok();
            self.autosaved_generation = Some(self.edit_generation);
        }
    }

    /// The document is safe on disk (or deliberately discarded): drop our recovery copy.
    pub(crate) fn clear_recovery(&mut self) {
        remove_recovery(&self.recovery_file());
        self.autosaved_generation = None;
    }

    /// Close, quit or open another file — asking first if there are unsaved changes.
    pub(crate) fn request(&mut self, after: After, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        if self.dirty {
            self.pending = Some(after);
            self.menu = None;
            self.palette = None;
        } else {
            self.perform(after, window, cx);
        }
        cx.notify();
    }

    pub(crate) fn perform(&mut self, after: After, window: &mut Window, cx: &mut Context<Self>) {
        self.pending = None;
        match after {
            After::Close => {
                self.clear_recovery();
                self.close_confirmed = true;
                window.remove_window();
            }
            After::Quit => {
                self.clear_recovery();
                self.close_confirmed = true;
                cx.quit();
            }
            After::Open(path) => {
                self.clear_recovery();
                self.load(path, cx);
            }
        }
    }

    /// The window manager asked to close the window.
    pub(crate) fn should_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.close_confirmed || !self.dirty {
            self.clear_recovery();
            return true;
        }
        self.pending = Some(After::Close);
        cx.notify();
        false
    }

    pub(crate) fn dialog_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(after) = self.pending.clone() else { return };
        if self.would_lose_parts() {
            self.lossy_prompt = true;
            return;
        }
        match self.path.clone() {
            Some(p) => {
                self.save_to(p);
                if !self.dirty {
                    self.perform(after, window, cx);
                }
            }
            None => {
                // Never saved: ask where, then carry on.
                let dir = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
                let rx = cx.prompt_for_new_path(&dir, Some("Untitled.xlsx"));
                cx.spawn_in(window, async move |this, cx| {
                    if let Ok(Ok(Some(mut path))) = rx.await {
                        if xl_io::FileKind::of(&path).is_none() {
                            path.set_extension("xlsx");
                        }
                        this.update_in(cx, |this, window, cx| {
                            this.save_to(path);
                            if !this.dirty {
                                this.perform(after, window, cx);
                            }
                            cx.notify();
                        })
                        .ok();
                    }
                })
                .detach();
            }
        }
        cx.notify();
    }

    pub(crate) fn restore_recovery(&mut self, cx: &mut Context<Self>) {
        let Some(r) = self.recovery_offer.take() else { return };
        self.restore_as = Some((r.original.clone(), r.file.clone()));
        self.load(r.file, cx);
    }

    pub(crate) fn discard_recovery_offer(&mut self) {
        if let Some(r) = self.recovery_offer.take() {
            remove_recovery(&r.file);
        }
    }

    /// Keyboard handling while the dialog is up. Returns true if handled.
    pub(crate) fn key_dialog(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.pending.is_none() {
            return false;
        }
        match key {
            "enter" => self.dialog_save(window, cx),
            "escape" => self.pending = None,
            _ => {}
        }
        // The dialog is modal: swallow everything else.
        true
    }

    pub(crate) fn render_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let after = self.pending.clone().unwrap_or(After::Close);
        let name = self.file_name();
        let (question, discard) = match after {
            After::Close => (format!("Save changes to “{name}” before closing?"), "Close Without Saving"),
            After::Quit => (format!("Save changes to “{name}” before quitting?"), "Quit Without Saving"),
            After::Open(_) => (format!("Save changes to “{name}” before opening another file?"), "Open Without Saving"),
        };
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .h(px(32.))
                .px(px(14.))
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(px(14.))
                .child(label)
        };
        div()
            .id("dialog-scrim")
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
                    .id("dialog")
                    .w(px(440.))
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
                    .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child(SharedString::from(question)))
                    .child(div().text_size(px(14.)).text_color(t.ink_muted).child("If you don’t save, your changes since the last save will be lost."))
                    .child(
                        div()
                            .mt(px(12.))
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .child(button("dialog-discard", discard).text_color(t.error).hover(|s| s.bg(t.chrome_hover)).on_click(cx.listener(
                                move |this, _, window, cx| {
                                    let after = this.pending.clone().unwrap_or(After::Close);
                                    this.dirty = false;
                                    this.perform(after, window, cx);
                                    cx.notify();
                                },
                            )))
                            .child(div().flex_1())
                            .child(button("dialog-cancel", "Cancel").text_color(t.ink).border_1().border_color(t.divider).hover(|s| s.bg(t.chrome_hover)).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.pending = None;
                                    cx.notify();
                                }),
                            ))
                            .child(button("dialog-save", "Save").bg(t.accent).text_color(t.on_accent).font_weight(FontWeight::SEMIBOLD).on_click(cx.listener(
                                |this, _, window, cx| this.dialog_save(window, cx),
                            ))),
                    ),
            )
    }

    /// Overwrite the opened file anyway, dropping what xl can't keep, then
    /// carry on with a pending close/quit/open.
    pub(crate) fn lossy_save_anyway(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.lossy_prompt = false;
        let Some(p) = self.path.clone() else { return };
        self.save_to(p);
        if !self.dirty {
            self.unsupported.clear();
            if let Some(after) = self.pending.clone() {
                self.perform(after, window, cx);
            }
        }
    }

    pub(crate) fn lossy_save_copy(&mut self, cx: &mut Context<Self>) {
        self.lossy_prompt = false;
        self.pending = None;
        self.save(true, cx);
    }

    /// The dialog is modal: Enter saves a copy, Escape cancels, nothing else gets through.
    pub(crate) fn key_lossy(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        match key {
            "enter" => self.lossy_save_copy(cx),
            "escape" => self.lossy_prompt = false,
            _ => {}
        }
        true
    }

    pub(crate) fn render_lossy_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let parts = match self.unsupported.as_slice() {
            [] => String::new(),
            [one] => one.to_string(),
            [init @ .., last] => format!("{} and {last}", init.join(", ")),
        };
        let button = |id: &'static str, label: &'static str| {
            div().id(id).flex().items_center().justify_center().h(px(32.)).px(px(14.)).rounded(px(4.)).cursor_pointer().text_size(px(14.)).child(label)
        };
        div()
            .id("lossy-scrim")
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
                    .id("lossy-dialog")
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
                    .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child(SharedString::from(format!("“{}” has {parts} that xl can’t keep", self.file_name()))))
                    .child(div().text_size(px(14.)).text_color(t.ink_muted).child("Saving over it would remove them. Save a copy to keep the original file intact."))
                    .child(
                        div()
                            .mt(px(12.))
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .child(button("lossy-anyway", "Save Anyway").text_color(t.error).hover(|s| s.bg(t.chrome_hover)).on_click(cx.listener(|this, _, window, cx| {
                                this.lossy_save_anyway(window, cx);
                                cx.notify();
                            })))
                            .child(div().flex_1())
                            .child(button("lossy-cancel", "Cancel").text_color(t.ink).border_1().border_color(t.divider).hover(|s| s.bg(t.chrome_hover)).on_click(cx.listener(|this, _, _, cx| {
                                this.lossy_prompt = false;
                                cx.notify();
                            })))
                            .child(button("lossy-copy", "Save a Copy…").bg(t.accent).text_color(t.on_accent).font_weight(FontWeight::SEMIBOLD).on_click(cx.listener(|this, _, _, cx| {
                                this.lossy_save_copy(cx);
                                cx.notify();
                            }))),
                    ),
            )
    }

    /// Offered after a crash: restore the unsaved copy or discard it.
    pub(crate) fn render_recovery_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let t = self.theme;
        let r = self.recovery_offer.as_ref()?;
        let name = r.original.as_ref().and_then(|p| p.file_name()).map_or("an unsaved workbook".to_string(), |n| format!("“{}”", n.to_string_lossy()));
        let text = format!("xl didn’t close properly last time. Unsaved changes to {name} from {} can be restored.", ago(r.modified));
        let action = |id: &'static str, label: &'static str| {
            div().id(id).px(px(10.)).h(px(24.)).flex().items_center().rounded(px(4.)).cursor_pointer().text_size(px(13.5)).hover(|s| s.bg(t.chrome_hover)).child(label)
        };
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .flex_none()
                .h(px(36.))
                .px(px(12.))
                .bg(t.header_selected)
                .border_b_1()
                .border_color(t.divider)
                .text_size(px(14.))
                .text_color(t.ink)
                .child(svg().path("icons/sheet.svg").size(px(16.)).text_color(t.accent))
                .child(div().flex_1().overflow_hidden().whitespace_nowrap().child(SharedString::from(text)))
                .child(action("recovery-discard", "Discard").text_color(t.ink_muted).on_click(cx.listener(|this, _, _, cx| {
                    this.discard_recovery_offer();
                    cx.notify();
                })))
                .child(action("recovery-restore", "Restore").text_color(t.accent).font_weight(FontWeight::SEMIBOLD).on_click(cx.listener(|this, _, _, cx| {
                    this.restore_recovery(cx);
                    cx.notify();
                }))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_only_dead_instances_recovery() {
        let dir = std::env::temp_dir().join(format!("xl-recovery-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Our own pid: a live instance, ignored.
        std::fs::write(dir.join(format!("{}.xlsx", std::process::id())), b"x").unwrap();
        assert!(find_recovery(&dir).is_none());
        // A pid that can't be running xl.
        std::fs::write(dir.join("999999999.xlsx"), b"x").unwrap();
        std::fs::write(dir.join("999999999.path"), "/home/me/Budget.xlsx").unwrap();
        let r = find_recovery(&dir).unwrap();
        assert_eq!(r.original, Some(PathBuf::from("/home/me/Budget.xlsx")));
        remove_recovery(&r.file);
        assert!(find_recovery(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn relative_times() {
        assert_eq!(ago(SystemTime::now()), "less than a minute ago");
        assert_eq!(ago(SystemTime::now() - Duration::from_secs(600)), "10 minutes ago");
        assert_eq!(ago(SystemTime::now() - Duration::from_secs(3 * 86_400)), "3 days ago");
    }
}
