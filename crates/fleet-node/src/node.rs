//! The node: [`Node`] ties the [`PtyHost`](crate::PtyHost), the agent
//! [`Store`] and one [`AgentPipeline`] per agent together.
//!
//! Per-agent output pipeline: PTY output feeds the agent's [`Terminal`] grid,
//! is appended to its [`Scrollback`], then broadcast to attached subscribers.
//! Rows the grid scrolls off become lines of the agent's [`History`].
//!
//! Attach is [`AgentPipeline::attach`]: the history tail, the snapshot and the
//! subscription are taken under the pipeline lock that also covers feeding the
//! grid, so tail, screen and stream are contiguous.
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

use crate::history::{History, HistoryInfo, Page};
use crate::pty::{AgentId, AgentInput, CommandSpec, OutputSink, PtyHost};
use crate::scrollback::{self, Scrollback};
use crate::store::{self, AgentRecord, Labels, NewAgent, Store};
use crate::term::{HistoryEvent, Terminal};
use bytes::Bytes;
use fleet_proto::{AgentExited, AgentInfo, AttachOk, SpawnAgent};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
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
    /// A stream that holds only [`StreamItem::Exited`].
    fn exited(code: i32) -> Self {
        let (tx, rx) = mpsc::channel(1);
        // Capacity is 1, so this cannot fail.
        let _ = tx.try_send(StreamItem::Exited(code));
        Subscription {
            rx,
            lagged: Arc::new(AtomicBool::new(false)),
            done: false,
        }
    }

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
    /// `None` once the agent has exited (flushed and closed).
    scrollback: Option<Scrollback>,
    subscribers: Vec<Subscriber>,
    input: Option<AgentInput>,
    exit_code: Option<i32>,
    partial: PartialSequence,
    /// `None` without history, and once the agent has exited (flushed and
    /// closed).
    history: Option<History>,
}

impl State {
    /// Moves the grid's history events into the agent's history. Must follow
    /// every grid operation that can scroll rows off (feed, sync flush,
    /// resize): undrained events pile up in the terminal.
    fn drain_history(&mut self) {
        let events = self.term.drain_history();
        if events.is_empty() {
            return;
        }
        let Some(history) = &mut self.history else {
            return;
        };
        for event in events {
            match event {
                HistoryEvent::Line(line) => history.push(&line),
                HistoryEvent::Cleared => history.clear(),
            }
        }
    }
}

/// The history part of an attach: the in-memory tail and where it sits.
#[derive(Debug, Clone)]
pub struct HistoryTail {
    /// Line number of `lines[0]`.
    pub tail_first_line: u64,
    pub lines: Vec<Bytes>,
    pub info: HistoryInfo,
}

/// What [`AgentPipeline::attach`] hands out.
pub struct Attached {
    pub snapshot: Vec<u8>,
    /// Grid size `(cols, rows)` the snapshot was taken at.
    pub size: (u16, u16),
    /// `None` if the pipeline keeps no history or the agent has exited.
    pub history: Option<HistoryTail>,
    pub subscription: Subscription,
}

/// One agent's pipeline; clones share it. Pass a clone to
/// [`PtyHost::spawn_with_sink`](crate::PtyHost::spawn_with_sink) and keep one
/// to serve attaches.
#[derive(Clone)]
pub struct AgentPipeline {
    state: Arc<Mutex<State>>,
}

impl AgentPipeline {
    /// A pipeline without history (scrolled-off rows are dropped).
    pub fn new(cols: u16, rows: u16, scrollback: Scrollback) -> Self {
        Self::with_history(cols, rows, scrollback, None)
    }

    /// A pipeline that appends scrolled-off rows to `history`.
    pub fn with_history(
        cols: u16,
        rows: u16,
        scrollback: Scrollback,
        history: Option<History>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                term: Terminal::new(cols, rows),
                scrollback: Some(scrollback),
                subscribers: Vec::new(),
                input: None,
                exit_code: None,
                partial: PartialSequence::new(),
                history,
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
        let (snapshot, _, sub) = self.subscribe_sized();
        (snapshot, sub)
    }

