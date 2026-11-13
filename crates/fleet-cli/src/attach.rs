//! Interactive attach: raw terminal, snapshot, streamed output, forwarded
//! input and resizes; detach with Ctrl-\ (see [`DetachScanner`]).

use std::io::{self, Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use fleet_proto::{Attach, Detach, Frame, Resize, SendInput, event, request, response};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};
use signal_hook::iterator::Signals;

use fleet_client::{Connection, Error, Result, Sender};

/// Ctrl-\ as a legacy control byte.
const DETACH_KEY: u8 = 0x1c;
/// How long an escape sequence that may still turn into the detach key is held
/// back at most when input stops in the middle of it (e.g. a lone Esc key
/// press), however busy the output stream is.
const PENDING_INPUT_TIMEOUT: Duration = Duration::from_millis(20);
/// Longest escape sequence held back while it may still be the detach key.
/// The longest detach form is well under this (`\x1b[92:124:92;133:2u`).
const MAX_PENDING: usize = 32;
/// Enter the local alternate screen and soft-reset: snapshots assume a fresh
/// terminal.
const ENTER: &[u8] = b"\x1b[?1049h\x1b[!p";
/// Undo everything an agent may have switched on: soft reset, pop the kitty
/// keyboard mode (pushed on the alt screen, whose stack is separate), leave
/// the alt screen, then reset the modes DECSTR leaves alone (cursor, bracketed
/// paste, mouse 1000/1002/1003/1005/1006/1015, focus events, application
/// cursor keys, autowrap).
const RESTORE: &[u8] = concat!(
    "\x1b[!p\x1b[<u\x1b[?1049l\x1b[?25h\x1b[?2004l",
    "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l",
    "\x1b[?1l\x1b[?7h",
)
.as_bytes();

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Exited,
    Detached,
    Lagged,
    /// Ended by SIGINT/SIGTERM/SIGHUP (the signal number).
    Signaled(i32),
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

// Frames are the common case; boxing them would allocate per frame.
#[allow(clippy::large_enum_variant)]
enum Ev {
    Frame(Frame),
    /// The server closed the connection or it failed.
    Closed(String),
    Stdin(Vec<u8>),
    StdinEof,
    Winch,
    Terminate(i32),
}

/// Frames the reader may have queued for the main loop. Beyond this it
/// blocks, the socket fills and the daemon reports a lag rather than `fleet`
/// buffering output for a stalled terminal.
const FRAME_QUEUE: usize = 64;

fn terminal_size() -> (u32, u32) {
    crossterm::terminal::size()
        .map(|(c, r)| (u32::from(c), u32::from(r)))
        .unwrap_or((80, 24))
}

/// Attaches and prints the snapshot only; no terminal changes.
pub fn snapshot_only(mut client: Connection, agent_id: u64) -> Result<()> {
    match client.call(request::Kind::Attach(Attach { agent_id }))? {
        response::Kind::Attached(ok) => {
            write_stdout(&ok.snapshot)?;
            Ok(())
        }
        _ => Err(Error::Protocol("unexpected reply to attach".into())),
    }
}

