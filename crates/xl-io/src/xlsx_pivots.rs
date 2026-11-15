//! xl's own pivot tables, slicers, timelines and pivot charts in xlsx files.
//!
//! The pivot output is ordinary cells (so other apps see the values); the
//! definitions — fields, calculated fields, grouping, filters, sorting,
//! "show values as", slicer and timeline panels and which charts are pivot
//! charts — go in one custom XML part (`customXml/xlPivots.xml`, related
//! from the workbook part with its properties part, the standard shape for
//! custom XML). Excel and LibreOffice keep custom XML untouched, and xl reads
//! it back on open so the pivots keep working.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::Arc;

use quick_xml::events::Event;
use xl_engine::formula::parse_formula;
use xl_engine::pivot::{Agg, BaseItem, CalcField, DateLevel, FieldFilter, FieldSort, Grouping, LabelFilter, LabelOp, Pivot, ShowAs, TopFilter, ValueField, ValueFilter, ValueOp};
use xl_engine::slicer::{Slicer, SlicerSource, Timeline};
use xl_engine::{CellRef, Range, Workbook, precise};

use crate::xlsx_patch::esc;
use crate::xlsx_read::{attr, local, push_entity, xml};

pub(crate) const PART: &str = "customXml/xlPivots.xml";
pub(crate) const PROPS: &str = "customXml/xlPivotsProps.xml";
pub(crate) const PART_RELS: &str = "customXml/_rels/xlPivots.xml.rels";
/// The workbook's relationship to the part.
pub(crate) const TARGET: &str = "../customXml/xlPivots.xml";
pub(crate) const REL_ID: &str = "rIdXlPivots";
pub(crate) const REL_CUSTOM_XML: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml";
const REL_PROPS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXmlProps";
pub(crate) const CT_PROPS: &str = "application/vnd.openxmlformats-officedocument.customXmlProperties+xml";
pub(crate) const CT_XML: &str = "application/xml";
const NS: &str = "urn:xl-spreadsheet:pivots:1";

/// Is a package part one of ours (so it isn't reported as unknown custom XML)?
pub(crate) fn is_ours(name: &str) -> bool {
    name == PART || name == PROPS || name == PART_RELS
}

fn agg_name(a: Agg) -> &'static str {
    match a {
        Agg::Sum => "sum",
        Agg::Count => "count",
        Agg::Average => "average",
        Agg::Min => "min",
        Agg::Max => "max",
    }
}

fn level_name(l: DateLevel) -> &'static str {
    match l {
        DateLevel::Years => "years",
        DateLevel::Quarters => "quarters",
        DateLevel::Months => "months",
        DateLevel::Days => "days",
    }
}

fn level_of(s: &str) -> Option<DateLevel> {
    DateLevel::ALL.into_iter().find(|l| level_name(*l) == s)
}

fn label_op_name(o: LabelOp) -> &'static str {
    match o {
        LabelOp::Equals => "equals",
        LabelOp::NotEquals => "notEquals",
        LabelOp::BeginsWith => "beginsWith",
        LabelOp::EndsWith => "endsWith",
        LabelOp::Contains => "contains",
        LabelOp::NotContains => "notContains",
    }
}

fn value_op_name(o: ValueOp) -> &'static str {
    match o {
        ValueOp::Greater => "gt",
        ValueOp::GreaterEq => "ge",
        ValueOp::Less => "lt",
        ValueOp::LessEq => "le",
        ValueOp::Equal => "eq",
        ValueOp::NotEqual => "ne",
        ValueOp::Between => "between",
        ValueOp::NotBetween => "notBetween",
    }
}

