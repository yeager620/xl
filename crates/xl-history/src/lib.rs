//! xl-history: a local version history of saved workbooks.
//!
//! Every save records a version of the file under
//! `$XDG_DATA_HOME/xl/history/<lineage>/`, where a *lineage* is a stable id
//! for one document: a file path maps to a lineage (in `history/paths`),
//! and "Save as" links the new path to the old path's lineage, so a
//! document's history follows it when it's renamed or moved by the app.
//!
//! Storage, per lineage:
//!
//! ```text
//! <lineage>/objects/ab/cdef…   one blob per distinct zip entry (or whole
//!                              non-zip file), named by the SHA-256 of its
//!                              content, deflate-compressed
//! <lineage>/versions/<ms>-<n>  one text manifest per version
//! ```
//!
//! An xlsx is a zip; each entry (sheet XML, styles, images…) is stored once
//! no matter how many versions contain it, so saving a big workbook again
//! after a small edit only stores the parts that changed. A restored
//! version is the same zip entries, re-zipped (identical content, not
//! necessarily identical bytes).
//!
//! A manifest is line-based:
//!
//! ```text
//! xl-version 1
//! time <unix milliseconds>
//! size <bytes of the original file>
//! path <the file's path when recorded>
//! label <user label, escaped>      (optional)
//! auto 1                           (optional: recorded by autosave)
//! entry <sha256> <zip entry name>  (one per entry, in order)   or
//! raw <sha256>                     (a non-zip file)
//! ```
//!
//! Retention ([`Policy`], overridable in `history/config`): the newest
//! version and labeled versions are always kept; otherwise every version
//! from the last day, the newest per hour for a week, the newest per day
//! for a month. Then, if a lineage is over its size cap, the oldest
//! versions go (unlabeled first). Unreferenced blobs are deleted.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

const MANIFEST_HEADER: &str = "xl-version 1";
const PATHS_HEADER: &str = "xl-history-paths 1";

/// How long versions are kept, and how much space a document's history may use.
#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    /// Keep every version this recent.
    pub keep_all: Duration,
    /// Then the newest version of each hour, this far back.
    pub hourly: Duration,
    /// Then the newest version of each day, this far back.
    pub daily: Duration,
    /// Most bytes one document's history may use (older versions go first).
    pub max_bytes: u64,
    /// Also record a version while editing, at most this often (0 = only on save).
    pub autosave_minutes: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            keep_all: Duration::from_secs(24 * 3600),
            hourly: Duration::from_secs(7 * 24 * 3600),
            daily: Duration::from_secs(30 * 24 * 3600),
            max_bytes: 256 * 1024 * 1024,
            autosave_minutes: 0,
        }
    }
}

impl Policy {
    /// Read `key = value` lines (`keep_all_hours`, `hourly_days`,
    /// `daily_days`, `max_mb`, `autosave_minutes`); unknown keys and bad
    /// values are ignored.
    pub fn parse(text: &str) -> Policy {
        let mut p = Policy::default();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((k, v)) = line.split_once('=') else { continue };
            let Ok(v) = v.trim().parse::<u64>() else { continue };
            match k.trim() {
                "keep_all_hours" => p.keep_all = Duration::from_secs(v * 3600),
                "hourly_days" => p.hourly = Duration::from_secs(v * 86_400),
                "daily_days" => p.daily = Duration::from_secs(v * 86_400),
                "max_mb" => p.max_bytes = v * 1024 * 1024,
                "autosave_minutes" => p.autosave_minutes = v,
                _ => {}
            }
        }
        p
    }
}

/// One recorded version of a document.
#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    /// Unique within the document's history; sorts by time.
    pub id: String,
    pub time: SystemTime,
    /// Size of the file as saved.
    pub size: u64,
    pub label: Option<String>,
    /// Where the file was when this version was recorded.
    pub path: PathBuf,
    /// Recorded by autosave rather than a save.
    pub auto: bool,
    content: Content,
}

