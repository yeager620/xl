//! Goal Seek and Solver: what-if analysis that drives input cells until
//! formulas meet a goal.
//!
//! Both treat the workbook as a black box: write numbers into the changing
//! cells, let the incremental recalc run, read the result cells. That keeps
//! them independent of the formula language (any function, any chain of
//! intermediate cells, cross-sheet references) at the cost of one recalc per
//! evaluation, which is cheap for the model sizes people build by hand.
//!
//! On success the found values are left in the changing cells; on failure the
//! cells get their original inputs (and formats) back, so a failed run never
//! leaves the sheet in a half-searched state.

use crate::address::Addr;
use crate::formula::quote_sheet_name;
use crate::store::CellInput;
use crate::value::Value;
use crate::workbook::{CellSnapshot, Workbook, precise};

// ================================================================== shared

/// Outputs this large (after scaling by the starting objective) are taken
/// to mean the objective is running off to infinity.
const UNBOUNDED: f64 = 1e14;

/// Evaluates the workbook as a function of a set of input cells.
struct Probe<'w> {
    wb: &'w mut Workbook,
    vars: Vec<Addr>,
    outs: Vec<Addr>,
    evals: usize,
}

impl Probe<'_> {
    /// Write `x` into the variable cells and read every output cell. `None`
    /// when any output is an error or text: the point is outside the
    /// model's domain and the search must back off from it.
    fn eval(&mut self, x: &[f64]) -> Option<Vec<f64>> {
        self.evals += 1;
        if x.iter().any(|v| !v.is_finite()) {
            return None;
        }
        let items: Vec<(Addr, String)> = self.vars.iter().zip(x).map(|(&a, &v)| (a, exact(v))).collect();
        self.wb.set_inputs(items);
        self.outs.iter().map(|&a| number(self.wb.value(a))).collect()
    }
}

/// Shortest text that parses back to exactly `v`. Searches write these
/// instead of `precise` (15 significant digits) so finite differences and
/// bracket endpoints aren't blurred by rounding; only the final answer is
/// rounded for display.
fn exact(v: f64) -> String {
    format!("{v:?}")
}

/// A cell value as a number, the way the solver sees it: blank counts as
/// zero (as in Excel), anything else non-numeric is invalid.
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) if n.is_finite() => Some(*n),
        Value::Empty => Some(0.0),
        _ => None,
    }
}

/// `B3`, or `Sheet2!B3` when the workbook has more than one sheet.
fn label(wb: &Workbook, a: Addr) -> String {
    if wb.sheet_ids().len() > 1 {
        format!("{}!{}", quote_sheet_name(wb.sheet_name(a.sheet)), a.cell)
    } else {
        a.cell.to_string()
    }
}

fn check_sheet(wb: &Workbook, a: Addr) -> Result<(), String> {
    if wb.sheet_ids().contains(&a.sheet) { Ok(()) } else { Err("A cell refers to a sheet that no longer exists.".into()) }
}

/// The number in a cell the search is allowed to change: it must hold a
/// constant (blank counts as 0), never a formula, which would be destroyed.
fn constant_value(wb: &Workbook, a: Addr, role: &str) -> Result<f64, String> {
    check_sheet(wb, a)?;
    match wb.cell(a).map(|c| &c.input) {
        None | Some(CellInput::Value(Value::Empty)) => Ok(0.0),
        Some(CellInput::Value(Value::Number(n))) => Ok(*n),
        Some(CellInput::Formula(_) | CellInput::Opaque(_) | CellInput::Spill(_)) => {
            Err(format!("The {role} {} must contain a constant value, not a formula.", label(wb, a)))
        }
        Some(CellInput::Value(other)) => {
            Err(format!("The {role} {} must contain a number (it contains \"{}\").", label(wb, a), other.display()))
        }
    }
}

fn require_formula(wb: &Workbook, a: Addr, role: &str) -> Result<f64, String> {
    check_sheet(wb, a)?;
    if !wb.cell(a).is_some_and(|c| c.is_formula()) {
        return Err(format!("The {role} {} must contain a formula.", label(wb, a)));
    }
    number(wb.value(a)).ok_or_else(|| {
        format!("The {role} {} must evaluate to a number (it shows {}).", label(wb, a), wb.value(a).display())
    })
}

fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    // Not f64::clamp: that panics on NaN bounds, and a NaN here should just
    // propagate into an invalid evaluation.
    v.max(lo).min(hi)
}

fn clamp_all(x: &[f64], lo: &[f64], hi: &[f64]) -> Vec<f64> {
    x.iter().zip(lo).zip(hi).map(|((&v, &l), &h)| clamp(v, l, h)).collect()
}

/// splitmix64: tiny, seedable, and good enough to scatter probe points.
/// Seeded so that the same model always gives the same answer.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn uniform(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * ((self.next() >> 11) as f64 / (1u64 << 53) as f64)
    }
}

// =============================================================== goal seek

#[derive(Clone, Debug, PartialEq)]
pub struct GoalSeekOptions {
    /// Evaluation budget for the local (secant) and bracketed (Brent)
    /// phases. The outward bracket scan in between adds at most 120 more, and
    /// polishing an accepted answer at most 6.
    pub max_iterations: usize,
    /// Accept when |set cell - target| <= tolerance * max(1, |target|).
    pub tolerance: f64,
}

