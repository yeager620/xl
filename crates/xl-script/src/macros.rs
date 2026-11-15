//! Macros: scripts run on demand with the `xl` automation API.
//!
//! | Lua                                   | does                                              |
//! |---------------------------------------|---------------------------------------------------|
//! | `xl.get(ref)`                         | value of a cell, or `t[row][col]` for a range     |
//! | `xl.set(ref, v)`                      | write a value (a scalar fills a range; a table of rows writes a block) |
//! | `xl.formula(ref)`                     | a cell's formula (`"=A1*2"`) or nil               |
//! | `xl.set_formula(ref, f)`              | enter a formula (or any input, as typed)          |
//! | `xl.selection()` / `xl.select(ref)`   | the selected range ("B2:C5") / select one         |
//! | `xl.sheets()` / `xl.sheet()`          | sheet names / the active sheet's name             |
//! | `xl.add_sheet([name])`                | add a sheet, returns its name                     |
//! | `xl.rename_sheet(old, new)`           | rename a sheet                                    |
//! | `xl.activate(name)`                   | switch to a sheet                                 |
//! | `xl.bold(ref, on)`                    | bold on/off                                       |
//! | `xl.fill(ref, "#RRGGBB" or nil)`      | background fill                                   |
//! | `xl.number_format(ref, code or nil)`  | number format ("0.00%", "m/d/yyyy", …)            |
//! | `xl.insert_rows(row, [n])` / `xl.delete_rows(row, [n])` | on the active sheet, 1-based  |
//! | `xl.address(row, col)`                | "B3" for (3, 2)                                   |
//! | `xl.message(text)`                    | show a message when the macro finishes            |
//! | `print(...)`                          | write to the script output                        |
//!
//! References are A1 text ("B3", "A1:C9", "Data!B2") or defined names,
//! relative to the active sheet. Every change goes through the
//! [`MacroHost`], which in the app makes the whole run one undo step.

use std::cell::RefCell;

use mlua::{Function, Lua, Table, Value as Lv};
use xl_engine::udf::Array;
use xl_engine::{Addr, CellRef, Range, Rgb, SheetId, Shift, Value, Workbook};

use crate::sandbox::{Limits, Sandbox};
use crate::{ScriptError, ScriptModule, convert};

/// What a macro can do to the workbook it runs on.
pub trait MacroHost {
    /// Values of a cell (1×1) or range.
    fn get(&mut self, reference: &str) -> Result<Array, String>;
    /// Write values: a 1×1 block fills the whole range, a bigger one is
    /// written from the range's top-left corner.
    fn set(&mut self, reference: &str, values: &Array) -> Result<(), String>;
    fn formula(&mut self, reference: &str) -> Result<Option<String>, String>;
    /// Enter input as if typed ("=SUM(A1:A3)", "12%", "hello").
    fn set_formula(&mut self, reference: &str, input: &str) -> Result<(), String>;
    fn selection(&mut self) -> String;
    fn select(&mut self, reference: &str) -> Result<(), String>;
    fn sheets(&mut self) -> Vec<String>;
    fn active_sheet(&mut self) -> String;
    fn add_sheet(&mut self, name: Option<&str>) -> Result<String, String>;
    fn rename_sheet(&mut self, from: &str, to: &str) -> Result<(), String>;
    fn activate_sheet(&mut self, name: &str) -> Result<(), String>;
    fn set_bold(&mut self, reference: &str, on: bool) -> Result<(), String>;
    fn set_fill(&mut self, reference: &str, color: Option<Rgb>) -> Result<(), String>;
    fn set_number_format(&mut self, reference: &str, code: Option<&str>) -> Result<(), String>;
    /// Insert `count` rows before 0-based row `at` of the active sheet.
    fn insert_rows(&mut self, at: u32, count: u32) -> Result<(), String>;
    fn delete_rows(&mut self, at: u32, count: u32) -> Result<(), String>;
    fn message(&mut self, text: &str);
}

/// How a macro run went.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MacroOutcome {
    /// Lines printed.
    pub output: Vec<String>,
    pub error: Option<ScriptError>,
}

