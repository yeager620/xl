//! Page setup and pagination: how a sheet prints.
//!
//! [`PageSetup`] holds what Excel keeps per sheet for printing (paper,
//! orientation, margins, scaling, print area and titles, headers and
//! footers, manual page breaks…). [`paginate`] splits the printed range
//! into pages the way Excel does: whole rows and columns per page, manual
//! breaks honored, title rows/columns repeated, hidden lines skipped, and
//! "fit to W × H pages" solved by shrinking the scale.
//!
//! Units: margins are in inches (as in the file); everything else here is
//! in points (1/72 inch).

use std::collections::BTreeSet;

use crate::address::{CellRef, Range};
use crate::formula::Shift;

/// Points per inch.
pub const PT_PER_INCH: f64 = 72.0;
/// xl's horizontal pixels to printed points. xl's 100px default column is
/// Excel's 64px one (48pt at 96 dpi).
pub const COL_PX_TO_PT: f64 = 48.0 / 100.0;
/// xl's vertical pixels to printed points: the 22px default row is Excel's
/// 15pt one.
pub const ROW_PX_TO_PT: f64 = 15.0 / 22.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Orientation {
    #[default]
    Portrait,
    Landscape,
}

/// Paper sizes xl knows by name: (Excel `paperSize` code, name, width and
/// height in points, portrait).
pub const PAPERS: &[(u16, &str, f64, f64)] = &[
    (1, "Letter", 612.0, 792.0),
    (5, "Legal", 612.0, 1008.0),
    (9, "A4", 595.276, 841.89),
    (8, "A3", 841.89, 1190.551),
    (11, "A5", 419.528, 595.276),
    (3, "Tabloid", 792.0, 1224.0),
    (7, "Executive", 522.0, 756.0),
    (12, "B4 (JIS)", 728.504, 1031.811),
    (13, "B5 (JIS)", 515.906, 728.504),
];

/// Page margins in inches.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Margins {
    pub left: f64,
    pub right: f64,
    pub top: f64,
    pub bottom: f64,
    pub header: f64,
    pub footer: f64,
}

impl Default for Margins {
    /// Excel's "Normal" margins.
    fn default() -> Self {
        Margins { left: 0.7, right: 0.7, top: 0.75, bottom: 0.75, header: 0.3, footer: 0.3 }
    }
}

/// Print area, title rows and title columns (kept in files as defined names).
pub type PrintNames = (Option<Range>, Option<(u32, u32)>, Option<(u32, u32)>);

/// How a sheet prints.
#[derive(Clone, Debug, PartialEq)]
pub struct PageSetup {
    pub orientation: Orientation,
    /// Excel's `paperSize` code (see [`PAPERS`]); unknown codes print on Letter.
    pub paper: u16,
    pub margins: Margins,
    /// Percent, 10–400 (ignored when fitting to pages).
    pub scale: u16,
    /// Fit to (width, height) pages; 0 leaves that direction unconstrained.
    pub fit: Option<(u16, u16)>,
    /// What to print; None prints the used range.
    pub print_area: Option<Range>,
    /// Rows and columns repeated on every page (first, last).
    pub title_rows: Option<(u32, u32)>,
    pub title_cols: Option<(u32, u32)>,
    pub gridlines: bool,
    /// Row numbers and column letters.
    pub headings: bool,
    pub center_h: bool,
    pub center_v: bool,
    /// Page order: across then down instead of Excel's default down then across.
    pub over_then_down: bool,
    /// Header and footer in Excel's code form ("&LLeft&CPage &P of &N&R&D").
    pub header: String,
    pub footer: String,
    /// Number of the first page (None: 1, or Excel's "auto").
    pub first_page: Option<u32>,
    /// Manual breaks: each is the first row (column) of a new page.
    pub row_breaks: BTreeSet<u32>,
    pub col_breaks: BTreeSet<u32>,
    pub black_and_white: bool,
}