pub fn run(mut client: Connection, agent_id: u64) -> Result<Outcome> {
    let (events, inbox) = mpsc::channel::<Ev>();
    // Signals first, so none is missed while the terminal is being set up.
    let mut signals = Signals::new([SIGWINCH, SIGINT, SIGTERM, SIGHUP])?;

    // Resize first, so the snapshot is drawn at this terminal's size. An
    // exited agent cannot be resized; attaching still shows its last screen.
    let (cols, rows) = terminal_size();
    match client.call(request::Kind::Resize(Resize {
        agent_id,
        cols,
        rows,
    })) {
        Ok(_) | Err(Error::Server(_)) => {}
        Err(e) => return Err(e),
    }
    let ok = match client.call(request::Kind::Attach(Attach { agent_id }))? {
        response::Kind::Attached(ok) => ok,
        _ => return Err(Error::Protocol("unexpected reply to attach".into())),
    };
    let (mut frames, mut sender) = client.split();

    let guard = TerminalGuard::enter()?;
    write_stdout(&ok.snapshot)?;

    let (permits, released) = mpsc::sync_channel::<()>(FRAME_QUEUE);
    let tx = events.clone();
    thread::spawn(move || {
        loop {
            let ev = match frames.read_frame() {
                Ok(Some(f)) => Ev::Frame(f),
                Ok(None) => Ev::Closed("connection to fleetd closed".into()),
                Err(e) => Ev::Closed(e.to_string()),
            };
            let last = matches!(ev, Ev::Closed(_));
            if matches!(ev, Ev::Frame(_)) && permits.send(()).is_err() {
                return;
            }
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
                Ev::Terminate(sig)
            };
            if events.send(ev).is_err() {
                return;
            }
        }
    });

    let mut detach = DetachScanner::default();
    let mut input = Vec::new();
    // When input held back as a possible detach sequence is forwarded anyway.
    let mut flush_at: Option<Instant> = None;
    let outcome = loop {
        let ev = match flush_at {
            // Checked before receiving, so a steady stream of output frames
            // cannot hold the input back.
            Some(at) if Instant::now() >= at => {
                input.clear();
                detach.flush(&mut input);
                send_input(&mut sender, agent_id, &input)?;
                flush_at = None;
                continue;
            }
            Some(at) => match inbox.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(ev) => ev,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break Outcome::Detached,
            },
            None => match inbox.recv() {
                Ok(ev) => ev,
                Err(_) => break Outcome::Detached,
            },
        };
        if matches!(ev, Ev::Frame(_)) {
            let _ = released.try_recv();
        }
        match ev {
            Ev::Frame(Frame::Output { agent_id: a, data }) if a == agent_id => {
                write_stdout(&data)?;
            }
            Ev::Frame(Frame::Event(e)) => match e.kind {
                Some(event::Kind::Exited(x)) if x.agent_id == agent_id => break Outcome::Exited,
                Some(event::Kind::Lagged(l)) if l.agent_id == agent_id => break Outcome::Lagged,
                _ => {}
            },
            // Acks (and errors) for our input and resizes.
            Ev::Frame(_) => {}
            Ev::Closed(msg) => {
                drop(guard);
                return Err(Error::Protocol(msg));
            }
            Ev::Stdin(data) => {
                input.clear();
                let detached = detach.scan(&data, &mut input);
                send_input(&mut sender, agent_id, &input)?;
                if detached {
                    break Outcome::Detached;
                }
                flush_at = match (detach.has_pending(), flush_at) {
                    (false, _) => None,
                    (true, Some(at)) => Some(at),
                    (true, None) => Some(Instant::now() + PENDING_INPUT_TIMEOUT),
                };
            }
            Ev::StdinEof => {
                input.clear();
                detach.flush(&mut input);
                send_input(&mut sender, agent_id, &input)?;
                break Outcome::Detached;
            }
            Ev::Terminate(sig) => break Outcome::Signaled(sig),
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
    if matches!(outcome, Outcome::Detached | Outcome::Signaled(_)) {
        let _ = sender.send(request::Kind::Detach(Detach { agent_id }));
    }
    drop(guard);
    Ok(outcome)
}

fn send_input(sender: &mut Sender, agent_id: u64, data: &[u8]) -> Result<()> {
    if !data.is_empty() {
        sender.send(request::Kind::Input(SendInput {
            agent_id,
            data: data.to_vec(),
        }))?;
    }
    Ok(())
}