/// Run the macro `name` defined by `modules` against `host`.
pub fn run_macro(modules: &[ScriptModule], name: &str, host: &mut dyn MacroHost, limits: Limits) -> MacroOutcome {
    let sandbox = match Sandbox::new(limits) {
        Ok(s) => s,
        Err(e) => return MacroOutcome { output: Vec::new(), error: Some(e) },
    };
    for m in modules {
        if let Err(e) = sandbox.load_module(&m.name, &m.source) {
            return MacroOutcome { output: sandbox.take_output(), error: Some(e) };
        }
    }
    let func: Option<Function> = sandbox.macros.get::<Table>(name).and_then(|t| t.get("fn")).ok();
    let Some(func) = func else {
        return MacroOutcome { output: sandbox.take_output(), error: Some(ScriptError::plain(format!("there's no macro named “{name}”"))) };
    };
    let host = RefCell::new(host);
    let result = sandbox.timed(|| {
        sandbox.lua.scope(|scope| {
            let xl: Table = sandbox.lua.globals().get("xl")?;
            let errmeta = sandbox.errmeta.clone();
            let h = &host;
            xl.set(
                "get",
                scope.create_function(move |lua: &Lua, r: String| {
                    let a = h.borrow_mut().get(&r).map_err(|e| located(lua, e))?;
                    if a.rows == 1 && a.cols == 1 { convert::to_lua(lua, &errmeta, &a.values[0]) } else { convert::array_to_lua(lua, &errmeta, &a) }
                })?,
            )?;
            let errmeta = sandbox.errmeta.clone();
            xl.set(
                "set",
                scope.create_function(move |lua: &Lua, (r, v): (String, Lv)| {
                    let a = match &v {
                        Lv::Table(t) if t.metatable().is_none() => convert::table_to_array(t, &errmeta).map_err(|e| located(lua, e))?,
                        other => Array { rows: 1, cols: 1, values: vec![convert::from_lua_scalar(other, &errmeta).map_err(|e| located(lua, e))?] },
                    };
                    h.borrow_mut().set(&r, &a).map_err(|e| located(lua, e))
                })?,
            )?;
            xl.set("formula", scope.create_function(move |lua: &Lua, r: String| h.borrow_mut().formula(&r).map_err(|e| located(lua, e)))?)?;
            xl.set("set_formula", scope.create_function(move |lua: &Lua, (r, f): (String, String)| h.borrow_mut().set_formula(&r, &f).map_err(|e| located(lua, e)))?)?;
            xl.set("selection", scope.create_function(move |_: &Lua, ()| Ok(h.borrow_mut().selection()))?)?;
            xl.set("select", scope.create_function(move |lua: &Lua, r: String| h.borrow_mut().select(&r).map_err(|e| located(lua, e)))?)?;
            xl.set("sheets", scope.create_function(move |_: &Lua, ()| Ok(h.borrow_mut().sheets()))?)?;
            xl.set("sheet", scope.create_function(move |_: &Lua, ()| Ok(h.borrow_mut().active_sheet()))?)?;
            xl.set("add_sheet", scope.create_function(move |lua: &Lua, n: Option<String>| h.borrow_mut().add_sheet(n.as_deref()).map_err(|e| located(lua, e)))?)?;
            xl.set("rename_sheet", scope.create_function(move |lua: &Lua, (a, b): (String, String)| h.borrow_mut().rename_sheet(&a, &b).map_err(|e| located(lua, e)))?)?;
            xl.set("activate", scope.create_function(move |lua: &Lua, n: String| h.borrow_mut().activate_sheet(&n).map_err(|e| located(lua, e)))?)?;
            xl.set("bold", scope.create_function(move |lua: &Lua, (r, on): (String, Option<bool>)| h.borrow_mut().set_bold(&r, on.unwrap_or(true)).map_err(|e| located(lua, e)))?)?;
            xl.set(
                "fill",
                scope.create_function(move |lua: &Lua, (r, c): (String, Option<String>)| {
                    let color = match c.as_deref() {
                        None | Some("") => None,
                        Some(text) => Some(parse_color(text).ok_or_else(|| located(lua, format!("“{text}” isn't a color like \"#FFCC00\"")))?),
                    };
                    h.borrow_mut().set_fill(&r, color).map_err(|e| located(lua, e))
                })?,
            )?;
            xl.set("number_format", scope.create_function(move |lua: &Lua, (r, c): (String, Option<String>)| h.borrow_mut().set_number_format(&r, c.as_deref()).map_err(|e| located(lua, e)))?)?;
            xl.set(
                "insert_rows",
                scope.create_function(move |lua: &Lua, (row, n): (i64, Option<i64>)| {
                    let (at, n) = row_args(row, n).map_err(|e| located(lua, e))?;
                    h.borrow_mut().insert_rows(at, n).map_err(|e| located(lua, e))
                })?,
            )?;
            xl.set(
                "delete_rows",
                scope.create_function(move |lua: &Lua, (row, n): (i64, Option<i64>)| {
                    let (at, n) = row_args(row, n).map_err(|e| located(lua, e))?;
                    h.borrow_mut().delete_rows(at, n).map_err(|e| located(lua, e))
                })?,
            )?;
            xl.set("message", scope.create_function(move |lua: &Lua, v: Lv| {
                let text = lua.coerce_string(v)?.map(|s| s.to_string_lossy()).unwrap_or_default();
                h.borrow_mut().message(&text);
                Ok(())
            })?)?;
            func.call::<()>(())
        })
    });
    MacroOutcome { output: sandbox.take_output(), error: result.err() }
}

