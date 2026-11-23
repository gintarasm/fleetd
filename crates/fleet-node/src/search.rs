//! Searching every agent's output (ADR 0098): one pass over each agent's
//! screen and stored history, newest first, that stops at a hit cap or a
//! byte budget. [`Node::search`](crate::node::Node::search) feeds it each
//! agent's screen and pages.

use std::io;

use fleet_term::{Needle, history_line_text};

use crate::history::{HISTORY_PAGE_MAX_LINES, Page};

/// Hits per request when the request names none, and the cap.
pub const SEARCH_LIMIT_DEFAULT: u32 = 200;
pub const SEARCH_LIMIT_MAX: u32 = 1000;
/// Hits per agent when the request names none.
pub const SEARCH_PER_AGENT_DEFAULT: u32 = 20;
/// History and screen bytes one request reads at most (four agents' full
/// histories); past it the reply is truncated.
pub const SEARCH_BUDGET_BYTES: u64 = 256 * 1024 * 1024;
/// A hit's text is cut to this many bytes (at a character boundary).
pub const SEARCH_TEXT_MAX_BYTES: usize = 1024;

/// One matching line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub agent_id: u64,
    /// History line number; a screen row `r` is `next_line + r`, as the
    /// GUI's find numbers it.
    pub line: u64,
    pub text: String,
}

/// What a search found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hits {
    /// By agent in the order scanned, each agent's newest line first.
    pub hits: Vec<Hit>,
    /// The hit cap or the byte budget stopped the search before it had
    /// read everything.
    pub truncated: bool,
}

/// A search in progress over several agents.
pub struct Scan {
    needle: Needle,
    limit: usize,
    per_agent: usize,
    budget: u64,
    out: Hits,
    text: String,
}

impl Scan {
    /// `limit` and `per_agent` of 0 take the defaults; both are capped.
    pub fn new(needle: Needle, limit: u32, per_agent: u32) -> Scan {
        Scan::with_budget(needle, limit, per_agent, SEARCH_BUDGET_BYTES)
    }

    pub fn with_budget(needle: Needle, limit: u32, per_agent: u32, budget: u64) -> Scan {
        let limit = match limit {
            0 => SEARCH_LIMIT_DEFAULT,
            n => n.min(SEARCH_LIMIT_MAX),
        };
        let per_agent = match per_agent {
            0 => SEARCH_PER_AGENT_DEFAULT,
            n => n,
        }
        .min(limit);
        Scan {
            needle,
            limit: limit as usize,
            per_agent: per_agent as usize,
            budget,
            out: Hits::default(),
            text: String::new(),
        }
    }

    /// No more agents need scanning.
    pub fn done(&self) -> bool {
        self.out.truncated
    }

    /// Scans one agent: its screen rows bottom up (`screen[r]` is line
    /// `next_line + r`), then its history from `next_line` down to
    /// `first_line`, a page at a time from `read(before_line, max_lines)`.
    /// A page that fails ends this agent's scan (its history went, or was
    /// cleared meanwhile).
    pub fn agent(
        &mut self,
        agent_id: u64,
        screen: &[String],
        history: Option<(u64, u64)>,
        mut read: impl FnMut(u64, u32) -> io::Result<Page>,
    ) {
        if self.done() {
            return;
        }
        let mut found = 0;
        let next_line = history.map_or(0, |(_, next)| next);
        for (r, row) in screen.iter().enumerate().rev() {
            if !self.charge(row.len()) {
                return;
            }
            let row = row.trim_end_matches(' ');
            if self.hit(agent_id, next_line + r as u64, row, &mut found) {
                return;
            }
        }
        let Some((first_line, mut before)) = history else {
            return;
        };
        while before > first_line {
            let Ok(page) = read(before, HISTORY_PAGE_MAX_LINES) else {
                return;
            };
            if page.lines.is_empty() || page.first_line >= before {
                return;
            }
            for (i, raw) in page.lines.iter().enumerate().rev() {
                if !self.charge(raw.len()) {
                    return;
                }
                let mut text = std::mem::take(&mut self.text);
                history_line_text(raw, &mut text);
                let stop = self.hit(agent_id, page.first_line + i as u64, &text, &mut found);
                self.text = text;
                if stop {
                    return;
                }
            }
            before = page.first_line;
        }
    }

    /// Takes `bytes` from the budget; `false` (and truncated) once spent.
    fn charge(&mut self, bytes: usize) -> bool {
        let bytes = bytes as u64 + 1;
        if self.budget < bytes {
            self.budget = 0;
            self.out.truncated = true;
            return false;
        }
        self.budget -= bytes;
        true
    }

    /// Records `text` if it matches; `true` once this agent (or the whole
    /// search) has all the hits it may have.
    fn hit(&mut self, agent_id: u64, line: u64, text: &str, found: &mut usize) -> bool {
        if self.needle.find_in(text).is_none() {
            return false;
        }
        if self.out.hits.len() >= self.limit {
            self.out.truncated = true;
            return true;
        }
        if *found >= self.per_agent {
            return true;
        }
        *found += 1;
        self.out.hits.push(Hit {
            agent_id,
            line,
            text: cut(text, SEARCH_TEXT_MAX_BYTES).to_owned(),
        });
        false
    }

    pub fn finish(self) -> Hits {
        self.out
    }
}

