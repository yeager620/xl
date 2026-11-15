//! Version history in the app: recording a version on every save (and
//! optionally while autosaving), the "Version history" panel, diffs of the
//! workbook against a version, another file or the file on disk (changed
//! cells highlighted on the grid, old → new on hover, a clickable list of
//! changes), naming versions, and restoring one (as an undoable edit) or
//! opening it as a copy.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use gpui::{
    AnyElement, AppContext, Bounds, Context, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, PathPromptOptions, Pixels, Point, SharedString, StatefulInteractiveElement,
    Styled, div, fill, point, prelude::FluentBuilder, px,
};
use xl_diff::{CellChange, CellKind, LineAxis, SheetStatus, WorkbookDiff};
use xl_engine::{Addr, CellRef, CellSnapshot, Range, SheetId, Workbook, col_name, quote_sheet_name};

use crate::app::{Field, Spreadsheet, UndoEntry};
use crate::paint::Op;

/// Most changes listed in the panel (the overlay shows them all).
const MAX_ITEMS: usize = 500;

/// What the workbook is being compared with.
#[derive(Clone, Debug, PartialEq)]
pub enum Against {
    /// A version from the history, by id.
    Version(String),
    /// Another file.
    File(PathBuf),
    /// The document's file as last saved.
    Disk(PathBuf),
}

/// How a cell is highlighted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkKind {
    Added,
    Removed,
    Modified,
}

impl MarkKind {
    fn color(self) -> Hsla {
        match self {
            MarkKind::Added => gpui::hsla(0.37, 0.62, 0.42, 1.0),
            MarkKind::Removed => gpui::hsla(0.0, 0.72, 0.52, 1.0),
            MarkKind::Modified => gpui::hsla(0.12, 0.92, 0.48, 1.0),
        }
    }
}

/// The highlights for one sheet of the current workbook.
#[derive(Default, Debug)]
pub struct SheetMarks {
    /// Changed cell -> (kind, index into that sheet's changes in the diff).
    pub cells: HashMap<CellRef, (MarkKind, usize)>,
    /// Rows / columns (current numbering) just after deleted ones.
    pub deleted_rows: Vec<u32>,
    pub deleted_cols: Vec<u32>,
}

/// One entry in the panel's list of changes.
#[derive(Clone, Debug)]
pub struct Item {
    pub kind: MarkKind,
    pub text: String,
    /// Where to go: sheet name (current workbook) and cell.
    pub sheet: Option<String>,
    pub at: Option<CellRef>,
}

/// A diff being shown.
pub struct DiffView {
    pub against: Against,
    /// "Version of Nov 15, 14:03", "Report.xlsx"…
    pub title: String,
    pub diff: WorkbookDiff,
    /// The other side (the old workbook), for restoring.
    pub other: Workbook,
    /// `edit_generation` when computed (later edits make it outdated).
    pub generation: u64,
    /// Highlights by sheet name.
    pub marks: HashMap<String, SheetMarks>,
    pub items: Vec<Item>,
    /// The document it was made for (ignored once another one is open).
    pub doc: Option<PathBuf>,
}

/// The version-history panel.
pub struct Panel {
    pub versions: Vec<xl_history::Version>,
    pub selected: Option<String>,
    /// Version whose name is being typed (into `HistoryState::label_text`).
    pub naming: Option<String>,
    /// The document whose history this is (another file may be open by now).
    pub doc: Option<PathBuf>,
}

pub struct HistoryState {
    pub store: xl_history::Store,
    pub panel: Option<Panel>,
    pub view: Option<DiffView>,
    /// The changed cell under the pointer.
    pub hover: Option<CellRef>,
    pub label_text: String,
    /// What's being loaded for comparing.
    pub loading: Option<String>,
    /// When a version was last recorded (autosave versions wait for the interval).
    pub last_auto: Option<Instant>,
}

impl HistoryState {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        HistoryState { store: xl_history::Store::new(root), panel: None, view: None, hover: None, label_text: String::new(), loading: None, last_auto: None }
    }
}

impl Default for HistoryState {
    fn default() -> Self {
        Self::new(crate::recent::default_dir().join("history"))
    }
}

/// "Nov 15, 14:03".
pub fn when(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t).format("%b %-d, %H:%M").to_string()
}

fn size_text(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.0} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

fn mark_of(c: &CellChange) -> MarkKind {
    match c.kind {
        CellKind::Added => MarkKind::Added,
        CellKind::Removed => MarkKind::Removed,
        CellKind::Modified => MarkKind::Modified,
    }
}

