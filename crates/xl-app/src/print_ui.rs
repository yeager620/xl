//! Printing in the app: Page Setup dialog, print preview, Export as PDF,
//! the Print dialog (portal or CUPS), print area and page-break commands,
//! and the page-break overlay on the grid.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, FontStyle, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement, PathBuilder, Pixels, ScrollEvent, SharedString,
    StatefulInteractiveElement, Styled, Window, canvas, div, fill, point, prelude::FluentBuilder, px,
};
use xl_engine::page::{Orientation, PAPERS, PageSetup, Pagination};
use xl_engine::SheetId;

use crate::app::{Field, Spreadsheet};
use crate::paint::{Op, cell_font, paint_ops, rect, shape};
use crate::print_layout::{DocInfo, Item, PageDraw, PrintDoc};
use crate::printing::Printers;

/// Printing state on the window.
#[derive(Default)]
pub(crate) struct PrintState {
    pub setup: Option<SetupDialog>,
    pub preview: Option<Preview>,
    pub dialog: Option<PrintDialog>,
    /// Dashed page-break lines on the grid.
    pub break_preview: bool,
    /// Pagination cached for the overlay: (edit generation, sheet, result).
    cache: RefCell<Option<(u64, SheetId, Option<Pagination>)>>,
}

/// Text fields of the Page Setup dialog, in order: (label, hint).
const SETUP_FIELDS: [(&str, &str); 15] = [
    ("Scale %", "10–400"),
    ("Pages wide", "blank = automatic"),
    ("Pages tall", "blank = automatic"),
    ("Top", "inches"),
    ("Bottom", "inches"),
    ("Left", "inches"),
    ("Right", "inches"),
    ("Header", "inches from the top"),
    ("Footer", "inches from the bottom"),
    ("Print area", "e.g. A1:F40 — blank prints the used range"),
    ("Rows to repeat at top", "e.g. 1:2"),
    ("Columns to repeat at left", "e.g. A:B"),
    ("Header", "&L left  &C center  &R right · &P page  &N pages  &D date  &T time  &A sheet  &F file"),
    ("Footer", "e.g. &CPage &P of &N"),
    ("First page number", "blank = 1"),
];

/// Print dialog text fields (indexes in `Field::Page`).
const COPIES_FIELD: u8 = 100;
const PAGES_FIELD: u8 = 101;

pub(crate) struct SetupDialog {
    pub sheet: SheetId,
    pub draft: PageSetup,
    pub texts: Vec<String>,
    pub fit_mode: bool,
}

pub(crate) struct Preview {
    pub doc: Arc<PrintDoc>,
    pub page: usize,
    /// 1.0 fits the whole page in the view.
    pub zoom: f32,
    pub whole_workbook: bool,
    /// Pan offset when zoomed in (pixels).
    pub pan: (f32, f32),
    images: Rc<RefCell<HashMap<usize, Arc<gpui::Image>>>>,
}

pub(crate) struct PrintDialog {
    /// None while still finding out.
    pub printers: Option<Printers>,
    /// The CUPS destination to use (None: the default).
    pub printer: Option<String>,
    pub copies: String,
    pub pages: String,
    pub whole_workbook: bool,
    pub busy: bool,
    pub status: Option<(String, bool)>,
}

fn num_text(v: f64) -> String {
    xl_engine::precise((v * 1000.0).round() / 1000.0)
}

/// "1:2", "$1:$2" or "3" as rows (0-based, first..=last).
fn parse_rows(s: &str) -> Option<(u32, u32)> {
    let s = s.replace('$', "");
    let s = s.rsplit('!').next().unwrap_or("").trim().to_string();
    let (a, b) = s.split_once(':').unwrap_or((&s, &s));
    let (a, b) = (a.trim().parse::<u32>().ok()?, b.trim().parse::<u32>().ok()?);
    (a >= 1 && b >= 1).then(|| (a.min(b) - 1, a.max(b) - 1))
}

/// "A:B", "$A:$B" or "C" as columns.
fn parse_cols(s: &str) -> Option<(u32, u32)> {
    let s = s.replace('$', "");
    let s = s.rsplit('!').next().unwrap_or("").trim().to_string();
    let (a, b) = s.split_once(':').unwrap_or((&s, &s));
    let (a, b) = (xl_engine::parse_col(a.trim())?, xl_engine::parse_col(b.trim())?);
    Some((a.min(b), a.max(b)))
}

fn rows_text(r: Option<(u32, u32)>) -> String {
    r.map_or(String::new(), |(a, b)| format!("{}:{}", a + 1, b + 1))
}

fn cols_text(c: Option<(u32, u32)>) -> String {
    c.map_or(String::new(), |(a, b)| format!("{}:{}", xl_engine::col_name(a), xl_engine::col_name(b)))
}

/// Local date and time for headers (`date` knows the time zone).
fn now_strings() -> (String, String) {
    let run = |fmt: &str| std::process::Command::new("date").arg(fmt).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    match (run("+%x"), run("+%X")) {
        (Some(d), Some(t)) => (d, t),
        _ => {
            let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
            let serial = secs / 86_400.0 + 25_569.0;
            let f = |code: &str| xl_engine::NumberFormat::parse(code).format(&xl_engine::Value::Number(serial)).text;
            (f("yyyy-mm-dd"), f("hh:mm"))
        }
    }
}

impl Spreadsheet {
    // ------------------------------------------------------------ commands

    fn doc_info(&self) -> DocInfo {
        let (date, time) = now_strings();
        let path = self.path.as_ref().and_then(|p| p.parent()).map(|p| p.display().to_string()).unwrap_or_default();
        DocInfo { file: self.file_name(), path, date, time }
    }

    /// Lay out the current sheet (or every sheet) for printing.
    pub(crate) fn print_doc(&self, whole_workbook: bool) -> PrintDoc {
        let sheets: Vec<SheetId> = if whole_workbook { self.wb.sheet_ids().to_vec() } else { vec![self.cur] };
        crate::print_layout::layout(&self.wb, &sheets, &self.doc_info())
    }

