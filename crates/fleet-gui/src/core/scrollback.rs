//! Client-side history of one pane: raw ANSI lines with absolute numbers.
//!
//! Lines are numbered as the node numbers them (decision 0021): the attach
//! tail starts at `AttachOk.tail_first_line`, lines that scroll off later
//! continue from there, and older pages are prepended. A clear (`CSI 3 J`)
//! bumps the generation and drops every loaded line, keeping the numbering.
//! Lines stay raw ANSI; they are parsed only when drawn.
//!
//! The user can also clear the loaded history from the view only (⌘K,
//! [`Scrollback::clear_view`], decision 0082): the node keeps it, the
//! generation stays, and nothing older than the clear is paged in again
//! until the history is rebuilt by a re-attach.
//!
//! The loaded lines are capped ([`Cap`]): past it the oldest are dropped
//! from the front. The node keeps them, so the history is not "fully
//! loaded", but nothing older is paged in until the history is rebuilt (a
//! re-attach or a clear).

use std::collections::VecDeque;

/// Memory counted per line besides its bytes.
pub const LINE_OVERHEAD: usize = std::mem::size_of::<Vec<u8>>();

/// How much history a pane keeps loaded, whichever limit comes first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cap {
    pub lines: usize,
    /// Line bytes plus [`LINE_OVERHEAD`] each.
    pub bytes: usize,
}

impl Default for Cap {
    /// 100,000 lines or 64 MiB (the node's retention per agent).
    fn default() -> Self {
        Cap {
            lines: 100_000,
            bytes: 64 << 20,
        }
    }
}

/// Loaded history lines `first_line..end_line` of one generation.
#[derive(Debug, Default)]
pub struct Scrollback {
    lines: VecDeque<Vec<u8>>,
    first_line: u64,
    generation: u64,
    /// Oldest line the node still has in this generation; nothing older can
    /// be paged in.
    oldest_on_node: u64,
    cap: Cap,
    /// [`Cap`] counted bytes of the loaded lines.
    bytes: usize,
    /// The cap was reached: no older page is loaded.
    full: bool,
    /// Lines older than this were cleared from the view
    /// ([`Scrollback::clear_view`]); a page never brings them back.
    floor: u64,
}

impl Scrollback {
    /// Empty history whose next line is `first_line`; the node holds lines
    /// from `oldest_on_node` on in `generation`.
    pub fn new(generation: u64, first_line: u64, oldest_on_node: u64) -> Self {
        Self::with_cap(generation, first_line, oldest_on_node, Cap::default())
    }

    /// [`Scrollback::new`] keeping at most `cap`.
    pub fn with_cap(generation: u64, first_line: u64, oldest_on_node: u64, cap: Cap) -> Self {
        Self {
            lines: VecDeque::new(),
            first_line,
            generation,
            oldest_on_node: oldest_on_node.min(first_line),
            cap,
            bytes: 0,
            full: false,
            floor: 0,
        }
    }

    /// The cap was reached: older pages are not loaded (they would only
    /// push out newer lines).
    pub fn is_full(&self) -> bool {
        self.full
    }