/// An error pointing at the script line that called the API.
fn located(lua: &Lua, msg: String) -> mlua::Error {
    let at = lua.inspect_stack(1, |d| (d.source().short_src.map(|s| s.into_owned()), d.current_line()));
    match at {
        Some((Some(src), Some(line))) => mlua::Error::runtime(format!("{src}:{line}: {msg}")),
        _ => mlua::Error::runtime(msg),
    }
}

fn row_args(row: i64, n: Option<i64>) -> Result<(u32, u32), String> {
    let n = n.unwrap_or(1);
    if row < 1 || row > xl_engine::MAX_ROWS as i64 {
        return Err(format!("row {row} is outside the sheet"));
    }
    if n < 1 || n > xl_engine::MAX_ROWS as i64 {
        return Err(format!("can't insert or delete {n} rows"));
    }
    Ok((row as u32 - 1, n as u32))
}

/// "#RRGGBB" or "RRGGBB".
pub fn parse_color(text: &str) -> Option<Rgb> {
    let hex = text.trim().trim_start_matches('#');
    (hex.len() == 6).then(|| u32::from_str_radix(hex, 16).ok()).flatten()
}

// ----------------------------------------------------------- helpers

/// Resolve a reference relative to `sheet`.
pub fn resolve(wb: &Workbook, sheet: SheetId, reference: &str) -> Result<(SheetId, Range), String> {
    wb.resolve_reference(reference, sheet).ok_or_else(|| format!("“{reference}” isn't a cell, range or name"))
}

/// Values of a block of cells.
pub fn read_block(wb: &Workbook, sheet: SheetId, r: Range) -> Result<Array, String> {
    if r.rows() as u64 * r.cols() as u64 > xl_engine::udf::MAX_ARG_CELLS {
        return Err("that range is too big to read at once".into());
    }
    let mut values = Vec::new();
    for row in r.start.row..=r.end.row {
        for col in r.start.col..=r.end.col {
            values.push(wb.value(Addr::new(sheet, CellRef::new(row, col))).clone());
        }
    }
    Ok(Array { rows: r.rows() as usize, cols: r.cols() as usize, values })
}

/// The inputs that put `values` into `r` (see [`MacroHost::set`]).
pub fn block_inputs(sheet: SheetId, r: Range, values: &Array) -> Result<Vec<(Addr, String)>, String> {
    let mut out = Vec::new();
    if values.rows == 1 && values.cols == 1 {
        if r.rows() as u64 * r.cols() as u64 > xl_engine::udf::MAX_ARG_CELLS {
            return Err("that range is too big to fill at once".into());
        }
        let input = value_input(&values.values[0]);
        for row in r.start.row..=r.end.row {
            for col in r.start.col..=r.end.col {
                out.push((Addr::new(sheet, CellRef::new(row, col)), input.clone()));
            }
        }
        return Ok(out);
    }
    for i in 0..values.rows {
        for j in 0..values.cols {
            let cell = r.start.offset(i as i64, j as i64).ok_or("the values run off the sheet")?;
            out.push((Addr::new(sheet, cell), value_input(values.get(i, j))));
        }
    }
    Ok(out)
}

/// Typed input that reproduces a value exactly (text that would be read as
/// something else is quoted).
pub fn value_input(v: &Value) -> String {
    match v {
        Value::Number(n) => xl_engine::precise(*n),
        Value::Text(t) if t.starts_with('=') || t.starts_with('\'') || xl_engine::input::classify_literal(t).is_some() => format!("'{t}"),
        Value::Text(t) => t.to_string(),
        Value::Empty => String::new(),
        other => other.display(),
    }
}

/// Every cell of a range, capped.
pub fn cells_of(sheet: SheetId, r: Range) -> Result<Vec<Addr>, String> {
    if r.rows() as u64 * r.cols() as u64 > xl_engine::udf::MAX_ARG_CELLS {
        return Err("that range is too big to format at once".into());
    }
    Ok((r.start.row..=r.end.row).flat_map(|row| (r.start.col..=r.end.col).map(move |col| Addr::new(sheet, CellRef::new(row, col)))).collect())
}

/// A [`MacroHost`] working directly on a workbook (no undo): for headless
/// use and tests.
pub struct WorkbookHost<'a> {
    pub wb: &'a mut Workbook,
    pub sheet: SheetId,
    pub selection: Range,
    pub messages: Vec<String>,
}

