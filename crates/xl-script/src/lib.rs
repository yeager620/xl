//! xl-script: sandboxed Lua 5.4 scripting for xl, in place of VBA.
//!
//! Scripts are plain Lua that register what they offer:
//!
//! ```lua
//! -- A worksheet function: =DISCOUNT(A1, 0.1)
//! xl.func("DISCOUNT", function(price, rate)
//!   return price * (1 - rate)
//! end, { description = "Price after a discount" })
//!
//! -- Recalculates on every change, like RAND().
//! xl.func("ROLL", function() return math.random(6) end, { volatile = true })
//!
//! -- A macro, run from the command palette (or its shortcut).
//! xl.macro("Highlight negatives", function()
//!   local sel = xl.selection()
//!   local v = xl.get(sel)
//!   ...
//! end, { key = "ctrl-alt-h" })
//! ```
//!
//! Worksheet functions run in one long-lived state ([`FunctionHost`]) that
//! has no access to the workbook — only their arguments — so recalculation
//! stays deterministic and re-entrancy-free. Macros run in a fresh state per
//! run with the `xl` automation API bound to a [`MacroHost`] (see
//! [`run_macro`]). Both are sandboxed ([`sandbox`]): no files, network,
//! processes or OS, capped memory, and a time limit.

mod convert;
pub mod consent;
mod functions;
pub mod macros;
pub mod sandbox;
pub mod storage;

pub use functions::{FunctionHost, ScriptCatalog, ScriptFunction, ScriptMacro};
pub use macros::{MacroHost, MacroOutcome, WorkbookHost, run_macro};
pub use sandbox::Limits;
pub use xl_engine::ScriptModule;

/// A script error, with where it happened when known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptError {
    /// The script (module) it happened in.
    pub module: Option<String>,
    /// 1-based line in that script.
    pub line: Option<u32>,
    pub message: String,
}

impl ScriptError {
    pub fn plain(message: impl Into<String>) -> Self {
        ScriptError { module: None, line: None, message: message.into() }
    }

    pub(crate) fn from_lua(e: &mlua::Error, limits: Limits) -> Self {
        let raw = root_message(e, limits);
        let raw = raw.split("\nstack traceback:").next().unwrap_or("").trim().to_string();
        // "Module:12: message" (chunks are named "=Module").
        if let Some((module, rest)) = raw.split_once(':')
            && let Some((line, msg)) = rest.split_once(':')
            && let Ok(line) = line.trim().parse::<u32>()
            && !module.contains(' ')
        {
            return ScriptError { module: Some(module.to_string()), line: Some(line), message: msg.trim().to_string() };
        }
        ScriptError::plain(raw)
    }

    /// Did the script run past its time limit?
    pub fn is_timeout(&self) -> bool {
        self.message.starts_with(sandbox::TIMEOUT_MESSAGE)
    }
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.module, self.line) {
            (Some(m), Some(l)) => write!(f, "{m}, line {l}: {}", self.message),
            (Some(m), None) => write!(f, "{m}: {}", self.message),
            _ => f.write_str(&self.message),
        }
    }
}

fn root_message(e: &mlua::Error, limits: Limits) -> String {
    match e {
        mlua::Error::CallbackError { cause, .. } => root_message(cause, limits),
        mlua::Error::WithContext { cause, .. } => root_message(cause, limits),
        mlua::Error::MemoryError(_) => format!("script used too much memory (more than {} MB)", limits.memory >> 20),
        mlua::Error::SyntaxError { message, .. } => message.clone(),
        mlua::Error::RuntimeError(m) => m.clone(),
        other => other.to_string(),
    }
}
