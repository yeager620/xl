//! A Lua 5.4 state with no way out: only the `string`, `table`, `math`,
//! `utf8` and `coroutine` libraries (no `io`, `os`, `package`/`require`,
//! `debug`, no `dofile`/`loadfile`, no bytecode loading), a memory cap, and
//! a deadline enforced by an instruction-count hook. `pcall` and `xpcall`
//! can't swallow the deadline: once it has passed they re-raise.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mlua::{Function, HookTriggers, Lua, LuaOptions, StdLib, Table, VmState};

use crate::ScriptError;

/// Resource limits for one Lua state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Most memory the state may allocate.
    pub memory: usize,
    /// How long one run (loading the scripts, one function call, one macro) may take.
    pub time: Duration,
}

impl Limits {
    /// For worksheet functions: they run during recalculation, so they're kept short.
    pub const FUNCTIONS: Limits = Limits { memory: 64 << 20, time: Duration::from_secs(2) };
    /// For macros, run on demand.
    pub const MACROS: Limits = Limits { memory: 256 << 20, time: Duration::from_secs(10) };
}

/// How often (in VM instructions) the deadline is checked.
const CHECK_EVERY: u32 = 1000;

pub(crate) const TIMEOUT_MESSAGE: &str = "script took too long";

/// The deadline of the run in progress, shared with the hook.
#[derive(Clone, Default)]
pub(crate) struct Deadline(Arc<Mutex<Option<(Instant, Duration)>>>);

impl Deadline {
    pub(crate) fn start(&self, limit: Duration) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now() + limit, limit));
    }

    pub(crate) fn clear(&self) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// The limit, if the deadline has passed.
    fn expired(&self) -> Option<Duration> {
        let d = *self.0.lock().unwrap_or_else(|p| p.into_inner());
        d.filter(|(at, _)| Instant::now() >= *at).map(|(_, limit)| limit)
    }
}

/// A sandboxed state, plus the tables the prelude keeps.
pub(crate) struct Sandbox {
    pub lua: Lua,
    pub deadline: Deadline,
    pub limits: Limits,
    /// Registered worksheet functions: NAME → {fn, volatile, description, module}.
    pub funcs: Table,
    /// Registered macros: name → {fn, key, description, module, order}.
    pub macros: Table,
    /// Metatable of error values (`xl.error("#N/A")`).
    pub errmeta: Table,
    /// Sets the module name recorded by registrations.
    set_module: Function,
    /// Lines printed with `print`.
    pub output: Arc<Mutex<Vec<String>>>,
}

const PRELUDE: &str = r##"
local expired, xl_print = ...
local funcs, macros, current, order = {}, {}, "?", 0
local errmeta = { __name = "xl.error" }
errmeta.__tostring = function(e) return e.error end

xl = {}

function xl.error(code)
  return setmetatable({ error = code or "#VALUE!" }, errmeta)
end

function xl.iserror(v)
  return getmetatable(v) == errmeta
end

function xl.func(name, fn, opts)
  if type(name) ~= "string" or not name:match("^[%a_][%w_%.]*$") then
    error("xl.func: function names are letters, digits, '_' and '.', got " .. tostring(name), 2)
  end
  if type(fn) ~= "function" then error("xl.func: the second argument must be a function", 2) end
  opts = opts or {}
  funcs[name:upper()] = { fn = fn, volatile = opts.volatile == true, description = opts.description, module = current }
end

function xl.macro(name, fn, opts)
  if type(name) ~= "string" or name == "" then error("xl.macro: the first argument must be a name", 2) end
  if type(fn) ~= "function" then error("xl.macro: the second argument must be a function", 2) end
  opts = opts or {}
  order = order + 1
  macros[name] = { fn = fn, key = opts.key, description = opts.description, module = current, order = order }
end

print = xl_print

-- The deadline can't be caught.
local raw_pcall, raw_xpcall, raw_error = pcall, xpcall, error
local function pass(ok, ...)
  if not ok and expired() then raw_error((...), 0) end
  return ok, ...
end
pcall = function(f, ...) return pass(raw_pcall(f, ...)) end
xpcall = function(f, h, ...)
  return pass(raw_xpcall(f, function(e) if expired() then return e end return h(e) end, ...))
end

-- Text chunks only: precompiled bytecode can break out of the VM.
local raw_load = load
load = function(chunk, name, _, ...)
  if select("#", ...) > 0 then return raw_load(chunk, name, "t", ...) end
  return raw_load(chunk, name, "t")
end
dofile, loadfile, require, collectgarbage = nil, nil, nil, nil
string.dump = nil

return funcs, macros, errmeta, function(m) current = m end
"##;

impl Sandbox {
    pub(crate) fn new(limits: Limits) -> Result<Sandbox, ScriptError> {
        let libs = StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE;
        let lua = Lua::new_with(libs, LuaOptions::default()).map_err(|e| ScriptError::plain(e.to_string()))?;
        let deadline = Deadline::default();
        let output: Arc<Mutex<Vec<String>>> = Arc::default();
        let setup = || -> mlua::Result<(Table, Table, Table, Function)> {
            let d = deadline.clone();
            let expired = lua.create_function(move |_, ()| Ok(d.expired().is_some()))?;
            let out = output.clone();
            let print = lua.create_function(move |lua, args: mlua::MultiValue| {
                let parts: Vec<String> = args.into_iter().map(|v| lua.coerce_string(v.clone()).ok().flatten().map(|s| s.to_string_lossy()).unwrap_or_else(|| format!("{v:?}"))).collect();
                let mut out = out.lock().unwrap_or_else(|p| p.into_inner());
                if out.len() < 1000 {
                    out.push(parts.join("\t"));
                }
                Ok(())
            })?;
            lua.load(PRELUDE).set_name("=prelude").call((expired, print))
        };
        let (funcs, macros, errmeta, set_module) = setup().map_err(|e| ScriptError::plain(e.to_string()))?;
        let d = deadline.clone();
        lua.set_global_hook(HookTriggers::new().every_nth_instruction(CHECK_EVERY), move |_, _| match d.expired() {
            Some(limit) => Err(mlua::Error::runtime(format!("{TIMEOUT_MESSAGE} (more than {} s)", limit.as_secs_f64()))),
            None => Ok(VmState::Continue),
        })
        .map_err(|e| ScriptError::plain(e.to_string()))?;
        lua.set_memory_limit(limits.memory).map_err(|e| ScriptError::plain(e.to_string()))?;
        Ok(Sandbox { lua, deadline, limits, funcs, macros, errmeta, set_module, output })
    }

    /// Run `f` under the time limit.
    pub(crate) fn timed<T>(&self, f: impl FnOnce() -> mlua::Result<T>) -> Result<T, ScriptError> {
        self.deadline.start(self.limits.time);
        let r = f();
        self.deadline.clear();
        r.map_err(|e| ScriptError::from_lua(&e, self.limits))
    }

    /// Run a script's top level (which registers its functions and macros).
    pub(crate) fn load_module(&self, name: &str, source: &str) -> Result<(), ScriptError> {
        self.set_module.call::<()>(name).map_err(|e| ScriptError::from_lua(&e, self.limits))?;
        let r = self.timed(|| self.lua.load(source).set_name(format!("={name}")).set_mode(mlua::chunk::ChunkMode::Text).exec());
        r.map_err(|mut e| {
            if e.module.is_none() {
                e.module = Some(name.to_string());
            }
            e
        })
    }

    pub(crate) fn take_output(&self) -> Vec<String> {
        std::mem::take(&mut *self.output.lock().unwrap_or_else(|p| p.into_inner()))
    }
}