impl Version {
    /// The file's extension when it was recorded (for opening a copy).
    pub fn extension(&self) -> &str {
        self.path.extension().and_then(|e| e.to_str()).unwrap_or(if matches!(self.content, Content::Zip(_)) { "xlsx" } else { "csv" })
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Content {
    /// Zip entries in order: (name, blob hash).
    Zip(Vec<(String, String)>),
    Raw(String),
}

impl Content {
    fn hashes(&self) -> Vec<&str> {
        match self {
            Content::Zip(e) => e.iter().map(|(_, h)| h.as_str()).collect(),
            Content::Raw(h) => vec![h.as_str()],
        }
    }
}

/// The history store (usually `$XDG_DATA_HOME/xl/history`).
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
    pub policy: Policy,
}

fn ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "\\r")
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Write a file so a crash never leaves it half-written.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// An absolute, symlink-free form of `path` (as far as it exists).
pub fn canonical(path: &Path) -> PathBuf {
    if let Ok(p) = std::fs::canonicalize(path) {
        return p;
    }
    let abs = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().map(|d| d.join(path)).unwrap_or_else(|_| path.to_path_buf()) };
    match (abs.parent().and_then(|d| std::fs::canonicalize(d).ok()), abs.file_name()) {
        (Some(dir), Some(name)) => dir.join(name),
        _ => abs,
    }
}

impl Store {
    /// A store at `root`, with the retention policy from `root/config` (if any).
    pub fn new(root: impl Into<PathBuf>) -> Store {
        let root = root.into();
        let policy = std::fs::read_to_string(root.join("config")).map(|t| Policy::parse(&t)).unwrap_or_default();
        Store { root, policy }
    }

    pub fn with_policy(root: impl Into<PathBuf>, policy: Policy) -> Store {
        Store { root: root.into(), policy }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // ------------------------------------------------------------ lineages

    fn read_paths(&self) -> Vec<(String, PathBuf)> {
        let Ok(text) = std::fs::read_to_string(self.root.join("paths")) else { return Vec::new() };
        let mut lines = text.lines();
        if lines.next() != Some(PATHS_HEADER) {
            return Vec::new();
        }
        lines.filter_map(|l| l.split_once('\t')).map(|(id, p)| (id.to_string(), PathBuf::from(unescape(p)))).collect()
    }

    fn write_paths(&self, entries: &[(String, PathBuf)]) -> std::io::Result<()> {
        let mut text = format!("{PATHS_HEADER}\n");
        for (id, p) in entries {
            text.push_str(&format!("{id}\t{}\n", escape(&p.to_string_lossy())));
        }
        write_atomic(&self.root.join("paths"), text.as_bytes())
    }

    /// The lineage (history id) of a file path, if it has a history.
    pub fn lineage(&self, path: &Path) -> Option<String> {
        let path = canonical(path);
        self.read_paths().into_iter().rev().find(|(_, p)| *p == path).map(|(id, _)| id)
    }

    fn set_lineage(&self, path: &Path, id: &str) -> std::io::Result<()> {
        let path = canonical(path);
        let mut entries: Vec<(String, PathBuf)> = self.read_paths().into_iter().filter(|(_, p)| *p != path).collect();
        entries.push((id.to_string(), path));
        self.write_paths(&entries)
    }

    fn lineage_or_create(&self, path: &Path) -> std::io::Result<String> {
        if let Some(id) = self.lineage(path) {
            return Ok(id);
        }
        let mut h = Sha256::new();
        h.update(canonical(path).to_string_lossy().as_bytes());
        h.update(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos().to_le_bytes());
        h.update(std::process::id().to_le_bytes());
        let id = hex(&h.finalize()[..8]);
        self.set_lineage(path, &id)?;
        Ok(id)
    }

    /// `to` is now the same document as `from` (Save as, rename): its
    /// versions continue `from`'s history.
    pub fn link(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        if canonical(from) == canonical(to) {
            return Ok(());
        }
        let id = self.lineage_or_create(from)?;
        self.set_lineage(to, &id)
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn object_path(&self, id: &str, hash: &str) -> PathBuf {
        self.dir(id).join("objects").join(&hash[..2]).join(&hash[2..])
    }

    // ------------------------------------------------------------- objects

    fn put_object(&self, id: &str, data: &[u8]) -> std::io::Result<String> {
        let hash = hex(&Sha256::digest(data));
        let path = self.object_path(id, &hash);
        if !path.exists() {
            let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(data)?;
            write_atomic(&path, &enc.finish()?)?;
        }
        Ok(hash)
    }

    fn get_object(&self, id: &str, hash: &str) -> std::io::Result<Vec<u8>> {
        let compressed = std::fs::read(self.object_path(id, hash))?;
        let mut out = Vec::new();
        flate2::read::DeflateDecoder::new(compressed.as_slice()).read_to_end(&mut out)?;
        Ok(out)
    }

    /// Split a file into blobs (zip entries, or the whole file) and store them.
    fn put_content(&self, id: &str, bytes: &[u8]) -> std::io::Result<Content> {
        if bytes.starts_with(b"PK")
            && let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes))
        {
            let mut entries = Vec::with_capacity(zip.len());
            let mut ok = true;
            for i in 0..zip.len() {
                let Ok(mut f) = zip.by_index(i) else {
                    ok = false;
                    break;
                };
                let name = f.name().map(|n| n.to_string()).unwrap_or_default();
                let mut data = Vec::new();
                if name.is_empty() || name.contains(['\n', '\r']) || f.read_to_end(&mut data).is_err() {
                    ok = false;
                    break;
                }
                entries.push((name, self.put_object(id, &data)?));
            }
            if ok {
                return Ok(Content::Zip(entries));
            }
        }
        Ok(Content::Raw(self.put_object(id, bytes)?))
    }

