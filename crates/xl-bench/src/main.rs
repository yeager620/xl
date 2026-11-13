//! Quick recalc benchmarks: `cargo run -p xl-bench --release`

use std::time::Instant;

use xl_engine::{Addr, CellRef, Workbook};

fn ms(d: std::time::Duration) -> String {
    format!("{:>9.3} ms", d.as_secs_f64() * 1e3)
}

fn main() {
    const N: u32 = 100_000;
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let at = |r: u32, c: u32| Addr::new(s, CellRef::new(r, c));

    // 1. 100k values + 100k copied-down formulas (B = A*2+1) + a column total.
    let t = Instant::now();
    let mut items = Vec::with_capacity(2 * N as usize + 1);
    for r in 0..N {
        items.push((at(r, 0), (r % 97).to_string()));
        items.push((at(r, 1), format!("=A{}*2+1", r + 1)));
    }
    items.push((at(0, 2), "=SUM(B:B)".to_string()));
    let st = wb.set_inputs(items);
    println!("load 200k cells + recalc          {}  (evaluated {})", ms(t.elapsed()), st.evaluated);

    let st = wb.set_input(at(500, 0), "1234").unwrap();
    println!("edit 1 cell -> 2 dependents       {}  (dirty {}, evaluated {})", ms(st.elapsed), st.dirty, st.evaluated);
    let st = wb.set_input(at(500, 0), "1234").unwrap();
    println!("re-enter same value               {}  (dirty {}, evaluated {})", ms(st.elapsed), st.dirty, st.evaluated);

    // 2. Long dependency chain.
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let at = |r: u32, c: u32| Addr::new(s, CellRef::new(r, c));
    let mut items = vec![(at(0, 0), "1".to_string())];
    for r in 1..N {
        items.push((at(r, 0), format!("=A{r}+1")));
    }
    wb.set_inputs(items);
    let st = wb.set_input(at(0, 0), "2").unwrap();
    println!("edit head of 100k-long chain      {}  (dirty {}, evaluated {})", ms(st.elapsed), st.dirty, st.evaluated);

    // 3. Running totals.
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let at = |r: u32, c: u32| Addr::new(s, CellRef::new(r, c));
    const M: u32 = 5_000;
    let mut items = Vec::new();
    for r in 0..M {
        items.push((at(r, 0), "1".to_string()));
        items.push((at(r, 1), format!("=SUM($A$1:A{})", r + 1)));
    }
    let t = Instant::now();
    wb.set_inputs(items);
    println!("5k running totals load            {}", ms(t.elapsed()));
    let st = wb.set_input(at(M - 1, 0), "2").unwrap();
    println!("edit last row of running totals   {}  (dirty {}, evaluated {})", ms(st.elapsed), st.dirty, st.evaluated);

    // 4. Rolling windows: 100k formulas each summing a 6-row window of column A.
    //    Stresses the range index: each edit must find the few ranges that
    //    contain the cell among 100k ranges in the same column.
    let mut wb = Workbook::new();
    let s = wb.sheet_ids()[0];
    let at = |r: u32, c: u32| Addr::new(s, CellRef::new(r, c));
    let mut items = Vec::new();
    for r in 0..N {
        items.push((at(r, 0), (r % 13).to_string()));
        items.push((at(r, 1), format!("=SUM(A{}:A{})", r + 1, r + 6)));
    }
    wb.set_inputs(items);
    let t = Instant::now();
    let mut evaluated = 0;
    for i in 0..1000u32 {
        evaluated += wb.set_input(at(i * 97 % N, 0), &(i % 7).to_string()).unwrap().evaluated;
    }
    println!("1000 edits under 100k rolling sums {}  (evaluated {evaluated})", ms(t.elapsed()));
    let t = Instant::now();
    let paste: Vec<_> = (0..5000u32).map(|i| (at(i * 19, 0), "5".to_string())).collect();
    let st = wb.set_inputs(paste);
    println!("paste 5k cells under rolling sums  {}  (evaluated {})", ms(t.elapsed()), st.evaluated);
}
