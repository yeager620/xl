//! Cell values to Lua and back.
//!
//! | cell                    | Lua                                         |
//! |-------------------------|---------------------------------------------|
//! | number                  | number                                      |
//! | text                    | string                                      |
//! | TRUE/FALSE              | boolean                                     |
//! | empty                   | nil                                         |
//! | error (`#N/A`, …)       | `xl.error("#N/A")` (test with `xl.iserror`) |
//! | range                   | `t[row][col]` (1-based), plus `t.rows`, `t.cols` |
//!
//! Returning a table of rows gives a 2-D result; a flat list is one row.

use mlua::{IntoLua, Lua, Table, Value as Lv};
use xl_engine::udf::Array;
use xl_engine::{ErrorKind, UdfArg, UdfValue, Value};

pub(crate) fn to_lua(lua: &Lua, errmeta: &Table, v: &Value) -> mlua::Result<Lv> {
    Ok(match v {
        Value::Empty => Lv::Nil,
        Value::Number(n) => Lv::Number(*n),
        Value::Text(t) => t.as_ref().into_lua(lua)?,
        Value::Bool(b) => Lv::Boolean(*b),
        Value::Error(e) => {
            let t = lua.create_table()?;
            t.raw_set("error", e.as_str())?;
            t.set_metatable(Some(errmeta.clone()))?;
            Lv::Table(t)
        }
    })
}

pub(crate) fn array_to_lua(lua: &Lua, errmeta: &Table, a: &Array) -> mlua::Result<Lv> {
    let t = lua.create_table_with_capacity(a.rows, 2)?;
    for r in 0..a.rows {
        let row = lua.create_table_with_capacity(a.cols, 0)?;
        for c in 0..a.cols {
            row.raw_set(c + 1, to_lua(lua, errmeta, a.get(r, c))?)?;
        }
        t.raw_set(r + 1, row)?;
    }
    t.raw_set("rows", a.rows)?;
    t.raw_set("cols", a.cols)?;
    Ok(Lv::Table(t))
}

pub(crate) fn arg_to_lua(lua: &Lua, errmeta: &Table, a: &UdfArg) -> mlua::Result<Lv> {
    match a {
        UdfArg::Value(v) => to_lua(lua, errmeta, v),
        UdfArg::Array(a) => array_to_lua(lua, errmeta, a),
    }
}

fn is_error(t: &Table, errmeta: &Table) -> bool {
    t.metatable().is_some_and(|m| m == *errmeta)
}

/// A scalar from Lua; Err(message) for things a cell can't hold.
pub(crate) fn from_lua_scalar(v: &Lv, errmeta: &Table) -> Result<Value, String> {
    Ok(match v {
        Lv::Nil => Value::Empty,
        Lv::Boolean(b) => Value::Bool(*b),
        Lv::Integer(i) => Value::Number(*i as f64),
        Lv::Number(n) => Value::num(*n),
        Lv::String(s) => Value::text(s.to_string_lossy()),
        Lv::Table(t) if is_error(t, errmeta) => {
            let code: String = t.raw_get("error").unwrap_or_default();
            Value::Error(ErrorKind::parse(&code).unwrap_or(ErrorKind::Value))
        }
        Lv::Table(_) => return Err("a table can't go in one cell".into()),
        other => return Err(format!("a {} can't go in a cell", other.type_name())),
    })
}

/// A table of rows (or a flat list, as one row) as an array.
pub(crate) fn table_to_array(t: &Table, errmeta: &Table) -> Result<Array, String> {
    let n = t.raw_len();
    if n == 0 {
        return Ok(Array { rows: 1, cols: 1, values: vec![Value::Empty] });
    }
    let first: Lv = t.raw_get(1).map_err(|e| e.to_string())?;
    let two_d = matches!(&first, Lv::Table(r) if !is_error(r, errmeta));
    if !two_d {
        let values = (1..=n).map(|i| from_lua_scalar(&t.raw_get::<Lv>(i).unwrap_or(Lv::Nil), errmeta)).collect::<Result<Vec<_>, _>>()?;
        return Ok(Array { rows: 1, cols: n, values });
    }
    let mut rows = Vec::with_capacity(n);
    for i in 1..=n {
        match t.raw_get::<Lv>(i).unwrap_or(Lv::Nil) {
            Lv::Table(r) if !is_error(&r, errmeta) => rows.push(r),
            _ => return Err(format!("row {i} of the result isn't a table")),
        }
    }
    let cols = rows.iter().map(Table::raw_len).max().unwrap_or(0).max(1);
    if n.saturating_mul(cols) > xl_engine::udf::MAX_ARG_CELLS as usize {
        return Err("the result is too big".into());
    }
    let mut values = Vec::with_capacity(n * cols);
    for r in &rows {
        for c in 1..=cols {
            values.push(from_lua_scalar(&r.raw_get::<Lv>(c).unwrap_or(Lv::Nil), errmeta)?);
        }
    }
    Ok(Array { rows: n, cols, values })
}

/// A function's return value.
pub(crate) fn result_from_lua(v: &Lv, errmeta: &Table) -> Result<UdfValue, String> {
    match v {
        Lv::Table(t) if !is_error(t, errmeta) => table_to_array(t, errmeta).map(|a| if a.rows == 1 && a.cols == 1 { UdfValue::Value(a.values.into_iter().next().unwrap()) } else { UdfValue::Array(a) }),
        other => from_lua_scalar(other, errmeta).map(UdfValue::Value),
    }
}
