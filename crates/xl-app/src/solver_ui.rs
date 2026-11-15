//! Goal Seek and Solver: a small form dialog that collects cell references
//! and runs the engine's solvers, as one undoable step.

use gpui::{Context, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px};
use xl_engine::solver::{Constraint, ConstraintOp, GoalSeekOptions, SolverGoal, SolverMethod, SolverOptions, SolverProblem, SolverStatus, goal_seek, solve};
use xl_engine::{Addr, CellRef, Value};

use crate::app::{Field, Spreadsheet, UndoEntry};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FormKind {
    GoalSeek,
    Solver,
    /// Insert or edit the active cell's hyperlink.
    Link,
    /// Data validation for the selection.
    Validation,
}

pub(crate) struct FormDialog {
    pub kind: FormKind,
    /// (label, hint, text) per field.
    pub fields: Vec<(&'static str, &'static str, String)>,
    /// Solver: maximize, minimize or hit a value (the value is field 1).
    pub goal: SolverGoalKind,
    pub non_negative: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SolverGoalKind {
    Max,
    Min,
    Value,
}

impl Spreadsheet {
    pub(crate) fn open_form(&mut self, kind: FormKind) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let here = self.active.to_string();
        let fields = match kind {
            FormKind::GoalSeek => vec![
                ("Set cell", "A formula cell, e.g. B7", here),
                ("To value", "The result you want", String::new()),
                ("By changing cell", "A number the formula depends on", String::new()),
            ],
            FormKind::Validation => vec![
                ("Allow", "list · whole · decimal · date · time · length · custom", "list".to_string()),
                ("Condition", "between · = · <> · > · < · >= · <= (not for lists)", "between".to_string()),
                ("Choices or value", "Red, Green, Blue  ·  =$H$2:$H$9  ·  1  ·  =A1>0", String::new()),
                ("Second value", "For between", String::new()),
                ("Error message", "Shown when a value is refused", String::new()),
            ],
            FormKind::Link => {
                let a = self.at(self.active);
                vec![
                    ("Address", "https://… or a place like Sheet2!A1", self.link_at(self.active).map(|l| l.target().to_string()).unwrap_or_default()),
                    ("Text to display", "Leave as is", self.wb.display(a).text),
                ]
            }
            FormKind::Solver => vec![
                ("Objective cell", "The formula to optimize", here),
                ("Target value", "Only for “value of”", String::new()),
                ("Variable cells", "e.g. B2:B4 or B2, D2", String::new()),
                ("Constraints", "e.g. B5 <= 100; B2:B4 int; C9 = D9", String::new()),
            ],
        };
        self.form = Some(FormDialog { kind, fields, goal: SolverGoalKind::Max, non_negative: true });
        self.field = Some(Field::Form(match kind {
            FormKind::GoalSeek => 1,
            FormKind::Solver => 2,
            FormKind::Link => 0,
            FormKind::Validation => 2,
        }));
    }

    fn cell_ref(&self, text: &str) -> Result<Addr, String> {
        match self.wb.resolve_reference(text, self.cur) {
            Some((sheet, r)) if r.start == r.end => Ok(Addr::new(sheet, r.start)),
            Some(_) => Err(format!("{text} should be a single cell")),
            None => Err(format!("{} isn't a cell reference", if text.trim().is_empty() { "(empty)" } else { text.trim() })),
        }
    }

    /// Cells of a list of references ("B2:B4, D2").
    fn cells_ref(&self, text: &str) -> Result<Vec<Addr>, String> {
        let mut out = Vec::new();
        for part in text.split([',', ';']).map(str::trim).filter(|p| !p.is_empty()) {
            let (sheet, r) = self.wb.resolve_reference(part, self.cur).ok_or_else(|| format!("{part} isn't a cell reference"))?;
            if r.cell_count() > 10_000 {
                return Err(format!("{part} has too many cells"));
            }
            out.extend(r.iter().map(|c| Addr::new(sheet, c)));
        }
        if out.is_empty() {
            return Err("Enter the cells to change".into());
        }
        Ok(out)
    }