    fn over_cap(&self, extra_lines: usize, extra_bytes: usize) -> bool {
        self.lines.len() + extra_lines > self.cap.lines
            || self.bytes.saturating_add(extra_bytes) > self.cap.bytes
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

    /// Appends the newest line (numbered `end_line`), dropping the oldest
    /// past the cap. Returns how many were dropped.
    pub fn push_back(&mut self, line: Vec<u8>) -> usize {
        self.bytes += line.len() + LINE_OVERHEAD;
        self.lines.push_back(line);
        let mut dropped = 0;
        while self.lines.len() > 1 && self.over_cap(0, 0) {
            if let Some(old) = self.lines.pop_front() {
                self.bytes -= old.len() + LINE_OVERHEAD;
                self.first_line += 1;
                dropped += 1;
            }
        }
        if dropped > 0 {
            self.full = true;
        }
        dropped
    }

    /// Prepends a page of older lines, `lines[0]` being line `first_line`.
    ///
    /// Returns false, dropping the page, when `generation` is not the current
    /// one or the page ends before `first_line()` (a gap). Lines the page
    /// shares with the loaded ones are trimmed, never duplicated. Older
    /// lines past the cap are left out (the history is then full).
    pub fn prepend_page(
        &mut self,
        generation: u64,
        mut first_line: u64,
        mut lines: Vec<Vec<u8>>,
    ) -> bool {
        if generation != self.generation || first_line + (lines.len() as u64) < self.first_line {
            return false;
        }
        if first_line < self.floor {
            // Lines cleared from the view stay cleared.
            let skip = usize::try_from(self.floor - first_line).unwrap_or(usize::MAX);
            lines.drain(..skip.min(lines.len()));
            first_line = self.floor;
        }
        let new = usize::try_from(self.first_line.saturating_sub(first_line)).unwrap_or(usize::MAX);
        lines.truncate(new);
        for line in lines.into_iter().rev() {
            if self.over_cap(1, line.len() + LINE_OVERHEAD) {
                self.full = true;
                break;
            }
            self.bytes += line.len() + LINE_OVERHEAD;
            self.lines.push_front(line);
            self.first_line -= 1;
        }
        self.oldest_on_node = self.oldest_on_node.min(self.first_line);
        true
    }

    /// Erases the loaded history (`CSI 3 J`): the generation is bumped and
    /// `first_line` becomes `end_line`; nothing older exists in the new
    /// generation.
    pub fn clear(&mut self) {
        let end = self.end_line();
        self.lines.clear();
        self.bytes = 0;
        self.full = false;
        self.first_line = end;
        self.oldest_on_node = end;
        self.generation += 1;
    }

    /// Clears the loaded history from this view only (⌘K): unlike
    /// [`Scrollback::clear`] the node still has it, so the generation is
    /// kept (page requests stay valid) and the history counts as fully
    /// loaded, with nothing older than `end_line` paged in again.
    pub fn clear_view(&mut self) {
        let end = self.end_line();
        self.lines.clear();
        self.bytes = 0;
        self.full = false;
        self.first_line = end;
        self.oldest_on_node = end;
        self.floor = end;
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
    fn push_over_the_line_cap_drops_the_oldest_lines() {
        let mut sb = Scrollback::with_cap(
            0,
            10,
            0,
            Cap {
                lines: 3,
                bytes: usize::MAX,
            },
        );
        for name in ["a", "b", "c", "d", "e"] {
            sb.push_back(name.as_bytes().to_vec());
        }
        assert_eq!(texts(&sb), ["c", "d", "e"]);
        assert_eq!((sb.first_line(), sb.end_line()), (12, 15));
        // The node still has lines 0..12: not fully loaded, and nothing is
        // paged while the cap is reached.
        assert_eq!(sb.oldest_on_node(), 0);
        assert!(!sb.fully_loaded());
        assert!(sb.is_full());
        // A clear empties it: room again.
        sb.clear();
        assert!(!sb.is_full());
    }

    #[test]
    fn push_over_the_byte_cap_drops_the_oldest_lines() {
        let per_line = LINE_OVERHEAD + 4;
        let mut sb = Scrollback::with_cap(
            0,
            0,
            0,
            Cap {
                lines: usize::MAX,
                bytes: 2 * per_line,
            },
        );
        for name in ["aaaa", "bbbb", "cccc"] {
            sb.push_back(name.as_bytes().to_vec());
        }
        assert_eq!(texts(&sb), ["bbbb", "cccc"]);
        assert_eq!(sb.first_line(), 1);
    }

    #[test]
    fn prepend_keeps_only_the_newest_lines_that_fit_the_cap() {
        let mut sb = Scrollback::with_cap(
            0,
            10,
            0,
            Cap {
                lines: 4,
                bytes: usize::MAX,
            },
        );
        sb.push_back(b"k".to_vec());
        sb.push_back(b"l".to_vec());
        assert!(!sb.is_full());
        assert!(sb.prepend_page(0, 6, lines(&["g", "h", "i", "j"])));
        assert_eq!(texts(&sb), ["i", "j", "k", "l"]);
        assert_eq!(sb.first_line(), 8);
        // Lines 0..8 stay on the node, but there is no room for them.
        assert!(!sb.fully_loaded());
        assert!(sb.is_full());
    }

    #[test]
    fn clear_view_keeps_the_generation_and_pages_nothing_older_back() {
        let mut sb = Scrollback::new(3, 10, 0);
        sb.push_back(b"a".to_vec());
        sb.push_back(b"b".to_vec());
        assert!(!sb.fully_loaded());
        sb.clear_view();
        assert_eq!(sb.generation(), 3);
        assert_eq!((sb.first_line(), sb.end_line()), (12, 12));
        assert!(sb.is_empty());
        assert!(sb.fully_loaded());
        // A page that was in flight (lines 2..12) adds nothing.
        assert!(!sb.prepend_page(3, 2, lines(&["x"; 9])));
        assert!(sb.prepend_page(3, 2, lines(&["x"; 10])));
        assert!(sb.is_empty());
        // New lines still arrive, numbered on.
        sb.push_back(b"c".to_vec());
        assert_eq!(texts(&sb), ["c"]);
        assert_eq!(sb.first_line(), 12);
        // A page straddling the floor keeps only what is at or after it.
        let mut sb = Scrollback::new(0, 5, 0);
        sb.clear_view();
        sb.push_back(b"f".to_vec());
        assert!(sb.prepend_page(0, 3, lines(&["d", "e", "x"])));
        assert_eq!(texts(&sb), ["f"]);
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
