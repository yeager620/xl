//! Starter workbooks offered on the Home page, built with xl's own engine:
//! real formulas, number formats, column widths, header styling, tables,
//! conditional formatting and charts.

use std::sync::Arc;

use xl_engine::chart::{Chart, ChartKind, ChartSeries};
use xl_engine::cond::{CfOp, CfRule, CfStyle, CfValue, CondFormat, TextMatch};
use xl_engine::formula::{Expr, parse_formula};
use xl_engine::{Addr, Border, CellRef, HAlign, Range, Rgb, SheetId, Style, Table, TableStyle, Workbook, parse_col};

pub struct Template {
    pub name: &'static str,
    pub blurb: &'static str,
    /// The workbook and the cell to start in.
    pub build: fn() -> (Workbook, CellRef),
}

pub const TEMPLATES: [Template; 6] = [
    Template { name: "Monthly budget", blurb: "Plan income and spending, see what's left", build: budget },
    Template { name: "Invoice", blurb: "Line items, tax and a total that adds itself up", build: invoice },
    Template { name: "Project tracker", blurb: "Tasks, owners, status and what's overdue", build: tracker },
    Template { name: "Loan calculator", blurb: "Monthly payment and a full amortization table", build: loan },
    Template { name: "Habit tracker", blurb: "Tick off each day, watch the streaks grow", build: habits },
    Template { name: "Grade book", blurb: "Weighted averages and letter grades", build: grades },
];

const TITLE_PT: u16 = 180;
const HEAD_FILL: Rgb = 0xF1F2F4;
const MUTED: Rgb = 0x6A6F78;
const INPUT_FILL: Rgb = 0xFFF6D6;
const GOOD: CfStyle = CfStyle { fill: Some(0xD9EDDA), color: Some(0x1E6B34), bold: false, italic: false };
const BAD: CfStyle = CfStyle { fill: Some(0xF6D5D2), color: Some(0xA3241A), bold: false, italic: false };

fn cell(a1: &str) -> CellRef {
    CellRef::parse(a1).unwrap_or_else(|| panic!("bad cell {a1}"))
}

fn range(r: &str) -> Range {
    match r.split_once(':') {
        Some((a, b)) => Range::new(cell(a), cell(b)),
        None => Range::single(cell(r)),
    }
}

/// A sheet under construction. Inputs are collected and set in one recalc.
struct Builder {
    wb: Workbook,
    s: SheetId,
    items: Vec<(Addr, String)>,
    tables: Vec<Table>,
}

impl Builder {
    fn new(sheet: &str) -> Self {
        let mut wb = Workbook::new();
        let s = wb.sheet_ids()[0];
        wb.rename_sheet(s, sheet).expect("valid sheet name");
        Self { wb, s, items: Vec::new(), tables: Vec::new() }
    }

    fn put(&mut self, a1: &str, v: impl Into<String>) {
        self.items.push((Addr::new(self.s, cell(a1)), v.into()));
    }

    /// Values across a row, starting at `a1` ("" skips a cell).
    fn row(&mut self, a1: &str, vals: &[&str]) {
        let start = cell(a1);
        for (i, v) in vals.iter().enumerate() {
            if !v.is_empty() {
                self.items.push((Addr::new(self.s, CellRef::new(start.row, start.col + i as u32)), v.to_string()));
            }
        }
    }

    fn fmt(&mut self, r: &str, code: &str) {
        for c in range(r).iter() {
            self.wb.set_format(Addr::new(self.s, c), Some(code));
        }
    }

    fn style(&mut self, r: &str, f: impl Fn(&mut Style)) {
        for c in range(r).iter() {
            let a = Addr::new(self.s, c);
            let mut st = self.wb.style(a).clone();
            f(&mut st);
            self.wb.set_style(a, &st);
        }
    }

    fn title(&mut self, a1: &str, text: &str) {
        self.put(a1, text);
        self.style(a1, |s| {
            s.bold = true;
            s.font_size = Some(TITLE_PT);
        });
        self.wb.set_row_height(self.s, cell(a1).row, Some(32.0));
    }

