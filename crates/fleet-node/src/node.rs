//! Per-agent output pipeline: PTY output feeds the agent's [`Terminal`] grid,
//! is appended to its [`Scrollback`], then broadcast to attached subscribers.
//!
//! Attach is [`AgentPipeline::subscribe`]: the snapshot and the subscription
//! are taken under the pipeline lock that also covers feeding the grid, so the
//! stream continues exactly where the snapshot ends.
//!
//! Subscriptions use a bounded `tokio::sync::mpsc` channel
//! ([`SUBSCRIBER_CAPACITY`] chunks): the reader thread only ever `try_send`s,
//! and the receiver can be awaited from a tokio task ([`Subscription::recv`]) or
//! read from a plain thread ([`Subscription::blocking_recv`]). A subscriber
//! whose channel is full is dropped and its next item is [`StreamItem::Lagged`];
//! the client must re-attach.
//!
//! The snapshot is taken with the parser in ground state, so it covers a
//! pending synchronized update (applied first) and is followed by the
//! unterminated tail of the last chunk (an escape sequence or UTF-8 character
//! split across reads, see [`PartialSequence`]); the client's parser then
//! continues in step with the pipeline's.
//!
//! Terminal query replies (DSR/CPR, DA, …) are written back to the agent only
//! while no subscriber is attached; otherwise the client's terminal answers.
//! They are dropped rather than ever blocking the reader thread.

use crate::pty::{AgentInput, OutputSink};
use crate::scrollback::Scrollback;
use crate::term::Terminal;
use bytes::Bytes;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::mpsc::{self, error::TrySendError};

/// Chunks a subscriber may fall behind before it is dropped.
pub const SUBSCRIBER_CAPACITY: usize = 256;

/// Longest unterminated sequence carried into a snapshot. Beyond it (e.g. a
/// huge OSC 52 payload) the tail is dropped and an attaching client may see
/// a garbled sequence until the next one.
const MAX_PARTIAL: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scan {
    Ground,
    /// Inside a UTF-8 character, this many continuation bytes still due.
    Utf8(u8),
    Esc,
    Csi,
    /// OSC, terminated by BEL or ST.
    Osc,
    /// DCS/SOS/PM/APC string, terminated by ST.
    Str,
    /// ESC seen inside an OSC (`osc == true`) or string: `\\` completes ST.
    StrEsc {
        osc: bool,
    },
}

/// Tracks the bytes of a sequence the output parser has started but not yet
/// finished, approximating the VT parser's states: ESC, CSI, OSC and DCS
/// (plus SOS/PM/APC) sequences, and UTF-8 characters.
#[derive(Debug)]
struct PartialSequence {
    scan: Scan,
    bytes: Vec<u8>,
    /// The current sequence outgrew [`MAX_PARTIAL`]; `bytes` is not kept.
    overflow: bool,
}

impl PartialSequence {
    fn new() -> Self {
        Self {
            scan: Scan::Ground,
            bytes: Vec::new(),
            overflow: false,
        }
    }

    /// The unterminated tail, if it is complete enough to replay.
    fn tail(&self) -> &[u8] {
        if self.overflow { &[] } else { &self.bytes }
    }

    fn feed(&mut self, chunk: &[u8]) {
        /// Where the sequence still open after `chunk` begins.
        enum Start {
            /// Before `chunk`: it continues the tracked sequence.
            Earlier,
            /// At this index of `chunk`.
            At(usize),
            /// At an ESC just before this index (possibly in an earlier chunk).
            EscBefore(usize),
        }
        let mut start = (self.scan != Scan::Ground).then_some(Start::Earlier);
        for (i, &b) in chunk.iter().enumerate() {
            let before = self.scan;
            self.scan = self.step(b);
            if self.scan == Scan::Ground {
                start = None;
                continue;
            }
            match before {
                Scan::Ground => start = Some(Start::At(i)),
                Scan::Utf8(_) if !(0x80..=0xbf).contains(&b) => start = Some(Start::At(i)),
                Scan::Esc | Scan::Csi if b == 0x1b => start = Some(Start::At(i)),
                Scan::StrEsc { .. } => start = Some(Start::EscBefore(i)),
                _ => {}
            }
        }
        let rest = match start {
            None => {
                self.bytes.clear();
                self.overflow = false;
                return;
            }
            Some(Start::Earlier) => chunk,
            Some(Start::At(i)) => {
                self.bytes.clear();
                self.overflow = false;
                &chunk[i..]
            }
            Some(Start::EscBefore(i)) => {
                self.bytes.clear();
                self.overflow = false;
                self.bytes.push(0x1b);
                &chunk[i..]
            }
        };
        if self.overflow || self.bytes.len() + rest.len() > MAX_PARTIAL {
            self.overflow = true;
            self.bytes.clear();
        } else {
            self.bytes.extend_from_slice(rest);
        }
    }

    fn step(&self, b: u8) -> Scan {
        Self::step_from(self.scan, b)
    }