impl Default for PageSetup {
    fn default() -> Self {
        PageSetup {
            orientation: Orientation::Portrait,
            paper: 1,
            margins: Margins::default(),
            scale: 100,
            fit: None,
            print_area: None,
            title_rows: None,
            title_cols: None,
            gridlines: false,
            headings: false,
            center_h: false,
            center_v: false,
            over_then_down: false,
            header: String::new(),
            footer: String::new(),
            first_page: None,
            row_breaks: BTreeSet::new(),
            col_breaks: BTreeSet::new(),
            black_and_white: false,
        }
    }
}

impl PageSetup {
    pub fn is_default(&self) -> bool {
        *self == PageSetup::default()
    }

    /// The paper's name, or "Paper N" for codes xl doesn't know.
    pub fn paper_name(&self) -> String {
        PAPERS.iter().find(|p| p.0 == self.paper).map_or_else(|| format!("Paper {}", self.paper), |p| p.1.to_string())
    }

    /// Page width and height in points, with the orientation applied.
    pub fn page_size(&self) -> (f64, f64) {
        let (w, h) = PAPERS.iter().find(|p| p.0 == self.paper).map_or((612.0, 792.0), |p| (p.2, p.3));
        match self.orientation {
            Orientation::Portrait => (w, h),
            Orientation::Landscape => (h, w),
        }
    }

    /// The area inside the margins, in points: (x, y, width, height).
    pub fn printable(&self) -> (f64, f64, f64, f64) {
        let (w, h) = self.page_size();
        let m = &self.margins;
        let (l, r, t, b) = (m.left * PT_PER_INCH, m.right * PT_PER_INCH, m.top * PT_PER_INCH, m.bottom * PT_PER_INCH);
        (l, t, (w - l - r).max(36.0), (h - t - b).max(36.0))
    }

    /// Only the parts kept as defined names: print area, title rows and columns.
    pub fn names_part(&self) -> PrintNames {
        (self.print_area, self.title_rows, self.title_cols)
    }

    /// Everything but the parts kept as defined names.
    pub fn without_names(&self) -> PageSetup {
        PageSetup { print_area: None, title_rows: None, title_cols: None, ..self.clone() }
    }

    /// Follow inserted or deleted lines on the sheet (the caller checks
    /// that `op` is on this sheet). A print area or titles wholly deleted
    /// are dropped, as are breaks on deleted lines.
    pub fn apply_shift(&mut self, op: Shift) {
        let rows = matches!(op, Shift::InsertRows { .. } | Shift::DeleteRows { .. });
        self.print_area = self.print_area.and_then(|r| op.map_range(r));
        let span = |s: Option<(u32, u32)>| s.and_then(|(a, b)| op.map_span_pub(a, b));
        let breaks = |set: &mut BTreeSet<u32>| *set = std::mem::take(set).into_iter().filter_map(|b| op.map_pos(b)).filter(|&b| b > 0).collect();
        if rows {
            self.title_rows = span(self.title_rows);
            breaks(&mut self.row_breaks);
        } else {
            self.title_cols = span(self.title_cols);
            breaks(&mut self.col_breaks);
        }
    }
}

// ---------------------------------------------------------- headers/footers

/// What header and footer codes stand for when printing.
#[derive(Clone, Debug, Default)]
pub struct HfContext {
    pub page: u32,
    pub pages: u32,
    pub date: String,
    pub time: String,
    /// File name (&F) and the folder it's in (&Z).
    pub file: String,
    pub path: String,
    /// Sheet name (&A).
    pub sheet: String,
}

