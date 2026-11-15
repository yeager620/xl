//! The statistics charts draw: trendline fits (with R² and an equation),
//! moving averages, error bar extents, histogram bins and waterfall bars.

use super::{BinMode, Bins, ErrorBars, ErrorDir, ErrorKind, TrendKind};

/// A fitted trendline.
#[derive(Clone, Debug, PartialEq)]
pub struct TrendFit {
    pub kind: TrendKind,
    /// Linear: [intercept, slope]. Polynomial: [c0, c1, …, cn] (y = Σ cᵢxⁱ).
    /// Exponential: [b, a] for y = b·e^(a·x). Logarithmic: [b, a] for
    /// y = a·ln(x) + b. Power: [b, a] for y = b·x^a.
    pub coeffs: Vec<f64>,
    /// Coefficient of determination. Exponential and power fits measure it
    /// on ln(y), as Excel does.
    pub r_squared: f64,
}

impl TrendFit {
    pub fn eval(&self, x: f64) -> f64 {
        let c = &self.coeffs;
        match self.kind {
            TrendKind::Linear | TrendKind::Polynomial(_) => c.iter().rev().fold(0.0, |acc, k| acc * x + k),
            TrendKind::Exponential => c[0] * (c[1] * x).exp(),
            TrendKind::Logarithmic => c[1] * x.ln() + c[0],
            TrendKind::Power => c[0] * x.powf(c[1]),
            TrendKind::MovingAverage(_) => f64::NAN,
        }
    }

    /// "y = 2x + 1", "y = 3e^(0.5x)", "y = 0.5x² − 2x + 4"…
    pub fn equation(&self) -> String {
        let c = &self.coeffs;
        let signed = |v: f64, first: bool| -> String {
            if first {
                num(v)
            } else if v < 0.0 {
                format!(" − {}", num(-v))
            } else {
                format!(" + {}", num(v))
            }
        };
        match self.kind {
            TrendKind::Linear | TrendKind::Polynomial(_) => {
                let mut s = String::from("y = ");
                let mut first = true;
                for (p, &k) in c.iter().enumerate().rev() {
                    if k == 0.0 && c.len() > 1 {
                        continue;
                    }
                    s.push_str(&signed(k, first));
                    match p {
                        0 => {}
                        1 => s.push('x'),
                        p => {
                            s.push('x');
                            s.push_str(&superscript(p));
                        }
                    }
                    first = false;
                }
                if first {
                    s.push('0');
                }
                s
            }
            TrendKind::Exponential => format!("y = {}e^({}x)", num(c[0]), num(c[1])),
            TrendKind::Logarithmic => format!("y = {}ln(x){}", num(c[1]), signed(c[0], false)),
            TrendKind::Power => format!("y = {}x^{}", num(c[0]), num(c[1])),
            TrendKind::MovingAverage(_) => String::new(),
        }
    }
}

