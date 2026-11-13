//! Cell addresses and ranges (A1 notation).

use std::fmt;

/// Excel's sheet limits.
pub const MAX_ROWS: u32 = 1_048_576;
pub const MAX_COLS: u32 = 16_384;

/// Stable sheet identifier (never reused within a workbook).
pub type SheetId = u32;

/// A cell on a specific sheet.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Addr {
    pub sheet: SheetId,
    pub cell: CellRef,
}

impl Addr {
    pub const fn new(sheet: SheetId, cell: CellRef) -> Self {
        Self { sheet, cell }
    }
}

/// A zero-based cell coordinate.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CellRef {
    pub row: u32,
    pub col: u32,
}

impl CellRef {
    pub const fn new(row: u32, col: u32) -> Self {
        Self { row, col }
    }

    /// Parse `A1`, `$B$7`, `xfd1048576` (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        parse_a1(s).map(|a| a.cell)
    }

    /// Offset by (dr, dc), returning None if it falls off the sheet.
    pub fn offset(self, dr: i64, dc: i64) -> Option<Self> {
        let r = self.row as i64 + dr;
        let c = self.col as i64 + dc;
        (r >= 0 && c >= 0 && r < MAX_ROWS as i64 && c < MAX_COLS as i64)
            .then(|| Self::new(r as u32, c as u32))
    }
}

impl fmt::Display for CellRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", col_name(self.col), self.row + 1)
    }
}

/// A rectangular, normalized (start <= end) range of cells.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Range {
    pub start: CellRef,
    pub end: CellRef,
}

impl Range {
    pub fn new(a: CellRef, b: CellRef) -> Self {
        Self {
            start: CellRef::new(a.row.min(b.row), a.col.min(b.col)),
            end: CellRef::new(a.row.max(b.row), a.col.max(b.col)),
        }
    }

    pub fn single(c: CellRef) -> Self {
        Self { start: c, end: c }
    }

    pub fn contains(&self, c: CellRef) -> bool {
        c.row >= self.start.row && c.row <= self.end.row && c.col >= self.start.col && c.col <= self.end.col
    }

    pub fn rows(&self) -> u32 {
        self.end.row - self.start.row + 1
    }

    pub fn cols(&self) -> u32 {
        self.end.col - self.start.col + 1
    }

    pub fn cell_count(&self) -> u64 {
        self.rows() as u64 * self.cols() as u64
    }

    /// Row-major iteration over every cell (including empty ones).
    pub fn iter(&self) -> impl Iterator<Item = CellRef> + '_ {
        (self.start.row..=self.end.row)
            .flat_map(move |r| (self.start.col..=self.end.col).map(move |c| CellRef::new(r, c)))
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}:{}", self.start, self.end)
        }
    }
}

/// Column index to letters: 0 -> A, 25 -> Z, 26 -> AA.
pub fn col_name(mut col: u32) -> String {
    let mut buf = [0u8; 4];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'A' + (col % 26) as u8;
        if col < 26 {
            break;
        }
        col = col / 26 - 1;
    }
    String::from_utf8_lossy(&buf[i..]).into_owned()
}

/// Letters to column index; None if invalid or out of range.
pub fn parse_col(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > 3 {
        return None;
    }
    let mut n: u32 = 0;
    for b in s.bytes() {
        let b = b.to_ascii_uppercase();
        if !b.is_ascii_uppercase() {
            return None;
        }
        n = n * 26 + (b - b'A' + 1) as u32;
    }
    (1..=MAX_COLS).contains(&n).then(|| n - 1)
}

/// A parsed A1 reference including `$` absolute markers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct A1 {
    pub cell: CellRef,
    pub col_abs: bool,
    pub row_abs: bool,
}

pub fn parse_a1(s: &str) -> Option<A1> {
    let b = s.as_bytes();
    let mut i = 0;
    let col_abs = b.first() == Some(&b'$');
    if col_abs {
        i += 1;
    }
    let cs = i;
    while i < b.len() && b[i].is_ascii_alphabetic() {
        i += 1;
    }
    let col = parse_col(&s[cs..i])?;
    let row_abs = b.get(i) == Some(&b'$');
    if row_abs {
        i += 1;
    }
    let rs = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i != b.len() || rs == i || b[rs] == b'0' {
        return None;
    }
    let row: u32 = s[rs..i].parse().ok()?;
    (1..=MAX_ROWS).contains(&row).then(|| A1 {
        cell: CellRef::new(row - 1, col),
        col_abs,
        row_abs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn col_names_roundtrip() {
        for c in [0, 1, 25, 26, 27, 51, 52, 701, 702, 16_383] {
            assert_eq!(parse_col(&col_name(c)), Some(c), "{c}");
        }
        assert_eq!(col_name(0), "A");
        assert_eq!(col_name(27), "AB");
        assert_eq!(col_name(16_383), "XFD");
        assert_eq!(parse_col("XFE"), None);
    }

    #[test]
    fn a1_parsing() {
        assert_eq!(CellRef::parse("A1"), Some(CellRef::new(0, 0)));
        assert_eq!(CellRef::parse("$c$10"), Some(CellRef::new(9, 2)));
        assert_eq!(CellRef::parse("A0"), None);
        assert_eq!(CellRef::parse("A"), None);
        assert_eq!(CellRef::parse("1"), None);
        assert_eq!(CellRef::parse("A1048577"), None);
    }
}