    /// Bold header cells on a light fill with a rule underneath.
    fn header(&mut self, r: &str) {
        self.style(r, |s| {
            s.bold = true;
            s.fill = Some(HEAD_FILL);
            s.bottom = Border::THIN;
        });
    }

    fn muted(&mut self, r: &str) {
        self.style(r, |s| s.color = Some(MUTED));
    }

    fn align(&mut self, r: &str, a: HAlign) {
        self.style(r, |s| s.align = a);
    }

    fn widths(&mut self, cols: &[(&str, f64)]) {
        for &(col, w) in cols {
            let c = parse_col(col).unwrap_or_else(|| panic!("bad column {col}"));
            self.wb.set_col_width(self.s, c, Some(w));
        }
    }

    fn expr(&self, src: &str) -> Arc<Expr> {
        Arc::new(parse_formula(src, self.wb.sheets()).unwrap_or_else(|e| panic!("bad formula {src}: {e}")))
    }

    fn cond(&mut self, r: &str, rule: CfRule) {
        if let Some(l) = self.wb.layout_mut(self.s) {
            l.cond.push(CondFormat::new(vec![range(r)], rule));
        }
    }

    fn chart(&mut self, kind: ChartKind, title: &str, at: &str, size: (f64, f64), categories: &str, series: &[(&str, &str)]) {
        let sheet = xl_engine::quote_sheet_name(self.wb.sheet_name(self.s)).to_string();
        let categories = Some(self.expr(&format!("{sheet}!{categories}")));
        let series = series
            .iter()
            .map(|&(name, values)| ChartSeries { name: Some(self.expr(&format!("{sheet}!{name}"))), categories: categories.clone(), values: self.expr(&format!("{sheet}!{values}")) })
            .collect();
        let chart = Chart { kind, title: Some(title.into()), series, anchor: cell(at), offset: (0.0, 0.0), size, stacked: false, pivot: None };
        if let Some(l) = self.wb.layout_mut(self.s) {
            l.charts.push(chart);
        }
    }

    fn table(&mut self, name: &str, r: &str, style: TableStyle) {
        self.tables.push(Table { name: name.into(), range: range(r), totals: false, style, banded: true });
    }

    fn freeze(&mut self, rows: u32, cols: u32) {
        if let Some(l) = self.wb.layout_mut(self.s) {
            l.freeze = (rows, cols);
        }
    }

    fn finish(mut self, start: &str) -> (Workbook, CellRef) {
        self.wb.set_inputs(std::mem::take(&mut self.items));
        for t in std::mem::take(&mut self.tables) {
            self.wb.add_table(self.s, t).expect("template table");
        }
        (self.wb, cell(start))
    }
}

