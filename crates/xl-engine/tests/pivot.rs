//! Pivot table summaries.

use std::sync::Arc;

use xl_engine::formula::parse_formula;
use xl_engine::pivot::{Agg, BaseItem, CalcField, DateLevel, FieldFilter, FieldKind, FieldSort, Grouping, LabelFilter, LabelOp, Pivot, ShowAs, TopFilter, ValueField, ValueFilter, ValueOp};
use xl_engine::slicer::{PageFilter, Slicer, SlicerSource, Timeline};
use xl_engine::{Addr, CellRef, Value, Workbook};

fn data() -> (Workbook, u32) {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let rows = [
        ["Region", "Rep", "Product", "Units", "Revenue"],
        ["West", "Ann", "Pens", "10", "15"],
        ["East", "Bo", "Pens", "4", "6"],
        ["West", "Cy", "Pads", "3", "9"],
        ["East", "Bo", "Pads", "", "12"],
        ["West", "Ann", "Pads", "5", "15"],
    ];
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            if !v.is_empty() {
                wb.set_input(Addr::new(s, CellRef::new(r as u32, c as u32)), v).unwrap();
            }
        }
    }
    (wb, s)
}

fn pivot(wb: &Workbook, rows: Vec<usize>, cols: Vec<usize>, values: Vec<(usize, Agg)>) -> Pivot {
    Pivot { rows, cols, values: values.into_iter().map(ValueField::from).collect(), ..Pivot::new(Arc::new(parse_formula("Sheet1!$A$1:$E$6", wb.sheets()).unwrap()), CellRef::new(0, 0)) }
}

fn t(s: &str) -> Value {
    Value::text(s)
}

fn n(x: f64) -> Value {
    Value::Number(x)
}

#[test]
fn rows_and_values() {
    let (wb, s) = data();
    let p = pivot(&wb, vec![0], vec![], vec![(4, Agg::Sum), (3, Agg::Count)]);
    let g = wb.pivot_grid(s, &p);
    assert_eq!(g[0], vec![t("Region"), t("Sum of Revenue"), t("Count of Units")]);
    assert_eq!(g[1], vec![t("East"), n(18.0), n(1.0)], "blank units don't count");
    assert_eq!(g[2], vec![t("West"), n(39.0), n(3.0)]);
    assert_eq!(g[3], vec![t("Grand Total"), n(57.0), n(4.0)]);
    assert_eq!(wb.pivot_fields(s, &p)[2], "Product");
}

#[test]
fn rows_by_columns_with_averages() {
    let (wb, s) = data();
    let p = pivot(&wb, vec![0, 1], vec![2], vec![(4, Agg::Average)]);
    let g = wb.pivot_grid(s, &p);
    assert_eq!(g[0], vec![t("Region"), t("Rep"), t("Pads"), t("Pens"), t("Total Average of Revenue")]);
    assert_eq!(g[1], vec![t("East"), t("Bo"), n(12.0), n(6.0), n(9.0)]);
    assert_eq!(g[2], vec![t("West"), t("Ann"), n(15.0), n(15.0), n(15.0)]);
    assert_eq!(g[3], vec![t("West"), t("Cy"), n(9.0), Value::Empty, n(9.0)], "no pens for Cy");
    assert_eq!(g.last().unwrap(), &vec![t("Grand Total"), Value::Empty, n(12.0), n(10.5), n(57.0 / 5.0)]);
}

#[test]
fn numbers_sort_numerically_and_min_max() {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let rows = [["Size", "Price"], ["10", "3"], ["9", "5"], ["-2", "1"], ["10", "7"]];
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            wb.set_input(Addr::new(s, CellRef::new(r as u32, c as u32)), v).unwrap();
        }
    }
    let p = Pivot { rows: vec![0], values: vec![(1, Agg::Max).into(), (1, Agg::Min).into()], ..Pivot::new(Arc::new(parse_formula("A1:B5", wb.sheets()).unwrap()), CellRef::new(0, 3)) };
    let g = wb.pivot_grid(s, &p);
    let keys: Vec<Value> = g[1..4].iter().map(|r| r[0].clone()).collect();
    assert_eq!(keys, vec![t("-2"), t("9"), t("10")]);
    assert_eq!(g[3], vec![t("10"), n(7.0), n(3.0)]);
}