    /// [`AgentPipeline::subscribe`], plus the grid size `(cols, rows)` the
    /// snapshot was taken at.
    pub fn subscribe_sized(&self) -> (Vec<u8>, (u16, u16), Subscription) {
        let a = self.attach();
        (a.snapshot, a.size, a.subscription)
    }

    /// [`AgentPipeline::subscribe_sized`], plus the history tail taken in the
    /// same critical section: the tail ends where the snapshot's screen
    /// begins.
    pub fn attach(&self) -> Attached {
        let mut state = self.lock();
        let snapshot = Self::snapshot_of(&mut state);
        let history = state.history.as_ref().map(|h| {
            let (tail_first_line, lines) = h.tail();
            HistoryTail {
                tail_first_line,
                lines,
                info: h.info(),
            }
        });
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
        let subscription = Subscription {
            rx,
            lagged,
            done: false,
        };
        Attached {
            snapshot,
            size: state.term.size(),
            history,
            subscription,
        }
    }

    /// A page of the agent's history (see [`History::read_page`]); `None` if
    /// the pipeline holds no history (none kept, or the agent exited).
    pub fn read_history(
        &self,
        generation: u64,
        before_line: u64,
        max_lines: u32,
    ) -> Option<io::Result<Page>> {
        let state = self.lock();
        let history = state.history.as_ref()?;
        Some(history.read_page(generation, before_line, max_lines))
    }

    /// The current screen as ANSI and the grid size `(cols, rows)`.
    fn snapshot_sized(&self) -> (Vec<u8>, (u16, u16)) {
        let mut state = self.lock();
        (Self::snapshot_of(&mut state), state.term.size())
    }

    fn snapshot_of(state: &mut State) -> Vec<u8> {
        state.term.flush_pending_sync();
        state.drain_history();
        let mut snapshot = state.term.snapshot();
        snapshot.extend_from_slice(state.partial.tail());
        snapshot
    }

    /// Resizes the grid. Call alongside [`PtyHost::resize`](crate::PtyHost::resize).
    pub fn resize(&self, cols: u16, rows: u16) {
        let mut state = self.lock();
        state.term.resize(cols, rows);
        state.drain_history();
    }

    /// The agent's exit code, once it has exited.
    pub fn exit_code(&self) -> Option<i32> {
        self.lock().exit_code
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
            state.drain_history();
            state.partial.feed(bytes);
            let replies = state.term.take_replies();
            // History is best effort: the live grid stays authoritative.
            if let Some(scrollback) = &mut state.scrollback {
                let _ = scrollback.append(bytes);
            }
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
        state.drain_history();
        // Flush, then close: an exited agent holds no fd.
        if let Some(mut scrollback) = state.scrollback.take() {
            let _ = scrollback.flush();
        }
        if let Some(mut history) = state.history.take()
            && let Err(e) = history.flush()
        {
            eprintln!("fleet-node: flushing agent history: {e}");
        }
        state.exit_code = Some(code);
        state.input = None;
        for sub in std::mem::take(&mut state.subscribers) {
            sub.offer(StreamItem::Exited(code));
        }
    }
}

/// Exit events a [`Node`] buffers per receiver before it lags.
pub const EXIT_EVENT_CAPACITY: usize = 256;

/// How long [`Node::shutdown`] lets agents exit after SIGTERM.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// How long [`Node::shutdown`] waits for agents after SIGKILL.
const KILL_WAIT: Duration = Duration::from_millis(500);

/// Exited agents whose final screen a [`Node`] keeps for attaches; beyond
/// this the oldest exit is evicted and attaching to it yields an empty
/// snapshot (see decision 0018).
pub const RETAINED_EXITED: usize = 64;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
/// Largest accepted `cols` or `rows`.
pub const MAX_DIM: u32 = 1000;
/// Largest accepted `cols * rows`.
pub const MAX_CELLS: u32 = 250_000;

/// The `TERM` agents get unless the spawn request sets one: the snapshot
/// and the grid speak xterm, whatever the daemon's own environment says.
const DEFAULT_TERM: &str = "xterm-256color";

fn with_default_term(env: HashMap<String, String>) -> Vec<(String, String)> {
    let mut env: Vec<_> = env.into_iter().collect();
    if !env.iter().any(|(k, _)| k == "TERM") {
        env.push(("TERM".into(), DEFAULT_TERM.into()));
    }
    env
}

