//! Recently opened and saved workbooks, kept locally in
//! `$XDG_DATA_HOME/xl/recent` (default `~/.local/share/xl/recent`).
//!
//! The file is line-based: a version header, then one entry per line as
//! `<unix seconds>\t<flags>\t<path>` (flags: `p` pinned, `-` none). The path
//! is the last field, so it may contain tabs; paths with newlines aren't
//! recorded. Unreadable lines are skipped. Writes go to a temporary file that
//! is renamed over the old one, so a crash never leaves a half-written list.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HEADER: &str = "xl-recent 1";
/// Most entries kept (pinned ones are evicted last).
pub const CAP: usize = 50;

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    /// When it was last opened or saved.
    pub opened: SystemTime,
    pub pinned: bool,
    /// The file was gone when the list was loaded.
    pub missing: bool,
}

#[derive(Clone, Debug)]
pub struct Recent {
    dir: PathBuf,
    /// Pinned first, then most recent first.
    entries: Vec<Entry>,
}

/// Where the list lives. Tests get a private directory so they never read
/// or write the user's real list.
pub fn default_dir() -> PathBuf {
    if cfg!(test) {
        return std::env::temp_dir().join(format!("xl-test-data-{}", std::process::id()));
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir)
        .join("xl")
}

fn parse(text: &[u8]) -> Vec<Entry> {
    let mut lines = text.split(|&b| b == b'\n');
    if lines.next().is_none_or(|h| h.strip_suffix(b"\r").unwrap_or(h) != HEADER.as_bytes()) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in lines {
        let mut fields = line.splitn(3, |&b| b == b'\t');
        let (Some(secs), Some(flags), Some(path)) = (fields.next(), fields.next(), fields.next()) else { continue };
        let Some(secs) = std::str::from_utf8(secs).ok().and_then(|s| s.parse::<u64>().ok()) else { continue };
        if path.is_empty() {
            continue;
        }
        let path = PathBuf::from(OsStr::from_bytes(path));
        if !path.is_absolute() {
            continue;
        }
        out.push(Entry { path, opened: UNIX_EPOCH + Duration::from_secs(secs), pinned: flags.contains(&b'p'), missing: false });
    }
    out
}

fn serialize(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 * entries.len() + 16);
    out.extend_from_slice(HEADER.as_bytes());
    out.push(b'\n');
    for e in entries {
        let secs = e.opened.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        out.extend_from_slice(format!("{secs}\t{}\t", if e.pinned { "p" } else { "-" }).as_bytes());
        out.extend_from_slice(e.path.as_os_str().as_bytes());
        out.push(b'\n');
    }
    out
}

