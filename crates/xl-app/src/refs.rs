//! Lightweight scanner that finds cell references in (possibly incomplete)
//! formula text, for colored highlighting while editing.

use std::ops::Range as Span;

use xl_engine::{CellRef, MAX_ROWS, Range, parse_col};

#[derive(Clone, Debug)]
pub struct RefSpan {
    pub span: Span<usize>,
    pub range: Range,
    /// Index into the theme's reference palette (same range => same color).
    pub color: usize,
}

fn parse_part(s: &str) -> Option<CellRef> {
    CellRef::parse(s)
}

fn parse_col_part(s: &str) -> Option<u32> {
    parse_col(s.strip_prefix('$').unwrap_or(s))
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
        if ch.is_ascii_alphanumeric() || ch == b'$' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'$' | b':' | b'.' | b'_')) {
                i += 1;
            }
            // Function names are followed by '('.
            if b.get(i) == Some(&b'(') {
                continue;
            }
            let tok = &text[start..i];
            let range = match tok.split_once(':') {
                None => parse_part(tok).map(Range::single),
                Some((a, b2)) => match (parse_part(a), parse_part(b2)) {
                    (Some(x), Some(y)) => Some(Range::new(x, y)),
                    _ => match (parse_col_part(a), parse_col_part(b2)) {
                        (Some(x), Some(y)) => Some(Range::new(CellRef::new(0, x), CellRef::new(MAX_ROWS - 1, y))),
                        _ => None,
                    },
                },
            };
            if let Some(range) = range {
                let color = out
                    .iter()
                    .find(|r| r.range == range)
                    .map(|r| r.color)
                    .unwrap_or_else(|| {
                        let mut distinct: Vec<Range> = Vec::new();
                        for r in &out {
                            if !distinct.contains(&r.range) {
                                distinct.push(r.range);
                            }
                        }
                        distinct.len()
                    });
                out.push(RefSpan { span: start..i, range, color });
            }
            continue;
        }
        i += 1;
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
}
