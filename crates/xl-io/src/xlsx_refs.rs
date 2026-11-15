//! Phase 3 of lossless saving: after rows/columns are inserted or deleted,
//! or sheets renamed, rewrite the cell addresses held by parts xl preserves
//! but doesn't model — validation ranges and formulas, hyperlinks,
//! selections, page breaks, comment and drawing anchors, extension ranges
//! and formulas (sparklines, conditional formats), chart series formulas,
//! pivot cache sources, built-in defined names — so they keep pointing at
//! the same cells.

use std::collections::HashMap;

use quick_xml::Writer;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, BytesText, Event};
use xl_engine::formula::{Shift, parse_formula};
use xl_engine::{CellRef, Range, SheetId, StructOp, Workbook, quote_sheet_name};

use crate::package::Package;
use crate::xlsx_read::{push_entity, read_rels, xml};

/// The edits since the package was read.
pub(crate) struct Edits<'a> {
    pub wb: &'a Workbook,
    pub shifts: Vec<Shift>,
    /// Every structural edit in order (shifts and cut/paste moves).
    pub ops: Vec<StructOp>,
    /// (old name, new name) of renamed sheets.
    pub renames: Vec<(String, String)>,
}

impl<'a> Edits<'a> {
    pub fn since(wb: &'a Workbook, pkg: &Package) -> Result<Edits<'a>, String> {
        let ops: Vec<StructOp> = wb.structure_log().iter().skip(pkg.log_len).copied().collect();
        let shifts = ops.iter().filter_map(|op| if let StructOp::Shift(s) = op { Some(*s) } else { None }).collect();
        let renames = pkg.sheets.iter().filter(|s| wb.sheet_ids().contains(&s.id) && wb.sheet_name(s.id) != s.name).map(|s| (s.name.clone(), wb.sheet_name(s.id).to_string())).collect();
        Ok(Edits { wb, shifts, ops, renames })
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty() && self.renames.is_empty()
    }

    /// Sheets whose cells moved (by shifts or cut/paste).
    fn touched(&self) -> Vec<SheetId> {
        self.ops
            .iter()
            .flat_map(|op| match op {
                StructOp::Shift(s) => vec![s.sheet()],
                StructOp::Move(m) => vec![m.from_sheet, m.to_sheet],
            })
            .collect()
    }

    /// Replace references to renamed sheets in formula text.
    fn renamed(&self, src: &str) -> String {
        let mut out = src.to_string();
        for (old, new) in &self.renames {
            let new_q = quote_sheet_name(new);
            for old_form in [format!("'{}'!", old.replace('\'', "''")), format!("{old}!")] {
                let mut result = String::with_capacity(out.len());
                let mut rest = out.as_str();
                while let Some(i) = rest.find(&old_form) {
                    // Only a whole sheet name: not the tail of a longer one.
                    let before = rest[..i].chars().last();
                    let whole = old_form.starts_with('\'') || !before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
                    result.push_str(&rest[..i]);
                    result.push_str(if whole { &new_q } else { &old_form[..old_form.len() - 1] });
                    result.push('!');
                    rest = &rest[i + old_form.len()..];
                }
                result.push_str(rest);
                out = result;
            }
        }
        out
    }

    /// A formula (no leading '=') on sheet `home`, adjusted for the edits.
    pub fn formula(&self, src: &str, home: SheetId) -> Result<String, String> {
        self.formula_in(src, home, false)
    }

    /// `qualified`: references always name their sheet (charts, defined
    /// names), so the home sheet doesn't matter.
    pub fn formula_in(&self, src: &str, home: SheetId, qualified: bool) -> Result<String, String> {
        let renamed = self.renamed(src);
        if self.ops.is_empty() {
            return Ok(renamed);
        }
        let mut expr = match parse_formula(&renamed, self.wb.sheets()) {
            Ok(e) => e,
            Err(e) => {
                // A formula xl can't parse is safe to leave alone if it can't
                // refer to a shifted sheet: none is named in it, and its
                // unqualified references (if any) are to an unshifted sheet.
                let shifted = self.touched();
                let names_one = shifted.iter().any(|&id| {
                    let name = self.wb.sheet_name(id);
                    renamed.contains(&format!("{name}!")) || renamed.contains(&format!("{}!", quote_sheet_name(name)))
                });
                if !names_one && (qualified || !shifted.contains(&home)) {
                    return Ok(renamed);
                }
                return Err(format!("a formula xl can't read ({src}): {e}"));
            }
        };
        for op in &self.ops {
            expr = match op {
                StructOp::Shift(s) => expr.shifted(*s, home),
                StructOp::Move(m) => expr.moved(m, home),
            };
        }
        Ok(expr.to_source(0, 0, self.wb.sheets()))
    }

    fn on_sheet(&self, sheet: SheetId) -> impl Iterator<Item = &Shift> + '_ {
        self.shifts.iter().filter(move |s| s.sheet() == sheet)
    }

