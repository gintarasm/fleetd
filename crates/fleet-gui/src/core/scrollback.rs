//! Client-side history of one pane: raw ANSI lines with absolute numbers.
//!
//! Lines are numbered as the node numbers them (decision 0021): the attach
//! tail starts at `AttachOk.tail_first_line`, lines that scroll off later
//! continue from there, and older pages are prepended. A clear (`CSI 3 J`)
//! bumps the generation and drops every loaded line, keeping the numbering.
//! Lines stay raw ANSI; they are parsed only when drawn.

use std::collections::VecDeque;

/// Loaded history lines `first_line..end_line` of one generation.
#[derive(Debug, Default)]
pub struct Scrollback {
    lines: VecDeque<Vec<u8>>,
    first_line: u64,
    generation: u64,
    /// Oldest line the node still has in this generation; nothing older can
    /// be paged in.
    oldest_on_node: u64,
}

impl Scrollback {
    /// Empty history whose next line is `first_line`; the node holds lines
    /// from `oldest_on_node` on in `generation`.
    pub fn new(generation: u64, first_line: u64, oldest_on_node: u64) -> Self {
        Self {
            lines: VecDeque::new(),
            first_line,
            generation,
            oldest_on_node: oldest_on_node.min(first_line),
        }
    }

    /// Number of the oldest loaded line (`end_line` when none are loaded).
    pub fn first_line(&self) -> u64 {
        self.first_line
    }

    /// One past the newest line.
    pub fn end_line(&self) -> u64 {
        self.first_line + self.lines.len() as u64
    }

    /// Number of loaded lines.
    pub fn len(&self) -> u64 {
        self.lines.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// History generation: the node's at attach plus the clears seen since.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Raw ANSI bytes of line `abs`, if loaded.
    pub fn line(&self, abs: u64) -> Option<&[u8]> {
        let i = usize::try_from(abs.checked_sub(self.first_line)?).ok()?;
        self.lines.get(i).map(Vec::as_slice)
    }

    /// Appends the newest line (numbered `end_line`).
    pub fn push_back(&mut self, line: Vec<u8>) {
        self.lines.push_back(line);
    }

    /// Prepends a page of older lines, `lines[0]` being line `first_line`.
    ///
    /// Returns false, dropping the page, when `generation` is not the current
    /// one or the page ends before `first_line()` (a gap). Lines the page
    /// shares with the loaded ones are trimmed, never duplicated.
    pub fn prepend_page(
        &mut self,
        generation: u64,
        first_line: u64,
        mut lines: Vec<Vec<u8>>,
    ) -> bool {
        if generation != self.generation || first_line + (lines.len() as u64) < self.first_line {
            return false;
        }
        let new = usize::try_from(self.first_line.saturating_sub(first_line)).unwrap_or(usize::MAX);
        lines.truncate(new);
        for line in lines.into_iter().rev() {
            self.lines.push_front(line);
        }
        self.first_line = self.first_line.min(first_line);
        self.oldest_on_node = self.oldest_on_node.min(self.first_line);
        true
    }

    /// Erases the loaded history (`CSI 3 J`): the generation is bumped and
    /// `first_line` becomes `end_line`; nothing older exists in the new
    /// generation.
    pub fn clear(&mut self) {
        let end = self.end_line();
        self.lines.clear();
        self.first_line = end;
        self.oldest_on_node = end;
        self.generation += 1;
    }

    /// Oldest line the node holds in this generation.
    pub fn oldest_on_node(&self) -> u64 {
        self.oldest_on_node
    }

    /// Whether every line the node holds in this generation is loaded.
    pub fn fully_loaded(&self) -> bool {
        self.first_line <= self.oldest_on_node
    }

    /// Treats the loaded lines as all there is (the node has no more).
    pub fn mark_fully_loaded(&mut self) {
        self.oldest_on_node = self.first_line;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(names: &[&str]) -> Vec<Vec<u8>> {
        names.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    fn texts(sb: &Scrollback) -> Vec<String> {
        (sb.first_line()..sb.end_line())
            .map(|n| String::from_utf8(sb.line(n).unwrap().to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn prepend_page_contiguous_and_overlapping() {
        let mut sb = Scrollback::new(0, 10, 0);
        sb.push_back(b"j".to_vec());
        assert!(sb.prepend_page(0, 8, lines(&["h", "i"])));
        assert_eq!(sb.first_line(), 8);
        // Lines 5..10 overlap 8 and 9: only 5..8 are added.
        assert!(sb.prepend_page(0, 5, lines(&["e", "f", "g", "x", "y"])));
        assert_eq!(texts(&sb), ["e", "f", "g", "h", "i", "j"]);
        // A gap (lines 1..3, next would be 3, not 5) is dropped.
        assert!(!sb.prepend_page(0, 1, lines(&["b", "c"])));
        assert_eq!(sb.first_line(), 5);
        // A stale generation is dropped.
        assert!(!sb.prepend_page(1, 3, lines(&["c", "d"])));
        assert_eq!(sb.first_line(), 5);
    }

    #[test]
    fn clear_bumps_generation_and_keeps_numbering() {
        let mut sb = Scrollback::new(4, 10, 2);
        sb.push_back(b"a".to_vec());
        sb.push_back(b"b".to_vec());
        assert!(!sb.fully_loaded());
        sb.clear();
        assert_eq!(sb.generation(), 5);
        assert_eq!((sb.first_line(), sb.end_line()), (12, 12));
        assert!(sb.fully_loaded());
        assert_eq!(sb.line(11), None);
        sb.push_back(b"c".to_vec());
        assert_eq!(sb.line(12), Some(&b"c"[..]));
    }

    #[test]
    fn fully_loaded_when_oldest_on_node_reached() {
        let mut sb = Scrollback::new(0, 3, 1);
        assert_eq!(sb.oldest_on_node(), 1);
        assert!(!sb.fully_loaded());
        assert!(sb.prepend_page(0, 1, lines(&["b", "c"])));
        assert!(sb.fully_loaded());
    }
}
