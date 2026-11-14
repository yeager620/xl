use xl_engine::solver::{
    Constraint, ConstraintOp, GoalSeekOptions, SolverGoal, SolverMethod, SolverOptions, SolverProblem, SolverStatus,
    goal_seek, solve,
};
use xl_engine::{Addr, CellRef, SheetId, Value, Workbook};

struct T {
    wb: Workbook,
    s: SheetId,
}

impl T {
    fn new() -> Self {
        let wb = Workbook::new();
        let s = wb.sheet_ids()[0];
        Self { wb, s }
    }
    fn a(&self, a1: &str) -> Addr {
        Addr::new(self.s, CellRef::parse(a1).unwrap())
    }
    fn set(&mut self, a1: &str, raw: &str) {
        let a = self.a(a1);
        self.wb.set_input(a, raw).unwrap();
    }
    fn num(&self, a1: &str) -> f64 {
        match self.wb.value(self.a(a1)) {
            Value::Number(n) => *n,
            Value::Empty => 0.0,
            other => panic!("{a1} is {other:?}"),
        }
    }
    fn input(&self, a1: &str) -> String {
        self.wb.input_text(self.a(a1))
    }
    fn seek(&mut self, set: &str, target: f64, changing: &str) -> Result<xl_engine::solver::GoalSeekResult, String> {
        let (s, c) = (self.a(set), self.a(changing));
        goal_seek(&mut self.wb, s, target, c, &GoalSeekOptions::default())
    }
    fn con(&self, a1: &str, op: ConstraintOp, rhs: f64) -> Constraint {
        Constraint { cell: self.a(a1), op, rhs }
    }
    fn problem(&self, objective: &str, goal: SolverGoal, vars: &[&str], constraints: Vec<Constraint>) -> SolverProblem {
        SolverProblem {
            objective: self.a(objective),
            goal,
            variables: vars.iter().map(|v| self.a(v)).collect(),
            constraints,
            non_negative: true,
        }
    }
    fn solve(&mut self, p: &SolverProblem) -> xl_engine::solver::SolverResult {
        solve(&mut self.wb, p, &SolverOptions::default()).unwrap()
    }
}

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol * b.abs().max(1.0)
}

#[track_caller]
fn assert_close(a: f64, b: f64, tol: f64) {
    assert!(close(a, b, tol), "{a} is not within {tol} of {b}");
}

// ================================================================ goal seek

/// B1 annual rate, B2 months, B3 principal, B4 monthly payment.
fn loan(rate: &str) -> T {
    let mut t = T::new();
    t.set("B1", rate);
    t.set("B2", "360");
    t.set("B3", "200000");
    t.set("B4", "=PMT(B1/12,B2,-B3)");
    t
}

#[test]
fn goal_seek_loan_rate() {
    let mut t = loan("0.05");
    let r = t.seek("B4", 1200.0, "B1").unwrap();
    assert_close(t.num("B4"), 1200.0, 1e-7);
    assert_close(r.achieved, 1200.0, 1e-7);
    assert_eq!(r.value, t.num("B1"));
    // The rate is ~6.007%; check by recomputing the payment independently.
    let i = r.value / 12.0;
    let pmt = 200000.0 * i / (1.0 - (1.0 + i).powf(-360.0));
    assert_close(pmt, 1200.0, 1e-6);
    assert!(r.value > 0.06 && r.value < 0.0601, "{}", r.value);
    assert!(r.iterations < 60, "{} iterations", r.iterations);
}

#[test]
fn goal_seek_loan_rate_starting_at_zero() {
    // PMT at rate 0 is principal / n; the search must leave that special point.
    let mut t = loan("0");
    let r = t.seek("B4", 1200.0, "B1").unwrap();
    assert_close(t.num("B4"), 1200.0, 1e-7);
    assert!(r.value > 0.06 && r.value < 0.0601, "{}", r.value);
}

#[test]
fn goal_seek_loan_principal() {
    let mut t = loan("0.06");
    t.seek("B4", 1000.0, "B3").unwrap();
    // Closed form: P = pmt * (1 - (1+i)^-n) / i
    let expected = 1000.0 * (1.0 - 1.005f64.powf(-360.0)) / 0.005;
    assert_close(t.num("B3"), expected, 1e-7);
    assert_close(t.num("B4"), 1000.0, 1e-7);
}

