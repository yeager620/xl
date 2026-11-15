//! Big data in the UI: progress (with Cancel) while a large file opens, the
//! "Query with SQL…" command and refreshing its results, the row-header
//! width for very tall sheets, and the warning when a sheet is too tall to
//! save as .xlsx.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{Context, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement, Styled, div, px, relative};
use xl_engine::{Addr, CellRef, Range, SheetId};

use crate::app::{Spreadsheet, UndoEntry};

/// A query whose results are on a sheet, so they can be refreshed.
#[derive(Clone, Debug)]
pub(crate) struct QueryDef {
    pub sql: String,
    pub sheet: SheetId,
    pub anchor: CellRef,
    /// Where the last results landed (header included).
    pub region: Range,
}

#[derive(Default)]
pub(crate) struct BigData {
    /// Progress of the file being opened.
    pub progress: Option<Arc<xl_io::Progress>>,
    pub queries: Vec<QueryDef>,
    /// The SQL database, kept between queries so loaded sheets are reused
    /// until the workbook changes.
    pub sql: Option<xl_sql::SqlEngine>,
    /// Saving as xlsx was refused: (the xlsx path, why).
    pub xlsx_refused: Option<(PathBuf, String)>,
    /// Status-bar totals of the last selection, keyed by what they depend on
    /// (a whole selected column of a tall sheet is millions of cells).
    summary: std::cell::Cell<Option<SummaryKey>>,
}

type SummaryKey = ((u64, SheetId, Range), (f64, u64, u64));

impl BigData {
    /// Another workbook replaced the current one: its queries, cached SQL
    /// tables and totals don't apply any more.
    pub(crate) fn forget_workbook(&mut self) {
        self.queries.clear();
        self.sql = None;
        self.summary.set(None);
    }
}

/// Width of the row-header strip for row numbers up to `rows` (the
/// default fits five digits; taller sheets widen it).
pub(crate) fn header_width(rows: u32) -> f64 {
    let digits = rows.max(1).ilog10() + 1;
    crate::app::HEADER_W.max(digits as f64 * 7.0 + 11.0)
}

/// Results of a query over this many cells replace without an undo step.
const UNDO_MAX_CELLS: u64 = 200_000;

impl Spreadsheet {
    /// Row-header width for the rows currently in view.
    pub(crate) fn header_w(&self) -> f64 {
        header_width(self.row_at_grid(self.grid_h()).saturating_add(1))
    }

    /// Sum / number count / count of the selection, cached until it or the
    /// workbook changes.
    pub(crate) fn summarize_cached(&self, sel: Range) -> (f64, u64, u64) {
        let key = (self.edit_generation, self.cur, sel);
        if let Some((k, v)) = self.big.summary.get()
            && k == key
        {
            return v;
        }
        let v = self.wb.store(self.cur).summarize(sel);
        self.big.summary.set(Some((key, v)));
        v
    }

    // ------------------------------------------------------------ loading

