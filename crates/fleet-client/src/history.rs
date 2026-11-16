//! An agent's history and screen, read with a read-only attach plus
//! `ReadHistory` pages: what `fleet history` and `fleet screen` print and
//! the GUI's Save Output saves (ADR 0086).

use std::io::{self, Write};

use bytes::Bytes;
use fleet_proto::{Attach, AttachOk, Detach, ReadHistory, request, response};
use fleet_term::Terminal;

use crate::{Connection, Error, Result};

/// Lines asked for per `ReadHistory` request (the daemon caps it further).
const PAGE: u32 = 4096;

/// A read-only attach: it never takes the primary role from the user's
/// `fleet attach` or GUI (ADR 0022).
fn attach(client: &mut Connection, agent_id: u64) -> Result<AttachOk> {
    match client.call(request::Kind::Attach(Attach {
        agent_id,
        observe: true,
    }))? {
        response::Kind::Attached(ok) => Ok(*ok),
        _ => Err(Error::Protocol("unexpected reply to attach".into())),
    }
}

fn detach(client: &mut Connection, agent_id: u64) {
    let _ = client.call(request::Kind::Detach(Detach { agent_id }));
}

/// The snapshot's visible rows, trailing blank rows dropped.
fn screen_lines(ok: &AttachOk) -> Vec<String> {
    let mut term = Terminal::new(ok.cols.clamp(1, 4096) as u16, ok.rows.clamp(1, 4096) as u16);
    term.feed(&ok.snapshot);
    let mut rows = term.visible_text();
    while rows.last().is_some_and(|r| r.trim().is_empty()) {
        rows.pop();
    }
    rows
}

/// An agent's current screen as text, trailing blank rows dropped.
pub fn screen(client: &mut Connection, agent_id: u64) -> Result<Vec<String>> {
    let ok = attach(client, agent_id)?;
    detach(client, agent_id);
    Ok(screen_lines(&ok))
}

/// An agent's output: its history lines (ANSI rows) oldest first,
/// followed by its screen rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transcript {
    pub lines: Vec<Bytes>,
    pub screen: Vec<String>,
}

impl Transcript {
    /// The history lines without colours or other escapes, trailing
    /// spaces trimmed, then the screen rows.
    pub fn plain_lines(&self) -> Vec<String> {
        // One wide single-row terminal renders each line; a row is never
        // wider than its byte length.
        let cols = self
            .lines
            .iter()
            .map(Bytes::len)
            .max()
            .unwrap_or(1)
            .clamp(1, 65535) as u16;
        let mut term = Terminal::new(cols, 1);
        let mut out = Vec::with_capacity(self.lines.len() + self.screen.len());
        for line in &self.lines {
            term.feed(b"\x1b[0m\x1b[H\x1b[2J");
            term.feed(line);
            let row = term.visible_text().into_iter().next().unwrap_or_default();
            out.push(row.trim_end().to_owned());
        }
        out.extend(self.screen.iter().cloned());
        out
    }

    /// [`Transcript::plain_lines`], each ending in a newline.
    pub fn plain_text(&self) -> String {
        let mut s = String::new();
        for line in self.plain_lines() {
            s.push_str(&line);
            s.push('\n');
        }
        s
    }

    /// Writes the history lines as ANSI (each reset at its end), then the
    /// screen rows.
    pub fn write_ansi(&self, out: &mut impl Write) -> io::Result<()> {
        for line in &self.lines {
            out.write_all(line)?;
            out.write_all(b"\x1b[0m\n")?;
        }
        for row in &self.screen {
            writeln!(out, "{row}")?;
        }
        Ok(())
    }
}

fn is_cleared(e: &Error) -> bool {
    matches!(e, Error::Server(m) if m.contains("history cleared"))
}

fn collect(client: &mut Connection, agent_id: u64, limit: Option<usize>) -> Result<Transcript> {
    let ok = attach(client, agent_id)?;
    let mut screen = screen_lines(&ok);
    if let Some(n) = limit {
        screen.drain(..screen.len().saturating_sub(n));
    }
    // Lines still wanted from history; the screen counts towards the limit.
    let mut want = limit.map(|n| n.saturating_sub(screen.len()));
    // Newest first, reversed at the end.
    let mut rev_pages: Vec<Vec<Bytes>> = Vec::new();
    let mut tail = ok.history_tail;
    if let Some(w) = want {
        let skip = tail.len().saturating_sub(w);
        tail.drain(..skip);
        want = Some(w - tail.len());
    }
    rev_pages.push(tail);
    let mut before = ok.tail_first_line;
    while before > ok.history_first_line && want != Some(0) {
        let max_lines = want.map_or(PAGE, |w| w.min(PAGE as usize) as u32);
        let page = match client.call(request::Kind::ReadHistory(ReadHistory {
            agent_id,
            generation: ok.history_generation,
            before_line: before,
            max_lines,
        }))? {
            response::Kind::History(p) => p,
            _ => return Err(Error::Protocol("unexpected reply to history".into())),
        };
        if page.lines.is_empty() || page.first_line >= before {
            break;
        }
        before = page.first_line;
        want = want.map(|w| w.saturating_sub(page.lines.len()));
        rev_pages.push(page.lines);
    }
    let lines = rev_pages.into_iter().rev().flatten().collect();
    Ok(Transcript { lines, screen })
}

/// Reads an agent's history (its last `limit` lines, screen included,
/// if given) and screen, for running and exited agents alike. A history
/// cleared while paging is read again once. Blocks on the connection.
pub fn read(client: &mut Connection, agent_id: u64, limit: Option<usize>) -> Result<Transcript> {
    let result = match collect(client, agent_id, limit) {
        // The history was cleared while paging; start over once.
        Err(e) if is_cleared(&e) => {
            detach(client, agent_id);
            collect(client, agent_id, limit)
        }
        r => r,
    };
    // Best effort, also after a failure.
    detach(client, agent_id);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_lines_drop_escapes_and_trailing_spaces_then_add_the_screen() {
        let t = Transcript {
            lines: vec![
                Bytes::from_static(b"\x1b[31mred\x1b[0m text   "),
                Bytes::new(),
                Bytes::from_static(b"\x1b[1;4mbold\x1b[0m"),
            ],
            screen: vec!["$ ls".into(), "a b".into()],
        };
        assert_eq!(t.plain_lines(), ["red text", "", "bold", "$ ls", "a b"]);
        assert_eq!(t.plain_text(), "red text\n\nbold\n$ ls\na b\n");
        assert_eq!(Transcript::default().plain_text(), "");
    }

    #[test]
    fn ansi_lines_are_reset_at_their_end() {
        let t = Transcript {
            lines: vec![Bytes::from_static(b"\x1b[31mred")],
            screen: vec!["$".into()],
        };
        let mut out = Vec::new();
        t.write_ansi(&mut out).unwrap();
        assert_eq!(out, b"\x1b[31mred\x1b[0m\n$\n");
    }
}