    /// A range on `sheet` after the edits; None if deleted. A range wholly
    /// inside a block moved within the sheet moves with it.
    fn range(&self, r: Range, sheet: SheetId) -> Option<Range> {
        self.ops.iter().try_fold(r, |r, op| match op {
            StructOp::Shift(s) if s.sheet() == sheet => s.map_range(r),
            StructOp::Move(m) if m.from_sheet == sheet && m.to_sheet == sheet && m.from.contains(r.start) && m.from.contains(r.end) => {
                let off = |c: CellRef| CellRef::new((c.row as i64 + m.dr) as u32, (c.col as i64 + m.dc) as u32);
                Some(Range::new(off(r.start), off(r.end)))
            }
            _ => Some(r),
        })
    }

    /// A space-separated range list ("A1:B2 D4", "A:A", "3:5"); None if all
    /// of it was deleted. Unchanged (text and all) when nothing shifted here.
    pub fn sqref(&self, s: &str, sheet: SheetId) -> Option<String> {
        if !self.touched().contains(&sheet) {
            return Some(s.to_string());
        }
        let out: Vec<String> = s
            .split_whitespace()
            .filter_map(|part| {
                let clean = part.replace('$', "");
                let (a, b) = clean.split_once(':').unwrap_or((&clean, &clean));
                if let (Some(a), Some(b)) = (CellRef::parse(a), CellRef::parse(b)) {
                    let r = self.range(Range::new(a, b), sheet)?;
                    return Some(if r.start == r.end { r.start.to_string() } else { format!("{}:{}", r.start, r.end) });
                }
                // Whole columns ("A:C") or whole rows ("3:5").
                if let (Some(c1), Some(c2)) = (xl_engine::parse_col(a), xl_engine::parse_col(b)) {
                    let r = self.range(Range::new(CellRef::new(0, c1), CellRef::new(xl_engine::MAX_ROWS - 1, c2)), sheet)?;
                    return Some(format!("{}:{}", xl_engine::col_name(r.start.col), xl_engine::col_name(r.end.col)));
                }
                if let (Ok(r1), Ok(r2)) = (a.parse::<u32>(), b.parse::<u32>()) {
                    let r = self.range(Range::new(CellRef::new(r1.saturating_sub(1), 0), CellRef::new(r2.saturating_sub(1), xl_engine::MAX_COLS - 1)), sheet)?;
                    return Some(format!("{}:{}", r.start.row + 1, r.end.row + 1));
                }
                // Something else (a name?): leave it.
                Some(part.to_string())
            })
            .collect();
        (!out.is_empty()).then(|| out.join(" "))
    }

    /// A row (or column) index on `sheet` after the edits; deleted lines
    /// land where the lines closed up.
    fn line(&self, i: u32, rows: bool, sheet: SheetId) -> u32 {
        self.on_sheet(sheet).filter(|op| matches!(op, Shift::InsertRows { .. } | Shift::DeleteRows { .. }) == rows).fold(i, |i, op| {
            op.map_pos(i).unwrap_or(match op {
                Shift::DeleteRows { at, .. } | Shift::DeleteCols { at, .. } => *at,
                _ => i,
            })
        })
    }