#[test]
fn calculated_fields_use_sums() {
    let (wb, s) = data();
    let mut p = pivot(&wb, vec![0], vec![], vec![(4, Agg::Sum), (3, Agg::Sum)]);
    // Revenue per unit, on the group's sums (not averaged per record).
    p.calculated.push(CalcField { name: "Per unit".into(), formula: "=Revenue/Units".into() });
    p.calculated.push(CalcField { name: "Bonus".into(), formula: "= Revenue * 10% - 'Units'".into() });
    p.calculated.push(CalcField { name: "Bad".into(), formula: "=Nope+1".into() });
    p.values.push(ValueField::new(5, Agg::Sum));
    p.values.push(ValueField::new(6, Agg::Count));
    p.values.push(ValueField::new(7, Agg::Sum));
    let g = wb.pivot_grid(s, &p);
    assert_eq!(g[0][3], t("Sum of Per unit"));
    assert_eq!(g[0][4], t("Sum of Bonus"));
    assert_eq!(g[1][..5], [t("East"), n(18.0), n(4.0), n(4.5), n(18.0 * 0.1 - 4.0)]);
    assert_eq!(g[2][3], n(39.0 / 18.0));
    assert_eq!(g[3][3], n(57.0 / 22.0), "the grand total divides the grand sums");
    assert_eq!(g[1][5], Value::Error(xl_engine::ErrorKind::Name), "unknown names are #NAME?");
    assert_eq!(wb.pivot_fields(s, &p)[5], "Per unit");
}

#[test]
fn show_values_as() {
    let (wb, s) = data();
    let mut p = pivot(&wb, vec![0], vec![2], vec![(4, Agg::Sum)]);
    // Columns: Pads, Pens, Total. East: 12, 6 → 18; West: 24, 15 → 39; total 36, 21 → 57.
    p.values[0].show = ShowAs::PctGrandTotal;
    let g = wb.pivot_grid(s, &p);
    assert_eq!(g[1][1..], [n(12.0 / 57.0), n(6.0 / 57.0), n(18.0 / 57.0)]);
    assert_eq!(g[3][3], n(1.0));
    p.values[0].show = ShowAs::PctRowTotal;
    assert_eq!(wb.pivot_grid(s, &p)[2][1..], [n(24.0 / 39.0), n(15.0 / 39.0), n(1.0)]);
    p.values[0].show = ShowAs::PctColTotal;
    let out = wb.pivot_table(s, &p);
    assert_eq!(out.grid[1][1..], [n(12.0 / 36.0), n(6.0 / 21.0), n(18.0 / 57.0)]);
    assert_eq!(out.col_formats[1], Some("0.00%"));
    // Along the rows: Ann 30, Bo 18, Cy 9.
    let mut p = pivot(&wb, vec![1], vec![], vec![(4, Agg::Sum)]);
    let col = |p: &Pivot| wb.pivot_grid(s, p)[1..=3].iter().map(|r| r[1].clone()).collect::<Vec<_>>();
    p.values[0].show = ShowAs::DiffFrom(BaseItem::Previous);
    assert_eq!(col(&p), vec![Value::Empty, n(-12.0), n(-9.0)]);
    p.values[0].show = ShowAs::DiffFrom(BaseItem::Item("bo".into()));
    assert_eq!(col(&p), vec![n(12.0), Value::Empty, n(-9.0)]);
    p.values[0].show = ShowAs::PctDiffFrom(BaseItem::Next);
    assert_eq!(col(&p), vec![n(12.0 / 18.0), n(1.0), Value::Empty]);
    p.values[0].show = ShowAs::RunningTotal;
    assert_eq!(col(&p), vec![n(30.0), n(48.0), n(57.0)]);
    assert_eq!(wb.pivot_grid(s, &p)[4][1], Value::Empty, "no running total on the grand total");
    p.values[0].show = ShowAs::Rank { descending: true };
    assert_eq!(col(&p), vec![n(1.0), n(2.0), n(3.0)]);
    p.values[0].show = ShowAs::Rank { descending: false };
    assert_eq!(col(&p), vec![n(3.0), n(2.0), n(1.0)]);
}

