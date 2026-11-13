//! Lightweight scanner that finds cell references in (possibly incomplete)
//! formula text, for colored highlighting while editing.

use std::ops::Range as Span;

use xl_engine::{CellRef, MAX_ROWS, Range, parse_col};

#[derive(Clone, Debug)]
pub struct RefSpan {
    /// Byte span in the formula text, including any sheet prefix.
    pub span: Span<usize>,
    /// Explicit sheet name (`Data!A1`), if any.
    pub sheet: Option<String>,
    pub range: Range,
    /// Index into the theme's reference palette (same reference => same color).
    pub color: usize,
}

fn parse_col_part(s: &str) -> Option<u32> {
    parse_col(s.strip_prefix('$').unwrap_or(s))
}

fn parse_range(tok: &str) -> Option<Range> {
    match tok.split_once(':') {
        None => CellRef::parse(tok).map(Range::single),
        Some((a, b)) => match (CellRef::parse(a), CellRef::parse(b)) {
            (Some(x), Some(y)) => Some(Range::new(x, y)),
            _ => match (parse_col_part(a), parse_col_part(b)) {
                (Some(x), Some(y)) => Some(Range::new(CellRef::new(0, x), CellRef::new(MAX_ROWS - 1, y))),
                _ => None,
            },
        },
    }
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'$' | b':' | b'.' | b'_') || b >= 0x80
}

pub fn scan_refs(text: &str) -> Vec<RefSpan> {
    let mut out: Vec<RefSpan> = Vec::new();
    if !text.starts_with('=') {
        return out;
    }
    let b = text.as_bytes();
    let mut i = 1;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            i += 1;
            continue;
        }
        let start = i;
        // Optional sheet prefix: Name! or 'Quoted Name'!
        let mut sheet = None;
        if ch == b'\'' {
            let mut j = i + 1;
            let mut name = String::new();
            while j < b.len() {
                if b[j] == b'\'' {
                    if b.get(j + 1) == Some(&b'\'') {
                        name.push('\'');
                        j += 2;
                        continue;
                    }
                    break;
                }
                let c = text[j..].chars().next().unwrap();
                name.push(c);
                j += c.len_utf8();
            }
            if b.get(j + 1) == Some(&b'!') {
                sheet = Some(name);
                i = j + 2;
            } else {
                i = j + 1;
                continue;
            }
        } else if is_ident(ch) {
            let mut j = i;
            while j < b.len() && is_ident(b[j]) {
                j += 1;
            }
            if b.get(j) == Some(&b'!') {
                sheet = Some(text[i..j].to_string());
                i = j + 1;
            }
        } else {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        let tok_start = i;
        while i < b.len() && is_ident(b[i]) {
            i += 1;
        }
        if i == tok_start || b.get(i) == Some(&b'(') {
            continue;
        }
        if let Some(range) = parse_range(&text[tok_start..i]) {
            let same = |r: &RefSpan| r.range == range && r.sheet.as_deref().map(str::to_lowercase) == sheet.as_deref().map(str::to_lowercase);
            let color = out.iter().find(|r| same(r)).map(|r| r.color).unwrap_or_else(|| {
                let mut distinct: Vec<(Option<String>, Range)> = Vec::new();
                for r in &out {
                    let key = (r.sheet.as_deref().map(str::to_lowercase), r.range);
                    if !distinct.contains(&key) {
                        distinct.push(key);
                    }
                }
                distinct.len()
            });
            out.push(RefSpan { span: start..i, sheet, range, color });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_refs() {
        let r = scan_refs("=SUM(A1:B2)+C3*\"D4\"+a1:b2");
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].range.to_string(), "A1:B2");
        assert_eq!(r[1].range.to_string(), "C3");
        assert_eq!(r[2].color, r[0].color);
        assert_ne!(r[1].color, r[0].color);
        assert!(scan_refs("A1+B2").is_empty());
    }

    #[test]
    fn sheet_prefixes() {
        let text = "=Data!A1+'My Sheet'!B2:B4+A1";
        let r = scan_refs(text);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].sheet.as_deref(), Some("Data"));
        assert_eq!(&text[r[0].span.clone()], "Data!A1");
        assert_eq!(r[1].sheet.as_deref(), Some("My Sheet"));
        assert_eq!(&text[r[1].span.clone()], "'My Sheet'!B2:B4");
        assert_eq!(r[2].sheet, None);
        assert_ne!(r[2].color, r[0].color, "same cell on a different sheet gets its own color");
    }
}
