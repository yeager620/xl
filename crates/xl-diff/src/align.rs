//! Aligning two sequences of lines (rows or columns), each summarized by a
//! content hash, so that an inserted line doesn't make everything after it
//! look changed.
//!
//! 1. Lines with equal hashes are matched: common prefix and suffix, then
//!    patience-style anchors (hashes occurring exactly once on each side,
//!    longest increasing run), recursively between anchors; small leftover
//!    regions with no unique lines get an exact LCS.
//! 2. Between matched lines, the unmatched old and new lines are paired up
//!    as "modified" lines, choosing the most similar pairs (a small DP);
//!    whatever is left over was inserted or deleted.

use std::collections::HashMap;

/// One step of an alignment, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Old line `.0` corresponds to new line `.1` (equal or modified).
    Both(usize, usize),
    /// Old line deleted.
    Old(usize),
    /// New line inserted.
    New(usize),
}

/// Exact LCS is used on regions up to this many cells (old × new lines).
const LCS_LIMIT: usize = 1_000_000;
/// Similarity pairing (DP) is used on gaps up to this many cells.
const PAIR_LIMIT: usize = 250_000;
/// Deeper anchor recursion falls back to LCS / positional pairing.
const MAX_DEPTH: usize = 48;

/// Align `old` with `new`. `sim(i, j)` (0..=1) says how alike old line `i`
/// and new line `j` are; it's only asked about lines that aren't equal.
pub fn align(old: &[u64], new: &[u64], sim: &dyn Fn(usize, usize) -> f64) -> Vec<Step> {
    let mut matched = Vec::new();
    match_equal(old, new, (0, old.len()), (0, new.len()), 0, &mut matched);
    let mut out = Vec::with_capacity(old.len().max(new.len()));
    let (mut i, mut j) = (0, 0);
    for &(a, b) in &matched {
        pair_gap((i, a), (j, b), sim, &mut out);
        out.push(Step::Both(a, b));
        (i, j) = (a + 1, b + 1);
    }
    pair_gap((i, old.len()), (j, new.len()), sim, &mut out);
    out
}

/// Matched pairs of equal lines in old[a.0..a.1] × new[b.0..b.1], in order.
fn match_equal(old: &[u64], new: &[u64], a: (usize, usize), b: (usize, usize), depth: usize, out: &mut Vec<(usize, usize)>) {
    let ((mut lo1, mut hi1), (mut lo2, mut hi2)) = (a, b);
    while lo1 < hi1 && lo2 < hi2 && old[lo1] == new[lo2] {
        out.push((lo1, lo2));
        lo1 += 1;
        lo2 += 1;
    }
    let mut suffix = Vec::new();
    while lo1 < hi1 && lo2 < hi2 && old[hi1 - 1] == new[hi2 - 1] {
        hi1 -= 1;
        hi2 -= 1;
        suffix.push((hi1, hi2));
    }
    if lo1 < hi1 && lo2 < hi2 {
        let anchors = if depth < MAX_DEPTH { unique_anchors(old, new, (lo1, hi1), (lo2, hi2)) } else { Vec::new() };
        if !anchors.is_empty() {
            let (mut x, mut y) = (lo1, lo2);
            for (p, q) in anchors {
                match_equal(old, new, (x, p), (y, q), depth + 1, out);
                out.push((p, q));
                (x, y) = (p + 1, q + 1);
            }
            match_equal(old, new, (x, hi1), (y, hi2), depth + 1, out);
        } else if (hi1 - lo1).saturating_mul(hi2 - lo2) <= LCS_LIMIT {
            lcs(old, new, (lo1, hi1), (lo2, hi2), out);
        }
    }
    out.extend(suffix.into_iter().rev());
}