/// Finds the detach key (Ctrl-\) in the user's input and passes everything
/// else through unchanged.
///
/// Ctrl-\ is recognised as the byte 0x1c and, because an agent may switch the
/// user's terminal to another key encoding, as:
/// - kitty keyboard protocol (CSI u): `CSI 92 ; 5 u`, with any alternate-key
///   sub-fields after the key code (`92:124:92`), an event type of press (`:1`)
///   or repeat (`:2`), not release (`:3`), and an optional text field;
/// - xterm modifyOtherKeys: `CSI 27 ; 5 ; 92 ~`.
///
/// In both, the modifiers must be Ctrl alone, ignoring Caps Lock and Num Lock
/// (kitty reports those too); the key code must be 92 (`\`), so on layouts
/// where `\` is not a base key only the 0x1c form applies.
///
/// An escape sequence that may still become a detach sequence when a read ends
/// is held back until the next read decides it: as soon as it can no longer
/// match, it is passed on. The caller flushes it with [`DetachScanner::flush`]
/// when no more input arrives in time (a lone Esc key press looks like the
/// start of one).
#[derive(Debug, Default)]
pub(crate) struct DetachScanner {
    pending: Vec<u8>,
}

/// What a held-back escape sequence has become with its latest byte.
enum Candidate {
    /// Still a prefix of some detach sequence.
    Prefix,
    /// A complete detach sequence.
    Detach,
    /// Complete and not a detach sequence: pass it all on.
    Other,
    /// The latest byte cannot continue it: pass on all bytes before it and
    /// scan that byte afresh (it may be 0x1c or start a new sequence).
    Broken,
}

impl DetachScanner {
    /// Scans `data`, appending the bytes to forward to `out`. Returns true at
    /// the detach key; the bytes after it are dropped (the session ends).
    pub(crate) fn scan(&mut self, data: &[u8], out: &mut Vec<u8>) -> bool {
        for &b in data {
            if self.pending.is_empty() {
                match b {
                    DETACH_KEY => return true,
                    0x1b => self.pending.push(b),
                    _ => out.push(b),
                }
                continue;
            }
            self.pending.push(b);
            match self.classify() {
                Candidate::Prefix => {}
                Candidate::Detach => {
                    self.pending.clear();
                    return true;
                }
                Candidate::Other => self.flush(out),
                Candidate::Broken => {
                    self.pending.pop();
                    self.flush(out);
                    match b {
                        DETACH_KEY => return true,
                        0x1b => self.pending.push(b),
                        _ => out.push(b),
                    }
                }
            }
        }
        false
    }

    /// Whether input is held back (see [`DetachScanner::flush`]).
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Appends the held-back bytes to `out`, giving up on them as a detach
    /// sequence.
    pub(crate) fn flush(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.pending);
    }

    /// Classifies `pending` (ESC, then at least one more byte).
    fn classify(&self) -> Candidate {
        let p = &self.pending;
        if p[1] != b'[' {
            return Candidate::Broken;
        }
        let Some(&last) = p[2..].last() else {
            return Candidate::Prefix;
        };
        match last {
            b'0'..=b'9' | b';' | b':' if p.len() < MAX_PENDING => Candidate::Prefix,
            b'u' | b'~' if is_detach_csi(&p[2..p.len() - 1], last) => Candidate::Detach,
            // Any other final byte ends the CSI sequence.
            0x40..=0x7e => Candidate::Other,
            _ => Candidate::Broken,
        }
    }
}

/// Whether `CSI params final` (params of digits, `;` and `:`) is Ctrl-\.
fn is_detach_csi(params: &[u8], final_byte: u8) -> bool {
    let Ok(params) = std::str::from_utf8(params) else {
        return false;
    };
    let fields: Vec<&str> = params.split(';').collect();
    match (final_byte, fields.as_slice()) {
        // CSI key[:alternates] ; modifiers[:event] [; text] u
        (b'u', [key, mods, rest @ ..]) if rest.len() <= 1 => {
            let mut mods = mods.split(':');
            let ctrl = mods.next().is_some_and(is_ctrl_only);
            let event = mods.next();
            key.split(':').next() == Some("92")
                && ctrl
                && matches!(event, None | Some("1") | Some("2"))
                && mods.next().is_none()
        }
        // CSI 27 ; modifiers ; key ~
        (b'~', ["27", mods, "92"]) => is_ctrl_only(mods),
        _ => false,
    }
}

