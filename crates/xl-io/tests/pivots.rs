//! xl's pivot tables, slicers, timelines and pivot charts survive saving and
//! reopening, in new files and in patched ones.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xl_engine::chart::{Chart, ChartKind, ChartSeries};
use xl_engine::formula::parse_formula;
use xl_engine::pivot::{Agg, BaseItem, CalcField, DateLevel, FieldFilter, FieldSort, Grouping, LabelFilter, LabelOp, Pivot, ShowAs, TopFilter, ValueField, ValueFilter, ValueOp};
use xl_engine::slicer::{Slicer, SlicerSource, Timeline};
use xl_engine::{Addr, CellRef, Range, SheetId, Workbook};
use xl_io::SavePlan;

fn tmp(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("xl-pivot-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn parts(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut out = BTreeMap::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        let mut b = Vec::new();
        f.read_to_end(&mut b).unwrap();
        out.insert(f.name().unwrap().to_string(), b);
    }
    out
}

/// Data on one sheet, a pivot of it (with every kind of option) on another,
/// written out as its values, plus a slicer, a timeline and a pivot chart.
fn book() -> (Workbook, SheetId, SheetId) {
    let mut wb = Workbook::new();
    let data = wb.sheet_ids()[0];
    wb.rename_sheet(data, "Sales & Co").unwrap();
    let rows = [["Date", "Fruit", "Amount", "Cost"], ["2024-01-05", "Pear", "10", "4"], ["2024-02-20", "Apple", "20", "5"], ["2024-05-14", "Plum <3>", "30", "9"], ["2025-01-02", "Apple", "50", "20"]];
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            wb.set_input(Addr::new(data, CellRef::new(r as u32, c as u32)), v).unwrap();
        }
    }
    wb.add_table_for_test(data);
    let out = wb.add_sheet(Some("Pivot 1")).unwrap();
    let source = Arc::new(parse_formula("'Sales & Co'!$A$1:$D$5", wb.sheets()).unwrap());
    let mut p = Pivot::new(source, CellRef::new(0, 0));
    p.name = "PivotTable1".into();
    p.rows = vec![0, 1];
    p.values = vec![ValueField::new(2, Agg::Sum), ValueField { field: 4, agg: Agg::Sum, show: ShowAs::DiffFrom(BaseItem::Item("Apple & co".into())) }, ValueField { field: 3, agg: Agg::Max, show: ShowAs::PctGrandTotal }];
    p.calculated.push(CalcField { name: "Margin".into(), formula: "=Amount-'Cost'".into() });
    p.groups.insert(0, Grouping::Dates(vec![DateLevel::Years, DateLevel::Quarters]));
    p.groups.insert(1, Grouping::Manual(vec![("Pomes".into(), ["Apple".to_string(), "Pear".to_string()].into())]));
    p.groups.insert(3, Grouping::Numbers { start: 0.0, end: 20.0, by: 5.0 });
    p.filters.insert(1, FieldFilter { hidden: ["Kiwi".to_string()].into(), label: Some(LabelFilter { op: LabelOp::NotContains, text: "x<y".into() }), value: Some(ValueFilter { value: 0, op: ValueOp::Between, a: 1.0, b: 1000.5 }), top: Some(TopFilter { n: 10, bottom: false, value: 0 }) });
    p.sorts.insert(1, FieldSort::ByValue { value: 0, descending: true });
    p.sorts.insert(0, FieldSort::Descending);
    p.subtotals = true;
    let grid = wb.pivot_grid(out, &p);
    let mut items = Vec::new();
    for (r, line) in grid.iter().enumerate() {
        for (c, v) in line.iter().enumerate() {
            items.push((Addr::new(out, CellRef::new(r as u32, c as u32)), v.display()));
        }
    }
    let extent = Range::new(CellRef::new(0, 0), CellRef::new(grid.len() as u32 - 1, grid[0].len() as u32 - 1));
    p.extent = Some(extent);
    wb.set_inputs(items);
    let l = wb.layout_mut(out).unwrap();
    l.pivots.push(p);
    l.slicers.push(Slicer { caption: "Fruit".into(), field: "Fruit".into(), source: SlicerSource::Pivots(vec!["PivotTable1".into()]), selected: ["Apple".to_string(), "Plum <3>".to_string()].into(), anchor: CellRef::new(1, 8), offset: (4.5, 2.0), size: (180.0, 230.0) });
    l.slicers.push(Slicer { caption: "Fruit (table)".into(), field: "Fruit".into(), source: SlicerSource::Table("Sales".into()), selected: Default::default(), anchor: CellRef::new(14, 8), offset: (0.0, 0.0), size: (180.0, 200.0) });
    l.timelines.push(Timeline { caption: "Date".into(), field: "Date".into(), pivots: vec!["PivotTable1".into()], level: DateLevel::Quarters, range: Some((45292.0, 45383.0)), anchor: CellRef::new(1, 12), offset: (0.0, 0.0), size: (360.0, 120.0) });
    let e = |s: &str| Arc::new(parse_formula(s, wb.sheets()).unwrap());
    let series = vec![ChartSeries::new(Some(e("'Pivot 1'!$C$1")), Some(e("'Pivot 1'!$B$2:$B$3")), e("'Pivot 1'!$C$2:$C$3"))];
    let plain = Chart { title: Some("Not a pivot chart".into()), ..Chart::new(ChartKind::Line, vec![ChartSeries::new(None, None, e("'Sales & Co'!$C$2:$C$5"))], CellRef::new(20, 0), (300.0, 200.0)) };
    let chart = Chart { title: Some("By fruit".into()), pivot: Some("PivotTable1".into()), ..Chart::new(ChartKind::Column, series, CellRef::new(20, 6), (300.0, 200.0)) };
    let l = wb.layout_mut(out).unwrap();
    l.charts.push(plain);
    l.charts.push(chart);
    (wb, data, out)
}

