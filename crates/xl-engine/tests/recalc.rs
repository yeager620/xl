use xl_engine::{CellRef, ErrorKind, Sheet, Value};

fn c(a1: &str) -> CellRef {
    CellRef::parse(a1).unwrap()
}

fn set(s: &mut Sheet, a1: &str, raw: &str) {
    s.set_input(c(a1), raw).unwrap();
}

fn v(s: &Sheet, a1: &str) -> Value {
    s.value(c(a1)).clone()
}

fn n(x: f64) -> Value {
    Value::Number(x)
}

#[test]
fn basic_arithmetic_and_propagation() {
    let mut s = Sheet::new();
    set(&mut s, "A1", "2");
    set(&mut s, "A2", "3");
    set(&mut s, "A3", "=A1*A2+1");
    set(&mut s, "A4", "=A3^2");
    assert_eq!(v(&s, "A3"), n(7.0));
    assert_eq!(v(&s, "A4"), n(49.0));
    set(&mut s, "A1", "10");
    assert_eq!(v(&s, "A3"), n(31.0));
    assert_eq!(v(&s, "A4"), n(961.0));
    set(&mut s, "A2", "");
    assert_eq!(v(&s, "A3"), n(1.0));
}

#[test]
fn precedence_matches_excel() {
    let mut s = Sheet::new();
    set(&mut s, "A1", "=-2^2");
    set(&mut s, "A2", "=2^3^2");
    set(&mut s, "A3", "=1+2*3-4/2");
    set(&mut s, "A4", "=50%*10");
    set(&mut s, "A5", "=\"a\"&1+1");
    assert_eq!(v(&s, "A1"), n(4.0));
    assert_eq!(v(&s, "A2"), n(64.0));
    assert_eq!(v(&s, "A3"), n(5.0));
    assert_eq!(v(&s, "A4"), n(5.0));
    assert_eq!(v(&s, "A5"), Value::text("a2"));
}

#[test]
fn ranges_and_aggregates() {
    let mut s = Sheet::new();
    for (i, x) in [1, 2, 3, 4].iter().enumerate() {
        s.set_input(CellRef::new(i as u32, 0), &x.to_string()).unwrap();
    }
    set(&mut s, "A5", "hello");
    set(&mut s, "B1", "=SUM(A1:A5)");
    set(&mut s, "B2", "=AVERAGE(A:A)");
    set(&mut s, "B3", "=COUNT(A1:A5)");
    set(&mut s, "B4", "=COUNTA(A1:A5)");
    set(&mut s, "B5", "=MAX(A1:A4)-MIN(A1:A4)");
    set(&mut s, "B6", "=SUMIF(A1:A4,\">2\")");
    set(&mut s, "B7", "=COUNTIF(A1:A5,\"h*\")");
    assert_eq!(v(&s, "B1"), n(10.0));
    assert_eq!(v(&s, "B2"), n(2.5));
    assert_eq!(v(&s, "B3"), n(4.0));
    assert_eq!(v(&s, "B4"), n(5.0));
    assert_eq!(v(&s, "B5"), n(3.0));
    assert_eq!(v(&s, "B6"), n(7.0));
    assert_eq!(v(&s, "B7"), n(1.0));
    // Editing a cell inside a range updates range dependents.
    set(&mut s, "A2", "20");
    assert_eq!(v(&s, "B1"), n(28.0));
    // Adding a value far down a whole-column range.
    set(&mut s, "A1000", "100");
    assert_eq!(v(&s, "B2"), n(128.0 / 5.0));
}

#[test]
fn logic_text_lookup() {
    let mut s = Sheet::new();
    set(&mut s, "A1", "apple");
    set(&mut s, "A2", "banana");
    set(&mut s, "A3", "cherry");
    set(&mut s, "B1", "1.5");
    set(&mut s, "B2", "0.25");
    set(&mut s, "B3", "3");
    set(&mut s, "C1", "=VLOOKUP(\"banana\",A1:B3,2,FALSE)");
    set(&mut s, "C2", "=INDEX(B1:B3,MATCH(\"cherry\",A1:A3,0))");
    set(&mut s, "C3", "=IF(B1>1,UPPER(A1),\"no\")");
    set(&mut s, "C4", "=IFERROR(1/0,\"div\")");
    set(&mut s, "C5", "=LEFT(A2,3)&MID(A3,2,2)&RIGHT(A1)");
    set(&mut s, "C6", "=AND(B1>1,OR(FALSE,B3=3))");
    set(&mut s, "C7", "=VLOOKUP(\"zzz\",A1:B3,2,FALSE)");
    set(&mut s, "C8", "=ROUND(2.675,2)");
    set(&mut s, "C9", "=LEN(TRIM(\"  a   b  \"))");
    assert_eq!(v(&s, "C1"), n(0.25));
    assert_eq!(v(&s, "C2"), n(3.0));
    assert_eq!(v(&s, "C3"), Value::text("APPLE"));
    assert_eq!(v(&s, "C4"), Value::text("div"));
    assert_eq!(v(&s, "C5"), Value::text("banhee"));
    assert_eq!(v(&s, "C6"), Value::Bool(true));
    assert_eq!(v(&s, "C7"), Value::Error(ErrorKind::NA));
    assert_eq!(v(&s, "C8"), n(2.68));
    assert_eq!(v(&s, "C9"), n(3.0));
}