fn budget() -> (Workbook, CellRef) {
    let mut b = Builder::new("Budget");
    b.title("A1", "Monthly budget");
    b.put("A2", "=TEXT(TODAY(),\"mmmm yyyy\")");
    b.muted("A2");

    b.put("A4", "Income");
    b.style("A4", |s| s.bold = true);
    b.row("A5", &["Source", "Budget", "Actual"]);
    b.header("A5:C5");
    b.row("A6", &["Salary", "5200", "5200"]);
    b.row("A7", &["Side projects", "400", "520"]);
    b.row("A8", &["Total income", "=SUM(B6:B7)", "=SUM(C6:C7)"]);
    b.style("A8:C8", |s| {
        s.bold = true;
        s.top = Border::THIN;
    });

    b.put("A10", "Expenses");
    b.style("A10", |s| s.bold = true);
    b.row("A11", &["Category", "Budget", "Actual", "Difference"]);
    let expenses: [(&str, &str, &str); 8] = [
        ("Rent", "1600", "1600"),
        ("Groceries", "450", "512"),
        ("Utilities", "180", "164"),
        ("Transport", "220", "198"),
        ("Insurance", "150", "150"),
        ("Dining out", "200", "265"),
        ("Entertainment", "120", "85"),
        ("Savings", "800", "800"),
    ];
    for (i, (name, budget, actual)) in expenses.iter().enumerate() {
        let r = 12 + i;
        b.row(&format!("A{r}"), &[name, budget, actual, &format!("=B{r}-C{r}")]);
    }
    b.table("Expenses", "A11:D19", TableStyle::Blue);
    b.row("A20", &["Total expenses", "=SUM(B12:B19)", "=SUM(C12:C19)", "=SUM(D12:D19)"]);
    b.style("A20:D20", |s| {
        s.bold = true;
        s.top = Border::THIN;
    });

    b.put("F4", "Summary");
    b.style("F4", |s| s.bold = true);
    b.row("F5", &["Income", "=C8"]);
    b.row("F6", &["Spent", "=C20"]);
    b.row("F7", &["Left over", "=G5-G6"]);
    b.style("F7:G7", |s| {
        s.bold = true;
        s.top = Border::THIN;
    });
    b.fmt("B6:D20", "$#,##0;[Red]-$#,##0");
    b.fmt("G5:G7", "$#,##0;[Red]-$#,##0");
    b.align("B5:D5", HAlign::Right);
    b.align("B11:D11", HAlign::Right);

    // Over budget stands out.
    let zero = b.expr("0");
    b.cond("D12:D19", CfRule::Cell { op: CfOp::Less, a: zero, b: None, style: BAD });
    b.chart(ChartKind::Column, "Budget vs. actual", "F10", (440.0, 260.0), "$A$12:$A$19", &[("$B$11", "$B$12:$B$19"), ("$C$11", "$C$12:$C$19")]);
    b.widths(&[("A", 150.0), ("B", 96.0), ("C", 96.0), ("D", 96.0), ("E", 24.0), ("F", 96.0), ("G", 96.0)]);
    b.finish("C6")
}

fn invoice() -> (Workbook, CellRef) {
    let mut b = Builder::new("Invoice");
    b.put("A1", "INVOICE");
    b.style("A1", |s| {
        s.bold = true;
        s.font_size = Some(220);
        s.color = Some(0x1F5FBF);
    });
    b.wb.set_row_height(b.s, 0, Some(36.0));
    b.put("A3", "Your Company");
    b.style("A3", |s| s.bold = true);
    b.put("A4", "123 Main Street");
    b.put("A5", "Springfield, ST 00000");
    b.put("A6", "billing@example.com");
    b.muted("A4:A6");

    b.row("C3", &["Invoice #", "1001"]);
    b.row("C4", &["Date", "=TODAY()"]);
    b.row("C5", &["Due", "=D4+30"]);
    b.style("C3:C5", |s| {
        s.bold = true;
        s.align = HAlign::Right;
        s.color = Some(MUTED);
    });
    b.fmt("D4:D5", "mmm d, yyyy");
    b.align("D3:D5", HAlign::Right);

    b.put("A8", "Bill to");
    b.style("A8", |s| s.bold = true);
    b.put("A9", "Client name");
    b.put("A10", "Client street address");
    b.muted("A10");

    b.row("A12", &["Description", "Qty", "Unit price", "Amount"]);
    let items: [(&str, &str, &str); 5] =
        [("Design consultation", "4", "120"), ("Website development", "20", "95"), ("Hosting (12 months)", "1", "180"), ("", "", ""), ("", "", "")];
    for (i, (desc, qty, price)) in items.iter().enumerate() {
        let r = 13 + i;
        b.row(&format!("A{r}"), &[desc, qty, price, &format!("=IF(B{r}=\"\",\"\",B{r}*C{r})")]);
    }
    b.table("Items", "A12:D17", TableStyle::Blue);
    b.fmt("C13:D17", "$#,##0.00");

    b.row("C19", &["Subtotal", "=SUM(D13:D17)"]);
    b.row("C20", &["Tax rate", "0.08"]);
    b.row("C21", &["Tax", "=ROUND(D19*D20,2)"]);
    b.row("C22", &["Total due", "=D19+D21"]);
    b.align("C19:C22", HAlign::Right);
    b.fmt("D19", "$#,##0.00");
    b.fmt("D20", "0.0%");
    b.fmt("D21:D22", "$#,##0.00");
    b.style("D20", |s| s.fill = Some(INPUT_FILL));
    b.style("C22:D22", |s| {
        s.bold = true;
        s.top = Border::THIN;
        s.fill = Some(0xD6E3F7);
    });

    b.put("A24", "Payment is due within 30 days. Thank you for your business!");
    b.style("A24", |s| {
        s.italic = true;
        s.color = Some(MUTED);
    });
    b.widths(&[("A", 260.0), ("B", 72.0), ("C", 100.0), ("D", 116.0)]);
    b.finish("A9")
}

