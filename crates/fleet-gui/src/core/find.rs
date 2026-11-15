//! Find in a pane's scrollback (ADR 0053): the text matched in a row's
//! cells, and the incremental search over the loaded history and the grid.
//!
//! The search never runs while drawing. [`Search::step`] scans a bounded
//! number of lines per call (the shell calls it off the UI thread); drawing
//! only looks the visible rows up in the matches found so far.
//!
//! Lines are numbered in one sequence: history lines by their absolute
//! number, grid row `r` as `end_line + r`. A grid row that scrolls into
//! history keeps its number, so matches found on the grid stay on their
//! text while it scrolls.

use std::collections::VecDeque;

use fleet_term::{Cell, Terminal};

use super::scrollback::Scrollback;

/// What to look for: the query's characters, lower-cased unless the
/// search is case sensitive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Needle {
    chars: Vec<char>,
    case_sensitive: bool,
}

impl Needle {
    /// `None` for an empty query.
    pub fn new(text: &str, case_sensitive: bool) -> Option<Needle> {
        if text.is_empty() {
            return None;
        }
        let chars = text
            .chars()
            .map(|c| if case_sensitive { c } else { fold(c) })
            .collect();
        Some(Needle {
            chars,
            case_sensitive,
        })
    }

    fn matches(&self, a: char, b: char) -> bool {
        if self.case_sensitive {
            a == b
        } else {
            fold(a) == b
        }
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

/// A match: columns `start..end` of line `line` (in the search's
/// numbering).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Match {
    pub line: u64,
    pub start: u16,
    pub end: u16,
}

/// What the find bar shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FindStatus {
    /// The current match's place, from 1 at the top.
    pub index: Option<usize>,
    /// Matches found so far.
    pub count: usize,
    /// Lines are left to scan.
    pub searching: bool,
}

/// The find bar's count: "3 of 17", "17 matches", "No matches"; a `+`
/// while lines are left to scan. Empty for an empty query.
pub fn status_label(status: FindStatus, has_query: bool) -> String {
    if !has_query {
        return String::new();
    }
    let more = if status.searching { "+" } else { "" };
    match (status.index, status.count) {
        (_, 0) if status.searching => "Searching…".into(),
        (_, 0) => "No matches".into(),
        (Some(i), n) => format!("{i} of {n}{more}"),
        (None, 1) if !status.searching => "1 match".into(),
        (None, n) => format!("{n}{more} matches"),
    }
}

/// What a step did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// Lines are left to scan: call again.
    pub more: bool,
    /// Matches or the current match changed: redraw.
    pub changed: bool,
}

/// An incremental search for one needle over a pane's loaded history and
/// its grid.
#[derive(Debug)]
pub struct Search {
    needle: Option<Needle>,
    generation: u64,
    /// Matches on history lines `lo..hi`, in order.
    hist: VecDeque<Match>,
    lo: u64,
    hi: u64,
    /// Matches on the grid as last scanned, in order.
    grid: Vec<Match>,
    grid_dirty: bool,
    /// The match the find bar is on.
    current: Option<Match>,
    /// The current match is to be picked once the search has found it.
    pick: bool,
    cells: Vec<Cell>,
    found: Vec<(u16, u16)>,
}

impl Search {
    /// A search for `needle`, starting at the newest line.
    pub fn new(needle: Option<Needle>, sb: &Scrollback) -> Search {
        let end = sb.end_line();
        Search {
            needle,
            generation: sb.generation(),
            hist: VecDeque::new(),
            lo: end,
            hi: end,
            grid: Vec::new(),
            grid_dirty: true,
            current: None,
            pick: true,
            cells: Vec::new(),
            found: Vec::new(),
        }
    }

    pub fn needle(&self) -> Option<&Needle> {
        self.needle.as_ref()
    }

    /// Forgets every match and scans again from the newest line (the
    /// history was rebuilt or cleared).
    pub fn restart(&mut self, sb: &Scrollback) {
        let needle = self.needle.take();
        *self = Search::new(needle, sb);
    }