/// Left, center and right sections of an Excel header/footer string, with
/// codes for fields kept (`&P`, `&N`…) and formatting codes removed.
pub fn split_sections(src: &str) -> [String; 3] {
    let mut out: [String; 3] = Default::default();
    // Text before any &L/&C/&R goes in the center, like Excel.
    let mut cur = 1;
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '&' {
            out[cur].push(c);
            i += 1;
            continue;
        }
        let Some(&n) = chars.get(i + 1) else { break };
        match n {
            'L' => cur = 0,
            'C' => cur = 1,
            'R' => cur = 2,
            _ => {
                // Kept for expansion: fields and the escaped ampersand.
                out[cur].push('&');
                out[cur].push(n);
            }
        }
        i += 2;
    }
    out
}

/// One section with its codes expanded: fields filled in, formatting codes
/// (fonts, sizes, colors, bold/italic toggles…) and pictures dropped.
pub fn expand_codes(section: &str, cx: &HfContext) -> String {
    let chars: Vec<char> = section.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '&' {
            out.push(c);
            i += 1;
            continue;
        }
        let Some(&n) = chars.get(i + 1) else { break };
        i += 2;
        match n {
            '&' => out.push('&'),
            'P' | 'N' => {
                let mut v = if n == 'P' { cx.page as i64 } else { cx.pages as i64 };
                // "&P+2" / "&P-1" offsets.
                if let Some(&sign @ ('+' | '-')) = chars.get(i)
                    && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())
                {
                    let mut j = i + 1;
                    let mut num = 0i64;
                    while let Some(d) = chars.get(j).and_then(|d| d.to_digit(10)) {
                        num = num * 10 + d as i64;
                        j += 1;
                    }
                    v += if sign == '+' { num } else { -num };
                    i = j;
                }
                out.push_str(&v.to_string());
            }
            'D' => out.push_str(&cx.date),
            'T' => out.push_str(&cx.time),
            'F' => out.push_str(&cx.file),
            'Z' => out.push_str(&cx.path),
            'A' => out.push_str(&cx.sheet),
            '"' => {
                // &"Font,Style": skip to the closing quote.
                while i < chars.len() && chars[i] != '"' {
                    i += 1;
                }
                i += 1;
            }
            'K' => {
                // &Krrggbb or &KttSnnn (theme color).
                i += 6.min(chars.len().saturating_sub(i));
            }
            d if d.is_ascii_digit() => {
                // Font size: up to three digits.
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            // &B &I &U &E &S &X &Y &O &H &G and anything unknown.
            _ => {}
        }
    }
    out
}

/// A header/footer string expanded into its three sections.
pub fn render_header(src: &str, cx: &HfContext) -> [String; 3] {
    split_sections(src).map(|s| expand_codes(&s, cx))
}

/// Build an Excel header/footer string from three plain sections
/// (ampersands in them are escaped unless they start a known code).
pub fn join_sections(left: &str, center: &str, right: &str) -> String {
    let mut out = String::new();
    for (code, text) in [("&L", left), ("&C", center), ("&R", right)] {
        if !text.is_empty() {
            out.push_str(code);
            out.push_str(text);
        }
    }
    out
}

// ---------------------------------------------------------------- pages

/// One printed page: the rows and columns of the print range it shows,
/// plus the title rows/columns repeated above and to the left.
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub rows: (u32, u32),
    pub cols: (u32, u32),
    pub title_rows: Option<(u32, u32)>,
    pub title_cols: Option<(u32, u32)>,
    /// Position in the printed sequence, from 0.
    pub index: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pagination {
    /// The scale applied (1.0 = 100%).
    pub scale: f64,
    pub pages: Vec<Page>,
    /// Row and column bands (first, last) the range was split into.
    pub row_bands: Vec<(u32, u32)>,
    pub col_bands: Vec<(u32, u32)>,
    /// The range paginated.
    pub area: Range,
}

impl Pagination {
    /// Rows (columns) where a new page starts, after the first, for drawing
    /// page-break lines.
    pub fn row_starts(&self) -> Vec<u32> {
        self.row_bands.iter().skip(1).map(|b| b.0).collect()
    }