/// `text` cut to at most `max` bytes, at a character boundary.
fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    /// History lines `first..first + lines.len()`, served in pages of at
    /// most `page` lines; counts the reads.
    struct Fake {
        first: u64,
        lines: Vec<Bytes>,
        page: u32,
        reads: usize,
    }

    impl Fake {
        fn new(first: u64, lines: &[&str], page: u32) -> Fake {
            Fake {
                first,
                lines: lines
                    .iter()
                    .map(|l| Bytes::from(l.as_bytes().to_vec()))
                    .collect(),
                page,
                reads: 0,
            }
        }

        fn range(&self) -> Option<(u64, u64)> {
            Some((self.first, self.first + self.lines.len() as u64))
        }

        fn read(&mut self, before: u64, max: u32) -> io::Result<Page> {
            self.reads += 1;
            let end = (before - self.first) as usize;
            let n = max.min(self.page) as usize;
            let start = end.saturating_sub(n);
            Ok(Page {
                first_line: self.first + start as u64,
                lines: self.lines[start..end].to_vec(),
            })
        }
    }

    fn needle(q: &str) -> Needle {
        Needle::new(q, false).unwrap()
    }

    fn lines_of(hits: &Hits) -> Vec<(u64, u64, &str)> {
        hits.hits
            .iter()
            .map(|h| (h.agent_id, h.line, h.text.as_str()))
            .collect()
    }

    #[test]
    fn finds_screen_rows_then_history_newest_first() {
        let mut h = Fake::new(10, &["error one", "ok", "\x1b[31mError\x1b[0m two  "], 2);
        let screen = ["fine".to_owned(), "an ERROR here".to_owned()];
        let mut scan = Scan::new(needle("error"), 0, 0);
        scan.agent(7, &screen, h.range(), |b, m| h.read(b, m));
        let hits = scan.finish();
        assert_eq!(
            lines_of(&hits),
            vec![(7, 14, "an ERROR here"), (7, 12, "Error two"), (7, 10, "error one")]
        );
        assert!(!hits.truncated);
        assert_eq!(h.reads, 2, "pages of two lines");
    }

    #[test]
    fn agents_keep_the_order_they_are_scanned_in() {
        let mut a = Fake::new(0, &["needle a"], 10);
        let mut b = Fake::new(0, &["needle b"], 10);
        let mut scan = Scan::new(needle("needle"), 0, 0);
        scan.agent(9, &[], b.range(), |x, m| b.read(x, m));
        scan.agent(3, &[], a.range(), |x, m| a.read(x, m));
        let hits = scan.finish();
        assert_eq!(lines_of(&hits), vec![(9, 0, "needle b"), (3, 0, "needle a")]);
    }

    #[test]
    fn per_agent_cap_leaves_room_for_the_next_agent() {
        let mut a = Fake::new(0, &["x"; 50], 100);
        let mut b = Fake::new(0, &["x"; 50], 100);
        let mut scan = Scan::new(needle("x"), 10, 3);
        scan.agent(1, &[], a.range(), |x, m| a.read(x, m));
        scan.agent(2, &[], b.range(), |x, m| b.read(x, m));
        let hits = scan.finish();
        let lines: Vec<_> = hits.hits.iter().map(|h| (h.agent_id, h.line)).collect();
        assert_eq!(lines, vec![(1, 49), (1, 48), (1, 47), (2, 49), (2, 48), (2, 47)]);
        assert!(!hits.truncated, "a per-agent cap is not truncation");
    }

    #[test]
    fn the_limit_stops_the_search_and_truncates() {
        let mut a = Fake::new(0, &["x"; 50], 100);
        let mut b = Fake::new(0, &["x"; 50], 100);
        let mut scan = Scan::new(needle("x"), 4, 4);
        scan.agent(1, &[], a.range(), |x, m| a.read(x, m));
        scan.agent(2, &[], b.range(), |x, m| b.read(x, m));
        assert!(scan.done());
        assert_eq!(b.reads, 0, "a full search reads nothing more");
        let hits = scan.finish();
        assert_eq!(hits.hits.len(), 4);
        assert!(hits.truncated);
    }

    #[test]
    fn the_byte_budget_stops_a_search_that_finds_nothing() {
        let mut a = Fake::new(0, &["abcdefghi"; 100], 10);
        let mut scan = Scan::with_budget(needle("zzz"), 0, 0, 95);
        scan.agent(1, &[], a.range(), |x, m| a.read(x, m));
        assert!(scan.done());
        assert_eq!(a.reads, 1, "the budget runs out in the first page");
        assert!(scan.finish().truncated);
    }

    #[test]
    fn a_failed_page_ends_only_that_agent() {
        let mut b = Fake::new(0, &["needle"], 10);
        let mut scan = Scan::new(needle("needle"), 0, 0);
        scan.agent(1, &[], Some((0, 5)), |_, _| {
            Err(io::Error::new(io::ErrorKind::NotFound, "gone"))
        });
        scan.agent(2, &[], b.range(), |x, m| b.read(x, m));
        assert_eq!(lines_of(&scan.finish()), vec![(2, 0, "needle")]);
    }

    #[test]
    fn long_lines_are_cut_at_a_character_boundary() {
        let long = format!("needle {}", "界".repeat(600));
        let mut a = Fake::new(0, &[long.as_str()], 10);
        let mut scan = Scan::new(needle("needle"), 0, 0);
        scan.agent(1, &[], a.range(), |x, m| a.read(x, m));
        let hits = scan.finish();
        let text = &hits.hits[0].text;
        assert!(text.len() <= SEARCH_TEXT_MAX_BYTES && text.len() > 1000);
        assert!(text.starts_with("needle 界"));
    }

    #[test]
    fn caps_apply_to_the_request() {
        let s = Scan::new(needle("x"), 0, 0);
        assert_eq!((s.limit, s.per_agent), (200, 20));
        let s = Scan::new(needle("x"), 5000, 0);
        assert_eq!((s.limit, s.per_agent), (1000, 20));
        let s = Scan::new(needle("x"), 5, 50);
        assert_eq!((s.limit, s.per_agent), (5, 5));
    }
}