/// Checks a terminal size: each side in `1..=MAX_DIM`, at most `MAX_CELLS`.
fn checked_size(cols: u32, rows: u32) -> io::Result<(u16, u16)> {
    let side = |v: u32, what: &str| match u16::try_from(v) {
        Ok(v) if (1..=MAX_DIM).contains(&u32::from(v)) => Ok(v),
        _ => Err(invalid(format!("{what} {v} out of range (1..={MAX_DIM})"))),
    };
    let (c, r) = (side(cols, "cols")?, side(rows, "rows")?);
    if cols * rows > MAX_CELLS {
        return Err(invalid(format!(
            "size {cols}x{rows} out of range (at most {MAX_CELLS} cells)"
        )));
    }
    Ok((c, r))
}

/// At most this many labels per agent (ADR 0025)...
pub const MAX_LABELS: usize = 64;
/// ...with keys of 1 to this many bytes...
pub const MAX_LABEL_KEY_BYTES: usize = 64;
/// ...and values of at most this many bytes.
pub const MAX_LABEL_VALUE_BYTES: usize = 256;
/// Label keys with this prefix are reserved for the core; clients cannot
/// set or remove them.
pub const RESERVED_LABEL_PREFIX: &str = "fleet.";

/// Checks a label key written by a client.
fn check_label_key(key: &str) -> io::Result<()> {
    if key.is_empty() || key.len() > MAX_LABEL_KEY_BYTES {
        return Err(invalid(format!(
            "label key {key:?} must be 1 to {MAX_LABEL_KEY_BYTES} bytes"
        )));
    }
    if key.starts_with(RESERVED_LABEL_PREFIX) {
        return Err(invalid(format!(
            "label key {key:?} is reserved (prefix {RESERVED_LABEL_PREFIX:?})"
        )));
    }
    Ok(())
}

/// Checks labels written by a client: keys, values and count.
fn check_labels(labels: &Labels) -> io::Result<()> {
    for (key, value) in labels {
        check_label_key(key)?;
        if value.len() > MAX_LABEL_VALUE_BYTES {
            return Err(invalid(format!(
                "label {key:?}: value longer than {MAX_LABEL_VALUE_BYTES} bytes"
            )));
        }
    }
    if labels.len() > MAX_LABELS {
        return Err(invalid(format!(
            "{} labels (at most {MAX_LABELS})",
            labels.len()
        )));
    }
    Ok(())
}

/// Whether `labels` contain every pair of `selector`.
fn labels_match(labels: &Labels, selector: &Labels) -> bool {
    selector.iter().all(|(k, v)| labels.get(k) == Some(v))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn store_err(e: rusqlite::Error) -> io::Error {
    io::Error::other(e)
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// A running agent of this node process.
#[derive(Clone)]
struct LiveAgent {
    host_id: AgentId,
    pipeline: AgentPipeline,
}

/// The final screen of an agent that exited in this node process.
struct ExitedAgent {
    id: u64,
    snapshot: Vec<u8>,
    cols: u16,
    rows: u16,
    exit_code: i32,
}

/// Histories of agents not running in this node process kept open at a time
/// (each holds its in-memory tail and index, but no fd).
const HISTORY_HANDLES: usize = 8;

/// The history directories (`<state-dir>/history/<agent-id>/`) and lazily
/// opened handles to the histories of agents not running here: exited in
/// this process, or left by a previous daemon. A running agent's history is
/// owned by its pipeline.
struct Histories {
    dir: PathBuf,
    /// Most recently used last; at most [`HISTORY_HANDLES`].
    open: VecDeque<(u64, History)>,
    /// Agents being spawned: their history is written by a pipeline that is
    /// not registered yet, and must not be opened from disk meanwhile.
    spawning: HashSet<u64>,
}

impl Histories {
    fn path(&self, id: u64) -> PathBuf {
        self.dir.join(id.to_string())
    }

    /// Runs `f` on the agent's history, opening it from disk if needed.
    /// Only for agents whose history no pipeline writes any more.
    fn with<T>(&mut self, id: u64, f: impl FnOnce(&History) -> T) -> io::Result<T> {
        if self.spawning.contains(&id) {
            return Err(not_live(id));
        }
        let history = match self.open.iter().position(|(i, _)| *i == id) {
            Some(pos) => self.open.remove(pos).expect("position is in range").1,
            None => History::open(&self.path(id))?,
        };
        if self.open.len() >= HISTORY_HANDLES {
            self.open.pop_front();
        }
        self.open.push_back((id, history));
        Ok(f(&self.open.back().expect("just pushed").1))
    }

    /// Closes and deletes the agent's history.
    fn delete(&mut self, id: u64) {
        self.open.retain(|(i, _)| *i != id);
        remove_history_dir(&self.path(id));
    }
}

fn remove_history_dir(path: &Path) {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("fleet-node: removing {}: {e}", path.display()),
    }
}

/// Deletes the history dirs under `dir` of agents other than `keep`.
fn prune_history_dirs(dir: &Path, keep: &HashSet<u64>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let id = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u64>().ok());
        if id.is_some_and(|id| !keep.contains(&id)) {
            remove_history_dir(&entry.path());
        }
    }
    Ok(())
}

