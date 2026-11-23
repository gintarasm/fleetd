//! Matching a query in terminal text (ADR 0053, 0098): the GUI's find in
//! scrollback matches a row's cells, the node's search of every agent's
//! output matches plain text. Both fold case the same way.

use std::ops::Range;

use crate::Cell;

/// What to look for: the query's characters, lower-cased unless the
/// search is case sensitive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Needle {
    chars: Vec<char>,
    /// `chars` as a string.
    text: String,
    case_sensitive: bool,
}

impl Needle {
    /// `None` for an empty query.
    pub fn new(text: &str, case_sensitive: bool) -> Option<Needle> {
        if text.is_empty() {
            return None;
        }
        let chars: Vec<char> = text
            .chars()
            .map(|c| if case_sensitive { c } else { fold(c) })
            .collect();
        Some(Needle {
            text: chars.iter().collect(),
            chars,
            case_sensitive,
        })
    }

    pub fn case_sensitive(&self) -> bool {
        self.case_sensitive
    }

    fn matches(&self, a: char, b: char) -> bool {
        if self.case_sensitive {
            a == b
        } else {
            fold(a) == b
        }
    }

    /// The byte range of the first match in `text`, if any.
    pub fn find_in(&self, text: &str) -> Option<Range<usize>> {
        self.find_from(text, 0)
    }

    /// The byte ranges of every match in `text`, left to right, not
    /// overlapping.
    pub fn find_all(&self, text: &str) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(r) = self.find_from(text, from) {
            from = r.end;
            out.push(r);
        }
        out
    }

    fn find_from(&self, text: &str, from: usize) -> Option<Range<usize>> {
        let hay = text.get(from..)?;
        if self.case_sensitive {
            return hay
                .find(&self.text)
                .map(|i| from + i..from + i + self.text.len());
        }
        // Fast path: an ASCII line folds byte by byte, and folds to ASCII
        // only, so a (folded) needle that is not ASCII cannot match it.
        if hay.is_ascii() {
            if !self.text.is_ascii() {
                return None;
            }
            let n = self.text.len();
            let needle = self.text.as_bytes();
            return hay
                .as_bytes()
                .windows(n)
                .position(|w| w.eq_ignore_ascii_case(needle))
                .map(|i| from + i..from + i + n);
        }
        let chars: Vec<(usize, char)> = hay.char_indices().collect();
        let n = self.chars.len();
        let mut i = 0;
        while i + n <= chars.len() {
            let hit = chars[i..i + n]
                .iter()
                .zip(&self.chars)
                .all(|(&(_, c), &q)| self.matches(c, q));
            if hit {
                let end = chars.get(i + n).map_or(hay.len(), |&(b, _)| b);
                return Some(from + chars[i].0..from + end);
            }
            i += 1;
        }
        None
    }
}

/// One character, lower-cased (its first lower-case character).
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Appends to `out` the columns `start..end` of every match of `needle`
/// in `cells`, left to right, not overlapping.
pub fn row_matches(cells: &[Cell], needle: &Needle, out: &mut Vec<(u16, u16)>) {
    let n = needle.chars.len();
    if cells.len() < n {
        return;
    }
    let mut cols = Vec::with_capacity(cells.len() + 1);
    let mut col = 0u16;
    for c in cells {
        cols.push(col);
        col = col.saturating_add(if c.wide { 2 } else { 1 });
    }
    cols.push(col);
    let mut i = 0;
    while i + n <= cells.len() {
        let hit = cells[i..i + n]
            .iter()
            .zip(&needle.chars)
            .all(|(c, &q)| c.extra.is_none() && needle.matches(c.ch, q));
        if hit {
            out.push((cols[i], cols[i + n]));
            i += n;
        } else {
            i += 1;
        }
    }
}