    /// A number, or a cell's current number.
    fn number_or_cell(&self, text: &str) -> Result<f64, String> {
        let t = text.trim();
        if let Ok(n) = t.parse::<f64>() {
            return Ok(n);
        }
        let a = self.cell_ref(t)?;
        match self.wb.value(a) {
            Value::Number(n) => Ok(*n),
            _ => Err(format!("{t} doesn't hold a number")),
        }
    }

    /// "B5 <= 100; B2:B4 int; C9 = D9"
    fn parse_constraints(&self, text: &str) -> Result<Vec<Constraint>, String> {
        let mut out = Vec::new();
        for part in text.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            let lower = part.to_ascii_lowercase();
            let (lhs, op, rhs) = if let Some(l) = lower.strip_suffix(" int").or_else(|| lower.strip_suffix(" integer")) {
                (part[..l.len()].to_string(), ConstraintOp::Int, None)
            } else if let Some(l) = lower.strip_suffix(" bin").or_else(|| lower.strip_suffix(" binary")) {
                (part[..l.len()].to_string(), ConstraintOp::Bin, None)
            } else {
                let ops = [("<=", ConstraintOp::Le), (">=", ConstraintOp::Ge), ("=<", ConstraintOp::Le), ("=>", ConstraintOp::Ge), ("<", ConstraintOp::Le), (">", ConstraintOp::Ge), ("=", ConstraintOp::Eq)];
                let (sym, op) = ops.iter().find(|(s, _)| part.contains(s)).ok_or_else(|| format!("“{part}” needs <=, >=, =, int or bin"))?;
                let (l, r) = part.split_once(sym).unwrap_or((part, ""));
                (l.to_string(), *op, Some(self.number_or_cell(r)?))
            };
            for cell in self.cells_ref(&lhs)? {
                out.push(Constraint { cell, op, rhs: rhs.unwrap_or(0.0) });
            }
        }
        Ok(out)
    }

    /// Run the dialog's Goal Seek or Solver.
    pub(crate) fn run_form(&mut self) {
        let Some(form) = self.form.as_ref() else { return };
        let texts: Vec<String> = form.fields.iter().map(|f| f.2.clone()).collect();
        let text = |i: usize| texts.get(i).cloned().unwrap_or_default();
        let (kind, goal_kind, non_negative) = (form.kind, form.goal, form.non_negative);
        if kind == FormKind::Link {
            self.apply_link_form(text(0), text(1));
            return;
        }
        if kind == FormKind::Validation {
            match self.apply_validation_form(&text(0), &text(1), &text(2), &text(3), &text(4)) {
                Ok(()) => {
                    self.form = None;
                    self.field = None;
                    self.message = Some(("Added the rule — values that break it will be refused".into(), false));
                }
                Err(e) => self.message = Some((e, true)),
            }
            return;
        }
        let result: Result<(Vec<Addr>, String), String> = match kind {
            FormKind::Link | FormKind::Validation => unreachable!("handled above"),
            FormKind::GoalSeek => (|| {
                let set = self.cell_ref(&text(0))?;
                let target = text(1).trim().parse::<f64>().map_err(|_| "Enter the value to reach".to_string())?;
                let changing = self.cell_ref(&text(2))?;
                let before = self.snapshots([changing]);
                match goal_seek(&mut self.wb, set, target, changing, &GoalSeekOptions::default()) {
                    Ok(r) => {
                        self.push_undo(UndoEntry::Cells(before));
                        Ok((vec![changing], format!("Goal Seek found a solution: {} = {} gives {}", text(2).trim(), xl_engine::precise(r.value), xl_engine::precise((r.achieved * 1e9).round() / 1e9))))
                    }
                    Err(e) => Err(e),
                }
            })(),
            FormKind::Solver => (|| {
                let objective = self.cell_ref(&text(0))?;
                let goal = match goal_kind {
                    SolverGoalKind::Max => SolverGoal::Max,
                    SolverGoalKind::Min => SolverGoal::Min,
                    SolverGoalKind::Value => SolverGoal::Value(self.number_or_cell(&text(1))?),
                };
                let variables = self.cells_ref(&text(2))?;
                let constraints = self.parse_constraints(&text(3))?;
                let problem = SolverProblem { objective, goal, variables: variables.clone(), constraints, non_negative };
                let before = self.snapshots(variables.iter().copied());
                let r = solve(&mut self.wb, &problem, &SolverOptions::default())?;
                let method = if r.method == SolverMethod::Linear { "linear model, exact" } else { "nonlinear model" };
                let objective_text = xl_engine::precise((r.objective * 1e9).round() / 1e9);
                match r.status {
                    SolverStatus::Optimal => {
                        self.push_undo(UndoEntry::Cells(before));
                        Ok((variables, format!("Solver found an optimal solution: objective {objective_text} ({method})")))
                    }
                    SolverStatus::Feasible => {
                        self.push_undo(UndoEntry::Cells(before));
                        Ok((variables, format!("Solver found a solution that meets the constraints (objective {objective_text}) but couldn't prove it's the best")))
                    }
                    SolverStatus::Infeasible => Err("No values satisfy all the constraints".into()),
                    SolverStatus::Unbounded => Err("The objective can grow without limit — add constraints".into()),
                    SolverStatus::Failed(e) => Err(e),
                }
            })(),
        };
        match result {
            Ok((changed, msg)) => {
                self.form = None;
                self.field = None;
                self.mark_dirty();
                self.message = Some((msg, false));
                if let Some(first) = changed.first() {
                    self.switch_sheet(first.sheet);
                    self.select(first.cell, false);
                }
                // Let tables and pivots notice the new values.
                let cells: Vec<CellRef> = changed.iter().filter(|a| a.sheet == self.cur).map(|a| a.cell).collect();
                self.refresh_pivots_using(self.cur, &cells);
            }
            Err(e) => self.message = Some((e, true)),
        }
    }

    pub(crate) fn link_at(&self, cell: CellRef) -> Option<&xl_engine::Link> {
        let l = self.wb.layout(self.cur);
        l.links.get(&l.merge_at(cell).map_or(cell, |m| m.start))
    }

    /// Save the link dialog: a web address (anything with a scheme or a dot
    /// and no "!"), else a place in the workbook. Empty removes the link.
    fn apply_link_form(&mut self, address: String, display: String) {
        let cell = self.active;
        let address = address.trim().to_string();
        let link = if address.is_empty() {
            None
        } else if address.contains("://") || address.starts_with("mailto:") {
            Some(xl_engine::Link::Url { url: address.clone(), tooltip: None })
        } else if self.wb.resolve_reference(&address, self.cur).is_some() {
            Some(xl_engine::Link::Place { location: address.clone(), tooltip: None })
        } else if address.contains('.') && !address.contains('!') {
            Some(xl_engine::Link::Url { url: format!("https://{address}"), tooltip: None })
        } else {
            self.message = Some((format!("{address} isn't a web address or a place in this workbook"), true));
            return;
        };
        self.form = None;
        self.field = None;
        let a = self.at(cell);
        // The text to show (the address, if the cell was empty), styled like a link.
        let current = self.wb.display(a).text;
        let display = if display.trim().is_empty() { if current.is_empty() { address.clone() } else { current.clone() } } else { display };
        let mut snap = self.wb.snapshot(a);
        if display != current {
            snap.input = display;
        }
        if link.is_some() && snap.style.color.is_none() && !snap.style.underline {
            snap.style.color = Some(0x0563C1);
            snap.style.underline = true;
        }
        let sheet = self.cur;
        self.change_layout(Some(xl_engine::Range::single(cell)), |s| {
            s.wb.restore(vec![(a, snap)]);
            if let Some(l) = s.wb.layout_mut(sheet) {
                match link {
                    Some(link) => {
                        l.links.insert(cell, link);
                    }
                    None => {
                        l.links.remove(&cell);
                    }
                }
            }
        });
    }

    pub(crate) fn remove_link(&mut self) {
        let sel = self.selection();
        self.context = None;
        self.change_layout(None, |s| {
            if let Some(l) = s.wb.layout_mut(s.cur) {
                l.links.retain(|c, _| !sel.contains(*c));
            }
        });
    }

    /// Ctrl+click: open a web link, or go to a place.
    pub(crate) fn follow_link(&mut self, cell: CellRef, cx: &mut Context<Self>) {
        match self.link_at(cell).cloned() {
            Some(xl_engine::Link::Url { url, .. }) => {
                cx.open_url(&url);
                self.message = Some((format!("Opened {url}"), false));
            }
            Some(xl_engine::Link::Place { location, .. }) => self.name_box_go(&location),
            None => {}
        }
    }

    pub(crate) fn key_form(&mut self, key: &str) -> bool {
        let Some(form) = self.form.as_mut() else { return false };
        let n = form.fields.len() as u8;
        let Some(Field::Form(i)) = self.field else {
            if key == "escape" {
                self.form = None;
            }
            return true;
        };
        match key {
            "enter" => self.run_form(),
            "escape" => {
                self.form = None;
                self.field = None;
            }
            "tab" => self.field = Some(Field::Form((i + 1) % n)),
            "backspace" => {
                if let Some(f) = form.fields.get_mut(i as usize) {
                    f.2.pop();
                }
            }
            _ if key.chars().count() == 1 || key == "space" => return false,
            _ => {}
        }
        true
    }

    pub(crate) fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let Some(form) = &self.form else { return div().into_any_element() };
        let title = match form.kind {
            FormKind::GoalSeek => "Goal Seek",
            FormKind::Solver => "Solver",
            FormKind::Link => "Link",
            FormKind::Validation => "Data validation",
        };
        let mut body = div().flex().flex_col().gap(px(8.));
        for (i, (label, hint, text)) in form.fields.iter().enumerate() {
            if form.kind == FormKind::Solver && i == 1 && form.goal != SolverGoalKind::Value {
                continue;
            }
            let focused = self.field == Some(Field::Form(i as u8));
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(div().text_size(px(13.)).text_color(t.ink_muted).child(*label))
                    .child(
                        div()
                            .id(("form-field", i))
                            .h(px(30.))
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
                            .text_size(px(14.))
                            .when(text.is_empty() && !focused, |d| d.text_color(t.ink_faint).child(*hint))
                            .when(!text.is_empty(), |d| d.text_color(t.ink).child(SharedString::from(text.clone())))
                            .when(focused, |d| d.child(div().w(px(1.5)).h(px(16.)).bg(t.accent)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    this.focus.focus(window, cx);
                                    this.field = Some(Field::Form(i as u8));
                                    cx.stop_propagation();
                                    cx.notify();
                                }),
                            ),
                    ),
            );
            if form.kind == FormKind::Solver && i == 0 {
                let chip = |id: &'static str, label: &'static str, kind: SolverGoalKind| {
                    let on = form.goal == kind;
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
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(f) = this.form.as_mut() {
                                f.goal = kind;
                            }
                            cx.notify();
                        }))
                        .child(label)
                };
                body = body.child(div().flex().flex_row().gap(px(6.)).child(chip("goal-max", "Maximize", SolverGoalKind::Max)).child(chip("goal-min", "Minimize", SolverGoalKind::Min)).child(chip("goal-value", "Value of", SolverGoalKind::Value)));
            }
        }
        if form.kind == FormKind::Solver {
            let on = form.non_negative;
            body = body.child(
                div()
                    .id("form-nonneg")
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
                    .child("Make variables without constraints non-negative")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(f) = this.form.as_mut() {
                            f.non_negative = !f.non_negative;
                        }
                        cx.notify();
                    })),
            );
        }
        let button = |id: &'static str, label: &'static str| div().id(id).flex().items_center().justify_center().h(px(32.)).px(px(14.)).rounded(px(4.)).cursor_pointer().text_size(px(14.)).child(label);
        div()
            .id("form-scrim")
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
                    .id("form-dialog")
                    .w(px(420.))
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
                    .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).text_color(t.ink).child(title))
                    .child(body)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .child(div().flex_1())
                            .child(button("form-cancel", "Cancel").text_color(t.ink).border_1().border_color(t.divider).hover(|s| s.bg(t.chrome_hover)).on_click(cx.listener(|this, _, _, cx| {
                                this.form = None;
                                this.field = None;
                                cx.notify();
                            })))
                            .child(button("form-run", match form.kind {
                                FormKind::GoalSeek => "Seek",
                                FormKind::Solver => "Solve",
                                FormKind::Link | FormKind::Validation => "Save",
                            }).bg(t.accent).text_color(t.on_accent).font_weight(FontWeight::SEMIBOLD).on_click(cx.listener(|this, _, _, cx| {
                                this.run_form();
                                cx.notify();
                            }))),
                    ),
            )
            .into_any_element()
    }
}