    pub fn col_starts(&self) -> Vec<u32> {
        self.col_bands.iter().skip(1).map(|b| b.0).collect()
    }
}

/// Split lines `first..=last` into bands no larger than `avail` (points,
/// unscaled), starting a band at each manual break. A line larger than a
/// page gets a band of its own. `titles` (first, last, size) shrink the
/// space on bands that start past them.
fn bands(first: u32, last: u32, avail: f64, size: &dyn Fn(u32) -> f64, breaks: &BTreeSet<u32>, titles: Option<(u32, u32, f64)>) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut start = first;
    while start <= last {
        let room = match titles {
            Some((_, t_end, t_size)) if start > t_end => (avail - t_size).max(avail * 0.1),
            _ => avail,
        };
        let mut used = 0.0;
        let mut end = start;
        let mut i = start;
        loop {
            if i > last {
                break;
            }
            if i > start && breaks.contains(&i) {
                break;
            }
            let s = size(i);
            if used + s > room + 1e-6 && used > 0.0 {
                break;
            }
            used += s;
            end = i;
            if i == u32::MAX {
                break;
            }
            i += 1;
        }
        // Bands of only hidden lines would print blank pages: fold them into
        // the previous band.
        if used == 0.0 && !out.is_empty() && !(start..=end).any(|l| breaks.contains(&l)) {
            if let Some(prev) = out.last_mut() {
                let p: &mut (u32, u32) = prev;
                p.1 = end;
            }
        } else if used > 0.0 || out.is_empty() {
            out.push((start, end));
        }
        if end == u32::MAX {
            break;
        }
        start = end + 1;
    }
    out
}

/// Total size of lines `a..=b`.
fn span_size(a: u32, b: u32, size: &dyn Fn(u32) -> f64) -> f64 {
    (a..=b).map(size).sum()
}

/// Paginate `area` with the given column widths and row heights in points
/// (0 for hidden lines). `extra` is the size of the row-number column and
/// column-letter row when headings print (width, height).
pub fn paginate(setup: &PageSetup, area: Range, col_pt: &dyn Fn(u32) -> f64, row_pt: &dyn Fn(u32) -> f64, extra: (f64, f64)) -> Pagination {
    let (_, _, avail_w, avail_h) = setup.printable();
    // Title lines count only where they're outside the print range or above it.
    let title_rows = setup.title_rows.map(|(a, b)| (a, b.max(a)));
    let title_cols = setup.title_cols.map(|(a, b)| (a, b.max(a)));
    let t_rows = title_rows.map(|(a, b)| (a, b, span_size(a, b, row_pt)));
    let t_cols = title_cols.map(|(a, b)| (a, b, span_size(a, b, col_pt)));
    let run = |scale: f64| {
        let w = (avail_w / scale - extra.0).max(1.0);
        let h = (avail_h / scale - extra.1).max(1.0);
        let cb = bands(area.start.col, area.end.col, w, col_pt, &setup.col_breaks, t_cols);
        let rb = bands(area.start.row, area.end.row, h, row_pt, &setup.row_breaks, t_rows);
        (rb, cb)
    };
    let mut scale = match setup.fit {
        Some(_) => 1.0,
        None => (setup.scale.clamp(10, 400) as f64) / 100.0,
    };
    let (mut rb, mut cb) = run(scale);
    if let Some((fw, fh)) = setup.fit {
        let fits = |rb: &Vec<(u32, u32)>, cb: &Vec<(u32, u32)>| (fw == 0 || cb.len() <= fw as usize) && (fh == 0 || rb.len() <= fh as usize);
        if !fits(&rb, &cb) {
            // A first guess from total sizes, then step down until it fits
            // (manual breaks can make it impossible: stop at 10%).
            let total_w = span_size(area.start.col, area.end.col, col_pt) + extra.0;
            let total_h = span_size(area.start.row, area.end.row, row_pt) + extra.1;
            let mut guess: f64 = 1.0;
            if fw > 0 && total_w > 0.0 {
                guess = guess.min(avail_w * fw as f64 / total_w);
            }
            if fh > 0 && total_h > 0.0 {
                guess = guess.min(avail_h * fh as f64 / total_h);
            }
            scale = ((guess * 100.0).floor() / 100.0).clamp(0.1, 1.0);
            loop {
                (rb, cb) = run(scale);
                if fits(&rb, &cb) || scale <= 0.1 + 1e-9 {
                    break;
                }
                scale = ((scale * 100.0).round() - 1.0).max(10.0) / 100.0;
            }
        }
    }
    let mut pages = Vec::new();
    let mut push = |r: (u32, u32), c: (u32, u32)| {
        let tr = title_rows.filter(|t| r.0 > t.1);
        let tc = title_cols.filter(|t| c.0 > t.1);
        let index = pages.len();
        pages.push(Page { rows: r, cols: c, title_rows: tr, title_cols: tc, index });
    };
    if setup.over_then_down {
        for r in &rb {
            for c in &cb {
                push(*r, *c);
            }
        }
    } else {
        for c in &cb {
            for r in &rb {
                push(*r, *c);
            }
        }
    }
    Pagination { scale, pages, row_bands: rb, col_bands: cb, area }
}