impl Recent {
    /// Read the list in `dir` (empty if there is none or it's unreadable).
    pub fn load(dir: &Path) -> Self {
        let mut entries = std::fs::read(dir.join("recent")).map(|t| parse(&t)).unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        entries.retain(|e| seen.insert(e.path.clone()));
        for e in &mut entries {
            e.missing = !e.path.is_file();
        }
        let mut this = Self { dir: dir.to_path_buf(), entries };
        this.sort();
        this
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn sort(&mut self) {
        self.entries.sort_by(|a, b| b.pinned.cmp(&a.pinned).then(b.opened.cmp(&a.opened)));
    }

    /// Change the list as stored on disk (re-read first, so several xl
    /// windows don't overwrite each other's entries) and write it back.
    fn update(&mut self, f: impl FnOnce(&mut Vec<Entry>)) {
        *self = Self::load(&self.dir.clone());
        f(&mut self.entries);
        self.sort();
        // Over the cap: drop the oldest unpinned entries, then the oldest pinned.
        while self.entries.len() > CAP {
            let i = self.entries.iter().rposition(|e| !e.pinned).unwrap_or(self.entries.len() - 1);
            self.entries.remove(i);
        }
        self.save().ok();
    }

    /// A workbook was opened or saved: put it at the top.
    pub fn record(&mut self, path: &Path) {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if !path.is_absolute() || path.as_os_str().as_bytes().contains(&b'\n') {
            return;
        }
        self.update(|entries| {
            let pinned = entries.iter().any(|e| e.path == path && e.pinned);
            entries.retain(|e| e.path != path);
            entries.insert(0, Entry { path, opened: SystemTime::now(), pinned, missing: false });
        });
    }

    pub fn remove(&mut self, path: &Path) {
        self.update(|entries| entries.retain(|e| e.path != path));
    }

    pub fn toggle_pin(&mut self, path: &Path) {
        self.update(|entries| {
            if let Some(e) = entries.iter_mut().find(|e| e.path == path) {
                e.pinned = !e.pinned;
            }
        });
    }

    /// Write atomically: a temporary file in the same directory, renamed over the list.
    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!(".recent.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serialize(&self.entries))?;
        std::fs::rename(&tmp, self.dir.join("recent")).inspect_err(|_| {
            std::fs::remove_file(&tmp).ok();
        })
    }
}

/// `~/Documents/Budgets` for a file in it (the folder, with home shortened).
pub fn short_folder(path: &Path) -> String {
    let folder = path.parent().unwrap_or(Path::new("/"));
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Ok(rest) = folder.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() { "~".into() } else { format!("~/{}", rest.display()) };
    }
    folder.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xl-recent-unit-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_skips_garbage_and_keeps_tabs_in_paths() {
        let text = b"xl-recent 1\n100\t-\t/a/b.xlsx\nnot a line\n200\tp\t/c/with\ttab.csv\n300\t-\trelative.xlsx\n\n";
        let e = parse(text);
        assert_eq!(e.len(), 2);
        assert_eq!(e[1].path, PathBuf::from("/c/with\ttab.csv"));
        assert!(e[1].pinned && !e[0].pinned);
        assert!(parse(b"something else\n100\t-\t/a.xlsx\n").is_empty(), "unknown header");
        assert_eq!(parse(&serialize(&e)), e);
    }

    #[test]
    fn record_dedupes_orders_caps_and_persists() {
        let dir = temp("store");
        let files: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("f{i}.xlsx"))).collect();
        for f in &files {
            std::fs::write(f, b"x").unwrap();
        }
        let mut r = Recent::load(&dir);
        assert!(r.entries().is_empty());
        r.record(&files[0]);
        r.record(&files[1]);
        // A different spelling of the same file is the same entry.
        r.record(&dir.join("./f0.xlsx"));
        let paths: Vec<&Path> = r.entries().iter().map(|e| e.path.as_path()).collect();
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        assert_eq!(paths, [canon(&files[0]), canon(&files[1])]);
        // Persisted.
        let again = Recent::load(&dir);
        assert_eq!(again.entries().len(), 2);
        assert_eq!(again.entries()[0].path, canon(&files[0]));
        // Pinned entries stay on top; missing files are flagged.
        r.toggle_pin(&canon(&files[1]));
        r.record(&files[2]);
        std::fs::remove_file(&files[2]).unwrap();
        let r = Recent::load(&dir);
        assert_eq!(r.entries()[0].path, canon(&files[1]));
        assert!(r.entries()[0].pinned);
        assert!(r.entries()[1].missing);
        // Capped.
        let mut r = r;
        for i in 0..CAP + 5 {
            r.record(&dir.join(format!("gone{i}.xlsx")));
        }
        assert_eq!(r.entries().len(), CAP);
        assert!(r.entries()[0].pinned, "the pinned entry survives the cap");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn folder_is_shortened_with_tilde() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/home/x".into()));
        if std::env::var_os("HOME").is_some() {
            assert_eq!(short_folder(&home.join("Docs/a.xlsx")), "~/Docs");
            assert_eq!(short_folder(&home.join("a.xlsx")), "~");
        }
        assert_eq!(short_folder(Path::new("/srv/data/a.xlsx")), "/srv/data");
    }
}