/// Sales on dates (formatted as dates) with amounts.
fn dated() -> (Workbook, u32) {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let rows = [
        ["Date", "Amount", "Fruit"],
        ["2023-11-20", "5", "Apple"],
        ["2024-01-05", "10", "Pear"],
        ["2024-01-20", "20", "Apple"],
        ["2024-02-14", "30", "Plum"],
        ["2024-05-01", "40", "Kiwi"],
        ["2025-01-02", "50", "Apple"],
    ];
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            wb.set_input(Addr::new(s, CellRef::new(r as u32, c as u32)), v).unwrap();
        }
    }
    (wb, s)
}

fn dated_pivot(wb: &Workbook) -> Pivot {
    let mut p = Pivot { rows: vec![0], values: vec![ValueField::new(1, Agg::Sum)], ..Pivot::new(Arc::new(parse_formula("Sheet1!$A$1:$C$7", wb.sheets()).unwrap()), CellRef::new(0, 5)) };
    p.name = "Sales".into();
    p
}

fn labels(g: &[Vec<Value>], col: usize) -> Vec<String> {
    g.iter().map(|r| r[col].display()).collect()
}

#[test]
fn date_grouping_by_levels() {
    let (wb, s) = dated();
    let mut p = dated_pivot(&wb);
    assert_eq!(wb.pivot_field_kind(s, &p, 0), FieldKind::Date);
    assert_eq!(wb.pivot_field_kind(s, &p, 1), FieldKind::Number);
    assert_eq!(wb.pivot_field_kind(s, &p, 2), FieldKind::Text);
    p.groups.insert(0, Grouping::Dates(vec![DateLevel::Years]));
    let g = wb.pivot_grid(s, &p);
    assert_eq!(labels(&g, 0), ["Date", "2023", "2024", "2025", "Grand Total"]);
    assert_eq!(labels(&g, 1), ["Sum of Amount", "5", "100", "50", "155"]);
    // Years and quarters: two row levels, with subtotals.
    p.groups.insert(0, Grouping::Dates(vec![DateLevel::Quarters, DateLevel::Years]));
    p.subtotals = true;
    let g = wb.pivot_grid(s, &p);
    assert_eq!(labels(&g, 0), ["Years", "2023", "2023 Total", "2024", "2024", "2024 Total", "2025", "2025 Total", "Grand Total"]);
    assert_eq!(labels(&g, 1), ["Date", "Qtr4", "", "Qtr1", "Qtr2", "", "Qtr1", "", ""]);
    assert_eq!(labels(&g, 2), ["Sum of Amount", "5", "5", "60", "40", "100", "50", "50", "155"]);
    let out = wb.pivot_table(s, &p);
    assert_eq!(out.bold_rows, vec![0, 2, 5, 7, 8]);
    // Months alone merge the years; days show day-month.
    p.groups.insert(0, Grouping::Dates(vec![DateLevel::Months]));
    let g = wb.pivot_grid(s, &p);
    assert_eq!(labels(&g, 0), ["Date", "Jan", "Feb", "May", "Nov", "Grand Total"]);
    assert_eq!(labels(&g, 1), ["Sum of Amount", "80", "30", "40", "5", "155"]);
    p.groups.insert(0, Grouping::Dates(vec![DateLevel::Days]));
    assert_eq!(labels(&wb.pivot_grid(s, &p), 0)[1..3], ["2-Jan", "5-Jan"]);
    assert_eq!(wb.pivot_field_items(s, &p, 0)[0].1[0], "2-Jan");
    // Ungrouped: each date.
    p.groups.clear();
    assert_eq!(wb.pivot_grid(s, &p).len(), 8);
}