/// The agents whose history [`Node::open`] keeps: running ones and the
/// [`RETAINED_EXITED`] most recently created others.
fn retained_history(records: &[AgentRecord]) -> HashSet<u64> {
    let mut stopped: Vec<&AgentRecord> = records
        .iter()
        .filter(|r| r.state != store::AgentState::Running)
        .collect();
    stopped.sort_by_key(|r| std::cmp::Reverse((r.created_unix_ms, r.id)));
    stopped.truncate(RETAINED_EXITED);
    records
        .iter()
        .filter(|r| r.state == store::AgentState::Running)
        .chain(stopped)
        .map(|r| r.id)
        .collect()
}

/// The agent's sink: feeds its pipeline and, on exit, records the exit code,
/// releases the agent and announces the exit.
struct NodeSink {
    id: u64,
    pipeline: AgentPipeline,
    store: Arc<Mutex<Store>>,
    host: Arc<PtyHost>,
    agents: Arc<Mutex<Agents>>,
    histories: Arc<Mutex<Histories>>,
    exits: broadcast::Sender<AgentExited>,
}

impl OutputSink for NodeSink {
    fn on_spawn(&mut self, input: AgentInput) {
        self.pipeline.on_spawn(input);
    }

    fn on_output(&mut self, bytes: &[u8]) {
        self.pipeline.on_output(bytes);
    }

    fn on_exit(&mut self, code: i32) {
        // The store first, so anyone who sees the exit also lists it.
        if let Err(e) = lock(&self.store).set_exited(self.id, code) {
            eprintln!("fleet-node: recording exit of agent {}: {e}", self.id);
        }
        self.pipeline.on_exit(code);
        // If the spawn has not registered the agent yet, it retires it.
        let mut agents = lock(&self.agents);
        let evicted = match agents.live.remove(&self.id) {
            Some(live) => agents.retire(&self.host, self.id, &live, code),
            None => None,
        };
        drop(agents);
        if let Some(evicted) = evicted {
            lock(&self.histories).delete(evicted);
        }
        // No receivers is fine.
        let _ = self.exits.send(AgentExited {
            agent_id: self.id,
            exit_code: code,
        });
    }
}

/// One node: spawns agents, persists them and serves attaches.
///
/// Every method may block (SQLite, PTY writes, process spawn); call them off
/// the async executor.
pub struct Node {
    host: Arc<PtyHost>,
    store: Arc<Mutex<Store>>,
    agents: Arc<Mutex<Agents>>,
    exits: broadcast::Sender<AgentExited>,
    scrollback_dir: PathBuf,
    histories: Arc<Mutex<Histories>>,
}

#[derive(Default)]
struct Agents {
    /// Running agents.
    live: HashMap<u64, LiveAgent>,
    /// The last [`RETAINED_EXITED`] exits, oldest first.
    exited: VecDeque<ExitedAgent>,
    /// Set by [`Node::shutdown`]: no new agents.
    closing: bool,
}