/// Highlights and list entries for a diff.
pub fn build_view(diff: &WorkbookDiff) -> (HashMap<String, SheetMarks>, Vec<Item>) {
    let mut marks: HashMap<String, SheetMarks> = HashMap::new();
    let mut items = Vec::new();
    for s in &diff.sheets {
        let name = s.name().to_string();
        let here = s.new_name.clone();
        match s.status {
            SheetStatus::Added => items.push(Item { kind: MarkKind::Added, text: format!("Sheet {} added", quote_sheet_name(&name)), sheet: here.clone(), at: Some(CellRef::new(0, 0)) }),
            SheetStatus::Removed => items.push(Item { kind: MarkKind::Removed, text: format!("Sheet {} removed", quote_sheet_name(&name)), sheet: None, at: None }),
            SheetStatus::Matched if s.renamed() => items.push(Item {
                kind: MarkKind::Modified,
                text: format!("Sheet {} renamed to {}", quote_sheet_name(s.old_name.as_deref().unwrap_or("")), quote_sheet_name(&name)),
                sheet: here.clone(),
                at: None,
            }),
            SheetStatus::Matched => {}
        }
        if s.status == SheetStatus::Removed {
            continue;
        }
        let m = marks.entry(name.clone()).or_default();
        for l in &s.lines {
            let what = match (l.axis, l.count) {
                (LineAxis::Row, 1) => format!("row {}", l.start + 1),
                (LineAxis::Row, n) => format!("rows {}–{}", l.start + 1, l.start + n),
                (LineAxis::Col, 1) => format!("column {}", col_name(l.start)),
                (LineAxis::Col, n) => format!("columns {}–{}", col_name(l.start), col_name(l.start + n - 1)),
            };
            let at = match l.axis {
                LineAxis::Row => CellRef::new(l.at_new, 0),
                LineAxis::Col => CellRef::new(0, l.at_new),
            };
            if l.inserted {
                items.push(Item { kind: MarkKind::Added, text: format!("{} {what} inserted", quote_sheet_name(&name)), sheet: here.clone(), at: Some(at) });
            } else {
                match l.axis {
                    LineAxis::Row => m.deleted_rows.push(l.at_new),
                    LineAxis::Col => m.deleted_cols.push(l.at_new),
                }
                items.push(Item { kind: MarkKind::Removed, text: format!("{} {what} deleted (old numbering)", quote_sheet_name(&name)), sheet: here.clone(), at: Some(at) });
            }
        }
        for (i, c) in s.cells.iter().enumerate() {
            if let Some(at) = c.new_at {
                m.cells.insert(at, (mark_of(c), i));
            }
            if c.in_line {
                continue;
            }
            let Some(at) = c.new_at.or(c.old_at) else { continue };
            let prefix = match c.kind {
                CellKind::Added => "added: ",
                CellKind::Removed => "cleared: ",
                CellKind::Modified => "",
            };
            items.push(Item { kind: mark_of(c), text: format!("{}!{at}: {prefix}{}", quote_sheet_name(&name), xl_diff::cell_aspects(c).join("; ")), sheet: here.clone(), at: c.new_at });
        }
        for l in &s.layout {
            items.push(Item { kind: MarkKind::Modified, text: format!("{}: {}", quote_sheet_name(&name), l.text), sheet: here.clone(), at: l.at });
        }
    }
    if diff.sheet_order_changed {
        items.push(Item { kind: MarkKind::Modified, text: "Sheet order changed".into(), sheet: None, at: None });
    }
    for n in &diff.names {
        let (kind, text) = match (&n.old, &n.new) {
            (None, Some(f)) => (MarkKind::Added, format!("Name {} added: {f}", n.name)),
            (Some(f), None) => (MarkKind::Removed, format!("Name {} removed: {f}", n.name)),
            (Some(a), Some(b)) => (MarkKind::Modified, format!("Name {}: {a} → {b}", n.name)),
            (None, None) => continue,
        };
        items.push(Item { kind, text, sheet: None, at: None });
    }
    (marks, items)
}

/// Window options for a copy opened in its own window (like the first window's).
fn window_options(cx: &gpui::App) -> gpui::WindowOptions {
    let bounds = Bounds::centered(None, gpui::size(px(1280.), px(820.)), cx);
    gpui::WindowOptions {
        window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui::TitlebarOptions { title: Some("xl".into()), ..Default::default() }),
        app_id: Some("io.github.yeager620.xl".into()),
        window_min_size: Some(gpui::size(px(560.), px(360.))),
        window_decorations: Some(gpui::WindowDecorations::Client),
        ..Default::default()
    }
}