    /// Next state after `b` in state `scan`.
    fn step_from(scan: Scan, b: u8) -> Scan {
        const CAN: u8 = 0x18;
        const SUB: u8 = 0x1a;
        const ESC: u8 = 0x1b;
        const BEL: u8 = 0x07;
        match scan {
            Scan::Ground | Scan::Utf8(_) => {
                if let Scan::Utf8(due) = scan
                    && (0x80..=0xbf).contains(&b)
                {
                    return if due > 1 {
                        Scan::Utf8(due - 1)
                    } else {
                        Scan::Ground
                    };
                }
                // Not a continuation: an unfinished character is abandoned.
                match b {
                    ESC => Scan::Esc,
                    0xc2..=0xdf => Scan::Utf8(1),
                    0xe0..=0xef => Scan::Utf8(2),
                    0xf0..=0xf4 => Scan::Utf8(3),
                    _ => Scan::Ground,
                }
            }
            Scan::Esc => match b {
                CAN | SUB => Scan::Ground,
                ESC => Scan::Esc,
                b'[' => Scan::Csi,
                b']' => Scan::Osc,
                b'P' | b'X' | b'^' | b'_' => Scan::Str,
                0x30..=0x7e => Scan::Ground,
                _ => Scan::Esc, // intermediates and C0 controls
            },
            Scan::Csi => match b {
                CAN | SUB => Scan::Ground,
                ESC => Scan::Esc,
                0x40..=0x7e => Scan::Ground,
                _ => Scan::Csi,
            },
            Scan::Osc => match b {
                CAN | SUB | BEL => Scan::Ground,
                ESC => Scan::StrEsc { osc: true },
                _ => Scan::Osc,
            },
            Scan::Str => match b {
                CAN | SUB => Scan::Ground,
                ESC => Scan::StrEsc { osc: false },
                _ => Scan::Str,
            },
            Scan::StrEsc { .. } => match b {
                b'\\' => Scan::Ground,
                // The string ended; its ESC starts a new sequence.
                _ => Self::step_from(Scan::Esc, b),
            },
        }
    }
}

/// One item of an agent's live stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamItem {
    /// Raw PTY output, in order.
    Output(Bytes),
    /// The agent exited with this code; the stream ends after it.
    Exited(i32),
    /// The subscriber fell behind and was dropped; the stream ends after it.
    Lagged,
}

/// Receiving end of a subscription; dropping it detaches.
pub struct Subscription {
    rx: mpsc::Receiver<StreamItem>,
    lagged: Arc<AtomicBool>,
    done: bool,
}

impl Subscription {
    /// Next item; `None` once the stream has ended.
    pub async fn recv(&mut self) -> Option<StreamItem> {
        if let Some(item) = self.check_lagged() {
            return item;
        }
        let item = self.rx.recv().await;
        self.after_recv(item)
    }

    /// Blocking variant of [`Subscription::recv`]; must not be called from
    /// within an async runtime.
    pub fn blocking_recv(&mut self) -> Option<StreamItem> {
        if let Some(item) = self.check_lagged() {
            return item;
        }
        let item = self.rx.blocking_recv();
        self.after_recv(item)
    }

    /// `Some` when the stream ended or lagged (pending output is discarded:
    /// the client must re-attach anyway).
    fn check_lagged(&mut self) -> Option<Option<StreamItem>> {
        if self.done {
            return Some(None);
        }
        if self.lagged.load(Ordering::Acquire) {
            self.done = true;
            return Some(Some(StreamItem::Lagged));
        }
        None
    }

    fn after_recv(&mut self, item: Option<StreamItem>) -> Option<StreamItem> {
        match item {
            // The sender is dropped right after setting the flag, so a closed
            // channel may still mean "lagged".
            None if self.lagged.load(Ordering::Acquire) => {
                self.done = true;
                Some(StreamItem::Lagged)
            }
            None => {
                self.done = true;
                None
            }
            Some(item) => Some(item),
        }
    }
}

struct Subscriber {
    tx: mpsc::Sender<StreamItem>,
    lagged: Arc<AtomicBool>,
}

impl Subscriber {
    /// Delivers `item`; `false` if the subscriber is gone (closed or lagged).
    fn offer(&self, item: StreamItem) -> bool {
        match self.tx.try_send(item) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.lagged.store(true, Ordering::Release);
                false
            }
            Err(TrySendError::Closed(_)) => false,
        }
    }
}

struct State {
    term: Terminal,
    scrollback: Scrollback,
    subscribers: Vec<Subscriber>,
    input: Option<AgentInput>,
    exit_code: Option<i32>,
    partial: PartialSequence,
}

/// One agent's pipeline; clones share it. Pass a clone to
/// [`PtyHost::spawn_with_sink`](crate::PtyHost::spawn_with_sink) and keep one
/// to serve attaches.
#[derive(Clone)]
pub struct AgentPipeline {
    state: Arc<Mutex<State>>,
}

