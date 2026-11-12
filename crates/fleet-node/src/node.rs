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
//! Terminal query replies (DSR/CPR, DA, …) are written back to the agent only
//! while no subscriber is attached; otherwise the client's terminal answers.

use crate::pty::{AgentInput, OutputSink};
use crate::scrollback::Scrollback;
use crate::term::Terminal;
use bytes::Bytes;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::mpsc::{self, error::TrySendError};

/// Chunks a subscriber may fall behind before it is dropped.
pub const SUBSCRIBER_CAPACITY: usize = 256;

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
        state.term.flush_expired_sync();
        let snapshot = state.term.snapshot();
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
        // Written outside the lock: the PTY write may block briefly.
        if let Some((input, replies)) = reply {
            let _ = input.write_all(&replies);
        }
    }

    fn on_exit(&mut self, code: i32) {
        let mut state = self.lock();
        let _ = state.scrollback.flush();
        state.exit_code = Some(code);
        state.input = None;
        for sub in std::mem::take(&mut state.subscribers) {
            sub.offer(StreamItem::Exited(code));
        }
    }
}