#[test]
fn goal_seek_cubic_root_away_from_start() {
    let mut t = T::new();
    t.set("A1", "0");
    t.set("B1", "=A1^3-2*A1-5");
    let r = t.seek("B1", 0.0, "A1").unwrap();
    assert_close(r.value, 2.0945514815423265, 1e-6);
    assert!(t.num("B1").abs() <= 1e-7);
}

#[test]
fn goal_seek_flat_start() {
    // The derivative of x^3 is zero at the start, so Newton can't take a
    // first step; the bracket scan has to find the root at 10.
    let mut t = T::new();
    t.set("A1", "0");
    t.set("B1", "=A1^3");
    t.seek("B1", 1000.0, "A1").unwrap();
    assert_close(t.num("A1"), 10.0, 1e-9);
    assert_eq!(t.input("A1"), "10");
}

#[test]
fn goal_seek_backs_off_error_values() {
    // SQRT of the starting value is #NUM!; the root is at 4.
    let mut t = T::new();
    t.set("A1", "-5");
    t.set("B1", "=SQRT(A1)");
    t.seek("B1", 2.0, "A1").unwrap();
    assert_close(t.num("A1"), 4.0, 1e-7);

    // A secant step that overshoots into the invalid region (LN of a
    // negative) is halved back.
    t.set("A1", "10");
    t.set("C1", "=LN(A1)");
    t.seek("C1", 0.5, "A1").unwrap();
    assert_close(t.num("A1"), 0.5f64.exp(), 1e-7);
}

#[test]
fn goal_seek_through_intermediate_cells_and_sheets() {
    let mut t = T::new();
    let s2 = t.wb.add_sheet(Some("Calc")).unwrap();
    t.set("A1", "1");
    let calc = Addr::new(s2, CellRef::parse("A1").unwrap());
    t.wb.set_input(calc, "=Sheet1!A1*2+1").unwrap();
    t.set("B1", "=Calc!A1^2");
    t.seek("B1", 49.0, "A1").unwrap();
    assert_close(t.num("A1"), 3.0, 1e-7);
}

#[test]
fn goal_seek_no_solution_restores_input() {
    let mut t = T::new();
    t.set("A1", "3");
    t.set("B1", "=A1^2+1");
    let err = t.seek("B1", 0.0, "A1").unwrap_err();
    assert!(err.contains("couldn't find"), "{err}");
    assert_eq!(t.input("A1"), "3");
    assert_eq!(t.num("B1"), 10.0);
}

#[test]
fn goal_seek_unrelated_cell_explains() {
    let mut t = T::new();
    t.set("A1", "1.5");
    t.set("C1", "7");
    t.set("B1", "=C1*2");
    let err = t.seek("B1", 3.0, "A1").unwrap_err();
    assert!(err.contains("doesn't change"), "{err}");
    assert_eq!(t.input("A1"), "1.5");
}

#[test]
fn goal_seek_discontinuity_is_not_a_root() {
    // A step function changes sign at 1 but never equals 0.
    let mut t = T::new();
    t.set("A1", "-2");
    t.set("B1", "=IF(A1<1,-1,1)");
    assert!(t.seek("B1", 0.0, "A1").is_err());
    assert_eq!(t.input("A1"), "-2");
}

#[test]
fn goal_seek_validates_cells() {
    let mut t = T::new();
    t.set("A1", "1");
    t.set("B1", "5");
    let err = t.seek("B1", 3.0, "A1").unwrap_err();
    assert!(err.contains("B1") && err.contains("formula"), "{err}");

    t.set("C1", "=A1*2");
    t.set("D1", "=C1+1");
    let err = t.seek("D1", 3.0, "C1").unwrap_err();
    assert!(err.contains("C1") && err.contains("constant"), "{err}");

    t.set("E1", "hello");
    let err = t.seek("D1", 3.0, "E1").unwrap_err();
    assert!(err.contains("number"), "{err}");
    assert!(t.seek("C1", f64::NAN, "A1").is_err());
}

