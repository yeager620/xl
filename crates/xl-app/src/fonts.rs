//! Fonts: which families are installed, what to draw when a file names one
//! that isn't (metric-compatible stand-ins for Office fonts first, then a
//! font of the same kind), and the font files PDF export embeds.
//!
//! The name stored in a cell's style never changes; substitution only
//! affects drawing, so files keep their fonts when saved.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, RwLock};

use gpui::SharedString;

use crate::FONT;

/// Office and other common fonts, and free fonts with the same metrics (or
/// at least the same feel), best first. Rows keep their layout when a
/// metric-compatible one is installed.
const SUBSTITUTES: &[(&str, &[&str])] = &[
    ("Calibri", &["Carlito"]),
    ("Calibri Light", &["Carlito"]),
    ("Cambria", &["Caladea"]),
    ("Cambria Math", &["Caladea"]),
    ("Arial", &["Liberation Sans", "Arimo", "Helvetica", "Nimbus Sans", "Nimbus Sans L"]),
    ("Helvetica", &["Liberation Sans", "Arimo", "Nimbus Sans", "Nimbus Sans L"]),
    ("Helvetica Neue", &["Liberation Sans", "Arimo", "Nimbus Sans"]),
    ("Arial Narrow", &["Liberation Sans Narrow", "Nimbus Sans Narrow"]),
    ("Times New Roman", &["Liberation Serif", "Tinos", "Nimbus Roman", "Nimbus Roman No9 L"]),
    ("Times", &["Liberation Serif", "Tinos", "Nimbus Roman"]),
    ("Courier New", &["Liberation Mono", "Cousine", "Nimbus Mono PS", "Nimbus Mono L"]),
    ("Courier", &["Liberation Mono", "Cousine", "Nimbus Mono PS"]),
    ("Georgia", &["Gelasio", "DejaVu Serif"]),
    ("Book Antiqua", &["TeX Gyre Pagella", "P052", "URW Palladio L"]),
    ("Palatino Linotype", &["TeX Gyre Pagella", "P052", "URW Palladio L"]),
    ("Century Gothic", &["TeX Gyre Adventor", "URW Gothic"]),
    ("Bookman Old Style", &["TeX Gyre Bonum", "URW Bookman"]),
    ("Century Schoolbook", &["TeX Gyre Schola", "C059"]),
    ("Verdana", &["DejaVu Sans"]),
    ("Tahoma", &["DejaVu Sans"]),
    ("Segoe UI", &["Noto Sans", "Open Sans", "Ubuntu", "Cantarell"]),
    ("Aptos", &["Noto Sans", "Inter", "Open Sans"]),
    ("Consolas", &["DejaVu Sans Mono", "Liberation Mono", "Noto Sans Mono", "Ubuntu Mono"]),
    ("Lucida Console", &["DejaVu Sans Mono", "Liberation Mono"]),
    ("Comic Sans MS", &["Comic Neue", "Comic Relief"]),
    ("MS Sans Serif", &["Liberation Sans", "DejaVu Sans"]),
    ("MS Gothic", &["Noto Sans CJK JP", "IPAGothic", "TakaoGothic"]),
    ("MS PGothic", &["Noto Sans CJK JP", "IPAPGothic", "TakaoPGothic"]),
    ("MS Mincho", &["Noto Serif CJK JP", "IPAMincho"]),
    ("Meiryo", &["Noto Sans CJK JP"]),
    ("Yu Gothic", &["Noto Sans CJK JP"]),
    ("SimSun", &["Noto Serif CJK SC", "AR PL UMing CN"]),
    ("Microsoft YaHei", &["Noto Sans CJK SC", "WenQuanYi Micro Hei"]),
    ("PMingLiU", &["Noto Serif CJK TC", "AR PL UMing TW"]),
    ("Malgun Gothic", &["Noto Sans CJK KR", "NanumGothic"]),
    ("Batang", &["Noto Serif CJK KR", "NanumMyeongjo"]),
];

const SERIF: &[&str] = &["Liberation Serif", "Tinos", "DejaVu Serif", "Noto Serif", "Nimbus Roman"];
const MONO: &[&str] = &["Liberation Mono", "DejaVu Sans Mono", "Noto Sans Mono", "Cousine", "Ubuntu Mono"];

/// How a family name is drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The installed family to draw with.
    pub family: SharedString,
    /// True when it isn't the family the cell asked for.
    pub substituted: bool,
}

struct FontBook {
    db: fontdb::Database,
    /// Lower-cased family name → the installed spelling.
    families: HashMap<String, String>,
    /// Every installed family (plus the app font), sorted for display.
    sorted: Vec<SharedString>,
}