impl Spreadsheet {
    // ------------------------------------------------------------ recording

    /// After a successful save to `path` (before `self.path` is updated):
    /// record the version, continuing the old path's history on Save As.
    pub(crate) fn record_history(&mut self, path: &Path) {
        let store = &self.history.store;
        if let Some(prev) = self.path.as_ref().filter(|p| p.as_path() != path) {
            store.link(prev, path).ok();
        }
        if store.record(path, None, false).is_ok() {
            self.history.last_auto = Some(Instant::now());
        }
        self.refresh_versions_for(path);
    }

    /// After autosave wrote a recovery copy: record it as a version too,
    /// if the policy asks for that and it's been long enough.
    pub(crate) fn history_autosave(&mut self, recovery: &Path) {
        let minutes = self.history.store.policy.autosave_minutes;
        let Some(path) = self.path.clone() else { return };
        if minutes == 0 || self.history.last_auto.is_some_and(|t| t.elapsed().as_secs() < minutes * 60) {
            return;
        }
        if let Ok(bytes) = std::fs::read(recovery)
            && self.history.store.record_bytes(&path, &bytes, None, true, SystemTime::now()).is_ok()
        {
            self.history.last_auto = Some(Instant::now());
            self.refresh_versions_for(&path);
        }
    }

    /// The panel and diff now belong to `path` (just saved, maybe as a new name).
    fn refresh_versions_for(&mut self, path: &Path) {
        let same = |doc: &Option<PathBuf>| doc.is_none() || doc == &self.path;
        if let Some(p) = self.history.panel.as_mut().filter(|p| same(&p.doc)) {
            p.versions = self.history.store.versions(path);
            p.doc = Some(path.to_path_buf());
        }
        if let Some(v) = self.history.view.as_mut().filter(|v| same(&v.doc)) {
            v.doc = Some(path.to_path_buf());
        }
    }

    /// The diff being shown, if it's about the open document.
    pub(crate) fn shown_diff(&self) -> Option<&DiffView> {
        self.history.view.as_ref().filter(|v| v.doc == self.path)
    }

    // ---------------------------------------------------------------- panel