    /// The grid changed: scan it again on the next step.
    pub fn grid_changed(&mut self) {
        self.grid_dirty = true;
    }

    /// The search has reached the oldest loaded line: only an older page
    /// gives it more.
    pub fn at_oldest(&self, sb: &Scrollback) -> bool {
        self.needle.is_some() && self.lo <= sb.first_line()
    }

    /// Nothing is left to scan.
    pub fn done(&self, sb: &Scrollback) -> bool {
        self.needle.is_none()
            || (!self.grid_dirty && self.hi >= sb.end_line() && self.lo <= sb.first_line())
    }

    /// Scans up to `budget` lines: the grid if it changed, the history
    /// lines added since the last step, then older ones. `bottom` is the
    /// view's last line, from where the current match is first picked.
    pub fn step(&mut self, sb: &Scrollback, term: &Terminal, budget: usize, bottom: u64) -> Step {
        let mut changed = false;
        if self.needle.is_none() {
            return Step::default();
        }
        if sb.generation() != self.generation {
            self.restart(sb);
            changed = true;
        }
        // Lines dropped off the front past the cap.
        let first = sb.first_line();
        while self.hist.front().is_some_and(|m| m.line < first) {
            self.hist.pop_front();
            changed = true;
        }
        self.lo = self.lo.max(first);
        self.hi = self.hi.max(self.lo);
        if self.current.is_some_and(|c| c.line < first) {
            self.current = None;
        }

        let mut left = budget;
        if self.grid_dirty {
            changed |= self.scan_grid(sb, term);
            left = left.saturating_sub(usize::from(term.size().1));
        }
        let end = sb.end_line();
        while left > 0 && self.hi < end {
            let line = self.hi;
            self.scan_history(sb, line);
            for &(start, end) in &self.found {
                self.hist.push_back(Match { line, start, end });
                changed = true;
            }
            self.hi += 1;
            left -= 1;
        }
        while left > 0 && self.lo > first {
            let line = self.lo - 1;
            self.scan_history(sb, line);
            for &(start, end) in self.found.iter().rev() {
                self.hist.push_front(Match { line, start, end });
                changed = true;
            }
            self.lo -= 1;
            left -= 1;
        }
        if self.pick {
            changed |= self.try_pick(sb, bottom);
        }
        Step {
            more: !self.done(sb),
            changed,
        }
    }

    fn scan_history(&mut self, sb: &Scrollback, line: u64) {
        self.found.clear();
        if let Some(raw) = sb.line(line) {
            fleet_term::parse_history_line(raw, &mut self.cells);
            if let Some(n) = &self.needle {
                row_matches(&self.cells, n, &mut self.found);
            }
        }
    }

    /// Scans every grid row again; `true` if the matches changed.
    fn scan_grid(&mut self, sb: &Scrollback, term: &Terminal) -> bool {
        self.grid_dirty = false;
        let Some(needle) = &self.needle else {
            return false;
        };
        let end = sb.end_line();
        let mut grid = Vec::new();
        for row in 0..term.size().1 {
            term.row_cells(row, &mut self.cells);
            self.found.clear();
            row_matches(&self.cells, needle, &mut self.found);
            let line = end + u64::from(row);
            grid.extend(
                self.found
                    .iter()
                    .map(|&(start, end)| Match { line, start, end }),
            );
        }
        let changed = grid != self.grid;
        self.grid = grid;
        changed
    }

    /// Grid matches not already covered by scanned history (a stale grid
    /// scan can number rows that have since scrolled into history).
    fn grid_part(&self) -> &[Match] {
        let from = self.grid.partition_point(|m| m.line < self.hi);
        &self.grid[from..]
    }

    /// Every match found so far, top to bottom.
    pub fn matches(&self) -> impl Iterator<Item = &Match> {
        self.hist.iter().chain(self.grid_part())
    }

    pub fn count(&self) -> usize {
        self.hist.len() + self.grid_part().len()
    }

    /// The current match, if the search is on one.
    pub fn current(&self) -> Option<Match> {
        self.current
    }

