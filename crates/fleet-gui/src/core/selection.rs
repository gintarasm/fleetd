//! Text selection over a pane's history lines and grid rows.
//!
//! Points are cell columns on a line; history lines (by absolute number) come
//! before the grid rows. A selection is inclusive of both ends. It has an
//! anchor range (the cell, word or line first picked) and a head range (the
//! one under the pointer now); the selection spans both.

use fleet_term::Cell;

/// A line a pane shows: a history line by absolute number or a grid row.
/// Ordered top to bottom: every history line before every grid row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LineRef {
    History(u64),
    Grid(u16),
}

/// One cell of a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SelPoint {
    pub line: LineRef,
    pub col: u16,
}

/// What a selection snaps to while it is extended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelUnit {
    Cell,
    Word,
    Line,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    unit: SelUnit,
    anchor: (SelPoint, SelPoint),
    head: (SelPoint, SelPoint),
}

/// Column standing for "to the end of the line".
pub const LINE_END: u16 = u16::MAX;

impl Selection {
    /// A cell selection anchored at `p`; empty until extended elsewhere.
    pub fn cells(p: SelPoint) -> Self {
        Self {
            unit: SelUnit::Cell,
            anchor: (p, p),
            head: (p, p),
        }
    }

    /// A selection of `start..=end` that snaps to `unit` when extended.
    pub fn range(unit: SelUnit, start: SelPoint, end: SelPoint) -> Self {
        Self {
            unit,
            anchor: (start, end),
            head: (start, end),
        }
    }

    pub fn unit(&self) -> SelUnit {
        self.unit
    }

    /// Moves the head to cell `p`.
    pub fn extend_cells(&mut self, p: SelPoint) {
        self.head = (p, p);
    }

    /// Moves the head to the range `start..=end` (a word or a line).
    pub fn extend_range(&mut self, start: SelPoint, end: SelPoint) {
        self.head = (start, end);
    }

    /// First and last selected cell, or `None` when nothing is selected.
    pub fn bounds(&self) -> Option<(SelPoint, SelPoint)> {
        if self.unit == SelUnit::Cell && self.anchor == self.head {
            return None;
        }
        Some((
            self.anchor.0.min(self.head.0),
            self.anchor.1.max(self.head.1),
        ))
    }

    /// Whether cell `col` of `line` is selected.
    pub fn contains(&self, line: LineRef, col: u16) -> bool {
        let p = SelPoint { line, col };
        self.bounds()
            .is_some_and(|(start, end)| start <= p && p <= end)
    }

    /// Whether any selected cell is on a grid row.
    pub fn touches_grid(&self) -> bool {
        self.bounds()
            .is_some_and(|(_, end)| matches!(end.line, LineRef::Grid(_)))
    }

    /// Whether any selected cell is on a history line older than `abs`.
    pub fn touches_history_before(&self, abs: u64) -> bool {
        self.bounds()
            .is_some_and(|(start, _)| matches!(start.line, LineRef::History(n) if n < abs))
    }
}

/// Start column of each cell of `cells` (wide cells take two columns).
fn columns(cells: &[Cell]) -> impl Iterator<Item = (u16, &Cell)> {
    let mut col = 0u16;
    cells.iter().map(move |c| {
        let at = col;
        col = col.saturating_add(if c.wide { 2 } else { 1 });
        (at, c)
    })
}

#[derive(PartialEq, Eq)]
enum Class {
    Space,
    Word,
    /// Brackets, quotes and separators: each is a word of its own.
    Delimiter,
}

fn class(ch: char) -> Class {
    if ch.is_whitespace() {
        Class::Space
    } else if "()[]{}<>\"'`,;|".contains(ch) {
        Class::Delimiter
    } else {
        Class::Word
    }
}

/// First and last column of the word around column `col` of `cells`: the run
/// of cells of the same class (word characters or blanks); a delimiter or a
/// column past the end is a word of its own.
pub fn word_bounds(cells: &[Cell], col: u16) -> (u16, u16) {
    let cols: Vec<(u16, &Cell)> = columns(cells).collect();
    let Some(i) = cols
        .iter()
        .rposition(|&(at, _)| at <= col)
        .filter(|&i| col < cols[i].0 + if cols[i].1.wide { 2 } else { 1 })
    else {
        return (col, col);
    };
    let width = |i: usize| if cols[i].1.wide { 2 } else { 1 };
    let k = class(cols[i].1.ch);
    if k == Class::Delimiter {
        return (cols[i].0, cols[i].0 + width(i) - 1);
    }
    let mut first = i;
    while first > 0 && class(cols[first - 1].1.ch) == k {
        first -= 1;
    }
    let mut last = i;
    while last + 1 < cols.len() && class(cols[last + 1].1.ch) == k {
        last += 1;
    }
    (cols[first].0, cols[last].0 + width(last) - 1)
}

/// Text of the cells of `cells` starting in columns `from..=to`, trailing
/// spaces trimmed.
pub fn row_text(cells: &[Cell], from: u16, to: u16) -> String {
    let mut s = String::new();
    for (at, c) in columns(cells) {
        if at > to {
            break;
        }
        if at >= from {
            s.push(c.ch);
            if let Some(extra) = &c.extra {
                s.push_str(extra);
            }
        }
    }
    s.truncate(s.trim_end_matches(' ').len());
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: LineRef, col: u16) -> SelPoint {
        SelPoint { line, col }
    }

    #[test]
    fn history_orders_before_grid() {
        assert!(LineRef::History(u64::MAX) < LineRef::Grid(0));
        assert!(p(LineRef::Grid(0), 5) < p(LineRef::Grid(1), 0));
    }

    #[test]
    fn cell_selection_is_inclusive_and_empty_until_extended() {
        let mut s = Selection::cells(p(LineRef::Grid(1), 4));
        assert!(!s.contains(LineRef::Grid(1), 4));
        s.extend_cells(p(LineRef::Grid(0), 2));
        assert!(s.contains(LineRef::Grid(0), 2));
        assert!(s.contains(LineRef::Grid(0), 70));
        assert!(s.contains(LineRef::Grid(1), 4));
        assert!(!s.contains(LineRef::Grid(1), 5));
        assert!(!s.contains(LineRef::Grid(0), 1));
    }

    #[test]
    fn row_text_by_columns_trims_trailing_spaces() {
        let mut row = Vec::new();
        fleet_term::parse_history_line("ab 日x   ".as_bytes(), &mut row);
        assert_eq!(row_text(&row, 1, LINE_END), "b 日x");
        assert_eq!(row_text(&row, 0, 3), "ab 日");
        assert_eq!(row_text(&row, 4, 4), "");
    }

    #[test]
    fn word_bounds_by_class() {
        let mut row = Vec::new();
        fleet_term::parse_history_line("ls  foo/bar (x)".as_bytes(), &mut row);
        assert_eq!(word_bounds(&row, 5), (4, 10));
        assert_eq!(word_bounds(&row, 2), (2, 3));
        assert_eq!(word_bounds(&row, 12), (12, 12));
        assert_eq!(word_bounds(&row, 13), (13, 13));
        assert_eq!(word_bounds(&row, 40), (40, 40));
        // A wide cell spans two columns.
        fleet_term::parse_history_line("a 日本 b".as_bytes(), &mut row);
        assert_eq!(word_bounds(&row, 3), (2, 5));
        assert_eq!(word_bounds(&row, 7), (7, 7));
    }
}