/// Whether a modifier parameter (1 + bit mask) is Ctrl, possibly with the
/// Caps Lock (64) or Num Lock (128) bits.
fn is_ctrl_only(param: &str) -> bool {
    const CTRL: u32 = 4;
    const LOCKS: u32 = 64 | 128;
    param
        .parse::<u32>()
        .ok()
        .and_then(|m| m.checked_sub(1))
        .is_some_and(|bits| bits & !LOCKS == CTRL)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scans `chunks` in order; returns the forwarded bytes, whether it
    /// detached and what is still held back.
    fn scan_chunks(chunks: &[&[u8]]) -> (Vec<u8>, bool, Vec<u8>) {
        let mut scanner = DetachScanner::default();
        let mut out = Vec::new();
        for chunk in chunks {
            if scanner.scan(chunk, &mut out) {
                return (out, true, scanner.pending);
            }
        }
        (out, false, scanner.pending)
    }

    fn scan(data: &[u8]) -> (Vec<u8>, bool) {
        let (mut out, detached, pending) = scan_chunks(&[data]);
        out.extend_from_slice(&pending);
        (out, detached)
    }

    const DETACH_FORMS: &[&[u8]] = &[
        b"\x1c",
        b"\x1b[92;5u",
        b"\x1b[92;5:1u",
        b"\x1b[92;5:2u",
        b"\x1b[92:124;5u",
        b"\x1b[92::92;5:1u",
        b"\x1b[92:124:92;5:1u",
        b"\x1b[92;5;92u",
        b"\x1b[92;69u",
        b"\x1b[92;133:1u",
        b"\x1b[92;197u",
        b"\x1b[27;5;92~",
        b"\x1b[27;69;92~",
    ];

    #[test]
    fn detach_forms_detach_and_keep_earlier_input() {
        for form in DETACH_FORMS {
            let mut data = b"ls -l".to_vec();
            data.extend_from_slice(form);
            data.extend_from_slice(b"dropped");
            assert_eq!(
                scan(&data),
                (b"ls -l".to_vec(), true),
                "{:?}",
                String::from_utf8_lossy(form)
            );
        }
    }

    #[test]
    fn plain_input_passes_through() {
        let mut all: Vec<u8> = (0u8..=255)
            .filter(|&b| b != DETACH_KEY && b != 0x1b)
            .collect();
        all.extend_from_slice("héllo ✓".as_bytes());
        assert_eq!(scan(&all), (all.clone(), false));
    }

    #[test]
    fn near_misses_pass_through_unchanged() {
        let near_misses: &[&[u8]] = &[
            b"\x1b[92;5:3u",    // release
            b"\x1b[92;1u",      // no Ctrl
            b"\x1b[92u",        // no modifiers
            b"\x1b[92;7u",      // Ctrl+Alt
            b"\x1b[92;6u",      // Ctrl+Shift
            b"\x1b[92;13u",     // Ctrl+Super
            b"\x1b[92;0u",      // invalid modifiers
            b"\x1b[92;;u",      // empty modifiers
            b"\x1b[92;5:1:1u",  // extra sub-field
            b"\x1b[92;5;92;1u", // too many fields
            b"\x1b[93;5u",      // ']'
            b"\x1b[920;5u",     // another key
            b"\x1b[124:92;5u",  // '|' with a base layout '\\'
            b"\x1b[92;5~",      // wrong final
            b"\x1b[27;5;93~",   // modifyOtherKeys, another key
            b"\x1b[27;6;92~",   // modifyOtherKeys, Ctrl+Shift
            b"\x1b[27;5;92;1~", // modifyOtherKeys, extra field
            b"\x1b[27;5:1;92~", // modifyOtherKeys, sub-field
            b"\x1b[?92;5u",     // private marker
            b"\x1b[>92;5u",     // private marker
            b"\x1b[92;5$u",     // intermediate
            b"\x1b[1;5A",       // Ctrl-Up
            b"\x1b[200~pasted\x1b[201~",
            b"\x1b[27u", // Esc under kitty
            b"\x1bO5A",
            b"\x1b\\",     // Alt-\\
            b"\x1b\x1b[A", // Esc, then Up
        ];
        for miss in near_misses {
            let mut data = b"a".to_vec();
            data.extend_from_slice(miss);
            data.extend_from_slice(b"z");
            assert_eq!(
                scan(&data),
                (data.clone(), false),
                "{:?}",
                String::from_utf8_lossy(miss)
            );
        }
    }

    #[test]
    fn overlong_candidate_is_passed_on() {
        let mut data = b"\x1b[".to_vec();
        data.extend(std::iter::repeat_n(b'9', 100));
        data.extend_from_slice(b";5u");
        assert_eq!(scan(&data), (data.clone(), false));
        // And scanning continues after it.
        data.extend_from_slice(b"\x1b[92;5u");
        assert!(scan(&data).1);
    }

    #[test]
    fn detach_inside_a_broken_sequence() {
        // C0 bytes are executed inside a CSI sequence; ESC restarts one.
        assert_eq!(scan(b"\x1b[92\x1c"), (b"\x1b[92".to_vec(), true));
        assert_eq!(scan(b"\x1b\x1c"), (b"\x1b".to_vec(), true));
        assert_eq!(scan(b"\x1b[9\x1b[92;5u"), (b"\x1b[9".to_vec(), true));
        assert_eq!(scan(b"\x1b\x1b[92;5u"), (b"\x1b".to_vec(), true));
    }

    #[test]
    fn split_detach_sequences_detach() {
        for form in DETACH_FORMS {
            for cut in 1..form.len() {
                let (head, tail) = form.split_at(cut);
                let (out, detached, _) = scan_chunks(&[b"x", head, tail, b"y"]);
                assert!(detached, "{:?} cut at {cut}", String::from_utf8_lossy(form));
                assert_eq!(out, b"x");
            }
            // One byte per read.
            let bytes: Vec<&[u8]> = form.chunks(1).collect();
            let (out, detached, _) = scan_chunks(&bytes);
            assert!(detached && out.is_empty());
        }
    }

    #[test]
    fn split_prefix_is_held_then_released() {
        let mut scanner = DetachScanner::default();
        let mut out = Vec::new();
        assert!(!scanner.scan(b"ab\x1b[92", &mut out));
        assert_eq!(out, b"ab");
        assert!(scanner.has_pending());
        // The next read shows it is something else: all of it is forwarded.
        assert!(!scanner.scan(b";5Acd", &mut out));
        assert_eq!(out, b"ab\x1b[92;5Acd");
        assert!(!scanner.has_pending());

        // A lone Esc key press is held until flushed (on timeout or EOF).
        let mut out = Vec::new();
        assert!(!scanner.scan(b"\x1b", &mut out));
        assert!(out.is_empty() && scanner.has_pending());
        scanner.flush(&mut out);
        assert_eq!(out, b"\x1b");
        assert!(!scanner.has_pending());

        // Split near misses come out whole and in order.
        let (out, detached, pending) = scan_chunks(&[b"\x1b[92;5", b":3", b"u!"]);
        assert_eq!(
            (out, detached, pending),
            (b"\x1b[92;5:3u!".to_vec(), false, vec![])
        );
    }

    #[test]
    fn every_split_of_mixed_input_forwards_it_unchanged() {
        let input: &[u8] =
            b"echo \x1b[1;5A\x1b[92;5:3u\x1b[27;5;93~\x1bx\x1b[200~p\x1b[201~\x1b[92;6u done";
        for cut in 0..=input.len() {
            let (head, tail) = input.split_at(cut);
            let (mut out, detached, pending) = scan_chunks(&[head, tail]);
            out.extend_from_slice(&pending);
            assert!(!detached);
            assert_eq!(out, input, "cut at {cut}");
        }
    }
}