    // ------------------------------------------------------------ versions

    /// Record the file at `path` as it is now. Returns the new version, or
    /// None if it's identical to the newest one (and no label was given).
    pub fn record(&self, path: &Path, label: Option<&str>, auto: bool) -> std::io::Result<Option<Version>> {
        let bytes = std::fs::read(path)?;
        self.record_bytes(path, &bytes, label, auto, SystemTime::now())
    }

    /// Record `bytes` as a version of the document at `path`, made at `time`.
    pub fn record_bytes(&self, path: &Path, bytes: &[u8], label: Option<&str>, auto: bool, time: SystemTime) -> std::io::Result<Option<Version>> {
        let id = self.lineage_or_create(path)?;
        let content = self.put_content(&id, bytes)?;
        if label.is_none()
            && let Some(latest) = self.versions_of(&id).into_iter().next()
            && latest.content == content
        {
            return Ok(None);
        }
        let versions_dir = self.dir(&id).join("versions");
        std::fs::create_dir_all(&versions_dir)?;
        let stamp = ms(time);
        let vid = (0..).map(|n| format!("{stamp:013}-{n}")).find(|v| !versions_dir.join(v).exists()).unwrap_or_default();
        let v = Version {
            id: vid,
            time: UNIX_EPOCH + Duration::from_millis(stamp),
            size: bytes.len() as u64,
            label: label.map(str::to_string).filter(|l| !l.trim().is_empty()),
            path: canonical(path),
            auto,
            content,
        };
        self.write_manifest(&id, &v)?;
        self.prune_lineage(&id, time)?;
        Ok(Some(v))
    }