/// Lines unique on both sides, as the longest run increasing on both sides.
fn unique_anchors(old: &[u64], new: &[u64], a: (usize, usize), b: (usize, usize)) -> Vec<(usize, usize)> {
    // hash -> (count in old, position in old, count in new, position in new)
    let mut seen: HashMap<u64, (u32, usize, u32, usize)> = HashMap::new();
    for (i, h) in old.iter().enumerate().take(a.1).skip(a.0) {
        let e = seen.entry(*h).or_insert((0, i, 0, 0));
        e.0 += 1;
        e.1 = i;
    }
    for (j, h) in new.iter().enumerate().take(b.1).skip(b.0) {
        if let Some(e) = seen.get_mut(h) {
            e.2 += 1;
            e.3 = j;
        }
    }
    let mut pairs: Vec<(usize, usize)> = seen.values().filter(|e| e.0 == 1 && e.2 == 1).map(|e| (e.1, e.3)).collect();
    pairs.sort_unstable();
    longest_increasing(&pairs)
}

/// Longest subsequence of `pairs` (sorted by .0) increasing in .1.
fn longest_increasing(pairs: &[(usize, usize)]) -> Vec<(usize, usize)> {
    // tails[k] = index into pairs of the smallest tail of an increasing run of length k+1.
    let mut tails: Vec<usize> = Vec::new();
    let mut prev: Vec<Option<usize>> = vec![None; pairs.len()];
    for (i, &(_, y)) in pairs.iter().enumerate() {
        let k = tails.partition_point(|&t| pairs[t].1 < y);
        if k > 0 {
            prev[i] = Some(tails[k - 1]);
        }
        if k == tails.len() {
            tails.push(i);
        } else {
            tails[k] = i;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut cur = tails.last().copied();
    while let Some(i) = cur {
        out.push(pairs[i]);
        cur = prev[i];
    }
    out.reverse();
    out
}

/// Exact longest common subsequence (small regions only).
fn lcs(old: &[u64], new: &[u64], a: (usize, usize), b: (usize, usize), out: &mut Vec<(usize, usize)>) {
    let (n, m) = (a.1 - a.0, b.1 - b.0);
    let w = m + 1;
    let mut t = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i * w + j] = if old[a.0 + i] == new[b.0 + j] { t[(i + 1) * w + j + 1] + 1 } else { t[(i + 1) * w + j].max(t[i * w + j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old[a.0 + i] == new[b.0 + j] {
            out.push((a.0 + i, b.0 + j));
            i += 1;
            j += 1;
        } else if t[(i + 1) * w + j] >= t[i * w + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
}

/// Pair up the unmatched lines old[a.0..a.1] and new[b.0..b.1].
fn pair_gap(a: (usize, usize), b: (usize, usize), sim: &dyn Fn(usize, usize) -> f64, out: &mut Vec<Step>) {
    let (k, l) = (a.1 - a.0, b.1 - b.0);
    if k == l || k == 0 || l == 0 || k.saturating_mul(l) > PAIR_LIMIT {
        // Same count: edited in place. Otherwise pair from the top and
        // report the rest as deleted / inserted.
        let both = k.min(l);
        out.extend((0..both).map(|d| Step::Both(a.0 + d, b.0 + d)));
        out.extend((a.0 + both..a.1).map(Step::Old));
        out.extend((b.0 + both..b.1).map(Step::New));
        return;
    }
    // Maximize the total score of a monotone pairing. Pairing always scores
    // above leaving both lines out, so min(k, l) pairs are made; the DP picks
    // which lines go unpaired: the least similar.
    let w = l + 1;
    let mut s = vec![0f64; (k + 1) * w];
    for i in 1..=k {
        for j in 1..=l {
            let pair = s[(i - 1) * w + j - 1] + 0.3 + 0.7 * sim(a.0 + i - 1, b.0 + j - 1);
            s[i * w + j] = pair.max(s[(i - 1) * w + j]).max(s[i * w + j - 1]);
        }
    }
    let mut rev = Vec::with_capacity(k.max(l));
    let (mut i, mut j) = (k, l);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 {
            let pair = s[(i - 1) * w + j - 1] + 0.3 + 0.7 * sim(a.0 + i - 1, b.0 + j - 1);
            if (s[i * w + j] - pair).abs() < 1e-9 {
                rev.push(Step::Both(a.0 + i - 1, b.0 + j - 1));
                i -= 1;
                j -= 1;
                continue;
            }
        }
        if j > 0 && (i == 0 || (s[i * w + j] - s[i * w + j - 1]).abs() < 1e-9) {
            rev.push(Step::New(b.0 + j - 1));
            j -= 1;
        } else {
            rev.push(Step::Old(a.0 + i - 1));
            i -= 1;
        }
    }
    rev.reverse();
    out.extend(rev);
}

/// Old index -> new index, and new index -> old index, from an alignment.
pub fn maps(steps: &[Step], old_len: usize, new_len: usize) -> (Vec<Option<u32>>, Vec<Option<u32>>) {
    let mut fwd = vec![None; old_len];
    let mut back = vec![None; new_len];
    for s in steps {
        if let Step::Both(i, j) = *s {
            fwd[i] = Some(j as u32);
            back[j] = Some(i as u32);
        }
    }
    (fwd, back)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> Vec<u64> {
        s.bytes().map(u64::from).collect()
    }

    fn run(old: &str, new: &str) -> String {
        let (o, n) = (h(old), h(new));
        let steps = align(&o, &n, &|i, j| if (o[i] as u8).eq_ignore_ascii_case(&(n[j] as u8)) { 1.0 } else { 0.0 });
        steps
            .iter()
            .map(|s| match *s {
                Step::Both(i, j) if o[i] == n[j] => format!("={}", old.as_bytes()[i] as char),
                Step::Both(i, j) => format!("~{}{}", old.as_bytes()[i] as char, new.as_bytes()[j] as char),
                Step::Old(i) => format!("-{}", old.as_bytes()[i] as char),
                Step::New(j) => format!("+{}", new.as_bytes()[j] as char),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn inserted_line_keeps_the_rest_aligned() {
        assert_eq!(run("abcdef", "abcXdef"), "=a =b =c +X =d =e =f");
        assert_eq!(run("abcdef", "Xabcdef"), "+X =a =b =c =d =e =f");
        assert_eq!(run("abcdef", "abcdefX"), "=a =b =c =d =e =f +X");
    }

    #[test]
    fn deleted_lines() {
        assert_eq!(run("abcdef", "abef"), "=a =b -c -d =e =f");
        assert_eq!(run("abcdef", ""), "-a -b -c -d -e -f");
    }

    #[test]
    fn edited_line_is_paired_not_deleted_and_inserted() {
        assert_eq!(run("abcdef", "abXdef"), "=a =b ~cX =d =e =f");
    }

    #[test]
    fn insert_plus_edit_in_one_gap_pairs_the_similar_line() {
        // c was edited to C (similar) and X was inserted after it.
        assert_eq!(run("abcdef", "abCXdef"), "=a =b ~cC +X =d =e =f");
        // X inserted before the edit.
        assert_eq!(run("abcdef", "abXCdef"), "=a =b +X ~cC =d =e =f");
    }

    #[test]
    fn repeated_lines_without_unique_anchors() {
        // Blank-ish repeated lines: LCS still finds the insertion.
        assert_eq!(run("aaaa", "aaXaa"), "=a =a +X =a =a");
    }

    #[test]
    fn moved_block() {
        // A line moved from the end to the start: deleted there, inserted here.
        assert_eq!(run("abcdz", "zabcd"), "+z =a =b =c =d -z");
    }

    #[test]
    fn maps_round_trip() {
        let steps = align(&h("abcd"), &h("aXbd"), &|_, _| 0.0);
        let (fwd, back) = maps(&steps, 4, 4);
        assert_eq!(fwd, vec![Some(0), Some(2), None, Some(3)]);
        assert_eq!(back, vec![Some(0), None, Some(1), Some(3)]);
    }

    #[test]
    fn large_inputs_stay_fast() {
        let old: Vec<u64> = (0..200_000).collect();
        let mut new = old.clone();
        new.insert(100_000, 999_999_999);
        new.remove(5);
        let steps = align(&old, &new, &|_, _| 0.0);
        assert_eq!(steps.iter().filter(|s| matches!(s, Step::New(_))).count(), 1);
        assert_eq!(steps.iter().filter(|s| matches!(s, Step::Old(_))).count(), 1);
    }
}
