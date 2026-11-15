//! Dynamic-array functions (FILTER, SORT, SEQUENCE, VSTACK, …) and LET /
//! LAMBDA with their helpers (MAP, REDUCE, SCAN, BYROW, …). They work on
//! [`Val`]s, so results can be arrays that spill.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::eval::{Ctx, Func, Lambda, MAX_ARRAY_CELLS, Val, arg_count, broadcast, compare, eval, eval_val, num_arg, opt_num, random, scalar, text_arg, to_array};
use crate::formula::Expr;
use crate::value::{Array, ErrorKind, EvalResult, Value};

fn arr(args: &[Expr], i: usize, cx: &Ctx) -> EvalResult<Arc<Array>> {
    match args.get(i) {
        Some(Expr::Missing) | None => Err(ErrorKind::Value),
        Some(e) => to_array(eval_val(e, cx), cx),
    }
}

fn given(args: &[Expr], i: usize) -> bool {
    !matches!(args.get(i), Some(Expr::Missing) | None)
}

fn opt_bool(args: &[Expr], i: usize, cx: &Ctx, default: bool) -> EvalResult<bool> {
    if given(args, i) { eval(&args[i], cx).to_bool() } else { Ok(default) }
}

/// An optional count argument, truncated (None when omitted).
fn opt_int(args: &[Expr], i: usize, cx: &Ctx) -> EvalResult<Option<i64>> {
    if given(args, i) { Ok(Some(num_arg(args, i, cx)?.trunc() as i64)) } else { Ok(None) }
}

fn pad_arg(args: &[Expr], i: usize, cx: &Ctx) -> Value {
    if given(args, i) { scalar(eval_val(&args[i], cx), cx) } else { Value::Error(ErrorKind::NA) }
}

fn lambda_arg(args: &[Expr], i: usize, cx: &Ctx) -> EvalResult<Arc<Lambda>> {
    match args.get(i).map(|e| eval_val(e, cx)) {
        Some(Val::L(l)) => Ok(l),
        Some(Val::S(Value::Error(e))) => Err(e),
        _ => Err(ErrorKind::Value),
    }
}

fn check_size(rows: usize, cols: usize) -> EvalResult<()> {
    if rows == 0 || cols == 0 {
        return Err(ErrorKind::Calc);
    }
    if rows.saturating_mul(cols) > MAX_ARRAY_CELLS {
        return Err(ErrorKind::Num);
    }
    Ok(())
}

fn out(a: Array) -> Val {
    Val::A(Arc::new(a))
}

fn transpose(a: &Array) -> Array {
    let mut data = Vec::with_capacity(a.data.len());
    for c in 0..a.cols {
        for r in 0..a.rows {
            data.push(a.get(r, c).clone());
        }
    }
    Array::new(a.cols, a.rows, data)
}

/// Sort order of SORT / SORTBY: numbers, text, logicals, errors, blanks last.
fn sort_cmp(a: &Value, b: &Value) -> Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Number(_) => 0,
            Value::Text(_) => 1,
            Value::Bool(_) => 2,
            Value::Error(_) => 3,
            Value::Empty => 4,
        }
    }
    match rank(a).cmp(&rank(b)) {
        Ordering::Equal if rank(a) < 3 => compare(a, b),
        o => o,
    }
}

/// Key for UNIQUE: values equal the way Excel compares them (text ignores case).
fn unique_key(vals: &[Value]) -> Vec<(u8, String)> {
    vals.iter()
        .map(|v| match v {
            Value::Empty => (0, String::new()),
            Value::Number(n) => (1, format!("{:?}", n + 0.0)),
            Value::Text(t) => (2, t.to_lowercase()),
            Value::Bool(b) => (3, b.to_string()),
            Value::Error(e) => (4, e.to_string()),
        })
        .collect()
}

/// Row (or column, `by_col`) indexes of `a` in `order`, as a new array.
fn pick(a: &Array, idx: &[usize], by_col: bool) -> Array {
    if by_col {
        let mut data = Vec::with_capacity(a.rows * idx.len());
        for r in 0..a.rows {
            for &c in idx {
                data.push(a.get(r, c).clone());
            }
        }
        Array::new(a.rows, idx.len(), data)
    } else {
        Array::new(idx.len(), a.cols, idx.iter().flat_map(|&r| a.row(r).iter().cloned()).collect())
    }
}