#[test]
fn goal_seek_already_at_target() {
    let mut t = T::new();
    t.set("A1", "2");
    t.set("B1", "=A1*3");
    let r = t.seek("B1", 6.0, "A1").unwrap();
    assert_eq!(r.value, 2.0);
    assert_eq!(r.iterations, 1);
}

#[test]
fn goal_seek_empty_changing_cell_starts_at_zero() {
    let mut t = T::new();
    t.set("B1", "=A1*4-2");
    t.seek("B1", 10.0, "A1").unwrap();
    assert_close(t.num("A1"), 3.0, 1e-9);
}

// ========================================================== linear programs

/// Wyndor Glass: maximize 3x + 5y s.t. x <= 4, 2y <= 12, 3x + 2y <= 18.
/// Optimum (2, 6), profit 36.
fn product_mix() -> T {
    let mut t = T::new();
    // Row 1: profit per unit; rows 2-4: resource use per unit; B5:C5 quantities.
    for (row, vals) in [("1", ["3", "5"]), ("2", ["1", "0"]), ("3", ["0", "2"]), ("4", ["3", "2"])] {
        t.set(&format!("B{row}"), vals[0]);
        t.set(&format!("C{row}"), vals[1]);
    }
    t.set("B5", "0");
    t.set("C5", "0");
    t.set("D1", "=SUMPRODUCT(B1:C1,$B$5:$C$5)");
    t.set("D2", "=SUMPRODUCT(B2:C2,$B$5:$C$5)");
    t.set("D3", "=SUMPRODUCT(B3:C3,$B$5:$C$5)");
    t.set("D4", "=SUMPRODUCT(B4:C4,$B$5:$C$5)");
    t
}

#[test]
fn lp_product_mix() {
    let mut t = product_mix();
    let p = t.problem(
        "D1",
        SolverGoal::Max,
        &["B5", "C5"],
        vec![t.con("D2", ConstraintOp::Le, 4.0), t.con("D3", ConstraintOp::Le, 12.0), t.con("D4", ConstraintOp::Le, 18.0)],
    );
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Optimal);
    assert_eq!(r.method, SolverMethod::Linear);
    assert_eq!(r.values, vec![2.0, 6.0]);
    assert_eq!(r.objective, 36.0);
    // Exact values are written back, not 1.9999999999.
    assert_eq!(t.input("B5"), "2");
    assert_eq!(t.input("C5"), "6");
    assert_eq!(t.num("D1"), 36.0);
}

#[test]
fn lp_infeasible_restores_inputs() {
    let mut t = product_mix();
    t.set("B5", "1.5");
    t.set("C5", "");
    // Profit at least 100 is impossible with these resources.
    let p = t.problem(
        "D1",
        SolverGoal::Max,
        &["B5", "C5"],
        vec![t.con("D4", ConstraintOp::Le, 18.0), t.con("D1", ConstraintOp::Ge, 100.0), t.con("D2", ConstraintOp::Le, 4.0)],
    );
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Infeasible);
    assert_eq!(r.method, SolverMethod::Linear);
    assert_eq!(t.input("B5"), "1.5");
    assert_eq!(t.input("C5"), "");
    assert_eq!(r.values, vec![1.5, 0.0]);
    assert_eq!(r.objective, 4.5);
}

#[test]
fn lp_unbounded() {
    let mut t = T::new();
    t.set("A1", "1");
    t.set("A2", "1");
    t.set("B1", "=A1+A2");
    t.set("B2", "=A1-A2");
    let p = t.problem("B1", SolverGoal::Max, &["A1", "A2"], vec![t.con("B2", ConstraintOp::Le, 1.0)]);
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Unbounded);
    assert_eq!(t.input("A1"), "1");
    assert_eq!(t.input("A2"), "1");
}