/// (show, base) attribute values for a "show values as".
fn show_attrs(s: &ShowAs) -> (&'static str, Option<String>) {
    let base = |b: &BaseItem| match b {
        BaseItem::Previous => "previous".to_string(),
        BaseItem::Next => "next".to_string(),
        BaseItem::Item(s) => format!("item:{s}"),
    };
    match s {
        ShowAs::Normal => ("normal", None),
        ShowAs::PctGrandTotal => ("pctGrand", None),
        ShowAs::PctRowTotal => ("pctRow", None),
        ShowAs::PctColTotal => ("pctCol", None),
        ShowAs::DiffFrom(b) => ("diff", Some(base(b))),
        ShowAs::PctDiffFrom(b) => ("pctDiff", Some(base(b))),
        ShowAs::RunningTotal => ("running", None),
        ShowAs::Rank { descending: true } => ("rankDesc", None),
        ShowAs::Rank { descending: false } => ("rankAsc", None),
    }
}

fn show_of(show: &str, base: Option<&str>) -> ShowAs {
    let base = || match base.unwrap_or("previous") {
        "next" => BaseItem::Next,
        b => b.strip_prefix("item:").map_or(BaseItem::Previous, |s| BaseItem::Item(s.to_string())),
    };
    match show {
        "pctGrand" => ShowAs::PctGrandTotal,
        "pctRow" => ShowAs::PctRowTotal,
        "pctCol" => ShowAs::PctColTotal,
        "diff" => ShowAs::DiffFrom(base()),
        "pctDiff" => ShowAs::PctDiffFrom(base()),
        "running" => ShowAs::RunningTotal,
        "rankDesc" => ShowAs::Rank { descending: true },
        "rankAsc" => ShowAs::Rank { descending: false },
        _ => ShowAs::Normal,
    }
}

fn range_text(r: Range) -> String {
    if r.start == r.end { r.start.to_string() } else { format!("{}:{}", r.start, r.end) }
}

fn place_attrs(anchor: CellRef, offset: (f64, f64), size: (f64, f64)) -> String {
    format!(" anchor=\"{anchor}\" dx=\"{}\" dy=\"{}\" w=\"{}\" h=\"{}\"", precise(offset.0), precise(offset.1), precise(size.0), precise(size.1))
}