/// 1-based positions (negative counts from the end) to 0-based indexes.
fn positions(vals: &[Value], n: usize) -> EvalResult<Vec<usize>> {
    let mut out = Vec::new();
    for v in vals {
        let k = v.to_number()?.trunc() as i64;
        let i = if k > 0 { k - 1 } else if k < 0 { n as i64 + k } else { -1 };
        if i < 0 || i >= n as i64 {
            return Err(ErrorKind::Value);
        }
        out.push(i as usize);
    }
    Ok(out)
}

/// IF with an array condition: pick element by element.
pub(crate) fn if_array(cond: Val, args: &[Expr], cx: &Ctx) -> EvalResult<Val> {
    arg_count(args, 2, 3)?;
    let c = to_array(cond, cx)?;
    let t = to_array(eval_val(&args[1], cx), cx)?;
    let f = match args.get(2) {
        None => Arc::new(Array::scalar(Value::Bool(false))),
        Some(Expr::Missing) => Arc::new(Array::scalar(Value::Number(0.0))),
        Some(e) => to_array(eval_val(e, cx), cx)?,
    };
    Ok(out(broadcast(&[&c, &t, &f], |v| match v[0].to_bool() {
        Ok(true) => v[1].clone(),
        Ok(false) => v[2].clone(),
        Err(e) => Value::Error(e),
    })))
}

