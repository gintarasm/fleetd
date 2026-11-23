//! The node: [`Node`] ties the [`AgentHost`](crate::AgentHost), the agent
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
//! Subscriptions use a bounded queue ([`SUBSCRIBER_CAPACITY`] chunks): the
//! reader thread only ever offers to it without waiting, and the receiver can
//! be awaited from a tokio task ([`Subscription::recv`]) or read from a plain
//! thread ([`Subscription::blocking_recv`]). Output chunks still queued are
//! merged up to [`COALESCE_BYTES`], so the queue's depth in output does not
//! shrink with the PTY's read size (~1 KiB on macOS). A subscriber whose
//! queue is full is dropped and its next item is [`StreamItem::Lagged`]; the
//! client must re-attach.
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
//!
//! **Primary client.** Subscribers taken by a connection ([`ConnId`]) have a
//! role: the primary is the connection that most recently attached to the
//! agent or sent it input; only its resizes apply (a resize from another
//! connection is ignored, one from a connection while no connection is
//! attached applies). A connection holds at most one subscription per agent:
//! attaching again replaces it. Role changes reach the affected subscribers
//! in their stream as [`StreamItem::Role`]. When the primary's subscription
//! ends (dropped, lagged), the remaining connection that most recently
//! attached or sent input becomes primary and is told so. An observing
//! attach ([`Node::observe`]) is an anonymous subscriber: it never takes or
//! changes the role. Applied resizes reach every subscriber in their stream
//! as [`StreamItem::Resized`], in order with the output.
//!
//! **Activity.** The output path only marks the agent dirty (a flag and a
//! timestamp, under the pipeline lock it already holds; the first mark since
//! the last tick queues the agent for the ticker). A ticker thread (started
//! by the first output, parked while nothing is queued) reads the queued
//! agents' last non-blank row at most every [`ACTIVITY_TICK`] and publishes
//! [`AgentActivity`] to every [`ActivityReceiver`]. A receiver keeps only the
//! newest pending event per agent, so a slow one loses nothing but
//! superseded values. Publishing happens under the pipeline lock, and exit
//! drops the agent's pending events under that lock before its stream ends,
//! so no activity follows an agent's exit.

use crate::approval::Approvals;
use crate::env::LoginEnv;
use crate::git::GitCache;
use crate::handoff::{self, HandoffGate};
use crate::history::{History, HistoryInfo, Page};
use crate::host::{AdoptSpec, AgentHost, TerminalHandle};
use crate::lease::{Leases, load_average, now_ms};
use crate::mailbox::Notice;
use crate::pty::{AgentId, AgentInput, CommandSpec, OutputSink, PtyHost};
use crate::scope::{self, Caller, Member, Quotas, Tree};

mod mailbox;
mod waiting;
pub use self::mailbox::{
    INBOX_PAGE_MAX, WaitSpec, mailbox_of, message_proto, wait_state_from_proto, wait_state_proto,
};
use crate::scrollback::{self, Scrollback};
use crate::search::{self as output_search, Hits};
use crate::store::{self, AgentRecord, Labels, NewAgent, Store};
use crate::term::{HistoryEvent, Needle, Terminal};
use crate::token::{self, AgentToken};
use bytes::{Bytes, BytesMut};
use fleet_proto::handoff::{HandoffAgent, handoff_agent};
use fleet_proto::status::{KIND_LABEL, SHELL_KIND};
use fleet_proto::{
    AGENT_TOKEN_ENV, AgentActivity, AgentExited, AgentInfo, AgentPriority, AgentRemoved,
    AgentReported, AttachOk, PendingSpawn, Reported, ReportedState, SpawnAgent, SpawnRefused,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::os::fd::{IntoRawFd, OwnedFd};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::thread::Thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

/// Chunks a subscriber may fall behind before it is dropped.
pub const SUBSCRIBER_CAPACITY: usize = 256;

/// Output chunks still queued for a subscriber are merged into one buffer of
/// up to this many bytes, so its queue holds up to [`SUBSCRIBER_CAPACITY`] x
/// this much output (4 MiB) however small the PTY reads are, not 256 reads.
/// A queued item never takes more than max(this, one read) plus a constant.
pub const COALESCE_BYTES: usize = 16 * 1024;

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
    /// The subscriber's connection became (`true`) or stopped being
    /// (`false`) the agent's primary client.
    Role(bool),
    /// The grid was resized to `cols` x `rows`; output after it is at that
    /// size. Every subscriber gets it, primary or not.
    Resized { cols: u16, rows: u16 },
}

/// Identifies a client connection to the node, for the primary-client rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnId(pub u64);

/// One subscription's queue: at most [`SUBSCRIBER_CAPACITY`] items, filled
/// by the reader thread without ever waiting and drained by one receiver,
/// either from a tokio task or a plain thread. Consecutive output chunks
/// still queued are merged up to [`COALESCE_BYTES`], so a slot holds more
/// than one PTY read (a macOS PTY read returns at most ~1 KiB).
struct Queue {
    state: Mutex<QueueState>,
    /// Wakes an async receiver; stores a permit if it is not waiting yet.
    wake: Notify,
    /// Wakes a blocking receiver.
    wake_blocking: Condvar,
}

#[derive(Default)]
struct QueueState {
    items: VecDeque<Queued>,
    /// The sending side is gone: the stream ends once `items` is drained.
    sender_gone: bool,
    /// The receiver is gone: nothing more is accepted.
    receiver_gone: bool,
}

enum Queued {
    /// One output chunk, shared with the other subscribers.
    Output(Bytes),
    /// Consecutive output chunks copied into one buffer of capacity
    /// [`COALESCE_BYTES`] (or one larger read).
    Merged(BytesMut),
    Other(StreamItem),
}

enum Offered {
    Queued,
    Full,
    Closed,
}

impl Queue {
    fn new() -> Arc<Queue> {
        Arc::new(Queue {
            state: Mutex::new(QueueState::default()),
            wake: Notify::new(),
            wake_blocking: Condvar::new(),
        })
    }

    /// Queues `item` without waiting; merges output into a queued chunk
    /// when it fits.
    fn offer(&self, item: StreamItem) -> Offered {
        let mut state = lock(&self.state);
        if state.receiver_gone {
            return Offered::Closed;
        }
        let was_empty = state.items.is_empty();
        if let StreamItem::Output(chunk) = &item
            && let Some(tail) = state.items.back_mut()
            && tail.merge(chunk)
        {
            return Offered::Queued;
        }
        if state.items.len() >= SUBSCRIBER_CAPACITY {
            return Offered::Full;
        }
        state.items.push_back(match item {
            StreamItem::Output(chunk) => Queued::Output(chunk),
            other => Queued::Other(other),
        });
        drop(state);
        // The receiver waits only on an empty queue.
        if was_empty {
            self.wake.notify_one();
            self.wake_blocking.notify_one();
        }
        Offered::Queued
    }

    fn close_sender(&self) {
        lock(&self.state).sender_gone = true;
        self.wake.notify_one();
        self.wake_blocking.notify_one();
    }

    fn close_receiver(&self) {
        let mut state = lock(&self.state);
        state.receiver_gone = true;
        state.items.clear();
    }

    /// `Some(Some(item))`, `Some(None)` once the sender is gone and the
    /// queue drained, or `None` while it is empty.
    fn take(state: &mut QueueState) -> Option<Option<Queued>> {
        match state.items.pop_front() {
            Some(item) => Some(Some(item)),
            None if state.sender_gone => Some(None),
            None => None,
        }
    }

    fn try_recv(&self) -> Option<Option<StreamItem>> {
        let taken = Self::take(&mut lock(&self.state));
        taken.map(|item| item.map(Queued::into_item))
    }

    async fn recv(&self) -> Option<StreamItem> {
        loop {
            if let Some(item) = self.try_recv() {
                return item;
            }
            self.wake.notified().await;
        }
    }

    fn blocking_recv(&self) -> Option<StreamItem> {
        let mut state = lock(&self.state);
        loop {
            if let Some(item) = Self::take(&mut state) {
                drop(state);
                return item.map(Queued::into_item);
            }
            state = self
                .wake_blocking
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Queued {
    /// Appends `chunk` if this is output with room for it; the first merge
    /// copies the item into a buffer of exactly [`COALESCE_BYTES`], so later
    /// ones never reallocate.
    fn merge(&mut self, chunk: &[u8]) -> bool {
        match self {
            Queued::Output(first) if first.len() + chunk.len() <= COALESCE_BYTES => {
                let mut merged = BytesMut::with_capacity(COALESCE_BYTES);
                merged.extend_from_slice(first);
                merged.extend_from_slice(chunk);
                *self = Queued::Merged(merged);
                true
            }
            Queued::Merged(merged) if merged.len() + chunk.len() <= COALESCE_BYTES => {
                merged.extend_from_slice(chunk);
                true
            }
            _ => false,
        }
    }

    fn into_item(self) -> StreamItem {
        match self {
            Queued::Output(chunk) => StreamItem::Output(chunk),
            Queued::Merged(merged) => StreamItem::Output(merged.freeze()),
            Queued::Other(item) => item,
        }
    }

    /// Heap bytes this item holds for its output.
    #[cfg(test)]
    fn output_bytes(&self) -> usize {
        match self {
            Queued::Output(chunk) => chunk.len(),
            Queued::Merged(merged) => merged.capacity(),
            Queued::Other(_) => 0,
        }
    }
}

/// Receiving end of a subscription; dropping it detaches.
pub struct Subscription {
    queue: Arc<Queue>,
    lagged: Arc<AtomicBool>,
    done: bool,
    /// The pipeline and this subscriber's key in it, to unsubscribe eagerly
    /// on drop (so the primary role moves on at once).
    owner: Option<(Weak<Mutex<State>>, u64)>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.queue.close_receiver();
        if let Some((state, key)) = self.owner.take()
            && let Some(state) = state.upgrade()
        {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pos) = state.subscribers.iter().position(|s| s.key == key) {
                state.subscribers.remove(pos);
                if state.primary == Some(key) {
                    state.elect_primary();
                }
            }
        }
    }
}

impl Subscription {
    /// Items queued and the heap bytes their output holds.
    #[cfg(test)]
    fn queued(&self) -> (usize, usize) {
        let state = lock(&self.queue.state);
        let bytes = state.items.iter().map(Queued::output_bytes).sum();
        (state.items.len(), bytes)
    }

    /// A stream that holds only [`StreamItem::Exited`].
    fn exited(code: i32) -> Self {
        let queue = Queue::new();
        queue.offer(StreamItem::Exited(code));
        queue.close_sender();
        Subscription {
            queue,
            lagged: Arc::new(AtomicBool::new(false)),
            done: false,
            owner: None,
        }
    }

    /// Next item; `None` once the stream has ended.
    pub async fn recv(&mut self) -> Option<StreamItem> {
        if let Some(item) = self.check_lagged() {
            return item;
        }
        let item = self.queue.recv().await;
        self.after_recv(item)
    }