impl Agents {
    /// Keeps an exited agent's final screen (evicting the oldest beyond
    /// [`RETAINED_EXITED`]) and drops everything else it held. Returns the
    /// evicted agent, whose history the caller deletes.
    #[must_use]
    fn retire(&mut self, host: &PtyHost, id: u64, live: &LiveAgent, exit_code: i32) -> Option<u64> {
        let (snapshot, (cols, rows)) = live.pipeline.snapshot_sized();
        let evicted = if self.exited.len() >= RETAINED_EXITED {
            self.exited.pop_front().map(|a| a.id)
        } else {
            None
        };
        self.exited.push_back(ExitedAgent {
            id,
            snapshot,
            cols,
            rows,
            exit_code,
        });
        // The host recorded the exit before calling `on_exit`, so this
        // cannot fail with "still running".
        let _ = host.remove(live.host_id);
        evicted
    }

    fn exited(&self, id: u64) -> Option<&ExitedAgent> {
        self.exited.iter().find(|a| a.id == id)
    }
}

fn not_live(id: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("agent {id} is not live on this node"),
    )
}

fn shutting_down() -> io::Error {
    io::Error::other("node is shutting down")
}

impl Node {
    /// Opens the node's state under `state_dir` (`state.db`, `scrollback/`,
    /// `history/`), marks agents left running by a previous daemon as lost
    /// and deletes the history of all but the [`RETAINED_EXITED`] most
    /// recently created of them.
    pub fn open(state_dir: &Path) -> io::Result<Node> {
        let scrollback_dir = state_dir.join("scrollback");
        let history_dir = state_dir.join("history");
        for dir in [&scrollback_dir, &history_dir] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let store = Store::open(&state_dir.join("state.db")).map_err(store_err)?;
        store.mark_running_as_lost().map_err(store_err)?;
        // A corrupt row's history is not kept (its id is unknown).
        let (records, _) = store.list_agents_lossy().map_err(store_err)?;
        prune_history_dirs(&history_dir, &retained_history(&records))?;
        let (exits, _) = broadcast::channel(EXIT_EVENT_CAPACITY);
        Ok(Node {
            host: Arc::new(PtyHost::new()),
            store: Arc::new(Mutex::new(store)),
            agents: Arc::new(Mutex::new(Agents::default())),
            exits,
            scrollback_dir,
            histories: Arc::new(Mutex::new(Histories {
                dir: history_dir,
                open: VecDeque::new(),
                spawning: HashSet::new(),
            })),
        })
    }

    /// Receives every agent exit from now on.
    pub fn exit_events(&self) -> broadcast::Receiver<AgentExited> {
        self.exits.subscribe()
    }

    /// Spawns an agent. Zero `cols`/`rows` mean 80x24; otherwise the size
    /// must pass the [`MAX_DIM`]/[`MAX_CELLS`] limits. An empty `cwd` means
    /// the daemon's working directory. `TERM` defaults to `xterm-256color`.
    /// The labels must pass the label limits, without reserved keys.
    /// `req.attach` is ignored (see [`Node::spawn_attached`]).
    pub fn spawn(&self, req: SpawnAgent) -> io::Result<AgentInfo> {
        self.start(req, None).map(|(info, _)| info)
    }

    /// [`Node::spawn`], attached: the subscription is taken before the
    /// process starts, so it carries all of the agent's output (and its
    /// terminal queries reach the subscriber, not the node). `before_start`
    /// runs with the agent's id before the process starts, e.g. to route the
    /// agent's exit to this subscription.
    pub fn spawn_attached(
        &self,
        req: SpawnAgent,
        before_start: impl FnOnce(u64),
    ) -> io::Result<(AgentInfo, AttachOk, Subscription)> {
        let (info, attached) = self.start(req, Some(Box::new(before_start)))?;
        let (ok, sub) = attached.expect("an attached start returns its attachment");
        Ok((info, ok, sub))
    }