    pub(crate) fn open_history(&mut self) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let versions = self.path.as_ref().map(|p| self.history.store.versions(p)).unwrap_or_default();
        self.history.panel = Some(Panel { versions, selected: None, naming: None, doc: self.path.clone() });
        if self.path.is_none() {
            self.message = Some(("Save the workbook to start its version history".into(), false));
        }
    }

    pub(crate) fn close_history(&mut self) {
        self.history.panel = None;
        self.history.view = None;
        self.history.hover = None;
        if self.field == Some(Field::VersionLabel) {
            self.field = None;
        }
    }

    /// Stop showing a diff (the panel stays).
    pub(crate) fn close_diff(&mut self) {
        self.history.view = None;
        self.history.hover = None;
        if let Some(p) = self.history.panel.as_mut() {
            p.selected = None;
        }
    }

    /// Compare the workbook with a version from the history.
    pub(crate) fn select_version(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else { return };
        let Some(v) = self.history.panel.as_ref().and_then(|p| p.versions.iter().find(|v| v.id == id)).cloned() else { return };
        if let Some(p) = self.history.panel.as_mut() {
            p.selected = Some(id.clone());
        }
        let bytes = match self.history.store.bytes(&path, &v) {
            Ok(b) => b,
            Err(e) => {
                self.message = Some((format!("Couldn't read that version: {e}"), true));
                return;
            }
        };
        let tmp = self.history.store.root().join("tmp").join(format!("{}-{}.{}", std::process::id(), v.id, v.extension()));
        let title = match &v.label {
            Some(l) => format!("“{l}” ({})", when(v.time)),
            None => format!("the version of {}", when(v.time)),
        };
        self.compare_in_background(Against::Version(id), title, tmp, Some(bytes), cx);
    }

    /// "Changes since last save": the workbook against its file on disk.
    pub(crate) fn changes_since_save(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            self.message = Some(("This workbook hasn't been saved yet".into(), true));
            return;
        };
        if self.history.panel.is_none() {
            self.open_history();
        }
        if let Some(p) = self.history.panel.as_mut() {
            p.selected = None;
        }
        self.compare_in_background(Against::Disk(path.clone()), "the last save".into(), path, None, cx);
    }

    /// "Compare with file…": the workbook against any xlsx/csv.
    pub(crate) fn compare_with_file(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Compare".into()) });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| {
                    this.compare_with_path(path, cx);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    pub(crate) fn compare_with_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.history.panel.is_none() {
            self.open_history();
        }
        if let Some(p) = self.history.panel.as_mut() {
            p.selected = None;
        }
        let title = path.file_name().map_or_else(|| path.display().to_string(), |n| format!("“{}”", n.to_string_lossy()));
        self.compare_in_background(Against::File(path.clone()), title, path, None, cx);
    }

    /// Open `file` (first writing `bytes` to it, then removing it) on a
    /// background thread, then diff against it.
    fn compare_in_background(&mut self, against: Against, title: String, file: PathBuf, bytes: Option<Vec<u8>>, cx: &mut Context<Self>) {
        self.history.loading = Some(title.clone());
        let task = cx.background_executor().spawn(async move {
            if let Some(b) = &bytes {
                if let Some(dir) = file.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                }
                std::fs::write(&file, b).map_err(|e| e.to_string())?;
            }
            let result = xl_io::open(&file).map(|l| l.workbook);
            if bytes.is_some() {
                std::fs::remove_file(&file).ok();
            }
            result
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.history.loading = None;
                match result {
                    Ok(mut other) => {
                        if let Against::File(p) = &against
                            && xl_io::FileKind::of(p) == Some(xl_io::FileKind::Csv)
                        {
                            xl_diff::same_sheet_name(&mut other, &this.wb);
                        }
                        this.show_diff(against, title, other)
                    }
                    Err(e) => this.message = Some((format!("Couldn't open {title}: {e}"), true)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Diff the workbook against `other` and show it.
    pub(crate) fn show_diff(&mut self, against: Against, title: String, other: Workbook) {
        let diff = xl_diff::diff_workbooks(&other, &self.wb);
        let (marks, items) = build_view(&diff);
        let n = diff.change_count();
        self.message = Some((if n == 0 { format!("No changes since {title}") } else { format!("{n} change(s) since {title}") }, false));
        self.history.hover = None;
        self.history.view = Some(DiffView { against, title, diff, other, generation: self.edit_generation, marks, items, doc: self.path.clone() });
    }

    /// Recompute the shown diff after edits.
    pub(crate) fn refresh_diff(&mut self) {
        let Some(v) = self.history.view.take() else { return };
        self.show_diff(v.against, v.title, v.other);
    }

    /// Go to a listed change.
    pub(crate) fn jump_to_change(&mut self, i: usize) {
        let Some(item) = self.shown_diff().and_then(|v| v.items.get(i)).cloned() else { return };
        if let Some(id) = item.sheet.as_deref().and_then(|n| self.wb.sheet_id(n)) {
            self.switch_sheet(id);
        }
        if let Some(at) = item.at {
            self.select(at, false);
            self.ensure_visible(at);
        }
    }

    // ------------------------------------------------------------ restoring

    /// Make the workbook match the compared version, as one undoable step.
    /// Sheets the version doesn't have are kept; sheets only it has are added.
    pub(crate) fn restore_shown(&mut self) {
        if self.shown_diff().is_none() {
            return;
        }
        let Some(view) = self.history.view.take() else { return };
        if self.edit.is_some() && !self.commit_edit() {
            self.history.view = Some(view);
            return;
        }
        let other = &view.other;
        // Layout parts holding formulas (charts, validations…) refer to
        // sheets by id: copy them only when the ids line up.
        let same_ids = other.sheet_ids().iter().all(|&o| self.wb.sheet_id(other.sheet_name(o)) == Some(o));
        let mut steps = Vec::new();
        let mut added = Vec::new();
        for &o in other.sheet_ids() {
            let name = other.sheet_name(o).to_string();
            let sheet = match self.wb.sheet_id(&name) {
                Some(s) => s,
                None => match self.wb.add_sheet(Some(&name)) {
                    Ok(s) => {
                        added.push(name.clone());
                        s
                    }
                    Err(_) => continue,
                },
            };
            let cells_of = |wb: &Workbook, s: SheetId| -> Vec<CellRef> {
                wb.store(s).iter().map(|(c, _)| c).chain(wb.styled_cells(s).map(|(c, _)| c)).chain(wb.formatted_cells(s).map(|(c, _)| c)).collect()
            };
            let addrs: BTreeSet<CellRef> = cells_of(&self.wb, sheet).into_iter().chain(cells_of(other, o)).collect();
            let cells: Vec<(Addr, CellSnapshot)> = addrs.into_iter().map(|c| (Addr::new(sheet, c), other.snapshot(Addr::new(o, c)))).collect();
            let theirs = other.layout(o);
            let layout = if same_ids {
                theirs.clone()
            } else {
                let mut l = self.wb.layout(sheet).clone();
                l.col_widths = theirs.col_widths.clone();
                l.row_heights = theirs.row_heights.clone();
                l.hidden_rows = theirs.hidden_rows.clone();
                l.hidden_cols = theirs.hidden_cols.clone();
                l.merges = theirs.merges.clone();
                l.freeze = theirs.freeze;
                l.notes = theirs.notes.clone();
                l.links = theirs.links.clone();
                l.row_levels = theirs.row_levels.clone();
                l.col_levels = theirs.col_levels.clone();
                l
            };
            steps.push(UndoEntry::Layout { sheet, layout, cells });
        }
        let inverse = self.invert(UndoEntry::Group(steps));
        self.push_undo(inverse);
        self.mark_dirty();
        let extra: Vec<String> = self.wb.sheet_ids().iter().map(|&s| self.wb.sheet_name(s).to_string()).filter(|n| other.sheet_id(n).is_none()).collect();
        let mut msg = format!("Restored {} — Undo (Ctrl+Z) to go back", view.title);
        if !added.is_empty() {
            msg.push_str(&format!("; added sheet(s) {}", added.join(", ")));
        }
        if !extra.is_empty() {
            msg.push_str(&format!("; kept sheet(s) {} that it didn't have", extra.join(", ")));
        }
        self.message = Some((msg, false));
        self.history.hover = None;
        if let Some(p) = self.history.panel.as_mut() {
            p.selected = None;
        }
    }

    /// Open the selected version in a new window, as an unsaved copy.
    pub(crate) fn open_version_copy(&mut self, cx: &mut Context<Self>) {
        let (Some(path), Some(Against::Version(id))) = (self.path.clone(), self.shown_diff().map(|v| v.against.clone())) else { return };
        let Some(v) = self.history.panel.as_ref().and_then(|p| p.versions.iter().find(|v| v.id == id)).cloned() else { return };
        let stem = path.file_stem().map_or("Workbook".to_string(), |s| s.to_string_lossy().into_owned());
        let file = self.history.store.root().join("tmp").join(format!("{stem} ({}).{}", v.id, v.extension()));
        if let Err(e) = self.history.store.export(&path, &v, &file) {
            self.message = Some((format!("Couldn't read that version: {e}"), true));
            return;
        }
        let title = format!("{stem} as of {}", when(v.time));
        let options = window_options(cx);
        let opened = cx.open_window(options, move |window, cx| {
            cx.new(|cx| {
                let mut s = Spreadsheet::new(None, window, cx);
                s.load_copy(file, title, cx);
                s
            })
        });
        if opened.is_err() {
            self.message = Some(("Couldn't open a new window".into(), true));
        }
    }

    /// Load `file` as an untitled copy (Save asks where), then delete it.
    pub(crate) fn load_copy(&mut self, file: PathBuf, title: String, cx: &mut Context<Self>) {
        self.home = None;
        let task = cx.background_executor().spawn({
            let file = file.clone();
            async move { xl_io::open(&file) }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                // Stands in for no document: not recorded as recent, no path.
                this.restore_as = Some((None, file.clone()));
                this.finish_load(file, result);
                this.message = Some((format!("Opened a copy of {title} — save it to keep it"), false));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // --------------------------------------------------------------- naming

    pub(crate) fn start_naming(&mut self, id: String) {
        let current = self.history.panel.as_ref().and_then(|p| p.versions.iter().find(|v| v.id == id)).and_then(|v| v.label.clone()).unwrap_or_default();
        if let Some(p) = self.history.panel.as_mut() {
            p.naming = Some(id);
            self.history.label_text = current;
            self.field = Some(Field::VersionLabel);
        }
    }

    /// "Name this version…" from the palette: the selected version, else the newest.
    pub(crate) fn name_current_version(&mut self) {
        if self.history.panel.is_none() {
            self.open_history();
        }
        let Some(p) = self.history.panel.as_ref() else { return };
        match p.selected.clone().or_else(|| p.versions.first().map(|v| v.id.clone())) {
            Some(id) => self.start_naming(id),
            None => self.message = Some(("Save the workbook first: versions are recorded when you save".into(), true)),
        }
    }

    fn finish_naming(&mut self, commit: bool) {
        self.field = None;
        let Some(id) = self.history.panel.as_mut().and_then(|p| p.naming.take()) else { return };
        let label = std::mem::take(&mut self.history.label_text);
        if !commit {
            return;
        }
        let Some(path) = self.path.clone() else { return };
        match self.history.store.set_label(&path, &id, Some(label.trim())) {
            Ok(()) => self.refresh_versions_for(&path),
            Err(e) => self.message = Some((format!("Couldn't name the version: {e}"), true)),
        }
    }

    pub(crate) fn key_version_label(&mut self, key: &str) -> bool {
        match key {
            "enter" => self.finish_naming(true),
            "escape" => self.finish_naming(false),
            "backspace" => {
                self.history.label_text.pop();
            }
            _ => return false,
        }
        true
    }

    // -------------------------------------------------------------- overlay

    fn current_marks(&self) -> Option<&SheetMarks> {
        self.shown_diff()?.marks.get(self.wb.sheet_name(self.cur))
    }

    /// Grid highlights for the shown diff (green added, red removed or
    /// cleared, yellow changed; a red rule where lines were deleted).
    pub(crate) fn diff_ops(&self, pieces: &dyn Fn(Range) -> Vec<(Bounds<Pixels>, Bounds<Pixels>)>, in_view: &dyn Fn(&Range) -> bool) -> Vec<Op> {
        let Some(m) = self.current_marks() else { return Vec::new() };
        let mut ops = Vec::new();
        for (&cell, &(kind, _)) in &m.cells {
            let r = Range::single(self.wb.layout(self.cur).merge_at(cell).unwrap_or(Range::single(cell)).start);
            if !in_view(&r) {
                continue;
            }
            let color = kind.color();
            let mut soft = color;
            soft.a = 0.16;
            for (rect, clip) in pieces(r) {
                ops.push(Op::Quad(crate::paint::outline(rect, 1.5, color, soft, 0.0), clip));
            }
        }
        let red = MarkKind::Removed.color();
        for &row in &m.deleted_rows {
            let r = Range::new(CellRef::new(row, 0), CellRef::new(row, xl_engine::MAX_COLS - 1));
            if in_view(&r) {
                for (rect, clip) in pieces(r) {
                    ops.push(Op::Quad(fill(crate::paint::rect(f64::from(rect.origin.x), f64::from(rect.origin.y) - 1.5, f64::from(rect.size.width), 3.0), red), clip));
                }
            }
        }
        for &col in &m.deleted_cols {
            let r = Range::new(CellRef::new(0, col), CellRef::new(xl_engine::MAX_ROWS - 1, col));
            if in_view(&r) {
                for (rect, clip) in pieces(r) {
                    ops.push(Op::Quad(fill(crate::paint::rect(f64::from(rect.origin.x) - 1.5, f64::from(rect.origin.y), 3.0, f64::from(rect.size.height)), red), clip));
                }
            }
        }
        ops
    }

    /// Track the changed cell under the pointer (for the old → new card).
    pub(crate) fn diff_hover_at(&mut self, pos: Point<Pixels>) -> bool {
        let over = match (self.shown_diff().is_some(), self.hit_test(pos)) {
            (true, Some(crate::app::Hit::Cell(c))) => {
                let c = self.wb.layout(self.cur).merge_at(c).map_or(c, |m| m.start);
                self.current_marks().filter(|m| m.cells.contains_key(&c)).map(|_| c)
            }
            _ => None,
        };
        let changed = over != self.history.hover;
        self.history.hover = over;
        changed
    }

    /// The change under the pointer: where and what.
    pub(crate) fn render_diff_card(&self) -> Option<(Point<Pixels>, AnyElement)> {
        let cell = self.history.hover?;
        let view = self.shown_diff()?;
        let name = self.wb.sheet_name(self.cur);
        let &(kind, i) = view.marks.get(name)?.cells.get(&cell)?;
        let change = view.diff.sheets.iter().find(|s| s.new_name.as_deref() == Some(name))?.cells.get(i)?;
        let t = self.theme;
        let heading = match change.kind {
            CellKind::Added => format!("{cell}: added"),
            CellKind::Removed => format!("{cell}: cleared"),
            CellKind::Modified => match change.old_at {
                Some(o) if o != cell => format!("{cell} (was {o}): changed"),
                _ => format!("{cell}: changed"),
            },
        };
        let mut card = div()
            .occlude()
            .max_w(px(360.))
            .p(px(10.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .rounded(px(6.))
            .bg(t.popover)
            .border_1()
            .border_color(kind.color())
            .shadow_lg()
            .text_size(px(13.5))
            .text_color(t.ink)
            .child(div().text_size(px(12.)).text_color(t.ink_muted).child(SharedString::from(heading)));
        for line in xl_diff::cell_aspects(change) {
            card = card.child(div().child(SharedString::from(line)));
        }
        let x = f64::from(self.viewport.origin.x) + self.hw() + self.grid_x(cell.col) + self.col_w(cell.col) + 4.0;
        let y = f64::from(self.viewport.origin.y) + self.hh() + self.grid_y(cell.row);
        Some((point(px(x as f32), px(y as f32)), card.into_any_element()))
    }

    // ---------------------------------------------------------------- panel

    pub(crate) fn render_history_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let panel = self.history.panel.as_ref()?;
        let t = self.theme;
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .h(px(28.))
                .px(px(10.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .cursor_pointer()
                .border_1()
                .border_color(t.divider)
                .text_size(px(13.))
                .text_color(t.ink)
                .hover(|s| s.bg(t.chrome_hover))
                .child(label)
        };
        let saved = self.path.is_some();

        // Versions.
        let mut list = div().flex().flex_col();
        if panel.versions.is_empty() {
            list = list.child(div().py(px(6.)).text_size(px(12.5)).text_color(t.ink_muted).child(if saved {
                "No versions yet. A version is recorded every time you save."
            } else {
                "Save the workbook to start its version history."
            }));
        }
        for (i, v) in panel.versions.iter().enumerate() {
            let selected = panel.selected.as_deref() == Some(v.id.as_str());
            let id = v.id.clone();
            let mut detail = format!("{} · {}", crate::lifecycle::ago(v.time), size_text(v.size));
            if v.auto {
                detail.push_str(" · autosave");
            }
            if i == 0 {
                detail.push_str(" · latest");
            }
            let naming = panel.naming.as_deref() == Some(v.id.as_str());
            let title: AnyElement = if naming {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(22.))
                    .px(px(4.))
                    .rounded(px(3.))
                    .bg(t.canvas)
                    .border_1()
                    .border_color(t.accent)
                    .text_size(px(13.5))
                    .child(SharedString::from(self.history.label_text.clone()))
                    .child(div().w(px(1.5)).h(px(15.)).bg(t.accent))
                    .into_any_element()
            } else {
                let mut row = div().flex().flex_row().gap(px(6.)).text_size(px(13.5)).text_color(t.ink).child(SharedString::from(when(v.time)));
                if let Some(l) = &v.label {
                    row = row.child(div().font_weight(FontWeight::SEMIBOLD).overflow_hidden().whitespace_nowrap().child(SharedString::from(l.clone())));
                }
                row.into_any_element()
            };
            let name_id = id.clone();
            list = list.child(
                div()
                    .id(("hv-ver", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .px(px(8.))
                    .py(px(5.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(t.header_selected))
                    .when(!selected, |d| d.hover(|s| s.bg(t.chrome_hover)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_version(id.clone(), cx);
                        cx.notify();
                    }))
                    .child(div().flex_1().min_w(px(0.)).flex().flex_col().gap(px(1.)).child(title).child(div().text_size(px(11.5)).text_color(t.ink_muted).child(SharedString::from(detail))))
                    .when(!naming, |d| {
                        d.child(
                            div()
                                .id(("hv-name", i))
                                .px(px(6.))
                                .h(px(20.))
                                .flex()
                                .items_center()
                                .rounded(px(3.))
                                .text_size(px(12.))
                                .text_color(t.accent)
                                .hover(|s| s.bg(t.chrome_hover))
                                .child("Name…")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.start_naming(name_id.clone());
                                    cx.stop_propagation();
                                    cx.notify();
                                })),
                        )
                    }),
            );
        }

        let mut root = div()
            .w(px(320.))
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
                    .child(div().flex_1().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child("Version history"))
                    .child(div().id("hv-close").px(px(6.)).h(px(22.)).flex().items_center().rounded(px(3.)).cursor_pointer().hover(|s| s.bg(t.chrome_hover)).child(crate::chrome::icon("x", t.ink_muted)).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.close_history();
                            cx.notify();
                        }),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(6.))
                    .when(saved, |d| {
                        d.child(button("hv-since-save", "Changes since save").on_click(cx.listener(|this, _, _, cx| {
                            this.changes_since_save(cx);
                            cx.notify();
                        })))
                    })
                    .child(button("hv-compare", "Compare with file…").on_click(cx.listener(|this, _, _, cx| {
                        this.compare_with_file(cx);
                        cx.notify();
                    }))),
            )
            .when(panel.doc == self.path, |d| d.child(div().id("hv-versions").max_h(px(260.)).flex_none().overflow_y_scroll().child(list)));

        if let Some(what) = &self.history.loading {
            root = root.child(div().text_size(px(12.5)).text_color(t.ink_muted).child(SharedString::from(format!("Comparing with {what}…"))));
        }
        if panel.doc != self.path {
            let name = panel.doc.as_ref().and_then(|p| p.file_name()).map_or("another workbook".to_string(), |n| format!("“{}”", n.to_string_lossy()));
            return Some(
                root.child(div().text_size(px(12.5)).text_color(t.ink_muted).child(SharedString::from(format!("This was the history of {name}."))))
                    .child(button("hv-reload", "Show this workbook's history").on_click(cx.listener(|this, _, _, cx| {
                        this.history.view = None;
                        this.open_history();
                        cx.notify();
                    })))
                    .into_any_element(),
            );
        }
        if let Some(view) = self.shown_diff() {
            let outdated = view.generation != self.edit_generation;
            let n = view.diff.change_count();
            let mut changes = div().flex().flex_col().gap(px(1.));
            for (i, item) in view.items.iter().take(MAX_ITEMS).enumerate() {
                changes = changes.child(
                    div()
                        .id(("hv-chg", i))
                        .flex()
                        .flex_row()
                        .gap(px(6.))
                        .px(px(6.))
                        .py(px(3.))
                        .rounded(px(3.))
                        .cursor_pointer()
                        .hover(|s| s.bg(t.chrome_hover))
                        .text_size(px(12.5))
                        .text_color(t.ink)
                        .child(div().flex_none().mt(px(4.)).size(px(8.)).rounded(px(2.)).bg(item.kind.color()))
                        .child(div().flex_1().min_w(px(0.)).child(SharedString::from(item.text.clone())))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.jump_to_change(i);
                            cx.notify();
                        })),
                );
            }
            if view.items.len() > MAX_ITEMS {
                changes = changes.child(div().px(px(6.)).text_size(px(12.)).text_color(t.ink_muted).child(SharedString::from(format!("… and {} more (highlighted on the grid)", view.items.len() - MAX_ITEMS))));
            }
            let restore_label = match view.against {
                Against::Version(_) => "Restore this version",
                Against::Disk(_) => "Revert to saved",
                Against::File(_) => "Replace with this file",
            };
            root = root
                .child(div().h(px(1.)).bg(t.divider))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .child(
                            div().flex_1().text_size(px(13.)).text_color(t.ink).child(SharedString::from(match n {
                                0 => format!("No changes since {}", view.title),
                                1 => format!("1 change since {}", view.title),
                                n => format!("{n} changes since {}", view.title),
                            })),
                        )
                        .child(div().id("hv-done").px(px(6.)).h(px(22.)).flex().items_center().rounded(px(3.)).cursor_pointer().text_size(px(12.5)).text_color(t.accent).hover(|s| s.bg(t.chrome_hover)).child("Done").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.close_diff();
                                cx.notify();
                            },
                        ))),
                )
                .when(outdated, |d| {
                    d.child(button("hv-refresh", "You've edited since — refresh").on_click(cx.listener(|this, _, _, cx| {
                        this.refresh_diff();
                        cx.notify();
                    })))
                })
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.))
                        .when(n > 0, |d| {
                            d.child(button("hv-restore", restore_label).on_click(cx.listener(|this, _, _, cx| {
                                this.restore_shown();
                                cx.notify();
                            })))
                        })
                        .when(matches!(view.against, Against::Version(_)), |d| {
                            d.child(button("hv-copy", "Open as copy").on_click(cx.listener(|this, _, _, cx| {
                                this.open_version_copy(cx);
                                cx.notify();
                            })))
                        }),
                )
                .child(div().id("hv-changes").flex_1().min_h(px(0.)).overflow_y_scroll().child(changes));
        } else {
            root = root.child(div().text_size(px(12.)).text_color(t.ink_faint).child("Pick a version to see what changed since. Changed cells are highlighted: green added, red removed, yellow changed."));
        }
        Some(root.into_any_element())
    }
}