    /// Blocking variant of [`Subscription::recv`]; must not be called from
    /// within an async runtime.
    pub fn blocking_recv(&mut self) -> Option<StreamItem> {
        if let Some(item) = self.check_lagged() {
            return item;
        }
        let item = self.queue.blocking_recv();
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
            // queue may still mean "lagged".
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
    queue: Arc<Queue>,
    lagged: Arc<AtomicBool>,
    /// Unique within the pipeline.
    key: u64,
    /// `None` for an anonymous subscriber ([`AgentPipeline::subscribe`]),
    /// which takes no part in the primary rule.
    conn: Option<ConnId>,
    /// When the connection last attached or sent input (a [`State::tick`]).
    seen: u64,
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        self.queue.close_sender();
    }
}

impl Subscriber {
    /// Delivers `item`; `false` if the subscriber is gone (closed or lagged).
    fn offer(&self, item: StreamItem) -> bool {
        match self.queue.offer(item) {
            Offered::Queued => true,
            Offered::Full => {
                self.lagged.store(true, Ordering::Release);
                false
            }
            Offered::Closed => false,
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
    /// The primary subscriber's key; `None` while no connection is attached.
    primary: Option<u64>,
    /// Source of subscriber keys and recency stamps.
    clock: u64,
    /// Output arrived since the activity ticker last looked.
    activity_dirty: bool,
    /// When output last arrived (Unix ms).
    last_output_unix_ms: i64,
}

impl State {
    /// The last non-blank visible row, trimmed to [`ACTIVITY_LINE_CHARS`].
    fn last_line(&self) -> String {
        let mut line = self.term.last_nonblank_line().unwrap_or_default();
        if let Some((cut, _)) = line.char_indices().nth(ACTIVITY_LINE_CHARS) {
            line.truncate(cut);
        }
        line
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn subscriber(&self, key: u64) -> Option<&Subscriber> {
        self.subscribers.iter().find(|s| s.key == key)
    }

    /// Makes the connection subscriber that most recently attached or sent
    /// input primary and tells it so; a subscriber that cannot take the
    /// message is dropped (lagged) and the next one tried.
    fn elect_primary(&mut self) {
        loop {
            let next = self
                .subscribers
                .iter()
                .enumerate()
                .filter(|(_, s)| s.conn.is_some())
                .max_by_key(|(_, s)| s.seen)
                .map(|(i, _)| i);
            let Some(i) = next else {
                self.primary = None;
                return;
            };
            if self.subscribers[i].offer(StreamItem::Role(true)) {
                self.primary = Some(self.subscribers[i].key);
                return;
            }
            self.subscribers.remove(i);
        }
    }

    /// Tells the current primary (if any) it no longer is; a subscriber
    /// that cannot take the message is dropped (lagged).
    fn demote_primary(&mut self) {
        if let Some(key) = self.primary.take()
            && let Some(i) = self.subscribers.iter().position(|s| s.key == key)
            && !self.subscribers[i].offer(StreamItem::Role(false))
        {
            self.subscribers.remove(i);
        }
    }

    /// After subscribers were dropped: elects a new primary if the old one
    /// went.
    fn check_primary(&mut self) {
        if let Some(key) = self.primary
            && self.subscriber(key).is_none()
        {
            self.elect_primary();
        }
    }

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
    /// The attaching connection is now primary (never for an anonymous
    /// subscriber or after exit).
    pub primary: bool,
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
                primary: None,
                clock: 0,
                activity_dirty: false,
                last_output_unix_ms: 0,
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
        let a = self.attach_as(None);
        (a.snapshot, a.size, a.subscription)
    }

    /// [`AgentPipeline::subscribe_sized`] for connection `conn`, plus the
    /// history tail taken in the same critical section: the tail ends where
    /// the snapshot's screen begins. Replaces the connection's previous
    /// subscription, and makes the connection primary (the previous primary
    /// gets [`StreamItem::Role`]`(false)`).
    pub fn attach(&self, conn: ConnId) -> Attached {
        self.attach_as(Some(conn))
    }

    /// A read-only [`AgentPipeline::attach`]: an anonymous subscriber that
    /// takes no part in the primary rule (it is never primary, changes no
    /// role and gets no [`StreamItem::Role`]).
    pub fn observe(&self) -> Attached {
        self.attach_as(None)
    }

    fn attach_as(&self, conn: Option<ConnId>) -> Attached {
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
        let queue = Queue::new();
        let lagged = Arc::new(AtomicBool::new(false));
        let mut owner = None;
        let mut primary = false;
        match state.exit_code {
            Some(code) => {
                queue.offer(StreamItem::Exited(code));
                queue.close_sender();
            }
            None => {
                let key = state.tick();
                if let Some(conn) = conn {
                    // The connection's previous subscription ends (its
                    // stream closes); a primary stays primary.
                    if let Some(i) = state.subscribers.iter().position(|s| s.conn == Some(conn)) {
                        let old = state.subscribers.remove(i);
                        if state.primary == Some(old.key) {
                            state.primary = None;
                        }
                    }
                    state.demote_primary();
                    state.primary = Some(key);
                    primary = true;
                }
                state.subscribers.push(Subscriber {
                    queue: Arc::clone(&queue),
                    lagged: Arc::clone(&lagged),
                    key,
                    conn,
                    seen: key,
                });
                owner = Some((Arc::downgrade(&self.state), key));
            }
        }
        let subscription = Subscription {
            queue,
            lagged,
            done: false,
            owner,
        };
        Attached {
            snapshot,
            size: state.term.size(),
            history,
            subscription,
            primary,
        }
    }

    /// Records input from `conn`: if it is attached, it becomes primary (the
    /// previous primary gets [`StreamItem::Role`]`(false)`, it `true`). Input
    /// from a connection that is not attached changes no role.
    pub fn input_from(&self, conn: ConnId) {
        let mut state = self.lock();
        let Some(i) = state.subscribers.iter().position(|s| s.conn == Some(conn)) else {
            return;
        };
        let seen = state.tick();
        state.subscribers[i].seen = seen;
        let key = state.subscribers[i].key;
        if state.primary == Some(key) {
            return;
        }
        state.demote_primary();
        // Demoting may have dropped a lagged subscriber, so look it up again.
        if let Some(i) = state.subscribers.iter().position(|s| s.key == key) {
            if state.subscribers[i].offer(StreamItem::Role(true)) {
                state.primary = Some(key);
            } else {
                state.subscribers.remove(i);
                state.elect_primary();
            }
        } else {
            state.elect_primary();
        }
    }

    /// Whether a resize from `conn` applies: it is primary, or no
    /// connection is attached.
    pub fn may_resize(&self, conn: ConnId) -> bool {
        let state = self.lock();
        match state.primary {
            None => true,
            Some(key) => state.subscriber(key).and_then(|s| s.conn) == Some(conn),
        }
    }

    /// Clears the activity mark and, if there was output since the last
    /// call, publishes the agent's activity to `sinks`: its last non-blank
    /// visible row (trimmed, at most [`ACTIVITY_LINE_CHARS`] chars) and when
    /// output last arrived. Nothing is published after exit, or while there
    /// are no receivers. Publishing under the pipeline lock orders it before
    /// [`AgentPipeline::exit`]'s purge.
    fn publish_activity(&self, agent_id: u64, sinks: &ActivitySinks) {
        let mut state = self.lock();
        if !std::mem::take(&mut state.activity_dirty) || state.exit_code.is_some() || !sinks.any() {
            return;
        }
        sinks.publish(AgentActivity {
            agent_id,
            last_line: state.last_line(),
            last_output_unix_ms: state.last_output_unix_ms,
        });
    }

    /// The agent's last non-blank visible row and when output last arrived
    /// (Unix ms; 0: none yet). `None` after exit. For the inferred-status
    /// sweep (ADR 0088): a read under the pipeline lock, nothing is kept.
    pub fn activity(&self) -> Option<(String, i64)> {
        let state = self.lock();
        state
            .exit_code
            .is_none()
            .then(|| (state.last_line(), state.last_output_unix_ms))
    }

    /// When output last arrived (Unix ms; 0: none yet), without the row.
    pub fn last_output_unix_ms(&self) -> i64 {
        self.lock().last_output_unix_ms
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

    /// The current screen and grid size for a successor daemon, with the
    /// scrollback and history flushed to disk, where the successor reopens
    /// them. Call with the host frozen: no output may follow.
    fn hand_off(&self) -> io::Result<(Vec<u8>, (u16, u16))> {
        let mut state = self.lock();
        let snapshot = Self::snapshot_of(&mut state);
        if let Some(scrollback) = &mut state.scrollback {
            scrollback.flush()?;
        }
        if let Some(history) = &mut state.history {
            history.flush()?;
        }
        Ok((snapshot, state.term.size()))
    }

    /// Replays a predecessor daemon's snapshot of the agent's screen into
    /// the grid, before any output. The bytes are already in the scrollback
    /// ring and the history, and were seen by every client, so they reach
    /// neither, nor subscribers or activity; rows the replay scrolls off are
    /// dropped. Query replies the replay generates are discarded: the agent
    /// asked its question of the old daemon, which answered it then (or left
    /// it to a client).
    pub fn restore_screen(&self, bytes: &[u8]) {
        let mut state = self.lock();
        state.term.feed(bytes);
        drop(state.term.drain_history());
        drop(state.term.take_replies());
        // A snapshot may end inside a sequence (its unterminated tail): the
        // next attach's snapshot must carry it on.
        state.partial.feed(bytes);
    }

    fn snapshot_of(state: &mut State) -> Vec<u8> {
        state.term.flush_pending_sync();
        state.drain_history();
        let mut snapshot = state.term.snapshot();
        snapshot.extend_from_slice(state.partial.tail());
        snapshot
    }

    /// Resizes the grid. Call alongside [`PtyHost::resize`](crate::PtyHost::resize).
    /// A change of size is queued to every subscriber as
    /// [`StreamItem::Resized`], in order with the output (under the lock that
    /// also covers feeding the grid).
    pub fn resize(&self, cols: u16, rows: u16) {
        let mut state = self.lock();
        let old = state.term.size();
        state.term.resize(cols, rows);
        state.drain_history();
        let (cols, rows) = state.term.size();
        if (cols, rows) != old && !state.subscribers.is_empty() {
            let before = state.subscribers.len();
            state
                .subscribers
                .retain(|s| s.offer(StreamItem::Resized { cols, rows }));
            if state.subscribers.len() != before {
                state.check_primary();
            }
        }
    }

    /// The agent's exit code, once it has exited.
    pub fn exit_code(&self) -> Option<i32> {
        self.lock().exit_code
    }

    /// What a search of the agent's output reads first, under one lock:
    /// the screen rows and where its history lies (ADR 0098).
    fn search_view(&self) -> (Vec<String>, Option<HistoryInfo>) {
        let state = self.lock();
        (
            state.term.visible_text(),
            state.history.as_ref().map(History::info),
        )
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

impl AgentPipeline {
    /// Feeds PTY output through the pipeline.
    fn feed(&self, bytes: &[u8]) -> Fed {
        let mut marked = false;
        let mut lagged = Vec::new();
        let reply = {
            let mut state = self.lock();
            state.last_output_unix_ms = unix_ms_now();
            if !state.activity_dirty {
                state.activity_dirty = true;
                marked = true;
            }
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
                let before = state.subscribers.len();
                state.subscribers.retain(|s| {
                    let kept = s.offer(StreamItem::Output(chunk.clone()));
                    if !kept && s.lagged.load(Ordering::Relaxed) {
                        lagged.push(s.conn);
                    }
                    kept
                });
                if state.subscribers.len() != before {
                    state.check_primary();
                }
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
        Fed { marked, lagged }
    }
}

/// What [`AgentPipeline::feed`] did beyond feeding.
struct Fed {
    /// The first output since the activity ticker last looked.
    marked: bool,
    /// Subscribers this output dropped because their queue was full (their
    /// connection; `None` for an anonymous one). Their client learns of the
    /// lag only when it reads its way to the end of the queue, which a
    /// stalled client may never do.
    lagged: Vec<Option<ConnId>>,
}

impl OutputSink for AgentPipeline {
    fn on_spawn(&mut self, input: AgentInput) {
        self.lock().input = Some(input);
    }

    fn on_output(&mut self, bytes: &[u8]) {
        self.feed(bytes);
    }

    fn on_exit(&mut self, code: i32) {
        self.exit(code, || {});
    }
}

impl AgentPipeline {
    /// Records the exit: flushes and closes history and scrollback, then
    /// runs `before_notify` (after the exit code is set, under the pipeline
    /// lock) and ends every subscriber's stream with
    /// [`StreamItem::Exited`].
    fn exit(&self, code: i32, before_notify: impl FnOnce()) {
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
            warn!(error = %e, "flushing agent history at exit failed");
        }
        state.exit_code = Some(code);
        state.input = None;
        state.primary = None;
        before_notify();
        for sub in std::mem::take(&mut state.subscribers) {
            sub.offer(StreamItem::Exited(code));
        }
    }
}

/// Exit events a [`Node`] buffers per receiver before it lags.
pub const EXIT_EVENT_CAPACITY: usize = 256;

/// The activity ticker looks at most this often, so each agent yields at
/// most one [`AgentActivity`] per tick.
pub const ACTIVITY_TICK: Duration = Duration::from_millis(250);
/// Longest `AgentActivity.last_line`, in chars.
pub const ACTIVITY_LINE_CHARS: usize = 200;
/// The ticker's stack: it only reads grid rows and sends events.
const ACTIVITY_STACK_BYTES: usize = 256 * 1024;

fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// One receiver's pending activity: the latest event per agent, so a
/// receiver that falls behind loses nothing but superseded values.
struct ActivitySink {
    pending: Mutex<HashMap<u64, AgentActivity>>,
    notify: tokio::sync::Notify,
}

/// Receives agents' activity (see [`Node::activity_events`]). Holds at most
/// one pending event per running agent; dropping it unsubscribes.
pub struct ActivityReceiver {
    sink: Arc<ActivitySink>,
}

impl ActivityReceiver {
    /// Waits until activity may be pending (spurious wake-ups possible).
    pub async fn changed(&self) {
        self.sink.notify.notified().await;
    }

    /// Removes one pending event and hands it to `deliver`, with the pending
    /// set locked: an agent's exit cannot overtake an event delivered here
    /// (as long as `deliver` enqueues it without waiting). `false` if none
    /// is pending.
    pub fn deliver_next(&self, deliver: impl FnOnce(AgentActivity)) -> bool {
        let mut pending = lock(&self.sink.pending);
        let Some(&id) = pending.keys().next() else {
            return false;
        };
        let activity = pending.remove(&id).expect("key just seen");
        deliver(activity);
        true
    }
}

/// The live [`ActivityReceiver`]s.
#[derive(Default)]
struct ActivitySinks {
    list: Mutex<Vec<Weak<ActivitySink>>>,
}

impl ActivitySinks {
    fn subscribe(&self) -> ActivityReceiver {
        let sink = Arc::new(ActivitySink {
            pending: Mutex::new(HashMap::new()),
            notify: tokio::sync::Notify::new(),
        });
        let mut list = lock(&self.list);
        list.retain(|s| s.strong_count() > 0);
        list.push(Arc::downgrade(&sink));
        ActivityReceiver { sink }
    }

    /// Whether any receiver is live.
    fn any(&self) -> bool {
        lock(&self.list).iter().any(|s| s.strong_count() > 0)
    }

    /// Replaces the agent's pending event in every receiver.
    fn publish(&self, activity: AgentActivity) {
        let mut list = lock(&self.list);
        list.retain(|s| match s.upgrade() {
            Some(sink) => {
                lock(&sink.pending).insert(activity.agent_id, activity.clone());
                sink.notify.notify_one();
                true
            }
            None => false,
        });
    }

    /// Drops the agent's pending event from every receiver.
    fn forget(&self, agent_id: u64) {
        for sink in lock(&self.list).iter().filter_map(Weak::upgrade) {
            lock(&sink.pending).remove(&agent_id);
        }
    }
}

/// Agents with output since the activity ticker last looked, and the
/// ticker's wake-up.
struct ActivityHub {
    /// Queued by the first output after each tick.
    dirty: Mutex<Vec<(u64, AgentPipeline)>>,
    sinks: ActivitySinks,
    /// Started by the first output, so an idle node has no ticker thread;
    /// `None` if it could not be started (no activity events then).
    ticker: OnceLock<Option<Thread>>,
    stop: AtomicBool,
}

impl ActivityHub {
    fn new() -> Self {
        ActivityHub {
            dirty: Mutex::new(Vec::new()),
            sinks: ActivitySinks::default(),
            ticker: OnceLock::new(),
            stop: AtomicBool::new(false),
        }
    }

    /// Queues the agent; wakes the ticker (starting it the first time) if
    /// it was idle.
    fn mark(self: &Arc<Self>, id: u64, pipeline: &AgentPipeline) {
        let mut dirty = lock(&self.dirty);
        let was_idle = dirty.is_empty();
        dirty.push((id, pipeline.clone()));
        drop(dirty);
        if was_idle && let Some(ticker) = self.ticker.get_or_init(|| self.start_ticker()) {
            ticker.unpark();
        }
    }

    fn start_ticker(self: &Arc<Self>) -> Option<Thread> {
        let hub = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("fleet-activity".into())
            .stack_size(ACTIVITY_STACK_BYTES)
            .spawn(move || hub.run());
        match spawned {
            Ok(handle) => Some(handle.thread().clone()),
            Err(e) => {
                warn!(error = %e, "starting the activity ticker failed");
                None
            }
        }
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(Some(ticker)) = self.ticker.get() {
            ticker.unpark();
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// The ticker: parked while no agent is queued; otherwise, at most once
    /// per [`ACTIVITY_TICK`], reads each queued agent's activity and
    /// publishes it (only the marks are cleared while nobody listens).
    fn run(&self) {
        let mut last_tick: Option<Instant> = None;
        let mut batch = Vec::new();
        loop {
            while lock(&self.dirty).is_empty() {
                if self.stopped() {
                    return;
                }
                std::thread::park();
            }
            if let Some(last) = last_tick {
                let due = last + ACTIVITY_TICK;
                loop {
                    let now = Instant::now();
                    if self.stopped() {
                        return;
                    }
                    if now >= due {
                        break;
                    }
                    std::thread::park_timeout(due - now);
                }
            }
            std::mem::swap(&mut *lock(&self.dirty), &mut batch);
            last_tick = Some(Instant::now());
            for (agent_id, pipeline) in batch.drain(..) {
                pipeline.publish_activity(agent_id, &self.sinks);
            }
        }
    }
}

/// How long [`Node::shutdown`] lets agents exit after SIGTERM.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// How long [`Node::kill`] lets an agent exit after SIGHUP and SIGTERM
/// before it SIGKILLs the process group (ADR 0071).
pub const KILL_GRACE: Duration = Duration::from_secs(2);
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

/// The agent's own id, set by the node in every agent's environment.
pub const AGENT_ID_ENV: &str = "FLEET_AGENT_ID";

/// The agent's environment: the request's, `TERM` defaulted,
/// [`AGENT_ID_ENV`] set to `id` and `FLEET_AGENT_TOKEN` to its token (ADR
/// 0072). The node sets both last, so a spawn request cannot make an agent
/// claim another's id or token (ADR 0025).
fn agent_env(env: HashMap<String, String>, id: u64, token: &AgentToken) -> Vec<(String, String)> {
    let mut env: Vec<_> = env
        .into_iter()
        .filter(|(k, _)| k != AGENT_ID_ENV && k != AGENT_TOKEN_ENV)
        .collect();
    if !env.iter().any(|(k, _)| k == "TERM") {
        env.push(("TERM".into(), DEFAULT_TERM.into()));
    }
    env.push((AGENT_ID_ENV.into(), id.to_string()));
    env.push((AGENT_TOKEN_ENV.into(), token.expose().to_owned()));
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

/// Runs `f` when dropped, on a return or an unwind, unless disarmed.
struct OnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> OnDrop<F> {
    fn new(f: F) -> Self {
        Self(Some(f))
    }

    fn disarm(mut self) {
        self.0 = None;
    }
}

impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// Test hook (debug builds): with `FLEETD_TEST_PANIC_DESCRIBING` set,
/// describing the frozen agents for a handoff panics.
fn test_panic_describing() {
    #[cfg(debug_assertions)]
    if std::env::var_os("FLEETD_TEST_PANIC_DESCRIBING").is_some() {
        panic!("test hook FLEETD_TEST_PANIC_DESCRIBING");
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The longest `fleet report` message, in characters (spec §2).
pub const REPORT_MESSAGE_MAX: usize = 200;
/// The longest question, in characters (ADR 0078).
pub const QUESTION_MAX: usize = 500;
/// Most choices a question offers.
pub const CHOICES_MAX: usize = 6;
/// The longest choice, in characters.
pub const CHOICE_MAX: usize = 60;

/// The question a report asks (ADR 0078).
#[derive(Clone, Copy, Debug)]
pub struct QuestionIn<'a> {
    pub text: &'a str,
    pub choices: &'a [String],
}

impl QuestionIn<'_> {
    fn check(&self) -> io::Result<()> {
        let one_line = |what: &str, s: &str, max: usize| {
            if s.is_empty() {
                return Err(invalid(format!("{what} must not be empty")));
            }
            if s.chars().count() > max {
                return Err(invalid(format!("{what} is longer than {max} characters")));
            }
            if s.chars().any(char::is_control) {
                return Err(invalid(format!("{what} has control characters")));
            }
            Ok(())
        };
        one_line("the question", self.text, QUESTION_MAX)?;
        if self.choices.len() > CHOICES_MAX {
            return Err(invalid(format!("at most {CHOICES_MAX} choices")));
        }
        self.choices
            .iter()
            .try_for_each(|c| one_line("a choice", c, CHOICE_MAX))
    }
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
    fn delete(&mut self, id: u64, reason: &str) {
        self.open.retain(|(i, _)| *i != id);
        debug!(agent = id, reason, "deleting history");
        remove_history_dir(&self.path(id));
    }
}

fn remove_history_dir(path: &Path) {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => warn!(path = ?path, error = %e, "removing agent history failed"),
    }
}

/// Deletes the history dirs under `dir` of agents other than `keep`.
fn prune_history_dirs(dir: &Path, keep: &HashSet<u64>) -> io::Result<()> {
    let mut pruned = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let id = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u64>().ok());
        if id.is_some_and(|id| !keep.contains(&id)) {
            remove_history_dir(&entry.path());
            pruned += 1;
        }
    }
    if pruned > 0 {
        info!(agents = pruned, "deleted history beyond retention");
    }
    Ok(())
}

/// Deletes the `<id>.ring` files under `dir` whose id has no store row (a
/// forget or restart that failed to delete it, or a crash in between): rings
/// hold raw output, which may contain secrets.
fn prune_orphan_rings(dir: &Path, rows: &HashSet<u64>) -> io::Result<()> {
    let entries = std::fs::read_dir(dir)?.map(|e| e.map(|e| e.path()));
    prune_ring_paths(entries, rows);
    Ok(())
}

/// [`prune_orphan_rings`] over `entries`, best effort: an unreadable entry
/// is logged and skipped. Only canonical names (`7.ring`, not `007.ring`)
/// are rings.
fn prune_ring_paths(entries: impl IntoIterator<Item = io::Result<PathBuf>>, rows: &HashSet<u64>) {
    let mut pruned = 0usize;
    for entry in entries {
        let path = match entry {
            Ok(path) => path,
            Err(e) => {
                warn!(error = %e, "reading a scrollback directory entry failed");
                continue;
            }
        };
        let id = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".ring"))
            .and_then(|n| n.parse::<u64>().ok().filter(|id| id.to_string() == n));
        if id.is_some_and(|id| !rows.contains(&id)) {
            match std::fs::remove_file(&path) {
                Ok(()) => pruned += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(path = ?path, error = %e, "removing orphan scrollback failed")
                }
            }
        }
    }
    if pruned > 0 {
        info!(rings = pruned, "deleted scrollback of unknown agents");
    }
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
    host: Arc<dyn AgentHost>,
    agents: Arc<Mutex<Agents>>,
    histories: Arc<Mutex<Histories>>,
    exits: broadcast::Sender<AgentExited>,
    activity: Arc<ActivityHub>,
    notices: broadcast::Sender<Notice>,
}

impl OutputSink for NodeSink {
    fn on_spawn(&mut self, input: AgentInput) {
        self.pipeline.on_spawn(input);
    }

    fn on_output(&mut self, bytes: &[u8]) {
        let fed = self.pipeline.feed(bytes);
        for conn in fed.lagged {
            warn!(
                agent = self.id,
                conn = conn.map(|c| c.0),
                "subscriber dropped: its queue is full"
            );
        }
        if fed.marked {
            self.activity.mark(self.id, &self.pipeline);
        }
    }

    fn on_exit(&mut self, code: i32) {
        // The store first, so anyone who sees the exit also lists it.
        info!(agent = self.id, code, "agent exited");
        if let Err(e) = lock(&self.store).set_exited(self.id, code) {
            warn!(agent = self.id, error = %e, "recording the exit failed");
        }
        // The parent's report (ADR 0076), after the exit is recorded.
        mailbox::report_exit(&self.store, &self.notices, self.id, code);
        // No activity for the agent after its exit: pending events are
        // dropped before its stream ends, and none are published after.
        let activity = &self.activity;
        self.pipeline.exit(code, || activity.sinks.forget(self.id));
        // If the spawn has not registered the agent yet, it retires it.
        let mut agents = lock(&self.agents);
        let evicted = match agents.live.remove(&self.id) {
            Some(live) => agents.retire(&*self.host, self.id, &live, code),
            None => None,
        };
        drop(agents);
        if let Some(evicted) = evicted {
            lock(&self.histories).delete(evicted, "beyond retention");
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
    host: Arc<dyn AgentHost>,
    store: Arc<Mutex<Store>>,
    agents: Arc<Mutex<Agents>>,
    exits: broadcast::Sender<AgentExited>,
    removed: broadcast::Sender<Removed>,
    scrollback_dir: PathBuf,
    histories: Arc<Mutex<Histories>>,
    activity: Arc<ActivityHub>,
    /// The base of every agent's environment.
    base_env: LoginEnv,
    /// The node's agent priority setting (ADR 0066): how much lower than
    /// the daemon agents run by default; 0 is off.
    agent_nice: u8,
    /// Holds spawns, restarts and forgets out of a handoff.
    handoff: HandoffGate,
    /// Limits on agents' spawns (spec §1).
    quotas: Quotas,
    /// Held by an agent's spawn from its quota check until its row is
    /// stored, so two spawns cannot both take the last slot.
    spawn_gate: Mutex<()>,
    refused: broadcast::Sender<SpawnRefused>,
    reported: broadcast::Sender<AgentReported>,
    /// Named locks (ADR 0077).
    leases: Leases,
    /// Mailbox notices for waiters (ADR 0076); never on the output path.
    notices: broadcast::Sender<Notice>,
    /// Agents' git checkouts, by cwd (ADR 0085).
    git: GitCache,
    /// Agents' spawns waiting for the user (ADR 0079).
    approvals: Approvals,
    /// The inferred-`waiting` sweep (ADR 0088).
    sweeper: Arc<waiting::Sweeper>,
}

impl Drop for Node {
    /// Stops the activity ticker and the waiting sweep.
    fn drop(&mut self) {
        self.activity.stop();
        self.sweeper.stop();
    }
}

/// An agent removed from the node, as announced by [`Node::removed_events`].
#[derive(Clone, Debug, PartialEq)]
pub struct Removed {
    pub event: AgentRemoved,
    /// The connection whose request caused the removal, if any.
    pub origin: Option<ConnId>,
}

/// The result of [`Node::forget`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForgetOutcome {
    pub forgotten: Vec<u64>,
    /// Running, unknown or being-restarted agents, left untouched.
    pub refused: Vec<u64>,
}

/// [`Node::forget`] failed: `partial` is what it did before the error
/// (each id in `partial.forgotten` was removed and announced).
#[derive(Debug)]
pub struct ForgetError {
    pub error: io::Error,
    pub partial: ForgetOutcome,
}

impl std::fmt::Display for ForgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for ForgetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl From<io::Error> for ForgetError {
    fn from(error: io::Error) -> Self {
        ForgetError {
            error,
            partial: ForgetOutcome::default(),
        }
    }
}

impl From<ForgetError> for io::Error {
    fn from(e: ForgetError) -> Self {
        e.error
    }
}

/// The result of [`Node::restart`].
#[derive(Clone, Debug, PartialEq)]
pub struct RestartOutcome {
    /// The new agent.
    pub info: AgentInfo,
    /// The store recorded the new agent as replacing the old one: the old
    /// one is forgotten and its [`Removed`] announced. `false` if recording
    /// failed: both agents stay known and nothing is announced.
    pub replaced: bool,
}

#[derive(Default)]
struct Agents {
    /// Running agents.
    live: HashMap<u64, LiveAgent>,
    /// The last [`RETAINED_EXITED`] exits, oldest first.
    exited: VecDeque<ExitedAgent>,
    /// Set by [`Node::shutdown`]: no new agents.
    closing: bool,
    /// Agents being restarted ([`Node::restart`]): neither restarted again
    /// nor forgotten meanwhile.
    restarting: HashSet<u64>,
}

/// Removes an agent from [`Agents::restarting`] when the restart ends.
struct Restarting<'a> {
    agents: &'a Mutex<Agents>,
    id: u64,
}

impl Drop for Restarting<'_> {
    fn drop(&mut self) {
        lock(self.agents).restarting.remove(&self.id);
    }
}

impl Agents {
    /// Keeps an exited agent's final screen (evicting the oldest beyond
    /// [`RETAINED_EXITED`]) and drops everything else it held. Returns the
    /// evicted agent, whose history the caller deletes.
    #[must_use]
    fn retire(
        &mut self,
        host: &dyn AgentHost,
        id: u64,
        live: &LiveAgent,
        exit_code: i32,
    ) -> Option<u64> {
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

    /// Lists an agent the host now runs as live, or retires it if it has
    /// already exited (its sink found it unregistered). Returns the agent
    /// evicted from retention, whose history the caller deletes.
    #[must_use]
    fn register(&mut self, host: &dyn AgentHost, id: u64, live: LiveAgent) -> Option<u64> {
        match live.pipeline.exit_code() {
            Some(code) => self.retire(host, id, &live, code),
            None => {
                self.live.insert(id, live);
                None
            }
        }
    }
}

/// Runs with a spawn-attached agent's id before its process starts.
type BeforeStart<'a> = Box<dyn FnOnce(u64) + 'a>;

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
    /// recently created of them, and the scrollback rings of agents with no
    /// store row.
    pub fn open(state_dir: &Path) -> io::Result<Node> {
        Self::open_sparing(state_dir, &[])
    }