    /// Spawns an agent; with `attach`, also attaches to it before the process
    /// starts, calling `attach` with its id first.
    fn start(
        &self,
        req: SpawnAgent,
        attach: Option<Box<dyn FnOnce(u64) + '_>>,
    ) -> io::Result<(AgentInfo, Option<(AttachOk, Subscription)>)> {
        if lock(&self.agents).closing {
            return Err(shutting_down());
        }
        if req.program.is_empty() {
            return Err(invalid("program is empty".into()));
        }
        check_labels(&req.labels)?;
        let or_default = |v: u32, default: u16| if v == 0 { u32::from(default) } else { v };
        let (cols, rows) = checked_size(
            or_default(req.cols, DEFAULT_COLS),
            or_default(req.rows, DEFAULT_ROWS),
        )?;
        let cwd = if req.cwd.is_empty() {
            std::env::current_dir()?
        } else {
            PathBuf::from(&req.cwd)
        };
        let created_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        let new = NewAgent {
            program: req.program.clone(),
            args: req.args.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            created_unix_ms,
            scrollback_path: String::new(),
            labels: req.labels,
        };
        let (id, path) = lock(&self.store)
            .insert_agent_with_scrollback(&new, |id| {
                self.scrollback_dir
                    .join(format!("{id}.ring"))
                    .to_string_lossy()
                    .into_owned()
            })
            .map_err(store_err)?;
        // Until the agent is registered, its history must not be opened
        // from disk (its pipeline may already be writing it).
        let history_path = {
            let mut histories = lock(&self.histories);
            histories.spawning.insert(id);
            histories.path(id)
        };
        let started = Scrollback::create(Path::new(&path), scrollback::DEFAULT_CAPACITY)
            .and_then(|sb| Ok((sb, History::create(&history_path)?)))
            .and_then(|(sb, history)| {
                let pipeline = AgentPipeline::with_history(cols, rows, sb, Some(history));
                // Subscribed before the process exists: nothing it prints
                // or asks escapes the subscriber.
                let attached = attach.map(|before_start| {
                    before_start(id);
                    let a = pipeline.attach();
                    let ok = attach_ok(id, a.snapshot, a.size, a.history);
                    (ok, a.subscription)
                });
                let sink = NodeSink {
                    id,
                    pipeline: pipeline.clone(),
                    store: Arc::clone(&self.store),
                    host: Arc::clone(&self.host),
                    agents: Arc::clone(&self.agents),
                    histories: Arc::clone(&self.histories),
                    exits: self.exits.clone(),
                };
                let spec = CommandSpec {
                    program: PathBuf::from(&req.program),
                    args: req.args.clone(),
                    cwd: Some(cwd),
                    env: with_default_term(req.env),
                    cols,
                    rows,
                };
                let host_id = self.host.spawn_with_sink(spec, Box::new(sink))?;
                Ok((LiveAgent { host_id, pipeline }, attached))
            });
        let (live, attached) = match started {
            Ok(started) => started,
            Err(e) => {
                if let Err(e) = lock(&self.store).delete_agent(id) {
                    eprintln!("fleet-node: removing failed agent {id}: {e}");
                }
                let _ = std::fs::remove_file(&path);
                let mut histories = lock(&self.histories);
                histories.spawning.remove(&id);
                remove_history_dir(&history_path);
                return Err(e);
            }
        };
        let evicted = {
            let mut agents = lock(&self.agents);
            if agents.closing {
                // Shutdown began during the spawn and will not signal this
                // agent; its exit is still recorded and waited for. Its
                // history stays `spawning`: it is never registered.
                drop(agents);
                let _ = self.host.force_kill(live.host_id);
                return Err(shutting_down());
            }
            // An agent that already exited was not found by its sink.
            match live.pipeline.exit_code() {
                Some(code) => agents.retire(&self.host, id, &live, code),
                None => {
                    agents.live.insert(id, live);
                    None
                }
            }
        };
        let mut histories = lock(&self.histories);
        histories.spawning.remove(&id);
        if let Some(evicted) = evicted {
            histories.delete(evicted);
        }
        drop(histories);
        let info = AgentInfo {
            id,
            program: new.program,
            args: new.args,
            cwd: new.cwd,
            state: fleet_proto::AgentState::Running.into(),
            exit_code: None,
            created_unix_ms,
            labels: new.labels,
        };
        Ok((info, attached))
    }

    /// Every agent this node knows of, including exited and lost ones. A
    /// corrupt row is logged and left out.
    pub fn list(&self) -> io::Result<Vec<AgentInfo>> {
        let (records, bad) = lock(&self.store).list_agents_lossy().map_err(store_err)?;
        for e in bad {
            eprintln!("fleet-node: skipping unreadable agent row: {e}");
        }
        Ok(records.into_iter().map(agent_info).collect())
    }