fn tracker() -> (Workbook, CellRef) {
    let mut b = Builder::new("Tasks");
    b.title("A1", "Project tracker");
    b.put("A2", "=COUNTIF(C5:C12,\"Done\")&\" of \"&COUNTA(A5:A12)&\" tasks done\"");
    b.muted("A2");
    b.row("E2", &["Progress", "=IFERROR(COUNTIF(C5:C12,\"Done\")/COUNTA(A5:A12),0)"]);
    b.style("E2", |s| {
        s.align = HAlign::Right;
        s.color = Some(MUTED);
    });
    b.fmt("F2", "0%");

    b.row("A4", &["Task", "Owner", "Status", "Priority", "Due", "Notes"]);
    let tasks: [(&str, &str, &str, &str, &str, &str); 8] = [
        ("Kick-off meeting", "Ana", "Done", "High", "=TODAY()-14", "Agenda shared"),
        ("Gather requirements", "Ben", "Done", "High", "=TODAY()-7", ""),
        ("Draft design", "Ana", "In progress", "High", "=TODAY()-1", "Waiting on feedback"),
        ("Set up environment", "Chen", "In progress", "Medium", "=TODAY()+3", ""),
        ("Build prototype", "Ben", "Not started", "High", "=TODAY()+10", ""),
        ("User testing", "Dana", "Not started", "Medium", "=TODAY()+17", ""),
        ("Vendor contract", "Chen", "Blocked", "Low", "=TODAY()+5", "Legal review"),
        ("Launch plan", "Dana", "Not started", "Medium", "=TODAY()+24", ""),
    ];
    for (i, t) in tasks.iter().enumerate() {
        b.row(&format!("A{}", 5 + i), &[t.0, t.1, t.2, t.3, t.4, t.5]);
    }
    b.table("Tasks", "A4:F12", TableStyle::Green);
    b.fmt("E5:E12", "ddd, mmm d");
    b.align("C5:D12", HAlign::Center);
    b.align("C4:D4", HAlign::Center);
    b.muted("F5:F12");

    b.cond("C5:C12", CfRule::Text { kind: TextMatch::Contains, text: "Done".into(), style: GOOD });
    b.cond("C5:C12", CfRule::Text { kind: TextMatch::Contains, text: "Blocked".into(), style: BAD });
    let overdue = b.expr("AND($E5<TODAY(),$C5<>\"Done\")");
    b.cond("E5:E12", CfRule::Formula { expr: overdue, style: CfStyle { fill: None, color: Some(0xC4271B), bold: true, italic: false } });
    b.cond("F2", CfRule::DataBar { color: 0x2E7D32, min: CfValue::Number(0.0), max: CfValue::Number(1.0) });
    b.widths(&[("A", 190.0), ("B", 80.0), ("C", 100.0), ("D", 76.0), ("E", 104.0), ("F", 180.0)]);
    b.freeze(4, 0);
    b.finish("A5")
}

/// Rows of the amortization schedule (30 years of monthly payments).
const LOAN_ROWS: u32 = 360;