#[test]
fn lp_diet_minimization() {
    // Two foods costing 2 and 3; nutrient 1 needs x + y >= 4, nutrient 2
    // needs x + 3y >= 6. Optimum (3, 1) at cost 9.
    let mut t = T::new();
    t.set("A1", "2");
    t.set("B1", "3");
    t.set("A2", "1");
    t.set("B2", "1");
    t.set("A3", "1");
    t.set("B3", "3");
    t.set("A5", "10");
    t.set("B5", "10");
    t.set("C1", "=SUMPRODUCT(A1:B1,A5:B5)");
    t.set("C2", "=SUMPRODUCT(A2:B2,A5:B5)");
    t.set("C3", "=SUMPRODUCT(A3:B3,A5:B5)");
    let p = t.problem(
        "C1",
        SolverGoal::Min,
        &["A5", "B5"],
        vec![t.con("C2", ConstraintOp::Ge, 4.0), t.con("C3", ConstraintOp::Ge, 6.0)],
    );
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Optimal);
    assert_eq!(r.method, SolverMethod::Linear);
    assert_eq!(r.values, vec![3.0, 1.0]);
    assert_eq!(r.objective, 9.0);
}

#[test]
fn lp_equality_and_free_variables() {
    // minimize x + 2y with x + y = 1 and y >= -3, variables free:
    // optimum y = -3, x = 4, objective -2.
    let mut t = T::new();
    t.set("C1", "=A1+2*B1");
    t.set("C2", "=A1+B1");
    let mut p = t.problem(
        "C1",
        SolverGoal::Min,
        &["A1", "B1"],
        vec![t.con("C2", ConstraintOp::Eq, 1.0), t.con("B1", ConstraintOp::Ge, -3.0)],
    );
    p.non_negative = false;
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Optimal);
    assert_eq!(r.method, SolverMethod::Linear);
    assert_eq!(r.values, vec![4.0, -3.0]);
    assert_eq!(r.objective, -2.0);
}

#[test]
fn lp_redundant_and_degenerate_constraints() {
    // Duplicate equality rows and constraints that all meet at the optimum.
    let mut t = T::new();
    t.set("C1", "=A1+B1");
    t.set("C2", "=A1+B1");
    t.set("C3", "=2*A1+2*B1");
    t.set("C4", "=A1");
    t.set("C5", "=A1+2*B1");
    let p = t.problem(
        "C5",
        SolverGoal::Max,
        &["A1", "B1"],
        vec![
            t.con("C1", ConstraintOp::Eq, 2.0),
            t.con("C2", ConstraintOp::Eq, 2.0),
            t.con("C3", ConstraintOp::Le, 4.0),
            t.con("C4", ConstraintOp::Le, 0.0),
        ],
    );
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Optimal);
    assert_eq!(r.values, vec![0.0, 2.0]);
    assert_eq!(r.objective, 4.0);
}

#[test]
fn lp_transportation() {
    // 3 plants x 4 cities. Costs in B2:E4, shipments in B7:E9, supply
    // limits per plant (row sums), demand met exactly (column sums).
    // Optimum 1020 (checked against scipy's linprog).
    let mut t = T::new();
    let costs = [[8, 6, 10, 9], [9, 12, 13, 7], [14, 9, 16, 5]];
    let cols = ["B", "C", "D", "E"];
    let mut vars = Vec::new();
    for (i, row) in costs.iter().enumerate() {
        for (j, c) in row.iter().enumerate() {
            t.set(&format!("{}{}", cols[j], i + 2), &c.to_string());
            let v = format!("{}{}", cols[j], i + 7);
            t.set(&v, "0");
            vars.push(v);
        }
        t.set(&format!("F{}", i + 7), &format!("=SUM(B{r}:E{r})", r = i + 7));
    }
    for c in cols {
        t.set(&format!("{c}10"), &format!("=SUM({c}7:{c}9)"));
    }
    t.set("A12", "=SUMPRODUCT(B2:E4,B7:E9)");
    let mut cons = Vec::new();
    for (r, supply) in [(7, 35.0), (8, 50.0), (9, 40.0)] {
        cons.push(t.con(&format!("F{r}"), ConstraintOp::Le, supply));
    }
    for (c, demand) in cols.iter().zip([45.0, 20.0, 30.0, 30.0]) {
        cons.push(t.con(&format!("{c}10"), ConstraintOp::Eq, demand));
    }
    let vars: Vec<&str> = vars.iter().map(String::as_str).collect();
    let p = t.problem("A12", SolverGoal::Min, &vars, cons);
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Optimal);
    assert_eq!(r.method, SolverMethod::Linear);
    assert_eq!(r.objective, 1020.0);
    assert_eq!([t.num("B10"), t.num("C10"), t.num("D10"), t.num("E10")], [45.0, 20.0, 30.0, 30.0]);
    assert!(t.num("F7") <= 35.0 && t.num("F8") <= 50.0 && t.num("F9") <= 40.0);
}