    /// The agents whose labels contain every `selector` pair (all of them
    /// for an empty selector).
    pub fn list_matching(&self, selector: &Labels) -> io::Result<Vec<AgentInfo>> {
        let mut agents = self.list()?;
        agents.retain(|a| labels_match(&a.labels, selector));
        Ok(agents)
    }

    /// Sets, then removes, labels of a known agent (running or not). Keys
    /// must not be reserved, and the limits hold for the result.
    pub fn set_labels(&self, id: u64, set: Labels, remove: &[String]) -> io::Result<()> {
        check_labels(&set)?;
        for key in remove {
            check_label_key(key)?;
        }
        let store = lock(&self.store);
        let mut labels = store
            .labels(id)
            .map_err(store_err)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no agent {id}")))?;
        labels.extend(set);
        for key in remove {
            labels.remove(key);
        }
        check_labels(&labels)?;
        store.replace_labels(id, &labels).map_err(store_err)
    }

    fn live(&self, id: u64) -> io::Result<LiveAgent> {
        lock(&self.agents)
            .live
            .get(&id)
            .cloned()
            .ok_or_else(|| not_live(id))
    }

    /// Attaches to an agent (see [`AgentPipeline::attach`]). An exited
    /// agent yields its final screen (one of the last [`RETAINED_EXITED`]
    /// exits of this node process) or else an empty snapshot with size 0x0,
    /// then its exit. The history fields describe the agent's history if it
    /// is still kept (and are zero/empty otherwise).
    pub fn attach(&self, id: u64) -> io::Result<(AttachOk, Subscription)> {
        let (snapshot, (cols, rows), history, sub) = {
            let agents = lock(&self.agents);
            if let Some(live) = agents.live.get(&id) {
                let a = live.pipeline.attach();
                (a.snapshot, a.size, a.history, a.subscription)
            } else if let Some(a) = agents.exited(id) {
                let sub = Subscription::exited(a.exit_code);
                (a.snapshot.clone(), (a.cols, a.rows), None, sub)
            } else {
                drop(agents);
                match lock(&self.store).exit_code(id).map_err(store_err)? {
                    Some(code) => (Vec::new(), (0, 0), None, Subscription::exited(code)),
                    None => return Err(not_live(id)),
                }
            }
        };
        // No history from the pipeline: the agent exited, and no pipeline
        // writes its history any more.
        let history = history.or_else(|| {
            lock(&self.histories)
                .with(id, |h| {
                    let (tail_first_line, lines) = h.tail();
                    HistoryTail {
                        tail_first_line,
                        lines,
                        info: h.info(),
                    }
                })
                .ok()
        });
        Ok((attach_ok(id, snapshot, (cols, rows), history), sub))
    }

    /// The lines of the agent's history immediately before `before_line`
    /// (see [`History::read_page`]), for running agents, retained exited
    /// ones and those of a previous daemon run. A stale `generation` fails
    /// with `InvalidInput`.
    pub fn read_history(
        &self,
        id: u64,
        generation: u64,
        before_line: u64,
        max_lines: u32,
    ) -> io::Result<Page> {
        let live = lock(&self.agents).live.get(&id).cloned();
        if let Some(page) =
            live.and_then(|l| l.pipeline.read_history(generation, before_line, max_lines))
        {
            return page;
        }
        lock(&self.histories).with(id, |h| h.read_page(generation, before_line, max_lines))?
    }

    /// Exited agents whose final screen is kept (at most [`RETAINED_EXITED`]).
    pub fn retained_exited_agents(&self) -> usize {
        lock(&self.agents).exited.len()
    }

    /// Writes to the agent's terminal input; blocks while its input queue is
    /// full.
    pub fn input(&self, id: u64, bytes: &[u8]) -> io::Result<()> {
        let live = self.live(id)?;
        self.host.write_input(live.host_id, bytes)
    }

    /// Resizes the agent's PTY and grid (within [`MAX_DIM`]/[`MAX_CELLS`]).
    pub fn resize(&self, id: u64, cols: u32, rows: u32) -> io::Result<()> {
        let (cols, rows) = checked_size(cols, rows)?;
        let live = self.live(id)?;
        self.host.resize(live.host_id, cols, rows)?;
        live.pipeline.resize(cols, rows);
        Ok(())
    }