impl Default for GoalSeekOptions {
    fn default() -> Self {
        Self { max_iterations: 100, tolerance: 1e-7 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GoalSeekResult {
    /// The value left in the changing cell.
    pub value: f64,
    /// What the set cell evaluates to with that value.
    pub achieved: f64,
    /// Workbook evaluations used.
    pub iterations: usize,
}

/// Steps in the outward bracket scan, per direction. Doubling from 1% of the
/// start covers ~1e16 times the starting magnitude.
const SCAN_STEPS: i32 = 60;
/// Extra evaluations allowed for refining an accepted root.
const POLISH_STEPS: usize = 6;

/// Find a value for `changing` that makes the formula in `set_cell` equal
/// `target`, like Excel's Goal Seek.
///
/// Strategy: secant steps from the current value (fast when the start is
/// close), then — if those stall or the function is flat there — an outward
/// scan for a sign change, then Brent's method inside the bracket, which
/// cannot diverge. Points where the model errors (#NUM!, #DIV/0!, ...) are
/// treated as outside the domain: the search steps back or around them.
pub fn goal_seek(
    wb: &mut Workbook,
    set_cell: Addr,
    target: f64,
    changing: Addr,
    opts: &GoalSeekOptions,
) -> Result<GoalSeekResult, String> {
    if !target.is_finite() {
        return Err("The target value must be a finite number.".into());
    }
    let x0 = constant_value(wb, changing, "changing cell")?;
    check_sheet(wb, set_cell)?;
    if !wb.cell(set_cell).is_some_and(|c| c.is_formula()) {
        return Err(format!("The set cell {} must contain a formula.", label(wb, set_cell)));
    }
    let (set_name, changing_name) = (label(wb, set_cell), label(wb, changing));
    let original = wb.snapshot(changing);
    let tol = opts.tolerance * target.abs().max(1.0);

    let mut probe = Probe { wb: &mut *wb, vars: vec![changing], outs: vec![set_cell], evals: 0 };
    let mut f = |x: f64| probe.eval(&[x]).map(|v| v[0] - target);
    let mut search = RootSearch {
        f: &mut f,
        tol,
        budget: opts.max_iterations + 2 * SCAN_STEPS as usize,
        evals: 0,
        seen: Vec::new(),
    };
    let outcome = search.run(x0, (opts.max_iterations / 2).max(10));
    let iterations = search.evals;

    match outcome {
        Ok(x) => {
            // Store the rounded form the user will see in the formula bar,
            // unless rounding alone breaks the tolerance (very steep models).
            let mut value: f64 = precise(x).parse().unwrap_or(x);
            wb.set_inputs([(changing, precise(value))]);
            let mut achieved = number(wb.value(set_cell)).unwrap_or(f64::NAN);
            let within = (achieved - target).abs() <= tol; // false for NaN too
            if !within {
                value = x;
                wb.set_inputs([(changing, exact(x))]);
                achieved = number(wb.value(set_cell)).unwrap_or(f64::NAN);
            }
            Ok(GoalSeekResult { value, achieved, iterations })
        }
        Err(why) => {
            wb.restore(vec![(changing, original)]);
            let target_text = precise(target);
            Err(match why {
                SeekFailure::AlwaysError => format!(
                    "Goal Seek couldn't find a solution: {set_name} returned an error for every value of {changing_name} tried."
                ),
                SeekFailure::NoEffect => format!(
                    "Goal Seek couldn't find a solution: {set_name} doesn't change when {changing_name} changes."
                ),
                SeekFailure::NoSolution(Some((x, fx))) => format!(
                    "Goal Seek couldn't find a value of {changing_name} that makes {set_name} equal {target_text}. \
                     The closest was {} at {changing_name} = {}.",
                    precise(fx + target),
                    precise(x)
                ),
                SeekFailure::NoSolution(None) => {
                    format!("Goal Seek couldn't find a value of {changing_name} that makes {set_name} equal {target_text}.")
                }
            })
        }
    }
}

enum SeekFailure {
    AlwaysError,
    NoEffect,
    /// Best point seen (x, f(x) - target), if any was valid.
    NoSolution(Option<(f64, f64)>),
}

/// One-dimensional root search for f(x) = 0 where f may be undefined
/// (`None`) at some points.
struct RootSearch<'f> {
    f: &'f mut dyn FnMut(f64) -> Option<f64>,
    tol: f64,
    budget: usize,
    evals: usize,
    /// Every valid point evaluated, used to spot sign changes wherever they
    /// turn up (a wild secant overshoot is still a useful bracket end).
    seen: Vec<(f64, f64)>,
}

impl RootSearch<'_> {
    fn eval(&mut self, x: f64) -> Option<f64> {
        if self.evals >= self.budget || !x.is_finite() {
            return None;
        }
        self.evals += 1;
        let fx = (self.f)(x).filter(|v| v.is_finite())?;
        self.seen.push((x, fx));
        Some(fx)
    }

    fn exhausted(&self) -> bool {
        self.evals >= self.budget
    }

    fn best(&self) -> Option<(f64, f64)> {
        self.seen.iter().copied().min_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
    }

    /// Neighbouring points (in x order) with opposite signs, preferring the
    /// pair whose better end is closest to zero.
    fn bracket(&self) -> Option<((f64, f64), (f64, f64))> {
        let mut pts = self.seen.clone();
        pts.sort_by(|a, b| a.0.total_cmp(&b.0));
        pts.windows(2)
            .filter(|w| (w[0].1 < 0.0 && w[1].1 > 0.0) || (w[0].1 > 0.0 && w[1].1 < 0.0))
            .map(|w| (w[0], w[1]))
            .min_by(|a, b| a.0.1.abs().min(a.1.1.abs()).total_cmp(&b.0.1.abs().min(b.1.1.abs())))
    }

    fn run(&mut self, x0: f64, local_budget: usize) -> Result<f64, SeekFailure> {
        let found = self.find(x0, local_budget)?;
        Ok(self.polish(found))
    }

    /// Once within tolerance, a few more secant steps through the two best
    /// points usually reach full precision. Cheap, and worth it: people read
    /// the answer, and 10 beats 9.99999999904.
    fn polish(&mut self, found: f64) -> f64 {
        self.budget = self.evals + POLISH_STEPS;
        for _ in 0..POLISH_STEPS {
            let mut pts = self.seen.clone();
            pts.sort_by(|a, b| a.1.abs().total_cmp(&b.1.abs()));
            let Some(&(xb, fb)) = pts.first() else { break };
            let Some(&(xs, fs)) = pts.iter().find(|p| p.0 != xb && p.1 != fb) else { break };
            if fb == 0.0 {
                break;
            }
            let xn = xb - fb * (xb - xs) / (fb - fs);
            if xn == xb || !matches!(self.eval(xn), Some(f) if f.abs() < fb.abs()) {
                break;
            }
        }
        self.best().map_or(found, |b| if b.1.abs() <= self.tol { b.0 } else { found })
    }

    fn find(&mut self, x0: f64, local_budget: usize) -> Result<f64, SeekFailure> {
        if let Some(x) = self.secant(x0, local_budget) {
            return Ok(x);
        }
        if self.bracket().is_none() {
            let center = self.best().map_or(x0, |b| b.0);
            if let Some(x) = self.scan(center) {
                return Ok(x);
            }
        }
        if let Some((a, b)) = self.bracket()
            && let Some(x) = self.brent(a, b)
        {
            return Ok(x);
        }
        Err(if self.seen.is_empty() {
            SeekFailure::AlwaysError
        } else if self.seen.len() > 1 && self.seen.iter().all(|p| p.1 == self.seen[0].1) {
            SeekFailure::NoEffect
        } else {
            SeekFailure::NoSolution(self.best())
        })
    }

    /// Damped secant iteration. Returns a root, or None to hand over to the
    /// bracketing phases (start invalid, slope flat, no progress, or a sign
    /// change found — Brent is then strictly safer).
    fn secant(&mut self, x0: f64, budget: usize) -> Option<f64> {
        let start = self.evals;
        let mut fx = self.eval(x0)?;
        if fx.abs() <= self.tol {
            return Some(x0);
        }
        let mut x = x0;
        // The first slope is a forward difference; a relative step keeps it
        // meaningful for rates (0.05) and principals (200000) alike.
        let h = 1e-6 * x.abs().max(1e-2);
        let (mut xp, mut fp) = match self.eval(x + h) {
            Some(v) => (x + h, v),
            None => (x - h, self.eval(x - h)?),
        };
        if fp.abs() <= self.tol {
            return Some(xp);
        }
        while self.evals - start < budget {
            if self.bracket().is_some() {
                return None;
            }
            let slope = (fx - fp) / (x - xp);
            if !slope.is_finite() || slope == 0.0 {
                return None;
            }
            let mut step = -fx / slope;
            let mut accepted = false;
            for _ in 0..8 {
                let xn = x + step;
                if let Some(fnew) = self.eval(xn) {
                    if fnew.abs() <= self.tol {
                        return Some(xn);
                    }
                    if fnew.abs() < fx.abs() {
                        (xp, fp, x, fx) = (x, fx, xn, fnew);
                        accepted = true;
                        break;
                    }
                }
                if self.bracket().is_some() || self.evals - start >= budget {
                    return None;
                }
                // Overshoot or an error value: back off toward x.
                step *= 0.5;
            }
            if !accepted {
                return None;
            }
        }
        None
    }

    /// Step outward from `center` in both directions with doubling steps
    /// until the function changes sign (or hits the target). Errors don't
    /// stop a direction: SQRT-like models are invalid on one side of the
    /// start and fine further along.
    fn scan(&mut self, center: f64) -> Option<f64> {
        let base = 0.01 * center.abs().max(1.0);
        let mut alive = [true, true];
        for k in 0..SCAN_STEPS {
            let d = base * 2f64.powi(k);
            for (i, dir) in [1.0, -1.0].into_iter().enumerate() {
                let x = center + dir * d;
                if !alive[i] {
                    continue;
                }
                if !x.is_finite() || x.abs() > 1e300 {
                    alive[i] = false;
                    continue;
                }
                if let Some(fx) = self.eval(x)
                    && fx.abs() <= self.tol
                {
                    return Some(x);
                }
                if self.bracket().is_some() || self.exhausted() {
                    return None;
                }
            }
        }
        None
    }

    /// Brent's method (inverse quadratic interpolation with bisection
    /// fallback) on a bracket with f(a), f(b) of opposite signs. If a trial
    /// point errors, bisect instead; if the bracket collapses without |f|
    /// reaching tolerance, the sign change was a discontinuity (e.g. 1/x at
    /// 0), not a root.
    fn brent(&mut self, (mut a, mut fa): (f64, f64), (mut b, mut fb): (f64, f64)) -> Option<f64> {
        let (mut c, mut fc) = (b, fb);
        let mut d = b - a;
        let mut e = d;
        loop {
            if (fb > 0.0) == (fc > 0.0) {
                (c, fc) = (a, fa);
                d = b - a;
                e = d;
            }
            if fc.abs() < fb.abs() {
                (a, fa) = (b, fb);
                (b, fb) = (c, fc);
                (c, fc) = (a, fa);
            }
            let tol1 = 2.0 * f64::EPSILON * b.abs() + 1e-300;
            let xm = 0.5 * (c - b);
            if fb.abs() <= self.tol {
                return Some(b);
            }
            if xm.abs() <= tol1 {
                return None;
            }
            if e.abs() >= tol1 && fa.abs() > fb.abs() {
                let s = fb / fa;
                let (mut p, mut q);
                if a == c {
                    p = 2.0 * xm * s;
                    q = 1.0 - s;
                } else {
                    let qq = fa / fc;
                    let r = fb / fc;
                    p = s * (2.0 * xm * qq * (qq - r) - (b - a) * (r - 1.0));
                    q = (qq - 1.0) * (r - 1.0) * (s - 1.0);
                }
                if p > 0.0 {
                    q = -q;
                }
                p = p.abs();
                if 2.0 * p < (3.0 * xm * q - (tol1 * q).abs()).min((e * q).abs()) {
                    e = d;
                    d = p / q;
                } else {
                    d = xm;
                    e = d;
                }
            } else {
                d = xm;
                e = d;
            }
            (a, fa) = (b, fb);
            b += if d.abs() > tol1 { d } else { tol1.copysign(xm) };
            fb = match self.eval(b) {
                Some(v) => v,
                None => {
                    if self.exhausted() {
                        return None;
                    }
                    b = 0.5 * (a + c);
                    d = b - a;
                    e = d;
                    self.eval(b)?
                }
            };
        }
    }
}

// ================================================================== solver

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SolverGoal {
    Max,
    Min,
    /// Make the objective equal this value.
    Value(f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstraintOp {
    Le,
    Ge,
    Eq,
    /// The (variable) cell must be a whole number.
    Int,
    /// The (variable) cell must be 0 or 1.
    Bin,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Constraint {
    pub cell: Addr,
    pub op: ConstraintOp,
    /// Ignored for `Int` and `Bin`.
    pub rhs: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SolverProblem {
    pub objective: Addr,
    pub goal: SolverGoal,
    pub variables: Vec<Addr>,
    pub constraints: Vec<Constraint>,
    /// Make unconstrained variables non-negative (Excel's checkbox).
    pub non_negative: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SolverOptions {
    /// Per-subproblem cap: simplex pivots for linear models, Nelder–Mead
    /// iterations per inner minimization for nonlinear ones.
    pub max_iterations: usize,
    /// Relative tolerance for constraint satisfaction and convergence
    /// (a constraint `c <= r` holds when c - r <= tolerance * max(1, |r|)).
    pub tolerance: f64,
    /// How far from a whole number an integer variable may be.
    pub integer_tolerance: f64,
    /// Branch-and-bound node cap; when hit, the best integer solution found
    /// so far is returned as `Feasible`.
    pub max_subproblems: usize,
    /// Extra starting points for nonlinear models, besides the current
    /// values.
    pub multistart: usize,
    /// Seed for the starting-point and linearity-probe generator.
    pub seed: u64,
}

impl Default for SolverOptions {
    fn default() -> Self {
        Self {
            max_iterations: 5000,
            tolerance: 1e-6,
            integer_tolerance: 1e-6,
            max_subproblems: 2000,
            multistart: 4,
            seed: 0x5EED_CE11,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SolverStatus {
    /// Converged: the exact optimum for linear models, a (local) optimum
    /// for nonlinear ones.
    Optimal,
    /// Stopped early (iteration or node limit) at a point that satisfies
    /// every constraint.
    Feasible,
    /// No point satisfying the constraints was found. Proven for linear
    /// models; for nonlinear ones it means none was found from any start.
    Infeasible,
    /// The objective can be improved without limit.
    Unbounded,
    Failed(String),
}

impl SolverStatus {
    /// Whether the variable cells were left holding a solution.
    pub fn is_success(&self) -> bool {
        matches!(self, SolverStatus::Optimal | SolverStatus::Feasible)
    }
}

/// Which engine solved the problem (decided automatically).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SolverMethod {
    /// Simplex (with branch-and-bound for integer variables).
    Linear,
    /// Augmented Lagrangian around multi-start Nelder–Mead.
    Nonlinear,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SolverResult {
    pub status: SolverStatus,
    /// Final variable values (the originals when the solve failed).
    pub values: Vec<f64>,
    /// The objective cell's value at `values`.
    pub objective: f64,
    /// Simplex pivots (linear) or Nelder–Mead iterations (nonlinear),
    /// summed over all subproblems.
    pub iterations: usize,
    pub method: SolverMethod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rel {
    Le,
    Ge,
    Eq,
}

impl Rel {
    fn flip(self) -> Self {
        match self {
            Rel::Le => Rel::Ge,
            Rel::Ge => Rel::Le,
            Rel::Eq => Rel::Eq,
        }
    }
}

/// A constraint on output `out` (index into the probe's output cells).
struct Row {
    out: usize,
    rel: Rel,
    rhs: f64,
}

/// The validated problem in solver terms: minimize `sense * f` where `f`
/// is output 0, subject to `rows` and box bounds.
struct Spec {
    /// +1 minimize, -1 maximize, 0 for a Value goal (pure feasibility: the
    /// target becomes an equality row, so one code path serves all goals).
    sense: f64,
    rows: Vec<Row>,
    lo: Vec<f64>,
    hi: Vec<f64>,
    integer: Vec<bool>,
    x0: Vec<f64>,
    /// Output values at x0.
    f0: Vec<f64>,
}

impl Spec {
    /// Largest relative violation over all rows, given output values.
    fn violation(&self, out: &[f64]) -> f64 {
        self.rows.iter().map(|r| row_excess(r, out[r.out]).max(0.0)).fold(0.0, f64::max)
    }
}

/// Signed relative excess of a row: <= 0 when satisfied (inequalities);
/// for equalities, its absolute value is the violation.
fn row_excess(r: &Row, v: f64) -> f64 {
    let s = r.rhs.abs().max(1.0);
    match r.rel {
        Rel::Le => (v - r.rhs) / s,
        Rel::Ge => (r.rhs - v) / s,
        Rel::Eq => ((v - r.rhs) / s).abs(),
    }
}

fn prepare(wb: &Workbook, p: &SolverProblem) -> Result<(Vec<Addr>, Spec), String> {
    let f_obj = require_formula(wb, p.objective, "objective cell")?;
    if p.variables.is_empty() {
        return Err("Choose at least one variable cell.".into());
    }
    let n = p.variables.len();
    let mut x0 = Vec::with_capacity(n);
    for (i, &v) in p.variables.iter().enumerate() {
        if p.variables[..i].contains(&v) {
            return Err(format!("The variable cell {} is listed more than once.", label(wb, v)));
        }
        x0.push(constant_value(wb, v, "variable cell")?);
    }
    let sense = match p.goal {
        SolverGoal::Max => -1.0,
        SolverGoal::Min => 1.0,
        SolverGoal::Value(t) if t.is_finite() => 0.0,
        SolverGoal::Value(_) => return Err("The target value must be a finite number.".into()),
    };
    let base_lo = if p.non_negative { 0.0 } else { f64::NEG_INFINITY };
    let mut lo = vec![base_lo; n];
    let mut hi = vec![f64::INFINITY; n];
    let mut integer = vec![false; n];
    let mut outs = vec![p.objective];
    let mut f0 = vec![f_obj];
    let mut rows = Vec::new();
    if let SolverGoal::Value(t) = p.goal {
        rows.push(Row { out: 0, rel: Rel::Eq, rhs: t });
    }
    for c in &p.constraints {
        check_sheet(wb, c.cell)?;
        let rel = match c.op {
            ConstraintOp::Int | ConstraintOp::Bin => {
                let Some(j) = p.variables.iter().position(|&v| v == c.cell) else {
                    return Err(format!(
                        "Integer and binary constraints apply to variable cells; {} is not one.",
                        label(wb, c.cell)
                    ));
                };
                integer[j] = true;
                if c.op == ConstraintOp::Bin {
                    lo[j] = lo[j].max(0.0);
                    hi[j] = hi[j].min(1.0);
                }
                continue;
            }
            ConstraintOp::Le => Rel::Le,
            ConstraintOp::Ge => Rel::Ge,
            ConstraintOp::Eq => Rel::Eq,
        };
        if !c.rhs.is_finite() {
            return Err(format!("The constraint on {} needs a finite right-hand side.", label(wb, c.cell)));
        }
        let Some(v) = number(wb.value(c.cell)) else {
            return Err(format!(
                "The constraint cell {} must evaluate to a number (it shows {}).",
                label(wb, c.cell),
                wb.value(c.cell).display()
            ));
        };
        let out = match outs.iter().position(|&o| o == c.cell) {
            Some(k) => k,
            None => {
                outs.push(c.cell);
                f0.push(v);
                outs.len() - 1
            }
        };
        rows.push(Row { out, rel, rhs: c.rhs });
    }
    Ok((outs, Spec { sense, rows, lo, hi, integer, x0, f0 }))
}

/// Run the Solver on `p`.
///
/// The model is first probed for linearity (objective and constraint cells
/// affine in the variables). Linear models — the common business case —
/// are solved exactly with a two-phase simplex; others with an augmented
/// Lagrangian around Nelder–Mead from several starting points. Integer and
/// binary variables are handled by branch-and-bound over either.
///
/// `Err` is for invalid setups (nothing is changed). Otherwise the result's
/// status says what happened: on `Optimal`/`Feasible` the solution is left in
/// the variable cells, on anything else their original inputs are restored.
pub fn solve(wb: &mut Workbook, p: &SolverProblem, opts: &SolverOptions) -> Result<SolverResult, String> {
    let (outs, spec) = prepare(wb, p)?;
    let originals: Vec<(Addr, CellSnapshot)> = p.variables.iter().map(|&a| (a, wb.snapshot(a))).collect();

    let outcome = {
        let mut probe = Probe { wb: &mut *wb, vars: p.variables.clone(), outs, evals: 0 };
        let mut rng = Rng(opts.seed);
        let linear = linear_model(&mut probe, &spec, &mut rng).and_then(|aff| {
            let o = solve_linear(&aff, &spec, opts);
            // The linearity probe samples only a few points; a model that is
            // linear there but not elsewhere (IF, MAX, ROUND...) shows up as
            // a mismatch at the simplex answer, and goes to the general path.
            let trusted = !o.status.is_success()
                || aff.agrees(&mut probe, &spec, &finalize(&o.x, &spec.integer), opts.tolerance);
            trusted.then_some(o)
        });
        linear.unwrap_or_else(|| solve_nonlinear(&mut probe, &spec, opts, &mut rng))
    };

    if outcome.status.is_success() {
        let values = finalize(&outcome.x, &spec.integer);
        wb.set_inputs(p.variables.iter().zip(&values).map(|(&a, &v)| (a, precise(v))));
        let objective = number(wb.value(p.objective)).unwrap_or(f64::NAN);
        Ok(SolverResult { status: outcome.status, values, objective, iterations: outcome.iterations, method: outcome.method })
    } else {
        wb.restore(originals);
        Ok(SolverResult {
            status: outcome.status,
            values: spec.x0.clone(),
            objective: spec.f0[0],
            iterations: outcome.iterations,
            method: outcome.method,
        })
    }
}

/// The values to store: integers exactly whole, values within rounding
/// noise of a whole number snapped to it (the simplex reports 3.9999999999
/// for 4), and everything rounded to the 15 digits the cell will show, so
/// `SolverResult::values` matches the cells exactly.
fn finalize(x: &[f64], integer: &[bool]) -> Vec<f64> {
    x.iter()
        .zip(integer)
        .map(|(&v, &int)| {
            let r = v.round();
            let v = if int || (v - r).abs() <= 1e-9 * v.abs().max(1.0) { r } else { v };
            precise(v).parse().unwrap_or(v)
        })
        .collect()
}

struct Outcome {
    status: SolverStatus,
    x: Vec<f64>,
    iterations: usize,
    method: SolverMethod,
}

// ------------------------------------------------------------- linearity

/// Every output as an affine function of the variables: out_k = constant_k
/// + coef_k · x.
struct Affine {
    coef: Vec<Vec<f64>>,
    constant: Vec<f64>,
}

impl Affine {
    fn predict(&self, k: usize, x: &[f64]) -> f64 {
        self.constant[k] + self.coef[k].iter().zip(x).map(|(c, v)| c * v).sum::<f64>()
    }

    /// Does the real model match the affine one at `x` and satisfy the
    /// constraints there?
    fn agrees(&self, probe: &mut Probe, spec: &Spec, x: &[f64], tol: f64) -> bool {
        let Some(out) = probe.eval(x) else { return false };
        let matches = out.iter().enumerate().all(|(k, &v)| (self.predict(k, x) - v).abs() <= 1e-6 * (1.0 + v.abs()));
        matches && spec.violation(&out) <= 10.0 * tol
    }
}

/// Probe whether every output is affine in the variables: fit coefficients
/// by finite differences at the start, then check the fit at a few seeded
/// random points spread over a wide range. Returns `None` for nonlinear
/// models or if any probe point errors.
fn linear_model(probe: &mut Probe, spec: &Spec, rng: &mut Rng) -> Option<Affine> {
    let n = spec.x0.len();
    let x0 = clamp_all(&spec.x0, &spec.lo, &spec.hi);
    let base = probe.eval(&x0)?;
    let k_outs = base.len();
    let mut coef = vec![vec![0.0; n]; k_outs];
    for j in 0..n {
        // A unit-scale step: for a truly linear model any step is exact, and
        // a large one keeps cancellation error small relative to the slope.
        let mut h = x0[j].abs().max(1.0);
        if x0[j] + h > spec.hi[j] && x0[j] - h >= spec.lo[j] {
            h = -h;
        }
        let mut x = x0.clone();
        x[j] += h;
        let out = probe.eval(&x)?;
        for k in 0..k_outs {
            let diff = out[k] - base[k];
            // Treat differences at rounding level as exact zeros, and round
            // to 12 significant digits so a coefficient of 3 isn't
            // 2.9999999999996 (which would leak into the answer).
            coef[k][j] =
                if diff.abs() <= 1e-13 * (out[k].abs() + base[k].abs()) { 0.0 } else { round_sig(diff / h, 12) };
        }
    }
    let constant: Vec<f64> =
        (0..k_outs).map(|k| base[k] - coef[k].iter().zip(&x0).map(|(c, v)| c * v).sum::<f64>()).collect();
    let aff = Affine { coef, constant };

    for _ in 0..3 {
        let y: Vec<f64> = (0..n)
            .map(|j| {
                let scale = x0[j].abs().max(1.0) * 3.0;
                let (l, h) = (spec.lo[j], spec.hi[j]);
                if l.is_finite() && h.is_finite() {
                    rng.uniform(l, h)
                } else {
                    let mut v = x0[j] + rng.uniform(-scale, scale);
                    if v < l {
                        v = l + (l - v); // reflect into the domain
                    }
                    clamp(v, l, h)
                }
            })
            .collect();
        let out = probe.eval(&y)?;
        for (k, &actual) in out.iter().enumerate() {
            let spread: f64 = aff.coef[k].iter().zip(y.iter().zip(&x0)).map(|(c, (a, b))| (c * (a - b)).abs()).sum();
            let allowed = 1e-8 * (1.0 + actual.abs() + base[k].abs() + spread);
            if (aff.predict(k, &y) - actual).abs() > allowed {
                return None;
            }
        }
    }
    Some(aff)
}

fn round_sig(v: f64, digits: i32) -> f64 {
    if v == 0.0 || !v.is_finite() {
        return v;
    }
    let mag = v.abs().log10().floor() as i32;
    let scale = 10f64.powi(digits - 1 - mag);
    if !scale.is_finite() || scale == 0.0 {
        return v;
    }
    (v * scale).round() / scale
}

fn solve_linear(aff: &Affine, spec: &Spec, opts: &SolverOptions) -> Outcome {
    let cost: Vec<f64> = aff.coef[0].iter().map(|c| spec.sense * c).collect();
    let rows: Vec<LinRow> = spec
        .rows
        .iter()
        .map(|r| LinRow { coef: aff.coef[r.out].clone(), rel: r.rel, rhs: r.rhs - aff.constant[r.out] })
        .collect();
    let mut pivots = 0;
    let (status, x) = branch_and_bound(spec, opts, &mut |lo, hi| {
        match simplex(&cost, &rows, lo, hi, opts.max_iterations, &mut pivots) {
            LpResult::Optimal(x) => {
                let value = cost.iter().zip(&x).map(|(c, v)| c * v).sum();
                Relaxed::Solved { x, value, exact: true }
            }
            LpResult::Infeasible => Relaxed::Infeasible,
            LpResult::Unbounded => Relaxed::Unbounded,
            LpResult::IterationLimit => {
                Relaxed::Failed("The simplex method reached the iteration limit before finding the optimum.".into())
            }
        }
    });
    Outcome { status, x: x.unwrap_or_else(|| spec.x0.clone()), iterations: pivots, method: SolverMethod::Linear }
}

// ---------------------------------------------------------------- simplex

struct LinRow {
    coef: Vec<f64>,
    rel: Rel,
    rhs: f64,
}

enum LpResult {
    Optimal(Vec<f64>),
    Infeasible,
    Unbounded,
    IterationLimit,
}

/// How an original variable is expressed with non-negative tableau columns.
#[derive(Clone, Copy)]
enum VarMap {
    /// x = lo + t (finite lower bound; a finite upper bound adds a row).
    Shift(usize, f64),
    /// x = hi - t (only an upper bound).
    Mirror(usize, f64),
    /// x = t⁺ - t⁻ (free variable).
    Split(usize, usize),
}

/// Minimize `cost · x` subject to `rows` and `lo <= x <= hi` with a dense
/// two-phase tableau simplex. Phase 1 minimizes the sum of artificial
/// variables to find a feasible basis (or prove there is none); phase 2
/// optimizes the real objective from there. Pivoting uses Dantzig's rule for
/// speed and switches to Bland's rule — which provably cannot cycle — after
/// a run of degenerate pivots.
fn simplex(cost: &[f64], rows: &[LinRow], lo: &[f64], hi: &[f64], max_pivots: usize, pivots: &mut usize) -> LpResult {
    let mut maps = Vec::with_capacity(cost.len());
    let mut nt = 0;
    let mut upper_rows = Vec::new();
    for (&l, &h) in lo.iter().zip(hi) {
        if l > h {
            return LpResult::Infeasible;
        }
        maps.push(if l.is_finite() {
            if h.is_finite() {
                upper_rows.push((nt, h - l));
            }
            nt += 1;
            VarMap::Shift(nt - 1, l)
        } else if h.is_finite() {
            nt += 1;
            VarMap::Mirror(nt - 1, h)
        } else {
            nt += 2;
            VarMap::Split(nt - 2, nt - 1)
        });
    }
    // Rewrite a coefficient row over the tableau columns; also returns the
    // constant that the substitution moves to the right-hand side.
    let expand = |coef: &[f64]| -> (Vec<f64>, f64) {
        let mut out = vec![0.0; nt];
        let mut moved = 0.0;
        for (&a, m) in coef.iter().zip(&maps) {
            match *m {
                VarMap::Shift(c, l) => {
                    out[c] += a;
                    moved += a * l;
                }
                VarMap::Mirror(c, h) => {
                    out[c] -= a;
                    moved += a * h;
                }
                VarMap::Split(p, q) => {
                    out[p] += a;
                    out[q] -= a;
                }
            }
        }
        (out, moved)
    };
    let mut cons: Vec<(Vec<f64>, Rel, f64)> = rows
        .iter()
        .map(|r| {
            let (c, moved) = expand(&r.coef);
            (c, r.rel, r.rhs - moved)
        })
        .collect();
    for (col, ub) in upper_rows {
        let mut c = vec![0.0; nt];
        c[col] = 1.0;
        cons.push((c, Rel::Le, ub));
    }
    for (c, rel, b) in &mut cons {
        if *b < 0.0 {
            c.iter_mut().for_each(|v| *v = -*v);
            *b = -*b;
            *rel = rel.flip();
        }
    }
    let (tcost, _) = expand(cost);

    let m = cons.len();
    let n_slack = cons.iter().filter(|c| c.1 != Rel::Eq).count();
    let n_art = cons.iter().filter(|c| c.1 != Rel::Le).count();
    let art0 = nt + n_slack;
    let ncol = art0 + n_art;
    let mut t = Tableau { rows: vec![vec![0.0; ncol + 1]; m], basis: vec![0; m] };
    let (mut s, mut a) = (nt, art0);
    for (i, (c, rel, b)) in cons.iter().enumerate() {
        let row = &mut t.rows[i];
        row[..nt].copy_from_slice(c);
        row[ncol] = *b;
        if *rel != Rel::Eq {
            row[s] = if *rel == Rel::Le { 1.0 } else { -1.0 };
            s += 1;
        }
        if *rel == Rel::Le {
            t.basis[i] = s - 1;
        } else {
            row[a] = 1.0;
            t.basis[i] = a;
            a += 1;
        }
    }

    if n_art > 0 {
        let mut phase1 = vec![0.0; ncol];
        phase1[art0..].fill(1.0);
        if let Err(e) = t.optimize(&phase1, ncol, max_pivots, pivots) {
            return e;
        }
        let residual: f64 = t.basis.iter().zip(&t.rows).filter(|(b, _)| **b >= art0).map(|(_, r)| r[ncol]).sum();
        let scale = cons.iter().fold(1.0, |acc: f64, c| acc.max(c.2.abs()));
        if residual > 1e-7 * scale {
            return LpResult::Infeasible;
        }
        // Pivot leftover (zero-valued) artificials out of the basis. A row
        // with no usable column is redundant; its artificial stays basic at
        // zero and, being barred from phase 2, never moves.
        for i in 0..m {
            if t.basis[i] >= art0
                && let Some(j) = (0..art0).find(|&j| t.rows[i][j].abs() > 1e-9)
            {
                t.pivot(i, j);
            }
        }
    }
    let mut phase2 = vec![0.0; ncol];
    phase2[..nt].copy_from_slice(&tcost);
    if let Err(e) = t.optimize(&phase2, art0, max_pivots, pivots) {
        return e;
    }

    let mut tv = vec![0.0; nt];
    for (row, &b) in t.rows.iter().zip(&t.basis) {
        if b < nt {
            tv[b] = row[ncol].max(0.0);
        }
    }
    LpResult::Optimal(
        maps.iter()
            .map(|m| match *m {
                VarMap::Shift(c, l) => l + tv[c],
                VarMap::Mirror(c, h) => h - tv[c],
                VarMap::Split(p, q) => tv[p] - tv[q],
            })
            .collect(),
    )
}

/// Rows hold B⁻¹A with the right-hand side in the last column.
struct Tableau {
    rows: Vec<Vec<f64>>,
    basis: Vec<usize>,
}

impl Tableau {
    fn pivot(&mut self, r: usize, c: usize) {
        let p = self.rows[r][c];
        self.rows[r].iter_mut().for_each(|v| *v /= p);
        let pr = self.rows[r].clone();
        for (i, row) in self.rows.iter_mut().enumerate() {
            let f = row[c];
            if i != r && f != 0.0 {
                row.iter_mut().zip(&pr).for_each(|(v, &q)| *v -= f * q);
                row[c] = 0.0;
            }
        }
        self.basis[r] = c;
    }

    /// Minimize `cost` over columns `< allowed` from the current basis.
    fn optimize(&mut self, cost: &[f64], allowed: usize, max_pivots: usize, pivots: &mut usize) -> Result<(), LpResult> {
        let rhs = cost.len();
        let eps = 1e-9 * cost.iter().fold(1.0, |m: f64, c| m.max(c.abs()));
        let mut bland = false;
        let mut degenerate_run = 0;
        loop {
            let cb: Vec<f64> = self.basis.iter().map(|&b| cost[b]).collect();
            let mut enter = None;
            let mut most_negative = -eps;
            for j in 0..allowed {
                if self.basis.contains(&j) {
                    continue;
                }
                let d = cost[j] - self.rows.iter().zip(&cb).map(|(row, c)| c * row[j]).sum::<f64>();
                if d < most_negative {
                    enter = Some(j);
                    if bland {
                        break;
                    }
                    most_negative = d;
                }
            }
            let Some(c) = enter else { return Ok(()) };
            if *pivots >= max_pivots {
                return Err(LpResult::IterationLimit);
            }
            // Ratio test; ties go to the lowest basic index (Bland).
            let mut leave: Option<(usize, f64)> = None;
            for (i, row) in self.rows.iter().enumerate() {
                if row[c] > 1e-9 {
                    let ratio = row[rhs].max(0.0) / row[c];
                    let better = match leave {
                        None => true,
                        Some((li, lr)) => ratio < lr - 1e-12 || (ratio <= lr + 1e-12 && self.basis[i] < self.basis[li]),
                    };
                    if better {
                        leave = Some((i, ratio));
                    }
                }
            }
            let Some((r, ratio)) = leave else { return Err(LpResult::Unbounded) };
            if ratio <= 1e-12 {
                degenerate_run += 1;
                bland |= degenerate_run > 50;
            } else {
                degenerate_run = 0;
            }
            self.pivot(r, c);
            *pivots += 1;
        }
    }
}

// ------------------------------------------------------- branch and bound

enum Relaxed {
    /// `value` is the minimized objective (sense * f); `exact` is false when
    /// the subproblem stopped on a budget rather than converging.
    Solved { x: Vec<f64>, value: f64, exact: bool },
    Infeasible,
    Unbounded,
    Failed(String),
}

/// Depth-first branch-and-bound over integer variables, with `relax`
/// solving the continuous problem within given bounds. Depth-first finds an
/// incumbent quickly (useful when the node cap is hit) and the nearer child
/// is explored first. Without integer variables this is a single relax call.
fn branch_and_bound(
    spec: &Spec,
    opts: &SolverOptions,
    relax: &mut dyn FnMut(&[f64], &[f64]) -> Relaxed,
) -> (SolverStatus, Option<Vec<f64>>) {
    let has_int = spec.integer.iter().any(|&b| b);
    let mut stack = vec![(spec.lo.clone(), spec.hi.clone())];
    let mut best: Option<(Vec<f64>, f64)> = None;
    let mut all_exact = true;
    let mut nodes = 0;
    let mut capped = false;
    let mut failure = None;
    while let Some((lo, hi)) = stack.pop() {
        if nodes >= opts.max_subproblems.max(1) {
            capped = true;
            break;
        }
        nodes += 1;
        let (x, value, exact) = match relax(&lo, &hi) {
            Relaxed::Solved { x, value, exact } => (x, value, exact),
            Relaxed::Infeasible => continue,
            // A relaxation that is unbounded makes the integer problem
            // unbounded too (or infeasible, which we can't cheaply tell).
            Relaxed::Unbounded => return (SolverStatus::Unbounded, None),
            Relaxed::Failed(msg) => {
                if !has_int {
                    return (SolverStatus::Failed(msg), None);
                }
                failure = Some(msg);
                continue;
            }
        };
        if let Some((_, incumbent)) = &best
            && value >= incumbent - 1e-9 * incumbent.abs().max(1.0)
        {
            continue;
        }
        all_exact &= exact;
        let branch = (0..x.len())
            .filter(|&j| spec.integer[j])
            .map(|j| (j, (x[j] - x[j].round()).abs()))
            .filter(|&(_, dist)| dist > opts.integer_tolerance)
            .max_by(|a, b| a.1.total_cmp(&b.1));
        match branch {
            None => {
                let x = x.iter().zip(&spec.integer).map(|(&v, &int)| if int { v.round() } else { v }).collect();
                best = Some((x, value));
            }
            Some((j, _)) => {
                let v = x[j];
                let mut down = (lo.clone(), hi.clone());
                down.1[j] = v.floor();
                let mut up = (lo, hi);
                up.0[j] = v.ceil();
                if v - v.floor() < 0.5 {
                    stack.push(up);
                    stack.push(down);
                } else {
                    stack.push(down);
                    stack.push(up);
                }
            }
        }
    }
    match best {
        Some((x, _)) => (if capped || !all_exact { SolverStatus::Feasible } else { SolverStatus::Optimal }, Some(x)),
        None if capped => (
            SolverStatus::Failed(format!(
                "Stopped after {} subproblems without finding an integer solution.",
                opts.max_subproblems
            )),
            None,
        ),
        None => (failure.map_or(SolverStatus::Infeasible, SolverStatus::Failed), None),
    }
}

// -------------------------------------------------------------- nonlinear

fn solve_nonlinear(probe: &mut Probe, spec: &Spec, opts: &SolverOptions, rng: &mut Rng) -> Outcome {
    let mut nlp = Nlp {
        probe,
        spec,
        obj_scale: spec.f0[0].abs().max(1.0),
        tol: opts.tolerance,
        max_iter: opts.max_iterations,
        iterations: 0,
    };
    let (status, x) = branch_and_bound(spec, opts, &mut |lo, hi| nlp.relax(lo, hi, rng, opts.multistart));
    Outcome {
        status,
        x: x.unwrap_or_else(|| spec.x0.clone()),
        iterations: nlp.iterations,
        method: SolverMethod::Nonlinear,
    }
}

/// Outer iterations of the augmented Lagrangian method.
const MAX_OUTER: usize = 30;
/// Penalty weight cap; beyond this Nelder–Mead can't make progress anyway.
const MAX_MU: f64 = 1e10;

/// Augmented Lagrangian over a derivative-free inner solver.
///
/// Why this combination: spreadsheet models are often only piecewise smooth
/// (IF, MIN, ABS, lookups) and every evaluation is a recalc, so numeric
/// gradients are both noisy and costly. Nelder–Mead needs neither, and the
/// augmented Lagrangian turns constraints into a smooth penalty whose
/// multipliers converge to exact feasibility without driving the penalty
/// weight to infinity (which plain penalty methods need, and which wrecks
/// Nelder–Mead's conditioning). Box bounds (non-negativity, binary, branch
/// bounds) are enforced exactly by projection instead.
struct Nlp<'p, 'w> {
    probe: &'p mut Probe<'w>,
    spec: &'p Spec,
    /// The objective is divided by its starting magnitude so penalty
    /// weights mean the same thing for a $1M model and a 0.01 one.
    obj_scale: f64,
    tol: f64,
    max_iter: usize,
    iterations: usize,
}

struct Local {
    x: Vec<f64>,
    phi: f64,
    viol: f64,
    converged: bool,
    unbounded: bool,
}

impl Nlp<'_, '_> {
    /// Scaled objective and signed relative row excesses at `x`.
    fn measure(&mut self, x: &[f64]) -> Option<(f64, Vec<f64>)> {
        let out = self.probe.eval(x)?;
        let spec = self.spec;
        let phi = spec.sense * out[0] / self.obj_scale;
        let g = spec
            .rows
            .iter()
            .map(|r| match r.rel {
                // Signed for equalities (multiplier updates need the sign).
                Rel::Eq => (out[r.out] - r.rhs) / r.rhs.abs().max(1.0),
                _ => row_excess(r, out[r.out]),
            })
            .collect();
        Some((phi, g))
    }

    fn violation(&self, g: &[f64]) -> f64 {
        self.spec.rows.iter().zip(g).map(|(r, &v)| if r.rel == Rel::Eq { v.abs() } else { v.max(0.0) }).fold(0.0, f64::max)
    }

    fn augmented(&mut self, x: &[f64], lam: &[f64], mu: f64) -> f64 {
        let Some((phi, g)) = self.measure(x) else { return f64::INFINITY };
        let spec = self.spec;
        phi + spec
            .rows
            .iter()
            .zip(g.iter().zip(lam))
            .map(|(r, (&gi, &li))| {
                if r.rel == Rel::Eq {
                    li * gi + 0.5 * mu * gi * gi
                } else {
                    let t = (li + mu * gi).max(0.0);
                    (t * t - li * li) / (2.0 * mu)
                }
            })
            .sum::<f64>()
    }

    /// One local solve from `start`. `None` if even the best point found
    /// makes the model error.
    fn local(&mut self, start: &[f64], lo: &[f64], hi: &[f64]) -> Option<Local> {
        let m = self.spec.rows.len();
        let mut x = clamp_all(start, lo, hi);
        let mut lam = vec![0.0; m];
        let mut mu = 10.0;
        let mut step: Vec<f64> = x.iter().map(|v| 0.1 * v.abs().max(1.0)).collect();
        let (mut prev_phi, mut prev_viol) = (f64::INFINITY, f64::INFINITY);
        let mut converged = false;
        for _ in 0..MAX_OUTER {
            let max_iter = self.max_iter;
            let (xn, value, nm_converged, iters) = {
                let mut f = |y: &[f64]| self.augmented(y, &lam, mu);
                nelder_mead(&mut f, &x, lo, hi, &step, max_iter)
            };
            self.iterations += iters;
            let (phi, g) = self.measure(&xn)?;
            let viol = self.violation(&g);
            if phi < -UNBOUNDED && viol <= self.tol {
                return Some(Local { x: xn, phi, viol, converged: true, unbounded: true });
            }
            if value < -UNBOUNDED {
                // The penalty is too weak to hold the objective back, so the
                // inner problem itself ran off to infinity. Neither that
                // point nor the multipliers it implies mean anything: retry
                // from the last sane point with a stiffer penalty.
                if mu >= MAX_MU {
                    break;
                }
                mu = (mu * 10.0).min(MAX_MU);
                continue;
            }
            x = xn;
            if m == 0 {
                converged = nm_converged;
                break;
            }
            // First-order multiplier update. Its size (divided by mu) is the
            // KKT residual: zero only when every active constraint is tight
            // and every slack one has a zero multiplier.
            let mut residual: f64 = 0.0;
            for ((l, r), &gi) in lam.iter_mut().zip(&self.spec.rows).zip(&g) {
                let next = if r.rel == Rel::Eq { *l + mu * gi } else { (*l + mu * gi).max(0.0) };
                residual = residual.max((next - *l).abs() / mu);
                *l = next;
            }
            if viol <= self.tol && residual <= self.tol && (phi - prev_phi).abs() <= self.tol * (1.0 + phi.abs()) {
                converged = nm_converged;
                break;
            }
            // Raise the penalty only when the multipliers alone aren't
            // shrinking the violation fast enough.
            if viol > 0.25 * prev_viol {
                mu = (mu * 10.0).min(MAX_MU);
            }
            (prev_phi, prev_viol) = (phi, viol);
            // Later rounds start near the answer: a smaller simplex.
            step = x.iter().map(|v| 0.02 * v.abs().max(1.0)).collect();
        }
        let (phi, g) = self.measure(&x)?;
        let viol = self.violation(&g);
        Some(Local { x, phi, viol, converged, unbounded: false })
    }

    /// Solve the continuous problem within bounds from the current values
    /// and `extra` seeded perturbations, keeping the best feasible result.
    /// Multi-start is a cheap guard against local optima and against
    /// starting on a plateau.
    fn relax(&mut self, lo: &[f64], hi: &[f64], rng: &mut Rng, extra: usize) -> Relaxed {
        let x0 = self.spec.x0.clone();
        let mut starts = vec![clamp_all(&x0, lo, hi)];
        for k in 0..extra {
            starts.push(
                (0..x0.len())
                    .map(|j| {
                        if lo[j].is_finite() && hi[j].is_finite() {
                            rng.uniform(lo[j], hi[j])
                        } else {
                            let s = x0[j].abs().max(1.0) * (k + 1) as f64;
                            clamp(x0[j] + rng.uniform(-s, s), lo[j], hi[j])
                        }
                    })
                    .collect(),
            );
        }
        let mut best: Option<Local> = None;
        for s in &starts {
            let Some(r) = self.local(s, lo, hi) else { continue };
            if r.unbounded {
                return Relaxed::Unbounded;
            }
            let better = match &best {
                None => true,
                Some(b) => {
                    let (rf, bf) = (r.viol <= self.tol, b.viol <= self.tol);
                    match (rf, bf) {
                        (true, false) => true,
                        (false, true) => false,
                        (true, true) => r.phi < b.phi - 1e-9 * (1.0 + b.phi.abs()),
                        (false, false) => r.viol < b.viol,
                    }
                }
            };
            if better {
                best = Some(r);
            }
        }
        match best {
            Some(b) if b.viol <= self.tol => Relaxed::Solved { x: b.x, value: b.phi, exact: b.converged },
            Some(_) => Relaxed::Infeasible,
            None => Relaxed::Failed("The model returned an error at every trial point.".into()),
        }
    }
}

/// Nelder–Mead with projection onto the box and restarts. Restarting from
/// the best vertex with a fresh simplex is the standard fix for the
/// method's habit of collapsing prematurely; we stop once a restart no
/// longer improves. Returns (x, f(x), converged, iterations).
fn nelder_mead(
    f: &mut dyn FnMut(&[f64]) -> f64,
    x0: &[f64],
    lo: &[f64],
    hi: &[f64],
    step: &[f64],
    max_iter: usize,
) -> (Vec<f64>, f64, bool, usize) {
    let mut best = clamp_all(x0, lo, hi);
    let mut f_best = finite_or_inf(f(&best));
    let mut used = 0;
    let mut scale = 1.0;
    for restart in 0..4 {
        let (x, fx, converged, it) = nm_run(f, &best, lo, hi, step, scale, max_iter - used);
        used += it;
        let gain = f_best - fx;
        if fx < f_best {
            best = x;
            f_best = fx;
        }
        if !converged || f_best < -UNBOUNDED {
            return (best, f_best, converged, used);
        }
        let improved = gain > 1e-12 * (1.0 + f_best.abs()); // false for NaN too
        if restart > 0 && !improved {
            break;
        }
        scale = 0.1;
    }
    (best, f_best, true, used)
}

fn finite_or_inf(v: f64) -> f64 {
    if v.is_nan() { f64::INFINITY } else { v }
}

/// One Nelder–Mead run with the dimension-adaptive coefficients of Gao &
/// Han (2012), which behave much better than the classic ones beyond a few
/// variables (and coincide with them for n = 2).
fn nm_run(
    f: &mut dyn FnMut(&[f64]) -> f64,
    x0: &[f64],
    lo: &[f64],
    hi: &[f64],
    step: &[f64],
    scale: f64,
    max_iter: usize,
) -> (Vec<f64>, f64, bool, usize) {
    let n = x0.len();
    let nn = n.max(2) as f64;
    let (gamma, rho, sigma) = (1.0 + 2.0 / nn, 0.75 - 0.5 / nn, 1.0 - 1.0 / nn);
    let mut pts: Vec<Vec<f64>> = vec![x0.to_vec()];
    for j in 0..n {
        let mut p = x0.to_vec();
        let s = step[j] * scale;
        p[j] = (x0[j] + s).min(hi[j]);
        if p[j] == x0[j] {
            p[j] = (x0[j] - s).max(lo[j]);
        }
        pts.push(p);
    }
    let mut vals: Vec<f64> = pts.iter().map(|p| finite_or_inf(f(p))).collect();
    let toward = |from: &[f64], to: &[f64], t: f64| -> Vec<f64> {
        from.iter().zip(to).zip(lo.iter().zip(hi)).map(|((&a, &b), (&l, &h))| clamp(a + t * (b - a), l, h)).collect()
    };
    for it in 0..max_iter {
        let mut order: Vec<usize> = (0..=n).collect();
        order.sort_by(|&a, &b| vals[a].total_cmp(&vals[b]));
        pts = order.iter().map(|&i| pts[i].clone()).collect();
        vals = order.iter().map(|&i| vals[i]).collect();
        let (fb, fw) = (vals[0], vals[n]);
        if fb < -UNBOUNDED {
            return (pts[0].clone(), fb, true, it);
        }
        let diam = pts[1..]
            .iter()
            .flat_map(|p| p.iter().zip(&pts[0]).map(|(a, b)| (a - b).abs() / (1.0 + b.abs())))
            .fold(0.0, f64::max);
        if (fw - fb <= 1e-12 * (1.0 + fb.abs()) && diam <= 1e-8) || diam <= 1e-14 {
            return (pts[0].clone(), fb, true, it);
        }
        let mut c = vec![0.0; n];
        for p in &pts[..n] {
            c.iter_mut().zip(p).for_each(|(ci, v)| *ci += v / n as f64);
        }
        // Reflection is the worst point pushed through the centroid.
        let xr = toward(&c, &pts[n], -1.0);
        let fr = finite_or_inf(f(&xr));
        if fr < vals[0] {
            let xe = toward(&c, &xr, gamma);
            let fe = finite_or_inf(f(&xe));
            (pts[n], vals[n]) = if fe < fr { (xe, fe) } else { (xr, fr) };
        } else if fr < vals[n - 1] {
            (pts[n], vals[n]) = (xr, fr);
        } else {
            let outside = fr < vals[n];
            let xc = if outside { toward(&c, &xr, rho) } else { toward(&c, &pts[n], rho) };
            let fc = finite_or_inf(f(&xc));
            if (outside && fc <= fr) || (!outside && fc < vals[n]) {
                (pts[n], vals[n]) = (xc, fc);
            } else {
                for i in 1..=n {
                    pts[i] = toward(&pts[0], &pts[i], sigma);
                    vals[i] = finite_or_inf(f(&pts[i]));
                }
            }
        }
    }
    let i = (0..=n).min_by(|&a, &b| vals[a].total_cmp(&vals[b])).unwrap_or(0);
    (pts[i].clone(), vals[i], false, max_iter)
}
