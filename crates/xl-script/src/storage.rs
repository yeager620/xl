//! The user's own script library: `$XDG_CONFIG_HOME/xl/scripts/*.lua`
//! (`~/.config/xl/scripts`), loaded into every workbook. These are trusted —
//! the user put them there.

use std::path::{Path, PathBuf};

use crate::ScriptModule;

/// `$XDG_CONFIG_HOME/xl` (or `~/.config/xl`).
pub fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("xl"))
}

/// Where the user library lives.
pub fn user_library_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("scripts"))
}

/// Every `*.lua` file in `dir`, sorted by name, as modules named after the
/// file (without the extension).
pub fn load_dir(dir: &Path) -> Vec<ScriptModule> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "lua") && p.is_file()).collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| {
            let source = std::fs::read_to_string(&p).ok()?;
            let name = p.file_stem()?.to_string_lossy().into_owned();
            Some(ScriptModule { name, source })
        })
        .collect()
}

/// The user library (empty when there is none).
pub fn user_library() -> Vec<ScriptModule> {
    user_library_dir().map(|d| load_dir(&d)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_lua_files_in_order() {
        let dir = std::env::temp_dir().join(format!("xl-script-lib-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.lua"), "xl.func('B', function() return 2 end)").unwrap();
        std::fs::write(dir.join("a.lua"), "xl.func('A', function() return 1 end)").unwrap();
        std::fs::write(dir.join("notes.txt"), "not a script").unwrap();
        let mods = load_dir(&dir);
        assert_eq!(mods.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        std::fs::remove_dir_all(&dir).ok();
        assert!(load_dir(&dir).is_empty());
    }
}