fn loan() -> (Workbook, CellRef) {
    let mut b = Builder::new("Loan");
    b.title("A1", "Loan calculator");
    b.row("A3", &["Loan amount", "300000"]);
    b.row("A4", &["Annual interest rate", "0.065"]);
    b.row("A5", &["Term (years)", "30"]);
    b.row("A6", &["Payments per year", "12"]);
    b.fmt("B3", "$#,##0");
    b.fmt("B4", "0.00%");
    b.style("B3:B6", |s| {
        s.fill = Some(INPUT_FILL);
        s.bottom = Border::THIN;
    });
    b.put("A7", "Change the yellow cells.");
    b.style("A7", |s| {
        s.italic = true;
        s.color = Some(MUTED);
    });

    b.row("D3", &["Payment", "=IF(B3*B5*B6=0,0,-PMT(B4/B6,B5*B6,B3))"]);
    b.row("D4", &["Number of payments", "=MIN(B5*B6,360)"]);
    b.row("D5", &["Total paid", "=E3*E4"]);
    b.row("D6", &["Total interest", "=E5-B3"]);
    b.style("D3:E3", |s| s.bold = true);
    b.fmt("E3", "$#,##0.00");
    b.fmt("E5:E6", "$#,##0.00");

    b.row("A9", &["No.", "Payment", "Principal", "Interest", "Balance"]);
    b.header("A9:E9");
    b.align("B9:E9", HAlign::Right);
    for n in 1..=LOAN_ROWS {
        let r = 9 + n;
        let prev = if n == 1 { "$B$3".to_string() } else { format!("E{}", r - 1) };
        b.row(
            &format!("A{r}"),
            &[
                &format!("=IF({n}<=$E$4,{n},\"\")"),
                &format!("=IF(A{r}=\"\",\"\",$E$3)"),
                &format!("=IF(A{r}=\"\",\"\",B{r}-D{r})"),
                &format!("=IF(A{r}=\"\",\"\",{prev}*$B$4/$B$6)"),
                &format!("=IF(A{r}=\"\",\"\",{prev}-C{r})"),
            ],
        );
    }
    let last = 9 + LOAN_ROWS;
    b.fmt(&format!("B10:E{last}"), "$#,##0.00");
    b.align(&format!("A10:A{last}"), HAlign::Center);
    b.chart(ChartKind::Line, "Balance over time", "G3", (420.0, 250.0), &format!("$A$10:$A${last}"), &[("$E$9", &format!("$E$10:$E${last}"))]);
    b.widths(&[("A", 150.0), ("B", 100.0), ("C", 100.0), ("D", 130.0), ("E", 110.0), ("F", 24.0)]);
    b.freeze(9, 0);
    b.finish("B3")
}

fn habits() -> (Workbook, CellRef) {
    let mut b = Builder::new("Habits");
    b.title("A1", "Habit tracker");
    b.put("A2", "=DATE(YEAR(TODAY()),MONTH(TODAY()),1)");
    b.fmt("A2", "mmmm yyyy");
    b.style("A2", |s| {
        s.color = Some(MUTED);
        s.align = HAlign::Left;
    });
    let days: Vec<String> = (1..=31).map(|d| d.to_string()).collect();
    let mut header: Vec<&str> = vec!["Habit"];
    header.extend(days.iter().map(String::as_str));
    header.extend(["Done", "Goal", "Progress"]);
    b.row("A4", &header);
    b.header("A4:AI4");
    let habits = ["Drink 2L of water", "Exercise 30 min", "Read 20 pages", "Meditate", "No phone after 10pm", "In bed by 11"];
    for (i, h) in habits.iter().enumerate() {
        let r = 5 + i;
        b.put(&format!("A{r}"), *h);
        // A few days already ticked off, so the sheet shows how it works.
        for d in 0..10u32 {
            if !(d + i as u32 * 3).is_multiple_of(4) {
                b.items.push((Addr::new(b.s, CellRef::new(r as u32 - 1, 1 + d)), "x".into()));
            }
        }
        b.row(&format!("AG{r}"), &[&format!("=COUNTIF(B{r}:AF{r},\"x\")"), "20", &format!("=IF(AH{r}=0,0,MIN(1,AG{r}/AH{r}))")]);
    }
    b.fmt("AI5:AI10", "0%");
    b.align("B4:AH10", HAlign::Center);
    b.style("AH5:AH10", |s| s.fill = Some(INPUT_FILL));
    b.cond("B5:AF10", CfRule::Text { kind: TextMatch::Contains, text: "x".into(), style: CfStyle { fill: Some(0xBFE3C6), color: Some(0x1E6B34), bold: true, italic: false } });
    // Days past the end of the month are greyed out.
    let past_end = b.expr("B$4>DAY(EOMONTH($A$2,0))");
    b.cond("B4:AF10", CfRule::Formula { expr: past_end, style: CfStyle { fill: Some(0xE4E6E9), color: Some(0x9AA0A9), bold: false, italic: false } });
    b.cond("AI5:AI10", CfRule::DataBar { color: 0x2E7D32, min: CfValue::Number(0.0), max: CfValue::Number(1.0) });
    let mut widths = vec![("A", 170.0)];
    let cols: Vec<String> = (1..=31).map(xl_engine::col_name).collect();
    widths.extend(cols.iter().map(|c| (c.as_str(), 26.0)));
    widths.extend([("AG", 52.0), ("AH", 52.0), ("AI", 90.0)]);
    b.widths(&widths);
    b.freeze(4, 1);
    b.finish("L5")
}

