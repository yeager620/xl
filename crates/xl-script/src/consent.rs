//! Consent before running scripts that came with a file.
//!
//! Scripts in a workbook someone sent you never run until you allow them.
//! The answer is remembered by a SHA-256 of the scripts themselves (names
//! and sources), stored one per line in `$XDG_CONFIG_HOME/xl/trusted-scripts`:
//! re-saving the file keeps it trusted, but any change to its scripts asks
//! again. Scripts written in xl's own editor are trusted as they're saved.

use std::collections::BTreeSet;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::ScriptModule;

/// The fingerprint consent is remembered by.
pub fn scripts_hash(modules: &[ScriptModule]) -> String {
    let mut h = Sha256::new();
    for m in modules {
        // Length-prefixed, so different splits can't collide.
        h.update((m.name.len() as u64).to_le_bytes());
        h.update(m.name.as_bytes());
        h.update((m.source.len() as u64).to_le_bytes());
        h.update(m.source.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// What to do about a workbook's scripts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Consent {
    /// No scripts: nothing to ask.
    NoScripts,
    /// Allowed before (or written here): run them.
    Trusted,
    /// Ask first; don't run anything until the user agrees.
    Ask,
}

/// The remembered answers.
#[derive(Clone, Debug, Default)]
pub struct TrustStore {
    path: Option<PathBuf>,
    hashes: BTreeSet<String>,
}

impl TrustStore {
    /// The store in the user's config directory.
    pub fn load_default() -> TrustStore {
        TrustStore::at(crate::storage::config_dir().map(|d| d.join("trusted-scripts")))
    }

    /// A store kept in `path` (None: in memory only).
    pub fn at(path: Option<PathBuf>) -> TrustStore {
        let hashes = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| text.lines().map(str::trim).filter(|l| l.len() == 64 && l.bytes().all(|b| b.is_ascii_hexdigit())).map(str::to_string).collect())
            .unwrap_or_default();
        TrustStore { path, hashes }
    }

    pub fn consent(&self, modules: &[ScriptModule]) -> Consent {
        if modules.iter().all(|m| m.source.trim().is_empty()) {
            Consent::NoScripts
        } else if self.hashes.contains(&scripts_hash(modules)) {
            Consent::Trusted
        } else {
            Consent::Ask
        }
    }

    /// Remember these scripts as allowed.
    pub fn trust(&mut self, modules: &[ScriptModule]) -> std::io::Result<()> {
        if !self.hashes.insert(scripts_hash(modules)) {
            return Ok(());
        }
        let Some(path) = &self.path else { return Ok(()) };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body: String = self.hashes.iter().map(|h| format!("{h}\n")).collect();
        std::fs::write(path, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(name: &str, source: &str) -> ScriptModule {
        ScriptModule { name: name.into(), source: source.into() }
    }

    #[test]
    fn asks_once_per_script_content() {
        let path = std::env::temp_dir().join(format!("xl-trust-{}", std::process::id()));
        std::fs::remove_file(&path).ok();
        let scripts = vec![m("Module1", "xl.func('F', function() return 1 end)")];
        let mut store = TrustStore::at(Some(path.clone()));
        assert_eq!(store.consent(&[]), Consent::NoScripts);
        assert_eq!(store.consent(&[m("Empty", "  \n")]), Consent::NoScripts);
        assert_eq!(store.consent(&scripts), Consent::Ask);
        store.trust(&scripts).unwrap();
        assert_eq!(store.consent(&scripts), Consent::Trusted);
        // Remembered across runs.
        let again = TrustStore::at(Some(path.clone()));
        assert_eq!(again.consent(&scripts), Consent::Trusted);
        // Any change to the code asks again.
        let edited = vec![m("Module1", "xl.func('F', function() return 2 end)")];
        assert_eq!(again.consent(&edited), Consent::Ask);
        // So does moving code between modules.
        assert_ne!(scripts_hash(&[m("ab", "c")]), scripts_hash(&[m("a", "bc")]));
        std::fs::remove_file(&path).ok();
    }
}