/// Four significant digits, like Excel's trendline labels.
fn num(v: f64) -> String {
    if v == 0.0 || !v.is_finite() {
        return "0".into();
    }
    let digits = (4 - 1 - v.abs().log10().floor() as i32).max(0) as usize;
    let s = format!("{v:.digits$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

fn superscript(n: usize) -> String {
    n.to_string().chars().map(|d| ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'][d.to_digit(10).unwrap_or(0) as usize]).collect()
}

/// Least squares y = a + b·x: (a, b, r²).
fn linear(xs: &[f64], ys: &[f64]) -> Option<(f64, f64, f64)> {
    let n = xs.len() as f64;
    if xs.len() < 2 {
        return None;
    }
    let (mx, my) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
    let sxx: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
    if sxx == 0.0 {
        return None;
    }
    let sxy: f64 = xs.iter().zip(ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    let b = sxy / sxx;
    let a = my - b * mx;
    Some((a, b, r_squared(ys, |i| a + b * xs[i])))
}

fn r_squared(ys: &[f64], fitted: impl Fn(usize) -> f64) -> f64 {
    let my = ys.iter().sum::<f64>() / ys.len() as f64;
    let tot: f64 = ys.iter().map(|y| (y - my).powi(2)).sum();
    let res: f64 = ys.iter().enumerate().map(|(i, y)| (y - fitted(i)).powi(2)).sum();
    if tot == 0.0 { 1.0 } else { 1.0 - res / tot }
}

/// Solve A·x = b by Gaussian elimination with partial pivoting.
fn solve(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    for col in 0..n {
        let pivot = (col..n).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[pivot][col].abs() < 1e-300 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        for row in col + 1..n {
            let f = a[row][col] / a[col][col];
            let pivot_row = a[col].clone();
            for (x, p) in a[row].iter_mut().zip(&pivot_row).skip(col) {
                *x -= f * p;
            }
            b[row] -= f * b[col];
        }
    }
    let mut x = vec![0.0; n];
    for row in (0..n).rev() {
        let s: f64 = (row + 1..n).map(|k| a[row][k] * x[k]).sum();
        x[row] = (b[row] - s) / a[row][row];
    }
    x.iter().all(|v| v.is_finite()).then_some(x)
}

/// Least squares polynomial of `order`: coefficients c0..cn.
fn polynomial(xs: &[f64], ys: &[f64], order: usize) -> Option<Vec<f64>> {
    if xs.len() <= order {
        return None;
    }
    let m = order + 1;
    // Normal equations on x scaled to about [-1, 1] for conditioning.
    let scale = xs.iter().fold(0.0f64, |m, x| m.max(x.abs())).max(1e-300);
    let mut a = vec![vec![0.0; m]; m];
    let mut b = vec![0.0; m];
    for (x, y) in xs.iter().zip(ys) {
        let t = x / scale;
        let powers: Vec<f64> = (0..2 * m).map(|p| t.powi(p as i32)).collect();
        for i in 0..m {
            for j in 0..m {
                a[i][j] += powers[i + j];
            }
            b[i] += powers[i] * y;
        }
    }
    let c = solve(a, b)?;
    Some(c.iter().enumerate().map(|(p, k)| k / scale.powi(p as i32)).collect())
}

/// Fit a trendline to the points (moving averages aren't fits: see
/// [`moving_average`]). None when the data doesn't allow it (too few points,
/// or values a log/power/exponential fit can't take).
pub fn fit_trend(kind: TrendKind, xs: &[f64], ys: &[f64]) -> Option<TrendFit> {
    let n = xs.len().min(ys.len());
    let (xs, ys) = (&xs[..n], &ys[..n]);
    let fit = |coeffs: Vec<f64>, r_squared: f64| Some(TrendFit { kind, coeffs, r_squared });
    match kind {
        TrendKind::Linear => {
            let (a, b, r2) = linear(xs, ys)?;
            fit(vec![a, b], r2)
        }
        TrendKind::Polynomial(order) => {
            let order = order.clamp(2, 6) as usize;
            let c = polynomial(xs, ys, order)?;
            let r2 = r_squared(ys, |i| c.iter().rev().fold(0.0, |acc, k| acc * xs[i] + k));
            fit(c, r2)
        }
        TrendKind::Exponential => {
            if ys.iter().any(|y| *y <= 0.0) {
                return None;
            }
            let ln: Vec<f64> = ys.iter().map(|y| y.ln()).collect();
            let (a, b, r2) = linear(xs, &ln)?;
            fit(vec![a.exp(), b], r2)
        }
        TrendKind::Logarithmic => {
            if xs.iter().any(|x| *x <= 0.0) {
                return None;
            }
            let ln: Vec<f64> = xs.iter().map(|x| x.ln()).collect();
            let (a, b, r2) = linear(&ln, ys)?;
            fit(vec![a, b], r2)
        }
        TrendKind::Power => {
            if xs.iter().chain(ys).any(|v| *v <= 0.0) {
                return None;
            }
            let lx: Vec<f64> = xs.iter().map(|x| x.ln()).collect();
            let ly: Vec<f64> = ys.iter().map(|y| y.ln()).collect();
            let (a, b, r2) = linear(&lx, &ly)?;
            fit(vec![a.exp(), b], r2)
        }
        TrendKind::MovingAverage(_) => None,
    }
}

/// The average of each `period` points ending at each point (None until
/// enough points; gaps are skipped like Excel does).
pub fn moving_average(ys: &[Option<f64>], period: u8) -> Vec<Option<f64>> {
    let p = period.max(2) as usize;
    let mut window: Vec<f64> = Vec::new();
    ys.iter()
        .map(|y| {
            let y = (*y)?;
            window.push(y);
            if window.len() > p {
                window.remove(0);
            }
            (window.len() == p).then(|| window.iter().sum::<f64>() / p as f64)
        })
        .collect()
}

/// Sample standard deviation.
fn std_dev(vals: &[f64]) -> f64 {
    let n = vals.len() as f64;
    if vals.len() < 2 {
        return 0.0;
    }
    let m = vals.iter().sum::<f64>() / n;
    (vals.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()
}

/// Where each point's error bar runs: (low, high), in value units.
pub fn error_ranges(values: &[Option<f64>], bars: &ErrorBars) -> Vec<Option<(f64, f64)>> {
    let nums: Vec<f64> = values.iter().flatten().copied().collect();
    let sd = std_dev(&nums);
    let mean = if nums.is_empty() { 0.0 } else { nums.iter().sum::<f64>() / nums.len() as f64 };
    values
        .iter()
        .map(|v| {
            let v = (*v)?;
            // StdDev bars sit around the mean of the series, the others around the point.
            let (center, amount) = match bars.kind {
                ErrorKind::Fixed => (v, bars.value.abs()),
                ErrorKind::Percent => (v, v.abs() * bars.value.abs() / 100.0),
                ErrorKind::StdDev => (mean, sd * bars.value.abs()),
                ErrorKind::StdErr => (v, if nums.is_empty() { 0.0 } else { sd / (nums.len() as f64).sqrt() }),
            };
            Some(match bars.dir {
                ErrorDir::Both => (center - amount, center + amount),
                ErrorDir::Plus => (center, center + amount),
                ErrorDir::Minus => (center - amount, center),
            })
        })
        .collect()
}

/// A histogram bin.
#[derive(Clone, Debug, PartialEq)]
pub struct Bin {
    /// "[1, 5]", "(5, 9]", "≤1", ">9".
    pub label: String,
    pub lo: f64,
    pub hi: f64,
    pub count: usize,
}

fn edge(v: f64) -> String {
    crate::workbook::precise((v * 1e9).round() / 1e9)
}

/// Excel's binning: bins closed on the right (the first also on the left),
/// starting at the smallest value; automatic width by Scott's rule
/// (3.5·σ/∛n); optional overflow and underflow bins.
pub fn histogram(values: &[f64], bins: &Bins) -> Vec<Bin> {
    let vals: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if vals.is_empty() {
        return Vec::new();
    }
    let mn = vals.iter().copied().fold(f64::INFINITY, f64::min);
    let mx = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let under = bins.underflow.filter(|u| *u >= mn && *u < mx);
    let over = bins.overflow.filter(|o| *o <= mx && *o > under.unwrap_or(f64::NEG_INFINITY));
    let (start, end) = (under.unwrap_or(mn), over.unwrap_or(mx));
    let span = end - start;
    let width = match bins.mode {
        BinMode::Width(w) if w > 0.0 => w,
        BinMode::Count(c) if c > 0 => span / c as f64,
        _ => 3.5 * std_dev(&vals) / (vals.len() as f64).cbrt(),
    };
    let n = if width > 0.0 && span > 0.0 { ((span / width) - 1e-9).ceil().max(1.0) as usize } else { 1 };
    let n = n.min(1000);
    let width = if width > 0.0 && span > 0.0 { width } else { 1.0 };
    let mut out: Vec<Bin> = Vec::new();
    if let Some(u) = under {
        out.push(Bin { label: format!("≤{}", edge(u)), lo: f64::NEG_INFINITY, hi: u, count: 0 });
    }
    let first = out.len();
    for k in 0..n {
        let (lo, hi) = (start + width * k as f64, start + width * (k + 1) as f64);
        let open = if k == 0 && under.is_none() { '[' } else { '(' };
        out.push(Bin { label: format!("{open}{}, {}]", edge(lo), edge(hi)), lo, hi, count: 0 });
    }
    if let Some(o) = over {
        out.push(Bin { label: format!(">{}", edge(o)), lo: o, hi: f64::INFINITY, count: 0 });
    }
    for v in vals {
        let i = if under.is_some_and(|u| v <= u) {
            0
        } else if over.is_some_and(|o| v > o) {
            out.len() - 1
        } else {
            let k = (((v - start) / width) - 1e-9).ceil() as isize - 1;
            first + k.clamp(0, n as isize - 1) as usize
        };
        out[i].count += 1;
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    Increase,
    Decrease,
    Total,
}

/// One waterfall column: from `start` to `end`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
    pub start: f64,
    pub end: f64,
    pub kind: StepKind,
}

/// A waterfall's columns: each value moves the running total; a total
/// point stands on the axis at its own value and the total carries on from it.
pub fn waterfall(values: &[Option<f64>], subtotals: &[u32]) -> Vec<Option<Step>> {
    let mut running = 0.0;
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let v = (*v)?;
            if subtotals.contains(&(i as u32)) {
                running = v;
                return Some(Step { start: 0.0, end: v, kind: StepKind::Total });
            }
            let start = running;
            running += v;
            Some(Step { start, end: running, kind: if v < 0.0 { StepKind::Decrease } else { StepKind::Increase } })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6 * (1.0 + b.abs())
    }

    #[test]
    fn linear_fit_of_known_data() {
        let xs = [1.0, 2.0, 3.0, 4.0, 5.0];
        let ys = [3.0, 5.0, 7.0, 9.0, 11.0];
        let f = fit_trend(TrendKind::Linear, &xs, &ys).unwrap();
        assert!(close(f.coeffs[0], 1.0) && close(f.coeffs[1], 2.0), "{:?}", f.coeffs);
        assert!(close(f.r_squared, 1.0));
        assert_eq!(f.equation(), "y = 2x + 1");
        // Noisy data: slope and R² match LINEST / RSQ.
        let ys = [2.0, 4.0, 5.0, 4.0, 5.0];
        let f = fit_trend(TrendKind::Linear, &xs, &ys).unwrap();
        assert!(close(f.coeffs[1], 0.6) && close(f.coeffs[0], 2.2));
        assert!(close(f.r_squared, 0.6));
        assert_eq!(f.equation(), "y = 0.6x + 2.2");
    }

    #[test]
    fn exponential_log_and_power_fits() {
        let xs = [1.0, 2.0, 3.0, 4.0];
        let ys: Vec<f64> = xs.iter().map(|x: &f64| 3.0 * (0.5 * x).exp()).collect();
        let f = fit_trend(TrendKind::Exponential, &xs, &ys).unwrap();
        assert!(close(f.coeffs[0], 3.0) && close(f.coeffs[1], 0.5) && close(f.r_squared, 1.0));
        assert!(close(f.eval(5.0), 3.0 * 2.5f64.exp()));
        assert_eq!(f.equation(), "y = 3e^(0.5x)");

        let ys: Vec<f64> = xs.iter().map(|x| 2.0 * x.ln() + 1.0).collect();
        let f = fit_trend(TrendKind::Logarithmic, &xs, &ys).unwrap();
        assert!(close(f.coeffs[1], 2.0) && close(f.coeffs[0], 1.0));
        assert_eq!(f.equation(), "y = 2ln(x) + 1");

        let ys: Vec<f64> = xs.iter().map(|x| 4.0 * x.powf(1.5)).collect();
        let f = fit_trend(TrendKind::Power, &xs, &ys).unwrap();
        assert!(close(f.coeffs[0], 4.0) && close(f.coeffs[1], 1.5));
        // Non-positive values can't take a log.
        assert!(fit_trend(TrendKind::Exponential, &xs, &[1.0, -1.0, 2.0, 3.0]).is_none());
        assert!(fit_trend(TrendKind::Power, &[0.0, 1.0, 2.0], &[1.0, 2.0, 3.0]).is_none());
    }

    #[test]
    fn polynomial_fits() {
        let xs: Vec<f64> = (1..=8).map(f64::from).collect();
        let ys: Vec<f64> = xs.iter().map(|x| 0.5 * x * x - 2.0 * x + 4.0).collect();
        let f = fit_trend(TrendKind::Polynomial(2), &xs, &ys).unwrap();
        assert!(close(f.coeffs[0], 4.0) && close(f.coeffs[1], -2.0) && close(f.coeffs[2], 0.5), "{:?}", f.coeffs);
        assert!(close(f.r_squared, 1.0));
        assert_eq!(f.equation(), "y = 0.5x² − 2x + 4");
        // A cubic recovered with order 3; order 6 needs at least 7 points.
        let ys: Vec<f64> = xs.iter().map(|x| x * x * x - x).collect();
        let f = fit_trend(TrendKind::Polynomial(3), &xs, &ys).unwrap();
        assert!(close(f.eval(10.0), 990.0), "{}", f.eval(10.0));
        assert!(fit_trend(TrendKind::Polynomial(6), &xs[..6], &ys[..6]).is_none());
        assert!(fit_trend(TrendKind::Polynomial(6), &xs, &ys).is_some());
    }

    #[test]
    fn moving_averages() {
        let ys = [Some(1.0), Some(2.0), Some(3.0), None, Some(5.0), Some(7.0)];
        assert_eq!(moving_average(&ys, 2), vec![None, Some(1.5), Some(2.5), None, Some(4.0), Some(6.0)]);
    }

    #[test]
    fn error_bar_extents() {
        let v = [Some(10.0), Some(20.0), None];
        let fixed = ErrorBars { kind: ErrorKind::Fixed, value: 2.0, dir: ErrorDir::Both };
        assert_eq!(error_ranges(&v, &fixed), vec![Some((8.0, 12.0)), Some((18.0, 22.0)), None]);
        let pct = ErrorBars { kind: ErrorKind::Percent, value: 10.0, dir: ErrorDir::Plus };
        assert_eq!(error_ranges(&v, &pct), vec![Some((10.0, 11.0)), Some((20.0, 22.0)), None]);
        // Sample sd of 10, 20 is √50; std dev bars sit on the mean (15).
        let sd = ErrorBars { kind: ErrorKind::StdDev, value: 1.0, dir: ErrorDir::Minus };
        let r = error_ranges(&v, &sd);
        assert!(close(r[0].unwrap().0, 15.0 - 50f64.sqrt()) && close(r[0].unwrap().1, 15.0));
        let se = ErrorBars { kind: ErrorKind::StdErr, value: 0.0, dir: ErrorDir::Both };
        let r = error_ranges(&v, &se);
        assert!(close(r[1].unwrap().1, 20.0 + 5.0));
    }

    #[test]
    fn histogram_binning() {
        let vals = [1.0, 2.0, 2.0, 3.0, 4.0, 5.0, 9.0];
        let b = histogram(&vals, &Bins { mode: BinMode::Width(2.0), ..Bins::default() });
        let got: Vec<(&str, usize)> = b.iter().map(|b| (b.label.as_str(), b.count)).collect();
        assert_eq!(got, vec![("[1, 3]", 4), ("(3, 5]", 2), ("(5, 7]", 0), ("(7, 9]", 1)]);
        let b = histogram(&vals, &Bins { mode: BinMode::Count(2), overflow: Some(5.0), underflow: Some(1.0) });
        let got: Vec<(&str, usize)> = b.iter().map(|b| (b.label.as_str(), b.count)).collect();
        assert_eq!(got, vec![("≤1", 1), ("(1, 3]", 3), ("(3, 5]", 2), (">5", 1)]);
        // Automatic: Scott's rule; every value lands in some bin.
        let b = histogram(&vals, &Bins::default());
        assert_eq!(b.iter().map(|b| b.count).sum::<usize>(), vals.len());
        assert!(b.len() >= 2);
        assert_eq!(histogram(&[4.0, 4.0], &Bins::default()).len(), 1);
    }

    #[test]
    fn waterfall_running_totals() {
        let v = [Some(100.0), Some(-30.0), Some(20.0), Some(90.0), Some(-10.0)];
        let steps = waterfall(&v, &[3]);
        let got: Vec<(f64, f64, StepKind)> = steps.iter().map(|s| s.map(|s| (s.start, s.end, s.kind)).unwrap()).collect();
        assert_eq!(got, vec![(0.0, 100.0, StepKind::Increase), (100.0, 70.0, StepKind::Decrease), (70.0, 90.0, StepKind::Increase), (0.0, 90.0, StepKind::Total), (90.0, 80.0, StepKind::Decrease)]);
    }
}