fn book() -> &'static FontBook {
    static BOOK: OnceLock<FontBook> = OnceLock::new();
    BOOK.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let mut families = HashMap::new();
        for face in db.faces() {
            for (name, _) in &face.families {
                families.entry(name.to_lowercase()).or_insert_with(|| name.clone());
            }
        }
        families.insert(FONT.to_lowercase(), FONT.to_string());
        let mut sorted: Vec<SharedString> = families.values().filter(|n| !n.starts_with('.')).map(|n| SharedString::from(n.clone())).collect();
        sorted.sort_by_key(|n| n.to_lowercase());
        FontBook { db, families, sorted }
    })
}

/// Load the system font list on a background thread so the first cell that
/// names a font doesn't wait for it.
pub fn warm_up() {
    std::thread::spawn(|| {
        book();
    });
}

/// Every installed family, sorted, including the app font.
pub fn installed() -> &'static [SharedString] {
    &book().sorted
}

pub fn is_installed(name: &str) -> bool {
    book().families.contains_key(&name.to_lowercase())
}

/// The installed spelling of `name`, if installed.
fn installed_name(name: &str) -> Option<&'static str> {
    book().families.get(&name.to_lowercase()).map(String::as_str)
}

/// Is a family name serif, monospaced or neither, judging by its name?
fn kind_of(name: &str) -> Option<&'static [&'static str]> {
    let n = name.to_lowercase();
    if ["mono", "courier", "consol", "code", "typewriter", "fixed", "terminal"].iter().any(|k| n.contains(k)) {
        return Some(MONO);
    }
    if n.contains("sans") {
        return None;
    }
    if ["serif", "times", "roman", "georgia", "garamond", "cambria", "book", "palatino", "baskerville", "bodoni", "century", "mincho", "song", "ming", "batang", "didot", "caslon"]
        .iter()
        .any(|k| n.contains(k))
    {
        return Some(SERIF);
    }
    None
}

/// What to draw for a family name, given a test for what's installed.
fn resolve_with(name: &str, installed: impl Fn(&str) -> Option<String>) -> Resolved {
    if let Some(family) = installed(name) {
        return Resolved { family: family.into(), substituted: false };
    }
    let subs = SUBSTITUTES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, s)| *s).unwrap_or(&[]);
    let kind = kind_of(name).unwrap_or(&[]);
    for s in subs.iter().chain(kind) {
        if let Some(family) = installed(s) {
            return Resolved { family: family.into(), substituted: true };
        }
    }
    Resolved { family: FONT.into(), substituted: true }
}

/// What to draw for a family name (cached).
pub fn resolve(name: &str) -> Resolved {
    static CACHE: OnceLock<RwLock<HashMap<String, Resolved>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(r) = cache.read().unwrap().get(name) {
        return r.clone();
    }
    let r = resolve_with(name, |n| installed_name(n).map(str::to_string));
    cache.write().unwrap().insert(name.to_string(), r.clone());
    r
}

/// Fonts named by a workbook that are drawn with something else, with what
/// they're drawn with. A font with a known stand-in installed (Carlito for
/// Calibri, say) isn't listed: its text lays out the same.
pub fn missing(names: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<(String, Resolved)> {
    let mut out: Vec<(String, Resolved)> = Vec::new();
    for n in names {
        let n = n.as_ref();
        let r = resolve(n);
        let twin = SUBSTITUTES.iter().any(|(m, subs)| m.eq_ignore_ascii_case(n) && subs.iter().any(|s| s.eq_ignore_ascii_case(&r.family)));
        if r.substituted && !twin && !out.iter().any(|(m, _)| m.eq_ignore_ascii_case(n)) {
            out.push((n.to_string(), r));
        }
    }
    out.sort_by_key(|a| a.0.to_lowercase());
    out
}

/// A message about fonts a file uses that aren't installed, if any.
pub fn missing_notice(names: impl IntoIterator<Item = impl AsRef<str>>) -> Option<String> {
    let missing = missing(names);
    let first = missing.first()?;
    let shown = |(n, r): &(String, Resolved)| format!("{n} (shown as {})", r.family);
    Some(match missing.len() {
        1 => format!("Font not installed: {}", shown(first)),
        k => format!("{k} fonts not installed: {}", missing.iter().take(3).map(shown).collect::<Vec<_>>().join(", ") + if k > 3 { ", …" } else { "" }),
    })
}

// ------------------------------------------------------------------ PDF faces

/// Faces 0–3 are the bundled Source Sans 3 (regular, italic, bold, bold
/// italic); system fonts used by a print job are added after them.
/// (family, bold, italic).
type FaceKey = (String, bool, bool);

struct Faces {
    /// (family, bold, italic) → face index.
    by_key: HashMap<FaceKey, usize>,
    /// Font data, collection index and (family, bold, italic) of faces from 4 on.
    data: Vec<(&'static [u8], u32, FaceKey)>,
}

fn faces() -> &'static Mutex<Faces> {
    static F: OnceLock<Mutex<Faces>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(Faces { by_key: HashMap::new(), data: Vec::new() }))
}

