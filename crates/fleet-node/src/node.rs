//! The node: [`Node`] ties the [`PtyHost`](crate::PtyHost), the agent
//! [`Store`] and one [`AgentPipeline`] per agent together.
//!
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

use crate::pty::{AgentId, AgentInput, CommandSpec, OutputSink, PtyHost};
use crate::scrollback::{self, Scrollback};
use crate::store::{self, AgentRecord, NewAgent, Store};
use crate::term::Terminal;
use bytes::Bytes;
use fleet_proto::{AgentExited, AgentInfo, AttachOk, SpawnAgent};
use std::collections::{HashMap, VecDeque};
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
                scrollback: Some(scrollback),
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
        let (snapshot, _, sub) = self.subscribe_sized();
        (snapshot, sub)
    }

    /// [`AgentPipeline::subscribe`], plus the grid size `(cols, rows)` the
    /// snapshot was taken at.
    pub fn subscribe_sized(&self) -> (Vec<u8>, (u16, u16), Subscription) {
        let mut state = self.lock();
        let snapshot = Self::snapshot_of(&mut state);
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
        (snapshot, state.term.size(), sub)
    }

    /// The current screen as ANSI and the grid size `(cols, rows)`.
    fn snapshot_sized(&self) -> (Vec<u8>, (u16, u16)) {
        let mut state = self.lock();
        (Self::snapshot_of(&mut state), state.term.size())
    }

    fn snapshot_of(state: &mut State) -> Vec<u8> {
        state.term.flush_pending_sync();
        let mut snapshot = state.term.snapshot();
        snapshot.extend_from_slice(state.partial.tail());
        snapshot
    }

    /// Resizes the grid. Call alongside [`PtyHost::resize`](crate::PtyHost::resize).
    pub fn resize(&self, cols: u16, rows: u16) {
        self.lock().term.resize(cols, rows);
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
        // Flush, then close: an exited agent holds no fd.
        if let Some(mut scrollback) = state.scrollback.take() {
            let _ = scrollback.flush();
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

/// The agent's sink: feeds its pipeline and, on exit, records the exit code,
/// releases the agent and announces the exit.
struct NodeSink {
    id: u64,
    pipeline: AgentPipeline,
    store: Arc<Mutex<Store>>,
    host: Arc<PtyHost>,
    agents: Arc<Mutex<Agents>>,
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
        if let Some(live) = agents.live.remove(&self.id) {
            agents.retire(&self.host, self.id, &live, code);
        }
        drop(agents);
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
    /// [`RETAINED_EXITED`]) and drops everything else it held.
    fn retire(&mut self, host: &PtyHost, id: u64, live: &LiveAgent, exit_code: i32) {
        let (snapshot, (cols, rows)) = live.pipeline.snapshot_sized();
        if self.exited.len() >= RETAINED_EXITED {
            self.exited.pop_front();
        }
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
    /// Opens the node's state under `state_dir` (`state.db`, `scrollback/`)
    /// and marks agents left running by a previous daemon as lost.
    pub fn open(state_dir: &Path) -> io::Result<Node> {
        let scrollback_dir = state_dir.join("scrollback");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&scrollback_dir)?;
        let store = Store::open(&state_dir.join("state.db")).map_err(store_err)?;
        store.mark_running_as_lost().map_err(store_err)?;
        let (exits, _) = broadcast::channel(EXIT_EVENT_CAPACITY);
        Ok(Node {
            host: Arc::new(PtyHost::new()),
            store: Arc::new(Mutex::new(store)),
            agents: Arc::new(Mutex::new(Agents::default())),
            exits,
            scrollback_dir,
        })
    }

    /// Receives every agent exit from now on.
    pub fn exit_events(&self) -> broadcast::Receiver<AgentExited> {
        self.exits.subscribe()
    }

    /// Spawns an agent. Zero `cols`/`rows` mean 80x24; an empty `cwd` means
    /// the daemon's working directory.
    pub fn spawn(&self, req: SpawnAgent) -> io::Result<AgentInfo> {
        if lock(&self.agents).closing {
            return Err(shutting_down());
        }
        if req.program.is_empty() {
            return Err(invalid("program is empty".into()));
        }
        let dim = |v: u32, default: u16, what: &str| match v {
            0 => Ok(default),
            v => u16::try_from(v).map_err(|_| invalid(format!("{what} {v} out of range"))),
        };
        let cols = dim(req.cols, DEFAULT_COLS, "cols")?;
        let rows = dim(req.rows, DEFAULT_ROWS, "rows")?;
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
        };
        let (id, path) = lock(&self.store)
            .insert_agent_with_scrollback(&new, |id| {
                self.scrollback_dir
                    .join(format!("{id}.ring"))
                    .to_string_lossy()
                    .into_owned()
            })
            .map_err(store_err)?;

        let started =
            Scrollback::create(Path::new(&path), scrollback::DEFAULT_CAPACITY).and_then(|sb| {
                let pipeline = AgentPipeline::new(cols, rows, sb);
                let sink = NodeSink {
                    id,
                    pipeline: pipeline.clone(),
                    store: Arc::clone(&self.store),
                    host: Arc::clone(&self.host),
                    agents: Arc::clone(&self.agents),
                    exits: self.exits.clone(),
                };
                let spec = CommandSpec {
                    program: PathBuf::from(&req.program),
                    args: req.args.clone(),
                    cwd: Some(cwd),
                    env: req.env.into_iter().collect(),
                    cols,
                    rows,
                };
                let host_id = self.host.spawn_with_sink(spec, Box::new(sink))?;
                Ok(LiveAgent { host_id, pipeline })
            });
        let live = match started {
            Ok(live) => live,
            Err(e) => {
                if let Err(e) = lock(&self.store).delete_agent(id) {
                    eprintln!("fleet-node: removing failed agent {id}: {e}");
                }
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
        };
        {
            let mut agents = lock(&self.agents);
            if agents.closing {
                // Shutdown began during the spawn and will not signal this
                // agent; its exit is still recorded and waited for.
                drop(agents);
                let _ = self.host.force_kill(live.host_id);
                return Err(shutting_down());
            }
            // An agent that already exited was not found by its sink.
            match live.pipeline.exit_code() {
                Some(code) => agents.retire(&self.host, id, &live, code),
                None => {
                    agents.live.insert(id, live);
                }
            }
        }
        Ok(AgentInfo {
            id,
            program: new.program,
            args: new.args,
            cwd: new.cwd,
            state: fleet_proto::AgentState::Running.into(),
            exit_code: None,
            created_unix_ms,
        })
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

    fn live(&self, id: u64) -> io::Result<LiveAgent> {
        lock(&self.agents)
            .live
            .get(&id)
            .cloned()
            .ok_or_else(|| not_live(id))
    }

    /// Attaches to an agent (see [`AgentPipeline::subscribe`]). An exited
    /// agent yields its final screen (one of the last [`RETAINED_EXITED`]
    /// exits of this node process) or else an empty snapshot with size 0x0,
    /// then its exit.
    pub fn attach(&self, id: u64) -> io::Result<(AttachOk, Subscription)> {
        let ok = |snapshot, cols: u16, rows: u16| AttachOk {
            agent_id: id,
            snapshot,
            cols: cols.into(),
            rows: rows.into(),
        };
        {
            let agents = lock(&self.agents);
            if let Some(live) = agents.live.get(&id) {
                let (snapshot, (cols, rows), sub) = live.pipeline.subscribe_sized();
                return Ok((ok(snapshot, cols, rows), sub));
            }
            if let Some(a) = agents.exited(id) {
                let sub = Subscription::exited(a.exit_code);
                return Ok((ok(a.snapshot.clone(), a.cols, a.rows), sub));
            }
        }
        match lock(&self.store).exit_code(id).map_err(store_err)? {
            Some(code) => Ok((ok(Vec::new(), 0, 0), Subscription::exited(code))),
            None => Err(not_live(id)),
        }
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

    /// Resizes the agent's PTY and grid.
    pub fn resize(&self, id: u64, cols: u32, rows: u32) -> io::Result<()> {
        let dim = |v: u32, what: &str| {
            u16::try_from(v)
                .ok()
                .filter(|v| *v > 0)
                .ok_or_else(|| invalid(format!("{what} {v} out of range")))
        };
        let (cols, rows) = (dim(cols, "cols")?, dim(rows, "rows")?);
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