    fn write_manifest(&self, id: &str, v: &Version) -> std::io::Result<()> {
        let mut m = format!("{MANIFEST_HEADER}\ntime {}\nsize {}\npath {}\n", ms(v.time), v.size, escape(&v.path.to_string_lossy()));
        if let Some(l) = &v.label {
            m.push_str(&format!("label {}\n", escape(l)));
        }
        if v.auto {
            m.push_str("auto 1\n");
        }
        match &v.content {
            Content::Zip(entries) => entries.iter().for_each(|(name, h)| m.push_str(&format!("entry {h} {name}\n"))),
            Content::Raw(h) => m.push_str(&format!("raw {h}\n")),
        }
        write_atomic(&self.dir(id).join("versions").join(&v.id), m.as_bytes())
    }

    fn read_manifest(path: &Path) -> Option<Version> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut lines = text.lines();
        if lines.next()? != MANIFEST_HEADER {
            return None;
        }
        let (mut time, mut size, mut vpath, mut label, mut auto) = (None, 0, PathBuf::new(), None, false);
        let (mut entries, mut raw) = (Vec::new(), None);
        for line in lines {
            let (k, v) = line.split_once(' ').unwrap_or((line, ""));
            match k {
                "time" => time = v.parse::<u64>().ok(),
                "size" => size = v.parse().unwrap_or(0),
                "path" => vpath = PathBuf::from(unescape(v)),
                "label" => label = Some(unescape(v)),
                "auto" => auto = v == "1",
                "entry" => {
                    let (h, name) = v.split_once(' ')?;
                    entries.push((name.to_string(), h.to_string()));
                }
                "raw" => raw = Some(v.to_string()),
                _ => {}
            }
        }
        let content = match raw {
            Some(h) => Content::Raw(h),
            None => Content::Zip(entries),
        };
        let id = path.file_name()?.to_str()?.to_string();
        Some(Version { id, time: UNIX_EPOCH + Duration::from_millis(time?), size, label, path: vpath, auto, content })
    }

    fn versions_of(&self, id: &str) -> Vec<Version> {
        let Ok(dir) = std::fs::read_dir(self.dir(id).join("versions")) else { return Vec::new() };
        let mut out: Vec<Version> =
            dir.flatten().filter(|e| !e.file_name().to_string_lossy().contains(".tmp")).filter_map(|e| Self::read_manifest(&e.path())).collect();
        out.sort_by(|a, b| b.id.cmp(&a.id));
        out
    }

    /// The document's versions, newest first.
    pub fn versions(&self, path: &Path) -> Vec<Version> {
        self.lineage(path).map(|id| self.versions_of(&id)).unwrap_or_default()
    }

    /// The file as it was at version `v` of the document at `path`.
    pub fn bytes(&self, path: &Path, v: &Version) -> std::io::Result<Vec<u8>> {
        let id = self.lineage(path).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no history for this file"))?;
        match &v.content {
            Content::Raw(h) => self.get_object(&id, h),
            Content::Zip(entries) => {
                let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
                let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
                for (name, h) in entries {
                    let data = self.get_object(&id, h)?;
                    w.start_file(name.as_str(), opts).map_err(std::io::Error::other)?;
                    w.write_all(&data)?;
                }
                Ok(w.finish().map_err(std::io::Error::other)?.into_inner())
            }
        }
    }

    /// Write version `v` to `dest` (e.g. to open it as a copy).
    pub fn export(&self, path: &Path, v: &Version, dest: &Path) -> std::io::Result<()> {
        write_atomic(dest, &self.bytes(path, v)?)
    }

    /// Name (or un-name, with None) a version. Named versions are kept by retention.
    pub fn set_label(&self, path: &Path, version: &str, label: Option<&str>) -> std::io::Result<()> {
        let id = self.lineage(path).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no history for this file"))?;
        let mut v = self.versions_of(&id).into_iter().find(|v| v.id == version).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such version"))?;
        v.label = label.map(str::to_string).filter(|l| !l.trim().is_empty());
        self.write_manifest(&id, &v)
    }

    /// Bytes used by the document's history.
    pub fn size(&self, path: &Path) -> u64 {
        self.lineage(path).map_or(0, |id| dir_size(&self.dir(&id)))
    }

    /// Apply the retention policy to the document's history now.
    pub fn prune(&self, path: &Path) -> std::io::Result<usize> {
        match self.lineage(path) {
            Some(id) => self.prune_lineage(&id, SystemTime::now()),
            None => Ok(0),
        }
    }

    /// Drop versions the policy doesn't keep, then blobs nothing uses.
    /// Returns how many versions were dropped.
    fn prune_lineage(&self, id: &str, now: SystemTime) -> std::io::Result<usize> {
        let versions = self.versions_of(id);
        let ages: Vec<(SystemTime, bool)> = versions.iter().map(|v| (v.time, v.label.is_some())).collect();
        let mut keep = retained(&ages, &self.policy, now);

        // Size cap: drop the oldest (unlabeled first) until the blobs fit.
        let sizes: HashMap<&str, u64> = versions
            .iter()
            .flat_map(|v| v.content.hashes())
            .map(|h| (h, std::fs::metadata(self.object_path(id, h)).map_or(0, |m| m.len())))
            .collect();
        let used = |keep: &[bool]| -> u64 {
            let hashes: HashSet<&str> = versions.iter().zip(keep).filter(|(_, k)| **k).flat_map(|(v, _)| v.content.hashes()).collect();
            hashes.iter().map(|h| sizes.get(h).copied().unwrap_or(0)).sum()
        };
        for labeled_too in [false, true] {
            while used(&keep) > self.policy.max_bytes {
                let Some(i) = (1..versions.len()).rev().find(|&i| keep[i] && (labeled_too || versions[i].label.is_none())) else { break };
                keep[i] = false;
            }
        }

        let dropped = keep.iter().filter(|k| !**k).count();
        if dropped == 0 {
            return Ok(0);
        }
        for (v, _) in versions.iter().zip(&keep).filter(|(_, k)| !**k) {
            std::fs::remove_file(self.dir(id).join("versions").join(&v.id)).ok();
        }
        let live: HashSet<&str> = versions.iter().zip(&keep).filter(|(_, k)| **k).flat_map(|(v, _)| v.content.hashes()).collect();
        let objects = self.dir(id).join("objects");
        for sub in std::fs::read_dir(&objects).into_iter().flatten().flatten() {
            let prefix = sub.file_name().to_string_lossy().into_owned();
            for f in std::fs::read_dir(sub.path()).into_iter().flatten().flatten() {
                let hash = format!("{prefix}{}", f.file_name().to_string_lossy());
                if !live.contains(hash.as_str()) {
                    std::fs::remove_file(f.path()).ok();
                }
            }
        }
        Ok(dropped)
    }
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        match e.file_type() {
            Ok(t) if t.is_dir() => total += dir_size(&e.path()),
            Ok(_) => total += e.metadata().map_or(0, |m| m.len()),
            Err(_) => {}
        }
    }
    total
}