/// The face index (for print layout and PDF) to draw a cell's text with.
pub fn face_for(name: Option<&str>, bold: bool, italic: bool) -> usize {
    let bundled = crate::print_layout::face_of(bold, italic);
    let Some(name) = name else { return bundled };
    let family = resolve(name).family;
    if family.as_ref() == FONT {
        return bundled;
    }
    let key = (family.to_string(), bold, italic);
    let mut f = faces().lock().unwrap();
    if let Some(&i) = f.by_key.get(&key) {
        return i;
    }
    let db = &book().db;
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(&family)],
        weight: if bold { fontdb::Weight::BOLD } else { fontdb::Weight::NORMAL },
        style: if italic { fontdb::Style::Italic } else { fontdb::Style::Normal },
        ..Default::default()
    };
    let found = db.query(&query).and_then(|id| db.with_face_data(id, |data, index| (data.to_vec(), index)));
    // Only faces ttf-parser can read (and so measure) are used.
    let i = match found.filter(|(data, index)| ttf_parser::Face::parse(data, *index).is_ok()) {
        Some((data, index)) => {
            // Leaked: a print job's faces live as long as the app, and each
            // (family, bold, italic) is loaded once.
            f.data.push((Box::leak(data.into_boxed_slice()), index, key.clone()));
            crate::print_layout::FACES.len() + f.data.len() - 1
        }
        None => bundled,
    };
    f.by_key.insert(key, i);
    i
}

/// Font data and collection index of a face.
pub fn face_data(face: usize) -> (&'static [u8], u32) {
    let bundled = &crate::print_layout::FACES;
    if face < bundled.len() {
        return (bundled[face], 0);
    }
    faces().lock().unwrap().data.get(face - bundled.len()).map_or((bundled[0], 0), |(d, i, _)| (*d, *i))
}

/// The family, boldness and slant of a face (for drawing it on screen).
pub fn face_font(face: usize) -> (SharedString, bool, bool) {
    let bundled = crate::print_layout::FACES.len();
    if face < bundled {
        return (FONT.into(), face / 2 == 1, face % 2 == 1);
    }
    faces().lock().unwrap().data.get(face - bundled).map_or((FONT.into(), false, false), |(_, _, (f, b, i))| (f.clone().into(), *b, *i))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with<'a>(list: &'a [&'a str]) -> impl Fn(&str) -> Option<String> + 'a {
        move |n: &str| list.iter().find(|i| i.eq_ignore_ascii_case(n)).map(|s| s.to_string())
    }

    #[test]
    fn installed_fonts_are_used_as_is() {
        let r = resolve_with("arial", with(&["Arial", "Liberation Sans"]));
        assert_eq!(r, Resolved { family: "Arial".into(), substituted: false });
    }

    #[test]
    fn office_fonts_fall_back_to_metric_twins() {
        let fonts = with(&["Carlito", "Liberation Sans", "Liberation Serif", "DejaVu Sans Mono"]);
        assert_eq!(resolve_with("Calibri", &fonts).family.as_ref(), "Carlito");
        assert_eq!(resolve_with("Arial", &fonts).family.as_ref(), "Liberation Sans");
        assert_eq!(resolve_with("Times New Roman", &fonts).family.as_ref(), "Liberation Serif");
        assert!(resolve_with("Calibri", &fonts).substituted);
        // Unknown fonts keep their kind.
        assert_eq!(resolve_with("Fancy Mono Pro", &fonts).family.as_ref(), "DejaVu Sans Mono");
        assert_eq!(resolve_with("Old Garamond", &fonts).family.as_ref(), "Liberation Serif");
        assert_eq!(resolve_with("Something Sans Serif", &fonts).family.as_ref(), FONT);
        assert_eq!(resolve_with("Wingdings", &fonts).family.as_ref(), FONT);
    }

    #[test]
    fn the_app_font_is_always_installed() {
        assert!(is_installed(FONT));
        assert!(installed().iter().any(|f| f.as_ref() == FONT));
        assert_eq!(resolve(FONT).family.as_ref(), FONT);
    }

    #[test]
    fn system_faces_measure_and_fall_back_to_the_bundled_font() {
        assert_eq!(face_for(None, true, false), 2);
        assert_eq!(face_for(Some(FONT), false, true), 1);
        // A name nothing resolves to draws with the bundled faces.
        assert_eq!(face_for(Some("No Such Font Anywhere"), false, false), 0);
        // Any installed font other than the app font gets its own face.
        if let Some(other) = installed().iter().find(|f| f.as_ref() != FONT && is_installed(f)) {
            let i = face_for(Some(other), false, false);
            if i >= 4 {
                let (data, index) = face_data(i);
                assert!(ttf_parser::Face::parse(data, index).is_ok());
                assert_eq!(face_for(Some(other), false, false), i, "faces are loaded once");
            }
        }
    }
}