#[test]
fn number_and_manual_grouping() {
    let (wb, s) = dated();
    let mut p = dated_pivot(&wb);
    p.rows = vec![1];
    p.values = vec![ValueField::new(1, Agg::Count)];
    p.groups.insert(1, Grouping::Numbers { start: 10.0, end: 39.0, by: 15.0 });
    let g = wb.pivot_grid(s, &p);
    assert_eq!(labels(&g, 0), ["Amount", "<10", "10-24", "25-39", ">39", "Grand Total"]);
    assert_eq!(labels(&g, 1), ["Count of Amount", "1", "2", "1", "2", "6"]);
    // Manual groups: a group level above the items; ungrouped items stand alone.
    p.rows = vec![2];
    p.values = vec![ValueField::new(1, Agg::Sum)];
    p.groups.clear();
    p.groups.insert(2, Grouping::Manual(vec![("Stone fruit".into(), ["Plum".to_string()].into()), ("Pomes".into(), ["Apple".to_string(), "Pear".to_string()].into())]));
    let g = wb.pivot_grid(s, &p);
    assert_eq!(labels(&g, 0), ["Fruit Group", "Kiwi", "Pomes", "Pomes", "Stone fruit", "Grand Total"]);
    assert_eq!(labels(&g, 1), ["Fruit", "Kiwi", "Apple", "Pear", "Plum", ""]);
    assert_eq!(labels(&g, 2), ["Sum of Amount", "40", "75", "10", "30", "155"]);
    // Filtering out a group hides its items.
    p.filters.insert(2, FieldFilter { hidden: ["Pomes".to_string()].into(), ..Default::default() });
    assert_eq!(labels(&wb.pivot_grid(s, &p), 0), ["Fruit Group", "Kiwi", "Stone fruit", "Grand Total"]);
}

#[test]
fn field_filters_and_sorting() {
    let (wb, s) = data();
    // Reps: Ann 30, Bo 18, Cy 9.
    let mut p = pivot(&wb, vec![1], vec![], vec![(4, Agg::Sum)]);
    p.filters.insert(1, FieldFilter { hidden: ["Bo".to_string()].into(), ..Default::default() });
    assert_eq!(labels(&wb.pivot_grid(s, &p), 0), ["Rep", "Ann", "Cy", "Grand Total"]);
    assert_eq!(wb.pivot_grid(s, &p)[3][1], n(39.0), "hidden items leave the totals");
    p.filters.insert(1, FieldFilter { label: Some(LabelFilter { op: LabelOp::BeginsWith, text: "b".into() }), ..Default::default() });
    assert_eq!(labels(&wb.pivot_grid(s, &p), 0), ["Rep", "Bo", "Grand Total"]);
    p.filters.insert(1, FieldFilter { value: Some(ValueFilter { value: 0, op: ValueOp::GreaterEq, a: 18.0, b: 0.0 }), ..Default::default() });
    assert_eq!(labels(&wb.pivot_grid(s, &p), 0), ["Rep", "Ann", "Bo", "Grand Total"]);
    p.filters.insert(1, FieldFilter { top: Some(TopFilter { n: 1, bottom: true, value: 0 }), ..Default::default() });
    assert_eq!(labels(&wb.pivot_grid(s, &p), 0), ["Rep", "Cy", "Grand Total"]);
    p.filters.clear();
    p.sorts.insert(1, FieldSort::Descending);
    assert_eq!(labels(&wb.pivot_grid(s, &p), 0), ["Rep", "Cy", "Bo", "Ann", "Grand Total"]);
    // By value inside each region: West has Ann 30, Cy 9; East has Bo 18.
    let mut p = pivot(&wb, vec![0, 1], vec![], vec![(4, Agg::Sum)]);
    p.sorts.insert(0, FieldSort::ByValue { value: 0, descending: true });
    p.sorts.insert(1, FieldSort::ByValue { value: 0, descending: false });
    let g = wb.pivot_grid(s, &p);
    assert_eq!(labels(&g, 0), ["Region", "West", "West", "East", "Grand Total"]);
    assert_eq!(labels(&g, 1), ["Rep", "Cy", "Ann", "Bo", ""]);
}

