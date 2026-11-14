//! Conditional formatting rules.

use std::sync::Arc;

use xl_engine::cond::{CfOp, CfResult, CfRule, CfStyle, CfValue, CondFormat, CondView, TextMatch};
use xl_engine::formula::parse_formula;
use xl_engine::{Addr, CellRef, Range, Shift, Workbook};

fn r(a: &str, b: &str) -> Range {
    Range::new(CellRef::parse(a).unwrap(), CellRef::parse(b).unwrap())
}

fn red() -> CfStyle {
    CfStyle { fill: Some(0xFFC7CE), color: Some(0x9C0006), ..CfStyle::default() }
}

fn setup(values: &[(&str, &str)]) -> (Workbook, u32) {
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    wb.set_inputs(values.iter().map(|(c, v)| (Addr::new(s, CellRef::parse(c).unwrap()), v.to_string())));
    (wb, s)
}

fn hit(wb: &Workbook, s: u32, c: &str) -> CfResult {
    CondView::new(wb, s).cell(CellRef::parse(c).unwrap())
}

#[test]
fn highlight_rules() {
    let (mut wb, s) = setup(&[("A1", "5"), ("A2", "15"), ("A3", "25"), ("B1", "apple pie"), ("B2", "Banana"), ("B3", "apple"), ("C1", "10")]);
    let expr = |src: &str| Arc::new(parse_formula(src, wb.sheets()).unwrap());
    let rules = vec![
        CondFormat::new(vec![r("A1", "A3")], CfRule::Cell { op: CfOp::Greater, a: expr("$C$1"), b: None, style: red() }),
        CondFormat::new(vec![r("B1", "B3")], CfRule::Text { kind: TextMatch::Contains, text: "APPLE".into(), style: CfStyle { bold: true, ..CfStyle::default() } }),
        CondFormat::new(vec![r("A1", "A3")], CfRule::Cell { op: CfOp::Between, a: expr("20"), b: Some(expr("1")), style: CfStyle { italic: true, fill: Some(0x00FF00), ..CfStyle::default() } }),
    ];
    wb.layout_mut(s).unwrap().cond = rules;
    assert_eq!(hit(&wb, s, "A1").fill, Some(0x00FF00), "5 is between 1 and 20 (bounds in either order)");
    assert!(hit(&wb, s, "A1").italic);
    let a2 = hit(&wb, s, "A2");
    assert_eq!(a2.fill, Some(0xFFC7CE), "> C1 wins: it has the higher priority");
    assert!(a2.italic, "lower rules still add what the higher ones don't set");
    assert!(!hit(&wb, s, "A3").italic);
    assert!(hit(&wb, s, "B1").bold && hit(&wb, s, "B3").bold && !hit(&wb, s, "B2").bold);
    // The threshold is a live reference.
    wb.set_input(Addr::new(s, CellRef::parse("C1").unwrap()), "100").unwrap();
    assert_eq!(hit(&wb, s, "A3").fill, None);
}

#[test]
fn ranking_average_duplicates_formula() {
    let (mut wb, s) = setup(&[("A1", "1"), ("A2", "9"), ("A3", "4"), ("A4", "7"), ("A5", "4"), ("B1", "x"), ("B2", "y"), ("B3", "X")]);
    let style = |c: u32| CfStyle { fill: Some(c), ..CfStyle::default() };
    let f = Arc::new(parse_formula("MOD(A1,2)=1", wb.sheets()).unwrap());
    wb.layout_mut(s).unwrap().cond = vec![
        CondFormat::new(vec![r("A1", "A5")], CfRule::Top { n: 2, percent: false, bottom: false, style: style(1) }),
        CondFormat::new(vec![r("A1", "A5")], CfRule::Average { above: false, style: style(2) }),
        CondFormat::new(vec![r("A1", "A5"), r("B1", "B3")], CfRule::Duplicates { unique: false, style: CfStyle { bold: true, ..CfStyle::default() } }),
        CondFormat::new(vec![r("C1", "C5")], CfRule::Formula { expr: f, style: style(3) }),
    ];
    let fills: Vec<Option<u32>> = (1..=5).map(|i| hit(&wb, s, &format!("A{i}")).fill).collect();
    assert_eq!(fills, [Some(2), Some(1), Some(2), Some(1), Some(2)], "top 2 = 9 and 7; mean 5 → 1 and both 4s below");
    assert!(hit(&wb, s, "A3").bold && hit(&wb, s, "A5").bold && !hit(&wb, s, "A1").bold);
    assert!(hit(&wb, s, "B1").bold && hit(&wb, s, "B3").bold, "duplicates ignore case");
    // The formula is relative to C1: C2 checks A2 (9, odd).
    assert_eq!(hit(&wb, s, "C2").fill, Some(3));
    assert_eq!(hit(&wb, s, "C3").fill, None);
}

#[test]
fn color_scales_and_data_bars() {
    let (mut wb, s) = setup(&[("A1", "0"), ("A2", "50"), ("A3", "100"), ("A4", "")]);
    wb.layout_mut(s).unwrap().cond = vec![
        CondFormat::new(vec![r("A1", "A4")], CfRule::ColorScale { stops: vec![(CfValue::Min, 0xF8696B), (CfValue::Percentile(50.0), 0xFFEB84), (CfValue::Max, 0x63BE7B)] }),
        CondFormat::new(vec![r("A1", "A4")], CfRule::DataBar { color: 0x638EC6, min: CfValue::Min, max: CfValue::Max }),
    ];
    assert_eq!(hit(&wb, s, "A1").fill, Some(0xF8696B));
    assert_eq!(hit(&wb, s, "A2").fill, Some(0xFFEB84));
    assert_eq!(hit(&wb, s, "A3").fill, Some(0x63BE7B));
    assert_eq!(hit(&wb, s, "A2").bar, Some((0.5, 0x638EC6)));
    assert!(hit(&wb, s, "A4").is_empty(), "blanks get neither");
}

#[test]
fn rules_follow_inserted_rows() {
    let (mut wb, s) = setup(&[("A1", "1"), ("A2", "2"), ("C1", "1")]);
    let a = Arc::new(parse_formula("$C$1", wb.sheets()).unwrap());
    wb.layout_mut(s).unwrap().cond = vec![CondFormat::new(vec![r("A1", "A2")], CfRule::Cell { op: CfOp::Greater, a, b: None, style: red() })];
    wb.apply_shift(Shift::InsertRows { sheet: s, at: 0, count: 1 });
    let cf = &wb.layout(s).cond[0];
    assert_eq!(cf.ranges, vec![r("A2", "A3")]);
    assert_eq!(hit(&wb, s, "A3").fill, Some(0xFFC7CE), "the threshold ref moved to $C$2 with its cell");
    wb.apply_shift(Shift::DeleteRows { sheet: s, at: 1, count: 2 });
    assert!(wb.layout(s).cond.is_empty(), "a rule whose cells are all deleted goes away");
}
