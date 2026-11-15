//! Worksheet functions defined by scripts, served to the formula engine.

use std::collections::HashMap;
use std::sync::Mutex;

use mlua::{Function, Table, Value as Lv};
use xl_engine::{Addr, ErrorKind, Func, UdfArg, UdfValue, UserFunctions, Value};

use crate::sandbox::{Limits, Sandbox};
use crate::{ScriptError, ScriptModule, convert};

/// A worksheet function a script registered with `xl.func`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptFunction {
    /// Upper-cased, as formulas call it.
    pub name: String,
    pub volatile: bool,
    pub description: Option<String>,
    pub module: String,
}

/// A macro a script registered with `xl.macro`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptMacro {
    pub name: String,
    /// Keyboard shortcut, in GPUI keystroke syntax ("ctrl-alt-h").
    pub key: Option<String>,
    pub description: Option<String>,
    pub module: String,
}

/// Everything the scripts define, and what went wrong loading them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScriptCatalog {
    pub functions: Vec<ScriptFunction>,
    /// In the order they were registered.
    pub macros: Vec<ScriptMacro>,
    /// Scripts that failed to load (the others still work), and names
    /// that can't be used.
    pub errors: Vec<ScriptError>,
}

struct Entry {
    func: Function,
}

struct State {
    sandbox: Sandbox,
    funcs: HashMap<String, Entry>,
}

/// Script functions for the formula engine: one sandboxed Lua state with
/// every script loaded, shared by all calls.
pub struct FunctionHost {
    state: Mutex<State>,
    catalog: ScriptCatalog,
    /// Function name → volatile.
    names: HashMap<String, bool>,
    /// The last error message per cell, for the UI.
    errors: Mutex<HashMap<Addr, String>>,
}

impl FunctionHost {
    /// Load `modules` (in order; later definitions win).
    pub fn new(modules: &[ScriptModule], limits: Limits) -> Result<FunctionHost, ScriptError> {
        let sandbox = Sandbox::new(limits)?;
        let mut catalog = ScriptCatalog::default();
        for m in modules {
            if let Err(e) = sandbox.load_module(&m.name, &m.source) {
                catalog.errors.push(e);
            }
        }
        let mut funcs = HashMap::new();
        for pair in sandbox.funcs.pairs::<String, Table>() {
            let Ok((name, t)) = pair else { continue };
            let module: String = t.get("module").unwrap_or_default();
            if Func::from_name(&name).is_some() {
                catalog.errors.push(ScriptError { module: Some(module), line: None, message: format!("{name} is a built-in function; pick another name") });
                continue;
            }
            let (Ok(func), volatile) = (t.get::<Function>("fn"), t.get::<bool>("volatile").unwrap_or(false)) else { continue };
            catalog.functions.push(ScriptFunction { name: name.clone(), volatile, description: t.get("description").ok(), module });
            funcs.insert(name, Entry { func });
        }
        catalog.functions.sort_by(|a, b| a.name.cmp(&b.name));
        let mut macros: Vec<(i64, ScriptMacro)> = Vec::new();
        for pair in sandbox.macros.pairs::<String, Table>() {
            let Ok((name, t)) = pair else { continue };
            macros.push((t.get("order").unwrap_or(0), ScriptMacro { name, key: t.get("key").ok(), description: t.get("description").ok(), module: t.get("module").unwrap_or_default() }));
        }
        macros.sort_by_key(|(o, _)| *o);
        catalog.macros = macros.into_iter().map(|(_, m)| m).collect();
        sandbox.take_output();
        let names = catalog.functions.iter().map(|f| (f.name.clone(), f.volatile)).collect();
        Ok(FunctionHost { state: Mutex::new(State { sandbox, funcs }), catalog, names, errors: Mutex::default() })
    }

    pub fn catalog(&self) -> &ScriptCatalog {
        &self.catalog
    }

    /// Why the script function in this cell last failed, if it did.
    pub fn cell_error(&self, at: Addr) -> Option<String> {
        self.errors.lock().unwrap_or_else(|p| p.into_inner()).get(&at).cloned()
    }

    /// Call a function directly (outside a formula), e.g. from tests or a
    /// "try it" box.
    pub fn call_direct(&self, name: &str, args: &[UdfArg]) -> Result<UdfValue, ScriptError> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let entry = state.funcs.get(&name.to_ascii_uppercase()).ok_or_else(|| ScriptError::plain(format!("no script function {name}")))?;
        let sb = &state.sandbox;
        let lua_args = args.iter().map(|a| convert::arg_to_lua(&sb.lua, &sb.errmeta, a)).collect::<mlua::Result<mlua::MultiValue>>().map_err(|e| ScriptError::from_lua(&e, sb.limits))?;
        let out: Lv = sb.timed(|| entry.func.call(lua_args))?;
        sb.take_output();
        convert::result_from_lua(&out, &sb.errmeta).map_err(|m| ScriptError::plain(format!("{name} returned {m}")))
    }
}

impl UserFunctions for FunctionHost {
    fn has(&self, name: &str) -> bool {
        self.names.contains_key(name)
    }

    fn is_volatile(&self, name: &str) -> bool {
        self.names.get(name).copied().unwrap_or(false)
    }

    fn call(&self, name: &str, args: &[UdfArg], at: Addr) -> UdfValue {
        let result = self.call_direct(name, args);
        let mut errors = self.errors.lock().unwrap_or_else(|p| p.into_inner());
        match result {
            Ok(v) => {
                errors.remove(&at);
                v
            }
            Err(e) => {
                errors.insert(at, e.to_string());
                UdfValue::Value(Value::Error(ErrorKind::Value))
            }
        }
    }
}