#[test]
fn lp_beale_cycling_example() {
    // Beale's problem cycles forever under the textbook pivoting rule;
    // the anti-cycling fallback must still reach the optimum -0.05.
    let mut t = T::new();
    t.set("E1", "=-0.75*A1+150*B1-0.02*C1+6*D1");
    t.set("E2", "=0.25*A1-60*B1-0.04*C1+9*D1");
    t.set("E3", "=0.5*A1-90*B1-0.02*C1+3*D1");
    let p = t.problem(
        "E1",
        SolverGoal::Min,
        &["A1", "B1", "C1", "D1"],
        vec![t.con("E2", ConstraintOp::Le, 0.0), t.con("E3", ConstraintOp::Le, 0.0), t.con("C1", ConstraintOp::Le, 1.0)],
    );
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Optimal);
    assert_eq!(r.method, SolverMethod::Linear);
    assert_close(r.objective, -0.05, 1e-12);
    assert_eq!(r.values, vec![0.04, 0.0, 1.0, 0.0]);
}

#[test]
fn lp_detects_hidden_nonlinearity() {
    // MAX is linear around the start but not at the simplex answer; the
    // verification step must catch that and fall back to the general method.
    // minimize MAX(x - 2, 2 - x) + y with 0 <= y, x <= 100 -> x = 2, 0.
    let mut t = T::new();
    t.set("A1", "10");
    t.set("B1", "1");
    t.set("C1", "=MAX(A1-2,2-A1)+B1");
    let mut p = t.problem(
        "C1",
        SolverGoal::Min,
        &["A1", "B1"],
        vec![t.con("B1", ConstraintOp::Ge, 0.0), t.con("A1", ConstraintOp::Le, 100.0)],
    );
    p.non_negative = false;
    let r = t.solve(&p);
    assert!(r.status.is_success(), "{:?}", r.status);
    assert_eq!(r.method, SolverMethod::Nonlinear);
    assert_close(r.values[0], 2.0, 1e-5);
    assert!(r.objective < 1e-5, "{}", r.objective);
}

#[test]
fn lp_value_goal() {
    let mut t = T::new();
    t.set("C1", "=2*A1+3*B1");
    let p = t.problem("C1", SolverGoal::Value(12.0), &["A1", "B1"], vec![]);
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Optimal);
    assert_eq!(r.method, SolverMethod::Linear);
    assert_close(t.num("C1"), 12.0, 1e-9);
    assert!(r.values.iter().all(|&v| v >= 0.0));
}

// ======================================================= nonlinear programs

#[test]
fn nlp_unconstrained_quadratic() {
    let mut t = T::new();
    t.set("A1", "0");
    t.set("B1", "0");
    t.set("C1", "=(A1-3)^2+(B1+1)^2+2");
    let mut p = t.problem("C1", SolverGoal::Min, &["A1", "B1"], vec![]);
    p.non_negative = false;
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Optimal);
    assert_eq!(r.method, SolverMethod::Nonlinear);
    assert_close(r.values[0], 3.0, 1e-5);
    assert_close(r.values[1], -1.0, 1e-5);
    assert_close(r.objective, 2.0, 1e-9);
    assert_eq!(t.num("A1"), r.values[0]);
}

#[test]
fn nlp_non_negative_bound_is_active() {
    // Same bowl, but with y >= 0 the best is y = 0, objective 3.
    let mut t = T::new();
    t.set("C1", "=(A1-3)^2+(B1+1)^2+2");
    let p = t.problem("C1", SolverGoal::Min, &["A1", "B1"], vec![]);
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Optimal);
    assert_close(r.values[0], 3.0, 1e-5);
    assert_eq!(r.values[1], 0.0);
    assert_close(r.objective, 3.0, 1e-8);
}