/// Which versions (newest first: time, labeled) the policy keeps at `now`,
/// ignoring the size cap.
pub fn retained(versions: &[(SystemTime, bool)], policy: &Policy, now: SystemTime) -> Vec<bool> {
    let mut hours = HashSet::new();
    let mut days = HashSet::new();
    versions
        .iter()
        .enumerate()
        .map(|(i, &(t, labeled))| {
            let age = now.duration_since(t).unwrap_or_default();
            let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            if i == 0 || labeled || age <= policy.keep_all {
                // Kept anyway; still claims its hour and day.
                hours.insert(secs / 3600);
                days.insert(secs / 86_400);
                true
            } else if age <= policy.hourly {
                days.insert(secs / 86_400);
                hours.insert(secs / 3600)
            } else if age <= policy.daily {
                days.insert(secs / 86_400)
            } else {
                false
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xl-history-test-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A small zip with the given (name, content) entries.
    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn objects(store: &Store, path: &Path) -> usize {
        let id = store.lineage(path).unwrap();
        std::fs::read_dir(store.dir(&id).join("objects")).unwrap().flatten().map(|d| std::fs::read_dir(d.path()).unwrap().count()).sum()
    }

    fn unzip(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut z = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        (0..z.len())
            .map(|i| {
                let mut f = z.by_index(i).unwrap();
                let mut d = Vec::new();
                f.read_to_end(&mut d).unwrap();
                (f.name().unwrap().to_string(), d)
            })
            .collect()
    }

    #[test]
    fn dedupes_identical_parts_across_versions() {
        let dir = scratch("dedupe");
        let store = Store::with_policy(dir.join("history"), Policy::default());
        let doc = dir.join("Budget.xlsx");
        let big = vec![b'x'; 200_000];
        let t0 = SystemTime::now();
        let v1 = zip_of(&[("styles.xml", &big), ("sheet1.xml", b"<a>1</a>")]);
        let v2 = zip_of(&[("styles.xml", &big), ("sheet1.xml", b"<a>2</a>")]);
        store.record_bytes(&doc, &v1, None, false, t0).unwrap().unwrap();
        store.record_bytes(&doc, &v2, None, false, t0 + Duration::from_secs(1)).unwrap().unwrap();
        // styles.xml stored once: 3 blobs for 4 entries.
        assert_eq!(objects(&store, &doc), 3);
        // Compressed: far smaller than the 400 KB recorded.
        assert!(store.size(&doc) < 20_000, "{}", store.size(&doc));
        // Saving the same content again records nothing.
        assert!(store.record_bytes(&doc, &v2, None, false, t0 + Duration::from_secs(2)).unwrap().is_none());
        let versions = store.versions(&doc);
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].size, v2.len() as u64);
        // Restoring gives back the same entries.
        assert_eq!(unzip(&store.bytes(&doc, &versions[1]).unwrap()), unzip(&v1));
        assert_eq!(unzip(&store.bytes(&doc, &versions[0]).unwrap()), unzip(&v2));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn raw_files_and_labels() {
        let dir = scratch("raw");
        let store = Store::with_policy(dir.join("history"), Policy::default());
        let doc = dir.join("data.csv");
        std::fs::write(&doc, "a,b\n1,2\n").unwrap();
        let v = store.record(&doc, Some("First draft"), false).unwrap().unwrap();
        assert_eq!(v.label.as_deref(), Some("First draft"));
        assert_eq!(store.bytes(&doc, &v).unwrap(), b"a,b\n1,2\n");
        // A label records even identical content.
        assert!(store.record(&doc, Some("Again"), false).unwrap().is_some());
        store.set_label(&doc, &v.id, Some("Renamed\nlabel")).unwrap();
        let vs = store.versions(&doc);
        assert_eq!(vs.len(), 2);
        assert_eq!(vs[1].label.as_deref(), Some("Renamed\nlabel"));
        store.set_label(&doc, &v.id, None).unwrap();
        assert_eq!(store.versions(&doc)[1].label, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_as_links_lineage() {
        let dir = scratch("lineage");
        let store = Store::with_policy(dir.join("history"), Policy::default());
        let (a, b) = (dir.join("a.csv"), dir.join("sub-b.csv"));
        let t = SystemTime::now();
        store.record_bytes(&a, b"1", None, false, t).unwrap();
        store.link(&a, &b).unwrap();
        store.record_bytes(&b, b"2", None, false, t + Duration::from_secs(1)).unwrap();
        assert_eq!(store.lineage(&a), store.lineage(&b));
        let vs = store.versions(&b);
        assert_eq!(vs.len(), 2, "b continues a's history");
        assert_eq!(vs[0].path, canonical(&b));
        assert_eq!(vs[1].path, canonical(&a));
        // Unrelated files have their own histories.
        store.record_bytes(&dir.join("c.csv"), b"1", None, false, t).unwrap();
        assert_ne!(store.lineage(&dir.join("c.csv")), store.lineage(&a));
        // Relative and absolute spellings of a path are the same document.
        assert_eq!(store.lineage(&dir.join(".").join("a.csv")), store.lineage(&a));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn retention_tiers() {
        let now = UNIX_EPOCH + Duration::from_secs(100 * 86_400);
        let p = Policy::default();
        let ago = |s: u64| now - Duration::from_secs(s);
        let versions = [
            (ago(10), false),               // newest: kept
            (ago(3600), false),             // within a day: kept
            (ago(2 * 86_400 + 60), false),  // hourly tier, newest in its hour: kept
            (ago(2 * 86_400 + 120), false), // same hour: dropped
            (ago(10 * 86_400), false),      // daily tier: kept
            (ago(10 * 86_400 + 7200), true),// same day but labeled: kept
            (ago(10 * 86_400 + 9000), false),// same day: dropped
            (ago(60 * 86_400), false),      // past a month: dropped
            (ago(90 * 86_400), true),       // labeled: kept
        ];
        assert_eq!(retained(&versions, &p, now), vec![true, true, true, false, true, true, false, false, true]);
    }

    #[test]
    fn pruning_removes_versions_and_unused_blobs() {
        let dir = scratch("prune");
        let store = Store::with_policy(dir.join("history"), Policy::default());
        let doc = dir.join("x.csv");
        let now = SystemTime::now();
        // Two versions in the same clock hour about two days ago (anchored to
        // the hour so the test can't straddle a boundary), then one now.
        let two_days_ago = (now - Duration::from_secs(2 * 86_400)).duration_since(UNIX_EPOCH).unwrap().as_secs();
        let hour = UNIX_EPOCH + Duration::from_secs(two_days_ago / 3600 * 3600);
        store.record_bytes(&doc, b"old-1", None, false, hour + Duration::from_secs(600)).unwrap();
        store.record_bytes(&doc, b"old-2", None, false, hour + Duration::from_secs(1200)).unwrap();
        store.record_bytes(&doc, b"new", None, false, now).unwrap();
        let vs = store.versions(&doc);
        assert_eq!(vs.len(), 2, "old-1 shares an hour with old-2");
        assert_eq!(store.bytes(&doc, &vs[1]).unwrap(), b"old-2");
        assert_eq!(objects(&store, &doc), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn size_cap_drops_oldest_unlabeled_first() {
        let dir = scratch("cap");
        let policy = Policy { max_bytes: 3000, ..Policy::default() };
        let store = Store::with_policy(dir.join("history"), policy);
        let doc = dir.join("x.csv");
        let t = SystemTime::now();
        // Incompressible-ish 1 KB blobs.
        let blob = |seed: u64| -> Vec<u8> {
            let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            (0..1024)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                })
                .collect()
        };
        store.record_bytes(&doc, &blob(1), Some("keep me"), false, t).unwrap();
        for i in 2..=5 {
            store.record_bytes(&doc, &blob(i), None, false, t + Duration::from_secs(i)).unwrap();
        }
        let vs = store.versions(&doc);
        assert!(store.size(&doc) < 3000 + 4096, "{}", store.size(&doc));
        assert_eq!(vs[0].id, store.versions(&doc)[0].id);
        assert_eq!(store.bytes(&doc, &vs[0]).unwrap(), blob(5), "newest kept");
        assert!(vs.iter().any(|v| v.label.as_deref() == Some("keep me")), "labeled kept while unlabeled ones can go");
        assert!(vs.len() < 5);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn policy_config() {
        let p = Policy::parse("# comment\nkeep_all_hours = 2\nmax_mb=10\nautosave_minutes = 5\nbogus = 3\nhourly_days = x\n");
        assert_eq!(p.keep_all, Duration::from_secs(7200));
        assert_eq!(p.max_bytes, 10 * 1024 * 1024);
        assert_eq!(p.autosave_minutes, 5);
        assert_eq!(p.hourly, Policy::default().hourly);
    }
}