trait TableForTest {
    fn add_table_for_test(&mut self, sheet: SheetId);
}

impl TableForTest for Workbook {
    fn add_table_for_test(&mut self, sheet: SheetId) {
        let range = Range::new(CellRef::new(0, 0), CellRef::new(4, 3));
        self.layout_mut(sheet).unwrap().tables.push(xl_engine::Table { name: "Sales".into(), range, totals: false, style: Default::default(), banded: true });
    }
}

fn layout_of(wb: &Workbook, name: &str) -> xl_engine::SheetLayout {
    wb.layout(wb.sheet_id(name).unwrap()).clone()
}

#[test]
fn pivots_survive_a_new_file_and_patched_saves() {
    let (wb, _, out) = book();
    let path = tmp("pivots.xlsx");
    let saved = xl_io::save(&wb, &path, out, None).unwrap();
    let p = parts(&path);
    assert!(p.contains_key("customXml/xlPivots.xml") && p.contains_key("customXml/xlPivotsProps.xml") && p.contains_key("customXml/_rels/xlPivots.xml.rels"));
    assert!(String::from_utf8_lossy(&p["xl/_rels/workbook.xml.rels"]).contains("../customXml/xlPivots.xml"));
    assert!(String::from_utf8_lossy(&p["[Content_Types].xml"]).contains("/customXml/xlPivotsProps.xml"));

    let loaded = xl_io::open(&path).unwrap();
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    assert!(!loaded.unsupported.contains(&"custom XML"), "our own part isn't foreign custom XML");
    let before = layout_of(&wb, "Pivot 1");
    let after = layout_of(&loaded.workbook, "Pivot 1");
    assert_eq!(after.pivots, before.pivots);
    assert_eq!(after.slicers, before.slicers);
    assert_eq!(after.timelines, before.timelines);
    assert_eq!(after.charts.iter().map(|c| c.pivot.clone()).collect::<Vec<_>>(), vec![None, Some("PivotTable1".to_string())]);
    // The pivot computes the same from the reopened workbook.
    assert_eq!(loaded.workbook.pivot_grid(out, &after.pivots[0]), wb.pivot_grid(out, &before.pivots[0]));

    // A patched save after changing the pivot rewrites just our part.
    let mut wb2 = loaded.workbook;
    let pkg = loaded.package.or(saved).expect("package");
    wb2.layout_mut(out).unwrap().pivots[0].subtotals = false;
    wb2.layout_mut(out).unwrap().slicers[0].selected.clear();
    assert_eq!(xl_io::save_plan(&wb2, &pkg), SavePlan::Patch);
    let path2 = tmp("pivots-patched.xlsx");
    let pkg2 = xl_io::save(&wb2, &path2, out, Some(&pkg)).unwrap().expect("package");
    let reopened = xl_io::open(&path2).unwrap();
    let l = layout_of(&reopened.workbook, "Pivot 1");
    assert!(!l.pivots[0].subtotals);
    assert!(l.slicers[0].selected.is_empty());
    assert_eq!(l.timelines, before.timelines);
    let (p1, p2) = (parts(&path), parts(&path2));
    let changed: Vec<&String> = p2.keys().filter(|k| p1.get(*k) != p2.get(*k)).collect();
    assert_eq!(changed, vec!["customXml/xlPivots.xml"], "only our part changed");

    // Removing every pivot and panel drops the part, its relationship and types.
    let mut wb3 = reopened.workbook;
    let l = wb3.layout_mut(out).unwrap();
    l.pivots.clear();
    l.slicers.clear();
    l.timelines.clear();
    for c in &mut l.charts {
        c.pivot = None;
    }
    let path3 = tmp("pivots-removed.xlsx");
    xl_io::save(&wb3, &path3, out, Some(&pkg2)).unwrap();
    let p3 = parts(&path3);
    assert!(!p3.keys().any(|k| k.starts_with("customXml/")));
    assert!(!String::from_utf8_lossy(&p3["xl/_rels/workbook.xml.rels"]).contains("customXml"));
    assert!(!String::from_utf8_lossy(&p3["[Content_Types].xml"]).contains("customXml"));
    let empty = xl_io::open(&path3).unwrap();
    assert!(layout_of(&empty.workbook, "Pivot 1").pivots.is_empty());
}

#[test]
fn pivots_added_to_an_opened_file_are_patched_in() {
    let (wb, data, out) = book();
    // A file without pivots first.
    let (mut plain, _, _) = book();
    let l = plain.layout_mut(out).unwrap();
    l.pivots.clear();
    l.slicers.clear();
    l.timelines.clear();
    l.charts.clear();
    let path = tmp("plain.xlsx");
    xl_io::save(&plain, &path, data, None).unwrap();
    assert!(!parts(&path).keys().any(|k| k.starts_with("customXml/")));
    let loaded = xl_io::open(&path).unwrap();
    let pkg = loaded.package.expect("package");
    let mut wb2 = loaded.workbook;
    let l = wb2.layout_mut(out).unwrap();
    l.pivots = wb.layout(out).pivots.clone();
    l.timelines = wb.layout(out).timelines.clone();
    let path2 = tmp("plain-plus-pivot.xlsx");
    xl_io::save(&wb2, &path2, data, Some(&pkg)).unwrap();
    let back = xl_io::open(&path2).unwrap();
    assert!(back.warnings.is_empty(), "{:?}", back.warnings);
    let l = layout_of(&back.workbook, "Pivot 1");
    assert_eq!(l.pivots, wb.layout(out).pivots);
    assert_eq!(l.timelines, wb.layout(out).timelines);
}