#[test]
fn errors_and_cycles() {
    let mut s = Sheet::new();
    set(&mut s, "A1", "=1/0");
    set(&mut s, "A2", "=A1+1");
    set(&mut s, "A3", "=NOSUCHFN(1)");
    assert_eq!(v(&s, "A2"), Value::Error(ErrorKind::Div0));
    assert_eq!(v(&s, "A3"), Value::Error(ErrorKind::Name));

    set(&mut s, "B1", "=B2+1");
    set(&mut s, "B2", "=B1+1");
    assert_eq!(v(&s, "B1"), Value::Error(ErrorKind::Circ));
    assert_eq!(v(&s, "B2"), Value::Error(ErrorKind::Circ));
    // Breaking the cycle recovers.
    set(&mut s, "B2", "5");
    assert_eq!(v(&s, "B1"), n(6.0));

    set(&mut s, "C1", "=C1");
    assert_eq!(v(&s, "C1"), Value::Error(ErrorKind::Circ));
    assert!(s.set_input(c("D1"), "=1+").is_err());
}

#[test]
fn early_cutoff_skips_unchanged_branches() {
    let mut s = Sheet::new();
    set(&mut s, "A1", "1");
    set(&mut s, "B1", "=IF(A1>0,1,0)"); // stays 1 for any positive A1
    for r in 2..=1000 {
        s.set_input(CellRef::new(r - 1, 1), &format!("=B{}+1", r - 1)).unwrap();
    }
    let stats = s.set_input(c("A1"), "5").unwrap();
    assert_eq!(stats.evaluated, 1, "only B1 should be re-evaluated");
    let stats = s.set_input(c("A1"), "-5").unwrap();
    assert_eq!(stats.evaluated, 1000);
    assert_eq!(v(&s, "B1000"), n(999.0));
}

#[test]
fn shifted_copy() {
    let mut s = Sheet::new();
    set(&mut s, "C1", "=A1*$B$1");
    assert_eq!(s.input_text_shifted(c("C1"), 3, 0), "=A4*$B$1");
    set(&mut s, "D1", "'123");
    assert_eq!(s.input_text(c("D1")), "'123");
    assert_eq!(v(&s, "D1"), Value::text("123"));
}

/// Tiny deterministic PRNG so the property test needs no extra deps.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn random_input(rng: &mut Lcg, rows: u32, cols: u32) -> String {
    let rc = |rng: &mut Lcg| format!("{}{}", xl_engine::col_name(rng.below(cols as u64) as u32), rng.below(rows as u64) + 1);
    match rng.below(10) {
        0 => String::new(),
        1..=3 => format!("{}", rng.below(100) as i64 - 50),
        4 => format!("={}+{}", rc(rng), rc(rng)),
        5 => format!("={}*2-{}", rc(rng), rc(rng)),
        6 => {
            let (a, b) = (rc(rng), rc(rng));
            format!("=SUM({a}:{b})")
        }
        7 => format!("=IF({}>0,{},{})", rc(rng), rc(rng), rng.below(5)),
        8 => format!("=MAX({},{})+COUNT({}:{})", rc(rng), rc(rng), rc(rng), rc(rng)),
        _ => format!("=IFERROR({}/{},0)", rc(rng), rc(rng)),
    }
}

#[test]
fn incremental_matches_full_recalc() {
    const ROWS: u32 = 12;
    const COLS: u32 = 5;
    for seed in 0..200 {
        let mut rng = Lcg(seed * 7919 + 1);
        let mut s = Sheet::new();
        let mut inputs = std::collections::BTreeMap::new();
        for step in 0..300 {
            let cell = CellRef::new(rng.below(ROWS as u64) as u32, rng.below(COLS as u64) as u32);
            let raw = random_input(&mut rng, ROWS, COLS);
            s.set_input(cell, &raw).unwrap();
            inputs.insert(cell, raw.clone());
            // Compare against a from-scratch recalculation after every edit.
            let mut fresh = Sheet::new();
            fresh.set_inputs(inputs.iter().map(|(c, r)| (*c, r.clone())));
            fresh.recalc_all();
            for r in 0..ROWS {
                for col in 0..COLS {
                    let c = CellRef::new(r, col);
                    if s.value(c) != fresh.value(c) {
                        let sheet: Vec<String> =
                            inputs.iter().filter(|(_, v)| !v.is_empty()).map(|(c, v)| format!("{c}: {v}  -> inc {:?} / full {:?}", s.value(*c), fresh.value(*c))).collect();
                        panic!(
                            "seed {seed} step {step}: edited {cell} = {raw:?}; mismatch at {c}: incremental {:?} vs full {:?}\n{}",
                            s.value(c),
                            fresh.value(c),
                            sheet.join("\n")
                        );
                    }
                }
            }
        }
    }
}
