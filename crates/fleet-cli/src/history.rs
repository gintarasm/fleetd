//! `fleet history` and `fleet screen`: read-only views built from an attach
//! snapshot plus `ReadHistory` pages.

use std::io::{self, Write};

use bytes::Bytes;
use fleet_client::{Connection, Error, Result};
use fleet_proto::{Attach, AttachOk, Detach, ReadHistory, request, response};
use fleet_term::Terminal;

/// Lines asked for per `ReadHistory` request (the daemon caps it further).
const PAGE: u32 = 4096;

fn attach(client: &mut Connection, agent_id: u64) -> Result<AttachOk> {
    match client.call(request::Kind::Attach(Attach { agent_id }))? {
        response::Kind::Attached(ok) => Ok(ok),
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

pub fn screen(mut client: Connection, agent_id: u64) -> Result<()> {
    let ok = attach(&mut client, agent_id)?;
    detach(&mut client, agent_id);
    let mut out = io::stdout().lock();
    for row in screen_lines(&ok) {
        writeln!(out, "{row}")?;
    }
    Ok(())
}

/// History lines (ANSI rows) oldest first, followed by the screen rows.
struct Collected {
    lines: Vec<Bytes>,
    screen: Vec<String>,
}

fn is_cleared(e: &Error) -> bool {
    matches!(e, Error::Server(m) if m.contains("history cleared"))
}

fn collect(client: &mut Connection, agent_id: u64, limit: Option<usize>) -> Result<Collected> {
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
    Ok(Collected { lines, screen })
}

pub fn history(
    mut client: Connection,
    agent_id: u64,
    limit: Option<usize>,
    plain: bool,
) -> Result<()> {
    let result = match collect(&mut client, agent_id, limit) {
        // The history was cleared while paging; start over once.
        Err(e) if is_cleared(&e) => {
            detach(&mut client, agent_id);
            collect(&mut client, agent_id, limit)
        }
        r => r,
    };
    // Best effort, also after a failure.
    detach(&mut client, agent_id);
    let c = result?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    if plain {
        // One wide single-row terminal renders each line; a row is never
        // wider than its byte length.
        let cols = c
            .lines
            .iter()
            .map(Bytes::len)
            .max()
            .unwrap_or(1)
            .clamp(1, 65535) as u16;
        let mut term = Terminal::new(cols, 1);
        for line in &c.lines {
            term.feed(b"\x1b[0m\x1b[H\x1b[2J");
            term.feed(line);
            let row = term.visible_text().into_iter().next().unwrap_or_default();
            writeln!(out, "{}", row.trim_end())?;
        }
    } else {
        for line in &c.lines {
            out.write_all(line)?;
            out.write_all(b"\x1b[0m\n")?;
        }
    }
    for row in &c.screen {
        writeln!(out, "{row}")?;
    }
    out.flush()?;
    Ok(())
}