impl AgentPipeline {
    pub fn new(cols: u16, rows: u16, scrollback: Scrollback) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                term: Terminal::new(cols, rows),
                scrollback,
                subscribers: Vec::new(),
                input: None,
                exit_code: None,
                partial: PartialSequence::new(),
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Attaches a subscriber: returns a snapshot of the current screen and the
    /// stream of everything after it. After exit the stream holds only
    /// [`StreamItem::Exited`].
    pub fn subscribe(&self) -> (Vec<u8>, Subscription) {
        let mut state = self.lock();
        state.term.flush_pending_sync();
        let mut snapshot = state.term.snapshot();
        snapshot.extend_from_slice(state.partial.tail());
        let (tx, rx) = mpsc::channel(SUBSCRIBER_CAPACITY);
        let lagged = Arc::new(AtomicBool::new(false));
        match state.exit_code {
            Some(code) => {
                // Capacity is at least 1, so this cannot fail.
                let _ = tx.try_send(StreamItem::Exited(code));
            }
            None => state.subscribers.push(Subscriber {
                tx,
                lagged: Arc::clone(&lagged),
            }),
        }
        let sub = Subscription {
            rx,
            lagged,
            done: false,
        };
        (snapshot, sub)
    }

    /// Resizes the grid. Call alongside [`PtyHost::resize`](crate::PtyHost::resize).
    pub fn resize(&self, cols: u16, rows: u16) {
        self.lock().term.resize(cols, rows);
    }

    /// The grid's visible text (see [`Terminal::visible_text`]).
    pub fn visible_text(&self) -> Vec<String> {
        self.lock().term.visible_text()
    }

    /// The grid's cursor (see [`Terminal::cursor`]).
    pub fn cursor(&self) -> (u16, u16) {
        self.lock().term.cursor()
    }
}

impl OutputSink for AgentPipeline {
    fn on_spawn(&mut self, input: AgentInput) {
        self.lock().input = Some(input);
    }

    fn on_output(&mut self, bytes: &[u8]) {
        let reply = {
            let mut state = self.lock();
            state.term.feed(bytes);
            state.partial.feed(bytes);
            let replies = state.term.take_replies();
            // History is best effort: the live grid stays authoritative.
            let _ = state.scrollback.append(bytes);
            if !state.subscribers.is_empty() {
                let chunk = Bytes::copy_from_slice(bytes);
                state
                    .subscribers
                    .retain(|s| s.offer(StreamItem::Output(chunk.clone())));
            }
            match (&state.input, replies.is_empty()) {
                (Some(input), false) if state.subscribers.is_empty() => {
                    Some((input.clone(), replies))
                }
                _ => None,
            }
        };
        if let Some((input, replies)) = reply {
            input.try_write_reply(&replies);
        }
    }

    fn on_exit(&mut self, code: i32) {
        let mut state = self.lock();
        state.term.flush_pending_sync();
        let _ = state.scrollback.flush();
        state.exit_code = Some(code);
        state.input = None;
        for sub in std::mem::take(&mut state.subscribers) {
            sub.offer(StreamItem::Exited(code));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tail_after(chunks: &[&[u8]]) -> Vec<u8> {
        let mut p = PartialSequence::new();
        for chunk in chunks {
            p.feed(chunk);
        }
        p.tail().to_vec()
    }

    #[test]
    fn complete_output_leaves_no_tail() {
        assert_eq!(
            tail_after(&[b"plain \x1b[31mred\x1b[0m \xe2\x82\xac\x1b]0;t\x07"]),
            b""
        );
        assert_eq!(tail_after(&[b"\x1bP+q\x1b\\", b"\x1b7"]), b"");
    }

    #[test]
    fn unterminated_sequences_are_kept() {
        assert_eq!(tail_after(&[b"a\x1b[3"]), b"\x1b[3");
        assert_eq!(tail_after(&[b"a\x1b[3", b"8;5"]), b"\x1b[38;5");
        assert_eq!(tail_after(&[b"a\xf0\x9f"]), b"\xf0\x9f");
        assert_eq!(
            tail_after(&[b"\x1b]0;a", b"b", b"c\x1b"]),
            b"\x1b]0;abc\x1b"
        );
        // A new sequence replaces a finished or abandoned one.
        assert_eq!(tail_after(&[b"\x1b[1m\x1b[2"]), b"\x1b[2");
        assert_eq!(tail_after(&[b"\x1b[1\x1b[2"]), b"\x1b[2");
        assert_eq!(tail_after(&[b"\xe2\x1b["]), b"\x1b[");
        // ESC ending an OSC without `\` starts the next sequence.
        assert_eq!(tail_after(&[b"\x1b]0;a\x1b", b"[3"]), b"\x1b[3");
        // CAN aborts.
        assert_eq!(tail_after(&[b"\x1b[3\x18"]), b"");
    }

    #[test]
    fn oversized_sequence_is_dropped_until_it_ends() {
        let big = vec![b'x'; MAX_PARTIAL];
        assert_eq!(tail_after(&[b"\x1b]52;c;", &big]), b"");
        assert_eq!(tail_after(&[b"\x1b]52;c;", &big, b"more"]), b"");
        assert_eq!(tail_after(&[b"\x1b]52;c;", &big, b"\x07\x1b[1"]), b"\x1b[1");
        assert_eq!(
            tail_after(&[b"\x1b]52;c;", &big, b"\x1b", b"[2"]),
            b"\x1b[2"
        );
    }
}