/// The part's XML, or None when the workbook has nothing to keep.
pub(crate) fn meta_xml(wb: &Workbook) -> Option<String> {
    let mut body = String::new();
    for &id in wb.sheet_ids() {
        let l = wb.layout(id);
        let charts: Vec<(usize, &str)> = l.charts.iter().enumerate().filter_map(|(i, c)| c.pivot.as_deref().map(|p| (i, p))).collect();
        if l.pivots.is_empty() && l.slicers.is_empty() && l.timelines.is_empty() && charts.is_empty() {
            continue;
        }
        let _ = write!(body, "<sheet name=\"{}\">", esc(wb.sheet_name(id)));
        for p in &l.pivots {
            let _ = write!(body, "<pivot name=\"{}\" source=\"{}\" anchor=\"{}\"", esc(&p.name), esc(&p.source.to_source(0, 0, wb.sheets())), p.anchor);
            if let Some(e) = p.extent {
                let _ = write!(body, " extent=\"{}\"", range_text(e));
            }
            if p.subtotals {
                body.push_str(" subtotals=\"1\"");
            }
            body.push('>');
            for f in &p.rows {
                let _ = write!(body, "<row f=\"{f}\"/>");
            }
            for f in &p.cols {
                let _ = write!(body, "<col f=\"{f}\"/>");
            }
            for v in &p.values {
                let (show, base) = show_attrs(&v.show);
                let _ = write!(body, "<value f=\"{}\" agg=\"{}\"", v.field, agg_name(v.agg));
                if show != "normal" {
                    let _ = write!(body, " show=\"{show}\"");
                }
                if let Some(b) = base {
                    let _ = write!(body, " base=\"{}\"", esc(&b));
                }
                body.push_str("/>");
            }
            for c in &p.calculated {
                let _ = write!(body, "<calc name=\"{}\" formula=\"{}\"/>", esc(&c.name), esc(&c.formula));
            }
            for (f, g) in &p.groups {
                match g {
                    Grouping::Dates(levels) => {
                        let names: Vec<&str> = levels.iter().map(|l| level_name(*l)).collect();
                        let _ = write!(body, "<dateGroup f=\"{f}\" levels=\"{}\"/>", names.join(","));
                    }
                    Grouping::Numbers { start, end, by } => {
                        let _ = write!(body, "<rangeGroup f=\"{f}\" start=\"{}\" end=\"{}\" by=\"{}\"/>", precise(*start), precise(*end), precise(*by));
                    }
                    Grouping::Manual(groups) => {
                        let _ = write!(body, "<manualGroup f=\"{f}\">");
                        for (name, items) in groups {
                            let _ = write!(body, "<group name=\"{}\">", esc(name));
                            for i in items {
                                let _ = write!(body, "<item>{}</item>", esc(i));
                            }
                            body.push_str("</group>");
                        }
                        body.push_str("</manualGroup>");
                    }
                }
            }
            for (f, flt) in p.filters.iter().filter(|(_, f)| !f.is_empty()) {
                let _ = write!(body, "<filter f=\"{f}\">");
                for h in &flt.hidden {
                    let _ = write!(body, "<hidden>{}</hidden>", esc(h));
                }
                if let Some(lf) = &flt.label {
                    let _ = write!(body, "<label op=\"{}\" text=\"{}\"/>", label_op_name(lf.op), esc(&lf.text));
                }
                if let Some(vf) = &flt.value {
                    let _ = write!(body, "<valueFilter v=\"{}\" op=\"{}\" a=\"{}\" b=\"{}\"/>", vf.value, value_op_name(vf.op), precise(vf.a), precise(vf.b));
                }
                if let Some(t) = &flt.top {
                    let _ = write!(body, "<top n=\"{}\" bottom=\"{}\" v=\"{}\"/>", t.n, t.bottom as u8, t.value);
                }
                body.push_str("</filter>");
            }
            for (f, s) in &p.sorts {
                match s {
                    FieldSort::Ascending => {}
                    FieldSort::Descending => {
                        let _ = write!(body, "<sort f=\"{f}\" order=\"desc\"/>");
                    }
                    FieldSort::ByValue { value, descending } => {
                        let _ = write!(body, "<sort f=\"{f}\" order=\"value\" v=\"{value}\" desc=\"{}\"/>", *descending as u8);
                    }
                }
            }
            body.push_str("</pivot>");
        }
        for s in &l.slicers {
            let _ = write!(body, "<slicer caption=\"{}\" field=\"{}\"{}", esc(&s.caption), esc(&s.field), place_attrs(s.anchor, s.offset, s.size));
            if let SlicerSource::Table(t) = &s.source {
                let _ = write!(body, " table=\"{}\"", esc(t));
            }
            body.push('>');
            if let SlicerSource::Pivots(names) = &s.source {
                for n in names {
                    let _ = write!(body, "<pivot>{}</pivot>", esc(n));
                }
            }
            for item in &s.selected {
                let _ = write!(body, "<selected>{}</selected>", esc(item));
            }
            body.push_str("</slicer>");
        }
        for t in &l.timelines {
            let _ = write!(body, "<timeline caption=\"{}\" field=\"{}\" level=\"{}\"{}", esc(&t.caption), esc(&t.field), level_name(t.level), place_attrs(t.anchor, t.offset, t.size));
            if let Some((a, b)) = t.range {
                let _ = write!(body, " start=\"{}\" end=\"{}\"", precise(a), precise(b));
            }
            body.push('>');
            for n in &t.pivots {
                let _ = write!(body, "<pivot>{}</pivot>", esc(n));
            }
            body.push_str("</timeline>");
        }
        for (i, name) in charts {
            let _ = write!(body, "<pivotChart index=\"{i}\" pivot=\"{}\"/>", esc(name));
        }
        body.push_str("</sheet>");
    }
    if body.is_empty() {
        return None;
    }
    Some(format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<xlPivots xmlns=\"{NS}\">{body}</xlPivots>"))
}