/// Split `text` at any of `delims` (case-insensitively if `fold`).
fn split_any(text: &str, delims: &[String], fold: bool) -> Vec<String> {
    let delims: Vec<&String> = delims.iter().filter(|d| !d.is_empty()).collect();
    if delims.is_empty() {
        return vec![text.to_string()];
    }
    let hay = if fold { text.to_lowercase() } else { text.to_string() };
    // Lowercasing can change byte lengths; fall back to exact matching then.
    let (hay, fold) = if hay.len() == text.len() { (hay, fold) } else { (text.to_string(), false) };
    let needles: Vec<String> = delims.iter().map(|d| if fold { d.to_lowercase() } else { (*d).clone() }).collect();
    let mut parts = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < hay.len() {
        if let Some(n) = needles.iter().filter(|n| hay[i..].starts_with(n.as_str())).max_by_key(|n| n.len()) {
            parts.push(text[start..i].to_string());
            i += n.len();
            start = i;
        } else {
            i += hay[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    parts.push(text[start..].to_string());
    parts
}

fn delims(args: &[Expr], i: usize, cx: &Ctx) -> EvalResult<Vec<String>> {
    if !given(args, i) {
        return Ok(Vec::new());
    }
    to_array(eval_val(&args[i], cx), cx)?.data.iter().map(|v| v.to_text().map(|t| t.to_string())).collect()
}

/// Call a LAMBDA once per value and require one value back.
fn call_one(cx: &Ctx, l: &Lambda, args: Vec<Val>) -> Value {
    match cx.invoke(l, args) {
        Val::A(a) if !a.is_scalar() => Value::Error(ErrorKind::Calc),
        v => scalar(v, cx),
    }
}

pub(crate) fn call(f: Func, args: &[Expr], cx: &Ctx) -> EvalResult<Val> {
    use Func::*;
    Ok(match f {
        Filter => {
            arg_count(args, 2, 3)?;
            let a = arr(args, 0, cx)?;
            let inc = arr(args, 1, cx)?;
            let by_col = if inc.cols == 1 && inc.rows == a.rows {
                false
            } else if inc.rows == 1 && inc.cols == a.cols {
                true
            } else {
                return Err(ErrorKind::Value);
            };
            let n = if by_col { a.cols } else { a.rows };
            let mut keep = Vec::new();
            for (i, v) in inc.data.iter().enumerate().take(n) {
                if v.to_bool()? {
                    keep.push(i);
                }
            }
            if keep.is_empty() {
                return if given(args, 2) { Ok(eval_val(&args[2], cx)) } else { Err(ErrorKind::Calc) };
            }
            out(pick(&a, &keep, by_col))
        }
        Sort => {
            arg_count(args, 1, 4)?;
            let a = arr(args, 0, cx)?;
            let by_col = opt_bool(args, 3, cx, false)?;
            let a = if by_col { Arc::new(transpose(&a)) } else { a };
            let keys: Vec<usize> = if given(args, 1) { positions(&to_array(eval_val(&args[1], cx), cx)?.data, a.cols)? } else { vec![0] };
            let orders: Vec<f64> = if given(args, 2) { to_array(eval_val(&args[2], cx), cx)?.data.iter().map(Value::to_number).collect::<EvalResult<_>>()? } else { vec![1.0] };
            if orders.iter().any(|o| *o != 1.0 && *o != -1.0) {
                return Err(ErrorKind::Value);
            }
            let mut idx: Vec<usize> = (0..a.rows).collect();
            idx.sort_by(|&x, &y| {
                for (k, &col) in keys.iter().enumerate() {
                    let desc = orders.get(k).or(orders.last()).copied().unwrap_or(1.0) < 0.0;
                    let o = sort_cmp(a.get(x, col), a.get(y, col));
                    let o = if desc && !(a.get(x, col).is_empty() || a.get(y, col).is_empty()) { o.reverse() } else { o };
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                Ordering::Equal
            });
            let sorted = pick(&a, &idx, false);
            out(if by_col { transpose(&sorted) } else { sorted })
        }
        SortBy => {
            if args.len() < 2 {
                return Err(ErrorKind::Value);
            }
            let a = arr(args, 0, cx)?;
            let mut keys: Vec<(Arc<Array>, bool)> = Vec::new();
            let mut by_col = None;
            let mut i = 1;
            while i < args.len() {
                let k = arr(args, i, cx)?;
                let desc = opt_num(args, i + 1, cx, 1.0)? < 0.0;
                let col = if k.cols == 1 && k.rows == a.rows {
                    false
                } else if k.rows == 1 && k.cols == a.cols {
                    true
                } else {
                    return Err(ErrorKind::Value);
                };
                if by_col.is_some_and(|b| b != col) {
                    return Err(ErrorKind::Value);
                }
                by_col = Some(col);
                keys.push((k, desc));
                i += 2;
            }
            let by_col = by_col.unwrap_or(false);
            let n = if by_col { a.cols } else { a.rows };
            let mut idx: Vec<usize> = (0..n).collect();
            idx.sort_by(|&x, &y| {
                for (k, desc) in &keys {
                    let (vx, vy) = (&k.data[x], &k.data[y]);
                    let o = sort_cmp(vx, vy);
                    let o = if *desc && !(vx.is_empty() || vy.is_empty()) { o.reverse() } else { o };
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                Ordering::Equal
            });
            out(pick(&a, &idx, by_col))
        }
        Unique => {
            arg_count(args, 1, 3)?;
            let a = arr(args, 0, cx)?;
            let by_col = opt_bool(args, 1, cx, false)?;
            let once = opt_bool(args, 2, cx, false)?;
            let t = if by_col { transpose(&a) } else { (*a).clone() };
            let mut counts: std::collections::HashMap<Vec<(u8, String)>, usize> = std::collections::HashMap::new();
            for r in 0..t.rows {
                *counts.entry(unique_key(t.row(r))).or_default() += 1;
            }
            let mut seen = std::collections::HashSet::new();
            let mut keep = Vec::new();
            for r in 0..t.rows {
                let k = unique_key(t.row(r));
                if (once && counts[&k] == 1) || (!once && seen.insert(k)) {
                    keep.push(r);
                }
            }
            if keep.is_empty() {
                return Err(ErrorKind::Calc);
            }
            let u = pick(&t, &keep, false);
            out(if by_col { transpose(&u) } else { u })
        }
        Sequence => {
            arg_count(args, 1, 4)?;
            let rows = opt_num(args, 0, cx, 1.0)?.trunc();
            let cols = opt_num(args, 1, cx, 1.0)?.trunc();
            let (start, step) = (opt_num(args, 2, cx, 1.0)?, opt_num(args, 3, cx, 1.0)?);
            if rows < 1.0 || cols < 1.0 {
                return Err(if rows == 0.0 || cols == 0.0 { ErrorKind::Calc } else { ErrorKind::Value });
            }
            let (rows, cols) = (rows as usize, cols as usize);
            check_size(rows, cols)?;
            out(Array::new(rows, cols, (0..rows * cols).map(|i| Value::num(start + step * i as f64)).collect()))
        }
        RandArray => {
            arg_count(args, 0, 5)?;
            let rows = opt_num(args, 0, cx, 1.0)?.trunc();
            let cols = opt_num(args, 1, cx, 1.0)?.trunc();
            let (lo, hi) = (opt_num(args, 2, cx, 0.0)?, opt_num(args, 3, cx, 1.0)?);
            let int = opt_bool(args, 4, cx, false)?;
            if rows < 1.0 || cols < 1.0 || lo > hi {
                return Err(ErrorKind::Value);
            }
            let (rows, cols) = (rows as usize, cols as usize);
            check_size(rows, cols)?;
            let (lo, hi) = if int { (lo.ceil(), hi.floor()) } else { (lo, hi) };
            if lo > hi {
                return Err(ErrorKind::Value);
            }
            let data = (0..rows * cols).map(|_| Value::Number(if int { lo + (random() * (hi - lo + 1.0)).floor() } else { lo + random() * (hi - lo) })).collect();
            out(Array::new(rows, cols, data))
        }
        Transpose => {
            arg_count(args, 1, 1)?;
            out(transpose(arr(args, 0, cx)?.as_ref()))
        }
        TextSplit => {
            arg_count(args, 2, 6)?;
            let text = text_arg(args, 0, cx)?;
            let (cd, rd) = (delims(args, 1, cx)?, delims(args, 2, cx)?);
            if cd.iter().all(String::is_empty) && rd.iter().all(String::is_empty) {
                return Err(ErrorKind::Value);
            }
            let ignore = opt_bool(args, 3, cx, false)?;
            let fold = opt_num(args, 4, cx, 0.0)? == 1.0;
            let pad = pad_arg(args, 5, cx);
            let mut rows: Vec<Vec<String>> = split_any(&text, &rd, fold).into_iter().map(|line| split_any(&line, &cd, fold)).collect();
            if ignore {
                for r in &mut rows {
                    r.retain(|s| !s.is_empty());
                }
                rows.retain(|r| !r.is_empty());
                if rows.is_empty() {
                    return Err(ErrorKind::Calc);
                }
            }
            let cols = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
            let data: Vec<Vec<Value>> = rows.into_iter().map(|r| (0..cols).map(|i| r.get(i).map_or(pad.clone(), |s| Value::text(s.as_str()))).collect()).collect();
            out(Array::from_rows(data))
        }
        VStack | HStack => {
            if args.is_empty() {
                return Err(ErrorKind::Value);
            }
            let parts: Vec<Arc<Array>> = (0..args.len()).map(|i| arr(args, i, cx)).collect::<EvalResult<_>>()?;
            let na = Value::Error(ErrorKind::NA);
            if f == VStack {
                let cols = parts.iter().map(|a| a.cols).max().unwrap_or(1);
                let rows: usize = parts.iter().map(|a| a.rows).sum();
                check_size(rows, cols)?;
                let mut data = Vec::with_capacity(rows * cols);
                for a in &parts {
                    for r in 0..a.rows {
                        data.extend((0..cols).map(|c| if c < a.cols { a.get(r, c).clone() } else { na.clone() }));
                    }
                }
                out(Array::new(rows, cols, data))
            } else {
                let rows = parts.iter().map(|a| a.rows).max().unwrap_or(1);
                let cols: usize = parts.iter().map(|a| a.cols).sum();
                check_size(rows, cols)?;
                let mut data = Vec::with_capacity(rows * cols);
                for r in 0..rows {
                    for a in &parts {
                        data.extend((0..a.cols).map(|c| if r < a.rows { a.get(r, c).clone() } else { na.clone() }));
                    }
                }
                out(Array::new(rows, cols, data))
            }
        }
        Take | Drop => {
            arg_count(args, 2, 3)?;
            let a = arr(args, 0, cx)?;
            let (r, c) = (opt_int(args, 1, cx)?, opt_int(args, 2, cx)?);
            // The kept span [lo, hi) along an axis of length n.
            let span = |k: Option<i64>, n: usize| -> (usize, usize) {
                let n = n as i64;
                match (k, f) {
                    (None, _) => (0, n as usize),
                    (Some(k), Take) if k >= 0 => (0, k.min(n) as usize),
                    (Some(k), Take) => ((n + k).max(0) as usize, n as usize),
                    (Some(k), _) if k >= 0 => (k.min(n) as usize, n as usize),
                    (Some(k), _) => (0, (n + k).max(0) as usize),
                }
            };
            let ((r0, r1), (c0, c1)) = (span(r, a.rows), span(c, a.cols));
            if r1 <= r0 || c1 <= c0 {
                return Err(ErrorKind::Calc);
            }
            let rows: Vec<usize> = (r0..r1).collect();
            let cols: Vec<usize> = (c0..c1).collect();
            out(pick(&pick(&a, &rows, false), &cols, true))
        }
        ChooseCols | ChooseRows => {
            if args.len() < 2 {
                return Err(ErrorKind::Value);
            }
            let a = arr(args, 0, cx)?;
            let n = if f == ChooseCols { a.cols } else { a.rows };
            let mut idx = Vec::new();
            for i in 1..args.len() {
                idx.extend(positions(&arr(args, i, cx)?.data, n)?);
            }
            out(pick(&a, &idx, f == ChooseCols))
        }
        ToCol | ToRow => {
            arg_count(args, 1, 3)?;
            let a = arr(args, 0, cx)?;
            let ignore = opt_num(args, 1, cx, 0.0)?.trunc() as i64;
            if !(0..=3).contains(&ignore) {
                return Err(ErrorKind::Value);
            }
            let by_col = opt_bool(args, 2, cx, false)?;
            let src = if by_col { transpose(&a) } else { (*a).clone() };
            let vals: Vec<Value> = src
                .data
                .into_iter()
                .filter(|v| !((ignore & 1 != 0 && v.is_empty()) || (ignore & 2 != 0 && matches!(v, Value::Error(_)))))
                .collect();
            if vals.is_empty() {
                return Err(ErrorKind::Calc);
            }
            let n = vals.len();
            out(if f == ToCol { Array::new(n, 1, vals) } else { Array::new(1, n, vals) })
        }
        WrapRows | WrapCols => {
            arg_count(args, 2, 3)?;
            let a = arr(args, 0, cx)?;
            if a.rows != 1 && a.cols != 1 {
                return Err(ErrorKind::Value);
            }
            let w = num_arg(args, 1, cx)?.trunc();
            if w < 1.0 {
                return Err(ErrorKind::Num);
            }
            let w = w as usize;
            let pad = pad_arg(args, 2, cx);
            let n = a.data.len();
            let lines = n.div_ceil(w);
            let grid: Vec<Vec<Value>> = (0..lines).map(|l| (0..w).map(|k| a.data.get(l * w + k).cloned().unwrap_or(pad.clone())).collect()).collect();
            let rows = Array::from_rows(grid);
            out(if f == WrapRows { rows } else { transpose(&rows) })
        }
        Expand => {
            arg_count(args, 2, 4)?;
            let a = arr(args, 0, cx)?;
            let rows = opt_int(args, 1, cx)?.unwrap_or(a.rows as i64);
            let cols = opt_int(args, 2, cx)?.unwrap_or(a.cols as i64);
            if rows < a.rows as i64 || cols < a.cols as i64 {
                return Err(ErrorKind::Value);
            }
            let (rows, cols) = (rows as usize, cols as usize);
            check_size(rows, cols)?;
            let pad = pad_arg(args, 3, cx);
            let data = (0..rows).flat_map(|r| (0..cols).map(move |c| (r, c))).map(|(r, c)| if r < a.rows && c < a.cols { a.get(r, c).clone() } else { pad.clone() }).collect();
            out(Array::new(rows, cols, data))
        }

        // ---- LET / LAMBDA ----
        Let => {
            if args.len() < 3 || args.len().is_multiple_of(2) {
                return Err(ErrorKind::Value);
            }
            let mut names = Vec::new();
            for pair in args[..args.len() - 1].chunks(2) {
                match &pair[0] {
                    Expr::Name(n) => names.push((n.clone(), &pair[1])),
                    _ => return Err(ErrorKind::Value),
                }
            }
            fn bind(cx: &Ctx, names: &[(Arc<str>, &Expr)], body: &Expr) -> Val {
                match names.split_first() {
                    None => eval_val(body, cx),
                    Some(((n, e), rest)) => {
                        let v = eval_val(e, cx);
                        cx.with_local(n.clone(), v, |cx| bind(cx, rest, body))
                    }
                }
            }
            bind(cx, &names, &args[args.len() - 1])
        }
        Lambda => {
            let Some((body, params)) = args.split_last() else { return Err(ErrorKind::Value) };
            let mut ps = Vec::new();
            for p in params {
                match p {
                    Expr::Name(n) if !ps.iter().any(|x: &Arc<str>| x.eq_ignore_ascii_case(n)) => ps.push(n.clone()),
                    _ => return Err(ErrorKind::Value),
                }
            }
            Val::L(Arc::new(crate::eval::Lambda { params: ps, body: Arc::new(body.clone()), env: cx.env() }))
        }
        IsOmitted => {
            arg_count(args, 1, 1)?;
            Val::S(Value::Bool(matches!(eval_val(&args[0], cx), Val::Omitted)))
        }
        Map => {
            if args.len() < 2 {
                return Err(ErrorKind::Value);
            }
            let l = lambda_arg(args, args.len() - 1, cx)?;
            let arrays: Vec<Arc<Array>> = (0..args.len() - 1).map(|i| arr(args, i, cx)).collect::<EvalResult<_>>()?;
            let refs: Vec<&Array> = arrays.iter().map(|a| a.as_ref()).collect();
            out(broadcast(&refs, |vs| call_one(cx, &l, vs.iter().cloned().map(Val::S).collect())))
        }
        Reduce | Scan => {
            arg_count(args, 2, 3)?;
            let (init, a, l) = if args.len() == 3 { (eval_val(&args[0], cx), arr(args, 1, cx)?, lambda_arg(args, 2, cx)?) } else { (Val::S(Value::Empty), arr(args, 0, cx)?, lambda_arg(args, 1, cx)?) };
            let mut acc = match init {
                Val::Omitted => Val::S(Value::Empty),
                v => v,
            };
            let mut steps = Vec::new();
            for v in &a.data {
                acc = cx.invoke(&l, vec![acc, Val::S(v.clone())]);
                if f == Scan {
                    let s = match &acc {
                        Val::A(x) if !x.is_scalar() => Value::Error(ErrorKind::Calc),
                        v => scalar(v.clone(), cx),
                    };
                    steps.push(s);
                }
            }
            if f == Reduce { acc } else { out(Array::new(a.rows, a.cols, steps)) }
        }
        ByRow | ByCol => {
            arg_count(args, 2, 2)?;
            let a = arr(args, 0, cx)?;
            let l = lambda_arg(args, 1, cx)?;
            if f == ByRow {
                let vals = (0..a.rows).map(|r| call_one(cx, &l, vec![Val::A(Arc::new(Array::new(1, a.cols, a.row(r).to_vec())))])).collect();
                out(Array::new(a.rows, 1, vals))
            } else {
                let vals = (0..a.cols).map(|c| call_one(cx, &l, vec![Val::A(Arc::new(Array::new(a.rows, 1, (0..a.rows).map(|r| a.get(r, c).clone()).collect())))])).collect();
                out(Array::new(1, a.cols, vals))
            }
        }
        MakeArray => {
            arg_count(args, 3, 3)?;
            let (rows, cols) = (num_arg(args, 0, cx)?.trunc(), num_arg(args, 1, cx)?.trunc());
            if rows < 1.0 || cols < 1.0 {
                return Err(ErrorKind::Value);
            }
            let (rows, cols) = (rows as usize, cols as usize);
            check_size(rows, cols)?;
            let l = lambda_arg(args, 2, cx)?;
            let data = (0..rows * cols).map(|i| call_one(cx, &l, vec![Val::S(Value::Number((i / cols + 1) as f64)), Val::S(Value::Number((i % cols + 1) as f64))])).collect();
            out(Array::new(rows, cols, data))
        }
        _ => return Err(ErrorKind::Value),
    })
}