    /// Sends SIGTERM to the agent's process group; a no-op once it exited.
    pub fn kill(&self, id: u64) -> io::Result<()> {
        match self.live(id) {
            Ok(live) => self.host.kill(live.host_id),
            Err(_) if self.exit_code(id).is_some() => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The agent's exit code once it has exited; `None` while it runs or if
    /// it is unknown.
    pub fn exit_code(&self, id: u64) -> Option<i32> {
        {
            let agents = lock(&self.agents);
            if let Some(live) = agents.live.get(&id) {
                return live.pipeline.exit_code();
            }
            if let Some(a) = agents.exited(id) {
                return Some(a.exit_code);
            }
        }
        lock(&self.store).exit_code(id).ok().flatten()
    }

    /// Stops every agent (there is no handoff to a successor daemon, see
    /// decision 0018): SIGTERM to each process group, [`SHUTDOWN_GRACE`] to
    /// exit, then SIGKILL. Exits are recorded as usual; an agent that still
    /// has not reported is left `running` in the store, to be marked lost by
    /// the next [`Node::open`]. Refuses new spawns from now on.
    pub fn shutdown(&self) {
        let host_ids: Vec<AgentId> = {
            let mut agents = lock(&self.agents);
            agents.closing = true;
            agents.live.values().map(|a| a.host_id).collect()
        };
        for &id in &host_ids {
            if let Err(e) = self.host.kill(id) {
                eprintln!("fleet-node: SIGTERM to agent group: {e}");
            }
        }
        if self.host.wait_all(SHUTDOWN_GRACE).is_ok() {
            return;
        }
        for &id in &host_ids {
            if let Err(e) = self.host.force_kill(id) {
                eprintln!("fleet-node: SIGKILL to agent group: {e}");
            }
        }
        if self.host.wait_all(KILL_WAIT).is_err() {
            eprintln!("fleet-node: some agents did not exit; they will be marked lost");
        }
    }
}

fn attach_ok(
    id: u64,
    snapshot: Vec<u8>,
    (cols, rows): (u16, u16),
    history: Option<HistoryTail>,
) -> AttachOk {
    let mut ok = AttachOk {
        agent_id: id,
        snapshot,
        cols: cols.into(),
        rows: rows.into(),
        ..Default::default()
    };
    if let Some(h) = history {
        ok.history_tail = h.lines;
        ok.tail_first_line = h.tail_first_line;
        ok.history_first_line = h.info.first_line;
        ok.history_generation = h.info.generation;
    }
    ok
}

fn agent_info(rec: AgentRecord) -> AgentInfo {
    use fleet_proto::AgentState as Wire;
    let state = match rec.state {
        store::AgentState::Running => Wire::Running,
        store::AgentState::Exited => Wire::Exited,
        store::AgentState::Lost => Wire::Lost,
    };
    AgentInfo {
        id: rec.id,
        program: rec.program,
        args: rec.args,
        cwd: rec.cwd,
        state: state.into(),
        exit_code: rec.exit_code,
        created_unix_ms: rec.created_unix_ms,
        labels: rec.labels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_sized(node: &Node, cols: u32, rows: u32) -> io::Result<AgentInfo> {
        node.spawn(SpawnAgent {
            program: "/bin/sleep".into(),
            args: vec!["100".into()],
            cols,
            rows,
            ..Default::default()
        })
    }

    #[test]
    fn spawn_and_resize_reject_out_of_range_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::open(dir.path()).unwrap();
        let bad = [(1001, 24), (80, 1001), (501, 500), (u32::MAX, 24)];
        for (cols, rows) in bad {
            let e = spawn_sized(&node, cols, rows).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{cols}x{rows}");
        }
        assert!(node.list().unwrap().is_empty(), "rejected spawns left rows");

        let id = spawn_sized(&node, 1000, 250).unwrap().id;
        for (cols, rows) in bad.into_iter().chain([(0, 24), (80, 0)]) {
            let e = node.resize(id, cols, rows).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{cols}x{rows}");
        }
        node.resize(id, 500, 500).unwrap();
        node.shutdown();
    }

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