    /// [`Node::open`], sparing the agents `keep` from being marked lost.
    fn open_sparing(state_dir: &Path, keep: &[u64]) -> io::Result<Node> {
        let scrollback_dir = state_dir.join("scrollback");
        let history_dir = state_dir.join("history");
        for dir in [&scrollback_dir, &history_dir] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let store = Store::open(&state_dir.join("state.db")).map_err(store_err)?;
        store.mark_running_as_lost_except(keep).map_err(store_err)?;
        // A corrupt row's history is not kept (its id is unknown).
        let (records, _) = store.list_agents_lossy_unlabeled().map_err(store_err)?;
        prune_history_dirs(&history_dir, &retained_history(&records))?;
        prune_orphan_rings(&scrollback_dir, &store.agent_ids().map_err(store_err)?)?;
        let (exits, _) = broadcast::channel(EXIT_EVENT_CAPACITY);
        let (removed, _) = broadcast::channel(EXIT_EVENT_CAPACITY);
        let (refused, _) = broadcast::channel(EXIT_EVENT_CAPACITY);
        let (reported, _) = broadcast::channel(EXIT_EVENT_CAPACITY);
        let (notices, _) = broadcast::channel(mailbox::NOTICE_CAPACITY);
        let activity = Arc::new(ActivityHub::new());
        let store = Arc::new(Mutex::new(store));
        let leases = Leases::load(Arc::clone(&store), now_ms(), load_average)?;
        let agents = Arc::new(Mutex::new(Agents::default()));
        Ok(Node {
            host: Arc::new(PtyHost::new()),
            store: Arc::clone(&store),
            agents: Arc::clone(&agents),
            exits,
            removed,
            scrollback_dir,
            histories: Arc::new(Mutex::new(Histories {
                dir: history_dir,
                open: VecDeque::new(),
                spawning: HashSet::new(),
            })),
            activity,
            base_env: LoginEnv::daemon(),
            agent_nice: 0,
            handoff: HandoffGate::default(),
            quotas: Quotas::default(),
            spawn_gate: Mutex::new(()),
            refused,
            reported,
            leases,
            git: GitCache::default(),
            approvals: Approvals::new(),
            sweeper: Arc::new(waiting::Sweeper::new(
                Arc::clone(&agents),
                Arc::clone(&store),
                notices.clone(),
            )),
            notices,
        })
    }