    /// Start a ticker that repaints while a file loads (for the progress bar).
    pub(crate) fn watch_progress(&mut self, progress: Arc<xl_io::Progress>, cx: &mut Context<Self>) {
        self.big.progress = Some(progress);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                let still = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.loading.is_some()
                    })
                    .unwrap_or(false);
                if !still {
                    break;
                }
            }
        })
        .detach();
    }

    pub(crate) fn cancel_load(&mut self) {
        if let Some(p) = &self.big.progress {
            p.cancel();
        }
    }

    pub(crate) fn render_load_progress(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let name = self.loading.clone().unwrap_or_default();
        let (phase, frac) = self.big.progress.as_ref().map_or(("Opening", 0.0), |p| p.status());
        let cancelling = self.big.progress.as_ref().is_some_and(|p| p.cancelled());
        let label = if cancelling { format!("Cancelling {name}…") } else { format!("{phase} {name}… {:.0}%", frac * 100.0) };
        div().absolute().bottom(px(44.)).left_0().right_0().flex().justify_center().child(
            div()
                .id("load-progress")
                .w(px(420.))
                .p(px(14.))
                .flex()
                .flex_col()
                .gap(px(10.))
                .rounded(px(8.))
                .bg(t.popover)
                .border_1()
                .border_color(t.divider)
                .shadow_lg()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.))
                        .child(div().flex_1().overflow_hidden().whitespace_nowrap().text_size(px(13.5)).text_color(t.ink).child(SharedString::from(label)))
                        .child(
                            div()
                                .id("load-cancel")
                                .px(px(10.))
                                .h(px(26.))
                                .flex()
                                .items_center()
                                .rounded(px(4.))
                                .cursor_pointer()
                                .text_size(px(13.))
                                .text_color(t.ink)
                                .border_1()
                                .border_color(t.divider)
                                .hover(|s| s.bg(t.chrome_hover))
                                .child("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.cancel_load();
                                    cx.notify();
                                })),
                        ),
                )
                .child(div().h(px(4.)).w_full().rounded(px(2.)).bg(t.divider).child(div().h_full().rounded(px(2.)).bg(t.accent).w(relative(frac as f32)))),
        )
    }

    // ------------------------------------------------------------ queries

    /// Run the Query dialog: SQL, and where to put the results (empty or
    /// "new sheet" for a new sheet, else a cell such as `Sheet2!A1`).
    pub(crate) fn run_query_form(&mut self, sql: String, target: String) {
        let target = target.trim();
        let dest = if target.is_empty() || target.eq_ignore_ascii_case("new sheet") {
            None
        } else {
            match self.wb.resolve_reference(target, self.cur) {
                Some((sheet, r)) => Some((sheet, r.start)),
                None => {
                    self.message = Some((format!("{target} isn't a cell — leave it empty for a new sheet"), true));
                    return;
                }
            }
        };
        match self.run_query(&sql, dest, None) {
            Ok(msg) => {
                self.form = None;
                self.field = None;
                self.message = Some((msg, false));
            }
            Err(e) => self.message = Some((e, true)),
        }
    }

    /// Run `sql` and put the results at `dest` (None: a new sheet). With
    /// `replacing`, the results of query number `replacing` are replaced.
    fn run_query(&mut self, sql: &str, dest: Option<(SheetId, CellRef)>, replacing: Option<usize>) -> Result<String, String> {
        if self.edit.is_some() && !self.commit_edit() {
            return Err("Finish editing the cell first".into());
        }
        let anchor = dest.map_or(CellRef::new(0, 0), |d| d.1);
        let base = self.path.as_ref().and_then(|p| p.parent()).map(|p| p.to_path_buf());
        if self.big.sql.is_none() {
            self.big.sql = Some(xl_sql::SqlEngine::new()?);
        }
        let engine = self.big.sql.as_mut().expect("just made");
        let result = engine.run(&self.wb, sql, self.edit_generation, base.as_deref(), anchor)?;
        let rows = result.rows;
        let region = result.region(anchor);
        let sheet = match dest {
            Some((sheet, _)) => {
                let old = replacing.and_then(|i| self.big.queries.get(i)).filter(|q| q.sheet == sheet).map(|q| q.region);
                // Never write over cells that aren't this query's own results.
                if let Some(c) = self.wb.store(sheet).iter_range(region).map(|(c, _)| c).find(|c| !old.is_some_and(|o| o.contains(*c))) {
                    return Err(format!("The results ({region}) would overwrite {c} — pick an empty spot or a new sheet"));
                }
                if region.cell_count() + old.map_or(0, |o| o.cell_count()) <= UNDO_MAX_CELLS {
                    let mut addrs: Vec<Addr> = region.iter().map(|c| Addr::new(sheet, c)).collect();
                    if let Some(o) = old {
                        addrs.extend(o.iter().filter(|c| !region.contains(*c)).map(|c| Addr::new(sheet, c)));
                    }
                    let before = self.snapshots(addrs);
                    self.push_undo(UndoEntry::Cells(before));
                }
                if let Some(o) = old {
                    self.wb.clear_region(sheet, o);
                }
                sheet
            }
            None => {
                let name = (1..).map(|i| format!("Query{i}")).find(|n| self.wb.sheet_id(n).is_none()).unwrap_or_default();
                self.wb.add_sheet(Some(&name))?
            }
        };
        result.place(&mut self.wb, sheet, anchor)?;
        let def = QueryDef { sql: sql.to_string(), sheet, anchor, region };
        match replacing {
            Some(i) if i < self.big.queries.len() => self.big.queries[i] = def,
            _ => self.big.queries.push(def),
        }
        self.mark_dirty();
        self.switch_sheet(sheet);
        self.select(anchor, false);
        Ok(format!("Query returned {rows} row{} — Refresh query updates it", if rows == 1 { "" } else { "s" }))
    }

    /// Re-run the query whose results hold the active cell (or the latest).
    pub(crate) fn refresh_query(&mut self) {
        let i = self.big.queries.iter().rposition(|q| q.sheet == self.cur && q.region.contains(self.active)).or_else(|| self.big.queries.len().checked_sub(1));
        let Some(i) = i else {
            self.message = Some(("No query to refresh — run one with Query with SQL…".into(), true));
            return;
        };
        let q = self.big.queries[i].clone();
        if !self.wb.sheet_ids().contains(&q.sheet) {
            self.message = Some(("The query's sheet was deleted".into(), true));
            return;
        }
        let r = self.run_query(&q.sql, Some((q.sheet, q.anchor)), Some(i));
        self.message = Some(match r {
            Ok(m) => (m.replace(" — Refresh query updates it", " (refreshed)"), false),
            Err(e) => (e, true),
        });
    }

    pub(crate) fn refresh_all_queries(&mut self) {
        let n = self.big.queries.len();
        for i in 0..n {
            let q = self.big.queries[i].clone();
            if !self.wb.sheet_ids().contains(&q.sheet) {
                continue;
            }
            if let Err(e) = self.run_query(&q.sql, Some((q.sheet, q.anchor)), Some(i)) {
                self.message = Some((e, true));
                return;
            }
        }
        self.message = Some((format!("Refreshed {n} quer{}", if n == 1 { "y" } else { "ies" }), false));
    }

    // ------------------------------------------------------- xlsx limit

    /// Saving as .xlsx can't hold this workbook: ask instead (returns true
    /// when the save was stopped).
    pub(crate) fn refuse_tall_xlsx(&mut self, path: &std::path::Path) -> bool {
        if xl_io::FileKind::of(path) != Some(xl_io::FileKind::Xlsx) {
            return false;
        }
        match xl_io::xlsx_overflow(&self.wb) {
            Some((sheet, rows)) => {
                self.big.xlsx_refused = Some((path.to_path_buf(), xl_io::xlsx_overflow_message(&sheet, rows)));
                true
            }
            None => false,
        }
    }

    /// Save the too-tall sheet as CSV next to the refused xlsx path.
    pub(crate) fn save_tall_as_csv(&mut self) {
        let Some((path, _)) = self.big.xlsx_refused.take() else { return };
        if let Some((name, _)) = xl_io::xlsx_overflow(&self.wb)
            && let Some(id) = self.wb.sheet_id(&name)
        {
            self.switch_sheet(id);
        }
        self.save_to(path.with_extension("csv"));
    }

    pub(crate) fn key_xlsx_refused(&mut self, key: &str) -> bool {
        match key {
            "enter" => self.save_tall_as_csv(),
            "escape" => self.big.xlsx_refused = None,
            _ => {}
        }
        true
    }

    pub(crate) fn render_xlsx_refused(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let why = self.big.xlsx_refused.as_ref().map(|(_, w)| w.clone()).unwrap_or_default();
        let others = self.wb.sheet_ids().len() > 1;
        let button = |id: &'static str, label: &'static str| div().id(id).flex().items_center().justify_center().h(px(32.)).px(px(14.)).rounded(px(4.)).cursor_pointer().text_size(px(14.)).child(label);
        div()
            .id("xlsx-limit-scrim")
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
                    .id("xlsx-limit-dialog")
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
                    .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child("Too many rows for .xlsx"))
                    .child(div().text_size(px(14.)).text_color(t.ink_muted).child(SharedString::from(why)))
                    .child(div().text_size(px(14.)).text_color(t.ink_muted).child(if others { "A .csv file holds one sheet: the tall one is saved." } else { "Nothing was saved yet." }))
                    .child(
                        div()
                            .mt(px(12.))
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .child(div().flex_1())
                            .child(button("xlsx-limit-cancel", "Cancel").text_color(t.ink).border_1().border_color(t.divider).hover(|s| s.bg(t.chrome_hover)).on_click(cx.listener(|this, _, _, cx| {
                                this.big.xlsx_refused = None;
                                cx.notify();
                            })))
                            .child(button("xlsx-limit-csv", "Save as CSV").bg(t.accent).text_color(t.on_accent).font_weight(FontWeight::SEMIBOLD).on_click(cx.listener(|this, _, _, cx| {
                                this.save_tall_as_csv();
                                cx.notify();
                            }))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_grows_with_row_numbers() {
        assert_eq!(header_width(1), crate::app::HEADER_W);
        assert_eq!(header_width(99_999), crate::app::HEADER_W);
        assert!(header_width(16_777_216) > header_width(1_048_576));
        assert!(header_width(16_777_216) >= 8.0 * 7.0);
    }
}