    fn line_deleted(&self, i: u32, rows: bool, sheet: SheetId) -> bool {
        let mut i = i;
        for op in self.on_sheet(sheet).filter(|op| matches!(op, Shift::InsertRows { .. } | Shift::DeleteRows { .. }) == rows) {
            match op.map_pos(i) {
                Some(n) => i = n,
                None => return true,
            }
        }
        false
    }
}

fn attrs_of(e: &BytesStart) -> Vec<(String, String)> {
    e.attributes()
        .with_checks(false)
        .flatten()
        .map(|a| (a.key.as_ref().to_string(), a.normalized_value(XmlVersion::Implicit1_0).map(|v| v.into_owned()).unwrap_or_default()))
        .collect()
}

fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// What to do with an element's start tag.
enum Action {
    Keep,
    /// Replace attributes (key, new value or None to remove).
    Attrs(Vec<(String, Option<String>)>),
    /// Drop the element and everything inside it.
    Drop,
}

/// Stream a part, rewriting start tags (via `on_start`) and the text of
/// chosen elements (via `on_text`, given the element's local name).
fn rewrite<S, T>(doc: &[u8], mut on_start: S, mut on_text: T) -> Result<Option<Vec<u8>>, String>
where
    S: FnMut(&str, &[(String, String)], &[String]) -> Action,
    T: FnMut(&str, &str, &[String]) -> Option<Result<String, String>>,
{
    let mut r = xml(doc);
    let mut w = Writer::new(Vec::with_capacity(doc.len()));
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut drop_depth = 0usize;
    let mut changed = false;
    // Text being collected for an element `on_text` wants, and its raw events.
    let mut text: Option<(String, Vec<Event<'static>>)> = None;
    loop {
        let ev = r.read_event_into(&mut buf).map_err(|e| e.to_string())?.into_owned();
        if drop_depth > 0 {
            match ev {
                Event::Start(_) => drop_depth += 1,
                Event::End(_) => drop_depth -= 1,
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
            continue;
        }
        match &ev {
            Event::Start(e) | Event::Empty(e) => {
                let name = e.name().as_ref().to_string();
                let attrs = attrs_of(e);
                let empty = matches!(ev, Event::Empty(_));
                match on_start(local(&name), &attrs, &stack) {
                    Action::Keep => w.write_event(ev.clone()).map_err(|e| e.to_string())?,
                    Action::Drop => {
                        changed = true;
                        if !empty {
                            drop_depth = 1;
                        }
                        buf.clear();
                        continue;
                    }
                    Action::Attrs(changes) => {
                        changed = true;
                        let mut start = BytesStart::new(name.clone());
                        for (k, v) in &attrs {
                            match changes.iter().find(|(ck, _)| ck == k) {
                                Some((_, Some(nv))) => start.push_attribute((k.as_str(), nv.as_str())),
                                Some((_, None)) => {}
                                None => start.push_attribute((k.as_str(), v.as_str())),
                            }
                        }
                        w.write_event(if empty { Event::Empty(start) } else { Event::Start(start) }).map_err(|e| e.to_string())?;
                    }
                }
                if !empty {
                    stack.push(local(&name).to_string());
                    if on_text(local(&name), "", &stack).is_some() {
                        text = Some((String::new(), Vec::new()));
                    }
                }
            }
            Event::Text(t) if text.is_some() => {
                let (s, raw) = text.as_mut().unwrap();
                s.push_str(&t.xml_content(XmlVersion::Implicit1_0));
                raw.push(ev.clone());
            }
            Event::GeneralRef(g) if text.is_some() => {
                let (s, raw) = text.as_mut().unwrap();
                push_entity(s, g);
                raw.push(ev.clone());
            }
            Event::End(_) => {
                if let Some((s, raw)) = text.take() {
                    let el = stack.last().cloned().unwrap_or_default();
                    match on_text(&el, &s, &stack) {
                        Some(Ok(new)) if new != s => {
                            changed = true;
                            w.write_event(Event::Text(BytesText::from_escaped(crate::xlsx_patch::esc(&new)))).map_err(|e| e.to_string())?;
                        }
                        Some(Err(e)) => return Err(e),
                        _ => {
                            for e in raw {
                                w.write_event(e).map_err(|e| e.to_string())?;
                            }
                        }
                    }
                }
                stack.pop();
                w.write_event(ev.clone()).map_err(|e| e.to_string())?;
            }
            Event::Eof => break,
            _ => w.write_event(ev.clone()).map_err(|e| e.to_string())?,
        }
        buf.clear();
    }
    Ok(changed.then(|| w.into_inner()))
}

impl Edits<'_> {
    /// A worksheet's preserved elements.
    pub fn sheet_xml(&self, doc: &[u8], sheet: SheetId) -> Result<Option<Vec<u8>>, String> {
        let shifted = self.touched().contains(&sheet);
        let fail: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
        let out = rewrite(
            doc,
            |name, attrs, stack| {
                if stack.iter().any(|s| s == "sheetData") {
                    return Action::Keep;
                }
                let get = |k: &str| attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
                let mut changes = Vec::new();
                match name {
                    "dataValidation" | "protectedRange" | "ignoredError" | "selection" if shifted => {
                        if let Some(sq) = get("sqref") {
                            match self.sqref(sq, sheet) {
                                Some(n) if n != sq => changes.push(("sqref".to_string(), Some(n))),
                                Some(_) => {}
                                None => return Action::Drop,
                            }
                        }
                        if let Some(c) = get("activeCell") {
                            match self.sqref(c, sheet) {
                                Some(n) if n != c => changes.push(("activeCell".to_string(), Some(n))),
                                Some(_) => {}
                                None => changes.push(("activeCell".to_string(), None)),
                            }
                        }
                    }
                    "hyperlink" => {
                        if shifted && let Some(rf) = get("ref") {
                            match self.sqref(rf, sheet) {
                                Some(n) if n != rf => changes.push(("ref".to_string(), Some(n))),
                                Some(_) => {}
                                None => return Action::Drop,
                            }
                        }
                        if let Some(loc) = get("location") {
                            // A place in this workbook ("Sheet2!A1" or a name); leave it if unreadable.
                            if let Ok(n) = self.formula(loc, sheet)
                                && n != loc
                            {
                                changes.push(("location".to_string(), Some(n)));
                            }
                        }
                    }
                    "sheetView" | "pane" if shifted => {
                        if let Some(c) = get("topLeftCell")
                            && let Some(n) = self.sqref(c, sheet)
                            && n != c
                        {
                            changes.push(("topLeftCell".to_string(), Some(n)));
                        }
                    }
                    "brk" if shifted => {
                        let rows = stack.iter().any(|s| s == "rowBreaks");
                        if let Some(id) = get("id").and_then(|v| v.parse::<u32>().ok()) {
                            if self.line_deleted(id, rows, sheet) {
                                return Action::Drop;
                            }
                            let n = self.line(id, rows, sheet);
                            if n != id {
                                changes.push(("id".to_string(), Some(n.to_string())));
                            }
                        }
                    }
                    "cellWatch" if shifted => {
                        if let Some(c) = get("r") {
                            match self.sqref(c, sheet) {
                                Some(n) if n != c => changes.push(("r".to_string(), Some(n))),
                                Some(_) => {}
                                None => return Action::Drop,
                            }
                        }
                    }
                    _ => {}
                }
                if changes.is_empty() { Action::Keep } else { Action::Attrs(changes) }
            },
            |el, text, stack| {
                if stack.iter().any(|s| s == "sheetData") {
                    return None;
                }
                match el {
                    "formula1" | "formula2" | "f" if stack.iter().any(|s| s == "dataValidation" || s == "extLst") => Some(self.formula(text, sheet)),
                    "formula" if stack.iter().any(|s| s == "extLst") => Some(self.formula(text, sheet)),
                    // (Called with empty text first, to ask whether to collect it.)
                    "sqref" if shifted && text.is_empty() => Some(Ok(String::new())),
                    "sqref" if shifted => Some(match self.sqref(text, sheet) {
                        Some(n) => Ok(n),
                        None => {
                            *fail.borrow_mut() = Some("an extension's cells were all deleted".into());
                            Ok(text.to_string())
                        }
                    }),
                    _ => None,
                }
            },
        )?;
        if let Some(e) = fail.into_inner() {
            return Err(e);
        }
        Ok(out)
    }

    /// Comments: their cell references. A comment on a deleted cell can't be
    /// dropped without also editing its VML shape, so that fails.
    pub fn comments_xml(&self, doc: &[u8], sheet: SheetId) -> Result<Option<Vec<u8>>, String> {
        let mut deleted = false;
        let out = rewrite(
            doc,
            |name, attrs, _| {
                if !matches!(name, "comment" | "threadedComment") {
                    return Action::Keep;
                }
                match attrs.iter().find(|(k, _)| k == "ref") {
                    Some((_, r)) => match self.sqref(r, sheet) {
                        Some(n) if n != *r => Action::Attrs(vec![("ref".into(), Some(n))]),
                        Some(_) => Action::Keep,
                        // Its note is rebuilt (with the rest) from the workbook's notes.
                        None => {
                            deleted = true;
                            Action::Drop
                        }
                    },
                    None => Action::Keep,
                }
            },
            |_, _, _| None,
        )?;
        let _ = deleted;
        Ok(out)
    }

    /// Legacy VML (comment boxes): anchors and the row/column of each note.
    /// VML is often not well-formed XML, so this works on the text.
    pub fn vml(&self, doc: &[u8], sheet: SheetId) -> Option<Vec<u8>> {
        let text = String::from_utf8_lossy(doc).to_string();
        let mut out = String::with_capacity(text.len());
        let mut rest = text.as_str();
        let mut changed = false;
        let tags: [(&str, &str); 3] = [("<x:Row>", "</x:Row>"), ("<x:Column>", "</x:Column>"), ("<x:Anchor>", "</x:Anchor>")];
        loop {
            let next = tags.iter().filter_map(|(o, c)| rest.find(o).map(|i| (i, *o, *c))).min_by_key(|t| t.0);
            let Some((i, open, close)) = next else { break };
            let body_start = i + open.len();
            let Some(len) = rest[body_start..].find(close) else { break };
            let body = &rest[body_start..body_start + len];
            let new = match open {
                "<x:Row>" => body.trim().parse::<u32>().ok().map(|v| self.line(v, true, sheet).to_string()),
                "<x:Column>" => body.trim().parse::<u32>().ok().map(|v| self.line(v, false, sheet).to_string()),
                _ => {
                    let nums: Vec<u32> = body.split(',').filter_map(|p| p.trim().parse().ok()).collect();
                    (nums.len() == 8).then(|| {
                        let (c1, r1, c2, r2) = (self.line(nums[0], false, sheet), self.line(nums[2], true, sheet), self.line(nums[4], false, sheet), self.line(nums[6], true, sheet));
                        format!("{c1}, {}, {r1}, {}, {c2}, {}, {r2}, {}", nums[1], nums[3], nums[5], nums[7])
                    })
                }
            };
            out.push_str(&rest[..body_start]);
            match new {
                Some(n) if n != body.trim() => {
                    changed = true;
                    out.push_str(&n);
                }
                _ => out.push_str(body),
            }
            rest = &rest[body_start + len..];
        }
        out.push_str(rest);
        changed.then(|| out.into_bytes())
    }

    /// Drawing anchors (charts, pictures, shapes) on `sheet`.
    pub fn drawing_xml(&self, doc: &[u8], sheet: SheetId) -> Result<Option<Vec<u8>>, String> {
        rewrite(
            doc,
            |_, _, _| Action::Keep,
            |el, text, stack| {
                let in_marker = stack.iter().rev().nth(1).is_some_and(|p| p == "from" || p == "to");
                match el {
                    "row" | "col" if in_marker => Some(Ok(text.trim().parse::<u32>().map_or(text.to_string(), |v| self.line(v, el == "row", sheet).to_string()))),
                    _ => None,
                }
            },
        )
    }

    /// Chart series formulas (they name their sheets).
    pub fn chart_xml(&self, doc: &[u8], home: SheetId) -> Result<Option<Vec<u8>>, String> {
        rewrite(doc, |_, _, _| Action::Keep, |el, text, _| (el == "f").then(|| self.formula_in(text, home, true)))
    }

    /// Pivot cache source: `<worksheetSource ref="A1:D10" sheet="Data"/>`.
    pub fn pivot_cache_xml(&self, doc: &[u8]) -> Result<Option<Vec<u8>>, String> {
        rewrite(
            doc,
            |name, attrs, _| {
                if name != "worksheetSource" {
                    return Action::Keep;
                }
                let get = |k: &str| attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
                let Some(sheet_name) = get("sheet") else { return Action::Keep };
                let mut changes = Vec::new();
                let current = self.renames.iter().find(|(old, _)| *old == sheet_name).map_or(sheet_name.clone(), |(_, new)| new.clone());
                if current != sheet_name {
                    changes.push(("sheet".to_string(), Some(current.clone())));
                }
                if let (Some(id), Some(r)) = (self.wb.sheet_id(&current), get("ref")) {
                    match self.sqref(&r, id) {
                        Some(n) if n != r => changes.push(("ref".into(), Some(n))),
                        _ => {}
                    }
                }
                if changes.is_empty() { Action::Keep } else { Action::Attrs(changes) }
            },
            |_, _, _| None,
        )
    }

    /// Defined names in workbook.xml (built-in ones survive the rewrite).
    pub fn workbook_xml(&self, doc: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let first = self.wb.sheet_ids().first().copied().unwrap_or(0);
        rewrite(doc, |_, _, _| Action::Keep, |el, text, _| (el == "definedName").then(|| self.formula_in(text, first, true)))
    }
}

/// Rewrite every preserved part that holds addresses on the edited sheets.
/// Err if something can't be adjusted safely (then the save writes anew).
pub(crate) fn rewrite_preserved(wb: &Workbook, pkg: &Package) -> Result<HashMap<String, Vec<u8>>, String> {
    let edits = Edits::since(wb, pkg)?;
    let mut out = HashMap::new();
    if edits.is_empty() {
        return Ok(out);
    }
    let mut zip = pkg.zip()?;
    let names: Vec<String> = zip.file_names().flatten().map(|n| n.into_owned()).collect();
    for s in pkg.sheets.iter().filter(|s| wb.sheet_ids().contains(&s.id)) {
        let doc = pkg.part(&s.part).ok_or_else(|| format!("missing {}", s.part))?;
        if let Some(new) = edits.sheet_xml(&doc, s.id)? {
            out.insert(s.part.clone(), new);
        }
        let rels = read_rels(&mut zip, &crate::xlsx_charts::rels_of(&s.part))?;
        for target in rels.values() {
            let part = crate::xlsx_charts::resolve(&s.part, target);
            let Some(doc) = pkg.part(&part) else { continue };
            let file = part.rsplit('/').next().unwrap_or("");
            let new = if file.starts_with("comments") || part.contains("threadedComments/") {
                edits.comments_xml(&doc, s.id)?
            } else if file.ends_with(".vml") {
                edits.vml(&doc, s.id)
            } else if part.contains("drawings/") && file.ends_with(".xml") {
                // Charts in this drawing name their sheets explicitly.
                let drels = read_rels(&mut zip, &crate::xlsx_charts::rels_of(&part))?;
                for t in drels.values().filter(|t| t.contains("charts/")) {
                    let chart = crate::xlsx_charts::resolve(&part, t);
                    if let Some(cdoc) = pkg.part(&chart)
                        && let Some(new) = edits.chart_xml(&cdoc, s.id)?
                    {
                        out.insert(chart, new);
                    }
                }
                edits.drawing_xml(&doc, s.id)?
            } else {
                None
            };
            if let Some(new) = new {
                out.insert(part, new);
            }
        }
    }
    for name in names.iter().filter(|n| n.starts_with("xl/pivotCache/pivotCacheDefinition") && n.ends_with(".xml")) {
        if let Some(doc) = pkg.part(name)
            && let Some(new) = edits.pivot_cache_xml(&doc)?
        {
            out.insert(name.clone(), new);
        }
    }
    if let Some(doc) = pkg.part("xl/workbook.xml")
        && let Some(new) = edits.workbook_xml(&doc)?
    {
        out.insert("xl/workbook.xml".into(), new);
    }
    // Parts that refer to cells in ways not handled above.
    for (prefix, what) in [("xl/slicerCaches/", "slicers"), ("xl/timelineCaches/", "timelines")] {
        if !edits.ops.is_empty() && names.iter().any(|n| n.starts_with(prefix)) {
            return Err(format!("the file has {what}, which xl can't adjust yet"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use xl_engine::Workbook;

    fn edits<'a>(wb: &'a Workbook, shifts: Vec<Shift>, renames: Vec<(&str, &str)>) -> Edits<'a> {
        let ops = shifts.iter().map(|s| StructOp::Shift(*s)).collect();
        Edits { wb, shifts, ops, renames: renames.into_iter().map(|(a, b)| (a.to_string(), b.to_string())).collect() }
    }

    #[test]
    fn rewrites_sheet_parts() {
        let wb = Workbook::new();
        let s = wb.sheet_ids()[0];
        let e = edits(&wb, vec![Shift::InsertRows { sheet: s, at: 1, count: 2 }], vec![]);
        assert_eq!(e.sqref("A1 B2:C4 $D$9", s).as_deref(), Some("A1 B4:C6 D11"));
        assert_eq!(e.formula("SUM(Sheet1!B2:B4)", s).unwrap(), "SUM(Sheet1!B4:B6)");
        let doc = br#"<worksheet><sheetData><row r="2"><c r="B2"/></row></sheetData><dataValidations count="1"><dataValidation type="list" sqref="B2:B5"><formula1>$H$2:$H$9</formula1></dataValidation></dataValidations><hyperlinks><hyperlink ref="A3" location="Sheet1!C2"/></hyperlinks><rowBreaks><brk id="3" max="16383" man="1"/></rowBreaks></worksheet>"#;
        let out = String::from_utf8(e.sheet_xml(doc, s).unwrap().unwrap()).unwrap();
        assert!(out.contains(r#"sqref="B4:B7""#), "{out}");
        assert!(out.contains("<formula1>$H$4:$H$11</formula1>"), "{out}");
        assert!(out.contains(r#"<hyperlink ref="A5" location="Sheet1!C4"/>"#), "{out}");
        assert!(out.contains(r#"<brk id="5""#), "{out}");
        assert!(out.contains(r#"<row r="2"><c r="B2"/></row>"#), "cells are left to the cell writer");
        let del = edits(&wb, vec![Shift::DeleteRows { sheet: s, at: 2, count: 1 }], vec![]);
        let out = String::from_utf8(del.sheet_xml(doc, s).unwrap().unwrap()).unwrap();
        assert!(!out.contains("<hyperlink "), "a link on a deleted row goes: {out}");
    }

    #[test]
    fn renames_sheet_references() {
        let mut wb = Workbook::new();
        let s = wb.sheet_ids()[0];
        wb.rename_sheet(s, "Q1 Sales").unwrap();
        let e = edits(&wb, vec![], vec![("Sheet1", "Q1 Sales")]);
        assert_eq!(e.formula("Sheet1!$B$2:$B$4", s).unwrap(), "'Q1 Sales'!$B$2:$B$4");
        assert_eq!(e.formula("XSheet1!A1", s).unwrap(), "XSheet1!A1", "not part of another name");
        let vml = b"<x:ClientData ObjectType=\"Note\"><x:Anchor>1, 15, 0, 2, 3, 15, 4, 16</x:Anchor><x:Row>1</x:Row><x:Column>0</x:Column></x:ClientData>";
        let shifted = edits(&wb, vec![Shift::InsertRows { sheet: s, at: 0, count: 1 }], vec![]);
        let out = String::from_utf8(shifted.vml(vml, s).unwrap()).unwrap();
        assert!(out.contains("<x:Row>2</x:Row>") && out.contains("<x:Anchor>1, 15, 1, 2, 3, 15, 5, 16</x:Anchor>"), "{out}");
    }
}