#[test]
fn nlp_constrained_product() {
    // maximize x*y subject to x + y <= 10 -> (5, 5), 25.
    let mut t = T::new();
    t.set("A1", "1");
    t.set("B1", "1");
    t.set("C1", "=A1*B1");
    t.set("C2", "=A1+B1");
    let p = t.problem("C1", SolverGoal::Max, &["A1", "B1"], vec![t.con("C2", ConstraintOp::Le, 10.0)]);
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Optimal);
    assert_eq!(r.method, SolverMethod::Nonlinear);
    assert_close(r.values[0], 5.0, 1e-4);
    assert_close(r.values[1], 5.0, 1e-4);
    assert_close(r.objective, 25.0, 1e-6);
    assert!(t.num("C2") <= 10.0 + 1e-5);
}

#[test]
fn nlp_equality_constraint() {
    // minimize x^2 + y^2 subject to x + 2y = 5 -> (1, 2), 5.
    let mut t = T::new();
    t.set("C1", "=A1^2+B1^2");
    t.set("C2", "=A1+2*B1");
    let mut p = t.problem("C1", SolverGoal::Min, &["A1", "B1"], vec![t.con("C2", ConstraintOp::Eq, 5.0)]);
    p.non_negative = false;
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Optimal);
    assert_close(r.values[0], 1.0, 1e-4);
    assert_close(r.values[1], 2.0, 1e-4);
    assert_close(r.objective, 5.0, 1e-5);
}

#[test]
fn nlp_value_goal() {
    // Make x^3 + x hit 30 -> x = 3.
    let mut t = T::new();
    t.set("A1", "1");
    t.set("B1", "=A1^3+A1");
    let p = t.problem("B1", SolverGoal::Value(30.0), &["A1"], vec![]);
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Optimal);
    assert_eq!(r.method, SolverMethod::Nonlinear);
    assert_close(t.num("B1"), 30.0, 1e-5);
    assert_close(t.num("A1"), 3.0, 1e-5);
}

#[test]
fn nlp_rosenbrock() {
    let mut t = T::new();
    t.set("A1", "-1.2");
    t.set("B1", "1");
    t.set("C1", "=100*(B1-A1^2)^2+(1-A1)^2");
    let mut p = t.problem("C1", SolverGoal::Min, &["A1", "B1"], vec![]);
    p.non_negative = false;
    let r = t.solve(&p);
    assert!(r.status.is_success());
    assert_close(r.values[0], 1.0, 1e-3);
    assert_close(r.values[1], 1.0, 1e-3);
    assert!(r.objective < 1e-6);
}

#[test]
fn nlp_errors_mark_points_invalid() {
    // SQRT is undefined for x < 3; the optimum of sqrt(x-3) + (x-5)^2/10
    // is in the valid region, and the search must not get stuck on errors.
    let mut t = T::new();
    t.set("A1", "4");
    t.set("B1", "=SQRT(A1-3)+(A1-5)^2/10");
    let mut p = t.problem("B1", SolverGoal::Min, &["A1"], vec![]);
    p.non_negative = false;
    let r = t.solve(&p);
    assert!(r.status.is_success());
    // Minimum is at the boundary x = 3 (derivative is +inf just above it
    // and the quadratic pulls the other way only weakly).
    assert!(r.values[0] >= 3.0);
    assert!(r.objective < 0.41, "{}", r.objective);
}

#[test]
fn nlp_unbounded() {
    let mut t = T::new();
    t.set("A1", "1");
    t.set("B1", "1");
    t.set("C1", "=A1*B1");
    let p = t.problem("C1", SolverGoal::Max, &["A1", "B1"], vec![]);
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Unbounded);
    assert_eq!(t.input("A1"), "1");
}

#[test]
fn nlp_infeasible_restores_inputs() {
    // x^2 + y^2 <= 1 and x + y >= 5 can't both hold.
    let mut t = T::new();
    t.set("A1", "0.25");
    t.set("B1", "");
    t.set("C1", "=A1*B1");
    t.set("C2", "=A1^2+B1^2");
    t.set("C3", "=A1+B1");
    let p = t.problem(
        "C1",
        SolverGoal::Max,
        &["A1", "B1"],
        vec![t.con("C2", ConstraintOp::Le, 1.0), t.con("C3", ConstraintOp::Ge, 5.0)],
    );
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Infeasible);
    assert_eq!(t.input("A1"), "0.25");
    assert_eq!(t.input("B1"), "");
    assert_eq!(t.num("C1"), 0.0);
}