    pub(crate) fn open_page_setup(&mut self) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let p = self.wb.layout(self.cur).page.clone();
        let m = p.margins;
        let (fw, fh) = p.fit.unwrap_or((1, 0));
        let blank0 = |v: u16| if v == 0 { String::new() } else { v.to_string() };
        let texts = vec![
            p.scale.to_string(),
            blank0(fw),
            blank0(fh),
            num_text(m.top),
            num_text(m.bottom),
            num_text(m.left),
            num_text(m.right),
            num_text(m.header),
            num_text(m.footer),
            p.print_area.map_or(String::new(), |r| r.to_string()),
            rows_text(p.title_rows),
            cols_text(p.title_cols),
            p.header.clone(),
            p.footer.clone(),
            p.first_page.map_or(String::new(), |n| n.to_string()),
        ];
        self.print.setup = Some(SetupDialog { sheet: self.cur, fit_mode: p.fit.is_some(), draft: p, texts });
        self.field = Some(Field::Page(9));
    }

    /// The dialog's settings, checked.
    fn setup_from_dialog(&self, d: &SetupDialog) -> Result<PageSetup, String> {
        let mut p = d.draft.clone();
        let t = |i: usize| d.texts[i].trim().to_string();
        let int = |i: usize, lo: u32, hi: u32, what: &str| -> Result<Option<u32>, String> {
            let s = t(i);
            if s.is_empty() {
                return Ok(None);
            }
            match s.trim_end_matches('%').trim().parse::<u32>() {
                Ok(v) if (lo..=hi).contains(&v) => Ok(Some(v)),
                _ => Err(format!("{what} should be a whole number from {lo} to {hi}")),
            }
        };
        if d.fit_mode {
            let w = int(1, 0, 999, "Pages wide")?.unwrap_or(0) as u16;
            let h = int(2, 0, 999, "Pages tall")?.unwrap_or(0) as u16;
            p.fit = Some((w, h));
        } else {
            p.fit = None;
            p.scale = int(0, 10, 400, "Scale")?.unwrap_or(100) as u16;
        }
        let margin = |i: usize, what: &str| -> Result<f64, String> {
            match t(i).parse::<f64>() {
                Ok(v) if (0.0..=10.0).contains(&v) => Ok(v),
                _ => Err(format!("The {what} margin should be a number of inches (0–10)")),
            }
        };
        p.margins.top = margin(3, "top")?;
        p.margins.bottom = margin(4, "bottom")?;
        p.margins.left = margin(5, "left")?;
        p.margins.right = margin(6, "right")?;
        p.margins.header = margin(7, "header")?;
        p.margins.footer = margin(8, "footer")?;
        let (_, _, w, h) = p.printable();
        let (pw, ph) = p.page_size();
        if w <= 36.0 && pw - (p.margins.left + p.margins.right) * 72.0 < 36.0 || h <= 36.0 && ph - (p.margins.top + p.margins.bottom) * 72.0 < 36.0 {
            return Err("The margins leave no room on the page".into());
        }
        p.print_area = match t(9) {
            s if s.is_empty() => None,
            s => match self.wb.resolve_reference(&s, d.sheet) {
                Some((sheet, r)) if sheet == d.sheet => Some(r),
                Some(_) => return Err("The print area must be on this sheet".into()),
                None => return Err(format!("{s} isn't a range like A1:F40")),
            },
        };
        p.title_rows = match t(10) {
            s if s.is_empty() => None,
            s => Some(parse_rows(&s).ok_or_else(|| format!("{s} isn't a row range like 1:2"))?),
        };
        p.title_cols = match t(11) {
            s if s.is_empty() => None,
            s => Some(parse_cols(&s).ok_or_else(|| format!("{s} isn't a column range like A:B"))?),
        };
        p.header = d.texts[12].clone();
        p.footer = d.texts[13].clone();
        if p.header.chars().count() > 255 || p.footer.chars().count() > 255 {
            return Err("Headers and footers can be at most 255 characters".into());
        }
        p.first_page = int(14, 0, 32767, "First page number")?;
        Ok(p)
    }

    /// OK in the Page Setup dialog: apply as one undoable step.
    pub(crate) fn apply_page_setup(&mut self) {
        let Some(d) = self.print.setup.as_ref() else { return };
        match self.setup_from_dialog(d) {
            Ok(p) => {
                let sheet = d.sheet;
                self.print.setup = None;
                self.field = None;
                if sheet == self.cur {
                    self.change_layout(None, |s| {
                        if let Some(l) = s.wb.layout_mut(sheet) {
                            l.page = p;
                        }
                    });
                }
                self.refresh_preview();
            }
            Err(e) => self.message = Some((e, true)),
        }
    }

    pub(crate) fn set_print_area(&mut self) {
        let sel = self.selection();
        let sheet = self.cur;
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(sheet) {
                l.page.print_area = Some(sel);
            }
        });
        self.message = Some((format!("Print area set to {sel}"), false));
    }

    pub(crate) fn clear_print_area(&mut self) {
        let sheet = self.cur;
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(sheet) {
                l.page.print_area = None;
            }
        });
        self.message = Some(("Print area cleared — the used range prints".into(), false));
    }

    /// Excel's Insert Page Break: above the active row and left of the
    /// active column (only one of them in row 1 or column A).
    pub(crate) fn insert_page_break(&mut self) {
        let c = self.active;
        if c.row == 0 && c.col == 0 {
            self.message = Some(("Select a cell below or right of A1 to put a page break above or left of it".into(), true));
            return;
        }
        let sheet = self.cur;
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(sheet) {
                if c.row > 0 {
                    l.page.row_breaks.insert(c.row);
                }
                if c.col > 0 {
                    l.page.col_breaks.insert(c.col);
                }
            }
        });
        self.print.break_preview = true;
    }

    pub(crate) fn remove_page_break(&mut self) {
        let c = self.active;
        let sheet = self.cur;
        let had = {
            let p = &self.wb.layout(sheet).page;
            p.row_breaks.contains(&c.row) || p.col_breaks.contains(&c.col)
        };
        if !had {
            self.message = Some(("There's no manual page break above or left of this cell".into(), true));
            return;
        }
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(sheet) {
                l.page.row_breaks.remove(&c.row);
                l.page.col_breaks.remove(&c.col);
            }
        });
    }

    pub(crate) fn reset_page_breaks(&mut self) {
        let sheet = self.cur;
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(sheet) {
                l.page.row_breaks.clear();
                l.page.col_breaks.clear();
            }
        });
        self.message = Some(("Removed the manual page breaks".into(), false));
    }

    pub(crate) fn toggle_break_preview(&mut self) {
        self.print.break_preview = !self.print.break_preview;
    }

    pub(crate) fn open_preview(&mut self) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let doc = self.print_doc(false);
        if doc.pages.is_empty() {
            self.message = Some(("Nothing to print on this sheet".into(), true));
            return;
        }
        self.print.preview = Some(Preview { doc: Arc::new(doc), page: 0, zoom: 1.0, whole_workbook: false, pan: (0.0, 0.0), images: Default::default() });
    }

    /// Lay the preview out again (after settings change).
    pub(crate) fn refresh_preview(&mut self) {
        let Some(whole) = self.print.preview.as_ref().map(|p| p.whole_workbook) else { return };
        let doc = self.print_doc(whole);
        if let Some(p) = self.print.preview.as_mut() {
            p.page = p.page.min(doc.pages.len().saturating_sub(1));
            p.doc = Arc::new(doc);
        }
    }

    pub(crate) fn export_pdf(&mut self, cx: &mut Context<Self>) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let whole = self.print.preview.as_ref().is_some_and(|p| p.whole_workbook);
        let doc = self.print_doc(whole);
        if doc.pages.is_empty() {
            self.message = Some(("Nothing to print on this sheet".into(), true));
            return;
        }
        let dir = self.path.as_ref().and_then(|p| p.parent().map(|p| p.to_path_buf())).or_else(|| std::env::var_os("HOME").map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("."));
        let stem = self.path.as_ref().and_then(|p| p.file_stem()).map_or("Untitled".to_string(), |s| s.to_string_lossy().into_owned());
        let title = stem.clone();
        let rx = cx.prompt_for_new_path(&dir, Some(&format!("{stem}.pdf")));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(mut path))) = rx.await else { return };
            if path.extension().is_none_or(|e| !e.eq_ignore_ascii_case("pdf")) {
                path.set_extension("pdf");
            }
            let pages = doc.pages.len();
            let truncated = doc.truncated;
            let target = path.clone();
            let result = cx.background_executor().spawn(async move { crate::pdf::render(&doc, &title).and_then(|bytes| std::fs::write(&target, bytes).map_err(|e| e.to_string())) }).await;
            this.update(cx, |this, cx| {
                this.message = Some(match result {
                    Ok(()) => (format!("Exported {pages} page{} to {}{}", if pages == 1 { "" } else { "s" }, path.display(), if truncated { " (cut short: too many pages)" } else { "" }), false),
                    Err(e) => (format!("Export failed: {e}"), true),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Ctrl+P: the Print dialog, finding out how printing can be done.
    pub(crate) fn open_print(&mut self, cx: &mut Context<Self>) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let whole = self.print.preview.as_ref().is_some_and(|p| p.whole_workbook);
        self.print.dialog = Some(PrintDialog { printers: None, printer: None, copies: "1".into(), pages: String::new(), whole_workbook: whole, busy: false, status: None });
        self.field = Some(Field::Page(COPIES_FIELD));
        let cups = cx.background_executor().spawn(async { crate::printing::cups_printers() });
        // (Tests don't talk to the session bus: its thread would wake the
        // deterministic test executor.)
        #[cfg(not(test))]
        let portal = cx.background_executor().spawn(crate::printing::portal_available());
        #[cfg(test)]
        let portal = cx.background_executor().spawn(async { false });
        cx.spawn(async move |this, cx| {
            let cups = cups.await;
            // Show CUPS printers right away; the portal check can take a moment.
            let update = |this: &mut Spreadsheet, portal: Option<bool>| {
                if let Some(d) = this.print.dialog.as_mut() {
                    let mut p = d.printers.clone().unwrap_or_default();
                    if let Some((list, default)) = &cups {
                        p.lp = true;
                        p.list = list.clone();
                        p.default = default.clone();
                    }
                    if let Some(on) = portal {
                        p.portal = on;
                    }
                    if d.printer.is_none() {
                        d.printer = p.default.clone().or_else(|| p.list.first().cloned());
                    }
                    d.printers = Some(p);
                }
            };
            this.update(cx, |this, cx| {
                update(this, None);
                cx.notify();
            })
            .ok();
            let portal = portal.await;
            this.update(cx, |this, cx| {
                update(this, Some(portal));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Print: write the PDF to a temporary file and hand it to the portal
    /// (system dialog) or `lp`.
    pub(crate) fn run_print(&mut self, via_portal: bool, cx: &mut Context<Self>) {
        let Some(d) = self.print.dialog.as_ref() else { return };
        if d.busy {
            return;
        }
        let Some(printers) = d.printers.clone() else { return };
        if !printers.portal && !printers.lp {
            self.message = Some((crate::printing::NO_PRINTING.into(), true));
            return;
        }
        let copies = match d.copies.trim().parse::<u32>() {
            Ok(n) if (1..=999).contains(&n) => n,
            _ => {
                self.message = Some(("Copies should be a number from 1 to 999".into(), true));
                return;
            }
        };
        let doc = self.print_doc(d.whole_workbook);
        if doc.pages.is_empty() {
            self.message = Some(("Nothing to print".into(), true));
            return;
        }
        if let Err(e) = crate::printing::valid_pages(&d.pages, doc.pages.len()) {
            self.message = Some((e, true));
            return;
        }
        let (pages, printer) = (d.pages.clone(), d.printer.clone());
        let title = self.file_name();
        let tmp = std::env::temp_dir().join(format!("xl-print-{}-{}.pdf", std::process::id(), self.edit_generation));
        if let Some(d) = self.print.dialog.as_mut() {
            d.busy = true;
            d.status = Some(("Preparing pages…".into(), false));
        }
        let use_portal = via_portal && printers.portal;
        let job = cx.background_executor().spawn({
            let tmp = tmp.clone();
            let title = title.clone();
            async move {
                let bytes = crate::pdf::render(&doc, &title)?;
                std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
                if use_portal {
                    crate::printing::print_with_portal(&tmp, &title, copies).await.map(|done| if done { Some("Sent to the printer".to_string()) } else { None })
                } else {
                    crate::printing::print_with_lp(&tmp, printer.as_deref(), copies, &pages, &title).map(|m| Some(if m.is_empty() { "Sent to the printer".to_string() } else { format!("Sent to the printer ({m})") }))
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            // lp copies the file into the spool; the portal has read it by now.
            std::fs::remove_file(&tmp).ok();
            this.update(cx, |this, cx| {
                match result {
                    Ok(Some(msg)) => {
                        this.print.dialog = None;
                        if matches!(this.field, Some(Field::Page(_))) {
                            this.field = None;
                        }
                        this.message = Some((msg, false));
                    }
                    Ok(None) => {
                        if let Some(d) = this.print.dialog.as_mut() {
                            d.busy = false;
                            d.status = Some(("Printing was cancelled".into(), false));
                        }
                    }
                    Err(e) => {
                        if let Some(d) = this.print.dialog.as_mut() {
                            d.busy = false;
                            d.status = Some((format!("Printing failed: {e}"), true));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ------------------------------------------------------------ keys and text

    /// The text of a focused print field.
    pub(crate) fn print_field_text(&mut self, i: u8) -> Option<&mut String> {
        match i {
            COPIES_FIELD => self.print.dialog.as_mut().map(|d| &mut d.copies),
            PAGES_FIELD => self.print.dialog.as_mut().map(|d| &mut d.pages),
            _ => self.print.setup.as_mut().and_then(|d| d.texts.get_mut(i as usize)),
        }
    }

    pub(crate) fn print_field_str(&self, i: u8) -> &str {
        match i {
            COPIES_FIELD => self.print.dialog.as_ref().map_or("", |d| &d.copies),
            PAGES_FIELD => self.print.dialog.as_ref().map_or("", |d| &d.pages),
            _ => self.print.setup.as_ref().and_then(|d| d.texts.get(i as usize)).map_or("", |s| s),
        }
    }

    /// Whether printing UI is up (and takes the keyboard).
    pub(crate) fn print_ui_open(&self) -> bool {
        self.print.setup.is_some() || self.print.dialog.is_some() || self.print.preview.is_some()
    }

    pub(crate) fn key_print(&mut self, key: &str, ctrl: bool, cx: &mut Context<Self>) -> bool {
        if self.print.setup.is_some() {
            let i = match self.field {
                Some(Field::Page(i)) if (i as usize) < SETUP_FIELDS.len() => i,
                _ => 9,
            };
            match key {
                "enter" => self.apply_page_setup(),
                "escape" => {
                    self.print.setup = None;
                    self.field = None;
                }
                "tab" => self.field = Some(Field::Page(((i as usize + 1) % SETUP_FIELDS.len()) as u8)),
                "backspace" => {
                    if let Some(t) = self.print_field_text(i) {
                        t.pop();
                    }
                }
                _ if key.chars().count() == 1 || key == "space" => return false,
                _ => {}
            }
            return true;
        }
        if self.print.dialog.is_some() {
            match key {
                "enter" => self.run_print(false, cx),
                "escape" => {
                    self.print.dialog = None;
                    self.field = None;
                }
                "tab" => self.field = Some(Field::Page(if self.field == Some(Field::Page(COPIES_FIELD)) { PAGES_FIELD } else { COPIES_FIELD })),
                "backspace" => {
                    if let Some(Field::Page(i)) = self.field
                        && let Some(t) = self.print_field_text(i)
                    {
                        t.pop();
                    }
                }
                _ if key.chars().count() == 1 || key == "space" => return false,
                _ => {}
            }
            return true;
        }
        let Some(p) = self.print.preview.as_mut() else { return false };
        let n = p.doc.pages.len();
        match key {
            "escape" => self.print.preview = None,
            "right" | "pagedown" | "down" => p.page = (p.page + 1).min(n.saturating_sub(1)),
            "left" | "pageup" | "up" => p.page = p.page.saturating_sub(1),
            "home" => p.page = 0,
            "end" => p.page = n.saturating_sub(1),
            "+" | "=" => p.zoom = (p.zoom * 1.25).min(6.0),
            "-" => {
                p.zoom = (p.zoom / 1.25).max(0.5);
                if p.zoom <= 1.0 {
                    p.pan = (0.0, 0.0);
                }
            }
            "p" if ctrl => self.open_print(cx),
            _ => {}
        }
        true
    }

    // ------------------------------------------------------------ page-break overlay

    /// The pagination of the current sheet, cached per edit.
    fn current_pagination(&self) -> Option<Pagination> {
        let mut cache = self.print.cache.borrow_mut();
        if let Some((generation, sheet, p)) = cache.as_ref()
            && *generation == self.edit_generation
            && *sheet == self.cur
        {
            return p.clone();
        }
        let p = crate::print_layout::paginate_sheet(&self.wb, self.cur);
        *cache = Some((self.edit_generation, self.cur, p.clone()));
        p
    }

    /// Dashed lines where pages break and an outline around what prints.
    pub(crate) fn page_break_ops(&self, gx: f64, gy: f64, clip: Bounds<Pixels>) -> Vec<Op> {
        let mut ops = Vec::new();
        let Some(p) = self.current_pagination() else { return ops };
        let color: gpui::Hsla = gpui::rgb(0x2F6FDE).into();
        let (cx0, cy0) = (f64::from(clip.origin.x), f64::from(clip.origin.y));
        let (cx1, cy1) = (cx0 + f64::from(clip.size.width), cy0 + f64::from(clip.size.height));
        let a = p.area;
        let left = (gx + self.grid_x(a.start.col)).max(cx0 - 2.0);
        let right = (gx + self.grid_x(a.end.col) + self.col_w(a.end.col)).min(cx1 + 2.0);
        let top = (gy + self.grid_y(a.start.row)).max(cy0 - 2.0);
        let bottom = (gy + self.grid_y(a.end.row) + self.row_h(a.end.row)).min(cy1 + 2.0);
        if right <= left || bottom <= top {
            return ops;
        }
        let mut dashed = |horizontal: bool, at: f64, from: f64, to: f64| {
            let mut s = from;
            while s < to {
                let len = (to - s).min(6.0);
                let r = if horizontal { rect(s, at - 1.0, len, 2.0) } else { rect(at - 1.0, s, 2.0, len) };
                ops.push(Op::Quad(fill(r, color), clip));
                s += 10.0;
            }
        };
        for r in p.row_starts() {
            let y = gy + self.grid_y(r);
            if y > cy0 && y < cy1 {
                dashed(true, y, left, right);
            }
        }
        for c in p.col_starts() {
            let x = gx + self.grid_x(c);
            if x > cx0 && x < cx1 {
                dashed(false, x, top, bottom);
            }
        }
        // The print range's outline.
        let solid = |r: Bounds<Pixels>| Op::Quad(fill(r, color), clip);
        let (x0, x1) = (gx + self.grid_x(a.start.col), gx + self.grid_x(a.end.col) + self.col_w(a.end.col));
        let (y0, y1) = (gy + self.grid_y(a.start.row), gy + self.grid_y(a.end.row) + self.row_h(a.end.row));
        if y0 >= cy0 - 2.0 && y0 <= cy1 {
            ops.push(solid(rect(left, y0 - 1.0, right - left, 2.0)));
        }
        if y1 >= cy0 && y1 <= cy1 + 2.0 {
            ops.push(solid(rect(left, y1 - 1.0, right - left, 2.0)));
        }
        if x0 >= cx0 - 2.0 && x0 <= cx1 {
            ops.push(solid(rect(x0 - 1.0, top, 2.0, bottom - top)));
        }
        if x1 >= cx0 && x1 <= cx1 + 2.0 {
            ops.push(solid(rect(x1 - 1.0, top, 2.0, bottom - top)));
        }
        ops
    }

    // ------------------------------------------------------------ rendering

    pub(crate) fn render_print_ui(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut root = div().absolute().top_0().left_0().size_full();
        if self.print.preview.is_some() {
            root = root.child(self.render_preview(cx));
        }
        if self.print.setup.is_some() {
            root = root.child(self.render_setup(cx));
        }
        if self.print.dialog.is_some() {
            root = root.child(self.render_print_dialog(cx));
        }
        root.into_any_element()
    }

    fn scrim(&self, id: &'static str) -> gpui::Stateful<gpui::Div> {
        let t = self.theme;
        div().id(id).absolute().top_0().left_0().size_full().flex().items_center().justify_center().bg(gpui::hsla(0., 0., 0., if t.dark { 0.45 } else { 0.25 })).occlude()
    }

    fn chip(&self, id: impl Into<gpui::ElementId>, label: impl Into<SharedString>, on: bool) -> gpui::Stateful<gpui::Div> {
        let t = self.theme;
        div()
            .id(id)
            .px(px(10.))
            .h(px(26.))
            .flex()
            .items_center()
            .rounded(px(4.))
            .cursor_pointer()
            .text_size(px(13.))
            .border_1()
            .border_color(if on { t.accent } else { t.divider })
            .when(on, |d| d.bg(t.header_selected).text_color(t.accent))
            .child(label.into())
    }

    fn check(&self, id: &'static str, label: &'static str, on: bool) -> gpui::Stateful<gpui::Div> {
        let t = self.theme;
        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .cursor_pointer()
            .text_size(px(13.5))
            .child(
                div()
                    .size(px(15.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.))
                    .border_1()
                    .border_color(if on { t.accent } else { t.ink_faint })
                    .when(on, |d| d.bg(t.accent).child(gpui::svg().path("icons/check.svg").size(px(12.)).text_color(t.on_accent))),
            )
            .child(label)
    }

    fn text_field(&self, i: u8, label: &'static str, hint: &'static str, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme;
        let text = self.print_field_str(i).to_string();
        let focused = self.field == Some(Field::Page(i));
        div().flex().flex_col().gap(px(3.)).min_w(px(0.)).child(div().text_size(px(12.5)).text_color(t.ink_muted).child(label)).child(
            div()
                .id(("page-field", i as usize))
                .h(px(28.))
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
                .text_size(px(13.5))
                .when(text.is_empty() && !focused, |d| d.text_color(t.ink_faint).child(hint))
                .when(!text.is_empty(), |d| d.text_color(t.ink).child(SharedString::from(text)))
                .when(focused, |d| d.child(div().w(px(1.5)).h(px(15.)).bg(t.accent)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        this.focus.focus(window, cx);
                        this.field = Some(Field::Page(i));
                        cx.stop_propagation();
                        cx.notify();
                    }),
                ),
        )
    }

    fn button(&self, id: &'static str, label: &'static str, primary: bool) -> gpui::Stateful<gpui::Div> {
        let t = self.theme;
        let b = div().id(id).flex().items_center().justify_center().h(px(32.)).px(px(14.)).rounded(px(4.)).cursor_pointer().text_size(px(14.)).child(label);
        if primary { b.bg(t.accent).text_color(t.on_accent).font_weight(FontWeight::SEMIBOLD) } else { b.text_color(t.ink).border_1().border_color(t.divider).hover(|s| s.bg(t.chrome_hover)) }
    }

    fn section(&self, title: &'static str) -> gpui::Div {
        let t = self.theme;
        div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink_muted).child(title)
    }

    fn render_setup(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let Some(d) = &self.print.setup else { return div().into_any_element() };
        let p = &d.draft;
        let edit = |f: fn(&mut SetupDialog)| move |this: &mut Spreadsheet, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Spreadsheet>| {
            if let Some(d) = this.print.setup.as_mut() {
                f(d);
            }
            cx.notify();
        };
        let mut papers = div().flex().flex_row().flex_wrap().gap(px(6.));
        for &(code, name, _, _) in PAPERS.iter().take(6) {
            papers = papers.child(self.chip(("paper", code as usize), name, p.paper == code).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(d) = this.print.setup.as_mut() {
                    d.draft.paper = code;
                }
                cx.notify();
            })));
        }
        if !PAPERS.iter().take(6).any(|x| x.0 == p.paper) {
            papers = papers.child(self.chip("paper-other", p.paper_name(), true));
        }
        let row = || div().flex().flex_row().gap(px(10.));
        let col = |el: gpui::Div| el.flex_1();
        let body = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(self.section("PAGE"))
            .child(
                row()
                    .items_center()
                    .child(self.chip("orient-p", "Portrait", p.orientation == Orientation::Portrait).on_click(cx.listener(edit(|d| d.draft.orientation = Orientation::Portrait))))
                    .child(self.chip("orient-l", "Landscape", p.orientation == Orientation::Landscape).on_click(cx.listener(edit(|d| d.draft.orientation = Orientation::Landscape)))),
            )
            .child(papers)
            .child(self.section("SCALING"))
            .child(
                row()
                    .items_end()
                    .child(self.chip("scale-pct", "Adjust to", !d.fit_mode).on_click(cx.listener(edit(|d| d.fit_mode = false))))
                    .child(col(self.text_field(0, SETUP_FIELDS[0].0, SETUP_FIELDS[0].1, cx)))
                    .child(self.chip("scale-fit", "Fit to", d.fit_mode).on_click(cx.listener(edit(|d| d.fit_mode = true))))
                    .child(col(self.text_field(1, SETUP_FIELDS[1].0, SETUP_FIELDS[1].1, cx)))
                    .child(col(self.text_field(2, SETUP_FIELDS[2].0, SETUP_FIELDS[2].1, cx))),
            )
            .child(self.section("MARGINS (INCHES)"))
            .child(row().children((3..9u8).map(|i| col(self.text_field(i, SETUP_FIELDS[i as usize].0, SETUP_FIELDS[i as usize].1, cx)))))
            .child(self.section("SHEET"))
            .child(self.text_field(9, SETUP_FIELDS[9].0, SETUP_FIELDS[9].1, cx))
            .child(row().child(col(self.text_field(10, SETUP_FIELDS[10].0, SETUP_FIELDS[10].1, cx))).child(col(self.text_field(11, SETUP_FIELDS[11].0, SETUP_FIELDS[11].1, cx))))
            .child(
                row()
                    .flex_wrap()
                    .gap(px(16.))
                    .child(self.check("pg-grid", "Gridlines", p.gridlines).on_click(cx.listener(edit(|d| d.draft.gridlines = !d.draft.gridlines))))
                    .child(self.check("pg-head", "Row and column headings", p.headings).on_click(cx.listener(edit(|d| d.draft.headings = !d.draft.headings))))
                    .child(self.check("pg-ch", "Center horizontally", p.center_h).on_click(cx.listener(edit(|d| d.draft.center_h = !d.draft.center_h))))
                    .child(self.check("pg-cv", "Center vertically", p.center_v).on_click(cx.listener(edit(|d| d.draft.center_v = !d.draft.center_v))))
                    .child(self.check("pg-order", "Over, then down", p.over_then_down).on_click(cx.listener(edit(|d| d.draft.over_then_down = !d.draft.over_then_down))))
                    .child(self.check("pg-bw", "Black and white", p.black_and_white).on_click(cx.listener(edit(|d| d.draft.black_and_white = !d.draft.black_and_white)))),
            )
            .child(self.section("HEADER AND FOOTER"))
            .child(self.text_field(12, SETUP_FIELDS[12].0, SETUP_FIELDS[12].1, cx))
            .child(row().child(col(self.text_field(13, SETUP_FIELDS[13].0, SETUP_FIELDS[13].1, cx))).child(self.text_field(14, SETUP_FIELDS[14].0, SETUP_FIELDS[14].1, cx).w(px(130.))));
        self.scrim("page-setup-scrim")
            .child(
                div()
                    .id("page-setup-dialog")
                    .w(px(640.))
                    .max_h_full()
                    .overflow_y_scroll()
                    .p(px(20.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .rounded(px(8.))
                    .bg(t.popover)
                    .border_1()
                    .border_color(t.divider)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child(format!("Page setup — {}", self.wb.sheet_name(d.sheet))))
                    .child(body)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .child(div().flex_1())
                            .child(self.button("pg-cancel", "Cancel", false).on_click(cx.listener(|this, _, _, cx| {
                                this.print.setup = None;
                                this.field = None;
                                cx.notify();
                            })))
                            .when(self.print.preview.is_none(), |d| {
                                d.child(self.button("pg-preview", "Preview", false).on_click(cx.listener(|this, _, _, cx| {
                                    this.apply_page_setup();
                                    if this.print.setup.is_none() {
                                        this.open_preview();
                                    }
                                    cx.notify();
                                })))
                            })
                            .child(self.button("pg-ok", "OK", true).on_click(cx.listener(|this, _, _, cx| {
                                this.apply_page_setup();
                                cx.notify();
                            }))),
                    ),
            )
            .into_any_element()
    }

    fn render_print_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let Some(d) = &self.print.dialog else { return div().into_any_element() };
        let mut body = div().flex().flex_col().gap(px(10.));
        match &d.printers {
            None => body = body.child(div().text_size(px(13.5)).text_color(t.ink_muted).child("Looking for printers…")),
            Some(p) if !p.portal && !p.lp => body = body.child(div().text_size(px(13.5)).text_color(t.ink).child(crate::printing::NO_PRINTING)),
            Some(p) => {
                if p.lp {
                    body = body.child(self.section("PRINTER"));
                    if p.list.is_empty() {
                        body = body.child(div().text_size(px(13.)).text_color(t.ink_muted).child("CUPS has no printers set up."));
                    } else {
                        let mut list = div().flex().flex_row().flex_wrap().gap(px(6.));
                        for (i, name) in p.list.iter().enumerate() {
                            let label = if p.default.as_ref() == Some(name) { format!("{name} (default)") } else { name.clone() };
                            let n = name.clone();
                            list = list.child(self.chip(("printer", i), label, d.printer.as_ref() == Some(name)).on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(d) = this.print.dialog.as_mut() {
                                    d.printer = Some(n.clone());
                                }
                                cx.notify();
                            })));
                        }
                        body = body.child(list);
                    }
                }
                body = body
                    .child(self.section("WHAT TO PRINT"))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(6.))
                            .child(self.chip("pr-sheet", "This sheet", !d.whole_workbook).on_click(cx.listener(|this, _, _, cx| {
                                if let Some(d) = this.print.dialog.as_mut() {
                                    d.whole_workbook = false;
                                }
                                cx.notify();
                            })))
                            .child(self.chip("pr-book", "Entire workbook", d.whole_workbook).on_click(cx.listener(|this, _, _, cx| {
                                if let Some(d) = this.print.dialog.as_mut() {
                                    d.whole_workbook = true;
                                }
                                cx.notify();
                            }))),
                    )
                    .child(div().flex().flex_row().gap(px(10.)).child(self.text_field(COPIES_FIELD, "Copies", "1", cx).w(px(90.))).when(p.lp, |r| r.child(self.text_field(PAGES_FIELD, "Pages", "all, or e.g. 1-3, 5", cx).flex_1())));
            }
        }
        if let Some((msg, err)) = &d.status {
            body = body.child(div().text_size(px(13.)).text_color(if *err { t.error } else { t.ink_muted }).child(msg.clone()));
        }
        let ready = d.printers.as_ref().is_some_and(|p| p.portal || p.lp) && !d.busy;
        let lp_ready = ready && d.printers.as_ref().is_some_and(|p| p.lp && !p.list.is_empty());
        let portal_ready = ready && d.printers.as_ref().is_some_and(|p| p.portal);
        self.scrim("print-scrim")
            .child(
                div()
                    .id("print-dialog")
                    .w(px(460.))
                    .p(px(20.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .rounded(px(8.))
                    .bg(t.popover)
                    .border_1()
                    .border_color(t.divider)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child("Print"))
                    .child(body)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .child(self.button("print-pdf", "Export PDF…", false).on_click(cx.listener(|this, _, _, cx| {
                                this.print.dialog = None;
                                this.field = None;
                                this.export_pdf(cx);
                                cx.notify();
                            })))
                            .child(div().flex_1())
                            .child(self.button("print-cancel", "Cancel", false).on_click(cx.listener(|this, _, _, cx| {
                                this.print.dialog = None;
                                this.field = None;
                                cx.notify();
                            })))
                            .when(portal_ready, |r| {
                                r.child(self.button("print-system", "System dialog…", !lp_ready).on_click(cx.listener(|this, _, _, cx| {
                                    this.run_print(true, cx);
                                    cx.notify();
                                })))
                            })
                            .when(lp_ready, |r| {
                                r.child(self.button("print-go", "Print", true).on_click(cx.listener(|this, _, _, cx| {
                                    this.run_print(false, cx);
                                    cx.notify();
                                })))
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let Some(p) = &self.print.preview else { return div().into_any_element() };
        let n = p.doc.pages.len();
        let tool = |id: &'static str, label: &'static str| div().id(id).h(px(28.)).px(px(10.)).flex().items_center().rounded(px(4.)).cursor_pointer().text_size(px(13.5)).text_color(t.ink).hover(|s| s.bg(t.chrome_hover)).child(label);
        let scale_note = p.doc.scales.iter().find(|(s, _)| *s == self.cur).map(|(_, sc)| format!("{}%", (sc * 100.0).round())).unwrap_or_default();
        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .h(px(44.))
            .px(px(12.))
            .bg(t.chrome)
            .border_b_1()
            .border_color(t.divider)
            .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).mr(px(12.)).child("Print preview"))
            .child(tool("pv-prev", "‹").on_click(cx.listener(|this, _, _, cx| {
                if let Some(p) = this.print.preview.as_mut() {
                    p.page = p.page.saturating_sub(1);
                }
                cx.notify();
            })))
            .child(div().text_size(px(13.5)).text_color(t.ink).child(format!("Page {} of {n}{}", p.page + 1, if p.doc.truncated { "+" } else { "" })))
            .child(tool("pv-next", "›").on_click(cx.listener(|this, _, _, cx| {
                if let Some(p) = this.print.preview.as_mut() {
                    p.page = (p.page + 1).min(p.doc.pages.len().saturating_sub(1));
                }
                cx.notify();
            })))
            .child(div().w(px(12.)))
            .child(tool("pv-zoom-out", "−").on_click(cx.listener(|this, _, _, cx| {
                if let Some(p) = this.print.preview.as_mut() {
                    p.zoom = (p.zoom / 1.25).max(0.5);
                    if p.zoom <= 1.0 {
                        p.pan = (0.0, 0.0);
                    }
                }
                cx.notify();
            })))
            .child(div().text_size(px(13.)).text_color(t.ink_muted).child(format!("{}%", (p.zoom * 100.0).round())))
            .child(tool("pv-zoom-in", "+").on_click(cx.listener(|this, _, _, cx| {
                if let Some(p) = this.print.preview.as_mut() {
                    p.zoom = (p.zoom * 1.25).min(6.0);
                }
                cx.notify();
            })))
            .child(div().w(px(12.)))
            .child(self.chip("pv-sheet", "This sheet", !p.whole_workbook).on_click(cx.listener(|this, _, _, cx| {
                if let Some(p) = this.print.preview.as_mut() {
                    p.whole_workbook = false;
                }
                this.refresh_preview();
                cx.notify();
            })))
            .child(self.chip("pv-book", "Entire workbook", p.whole_workbook).on_click(cx.listener(|this, _, _, cx| {
                if let Some(p) = this.print.preview.as_mut() {
                    p.whole_workbook = true;
                    p.page = 0;
                }
                this.refresh_preview();
                cx.notify();
            })))
            .when(!scale_note.is_empty(), |d| d.child(div().ml(px(8.)).text_size(px(12.5)).text_color(t.ink_faint).child(format!("Scale {scale_note}"))))
            .child(div().flex_1())
            .child(tool("pv-setup", "Page setup…").on_click(cx.listener(|this, _, _, cx| {
                this.open_page_setup();
                cx.notify();
            })))
            .child(tool("pv-pdf", "Export PDF…").on_click(cx.listener(|this, _, _, cx| {
                this.export_pdf(cx);
                cx.notify();
            })))
            .child(self.button("pv-print", "Print…", true).on_click(cx.listener(|this, _, _, cx| {
                this.open_print(cx);
                cx.notify();
            })))
            .child(tool("pv-close", "Close").on_click(cx.listener(|this, _, _, cx| {
                this.print.preview = None;
                cx.notify();
            })));
        // Thumbnails.
        const THUMBS: usize = 60;
        let mut thumbs = div().id("pv-thumbs").w(px(150.)).h_full().flex_none().overflow_y_scroll().flex().flex_col().items_center().gap(px(10.)).py(px(12.)).bg(t.chrome).border_r_1().border_color(t.divider);
        for i in 0..n.min(THUMBS) {
            let page = &p.doc.pages[i];
            let k = 110.0 / page.width.max(page.height);
            let (w, h) = ((page.width * k) as f32, (page.height * k) as f32);
            let doc = p.doc.clone();
            let images = p.images.clone();
            thumbs = thumbs.child(
                div()
                    .id(("pv-thumb", i))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(3.))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(p) = this.print.preview.as_mut() {
                            p.page = i;
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w(px(w))
                            .h(px(h))
                            .border_2()
                            .border_color(if i == p.page { t.accent } else { gpui::transparent_black() })
                            .child(canvas(|_, _, _| (), move |bounds, (), window, cx| paint_page(&doc.pages[i], bounds, k as f32, &images, window, cx)).size_full()),
                    )
                    .child(div().text_size(px(11.5)).text_color(t.ink_muted).child((i + 1).to_string())),
            );
        }
        if n > THUMBS {
            thumbs = thumbs.child(div().text_size(px(11.5)).text_color(t.ink_faint).child(format!("+{} more", n - THUMBS)));
        }
        // The current page, fitted to the view and zoomed.
        let doc = p.doc.clone();
        let images = p.images.clone();
        let (index, zoom, pan) = (p.page.min(n.saturating_sub(1)), p.zoom, p.pan);
        let page_view = div()
            .id("pv-page")
            .flex_1()
            .h_full()
            .overflow_hidden()
            .bg(gpui::rgb(if t.dark { 0x2A2B2F } else { 0xD9DBDF }))
            .on_scroll(cx.listener(|this, ev: &ScrollEvent, _, cx| {
                if let Some(p) = this.print.preview.as_mut() {
                    let d = ev.delta.pixel_delta(px(20.));
                    if p.zoom > 1.0 {
                        p.pan.0 += f32::from(d.x);
                        p.pan.1 += f32::from(d.y);
                    } else if f32::from(d.y) < -0.5 {
                        p.page = (p.page + 1).min(p.doc.pages.len().saturating_sub(1));
                    } else if f32::from(d.y) > 0.5 {
                        p.page = p.page.saturating_sub(1);
                    }
                }
                cx.notify();
            }))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, cx| {
                        let Some(page) = doc.pages.get(index) else { return };
                        let (vw, vh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                        let fit = ((vw - 48.0) / page.width as f32).min((vh - 48.0) / page.height as f32).max(0.05);
                        let k = fit * zoom;
                        let (w, h) = (page.width as f32 * k, page.height as f32 * k);
                        let mut x = f32::from(bounds.origin.x) + (vw - w) / 2.0;
                        let mut y = f32::from(bounds.origin.y) + (vh - h) / 2.0;
                        // Zoomed past the view: pan, keeping the page's edges in reach.
                        if w + 48.0 > vw {
                            x = (x + pan.0).clamp(f32::from(bounds.origin.x) + vw - w - 24.0, f32::from(bounds.origin.x) + 24.0);
                        }
                        if h + 48.0 > vh {
                            y = (y + pan.1).clamp(f32::from(bounds.origin.y) + vh - h - 24.0, f32::from(bounds.origin.y) + 24.0);
                        }
                        let page_bounds = Bounds::new(point(px(x), px(y)), gpui::size(px(w), px(h)));
                        window.paint_quad(fill(Bounds::new(point(px(x + 2.0), px(y + 3.0)), gpui::size(px(w), px(h))), gpui::hsla(0., 0., 0., 0.18)));
                        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| paint_page(page, page_bounds, k, &images, window, cx));
                    },
                )
                .size_full(),
            );
        div()
            .id("print-preview")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.canvas)
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(toolbar)
            .child(div().flex_1().min_h(px(0.)).flex().flex_row().child(thumbs).child(page_view))
            .into_any_element()
    }
}

/// `xl --pdf in.xlsx out.pdf`: every sheet of a workbook to PDF.
pub(crate) fn export_headless(input: Option<&String>, output: Option<&String>) -> Result<String, String> {
    let (Some(input), Some(output)) = (input, output) else { return Err("usage: xl --pdf <workbook> <output.pdf>".into()) };
    let loaded = xl_io::open(std::path::Path::new(input))?;
    let wb = &loaded.workbook;
    let (date, time) = now_strings();
    let p = std::path::Path::new(input);
    let info = DocInfo {
        file: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        path: p.parent().map(|d| d.display().to_string()).unwrap_or_default(),
        date,
        time,
    };
    let doc = crate::print_layout::layout(wb, wb.sheet_ids(), &info);
    let title = p.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    std::fs::write(output, crate::pdf::render(&doc, &title)?).map_err(|e| e.to_string())?;
    Ok(format!("{} page{} written to {output}", doc.pages.len(), if doc.pages.len() == 1 { "" } else { "s" }))
}

/// Paint one page (white paper and its items) at `k` pixels per point.
fn paint_page(page: &PageDraw, bounds: Bounds<Pixels>, k: f32, images: &Rc<RefCell<HashMap<usize, Arc<gpui::Image>>>>, window: &mut Window, cx: &mut App) {
    let (ox, oy) = (f64::from(bounds.origin.x), f64::from(bounds.origin.y));
    let k = k as f64;
    let at = |x: f64, y: f64| (ox + x * k, oy + y * k);
    let page_clip = bounds;
    let clip_of = |c: &Option<crate::print_layout::Rect>| match c {
        Some(r) => {
            let (x, y) = at(r.x, r.y);
            rect(x, y, r.w * k, r.h * k).intersect(&page_clip)
        }
        None => page_clip,
    };
    let rgb = |c: u32| -> gpui::Hsla { gpui::rgb(c).into() };
    let mut ops = vec![Op::Quad(fill(bounds, gpui::white()), page_clip)];
    for item in &page.items {
        match item {
            Item::Rect { r, color } => {
                let (x, y) = at(r.x, r.y);
                ops.push(Op::Quad(fill(rect(x, y, r.w * k, r.h * k), rgb(*color)), page_clip));
            }
            Item::Line { a, b, width, color, .. } => {
                let w = (width * k).max(0.6);
                let (x0, y0) = at(a.0, a.1);
                let (x1, y1) = at(b.0, b.1);
                if (y0 - y1).abs() < 0.01 {
                    ops.push(Op::Quad(fill(rect(x0.min(x1), y0 - w / 2.0, (x1 - x0).abs(), w), rgb(*color)), page_clip));
                } else if (x0 - x1).abs() < 0.01 {
                    ops.push(Op::Quad(fill(rect(x0 - w / 2.0, y0.min(y1), w, (y1 - y0).abs()), rgb(*color)), page_clip));
                } else {
                    let mut pb = PathBuilder::stroke(px(w as f32));
                    pb.move_to(point(px(x0 as f32), px(y0 as f32)));
                    pb.line_to(point(px(x1 as f32), px(y1 as f32)));
                    if let Ok(path) = pb.build() {
                        ops.push(Op::Path(path, rgb(*color), page_clip));
                    }
                }
            }
            Item::Path { points, closed, fill: f, stroke, clip } => {
                let clip = clip_of(clip);
                let pts: Vec<gpui::Point<Pixels>> = points.iter().map(|(x, y)| at(*x, *y)).map(|(x, y)| point(px(x as f32), px(y as f32))).collect();
                if pts.len() < 2 {
                    continue;
                }
                if let Some(c) = f {
                    let mut pb = PathBuilder::fill();
                    pb.move_to(pts[0]);
                    for p in &pts[1..] {
                        pb.line_to(*p);
                    }
                    pb.close();
                    if let Ok(path) = pb.build() {
                        ops.push(Op::Path(path, rgb(*c), clip));
                    }
                }
                if let Some((c, w)) = stroke {
                    let mut pb = PathBuilder::stroke(px((w * k).max(0.6) as f32));
                    pb.move_to(pts[0]);
                    for p in &pts[1..] {
                        pb.line_to(*p);
                    }
                    if *closed {
                        pb.line_to(pts[0]);
                    }
                    if let Ok(path) = pb.build() {
                        ops.push(Op::Path(path, rgb(*c), clip));
                    }
                }
            }
            Item::Text { x, y, text, face, size, color, underline, strike, clip } => {
                let size_px = (size * k) as f32;
                if size_px < 1.0 {
                    continue;
                }
                let f = gpui::Font { weight: if face / 2 == 1 { FontWeight::BOLD } else { FontWeight::NORMAL }, style: if face % 2 == 1 { FontStyle::Italic } else { FontStyle::Normal }, ..cell_font() };
                let line = shape(window, text.clone(), f, rgb(*color), size_px);
                let (asc, desc) = crate::print_layout::ascent_descent(*face, *size);
                let lh = (asc + desc) * k;
                let (tx, ty) = at(*x, *y);
                let clip = clip_of(clip);
                if *underline || *strike {
                    let w = f64::from(line.width);
                    let t = (size * k / 16.0).max(0.6);
                    if *underline {
                        ops.push(Op::Quad(fill(rect(tx, ty + size * k * 0.12, w, t), rgb(*color)), clip));
                    }
                    if *strike {
                        ops.push(Op::Quad(fill(rect(tx, ty - asc * k * 0.3, w, t), rgb(*color)), clip));
                    }
                }
                ops.push(Op::Text { line: Box::new(line), origin: point(px(tx as f32), px((ty - asc * k) as f32)), clip, line_height: px(lh as f32) });
            }
            Item::Image { r, data, format, clip } => {
                let fmt = match format {
                    xl_engine::picture::PictureFormat::Png => gpui::ImageFormat::Png,
                    xl_engine::picture::PictureFormat::Jpeg => gpui::ImageFormat::Jpeg,
                    xl_engine::picture::PictureFormat::Gif => gpui::ImageFormat::Gif,
                    xl_engine::picture::PictureFormat::Bmp => gpui::ImageFormat::Bmp,
                    xl_engine::picture::PictureFormat::Tiff => gpui::ImageFormat::Tiff,
                    xl_engine::picture::PictureFormat::Webp => gpui::ImageFormat::Webp,
                    xl_engine::picture::PictureFormat::Other => continue,
                };
                let image = images.borrow_mut().entry(data.as_ptr() as usize).or_insert_with(|| Arc::new(gpui::Image::from_bytes(fmt, data.to_vec()))).clone();
                let (x, y) = at(r.x, r.y);
                ops.push(Op::Image { image, bounds: rect(x, y, r.w * k, r.h * k), clip: clip_of(clip) });
            }
        }
    }
    paint_ops(ops, window, cx);
}

#[cfg(test)]
mod tests {
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use xl_engine::{Addr, CellRef};

    use crate::app::Spreadsheet;

    fn setup(cx: &mut TestAppContext) -> (Entity<Spreadsheet>, &mut VisualTestContext) {
        let (view, cx) = cx.add_window_view(|window, cx| Spreadsheet::new(None, window, cx));
        view.update(cx, |s, cx| {
            s.recovery_offer = None;
            s.recovery_dir = std::env::temp_dir().join(format!("xl-print-test-recovery-{}", std::process::id()));
            s.recent = crate::recent::Recent::load(&std::env::temp_dir().join(format!("xl-print-test-recent-{}", std::process::id())));
            s.home = None;
            let mut wb = xl_engine::Workbook::new();
            let sh = wb.sheet_ids()[0];
            let items: Vec<(Addr, String)> = (0..150u32).flat_map(|r| [(Addr::new(sh, CellRef::new(r, 0)), format!("Row {r}")), (Addr::new(sh, CellRef::new(r, 1)), (r * 3).to_string())]).collect();
            wb.set_inputs(items);
            s.wb = wb;
            s.cur = s.wb.sheet_ids()[0];
            cx.notify();
        });
        cx.run_until_parked();
        (view, cx)
    }

    fn run_command(view: &Entity<Spreadsheet>, cx: &mut VisualTestContext, name: &str) {
        let cmd = crate::palette::commands().into_iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no command {name}"));
        cx.update(|window, cx| view.update(cx, |s, cx| (cmd.run)(s, window, cx)));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn page_setup_dialog_applies_settings_as_one_undo_step(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx);
        run_command(&view, cx, "Page setup…");
        view.read_with(cx, |s, _| assert!(s.print.setup.is_some()));
        // Print area field is focused: type a range, then switch to landscape and fit to one page wide.
        cx.simulate_input("A1:B40");
        view.update(cx, |s, _| {
            let d = s.print.setup.as_mut().unwrap();
            d.draft.orientation = xl_engine::page::Orientation::Landscape;
            d.fit_mode = true;
            d.texts[1] = "1".into();
            d.texts[2] = String::new();
        });
        cx.simulate_keystrokes("tab");
        cx.simulate_input("1:1");
        cx.simulate_keystrokes("enter");
        view.read_with(cx, |s, _| {
            assert!(s.print.setup.is_none(), "{:?}", s.message);
            let p = &s.wb.layout(s.cur).page;
            assert_eq!(p.print_area.map(|r| r.to_string()), Some("A1:B40".into()));
            assert_eq!(p.title_rows, Some((0, 0)));
            assert_eq!(p.fit, Some((1, 0)));
            assert_eq!(p.orientation, xl_engine::page::Orientation::Landscape);
        });
        cx.simulate_keystrokes("ctrl-z");
        view.read_with(cx, |s, _| assert!(s.wb.layout(s.cur).page.is_default()));
    }

    #[gpui::test]
    fn bad_values_keep_the_dialog_open(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx);
        run_command(&view, cx, "Page setup…");
        view.update(cx, |s, _| s.print.setup.as_mut().unwrap().texts[0] = "5000".into());
        cx.simulate_keystrokes("enter");
        view.read_with(cx, |s, _| {
            assert!(s.print.setup.is_some());
            assert!(s.message.as_ref().is_some_and(|m| m.1));
        });
        cx.simulate_keystrokes("escape");
        view.read_with(cx, |s, _| assert!(s.print.setup.is_none()));
    }

    #[gpui::test]
    fn preview_builds_pages_and_pages_through(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx);
        run_command(&view, cx, "Print preview");
        let n = view.read_with(cx, |s, _| s.print.preview.as_ref().map(|p| p.doc.pages.len()).unwrap_or(0));
        // 150 rows of 15pt on Letter with normal margins: 4 pages.
        assert_eq!(n, 4);
        cx.simulate_keystrokes("right right");
        view.read_with(cx, |s, _| assert_eq!(s.print.preview.as_ref().unwrap().page, 2));
        cx.simulate_keystrokes("+");
        view.read_with(cx, |s, _| assert!(s.print.preview.as_ref().unwrap().zoom > 1.0));
        // Page setup from the preview re-lays it out.
        view.update(cx, |s, _| s.open_page_setup());
        view.update(cx, |s, _| {
            let d = s.print.setup.as_mut().unwrap();
            d.fit_mode = true;
            d.texts[1] = "1".into();
            d.texts[2] = "1".into();
        });
        cx.simulate_keystrokes("enter");
        view.read_with(cx, |s, _| assert_eq!(s.print.preview.as_ref().unwrap().doc.pages.len(), 1));
        cx.simulate_keystrokes("escape");
        view.read_with(cx, |s, _| assert!(s.print.preview.is_none()));
    }

    #[gpui::test]
    fn print_area_and_break_commands(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx);
        view.update(cx, |s, _| {
            s.anchor = CellRef::parse("A1").unwrap();
            s.active = CellRef::parse("B20").unwrap();
        });
        run_command(&view, cx, "Set print area");
        view.read_with(cx, |s, _| assert_eq!(s.wb.layout(s.cur).page.print_area.map(|r| r.to_string()), Some("A1:B20".into())));
        run_command(&view, cx, "Insert page break");
        view.read_with(cx, |s, _| {
            let p = &s.wb.layout(s.cur).page;
            assert!(p.row_breaks.contains(&19) && p.col_breaks.contains(&1));
            assert!(s.print.break_preview);
            // The overlay has something to draw.
            assert!(s.current_pagination().is_some_and(|p| p.row_starts().contains(&19)));
        });
        run_command(&view, cx, "Remove page break");
        view.read_with(cx, |s, _| assert!(s.wb.layout(s.cur).page.row_breaks.is_empty()));
        run_command(&view, cx, "Clear print area");
        view.read_with(cx, |s, _| assert!(s.wb.layout(s.cur).page.print_area.is_none()));
    }

    #[gpui::test]
    fn print_dialog_opens_with_ctrl_p(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx);
        cx.simulate_keystrokes("ctrl-p");
        view.read_with(cx, |s, _| assert!(s.print.dialog.is_some()));
        cx.simulate_input("2");
        view.read_with(cx, |s, _| assert_eq!(s.print.dialog.as_ref().unwrap().copies, "12"));
        cx.simulate_keystrokes("escape");
        view.read_with(cx, |s, _| assert!(s.print.dialog.is_none()));
    }
}