    /// The node's leases (ADR 0077).
    pub fn leases(&self) -> &Leases {
        &self.leases
    }

    /// Marks every agent the store under `state_dir` has running as lost,
    /// without opening the node; how many. For a daemon that took over a
    /// predecessor's agents and ended them before it could open its node (a
    /// failed handoff-origin start), so the store does not say they run.
    pub fn mark_left_running_lost(state_dir: &Path) -> io::Result<usize> {
        let store = Store::open(&state_dir.join("state.db")).map_err(store_err)?;
        store.mark_running_as_lost().map_err(store_err)
    }

    /// Uses `env` as the base of agents' environments instead of the
    /// daemon's own. The first spawn waits for a capture still running.
    pub fn with_base_env(mut self, env: LoginEnv) -> Node {
        self.base_env = env;
        self
    }

    /// Lowers agents spawned from now on by `nice` (as `nice -n`; 0 is
    /// off), unless spawned with `AGENT_PRIORITY_NORMAL` (ADR 0066).
    pub fn with_agent_nice(mut self, nice: u8) -> Node {
        self.agent_nice = nice;
        self
    }

    /// Limits agents' spawns by `quotas` (the user's are never limited).
    pub fn with_quotas(mut self, quotas: Quotas) -> Node {
        self.quotas = quotas;
        self
    }

    /// Agents' spawns waiting for the user (ADR 0079).
    pub fn approvals(&self) -> &Approvals {
        &self.approvals
    }

    /// How long an agent's spawn waits for the user's approval, in
    /// seconds; `None` when agents' spawns need none (the default).
    pub fn spawn_approval(&self) -> Option<u32> {
        self.quotas
            .spawn_approval
            .then_some(self.quotas.spawn_approval_timeout_secs)
    }

    /// What an agent's spawn would be, for the user to approve (ADR 0079):
    /// the quotas are checked first (a refusal is announced as usual) and
    /// the labels narrowed, but nothing is started. The spawn itself checks
    /// again once approved. The pending spawn's id and times are unset.
    pub fn precheck_spawn(&self, caller: Caller, req: &SpawnAgent) -> io::Result<PendingSpawn> {
        let Caller::Agent(parent) = caller else {
            return Err(invalid("only an agent's spawn waits for approval".into()));
        };
        check_labels(&req.labels)?;
        let mut labels = req.labels.clone();
        drop(self.admit_spawn(caller, Some(&mut labels), None)?);
        Ok(PendingSpawn {
            parent,
            program: req.program.clone(),
            args: req.args.clone(),
            cwd: req.cwd.clone(),
            labels,
            ..Default::default()
        })
    }

    /// Receives every spawn refused by a quota from now on.
    pub fn refused_events(&self) -> broadcast::Receiver<SpawnRefused> {
        self.refused.subscribe()
    }

    /// Receives every status report from now on.
    pub fn report_events(&self) -> broadcast::Receiver<AgentReported> {
        self.reported.subscribe()
    }

    /// [`Node::report`] for the connection's `caller`: only an agent has a
    /// status to report, so the user's is `forbidden:` (ADR 0075).
    pub fn report_by(&self, caller: Caller, state: ReportedState, message: &str) -> io::Result<()> {
        self.report_question_by(caller, state, message, None)
    }

    /// [`Node::report_question`] for the connection's `caller`.
    pub fn report_question_by(
        &self,
        caller: Caller,
        state: ReportedState,
        message: &str,
        question: Option<QuestionIn<'_>>,
    ) -> io::Result<()> {
        match caller {
            Caller::Agent(id) => self.report_question(id, state, message, question),
            Caller::User => Err(scope::forbidden(
                "fleet report needs an agent identity (run it inside an agent)",
            )),
        }
    }

    /// Records agent `id`'s own status (ADR 0075, `fleet report`) and
    /// announces it. `message` is at most [`REPORT_MESSAGE_MAX`] characters
    /// without control characters. The agent must be running and not a plain
    /// shell (shells have no status, ADR 0071).
    pub fn report(&self, id: u64, state: ReportedState, message: &str) -> io::Result<()> {
        self.report_question(id, state, message, None)
    }

    /// [`Node::report`] that may ask the user a question (ADR 0078): only
    /// with `waiting`. The question goes to the user's mailbox with a copy
    /// to the parent, and stays open until it is answered or the agent
    /// reports again (any report closes the last one).
    pub fn report_question(
        &self,
        id: u64,
        state: ReportedState,
        message: &str,
        question: Option<QuestionIn<'_>>,
    ) -> io::Result<()> {
        if state == ReportedState::Unspecified {
            return Err(invalid("report needs a state".into()));
        }
        if message.chars().count() > REPORT_MESSAGE_MAX {
            return Err(invalid(format!(
                "report message is longer than {REPORT_MESSAGE_MAX} characters"
            )));
        }
        if message.chars().any(char::is_control) {
            return Err(invalid("report message has control characters".into()));
        }
        if let Some(q) = &question {
            if state != ReportedState::ReportedWaiting {
                return Err(invalid("a question needs the state waiting".into()));
            }
            q.check()?;
        }
        let store = lock(&self.store);
        let labels = store
            .labels(id)
            .map_err(store_err)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no agent {id}")))?;
        if labels.get(KIND_LABEL).map(String::as_str) == Some(SHELL_KIND) {
            return Err(invalid("a shell has no status to report".into()));
        }
        let not_running = |e| match e {
            rusqlite::Error::StatementChangedRows(_) => {
                invalid(format!("agent {id} is not running"))
            }
            e => store_err(e),
        };
        let (at_unix_ms, asked, to) = match &question {
            None => (
                store
                    .set_reported(id, state as i32, message)
                    .map_err(not_running)?,
                None,
                Vec::new(),
            ),
            Some(q) => {
                let (at, asked, to) = store
                    .set_reported_asking(id, state as i32, message, q.text, q.choices)
                    .map_err(not_running)?;
                (at, Some(asked), to)
            }
        };
        // `status_changed` takes the store lock itself.
        drop(store);
        // No receivers is fine.
        let _ = self.reported.send(AgentReported {
            agent_id: id,
            reported: Some(Reported {
                state: state as i32,
                message: message.to_owned(),
                at_unix_ms,
                question: asked.map(question_proto),
            }),
        });
        for recipient in to {
            let _ = self
                .notices
                .send(crate::mailbox::Notice::Message { recipient });
        }
        // The parent's mailbox and its waiters (ADR 0076's seam): one
        // `status` message per change.
        if let Some(status) = crate::mailbox::Status::of_reported(state) {
            self.status_changed(id, status, message, false)?;
        }
        Ok(())
    }