/// The range to print when there's no print area: from A1 to the last
/// cell with something in it (`extent`), or None for an empty sheet.
pub fn default_area(extent: Option<CellRef>) -> Option<Range> {
    extent.map(|e| Range::new(CellRef::new(0, 0), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(a: &str, b: &str) -> Range {
        Range::new(CellRef::parse(a).unwrap(), CellRef::parse(b).unwrap())
    }

    /// Letter portrait with no margins: 612 × 792 points.
    fn bare() -> PageSetup {
        PageSetup { margins: Margins { left: 0.0, right: 0.0, top: 0.0, bottom: 0.0, header: 0.0, footer: 0.0 }, ..Default::default() }
    }

    #[test]
    fn splits_rows_and_columns_into_pages_down_then_over() {
        // 100pt columns, 100pt rows: 6 columns and 7 rows fit a page.
        let p = paginate(&bare(), r("A1", "J20"), &|_| 100.0, &|_| 100.0, (0.0, 0.0));
        assert_eq!(p.col_bands, vec![(0, 5), (6, 9)]);
        assert_eq!(p.row_bands, vec![(0, 6), (7, 13), (14, 19)]);
        assert_eq!(p.pages.len(), 6);
        assert_eq!((p.pages[1].rows, p.pages[1].cols), ((7, 13), (0, 5)));
        let over = paginate(&PageSetup { over_then_down: true, ..bare() }, r("A1", "J20"), &|_| 100.0, &|_| 100.0, (0.0, 0.0));
        assert_eq!((over.pages[1].rows, over.pages[1].cols), ((0, 6), (6, 9)));
    }

    #[test]
    fn scale_changes_how_much_fits() {
        let half = PageSetup { scale: 50, ..bare() };
        let p = paginate(&half, r("A1", "J20"), &|_| 100.0, &|_| 100.0, (0.0, 0.0));
        assert_eq!(p.scale, 0.5);
        assert_eq!(p.col_bands, vec![(0, 9)]);
        assert_eq!(p.row_bands, vec![(0, 14), (15, 19)]);
    }

    #[test]
    fn fit_to_pages_shrinks_until_it_fits() {
        let fit = PageSetup { fit: Some((1, 0)), ..bare() };
        let p = paginate(&fit, r("A1", "L30"), &|_| 100.0, &|_| 100.0, (0.0, 0.0));
        assert_eq!(p.col_bands.len(), 1);
        assert!(p.scale <= 0.51 && p.scale >= 0.4, "{}", p.scale);
        let one = PageSetup { fit: Some((1, 1)), ..bare() };
        let p = paginate(&one, r("A1", "L30"), &|_| 100.0, &|_| 100.0, (0.0, 0.0));
        assert_eq!(p.pages.len(), 1);
        // Never enlarges.
        let small = paginate(&one, r("A1", "B2"), &|_| 100.0, &|_| 100.0, (0.0, 0.0));
        assert_eq!(small.scale, 1.0);
    }

    #[test]
    fn manual_breaks_start_new_pages() {
        let mut s = bare();
        s.row_breaks.insert(3);
        s.col_breaks.insert(2);
        let p = paginate(&s, r("A1", "D10"), &|_| 50.0, &|_| 50.0, (0.0, 0.0));
        assert_eq!(p.row_bands, vec![(0, 2), (3, 9)]);
        assert_eq!(p.col_bands, vec![(0, 1), (2, 3)]);
        assert_eq!(p.row_starts(), vec![3]);
    }

    #[test]
    fn hidden_lines_take_no_room() {
        // Rows 2..=8 hidden: 20 rows of 100pt minus 7 → 13 rows on 2 pages.
        let row = |i: u32| if (2..=8).contains(&i) { 0.0 } else { 100.0 };
        let p = paginate(&bare(), r("A1", "A20"), &|_| 100.0, &row, (0.0, 0.0));
        assert_eq!(p.row_bands, vec![(0, 13), (14, 19)]);
    }

    #[test]
    fn title_rows_repeat_and_take_room() {
        let s = PageSetup { title_rows: Some((0, 0)), ..bare() };
        let p = paginate(&s, r("A1", "A20"), &|_| 100.0, &|_| 100.0, (0.0, 0.0));
        // First page: rows 0..=6 (titles are on it anyway); then 6 per page.
        assert_eq!(p.row_bands, vec![(0, 6), (7, 12), (13, 18), (19, 19)]);
        assert_eq!(p.pages[0].title_rows, None);
        assert_eq!(p.pages[1].title_rows, Some((0, 0)));
    }

    #[test]
    fn headers_expand_fields_and_drop_formatting() {
        let cx = HfContext { page: 2, pages: 5, sheet: "Data".into(), file: "book.xlsx".into(), date: "2026-11-15".into(), ..Default::default() };
        let [l, c, r] = render_header("&L&\"Arial,Bold\"&14&A&CPage &P of &N&R&D && more", &cx);
        assert_eq!((l.as_str(), c.as_str(), r.as_str()), ("Data", "Page 2 of 5", "2026-11-15 & more"));
        assert_eq!(render_header("Plain &P+1", &cx)[1], "Plain 3");
        assert_eq!(render_header("&K00FF00Green&B!", &cx)[1], "Green!");
        assert_eq!(join_sections("a", "", "c"), "&La&Rc");
    }

    #[test]
    fn shifts_move_area_titles_and_breaks() {
        let mut s = PageSetup { print_area: Some(r("B2", "D9")), title_rows: Some((0, 1)), ..Default::default() };
        s.row_breaks.insert(5);
        s.apply_shift(Shift::InsertRows { sheet: 0, at: 3, count: 2 });
        assert_eq!(s.print_area, Some(r("B2", "D11")));
        assert_eq!(s.row_breaks.iter().copied().collect::<Vec<_>>(), vec![7]);
        s.apply_shift(Shift::DeleteRows { sheet: 0, at: 0, count: 2 });
        assert_eq!(s.title_rows, None);
        s.apply_shift(Shift::DeleteCols { sheet: 0, at: 1, count: 3 });
        assert_eq!(s.print_area, None);
    }

    #[test]
    fn page_sizes_follow_orientation() {
        let a4 = PageSetup { paper: 9, orientation: Orientation::Landscape, ..Default::default() };
        let (w, h) = a4.page_size();
        assert!((w - 841.89).abs() < 0.01 && (h - 595.276).abs() < 0.01);
        assert_eq!(a4.paper_name(), "A4");
    }
}
