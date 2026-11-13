//! Interactive attach: raw terminal, snapshot, streamed output, forwarded
//! input and resizes; detach with Ctrl-\.

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;

use fleet_proto::{Attach, Detach, Frame, Resize, SendInput, event, request, response};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};
use signal_hook::iterator::Signals;

use crate::client::{Client, Error, Result};

const DETACH_KEY: u8 = 0x1c;
/// Enter the local alternate screen and soft-reset: snapshots assume a fresh
/// terminal.
const ENTER: &[u8] = b"\x1b[?1049h\x1b[!p";
/// Undo everything an agent may have switched on, then leave the alt screen.
const RESTORE: &[u8] = b"\x1b[!p\x1b[?1049l\x1b[?25h\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1l\x1b[?7h";

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Exited,
    Detached,
    Lagged,
}

/// Restores the terminal when dropped (every exit path, panics included).
struct TerminalGuard {
    raw: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let guard = TerminalGuard { raw: true };
        write_stdout(ENTER)?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = write_stdout(RESTORE);
        if self.raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

fn write_stdout(bytes: &[u8]) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(bytes)?;
    out.flush()
}

enum Ev {
    Frame(Frame),
    /// The server closed the connection or it failed.
    Closed(String),
    Stdin(Vec<u8>),
    StdinEof,
    Winch,
    Terminate,
}

fn terminal_size() -> (u32, u32) {
    crossterm::terminal::size()
        .map(|(c, r)| (u32::from(c), u32::from(r)))
        .unwrap_or((80, 24))
}

/// Attaches and prints the snapshot only; no terminal changes.
pub fn snapshot_only(mut client: Client, agent_id: u64) -> Result<()> {
    match client.call(request::Kind::Attach(Attach { agent_id }))? {
        response::Kind::Attached(ok) => {
            write_stdout(&ok.snapshot)?;
            Ok(())
        }
        _ => Err(Error::Protocol("unexpected reply to attach".into())),
    }
}

pub fn run(mut client: Client, agent_id: u64) -> Result<Outcome> {
    let (events, inbox) = mpsc::channel::<Ev>();
    // Signals first, so none is missed while the terminal is being set up.
    let mut signals = Signals::new([SIGWINCH, SIGINT, SIGTERM, SIGHUP])?;

    let ok = match client.call(request::Kind::Attach(Attach { agent_id }))? {
        response::Kind::Attached(ok) => ok,
        _ => return Err(Error::Protocol("unexpected reply to attach".into())),
    };
    let (mut frames, mut sender) = client.split();

    let guard = TerminalGuard::enter()?;
    write_stdout(&ok.snapshot)?;
    let (cols, rows) = terminal_size();
    sender.send(request::Kind::Resize(Resize {
        agent_id,
        cols,
        rows,
    }))?;

    let tx = events.clone();
    thread::spawn(move || {
        loop {
            let ev = match frames.read_frame() {
                Ok(Some(f)) => Ev::Frame(f),
                Ok(None) => Ev::Closed("connection to fleetd closed".into()),
                Err(e) => Ev::Closed(e.to_string()),
            };
            let last = matches!(ev, Ev::Closed(_));
            if tx.send(ev).is_err() || last {
                return;
            }
        }
    });
    let tx = events.clone();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            let ev = match stdin.read(&mut buf) {
                Ok(0) | Err(_) => Ev::StdinEof,
                Ok(n) => Ev::Stdin(buf[..n].to_vec()),
            };
            let last = matches!(ev, Ev::StdinEof);
            if tx.send(ev).is_err() || last {
                return;
            }
        }
    });
    thread::spawn(move || {
        for sig in signals.forever() {
            let ev = if sig == SIGWINCH {
                Ev::Winch
            } else {
                Ev::Terminate
            };
            if events.send(ev).is_err() {
                return;
            }
        }
    });

    let outcome = loop {
        let Ok(ev) = inbox.recv() else {
            break Outcome::Detached;
        };
        match ev {
            Ev::Frame(Frame::Output { agent_id: a, data }) if a == agent_id => {
                write_stdout(&data)?;
            }
            Ev::Frame(Frame::Event(e)) => {
                if let Some(event::Kind::Exited(x)) = e.kind
                    && x.agent_id == agent_id
                {
                    break Outcome::Exited;
                }
            }
            Ev::Frame(Frame::Response(r)) if r.id == 0 => {
                if let Some(response::Kind::Error(e)) = r.kind
                    && e.message.starts_with("lagged")
                {
                    break Outcome::Lagged;
                }
            }
            // Acks (and errors) for our input and resizes.
            Ev::Frame(_) => {}
            Ev::Closed(msg) => {
                drop(guard);
                return Err(Error::Protocol(msg));
            }
            Ev::Stdin(data) => {
                let (send, detach) = match data.iter().position(|&b| b == DETACH_KEY) {
                    Some(i) => (&data[..i], true),
                    None => (&data[..], false),
                };
                if !send.is_empty() {
                    sender.send(request::Kind::Input(SendInput {
                        agent_id,
                        data: send.to_vec(),
                    }))?;
                }
                if detach {
                    break Outcome::Detached;
                }
            }
            Ev::StdinEof | Ev::Terminate => break Outcome::Detached,
            Ev::Winch => {
                let (cols, rows) = terminal_size();
                sender.send(request::Kind::Resize(Resize {
                    agent_id,
                    cols,
                    rows,
                }))?;
            }
        }
    };
    if outcome == Outcome::Detached {
        let _ = sender.send(request::Kind::Detach(Detach { agent_id }));
    }
    drop(guard);
    Ok(outcome)
}