pub(crate) fn props_xml() -> String {
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\"?>\n<ds:datastoreItem ds:itemID=\"{6D1C3A52-5B8E-4F0B-9C1E-7A9F2C3B4D10}\" xmlns:ds=\"http://schemas.openxmlformats.org/officeDocument/2006/customXml\"><ds:schemaRefs><ds:schemaRef ds:uri=\"urn:xl-spreadsheet:pivots:1\"/></ds:schemaRefs></ds:datastoreItem>".to_string()
}

pub(crate) fn part_rels_xml() -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL_PROPS}\" Target=\"xlPivotsProps.xml\"/></Relationships>")
}

/// Add the part to a freshly written package (a new file, not a patch).
pub(crate) fn add_to_new(bytes: Vec<u8>, wb: &Workbook) -> Result<Vec<u8>, String> {
    use std::io::{Cursor, Write};
    let Some(meta) = meta_xml(wb) else { return Ok(bytes) };
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut out = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for i in 0..zip.len() {
        let name = zip.by_index_raw(i).map_err(|e| e.to_string())?.name().map_err(|e| e.to_string())?.into_owned();
        let patched = match name.as_str() {
            "xl/_rels/workbook.xml.rels" => {
                let doc = crate::xlsx_read::read_entry(&mut zip, &name)?.unwrap_or_default();
                Some(crate::xlsx_patch::patch_rels(&doc, &[(REL_ID.to_string(), REL_CUSTOM_XML, TARGET.to_string())], &[])?)
            }
            "[Content_Types].xml" => {
                let doc = crate::xlsx_read::read_entry(&mut zip, &name)?.unwrap_or_default();
                Some(crate::xlsx_patch::patch_content_types(&doc, &[(format!("/{PART}"), CT_XML), (format!("/{PROPS}"), CT_PROPS)], &[])?)
            }
            _ => None,
        };
        match patched {
            Some(b) => {
                out.start_file(name.as_str(), opts).map_err(|e| e.to_string())?;
                out.write_all(&b).map_err(|e| e.to_string())?;
            }
            None => out.raw_copy_file(zip.by_index_raw(i).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
        }
    }
    for (name, body) in [(PART, meta), (PROPS, props_xml()), (PART_RELS, part_rels_xml())] {
        out.start_file(name, opts).map_err(|e| e.to_string())?;
        out.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(out.finish().map_err(|e| e.to_string())?.into_inner())
}

/// Bring a patched save's part in line with the workbook: written, replaced
/// or removed (with its relationship and content types).
#[allow(clippy::too_many_arguments)]
pub(crate) fn sync_patched(
    wb: &Workbook,
    pkg: &crate::Package,
    replaced: &mut std::collections::HashMap<String, Vec<u8>>,
    added: &mut Vec<(String, Vec<u8>)>,
    removed: &mut BTreeSet<String>,
    rel_adds: &mut Vec<(String, &'static str, String)>,
    rel_removes: &mut Vec<String>,
    ct_adds: &mut Vec<(String, &'static str)>,
    ct_removes: &mut Vec<String>,
) {
    let now = meta_xml(wb);
    let before = pkg.part(PART);
    match (now, before) {
        (None, None) => {}
        (Some(x), Some(b)) => {
            if x.as_bytes() != b.as_slice() {
                replaced.insert(PART.to_string(), x.into_bytes());
            }
        }
        (Some(x), None) => {
            added.push((PART.to_string(), x.into_bytes()));
            if pkg.part(PROPS).is_none() {
                added.push((PROPS.to_string(), props_xml().into_bytes()));
            }
            if pkg.part(PART_RELS).is_none() {
                added.push((PART_RELS.to_string(), part_rels_xml().into_bytes()));
            }
            rel_adds.push((REL_ID.to_string(), REL_CUSTOM_XML, TARGET.to_string()));
            ct_adds.push((format!("/{PART}"), CT_XML));
            ct_adds.push((format!("/{PROPS}"), CT_PROPS));
        }
        (None, Some(_)) => {
            for p in [PART, PROPS, PART_RELS] {
                if pkg.part(p).is_some() {
                    removed.insert(p.to_string());
                }
            }
            rel_removes.push(TARGET.to_string());
            ct_removes.push(format!("/{PART}"));
            ct_removes.push(format!("/{PROPS}"));
        }
    }
}

/// Read the part (if the file has one) into the workbook's layouts.
pub(crate) fn read(bytes: &[u8], wb: &mut Workbook, warnings: &mut Vec<String>) {
    if let Err(e) = read_inner(bytes, wb) {
        warnings.push(format!("xl's pivot table settings couldn't be read ({e}); the pivots are kept as values"));
    }
}

/// Named groups of item labels.
type ManualGroups = Vec<(String, BTreeSet<String>)>;

/// What's being read inside a `<pivot>` or a panel.
#[derive(Default)]
struct Cur {
    pivot: Option<Pivot>,
    slicer: Option<Slicer>,
    timeline: Option<Timeline>,
    filter: Option<(usize, FieldFilter)>,
    manual: Option<(usize, ManualGroups)>,
    /// The element whose text is being collected.
    text_of: Option<String>,
    text: String,
}

fn read_inner(bytes: &[u8], wb: &mut Workbook) -> Result<(), String> {
    let mut r = xml(bytes);
    let mut sheet = None;
    let mut cur = Cur::default();
    let mut charts: Vec<(xl_engine::SheetId, usize, String)> = Vec::new();
    let num = |e: &quick_xml::events::BytesStart, k: &str| attr(e, k).and_then(|v| v.parse::<f64>().ok());
    let idx = |e: &quick_xml::events::BytesStart, k: &str| attr(e, k).and_then(|v| v.parse::<usize>().ok());
    let cell = |e: &quick_xml::events::BytesStart, k: &str| attr(e, k).and_then(|v| CellRef::parse(&v));
    loop {
        let ev = r.read_event().map_err(|e| e.to_string())?;
        let (e, empty) = match &ev {
            Event::Start(e) => (Some(e.clone()), false),
            Event::Empty(e) => (Some(e.clone()), true),
            _ => (None, false),
        };
        if let Some(e) = e {
            let name = local(e.name().as_ref()).to_string();
            match name.as_str() {
                "sheet" => sheet = attr(&e, "name").and_then(|n| wb.sheet_id(&n)),
                "pivot" if cur.slicer.is_none() && cur.timeline.is_none() => {
                    if sheet.is_none() {
                        continue;
                    }
                    let Some(src) = attr(&e, "source") else { continue };
                    let Ok(expr) = parse_formula(&src, wb.sheets()) else { continue };
                    let mut p = Pivot::new(Arc::new(expr), cell(&e, "anchor").unwrap_or(CellRef::new(0, 0)));
                    p.name = attr(&e, "name").unwrap_or_default();
                    p.extent = attr(&e, "extent").and_then(|x| crate::xlsx_read::parse_range(&x));
                    p.subtotals = attr(&e, "subtotals").is_some_and(|v| v == "1");
                    cur.pivot = Some(p);
                }
                "row" | "col" => {
                    if let (Some(p), Some(f)) = (cur.pivot.as_mut(), idx(&e, "f")) {
                        if name == "row" { p.rows.push(f) } else { p.cols.push(f) }
                    }
                }
                "value" => {
                    if let (Some(p), Some(f)) = (cur.pivot.as_mut(), idx(&e, "f")) {
                        let agg = attr(&e, "agg").and_then(|a| Agg::ALL.into_iter().find(|x| agg_name(*x) == a)).unwrap_or(Agg::Sum);
                        let show = show_of(attr(&e, "show").as_deref().unwrap_or("normal"), attr(&e, "base").as_deref());
                        p.values.push(ValueField { field: f, agg, show });
                    }
                }
                "calc" => {
                    if let Some(p) = cur.pivot.as_mut() {
                        p.calculated.push(CalcField { name: attr(&e, "name").unwrap_or_default(), formula: attr(&e, "formula").unwrap_or_default() });
                    }
                }
                "dateGroup" => {
                    if let (Some(p), Some(f)) = (cur.pivot.as_mut(), idx(&e, "f")) {
                        let levels = attr(&e, "levels").unwrap_or_default().split(',').filter_map(level_of).collect();
                        p.groups.insert(f, Grouping::Dates(levels));
                    }
                }
                "rangeGroup" => {
                    if let (Some(p), Some(f), Some(start), Some(end), Some(by)) = (cur.pivot.as_mut(), idx(&e, "f"), num(&e, "start"), num(&e, "end"), num(&e, "by")) {
                        p.groups.insert(f, Grouping::Numbers { start, end, by });
                    }
                }
                "manualGroup" => cur.manual = idx(&e, "f").map(|f| (f, Vec::new())),
                "group" => {
                    if let Some((_, groups)) = cur.manual.as_mut() {
                        groups.push((attr(&e, "name").unwrap_or_default(), BTreeSet::new()));
                    }
                }
                "filter" => cur.filter = idx(&e, "f").map(|f| (f, FieldFilter::default())),
                "label" => {
                    if let Some((_, flt)) = cur.filter.as_mut() {
                        let op = attr(&e, "op").and_then(|o| LabelOp::ALL.into_iter().find(|x| label_op_name(*x) == o)).unwrap_or(LabelOp::Contains);
                        flt.label = Some(LabelFilter { op, text: attr(&e, "text").unwrap_or_default() });
                    }
                }
                "valueFilter" => {
                    if let (Some((_, flt)), Some(v)) = (cur.filter.as_mut(), idx(&e, "v")) {
                        let op = attr(&e, "op").and_then(|o| ValueOp::ALL.into_iter().find(|x| value_op_name(*x) == o)).unwrap_or(ValueOp::Greater);
                        flt.value = Some(ValueFilter { value: v, op, a: num(&e, "a").unwrap_or(0.0), b: num(&e, "b").unwrap_or(0.0) });
                    }
                }
                "top" => {
                    if let (Some((_, flt)), Some(n), Some(v)) = (cur.filter.as_mut(), idx(&e, "n"), idx(&e, "v")) {
                        flt.top = Some(TopFilter { n, bottom: attr(&e, "bottom").is_some_and(|b| b == "1"), value: v });
                    }
                }
                "sort" => {
                    if let (Some(p), Some(f)) = (cur.pivot.as_mut(), idx(&e, "f")) {
                        let s = match attr(&e, "order").as_deref() {
                            Some("desc") => FieldSort::Descending,
                            Some("value") => FieldSort::ByValue { value: idx(&e, "v").unwrap_or(0), descending: attr(&e, "desc").is_some_and(|d| d == "1") },
                            _ => FieldSort::Ascending,
                        };
                        p.sorts.insert(f, s);
                    }
                }
                "slicer" | "timeline" => {
                    let place = (cell(&e, "anchor").unwrap_or(CellRef::new(0, 0)), (num(&e, "dx").unwrap_or(0.0), num(&e, "dy").unwrap_or(0.0)), (num(&e, "w").unwrap_or(180.0), num(&e, "h").unwrap_or(220.0)));
                    let (caption, field) = (attr(&e, "caption").unwrap_or_default(), attr(&e, "field").unwrap_or_default());
                    if name == "slicer" {
                        let source = attr(&e, "table").map_or(SlicerSource::Pivots(Vec::new()), SlicerSource::Table);
                        cur.slicer = Some(Slicer { caption, field, source, selected: BTreeSet::new(), anchor: place.0, offset: place.1, size: place.2 });
                    } else {
                        let range = num(&e, "start").zip(num(&e, "end"));
                        let level = attr(&e, "level").as_deref().and_then(level_of).unwrap_or(DateLevel::Months);
                        cur.timeline = Some(Timeline { caption, field, pivots: Vec::new(), level, range, anchor: place.0, offset: place.1, size: place.2 });
                    }
                }
                "pivotChart" => {
                    if let (Some(id), Some(i), Some(p)) = (sheet, idx(&e, "index"), attr(&e, "pivot")) {
                        charts.push((id, i, p));
                    }
                }
                _ => {}
            }
            let in_panel = cur.slicer.is_some() || cur.timeline.is_some();
            if !empty && (matches!(name.as_str(), "item" | "hidden" | "selected") || (name == "pivot" && in_panel)) {
                cur.text_of = Some(name.clone());
                cur.text.clear();
            }
            if !empty {
                continue;
            }
        }
        match ev {
            Event::Text(t) => {
                if cur.text_of.is_some() {
                    cur.text.push_str(&t.xml_content(quick_xml::XmlVersion::Implicit1_0));
                }
            }
            Event::GeneralRef(rf) => {
                if cur.text_of.is_some() {
                    push_entity(&mut cur.text, &rf);
                }
            }
            Event::End(e) => {
                let name = local(e.name().as_ref()).to_string();
                if cur.text_of.as_deref() == Some(name.as_str()) {
                    let text = std::mem::take(&mut cur.text);
                    cur.text_of = None;
                    match name.as_str() {
                        "item" => {
                            if let Some((_, groups)) = cur.manual.as_mut()
                                && let Some(g) = groups.last_mut()
                            {
                                g.1.insert(text);
                            }
                        }
                        "hidden" => {
                            if let Some((_, f)) = cur.filter.as_mut() {
                                f.hidden.insert(text);
                            }
                        }
                        "selected" => {
                            if let Some(s) = cur.slicer.as_mut() {
                                s.selected.insert(text);
                            }
                        }
                        _ => {
                            if let Some(s) = cur.slicer.as_mut() {
                                if let SlicerSource::Pivots(p) = &mut s.source {
                                    p.push(text);
                                }
                            } else if let Some(t) = cur.timeline.as_mut() {
                                t.pivots.push(text);
                            }
                        }
                    }
                    if name != "pivot" || cur.slicer.is_some() || cur.timeline.is_some() {
                        continue;
                    }
                }
                match name.as_str() {
                    "pivot" => {
                        if let (Some(p), Some(id)) = (cur.pivot.take(), sheet)
                            && let Some(l) = wb.layout_mut(id)
                        {
                            l.pivots.push(p);
                        }
                    }
                    "filter" => {
                        if let (Some((f, flt)), Some(p)) = (cur.filter.take(), cur.pivot.as_mut()) {
                            p.filters.insert(f, flt);
                        }
                    }
                    "manualGroup" => {
                        if let (Some((f, g)), Some(p)) = (cur.manual.take(), cur.pivot.as_mut()) {
                            p.groups.insert(f, Grouping::Manual(g));
                        }
                    }
                    "slicer" => {
                        if let (Some(s), Some(id)) = (cur.slicer.take(), sheet)
                            && let Some(l) = wb.layout_mut(id)
                        {
                            l.slicers.push(s);
                        }
                    }
                    "timeline" => {
                        if let (Some(t), Some(id)) = (cur.timeline.take(), sheet)
                            && let Some(l) = wb.layout_mut(id)
                        {
                            l.timelines.push(t);
                        }
                    }
                    "sheet" => sheet = None,
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    // Pivot charts: the chart at the index if it plots the pivot's output,
    // else the first chart on the sheet that does.
    for (id, i, name) in charts {
        let Some((psheet, pi)) = wb.find_pivot(&name) else { continue };
        let extent = wb.layout(psheet).pivots[pi].extent;
        let plots = |c: &xl_engine::chart::Chart| {
            c.series.first().and_then(|s| s.values.reference()).is_some_and(|(s, r)| s.unwrap_or(id) == psheet && extent.is_some_and(|e| e.contains(r.start) && e.contains(r.end)))
        };
        let charts = &wb.layout(id).charts;
        let found = if charts.get(i).is_some_and(|c| c.pivot.is_none() && plots(c)) { Some(i) } else { charts.iter().position(|c| c.pivot.is_none() && plots(c)) };
        if let (Some(j), Some(l)) = (found, wb.layout_mut(id)) {
            l.charts[j].pivot = Some(name);
        }
    }
    Ok(())
}
