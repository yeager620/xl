//! Quick recalc benchmarks: `cargo run -p xl-bench --release`

use std::time::Instant;

use xl_engine::{CellRef, Sheet};

fn ms(d: std::time::Duration) -> String {
    format!("{:>9.3} ms", d.as_secs_f64() * 1e3)
}

fn main() {
    const N: u32 = 100_000;

    // 1. Load 100k values + 100k copied-down formulas (B = A*2+1) + a column total.
    let mut s = Sheet::new();
    let t = Instant::now();
    let mut items = Vec::with_capacity(2 * N as usize + 1);
    for r in 0..N {
        items.push((CellRef::new(r, 0), (r % 97).to_string()));
        items.push((CellRef::new(r, 1), format!("=A{}*2+1", r + 1)));
    }
    items.push((CellRef::new(0, 2), "=SUM(B:B)".to_string()));
    let st = s.set_inputs(items);
    println!("load 200k cells + recalc          {}  (evaluated {})", ms(t.elapsed()), st.evaluated);

    // 2. Edit one input: 1 row formula + column SUM.
    let st = s.set_input(CellRef::new(500, 0), "1234").unwrap();
    println!("edit 1 cell -> 2 dependents       {}  (dirty {}, evaluated {})", ms(st.elapsed), st.dirty, st.evaluated);

    // 3. Edit with early cutoff (value unchanged in effect).
    let st = s.set_input(CellRef::new(500, 0), "1234").unwrap();
    println!("re-enter same value               {}  (dirty {}, evaluated {})", ms(st.elapsed), st.dirty, st.evaluated);

    // 4. Long dependency chain: C2 = C1+1 ... 100k deep, edit the head.
    let mut s = Sheet::new();
    let mut items = vec![(CellRef::new(0, 0), "1".to_string())];
    for r in 1..N {
        items.push((CellRef::new(r, 0), format!("=A{}+1", r)));
    }
    s.set_inputs(items);
    let st = s.set_input(CellRef::new(0, 0), "2").unwrap();
    println!("edit head of 100k-long chain      {}  (dirty {}, evaluated {})", ms(st.elapsed), st.dirty, st.evaluated);

    // 5. Running totals (each row sums a growing range) — worst case for range index.
    let mut s = Sheet::new();
    const M: u32 = 5_000;
    let mut items = Vec::new();
    for r in 0..M {
        items.push((CellRef::new(r, 0), "1".to_string()));
        items.push((CellRef::new(r, 1), format!("=SUM($A$1:A{})", r + 1)));
    }
    let t = Instant::now();
    s.set_inputs(items);
    println!("5k running totals load            {}", ms(t.elapsed()));
    let st = s.set_input(CellRef::new(M - 1, 0), "2").unwrap();
    println!("edit last row of running totals   {}  (dirty {}, evaluated {})", ms(st.elapsed), st.dirty, st.evaluated);
}