fn grades() -> (Workbook, CellRef) {
    let mut b = Builder::new("Grades");
    b.title("A1", "Grade book");
    b.put("A2", "Course · Term");
    b.muted("A2");
    b.row("A4", &["Student", "Quiz 1", "Quiz 2", "Midterm", "Project", "Final", "Average", "Grade"]);
    b.header("A4:H4");
    b.align("B4:H4", HAlign::Right);
    b.row("A5", &["Weight", "0.1", "0.1", "0.25", "0.2", "0.35", "=SUM(B5:F5)"]);
    b.fmt("B5:G5", "0%");
    b.style("A5:G5", |s| {
        s.italic = true;
        s.color = Some(MUTED);
        s.bottom = Border::THIN;
    });
    b.style("B5:F5", |s| s.fill = Some(INPUT_FILL));
    let students: [(&str, [u32; 5]); 8] = [
        ("Avery Johnson", [92, 88, 85, 95, 90]),
        ("Blake Kim", [78, 82, 74, 80, 77]),
        ("Casey Patel", [95, 97, 93, 98, 96]),
        ("Devon Garcia", [65, 70, 58, 72, 61]),
        ("Emerson Liu", [88, 84, 90, 86, 89]),
        ("Finley Brown", [55, 62, 49, 60, 52]),
        ("Harper Nguyen", [81, 79, 88, 92, 84]),
        ("Jordan Silva", [72, 68, 75, 70, 79]),
    ];
    for (i, (name, scores)) in students.iter().enumerate() {
        let r = 6 + i;
        let s: Vec<String> = scores.iter().map(u32::to_string).collect();
        b.row(
            &format!("A{r}"),
            &[
                name,
                &s[0],
                &s[1],
                &s[2],
                &s[3],
                &s[4],
                &format!("=IF($G$5=0,0,SUMPRODUCT(B{r}:F{r},$B$5:$F$5)/$G$5)"),
                &format!("=IFS(G{r}>=90,\"A\",G{r}>=80,\"B\",G{r}>=70,\"C\",G{r}>=60,\"D\",G{r}<60,\"F\")"),
            ],
        );
    }
    b.row("A14", &["Class average", "=AVERAGE(B6:B13)", "=AVERAGE(C6:C13)", "=AVERAGE(D6:D13)", "=AVERAGE(E6:E13)", "=AVERAGE(F6:F13)", "=AVERAGE(G6:G13)"]);
    b.style("A14:H14", |s| {
        s.bold = true;
        s.top = Border::THIN;
    });
    b.fmt("B14:G14", "0.0");
    b.fmt("G6:G13", "0.0");
    b.align("H6:H13", HAlign::Center);
    b.style("H6:H13", |s| s.bold = true);
    b.cond("G6:G13", CfRule::ColorScale { stops: vec![(CfValue::Min, 0xF8696B), (CfValue::Percentile(50.0), 0xFFEB84), (CfValue::Max, 0x63BE7B)] });
    b.cond("H6:H13", CfRule::Text { kind: TextMatch::BeginsWith, text: "F".into(), style: BAD });
    b.chart(ChartKind::Bar, "Average by student", "J4", (400.0, 260.0), "$A$6:$A$13", &[("$G$4", "$G$6:$G$13")]);
    b.widths(&[("A", 150.0), ("B", 72.0), ("C", 72.0), ("D", 72.0), ("E", 72.0), ("F", 72.0), ("G", 80.0), ("H", 64.0), ("I", 24.0)]);
    b.freeze(5, 1);
    b.finish("B6")
}