// ======================================================== integer programs

#[test]
fn knapsack_binary() {
    // weights 5,4,6,3; values 10,40,30,50; capacity 10. Best: items 2 and 4
    // (value 90); the LP relaxation (105) takes half of item 3.
    let mut t = T::new();
    for (col, (w, v)) in ["A", "B", "C", "D"].iter().zip([(5, 10), (4, 40), (6, 30), (3, 50)]) {
        t.set(&format!("{col}1"), &w.to_string());
        t.set(&format!("{col}2"), &v.to_string());
        t.set(&format!("{col}3"), "0");
    }
    t.set("E1", "=SUMPRODUCT(A1:D1,A3:D3)");
    t.set("E2", "=SUMPRODUCT(A2:D2,A3:D3)");
    let vars = ["A3", "B3", "C3", "D3"];
    let mut cons = vec![t.con("E1", ConstraintOp::Le, 10.0)];
    cons.extend(vars.iter().map(|v| t.con(v, ConstraintOp::Bin, 0.0)));
    let p = t.problem("E2", SolverGoal::Max, &vars, cons);
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Optimal);
    assert_eq!(r.method, SolverMethod::Linear);
    assert_eq!(r.values, vec![0.0, 1.0, 0.0, 1.0]);
    assert_eq!(r.objective, 90.0);
    assert_eq!(t.input("B3"), "1");
    assert_eq!(t.input("C3"), "0");
}

#[test]
fn integer_lp_where_rounding_fails() {
    // maximize 5x + 8y s.t. x + y <= 6, 5x + 9y <= 45. The LP optimum is
    // (2.25, 3.75); rounding gives (2, 4), which is infeasible, and (2, 3)
    // scores 34. The integer optimum is (0, 5) with 40.
    let mut t = T::new();
    t.set("C1", "=5*A1+8*B1");
    t.set("C2", "=A1+B1");
    t.set("C3", "=5*A1+9*B1");
    let p = t.problem(
        "C1",
        SolverGoal::Max,
        &["A1", "B1"],
        vec![
            t.con("C2", ConstraintOp::Le, 6.0),
            t.con("C3", ConstraintOp::Le, 45.0),
            t.con("A1", ConstraintOp::Int, 0.0),
            t.con("B1", ConstraintOp::Int, 0.0),
        ],
    );
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Optimal);
    assert_eq!(r.values, vec![0.0, 5.0]);
    assert_eq!(r.objective, 40.0);

    // Without the integer constraints, the relaxation is fractional.
    let relaxed = SolverProblem { constraints: p.constraints[..2].to_vec(), ..p };
    let r = t.solve(&relaxed);
    assert_close(r.values[0], 2.25, 1e-9);
    assert_close(r.values[1], 3.75, 1e-9);
    assert_close(r.objective, 41.25, 1e-9);
}

#[test]
fn integer_infeasible() {
    // 2x = 3 has no integer solution.
    let mut t = T::new();
    t.set("A1", "7");
    t.set("B1", "=A1");
    t.set("B2", "=2*A1");
    let p = t.problem("B1", SolverGoal::Min, &["A1"], vec![t.con("B2", ConstraintOp::Eq, 3.0), t.con("A1", ConstraintOp::Int, 0.0)]);
    let r = t.solve(&p);
    assert_eq!(r.status, SolverStatus::Infeasible);
    assert_eq!(t.input("A1"), "7");
}

#[test]
fn integer_node_cap_reports_feasible() {
    let mut t = T::new();
    t.set("C1", "=5*A1+8*B1");
    t.set("C2", "=A1+B1");
    t.set("C3", "=5*A1+9*B1");
    let p = t.problem(
        "C1",
        SolverGoal::Max,
        &["A1", "B1"],
        vec![
            t.con("C2", ConstraintOp::Le, 6.0),
            t.con("C3", ConstraintOp::Le, 45.0),
            t.con("A1", ConstraintOp::Int, 0.0),
            t.con("B1", ConstraintOp::Int, 0.0),
        ],
    );
    let opts = SolverOptions { max_subproblems: 5, ..SolverOptions::default() };
    let r = solve(&mut t.wb, &p, &opts).unwrap();
    // Depth-first reaches an integer point, (1, 4), at the fifth node but
    // can't prove it optimal within the cap.
    assert_eq!(r.status, SolverStatus::Feasible);
    assert!(t.num("C2") <= 6.0 && t.num("C3") <= 45.0);
    assert!(r.values.iter().all(|v| v.fract() == 0.0));
}