impl<'a> WorkbookHost<'a> {
    pub fn new(wb: &'a mut Workbook) -> Self {
        let sheet = wb.sheet_ids()[0];
        WorkbookHost { wb, sheet, selection: Range::single(CellRef::new(0, 0)), messages: Vec::new() }
    }

    fn sheet_named(&self, name: &str) -> Result<SheetId, String> {
        self.wb.sheet_id(name).ok_or_else(|| format!("there's no sheet named “{name}”"))
    }

    fn restyle(&mut self, reference: &str, f: impl Fn(&mut xl_engine::Style)) -> Result<(), String> {
        let (sheet, r) = resolve(self.wb, self.sheet, reference)?;
        for a in cells_of(sheet, r)? {
            let mut st = self.wb.style(a).clone();
            f(&mut st);
            self.wb.set_style(a, &st);
        }
        Ok(())
    }
}

impl MacroHost for WorkbookHost<'_> {
    fn get(&mut self, reference: &str) -> Result<Array, String> {
        let (sheet, r) = resolve(self.wb, self.sheet, reference)?;
        read_block(self.wb, sheet, r)
    }

    fn set(&mut self, reference: &str, values: &Array) -> Result<(), String> {
        let (sheet, r) = resolve(self.wb, self.sheet, reference)?;
        self.wb.set_inputs(block_inputs(sheet, r, values)?);
        Ok(())
    }

    fn formula(&mut self, reference: &str) -> Result<Option<String>, String> {
        let (sheet, r) = resolve(self.wb, self.sheet, reference)?;
        let a = Addr::new(sheet, r.start);
        Ok(self.wb.cell(a).filter(|c| c.is_formula() || matches!(c.input, xl_engine::CellInput::Opaque(_))).map(|_| self.wb.input_text(a)))
    }

    fn set_formula(&mut self, reference: &str, input: &str) -> Result<(), String> {
        let (sheet, r) = resolve(self.wb, self.sheet, reference)?;
        self.wb.set_input(Addr::new(sheet, r.start), input).map(|_| ()).map_err(|e| format!("formula error: {e}"))
    }

    fn selection(&mut self) -> String {
        range_text(self.selection)
    }

    fn select(&mut self, reference: &str) -> Result<(), String> {
        let (sheet, r) = resolve(self.wb, self.sheet, reference)?;
        self.sheet = sheet;
        self.selection = r;
        Ok(())
    }

    fn sheets(&mut self) -> Vec<String> {
        self.wb.sheet_ids().iter().map(|&id| self.wb.sheet_name(id).to_string()).collect()
    }

    fn active_sheet(&mut self) -> String {
        self.wb.sheet_name(self.sheet).to_string()
    }

    fn add_sheet(&mut self, name: Option<&str>) -> Result<String, String> {
        let id = self.wb.add_sheet(name)?;
        Ok(self.wb.sheet_name(id).to_string())
    }

    fn rename_sheet(&mut self, from: &str, to: &str) -> Result<(), String> {
        let id = self.sheet_named(from)?;
        self.wb.rename_sheet(id, to)
    }

    fn activate_sheet(&mut self, name: &str) -> Result<(), String> {
        self.sheet = self.sheet_named(name)?;
        Ok(())
    }

    fn set_bold(&mut self, reference: &str, on: bool) -> Result<(), String> {
        self.restyle(reference, |s| s.bold = on)
    }

    fn set_fill(&mut self, reference: &str, color: Option<Rgb>) -> Result<(), String> {
        self.restyle(reference, |s| s.fill = color)
    }

    fn set_number_format(&mut self, reference: &str, code: Option<&str>) -> Result<(), String> {
        let (sheet, r) = resolve(self.wb, self.sheet, reference)?;
        for a in cells_of(sheet, r)? {
            self.wb.set_format(a, code);
        }
        Ok(())
    }

    fn insert_rows(&mut self, at: u32, count: u32) -> Result<(), String> {
        self.wb.apply_shift(Shift::InsertRows { sheet: self.sheet, at, count });
        Ok(())
    }

    fn delete_rows(&mut self, at: u32, count: u32) -> Result<(), String> {
        self.wb.apply_shift(Shift::DeleteRows { sheet: self.sheet, at, count });
        Ok(())
    }

    fn message(&mut self, text: &str) {
        self.messages.push(text.to_string());
    }
}

/// "B2" or "B2:C5".
pub fn range_text(r: Range) -> String {
    if r.start == r.end { r.start.to_string() } else { format!("{}:{}", r.start, r.end) }
}