    /// The user's answer to agent `agent`'s open question (ADR 0078): an
    /// `answer` message to it and a copy to its parent, the question closed.
    /// `question` (the id the user saw) guards against answering a question
    /// that was closed and replaced meanwhile. The agent is then reported
    /// `working`: it has what it asked for. Only the user answers: an agent
    /// is `forbidden:`.
    pub fn answer(
        &self,
        caller: Caller,
        agent: u64,
        question: Option<u64>,
        text: &str,
    ) -> io::Result<()> {
        if let Caller::Agent(_) = caller {
            return Err(scope::forbidden("only the user answers questions"));
        }
        if text.is_empty() {
            return Err(invalid("an answer must not be empty".into()));
        }
        if text.len() > crate::mailbox::MAX_BODY {
            return Err(invalid(format!(
                "an answer is at most {} bytes (this one is {})",
                crate::mailbox::MAX_BODY,
                text.len()
            )));
        }
        let to = lock(&self.store)
            .answer_question(agent, question, text)
            .map_err(store_err)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("agent {agent} has no open question"),
                )
            })?;
        for recipient in to {
            let _ = self
                .notices
                .send(crate::mailbox::Notice::Message { recipient });
        }
        // Announces the closed question and wakes the parent.
        if let Err(e) = self.report(agent, ReportedState::ReportedWorking, "") {
            tracing::warn!(agent, error = %e, "reporting working after an answer failed");
        }
        Ok(())
    }

    /// Who a connection is (ADR 0072), from its peer's session id and the
    /// token its `Hello` carried: the running agent whose pid is the
    /// session, else the running agent the token belongs to, else the user.
    /// `Err` (the handshake is refused) when the two name different agents.
    /// A token that is no running agent's is ignored.
    ///
    /// A root shell (`kind=shell` without a parent: the user's own
    /// terminal, ADR 0071) is the user: its session and its token mean the
    /// user.
    pub fn resolve_caller(
        &self,
        session: Option<u32>,
        token: Option<&str>,
    ) -> Result<Caller, String> {
        let by_session = session
            .and_then(|sid| self.agent_with_pid(sid))
            .filter(|&id| !self.is_root_shell(id));
        let by_token = match token {
            None => None,
            // An unknown token is no token: one from another daemon (a
            // `--state-dir` of its own) or from an agent that has exited.
            Some(t) => lock(&self.store)
                .token_agent(&token::digest(t))
                .map_err(|e| format!("checking the agent token failed: {e}"))?,
        };
        let id = match (by_session, by_token) {
            (Some(a), Some(b)) if a != b => {
                return Err(format!(
                    "the caller's session belongs to agent {a} but its token to agent {b}"
                ));
            }
            (Some(id), _) | (None, Some(id)) => id,
            (None, None) => return Ok(Caller::User),
        };
        Ok(if self.is_root_shell(id) {
            Caller::User
        } else {
            Caller::Agent(id)
        })
    }

    /// The running agent whose process is `pid`.
    fn agent_with_pid(&self, pid: u32) -> Option<u64> {
        let agents = lock(&self.agents);
        agents
            .live
            .iter()
            .find(|(_, live)| self.host.pid(live.host_id) == Some(pid))
            .map(|(&id, _)| id)
    }

    fn is_root_shell(&self, id: u64) -> bool {
        match lock(&self.store).labels(id) {
            Ok(Some(labels)) => scope::is_root_shell(&labels),
            _ => false,
        }
    }

    /// The spawned-by tree of every agent the node knows (ADR 0072).
    pub fn tree(&self) -> io::Result<Tree> {
        let db = lock(&self.store);
        let (records, _) = db.list_agents_lossy().map_err(store_err)?;
        let replaced = db.replacements().map_err(store_err)?;
        drop(db);
        Ok(Tree::new(
            records.iter().map(|r| Member {
                id: r.id,
                labels: &r.labels,
                running: r.state == store::AgentState::Running,
            }),
            &replaced,
        ))
    }

    /// The agents `caller` may see whose labels contain every `selector`
    /// pair: all of them for the user, an agent's descendants for an agent.
    pub fn list_for(&self, caller: Caller, selector: &Labels) -> io::Result<Vec<AgentInfo>> {
        let mut agents = self.list_matching(selector)?;
        if let Caller::Agent(a) = caller {
            let mine = self.tree()?.descendants(a);
            agents.retain(|info| mine.contains(&info.id));
        }
        Ok(agents)
    }

    /// Admits a spawn (or, with `restarting`, a restart of that agent) by
    /// `caller`: nothing to check for the user. For an agent, refuses it
    /// past a quota (announcing a [`SpawnRefused`]) and narrows `labels`
    /// (a new agent's) to the parent's plus new keys, with `fleet.parent`.
    /// The guard holds other agents' spawns until this one's row is stored.
    fn admit_spawn(
        &self,
        caller: Caller,
        labels: Option<&mut Labels>,
        restarting: Option<u64>,
    ) -> io::Result<Option<MutexGuard<'_, ()>>> {
        let Caller::Agent(a) = caller else {
            return Ok(None);
        };
        let gate = lock(&self.spawn_gate);
        let tree = self.tree()?;
        // A restarted agent comes back under its own parent.
        let parent = match restarting {
            Some(old) => tree.parent(old).unwrap_or(a),
            None => a,
        };
        if let Err(reason) = tree.check_quota(&self.quotas, parent) {
            warn!(agent = a, reason = %reason, "spawn refused");
            // No receivers is fine.
            let _ = self.refused.send(SpawnRefused {
                parent: a,
                reason: reason.clone(),
            });
            return Err(scope::forbidden(reason));
        }
        if let Some(labels) = labels {
            let parent_labels = lock(&self.store)
                .labels(a)
                .map_err(store_err)?
                .unwrap_or_default();
            let narrowed = scope::child_labels(a, &parent_labels, std::mem::take(labels))?;
            if narrowed.len() > MAX_LABELS {
                return Err(invalid(format!(
                    "{} labels with the inherited ones (at most {MAX_LABELS})",
                    narrowed.len()
                )));
            }
            *labels = narrowed;
        }
        Ok(Some(gate))
    }

    /// Receives every agent exit from now on.
    pub fn exit_events(&self) -> broadcast::Receiver<AgentExited> {
        self.exits.subscribe()
    }

    /// Receives every agent removal ([`Node::forget`], [`Node::restart`])
    /// from now on.
    pub fn removed_events(&self) -> broadcast::Receiver<Removed> {
        self.removed.subscribe()
    }

    /// Receives agents' activity from now on: for each running agent with
    /// output, at most one event per [`ACTIVITY_TICK`] carrying its last
    /// non-blank visible row. Latest-value: a receiver that falls behind
    /// gets each agent's newest event (older ones are superseded), and
    /// nothing for an agent after its exit.
    pub fn activity_events(&self) -> ActivityReceiver {
        self.activity.sinks.subscribe()
    }

    /// Spawns an agent. Zero `cols`/`rows` mean 80x24; otherwise the size
    /// must pass the [`MAX_DIM`]/[`MAX_CELLS`] limits. An empty `cwd` means
    /// the daemon's working directory. `TERM` defaults to `xterm-256color`.
    /// The labels must pass the label limits, without reserved keys.
    /// `req.attach` is ignored (see [`Node::spawn_attached`]).
    pub fn spawn(&self, req: SpawnAgent) -> io::Result<AgentInfo> {
        self.spawn_by(Caller::User, req)
    }

    /// [`Node::spawn`] by `caller`: an agent's spawn becomes its child
    /// (`fleet.parent`, narrowed labels) and is subject to the quotas.
    pub fn spawn_by(&self, caller: Caller, mut req: SpawnAgent) -> io::Result<AgentInfo> {
        check_labels(&req.labels)?;
        let _gate = self.admit_spawn(caller, Some(&mut req.labels), None)?;
        self.start(req, None).map(|(info, _)| info)
    }

    /// [`Node::spawn`], attached by `conn` (which is primary): the
    /// subscription is taken before the process starts, so it carries all of
    /// the agent's output (and its terminal queries reach the subscriber,
    /// not the node). `before_start` runs with the agent's id before the
    /// process starts, e.g. to route the agent's exit to this subscription.
    pub fn spawn_attached(
        &self,
        req: SpawnAgent,
        conn: ConnId,
        before_start: impl FnOnce(u64),
    ) -> io::Result<(AgentInfo, AttachOk, Subscription)> {
        self.spawn_attached_by(Caller::User, req, conn, before_start)
    }

    /// [`Node::spawn_attached`] by `caller`, as for [`Node::spawn_by`].
    pub fn spawn_attached_by(
        &self,
        caller: Caller,
        mut req: SpawnAgent,
        conn: ConnId,
        before_start: impl FnOnce(u64),
    ) -> io::Result<(AgentInfo, AttachOk, Subscription)> {
        check_labels(&req.labels)?;
        let _gate = self.admit_spawn(caller, Some(&mut req.labels), None)?;
        let (info, attached) = self.start(req, Some((conn, Box::new(before_start))))?;
        let (ok, sub) = attached.expect("an attached start returns its attachment");
        Ok((info, ok, sub))
    }

    /// Restarts an exited or lost agent: a new agent (new id) runs the same
    /// program, args and cwd with the same labels, in a fresh environment
    /// (the base env and its own `FLEET_AGENT_ID`; the old agent's env is
    /// not kept). Once it started, the old agent is forgotten, recorded as
    /// replaced by the new one, and a [`Removed`] is announced; if it fails
    /// to start, the old agent is untouched. Running agents are refused,
    /// never killed. `cols`/`rows` are as for [`Node::spawn`].
    pub fn restart(
        &self,
        id: u64,
        cols: u32,
        rows: u32,
        origin: Option<ConnId>,
    ) -> io::Result<RestartOutcome> {
        self.restart_by(Caller::User, id, cols, rows, origin)
    }

    /// [`Node::restart`] by `caller`: an agent's restart is subject to the
    /// quotas, under the restarted agent's parent.
    pub fn restart_by(
        &self,
        caller: Caller,
        id: u64,
        cols: u32,
        rows: u32,
        origin: Option<ConnId>,
    ) -> io::Result<RestartOutcome> {
        self.restart_with(caller, id, cols, rows, None, origin)
            .map(|(out, _)| out)
    }

    /// [`Node::restart`], with the new agent attached by `conn` as for
    /// [`Node::spawn_attached`]. The removal's origin is `conn`.
    pub fn restart_attached(
        &self,
        id: u64,
        cols: u32,
        rows: u32,
        conn: ConnId,
        before_start: impl FnOnce(u64),
    ) -> io::Result<(RestartOutcome, AttachOk, Subscription)> {
        self.restart_attached_by(Caller::User, id, cols, rows, conn, before_start)
    }

    /// [`Node::restart_attached`] by `caller`, as for [`Node::restart_by`].
    pub fn restart_attached_by(
        &self,
        caller: Caller,
        id: u64,
        cols: u32,
        rows: u32,
        conn: ConnId,
        before_start: impl FnOnce(u64),
    ) -> io::Result<(RestartOutcome, AttachOk, Subscription)> {
        let (out, attached) = self.restart_with(
            caller,
            id,
            cols,
            rows,
            Some((conn, Box::new(before_start))),
            Some(conn),
        )?;
        let (ok, sub) = attached.expect("an attached start returns its attachment");
        Ok((out, ok, sub))
    }

    fn restart_with(
        &self,
        caller: Caller,
        id: u64,
        cols: u32,
        rows: u32,
        attach: Option<(ConnId, BeforeStart<'_>)>,
        origin: Option<ConnId>,
    ) -> io::Result<(RestartOutcome, Option<(AttachOk, Subscription)>)> {
        let _in_flight = self.handoff.enter()?;
        let (old, _restarting) = {
            let mut agents = lock(&self.agents);
            if agents.restarting.contains(&id) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("agent {id} is being restarted"),
                ));
            }
            // Under the agents lock, as in `forget`: an agent still `live`
            // (its row may already say exited) or being spawned is running.
            let busy =
                agents.live.contains_key(&id) || lock(&self.histories).spawning.contains(&id);
            let old = lock(&self.store).agent(id).map_err(store_err)?;
            let old = match old {
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("unknown agent {id}"),
                    ));
                }
                Some(rec) if busy || rec.state == store::AgentState::Running => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("agent {id} is running"),
                    ));
                }
                Some(rec) => rec,
            };
            agents.restarting.insert(id);
            (
                old,
                Restarting {
                    agents: &self.agents,
                    id,
                },
            )
        };
        // portable-pty starts a child whose cwd is gone in the home
        // directory instead; a restart must run where the agent ran.
        if !Path::new(&old.cwd).is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("working directory {} does not exist", old.cwd),
            ));
        }
        let _gate = self.admit_spawn(caller, None, Some(id))?;
        // Stored labels were checked when set; not re-checked here.
        let req = SpawnAgent {
            program: old.program,
            args: old.args,
            cwd: old.cwd,
            cols,
            rows,
            labels: old.labels,
            env: HashMap::new(),
            attach: false,
            priority: old.priority,
        };
        let (info, attached) = self.start(req, attach)?;
        if let Err(e) = lock(&self.store).replace_agent(id, info.id) {
            // Both agents stay known: never neither.
            warn!(agent = id, replaced_by = info.id, error = %e, "recording the restart failed");
            let out = RestartOutcome {
                info,
                replaced: false,
            };
            return Ok((out, attached));
        }
        self.drop_agent_files(id);
        info!(agent = id, replaced_by = info.id, "agent restarted");
        // No receivers is fine.
        let _ = self.removed.send(Removed {
            event: AgentRemoved {
                agent_id: id,
                replaced_by: Some(info.id),
            },
            origin,
        });
        let out = RestartOutcome {
            info,
            replaced: true,
        };
        Ok((out, attached))
    }

    /// Spawns an agent; with `attach`, also attaches to it before the process
    /// starts, calling `attach` with its id first. The labels are not checked.
    fn start(
        &self,
        req: SpawnAgent,
        attach: Option<(ConnId, BeforeStart<'_>)>,
    ) -> io::Result<(AgentInfo, Option<(AttachOk, Subscription)>)> {
        let _in_flight = self.handoff.enter()?;
        if lock(&self.agents).closing {
            return Err(shutting_down());
        }
        if req.program.is_empty() {
            return Err(invalid("program is empty".into()));
        }
        let priority = AgentPriority::try_from(req.priority)
            .map_err(|_| invalid(format!("unknown agent priority {}", req.priority)))?;
        // Waits (bounded) for a login-env capture still running, before
        // anything is recorded.
        let base_env = self.base_env.wait();
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
        let token = AgentToken::generate()?;
        let created_unix_ms = unix_ms_now();
        let new = NewAgent {
            program: req.program.clone(),
            args: req.args.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            created_unix_ms,
            scrollback_path: String::new(),
            labels: req.labels,
            priority: req.priority,
        };
        let (id, path) = {
            let db = lock(&self.store);
            let (id, path) = db
                .insert_agent_with_scrollback(&new, |id| {
                    self.scrollback_dir
                        .join(format!("{id}.ring"))
                        .to_string_lossy()
                        .into_owned()
                })
                .map_err(store_err)?;
            if let Err(e) = db.set_token(id, &token.digest()) {
                let _ = db.delete_agent(id);
                return Err(store_err(e));
            }
            (id, path)
        };
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
                let attached = attach.map(|(conn, before_start)| {
                    before_start(id);
                    let a = pipeline.attach(conn);
                    let mut ok = attach_ok(id, a.snapshot, a.size, a.history);
                    ok.primary = a.primary;
                    (ok, a.subscription)
                });
                let sink = self.sink(id, &pipeline);
                let spec = CommandSpec {
                    program: PathBuf::from(&req.program),
                    args: req.args.clone(),
                    cwd: Some(cwd),
                    env: agent_env(req.env, id, &token),
                    cols,
                    rows,
                    nice: crate::priority::agent_nice(self.agent_nice, priority),
                };
                let host_id = self.host.spawn_with_base_env(
                    spec,
                    base_env.as_deref().map(Vec::as_slice),
                    Box::new(sink),
                )?;
                Ok((LiveAgent { host_id, pipeline }, attached))
            });
        let (live, attached) = match started {
            Ok(started) => started,
            Err(e) => {
                // Only the kind: the error's text can carry the agent's env
                // (portable-pty names the PATH it searched).
                warn!(agent = id, program = ?new.program, error_kind = ?e.kind(), "spawn failed");
                if let Err(e) = lock(&self.store).delete_agent(id) {
                    warn!(agent = id, error = %e, "removing the failed agent's row failed");
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
            agents.register(&*self.host, id, live)
        };
        self.sweeper.kick();
        let mut histories = lock(&self.histories);
        histories.spawning.remove(&id);
        if let Some(evicted) = evicted {
            histories.delete(evicted, "beyond retention");
        }
        drop(histories);
        // Label keys only: values, args and env may hold secrets.
        let mut label_keys: Vec<&str> = new.labels.keys().map(String::as_str).collect();
        label_keys.sort_unstable();
        info!(
            agent = id,
            program = ?new.program,
            cwd = ?new.cwd,
            labels = ?label_keys,
            "agent spawned"
        );
        let git = self.git.get(&new.cwd);
        let info = AgentInfo {
            id,
            program: new.program,
            args: new.args,
            cwd: new.cwd,
            state: fleet_proto::AgentState::Running.into(),
            exit_code: None,
            created_unix_ms,
            labels: new.labels,
            exited_unix_ms: None,
            reported: None,
            git,
        };
        Ok((info, attached))
    }

    /// Every agent this node knows of, including exited and lost ones,
    /// with the git checkout of its cwd (cached, ADR 0085). A corrupt row
    /// is logged and left out.
    pub fn list(&self) -> io::Result<Vec<AgentInfo>> {
        let (records, bad) = lock(&self.store).list_agents_lossy().map_err(store_err)?;
        for e in bad {
            // Not the error's text: a decode error may quote the row's data.
            let _ = e;
            warn!("skipping an unreadable agent row");
        }
        Ok(records
            .into_iter()
            .map(|rec| {
                let mut info = agent_info(rec);
                info.git = self.git.get(&info.cwd);
                info
            })
            .collect())
    }

    /// The agents whose labels contain every `selector` pair (all of them
    /// for an empty selector).
    pub fn list_matching(&self, selector: &Labels) -> io::Result<Vec<AgentInfo>> {
        let mut agents = self.list()?;
        agents.retain(|a| labels_match(&a.labels, selector));
        Ok(agents)
    }

    /// The replacement records: old agent id to the agent that replaced it.
    pub fn replacements(&self) -> io::Result<HashMap<u64, u64>> {
        lock(&self.store).replacements().map_err(store_err)
    }

    /// Forgets exited or lost agents: their record, history and scrollback
    /// are deleted and a [`Removed`] is announced for each. Running and
    /// unknown agents are refused, never killed. With `exited_only`, `ids`
    /// must be empty and every non-running agent matching `selector` is
    /// forgotten. A store error stops it; the [`ForgetError`] carries the
    /// agents forgotten (and announced) before it.
    pub fn forget(
        &self,
        ids: &[u64],
        exited_only: bool,
        selector: &Labels,
        origin: Option<ConnId>,
    ) -> Result<ForgetOutcome, ForgetError> {
        let _in_flight = self.handoff.enter()?;
        let targets: Vec<u64> = if exited_only {
            if !ids.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "exited_only cannot be combined with agent ids",
                )
                .into());
            }
            self.list_matching(selector)?
                .into_iter()
                .filter(|a| a.state != fleet_proto::AgentState::Running as i32)
                .map(|a| a.id)
                .collect()
        } else {
            ids.to_vec()
        };
        let mut out = ForgetOutcome::default();
        for id in targets {
            // Under the agents lock, so an exit cannot retire the agent
            // between the check and the delete: an agent still `live` (its
            // row may already say exited while its pipeline finishes) or
            // still being spawned is refused.
            let agents = lock(&self.agents);
            let busy = agents.live.contains_key(&id)
                || agents.restarting.contains(&id)
                || lock(&self.histories).spawning.contains(&id);
            let gone = if busy {
                Ok(false)
            } else {
                lock(&self.store).forget_agent(id)
            };
            drop(agents);
            let gone = match gone {
                Ok(gone) => gone,
                Err(e) => {
                    return Err(ForgetError {
                        error: store_err(e),
                        partial: out,
                    });
                }
            };
            if !gone {
                out.refused.push(id);
                continue;
            }
            self.drop_agent_files(id);
            info!(agent = id, "agent forgotten");
            // No receivers is fine.
            let _ = self.removed.send(Removed {
                event: AgentRemoved {
                    agent_id: id,
                    replaced_by: None,
                },
                origin,
            });
            out.forgotten.push(id);
        }
        Ok(out)
    }

    /// [`Node::forget`] by `caller`: an agent's `exited_only` forgets only
    /// its own exited descendants. (Explicit ids are checked against its
    /// scope by the caller of this.)
    pub fn forget_by(
        &self,
        caller: Caller,
        ids: &[u64],
        exited_only: bool,
        selector: &Labels,
        origin: Option<ConnId>,
    ) -> Result<ForgetOutcome, ForgetError> {
        let Caller::Agent(a) = caller else {
            return self.forget(ids, exited_only, selector, origin);
        };
        if !exited_only {
            return self.forget(ids, false, selector, origin);
        }
        if !ids.is_empty() {
            return self.forget(ids, true, selector, origin);
        }
        let mine = self.tree()?.descendants(a);
        let targets: Vec<u64> = self
            .list_matching(selector)?
            .into_iter()
            .filter(|i| mine.contains(&i.id) && i.state != fleet_proto::AgentState::Running as i32)
            .map(|i| i.id)
            .collect();
        if targets.is_empty() {
            return Ok(ForgetOutcome::default());
        }
        self.forget(&targets, false, &Labels::new(), origin)
    }

    /// Drops everything the node keeps of a removed agent besides its row:
    /// retained final screen, history, scrollback ring and activity sink.
    fn drop_agent_files(&self, id: u64) {
        lock(&self.agents).exited.retain(|a| a.id != id);
        lock(&self.histories).delete(id, "agent removed");
        match std::fs::remove_file(self.scrollback_dir.join(format!("{id}.ring"))) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => warn!(agent = id, error = %e, "removing agent scrollback failed"),
        }
        self.activity.sinks.forget(id);
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
    /// is still kept (and are zero/empty otherwise). A running agent's
    /// attach replaces `conn`'s previous subscription to it and makes `conn`
    /// primary (`AttachOk.primary`); there is no primary after exit.
    pub fn attach(&self, id: u64, conn: ConnId) -> io::Result<(AttachOk, Subscription)> {
        self.attach_with(id, Some(conn))
    }

    /// A read-only [`Node::attach`] (see [`AgentPipeline::observe`]): it
    /// never takes or changes the primary role, and its connection's input
    /// does not promote it. `AttachOk.primary` is `false`.
    pub fn observe(&self, id: u64) -> io::Result<(AttachOk, Subscription)> {
        self.attach_with(id, None)
    }

    fn attach_with(&self, id: u64, conn: Option<ConnId>) -> io::Result<(AttachOk, Subscription)> {
        let (snapshot, (cols, rows), history, sub, primary) = {
            let agents = lock(&self.agents);
            if let Some(live) = agents.live.get(&id) {
                let a = match conn {
                    Some(conn) => live.pipeline.attach(conn),
                    None => live.pipeline.observe(),
                };
                (a.snapshot, a.size, a.history, a.subscription, a.primary)
            } else if let Some(a) = agents.exited(id) {
                let sub = Subscription::exited(a.exit_code);
                (a.snapshot.clone(), (a.cols, a.rows), None, sub, false)
            } else {
                drop(agents);
                match lock(&self.store).exit_code(id).map_err(store_err)? {
                    Some(code) => (Vec::new(), (0, 0), None, Subscription::exited(code), false),
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
        let mut ok = attach_ok(id, snapshot, (cols, rows), history);
        ok.primary = primary;
        Ok((ok, sub))
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

    /// Searches every agent's output for `needle` (ADR 0098): newest agent
    /// first, each its screen bottom up, then its history newest line first,
    /// within the hit caps and [`output_search::SEARCH_BUDGET_BYTES`]. An exited agent's
    /// screen is its kept final screen, if any. History pages are read one
    /// at a time, so no lock is held for the whole scan.
    pub fn search(&self, needle: Needle, limit: u32, per_agent: u32) -> io::Result<Hits> {
        let (records, _bad) = lock(&self.store).list_agents_lossy_unlabeled().map_err(store_err)?;
        let mut ids: Vec<u64> = records.iter().map(|r| r.id).collect();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        let mut scan = output_search::Scan::new(needle, limit, per_agent);
        for id in ids {
            if scan.done() {
                break;
            }
            let (screen, info) = self.search_view(id);
            let range = info.map(|i| (i.first_line, i.next_line));
            let generation = info.map_or(0, |i| i.generation);
            scan.agent(id, &screen, range, |before, max| {
                self.read_history(id, generation, before, max)
            });
        }
        Ok(scan.finish())
    }

    /// The agent's screen rows and history range, for [`Node::search`].
    fn search_view(&self, id: u64) -> (Vec<String>, Option<HistoryInfo>) {
        let screen = {
            let agents = lock(&self.agents);
            if let Some(live) = agents.live.get(&id) {
                let pipeline = live.pipeline.clone();
                drop(agents);
                let (screen, info) = pipeline.search_view();
                if info.is_some() {
                    return (screen, info);
                }
                screen
            } else if let Some(a) = agents.exited(id).filter(|a| a.cols > 0 && a.rows > 0) {
                let (snapshot, cols, rows) = (a.snapshot.clone(), a.cols, a.rows);
                drop(agents);
                let mut term = Terminal::new(cols, rows);
                term.feed(&snapshot);
                term.visible_text()
            } else {
                Vec::new()
            }
        };
        let info = lock(&self.histories).with(id, History::info).ok();
        (screen, info)
    }

    /// Exited agents whose final screen is kept (at most [`RETAINED_EXITED`]).
    pub fn retained_exited_agents(&self) -> usize {
        lock(&self.agents).exited.len()
    }

    /// Writes to the agent's terminal input; blocks while its input queue is
    /// full. If `conn` is attached to the agent, it becomes primary.
    pub fn input(&self, id: u64, conn: ConnId, bytes: &[u8]) -> io::Result<()> {
        let live = self.live(id)?;
        live.pipeline.input_from(conn);
        self.host.write_input(live.host_id, bytes)
    }

    /// Resizes the agent's PTY and grid (within [`MAX_DIM`]/[`MAX_CELLS`])
    /// if `conn` is the primary or no connection is attached; otherwise the
    /// valid request is ignored.
    pub fn resize(&self, id: u64, conn: ConnId, cols: u32, rows: u32) -> io::Result<()> {
        let (cols, rows) = checked_size(cols, rows)?;
        let live = self.live(id)?;
        if !live.pipeline.may_resize(conn) {
            return Ok(());
        }
        self.host.resize(live.host_id, cols, rows)?;
        live.pipeline.resize(cols, rows);
        Ok(())
    }

    /// Sends SIGHUP and SIGTERM to the agent's process group, then SIGKILL
    /// if it is still running after [`KILL_GRACE`] (ADR 0071); a no-op once
    /// it exited.
    pub fn kill(&self, id: u64) -> io::Result<()> {
        match self.live(id) {
            Ok(live) => {
                info!(agent = id, signal = "SIGHUP+SIGTERM", "agent killed");
                self.host.kill(live.host_id)?;
                let host = Arc::clone(&self.host);
                let host_id = live.host_id;
                // `force_kill` is a no-op once the agent has exited (its pid
                // may be reused by then), so the escalation never misfires.
                let escalate = move || {
                    std::thread::sleep(KILL_GRACE);
                    if host.stats(host_id).is_some_and(|s| s.exit_code.is_none()) {
                        info!(
                            agent = id,
                            signal = "SIGKILL",
                            "agent outlived the kill grace"
                        );
                        if let Err(e) = host.force_kill(host_id) {
                            warn!(agent = id, error = %e, "cannot SIGKILL agent");
                        }
                    }
                };
                if let Err(e) = std::thread::Builder::new()
                    .name("fleet-kill-grace".into())
                    .spawn(escalate)
                {
                    warn!(agent = id, error = %e, "cannot start the kill escalation");
                }
                Ok(())
            }
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

    /// Stops every agent (to hand them to a successor daemon instead, see
    /// [`Node::prepare_handoff`]): SIGTERM to each process group, [`SHUTDOWN_GRACE`] to
    /// exit, then SIGKILL. Exits are recorded as usual; an agent that still
    /// has not reported is left `running` in the store, to be marked lost by
    /// the next [`Node::open`]. Refuses new spawns from now on, and kills a
    /// login-env capture still running.
    pub fn shutdown(&self) {
        self.base_env.cancel();
        let host_ids: Vec<AgentId> = {
            let mut agents = lock(&self.agents);
            agents.closing = true;
            agents.live.values().map(|a| a.host_id).collect()
        };
        info!(agents = host_ids.len(), "shutdown: SIGTERM to every agent");
        for &id in &host_ids {
            if let Err(e) = self.host.kill(id) {
                warn!(error = %e, "SIGTERM to an agent's group failed");
            }
        }
        if self.host.wait_all(SHUTDOWN_GRACE).is_ok() {
            return;
        }
        warn!(grace = ?SHUTDOWN_GRACE, "shutdown: agents still running; SIGKILL");
        for &id in &host_ids {
            if let Err(e) = self.host.force_kill(id) {
                warn!(error = %e, "SIGKILL to an agent's group failed");
            }
        }
        if self.host.wait_all(KILL_WAIT).is_err() {
            warn!("some agents did not exit; they will be marked lost");
        }
    }

    /// The sink that feeds `pipeline` with agent `id`'s output and records
    /// its exit.
    fn sink(&self, id: u64, pipeline: &AgentPipeline) -> NodeSink {
        NodeSink {
            id,
            pipeline: pipeline.clone(),
            store: Arc::clone(&self.store),
            host: Arc::clone(&self.host),
            agents: Arc::clone(&self.agents),
            histories: Arc::clone(&self.histories),
            exits: self.exits.clone(),
            activity: Arc::clone(&self.activity),
            notices: self.notices.clone(),
        }
    }

    /// Readies the running agents to be handed to a successor daemon, just
    /// before `exec`: from now on spawns, restarts and forgets fail with
    /// "fleetd is restarting"; the host is frozen (no output is read, input
    /// fails); each agent's scrollback and history are flushed. Returns one
    /// record entry per running agent, its screen as the attach snapshot and
    /// its PTY master as an fd of this process.
    ///
    /// Refuses with "a restart is already in progress", or with "agents are
    /// starting; try again" if a spawn, restart or forget is still in flight
    /// at `deadline`; the freeze also has to finish by then, and so must the
    /// exit of any agent whose output has ended (else "an agent is exiting;
    /// try again", see `wait_for_exiting`). On any error
    /// nothing stays frozen and the node keeps serving. Undo a success with
    /// [`Node::thaw`] (for example when the `exec` fails).
    ///
    /// The final screens of exited agents ([`RETAINED_EXITED`]) are not
    /// handed over: after a restart, attaching to an agent that exited
    /// before it yields the empty snapshot, as for an agent of an earlier
    /// daemon run.
    pub fn prepare_handoff(&self, deadline: Instant) -> io::Result<Vec<HandoffAgent>> {
        self.handoff.begin(deadline)?;
        // Reopens the gate on an error or a panic; disarmed on success.
        let gate = OnDrop::new(|| self.handoff.end());
        let agents = self.freeze_for_handoff(deadline);
        match &agents {
            Ok(agents) => {
                info!(agents = agents.len(), "handoff prepared");
                gate.disarm();
            }
            Err(e) => warn!(error = %e, "preparing the handoff failed; serving on"),
        }
        agents
    }

    /// Freezes the host and describes its agents; thaws on an error or a
    /// panic.
    fn freeze_for_handoff(&self, deadline: Instant) -> io::Result<Vec<HandoffAgent>> {
        // A failed freeze thaws itself, or was not ours to thaw (busy).
        let frozen = self.host.freeze(deadline)?;
        let thaw = OnDrop::new(|| self.host.thaw());
        let described = self
            .wait_for_exiting(&frozen, deadline)
            .and_then(|()| self.describe_frozen(frozen));
        if described.is_ok() {
            thaw.disarm();
        }
        described
    }

    /// Waits until `deadline` for every live agent the freeze did not list
    /// to be retired. Such an agent's output has ended, but its reader has
    /// not yet reaped it and recorded its exit (`on_exit`): usually a matter
    /// of milliseconds, but a process that closed its terminal and keeps
    /// running holds its reader in `waitpid` for as long as it runs. Handed
    /// over neither way, it would be marked lost and left running untracked
    /// by the successor; so the handoff fails with "an agent is exiting; try
    /// again" if one remains.
    fn wait_for_exiting(&self, frozen: &[crate::FrozenAgent], deadline: Instant) -> io::Result<()> {
        let frozen: HashSet<AgentId> = frozen.iter().map(|a| a.id).collect();
        loop {
            let exiting = lock(&self.agents)
                .live
                .values()
                .filter(|live| !frozen.contains(&live.host_id))
                .count();
            if exiting == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                warn!(
                    agents = exiting,
                    "agents still exiting at the handoff deadline"
                );
                return Err(handoff::agent_exiting());
            }
            std::thread::sleep(handoff::EXITING_POLL);
        }
    }

    fn describe_frozen(&self, frozen: Vec<crate::FrozenAgent>) -> io::Result<Vec<HandoffAgent>> {
        // No spawn is in flight, so every frozen agent is registered.
        let by_host: HashMap<AgentId, (u64, AgentPipeline)> = lock(&self.agents)
            .live
            .iter()
            .map(|(&id, live)| (live.host_id, (id, live.pipeline.clone())))
            .collect();
        test_panic_describing();
        frozen
            .into_iter()
            .map(|agent| {
                let (id, pipeline) = by_host
                    .get(&agent.id)
                    .ok_or_else(|| io::Error::other("a frozen agent is unknown to the node"))?;
                let (snapshot, (cols, rows)) = pipeline.hand_off()?;
                let pid = i32::try_from(agent.pid).map_err(io::Error::other)?;
                let terminal = match agent.terminal {
                    TerminalHandle::Fd(fd) => handoff_agent::Terminal::Fd(fd),
                };
                Ok(HandoffAgent {
                    id: *id,
                    pid,
                    cols: cols.into(),
                    rows: rows.into(),
                    snapshot,
                    terminal: Some(terminal),
                })
            })
            .collect()
    }

    /// Undoes [`Node::prepare_handoff`]: output is read and input accepted
    /// again, then spawns, restarts and forgets. Idempotent.
    pub fn thaw(&self) {
        self.host.thaw();
        self.handoff.end();
        info!("handoff undone; serving on");
    }

    /// [`Node::open`] for a daemon started from a handoff: the agents a
    /// predecessor handed over (`agents`, from its
    /// [`Node::prepare_handoff`]) are not marked lost but adopted, each with
    /// its scrollback ring and history reopened and its screen replayed from
    /// the snapshot. Every other agent left running is marked lost, as by
    /// [`Node::open`].
    ///
    /// Owns every fd the record names (as an `exec` passes them on) that is
    /// a terminal. One the process does not have open when this is called,
    /// or that is not a terminal, is never touched, and its agent fails, so
    /// a stale or corrupt record cannot make the node close one of its own
    /// files (or the daemon's listener, lock or log).
    ///
    /// Returns the node and the ids kept and failed. An agent that fails to
    /// be restored is ended (SIGTERM to its group, SIGKILL after
    /// [`SHUTDOWN_GRACE`]) and recorded as exited with code -1, as when
    /// waiting for an agent fails. If the node cannot be opened at all,
    /// every agent in the record is ended so none runs untracked.
    ///
    /// A corrupt record may name one fd or one id twice. A later entry
    /// naming an fd already claimed fails like any agent without a
    /// terminal. A later entry repeating an id is dropped (its fd, if
    /// claimed, closed) without touching that id's row, which belongs to
    /// the first entry, so no id is registered twice; its process is ended
    /// unless a kept agent has the same pid.
    pub fn open_with_handoff(
        state_dir: &Path,
        agents: Vec<HandoffAgent>,
    ) -> io::Result<(Node, Vec<u64>, Vec<u64>)> {
        // Before the node opens any file of its own.
        let mut fds = HashSet::new();
        let mut ids = HashSet::new();
        let mut repeated = Vec::new();
        let mut claimed: Vec<(HandoffAgent, Option<OwnedFd>)> = Vec::new();
        for mut agent in agents {
            let fd = handoff::claim_terminal(agent.terminal.take(), &mut fds);
            if ids.insert(agent.id) {
                claimed.push((agent, fd));
            } else {
                warn!(
                    agent = agent.id,
                    "the handoff record repeats an agent; ending the copy"
                );
                repeated.push(agent.pid);
            }
        }
        let keep: Vec<u64> = claimed.iter().map(|(a, _)| a.id).collect();
        let node = match Self::open_sparing(state_dir, &keep) {
            Ok(node) => node,
            Err(e) => {
                let mut pids: Vec<i32> = claimed.iter().map(|(a, _)| a.pid).collect();
                pids.extend(repeated);
                drop(claimed);
                handoff::end_unrestored(&pids, SHUTDOWN_GRACE);
                return Err(e);
            }
        };
        let (mut kept, mut failed) = (Vec::new(), Vec::new());
        let mut kept_pids = HashSet::new();
        for (agent, fd) in claimed {
            match node.restore_agent(&agent, fd) {
                Ok(()) => {
                    info!(agent = agent.id, "agent restored");
                    kept.push(agent.id);
                    kept_pids.insert(agent.pid);
                }
                Err(e) => {
                    warn!(agent = agent.id, error = %e, "restoring the agent failed; ending it");
                    failed.push(agent);
                }
            }
        }
        // Never a kept agent's process, whatever else the record says.
        let pids: Vec<i32> = failed
            .iter()
            .map(|a| a.pid)
            .chain(repeated)
            .filter(|pid| !kept_pids.contains(pid))
            .collect();
        handoff::end_unrestored(&pids, SHUTDOWN_GRACE);
        let failed: Vec<u64> = failed.into_iter().map(|a| a.id).collect();
        for &id in &failed {
            if let Err(e) = lock(&node.store).set_exited(id, -1) {
                warn!(agent = id, error = %e, "recording the unrestored agent's exit failed");
            }
        }
        info!(
            kept = kept.len(),
            failed = failed.len(),
            "opened from a handoff"
        );
        Ok((node, kept, failed))
    }

    /// Adopts one handed-over agent: reopens its scrollback and history,
    /// replays its screen into a fresh grid of the recorded size, and has the
    /// host take over its process and terminal.
    fn restore_agent(&self, agent: &HandoffAgent, terminal: Option<OwnedFd>) -> io::Result<()> {
        let id = agent.id;
        let terminal =
            terminal.ok_or_else(|| invalid("no open terminal fd in the record".into()))?;
        let (cols, rows) = checked_size(agent.cols, agent.rows)?;
        let pid = u32::try_from(agent.pid).map_err(|_| invalid("invalid agent pid".into()))?;
        let record = lock(&self.store).agent(id).map_err(store_err)?;
        let record = match record {
            Some(rec) if rec.state == store::AgentState::Running => rec,
            _ => return Err(invalid(format!("agent {id} is not running in the store"))),
        };
        let scrollback = Scrollback::open(Path::new(&record.scrollback_path))?;
        let history = History::open(&lock(&self.histories).path(id))?;
        let pipeline = AgentPipeline::with_history(cols, rows, scrollback, Some(history));
        pipeline.restore_screen(&agent.snapshot);
        let spec = AdoptSpec {
            pid,
            terminal: TerminalHandle::Fd(terminal.into_raw_fd()),
            cols,
            rows,
        };
        let host_id = self.host.adopt(spec, Box::new(self.sink(id, &pipeline)))?;
        let evicted = lock(&self.agents).register(&*self.host, id, LiveAgent { host_id, pipeline });
        self.sweeper.kick();
        if let Some(evicted) = evicted {
            lock(&self.histories).delete(evicted, "beyond retention");
        }
        Ok(())
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
        exited_unix_ms: rec.exited_unix_ms,
        reported: rec.reported.map(|r| Reported {
            state: r.state,
            message: r.message,
            at_unix_ms: r.at_unix_ms,
            question: r.question.map(question_proto),
        }),
        git: None,
    }
}

fn question_proto(q: store::Question) -> fleet_proto::Question {
    fleet_proto::Question {
        id: q.id,
        text: q.text,
        choices: q.choices,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pruning_skips_unreadable_entries() {
        let dir = tempfile::tempdir().unwrap();
        let orphan = dir.path().join("7.ring");
        std::fs::write(&orphan, b"secret output").unwrap();
        let entries = vec![
            Err(io::Error::other("unreadable entry")),
            Ok(orphan.clone()),
        ];
        prune_ring_paths(entries, &HashSet::new());
        assert!(!orphan.exists(), "later entries are still pruned");
    }

    #[test]
    fn forget_refuses_agent_still_live_after_its_row_says_exited() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::open(dir.path()).unwrap();
        let id = spawn_sized(&node, 80, 24).unwrap().id;
        // The window in `on_exit`: row exited, agent not yet retired.
        lock(&node.store).set_exited(id, 0).unwrap();
        let out = node.forget(&[id], false, &Labels::new(), None).unwrap();
        assert_eq!(out.refused, vec![id]);
        assert!(out.forgotten.is_empty());
        assert!(node.list().unwrap().iter().any(|a| a.id == id));
        node.shutdown();
    }

    #[test]
    fn restart_refuses_agent_still_live_after_its_row_says_exited() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::open(dir.path()).unwrap();
        let id = spawn_sized(&node, 80, 24).unwrap().id;
        lock(&node.store).set_exited(id, 0).unwrap();
        let e = node.restart(id, 0, 0, None).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(node.list().unwrap().len(), 1);
        node.shutdown();
    }

    #[test]
    fn agent_being_restarted_is_neither_restarted_nor_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::open(dir.path()).unwrap();
        let id = spawn_sized(&node, 80, 24).unwrap().id;
        node.kill(id).unwrap();
        while node.list().unwrap()[0].state == fleet_proto::AgentState::Running as i32 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        lock(&node.agents).restarting.insert(id);
        let e = node.restart(id, 0, 0, None).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        let out = node.forget(&[id], false, &Labels::new(), None).unwrap();
        assert_eq!(out.refused, vec![id]);
        lock(&node.agents).restarting.remove(&id);
        assert!(node.restart(id, 0, 0, None).is_ok());
        node.shutdown();
    }

    /// Runs `sql` on a second connection to the node's state DB (e.g. a
    /// trigger that makes a store write fail).
    fn inject(dir: &Path, sql: &str) {
        rusqlite::Connection::open(dir.join("state.db"))
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }

    /// An agent that ran and was retired (its exit event is sent after).
    fn exited(node: &Node) -> u64 {
        let mut exits = node.exit_events();
        let id = spawn_sized(node, 80, 24).unwrap().id;
        node.kill(id).unwrap();
        while exits.blocking_recv().unwrap().agent_id != id {}
        id
    }

    #[test]
    fn restart_whose_record_fails_says_so_and_announces_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::open(dir.path()).unwrap();
        let old = exited(&node);
        let mut removed = node.removed_events();
        inject(
            dir.path(),
            "CREATE TRIGGER no_replace BEFORE INSERT ON agent_replacements \
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        );
        let out = node.restart(old, 0, 0, None).unwrap();
        assert!(!out.replaced);
        assert_ne!(out.info.id, old);
        // Both agents stay known, and nobody is told the old one is gone.
        let ids: Vec<u64> = node.list().unwrap().iter().map(|a| a.id).collect();
        assert!(ids.contains(&old) && ids.contains(&out.info.id), "{ids:?}");
        assert!(node.replacements().unwrap().is_empty());
        assert!(removed.try_recv().is_err());
        node.shutdown();
    }

    #[test]
    fn forget_that_fails_midway_reports_what_it_forgot() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::open(dir.path()).unwrap();
        let (a, b, c) = (exited(&node), exited(&node), exited(&node));
        inject(
            dir.path(),
            &format!(
                "CREATE TRIGGER no_forget BEFORE DELETE ON agents WHEN OLD.id = {b} \
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;"
            ),
        );
        let e = node
            .forget(&[a, b, c], false, &Labels::new(), None)
            .unwrap_err();
        assert_eq!(e.partial.forgotten, vec![a]);
        assert!(e.partial.refused.is_empty());
        assert!(e.to_string().contains("injected"), "{e}");
        let ids: Vec<u64> = node.list().unwrap().iter().map(|a| a.id).collect();
        assert_eq!(ids, vec![b, c]);
        node.shutdown();
    }

    /// fleetd's own nice value (agents inherit it).
    fn own_nice() -> i32 {
        // SAFETY: no memory effects; cannot fail for the calling process.
        unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) }
    }

    /// Spawns an agent that, after a pause (past the node's spawn), writes
    /// its nice value to `out`, and returns its id.
    fn spawn_reporting_nice(node: &Node, out: &Path, priority: AgentPriority) -> u64 {
        node.spawn(SpawnAgent {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                format!(
                    "sleep 0.2; ps -o nice= -p $$ > '{}.tmp' && mv '{0}.tmp' '{0}'",
                    out.display()
                ),
            ],
            priority: priority.into(),
            ..Default::default()
        })
        .unwrap()
        .id
    }

    /// The nice value an agent wrote to `out`, waiting for it.
    fn reported_nice(out: &Path) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(s) = std::fs::read_to_string(out) {
                return s.trim().parse().unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "no nice reported in {}",
                out.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn agents_keep_fleetds_priority_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::open(dir.path()).unwrap();
        let out = dir.path().join("nice");
        spawn_reporting_nice(&node, &out, AgentPriority::Default);
        assert_eq!(reported_nice(&out), own_nice());
        node.shutdown();
    }

    #[test]
    fn the_setting_lowers_agents_and_normal_opts_out() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::open(dir.path()).unwrap().with_agent_nice(5);
        let lowered = (own_nice() + 5).min(crate::pty::MAX_NICE);
        for (priority, want) in [
            (AgentPriority::Default, lowered),
            (AgentPriority::Low, lowered),
            (AgentPriority::Normal, own_nice()),
        ] {
            let out = dir.path().join(format!("nice-{}", priority.as_str_name()));
            spawn_reporting_nice(&node, &out, priority);
            assert_eq!(reported_nice(&out), want, "{priority:?}");
        }
        node.shutdown();
    }

    #[test]
    fn low_is_lowered_with_the_setting_off() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::open(dir.path()).unwrap();
        let out = dir.path().join("nice");
        spawn_reporting_nice(&node, &out, AgentPriority::Low);
        assert_eq!(
            reported_nice(&out),
            (own_nice() + i32::from(crate::priority::LOW_NICE)).min(crate::pty::MAX_NICE)
        );
        node.shutdown();
    }

    #[test]
    fn restart_keeps_the_agents_priority() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::open(dir.path()).unwrap().with_agent_nice(5);
        let out = dir.path().join("nice");
        let mut exits = node.exit_events();
        let id = spawn_reporting_nice(&node, &out, AgentPriority::Normal);
        assert_eq!(reported_nice(&out), own_nice());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !node
            .list()
            .unwrap()
            .iter()
            .any(|a| a.id == id && a.state == fleet_proto::AgentState::Exited as i32)
        {
            assert!(Instant::now() < deadline, "agent {id} did not exit");
            let _ = exits.try_recv();
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::remove_file(&out).unwrap();
        let new = node.restart(id, 0, 0, None).unwrap().info.id;
        assert_eq!(reported_nice(&out), own_nice(), "the restart was lowered");
        assert_eq!(
            lock(&node.store).agent(new).unwrap().unwrap().priority,
            AgentPriority::Normal as i32
        );
        node.shutdown();
    }

    #[test]
    fn spawn_refuses_an_unknown_priority() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::open(dir.path()).unwrap();
        let e = node
            .spawn(SpawnAgent {
                program: "/bin/sleep".into(),
                args: vec!["100".into()],
                priority: 99,
                ..Default::default()
            })
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert!(node.list().unwrap().is_empty());
        node.shutdown();
    }

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
            let e = node.resize(id, ConnId(1), cols, rows).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{cols}x{rows}");
        }
        node.resize(id, ConnId(1), 500, 500).unwrap();
        node.shutdown();
    }

    /// Queue memory of a subscriber that never reads is bounded by
    /// `SUBSCRIBER_CAPACITY` x `COALESCE_BYTES` (plus a constant per item),
    /// however small the reads.
    const QUEUE_MEMORY_BOUND: usize = SUBSCRIBER_CAPACITY * (COALESCE_BYTES + 1024);

    #[test]
    fn stalled_subscriber_memory_is_bounded_with_tiny_reads() {
        let dir = tempfile::tempdir().unwrap();
        let scrollback = Scrollback::create(&dir.path().join("sb"), 1024 * 1024).unwrap();
        let pipeline = AgentPipeline::new(80, 24, scrollback);
        let (_, stalled) = pipeline.subscribe();
        for _ in 0..100_000 {
            pipeline.feed(b"x");
        }
        let (items, bytes) = stalled.queued();
        // 100 000 bytes fit in ceil(100 000 / 16 KiB) merged buffers.
        assert_eq!(items, 100_000usize.div_ceil(COALESCE_BYTES));
        assert!(
            bytes <= items * COALESCE_BYTES,
            "{bytes} bytes in {items} items"
        );
    }

    #[test]
    fn full_queue_of_tiny_chunks_stays_within_its_bound() {
        let queue = Queue::new();
        let mut offered = 0usize;
        while matches!(
            queue.offer(StreamItem::Output(Bytes::from_static(b"x"))),
            Offered::Queued
        ) {
            offered += 1;
        }
        assert_eq!(offered, SUBSCRIBER_CAPACITY * COALESCE_BYTES);
        let state = lock(&queue.state);
        let bytes: usize = state.items.iter().map(Queued::output_bytes).sum();
        assert_eq!(state.items.len(), SUBSCRIBER_CAPACITY);
        assert!(bytes <= QUEUE_MEMORY_BOUND, "{bytes} bytes queued");
    }

    #[test]
    fn merged_output_keeps_order_and_bytes() {
        let queue = Queue::new();
        for chunk in [&b"ab"[..], b"cd", b"ef"] {
            assert!(matches!(
                queue.offer(StreamItem::Output(Bytes::copy_from_slice(chunk))),
                Offered::Queued
            ));
        }
        assert!(matches!(
            queue.offer(StreamItem::Role(true)),
            Offered::Queued
        ));
        assert!(matches!(
            queue.offer(StreamItem::Output(Bytes::from_static(b"gh"))),
            Offered::Queued
        ));
        queue.close_sender();
        let mut got = Vec::new();
        while let Some(item) = queue.blocking_recv() {
            got.push(item);
        }
        assert_eq!(
            got,
            [
                StreamItem::Output(Bytes::from_static(b"abcdef")),
                StreamItem::Role(true),
                StreamItem::Output(Bytes::from_static(b"gh")),
            ]
        );
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
