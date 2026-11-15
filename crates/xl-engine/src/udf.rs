//! User-defined (script) functions: the extension point the evaluator
//! consults when a formula calls a function name it doesn't know.
//!
//! The engine knows nothing about any scripting language. A host registers
//! an implementation of [`UserFunctions`] with
//! [`Workbook::set_user_functions`](crate::Workbook::set_user_functions);
//! from then on `=DISCOUNT(A1, 0.1)` evaluates its arguments (ranges as 2-D
//! arrays), hands them to the host and shows what comes back. Calls take
//! part in dependency tracking like any built-in: the formula's references
//! are its precedents, so editing A1 recalculates the cell. Functions are
//! deterministic unless the host says a name is volatile.
//!
//! The workbook also carries the script sources themselves
//! ([`ScriptModule`]) so file formats can store them.

use std::sync::Arc;

use crate::address::{Addr, CellRef, Range};
use crate::eval::{Ctx, eval};
use crate::formula::Expr;
use crate::value::{ErrorKind, Value};

/// Most cells passed to a script function in one range argument.
pub const MAX_ARG_CELLS: u64 = 1_000_000;

/// A 2-D block of values, row-major.
#[derive(Clone, Debug, PartialEq)]
pub struct Array {
    pub rows: usize,
    pub cols: usize,
    pub values: Vec<Value>,
}

impl Array {
    pub fn get(&self, row: usize, col: usize) -> &Value {
        &self.values[row * self.cols + col]
    }
}

/// An argument as a script function receives it.
#[derive(Clone, Debug, PartialEq)]
pub enum UdfArg {
    Value(Value),
    /// A range reference (or a multi-cell result), as rows of values.
    Array(Array),
}

/// What a script function returns.
#[derive(Clone, Debug, PartialEq)]
pub enum UdfValue {
    Value(Value),
    /// A 2-D result. Until the engine has dynamic arrays the cell shows the
    /// top-left value; see [`UdfValue::into_cell_value`].
    Array(Array),
}

impl UdfValue {
    /// The value a single cell shows (an array's top-left value).
    ///
    /// Formulas use [`UdfValue::into_val`], which lets arrays spill.
    /// (shape `rows × cols`) so the result spills like `SEQUENCE`.
    /// As a formula result: arrays spill.
    pub(crate) fn into_val(self) -> crate::eval::Val {
        match self {
            UdfValue::Value(v) => crate::eval::Val::S(v),
            UdfValue::Array(a) if a.values.is_empty() || a.rows * a.cols != a.values.len() => crate::eval::Val::S(Value::Error(ErrorKind::Value)),
            UdfValue::Array(a) => crate::eval::Val::A(std::sync::Arc::new(crate::value::Array::new(a.rows, a.cols, a.values))),
        }
    }

    pub fn into_cell_value(self) -> Value {
        match self {
            UdfValue::Value(v) => v,
            UdfValue::Array(a) if a.values.is_empty() => Value::Error(ErrorKind::Value),
            UdfValue::Array(a) => a.values.into_iter().next().unwrap_or(Value::Empty),
        }
    }
}

/// A source of functions defined outside the engine (scripts).
///
/// Names are passed upper-cased, as the parser stores unknown functions.
/// Implementations must not call back into the workbook: they see only
/// their arguments, which keeps them deterministic and recalc-safe.
pub trait UserFunctions: Send + Sync {
    /// Is `name` a function this source defines?
    fn has(&self, name: &str) -> bool;
    /// Must cells calling `name` recalculate on every change?
    fn is_volatile(&self, name: &str) -> bool;
    /// Call `name` from the formula in cell `at`. Errors are returned as
    /// error values; the host can keep a message per cell for the UI.
    fn call(&self, name: &str, args: &[UdfArg], at: Addr) -> UdfValue;
}

/// A named script stored in the workbook.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ScriptModule {
    pub name: String,
    pub source: String,
}

/// Does this formula call a user function the source marks volatile?
pub(crate) fn calls_volatile(expr: &Expr, udfs: &dyn UserFunctions) -> bool {
    expr.any(&|e| matches!(e, Expr::UnknownFunc(n, _) if udfs.is_volatile(&n.to_ascii_uppercase())))
}

/// Do all the unknown functions of this formula exist in `udfs`?
pub(crate) fn all_known(expr: &Expr, udfs: &dyn UserFunctions) -> bool {
    !expr.any(&|e| matches!(e, Expr::UnknownFunc(n, _) if !udfs.has(&n.to_ascii_uppercase())))
}

/// Evaluate `NAME(args…)` for a name the engine doesn't define.
/// Script functions are registered upper-cased; formulas keep the case typed.
pub(crate) fn call(name: &str, args: &[Expr], cx: &Ctx) -> crate::eval::Val {
    use crate::eval::Val;
    let name = name.to_ascii_uppercase();
    let Some(udfs) = cx.udfs.filter(|u| u.has(&name)) else {
        return Val::S(Value::Error(ErrorKind::Name));
    };
    let mut values = Vec::with_capacity(args.len());
    for a in args {
        match arg(a, cx) {
            Ok(v) => values.push(v),
            Err(e) => return Val::S(Value::Error(e)),
        }
    }
    udfs.call(&name, &values, Addr::new(cx.sheet, cx.cell)).into_val()
}

fn arg(e: &Expr, cx: &Ctx) -> Result<UdfArg, ErrorKind> {
    let Some(rr) = cx.resolve(e) else { return Ok(UdfArg::Value(eval(e, cx))) };
    let rr = rr?;
    let r = rr.range;
    if r.start == r.end {
        return Ok(UdfArg::Value(cx.value(rr.sheet, r.start).clone()));
    }
    // Whole columns/rows stop at the used area.
    let r = if r.rows() == crate::address::MAX_ROWS || r.cols() == crate::address::MAX_COLS { cx.clamp_used(rr).range } else { r };
    if r.rows() as u64 * r.cols() as u64 > MAX_ARG_CELLS {
        return Err(ErrorKind::Value);
    }
    Ok(UdfArg::Array(block(cx, rr.sheet, r)))
}

fn block(cx: &Ctx, sheet: crate::address::SheetId, r: Range) -> Array {
    let (rows, cols) = (r.rows() as usize, r.cols() as usize);
    let mut values = Vec::with_capacity(rows * cols);
    for row in r.start.row..=r.end.row {
        for col in r.start.col..=r.end.col {
            values.push(cx.value(sheet, CellRef::new(row, col)).clone());
        }
    }
    Array { rows, cols, values }
}

/// A shared handle to a function source, as the workbook keeps it.
pub type UserFunctionsRef = Arc<dyn UserFunctions>;