/// The text of a history line ([`HistoryEvent::Line`](crate::HistoryEvent)):
/// its CSI sequences (history lines carry SGR only) dropped, invalid UTF-8
/// replaced, trailing spaces trimmed. Written into `out` (cleared first).
pub fn history_line_text(line: &[u8], out: &mut String) {
    out.clear();
    if !line.contains(&0x1b) {
        out.push_str(&String::from_utf8_lossy(line));
    } else {
        let mut bytes = Vec::with_capacity(line.len());
        let mut i = 0;
        while i < line.len() {
            if line[i] == 0x1b {
                i += 1;
                if line.get(i) == Some(&b'[') {
                    i += 1;
                    // Parameters and intermediates, then one final byte.
                    while i < line.len() && !(0x40..=0x7e).contains(&line[i]) {
                        i += 1;
                    }
                }
                i += 1;
                continue;
            }
            bytes.push(line[i]);
            i += 1;
        }
        out.push_str(&String::from_utf8_lossy(&bytes));
    }
    let trimmed = out.trim_end_matches(' ').len();
    out.truncate(trimmed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(s: &str) -> Vec<Cell> {
        s.chars()
            .map(|ch| Cell {
                ch,
                wide: ch == '界',
                ..Cell::default()
            })
            .collect()
    }

    fn found(row: &str, q: &str, case: bool) -> Vec<(u16, u16)> {
        let mut out = Vec::new();
        row_matches(&cells(row), &Needle::new(q, case).unwrap(), &mut out);
        out
    }

    fn all(text: &str, q: &str, case: bool) -> Vec<Range<usize>> {
        Needle::new(q, case).unwrap().find_all(text)
    }

    fn text(line: &[u8]) -> String {
        let mut s = String::from("stale");
        history_line_text(line, &mut s);
        s
    }

    #[test]
    fn empty_query_is_no_needle() {
        assert_eq!(Needle::new("", false), None);
    }

    #[test]
    fn finds_every_match_left_to_right_without_overlap() {
        assert_eq!(found("abcabc", "abc", false), vec![(0, 3), (3, 6)]);
        assert_eq!(found("aaaa", "aa", false), vec![(0, 2), (2, 4)]);
        assert_eq!(found("ab", "abc", false), vec![]);
    }

    #[test]
    fn case_folds_unless_case_sensitive() {
        assert_eq!(found("Error ERROR error", "error", false).len(), 3);
        assert_eq!(found("Error ERROR error", "error", true), vec![(12, 17)]);
        assert_eq!(found("Error", "ERROR", false), vec![(0, 5)]);
        assert_eq!(found("error", "Error", true), vec![]);
    }

    #[test]
    fn wide_characters_take_two_columns() {
        assert_eq!(found("界x界x", "x", false), vec![(2, 3), (5, 6)]);
        assert_eq!(found("a界b", "界b", false), vec![(1, 4)]);
    }

    #[test]
    fn text_matches_are_byte_ranges() {
        assert_eq!(all("abcabc", "abc", false), vec![0..3, 3..6]);
        assert_eq!(all("aaaa", "aa", false), vec![0..2, 2..4]);
        assert_eq!(all("ab", "abc", false), Vec::<Range<usize>>::new());
        assert_eq!(all("Error ERROR error", "error", false).len(), 3);
        assert_eq!(all("Error ERROR error", "error", true), vec![12..17]);
        assert_eq!(all("Error", "ERROR", false), vec![0..5]);
    }

    #[test]
    fn text_matches_fold_non_ascii() {
        assert_eq!(all("Ąžuolas ĄŽUOLAS", "ąžuolas", false), vec![0..9, 10..19]);
        assert_eq!(all("界 ERROR 界", "error", false), vec![4..9]);
        assert_eq!(all("plain ascii", "ž", false), Vec::<Range<usize>>::new());
        assert_eq!(all("Žž", "ž", true), vec![2..4]);
    }

    #[test]
    fn history_lines_lose_their_sgr_and_trailing_spaces() {
        assert_eq!(text(b"plain"), "plain");
        assert_eq!(text(b"\x1b[1;31mred\x1b[0m and \x1b[4:3mcurl\x1b[0m  "), "red and curl");
        assert_eq!(text(b"err\x1b[38;2;1;2;3mor"), "error");
        assert_eq!(text("界\x1b[0m!".as_bytes()), "界!");
        assert_eq!(text(b""), "");
    }
}