/// A cell in a template's thumbnail.
#[derive(Clone, Debug, Default)]
pub struct PreviewCell {
    pub fill: Option<Rgb>,
    pub bold: bool,
    /// Length of the shown text (0 = empty).
    pub len: usize,
    pub numeric: bool,
    /// Header of a table (drawn in the table's color).
    pub table_header: Option<Rgb>,
}

/// The top-left corner of a template, for drawing a small picture of it.
pub struct Preview {
    /// Column widths in pixels.
    pub widths: Vec<f64>,
    pub rows: Vec<Vec<PreviewCell>>,
}

pub fn preview(wb: &Workbook, rows: u32, max_width: f64) -> Preview {
    let s = wb.sheet_ids()[0];
    let mut widths = Vec::new();
    let mut total = 0.0;
    let mut c = 0;
    while total < max_width && c < 12 {
        let w = wb.col_width(s, c).unwrap_or(crate::app::DEF_COL_W);
        widths.push(w);
        total += w;
        c += 1;
    }
    let table_header = |cell: CellRef| {
        wb.layout(s).tables.iter().find(|t| t.range.start.row == cell.row && t.range.contains(cell)).map(|t| crate::data::table_colors(t.style, false).0)
    };
    let rows = (0..rows)
        .map(|r| {
            (0..widths.len() as u32)
                .map(|c| {
                    let cell = CellRef::new(r, c);
                    let a = Addr::new(s, cell);
                    let st = wb.style(a);
                    let shown = wb.display(a).text;
                    PreviewCell {
                        fill: st.fill,
                        bold: st.bold,
                        len: shown.chars().count(),
                        numeric: matches!(wb.value(a), xl_engine::Value::Number(_)),
                        table_header: table_header(cell),
                    }
                })
                .collect()
        })
        .collect();
    Preview { widths, rows }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xl_engine::Value;

    fn num(wb: &Workbook, a1: &str) -> f64 {
        match wb.value(Addr::new(wb.sheet_ids()[0], cell(a1))) {
            Value::Number(n) => *n,
            other => panic!("{a1} is {other:?}"),
        }
    }

    #[test]
    fn every_template_builds_and_computes() {
        for t in &TEMPLATES {
            let (wb, _) = (t.build)();
            let s = wb.sheet_ids()[0];
            assert!(wb.store(s).len() > 10, "{} has content", t.name);
            for (a, _) in wb.formula_texts() {
                assert!(!matches!(wb.value(a), Value::Error(_)), "{}: {} is an error", t.name, a.cell);
            }
        }
        let (b, _) = budget();
        assert_eq!(num(&b, "D13"), -62.0);
        assert_eq!(num(&b, "G7"), 5720.0 - 3774.0);
        let (i, _) = invoice();
        assert_eq!(num(&i, "D19"), 2560.0);
        assert!((num(&i, "D22") - 2764.8).abs() < 1e-9);
        let (l, _) = loan();
        assert!((num(&l, "E3") - 1896.20).abs() < 0.01, "payment {}", num(&l, "E3"));
        assert!(num(&l, "E369").abs() < 0.01, "paid off");
        let (t, _) = tracker();
        assert_eq!(num(&t, "F2"), 0.25);
        let (g, _) = grades();
        assert_eq!(num(&g, "G5"), 1.0);
        let (h, _) = habits();
        assert!(num(&h, "AG5") > 0.0);
    }
}