#[test]
fn integer_nonlinear() {
    // minimize (x - 2.6)^2 + (y - 1.2)^2 over integers -> (3, 1).
    let mut t = T::new();
    t.set("C1", "=(A1-2.6)^2+(B1-1.2)^2");
    let p = t.problem(
        "C1",
        SolverGoal::Min,
        &["A1", "B1"],
        vec![t.con("A1", ConstraintOp::Int, 0.0), t.con("B1", ConstraintOp::Int, 0.0)],
    );
    let r = t.solve(&p);
    assert!(r.status.is_success(), "{:?}", r.status);
    assert_eq!(r.method, SolverMethod::Nonlinear);
    assert_eq!(r.values, vec![3.0, 1.0]);
    assert_close(r.objective, 0.2, 1e-9);
}

// ============================================================= validation

#[test]
fn solver_rejects_invalid_setups() {
    let mut t = product_mix();
    let ok = t.problem("D1", SolverGoal::Max, &["B5", "C5"], vec![t.con("D4", ConstraintOp::Le, 18.0)]);

    let p = SolverProblem { objective: t.a("B1"), ..ok.clone() };
    let err = solve(&mut t.wb, &p, &SolverOptions::default()).unwrap_err();
    assert!(err.contains("objective") && err.contains("formula"), "{err}");

    let p = SolverProblem { variables: vec![t.a("B5"), t.a("D2")], ..ok.clone() };
    let err = solve(&mut t.wb, &p, &SolverOptions::default()).unwrap_err();
    assert!(err.contains("D2") && err.contains("formula"), "{err}");

    let p = SolverProblem { variables: vec![t.a("B5"), t.a("B5")], ..ok.clone() };
    assert!(solve(&mut t.wb, &p, &SolverOptions::default()).unwrap_err().contains("more than once"));

    let p = SolverProblem { variables: vec![], ..ok.clone() };
    assert!(solve(&mut t.wb, &p, &SolverOptions::default()).is_err());

    t.set("F1", "text");
    let p = SolverProblem { constraints: vec![t.con("F1", ConstraintOp::Le, 1.0)], ..ok.clone() };
    let err = solve(&mut t.wb, &p, &SolverOptions::default()).unwrap_err();
    assert!(err.contains("F1") && err.contains("number"), "{err}");

    t.set("F2", "=1/0");
    let p = SolverProblem { constraints: vec![t.con("F2", ConstraintOp::Le, 1.0)], ..ok.clone() };
    let err = solve(&mut t.wb, &p, &SolverOptions::default()).unwrap_err();
    assert!(err.contains("#DIV/0!"), "{err}");

    let p = SolverProblem { constraints: vec![t.con("D2", ConstraintOp::Int, 0.0)], ..ok.clone() };
    let err = solve(&mut t.wb, &p, &SolverOptions::default()).unwrap_err();
    assert!(err.contains("variable"), "{err}");

    t.set("G1", "=B5/0");
    let p = SolverProblem { objective: t.a("G1"), ..ok.clone() };
    assert!(solve(&mut t.wb, &p, &SolverOptions::default()).is_err());

    // Nothing was changed by any of the rejected runs.
    assert_eq!(t.input("B5"), "0");
    assert_eq!(t.input("C5"), "0");
}

#[test]
fn solver_is_deterministic() {
    let run = || {
        let mut t = T::new();
        t.set("A1", "0.5");
        t.set("B1", "0.5");
        t.set("C1", "=(A1^2-1)^2+(B1^2-4)^2+0.3*A1-0.2*B1");
        let mut p = t.problem("C1", SolverGoal::Min, &["A1", "B1"], vec![]);
        p.non_negative = false;
        t.solve(&p)
    };
    assert_eq!(run(), run());
}