    /// The current match's place among the matches, from 0 at the top.
    pub fn current_index(&self) -> Option<usize> {
        let c = self.current?;
        let in_hist = self.hist.partition_point(|m| *m < c);
        if self.hist.get(in_hist) == Some(&c) {
            return Some(in_hist);
        }
        let grid = self.grid_part();
        let in_grid = grid.partition_point(|m| *m < c);
        (grid.get(in_grid) == Some(&c)).then_some(self.hist.len() + in_grid)
    }

    /// The match at `i` (top to bottom).
    fn nth(&self, i: usize) -> Option<Match> {
        match self.hist.get(i) {
            Some(m) => Some(*m),
            None => self.grid_part().get(i - self.hist.len()).copied(),
        }
    }

    /// Columns of matches on `line`, and whether each is the current one.
    pub fn on_line(&self, line: u64, out: &mut Vec<(u16, u16, bool)>) {
        out.clear();
        let part: Box<dyn Iterator<Item = &Match>> = if line < self.hi {
            let from = self.hist.partition_point(|m| m.line < line);
            Box::new(self.hist.range(from..))
        } else {
            let grid = self.grid_part();
            let from = grid.partition_point(|m| m.line < line);
            Box::new(grid[from..].iter())
        };
        out.extend(
            part.take_while(|m| m.line == line)
                .map(|m| (m.start, m.end, Some(*m) == self.current)),
        );
    }

    /// Picks the first current match: the newest at or above `bottom` once
    /// everything between it and `bottom` is scanned, else (when the search
    /// is done and none is above) the first below.
    fn try_pick(&mut self, sb: &Scrollback, bottom: u64) -> bool {
        let above = self
            .matches()
            .take_while(|m| m.line <= bottom)
            .last()
            .copied();
        let pick = match above {
            Some(m) if m.line >= self.lo => Some(m),
            _ if self.done(sb) => self.matches().next().copied(),
            _ => return false,
        };
        self.pick = false;
        self.current = pick;
        pick.is_some()
    }

    /// Moves to the next match up (`up`) or down, wrapping around. With no
    /// current match, the newest at or above `bottom` going up, the first at
    /// or below `top` going down.
    pub fn advance(&mut self, up: bool, top: u64, bottom: u64) -> Option<Match> {
        let count = self.count();
        if count == 0 {
            return None;
        }
        let next = match (self.current_index(), self.current) {
            (Some(i), _) if up => self.nth((i + count - 1) % count),
            (Some(i), _) => self.nth((i + 1) % count),
            // The current match went away: step from where it was.
            (None, Some(c)) if up => self
                .matches()
                .take_while(|m| **m < c)
                .last()
                .or_else(|| self.matches().last())
                .copied(),
            (None, Some(c)) => self
                .matches()
                .find(|m| **m > c)
                .or_else(|| self.matches().next())
                .copied(),
            (None, None) if up => self
                .matches()
                .take_while(|m| m.line <= bottom)
                .last()
                .or_else(|| self.matches().last())
                .copied(),
            (None, None) => self
                .matches()
                .find(|m| m.line >= top)
                .or_else(|| self.matches().next())
                .copied(),
        };
        self.pick = false;
        self.current = next;
        next
    }
}

#[cfg(test)]
mod tests {
    use super::{FindStatus, Needle, row_matches, status_label};
    use fleet_term::Cell;

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

    #[test]
    fn status_label_counts_matches() {
        let s = |index, count, searching| FindStatus {
            index,
            count,
            searching,
        };
        assert_eq!(status_label(s(Some(3), 17, false), true), "3 of 17");
        assert_eq!(status_label(s(Some(3), 17, true), true), "3 of 17+");
        assert_eq!(status_label(s(None, 17, false), true), "17 matches");
        assert_eq!(status_label(s(None, 1, false), true), "1 match");
        assert_eq!(status_label(s(None, 1, true), true), "1+ matches");
        assert_eq!(status_label(s(None, 0, false), true), "No matches");
        assert_eq!(status_label(s(None, 0, true), true), "Searching…");
        assert_eq!(status_label(s(None, 0, false), false), "");
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
}