#[test]
fn slicers_and_timelines_filter_pivots() {
    let (mut wb, s) = dated();
    let mut p = dated_pivot(&wb);
    p.rows = vec![2];
    let place = (CellRef::new(0, 9), (0.0, 0.0), (160.0, 200.0));
    wb.layout_mut(s).unwrap().pivots.push(p.clone());
    let slicer = Slicer { caption: "Fruit".into(), field: "fruit".into(), source: SlicerSource::Pivots(vec!["Sales".into()]), selected: Default::default(), anchor: place.0, offset: place.1, size: place.2 };
    assert_eq!(wb.slicer_items(&slicer), ["Apple", "Kiwi", "Pear", "Plum"]);
    wb.layout_mut(s).unwrap().slicers.push(slicer);
    assert_eq!(labels(&wb.pivot_grid(s, &p), 0).len(), 6, "an empty selection shows everything");
    wb.layout_mut(s).unwrap().slicers[0].selected = ["Apple".to_string(), "Plum".to_string()].into();
    let g = wb.pivot_grid(s, &p);
    assert_eq!(labels(&g, 0), ["Fruit", "Apple", "Plum", "Grand Total"]);
    assert_eq!(labels(&g, 1), ["Sum of Amount", "75", "30", "105"]);
    // A timeline for 2024's first quarter, on top of the slicer.
    let timeline = Timeline { caption: "Date".into(), field: "Date".into(), pivots: vec!["Sales".into()], level: DateLevel::Months, range: None, anchor: place.0, offset: place.1, size: place.2 };
    let periods = wb.timeline_periods(&timeline);
    assert_eq!(periods.len(), 15, "Nov 2023 through Jan 2025");
    assert_eq!(periods[0].0, "Nov 2023");
    let (start, end) = (periods[2].1, periods[4].2);
    assert_eq!(xl_engine::slicer::range_label((start, end), DateLevel::Months), "Jan 2024 – Mar 2024");
    wb.layout_mut(s).unwrap().timelines.push(Timeline { range: Some((start, end)), ..timeline });
    let g = wb.pivot_grid(s, &p);
    assert_eq!(labels(&g, 0), ["Fruit", "Apple", "Plum", "Grand Total"]);
    assert_eq!(labels(&g, 1), ["Sum of Amount", "20", "30", "50"]);
    assert_eq!(wb.pivot_page_filters("Sales").len(), 2);
    assert!(matches!(wb.pivot_page_filters("Sales")[1].1, PageFilter::Dates(..)));
    assert!(wb.pivot_page_filters("Other").is_empty());
}

#[test]
fn table_slicers_filter_rows() {
    let (mut wb, s) = dated();
    let r = xl_engine::Range::new(CellRef::new(0, 0), CellRef::new(6, 2));
    wb.layout_mut(s).unwrap().tables.push(xl_engine::Table { name: "Sales".into(), range: r, totals: false, style: Default::default(), banded: true });
    let slicer = Slicer { caption: "Fruit".into(), field: "Fruit".into(), source: SlicerSource::Table("sales".into()), selected: ["Apple".to_string()].into(), anchor: CellRef::new(0, 5), offset: (0.0, 0.0), size: (160.0, 200.0) };
    assert_eq!(wb.slicer_items(&slicer), ["Apple", "Kiwi", "Pear", "Plum"]);
    wb.layout_mut(s).unwrap().slicers.push(slicer);
    wb.apply_table_slicers("Sales");
    let hidden: Vec<u32> = wb.layout(s).filtered_rows.iter().copied().collect();
    assert_eq!(hidden, vec![2, 4, 5]);
    wb.layout_mut(s).unwrap().slicers[0].selected.clear();
    wb.apply_table_slicers("Sales");
    assert!(wb.layout(s).filtered_rows.is_empty());
}

#[test]
fn output_shifts_with_inserted_rows() {
    let (mut wb, s) = data();
    let mut p = pivot(&wb, vec![0], vec![], vec![(4, Agg::Sum)]);
    p.anchor = CellRef::new(10, 0);
    p.extent = Some(xl_engine::Range::new(CellRef::new(10, 0), CellRef::new(13, 1)));
    wb.layout_mut(s).unwrap().pivots.push(p);
    wb.apply_shift(xl_engine::Shift::InsertRows { sheet: s, at: 2, count: 3 });
    let p = &wb.layout(s).pivots[0];
    assert_eq!(p.anchor, CellRef::new(13, 0));
    assert_eq!(p.extent.unwrap().end, CellRef::new(16, 1));
    assert_eq!(p.source.to_source(0, 0, wb.sheets()), "Sheet1!$A$1:$E$9");
}
