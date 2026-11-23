//! Protocol connections: handshake, then requests answered in order while
//! attached agents' output and exit events stream alongside.
//!
//! Each connection has one writer task owning the socket's write half; the
//! request loop, the per-agent forwarders and the exit-event forwarder feed it
//! through a bounded channel. A client that stops reading thus stalls its
//! forwarders, its subscriptions fill up, and it is told to re-attach
//! (`Event::Lagged`) instead of slowing the agent down.
//!
//! Each connection has a [`ConnId`] for the node's primary-client rule; its
//! role changes arrive in the agent's stream and are sent as
//! `Event::Role`. Agents' activity is forwarded to every connection,
//! latest value per agent: a connection that falls behind gets each agent's
//! newest activity once it catches up.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use fleet_node::history::HISTORY_PAGE_MAX_LINES;
use fleet_node::lease::now_ms;
use fleet_node::node::{
    ActivityReceiver, ConnId, Node, QuestionIn, Removed, StreamItem, Subscription,
};
use fleet_node::scope::Caller;
use fleet_node::term::Needle;
use fleet_proto::{
    Ack, AgentList, AgentRemoved, AgentResized, AttachOk, AttachRole, Event, ForgetAgents,
    Forgotten, Frame, FrameCodec, HandshakeReject, HelloOk, HistoryPage, Lagged, LogLine, LogLines,
    MAX_FRAME, PROTOCOL_VERSION, ReadHistory, ReadLogs, Request, Response, RestartAgent, Restarted,
    RestartingDaemon, RestoreSummary, SearchHit, SearchHits, SearchOutput, SpawnAgent,
    SpawnedAttached, event, request, response,
};
use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::{debug, info, warn};

use crate::approval;
use crate::handoff::Identity;
use crate::logging;
use crate::scope;
use crate::wait;

pub(crate) mod lease;

/// Concurrent connections served; more are rejected.
const MAX_CONNECTIONS: usize = 64;
/// The first frame must be a `Hello` arriving within this time...
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// ...and no longer than this.
const HELLO_MAX_FRAME: usize = 4 * 1024;
/// Bound on writes during the handshake and on rejecting a connection.
const HANDSHAKE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Frames queued for a connection's writer.
const OUTBOX_CAPACITY: usize = 256;
/// How long the connection that asked for a restart waits for its
/// `RestartingDaemon` to be written before it lets the `exec` go ahead.
pub(crate) const RESTART_FLUSH: Duration = Duration::from_millis(200);

type Writer = FramedWrite<OwnedWriteHalf, FrameCodec>;
type Reader = FramedRead<OwnedReadHalf, FrameCodec>;

/// Source of connection ids, unique for the daemon's lifetime.
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) struct Server {
    node: Arc<Node>,
    shared: Arc<Shared>,
    slots: Arc<Semaphore>,
}

/// A `RestartDaemon` handed to the serve loop, the one restart coordinator.
pub(crate) struct RestartRequest {
    pub(crate) binary: String,
    pub(crate) reply: oneshot::Sender<RestartReply>,
}

pub(crate) enum RestartReply {
    /// Answered with an `Error`; the daemon serves on.
    Refused(String),
    /// Answered with `RestartingDaemon`; `flushed` is sent once the answer
    /// is written (or [`RESTART_FLUSH`] passed), and the `exec` follows.
    Restarting {
        agents: u32,
        flushed: std::sync::mpsc::SyncSender<()>,
    },
}

/// What every connection shares with the serve loop.
struct Shared {
    /// The `HelloOk` every handshake answers with; only its restore summary
    /// changes, when a restart fails.
    hello: Mutex<Arc<HelloOk>>,
    /// The restart handed to the serve loop (at most one: see
    /// `restart_pending`), announced by `restart_asked`.
    restart: Mutex<Option<RestartRequest>>,
    restart_asked: Notify,
    /// A restart was asked for and has not failed (one that succeeds ends
    /// in `exec`): a second is refused.
    restart_pending: AtomicBool,
    /// The node is being frozen for a restart: requests are refused.
    stopped: AtomicBool,
    /// Requests being handled (input aside, see [`Connection::handle`]); a
    /// restart waits for none to be left before it freezes, so no request
    /// changes the node (a resize after the snapshot would leave a stale
    /// size in the record).
    in_flight: AtomicUsize,
    /// Signalled (under `settle`) when the last request in flight finishes.
    settled: Condvar,
    settle: Mutex<()>,
}

fn restarting() -> response::Kind {
    error("fleetd is restarting".into())
}

impl Shared {
    fn hello(&self) -> Arc<HelloOk> {
        Arc::clone(&lock(&self.hello))
    }

    /// Admits a request: `None` while requests are stopped (see
    /// [`Restarts::stop_requests`]).
    ///
    /// Synchronous and counted, not an async lock: this runs for every
    /// request, and an await here would put the lock's states into every
    /// request's future (code paged in at start, measured as idle RSS).
    fn admit(self: &Arc<Self>) -> Option<Admitted> {
        if self.stopped.load(Ordering::SeqCst) {
            return None;
        }
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let admitted = Admitted(Arc::clone(self));
        // A stop that began meanwhile may already have seen no request in
        // flight: back out.
        (!self.stopped.load(Ordering::SeqCst)).then_some(admitted)
    }
}

/// A request in flight; leaves when dropped.
struct Admitted(Arc<Shared>);

impl Drop for Admitted {
    fn drop(&mut self) {
        if self.0.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            let _settle = lock(&self.0.settle);
            self.0.settled.notify_all();
        }
    }
}

/// Requests stopped for a restart; dropping it lets them in again.
pub(crate) struct RequestsStopped(Arc<Shared>);

impl Drop for RequestsStopped {
    fn drop(&mut self) {
        self.0.stopped.store(false, Ordering::SeqCst);
    }
}

impl Server {
    pub(crate) fn new(node: Arc<Node>, identity: Identity, restore: RestoreSummary) -> Self {
        let hello = HelloOk {
            protocol_version: PROTOCOL_VERSION,
            server: "fleetd".into(),
            instance: identity.instance,
            binary: Some(Box::new(identity.binary)),
            restore: (restore != RestoreSummary::default()).then(|| Box::new(restore)),
        };
        Server {
            node,
            shared: Arc::new(Shared {
                hello: Mutex::new(Arc::new(hello)),
                restart: Mutex::new(None),
                restart_asked: Notify::new(),
                restart_pending: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
                in_flight: AtomicUsize::new(0),
                settled: Condvar::new(),
                settle: Mutex::new(()),
            }),
            slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        }
    }

    /// Serves `stream` on its own task, or rejects it when full.
    pub(crate) fn accept(&self, stream: UnixStream) {
        match Arc::clone(&self.slots).try_acquire_owned() {
            Ok(slot) => {
                tokio::spawn(connection(
                    Arc::clone(&self.node),
                    Arc::clone(&self.shared),
                    stream,
                    slot,
                ));
            }
            Err(_) => {
                warn!(
                    max = MAX_CONNECTIONS,
                    "connection rejected: too many connections"
                );
                tokio::spawn(async move {
                    let (_, wr) = stream.into_split();
                    let mut writer = FramedWrite::new(wr, FrameCodec::new());
                    reject(&mut writer, "too many connections".into()).await;
                });
            }
        }
    }

    /// The serve loop's handle on restarts.
    pub(crate) fn restarts(&self) -> Restarts {
        Restarts(Arc::clone(&self.shared))
    }
}

/// What the restart coordinator (the serve loop and the blocking thread it
/// runs a restart on) does to the server. Synchronous, apart from waiting
/// for a request: a restart runs on a blocking thread, as plain code, which
/// is far smaller than the same steps as futures (code size is idle RSS
/// here, see `crate::restart`).
#[derive(Clone)]
pub(crate) struct Restarts(Arc<Shared>);

impl Restarts {
    /// The next restart a connection asked for.
    pub(crate) async fn next(&self) -> RestartRequest {
        loop {
            if let Some(req) = lock(&self.0.restart).take() {
                return req;
            }
            self.0.restart_asked.notified().await;
        }
    }

    /// Refuses every request from now on with "fleetd is restarting" and
    /// waits until `deadline` for those being handled to finish. `None`
    /// (requests admitted again) if some are still running then. Input is
    /// not waited for: the freeze itself ends a write stuck on an agent.
    pub(crate) fn stop_requests(&self, deadline: std::time::Instant) -> Option<RequestsStopped> {
        let shared = &self.0;
        shared.stopped.store(true, Ordering::SeqCst);
        let stopped = RequestsStopped(Arc::clone(shared));
        let mut settle = lock(&shared.settle);
        while shared.in_flight.load(Ordering::SeqCst) > 0 {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            settle = shared
                .settled
                .wait_timeout(settle, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Some(stopped)
    }

    /// A restart failed before its `exec` (or the `exec` did): handshakes
    /// report `reason` from now on (same instance, so a reconnecting
    /// client knows the old daemon serves on), and a new restart may be
    /// asked for.
    pub(crate) fn failed(&self, reason: &str) {
        {
            let mut hello = lock(&self.0.hello);
            let mut next = (**hello).clone();
            next.restore = Some(Box::new(RestoreSummary {
                restart_error: reason.to_owned(),
                ..Default::default()
            }));
            *hello = Arc::new(next);
        }
        self.0.restart_pending.store(false, Ordering::Release);
    }
}

async fn reject(writer: &mut Writer, reason: String) {
    let frame = Frame::Reject(HandshakeReject { reason });
    let _ = tokio::time::timeout(HANDSHAKE_WRITE_TIMEOUT, writer.send(frame)).await;
}

/// Reads the `Hello`, identifies the caller (ADR 0072: `session` is the
/// peer's) and answers it; the client's name and caller if the connection
/// may proceed.
async fn handshake(
    conn: ConnId,
    node: &Arc<Node>,
    session: Option<u32>,
    hello_ok: &HelloOk,
    reader: &mut Reader,
    writer: &mut Writer,
) -> Option<(String, Caller)> {
    let reject_reason = |reason: &str| {
        warn!(conn = conn.0, reason, "handshake rejected");
    };
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, reader.next()).await {
        Err(_) => {
            reject_reason(&format!("no hello within {HELLO_TIMEOUT:?}"));
            return None;
        }
        Ok(None) => {
            debug!(conn = conn.0, "connection closed before its hello");
            return None;
        }
        Ok(Some(Err(e))) => {
            reject_reason(&format!("bad hello: {e}"));
            return None;
        }
        Ok(Some(Ok(Frame::Hello(hello)))) => hello,
        Ok(Some(Ok(_))) => {
            reject_reason("first frame is not a hello");
            return None;
        }
    };
    if hello.protocol_version != PROTOCOL_VERSION {
        let reason = format!(
            "unsupported protocol version {} (server speaks {PROTOCOL_VERSION})",
            hello.protocol_version
        );
        reject_reason(&reason);
        reject(writer, reason).await;
        return None;
    }
    // Boxed: kept out of every connection's future.
    let caller = match Box::pin(scope::identify(node, session, hello.agent_token)).await {
        Ok(caller) => caller,
        Err(reason) => {
            reject_reason(&reason);
            reject(writer, reason).await;
            return None;
        }
    };
    let ok = Frame::HelloOk(hello_ok.clone());
    match tokio::time::timeout(HANDSHAKE_WRITE_TIMEOUT, writer.send(ok)).await {
        Ok(Ok(())) => Some((hello.client, caller)),
        _ => {
            debug!(conn = conn.0, "connection closed during the handshake");
            None
        }
    }
}

async fn connection(
    node: Arc<Node>,
    shared: Arc<Shared>,
    stream: UnixStream,
    _slot: OwnedSemaphorePermit,
) {
    let hello = shared.hello();
    let session = scope::peer_session(&stream);
    let (rd, wr) = stream.into_split();
    let mut reader = FramedRead::new(rd, FrameCodec::with_max(HELLO_MAX_FRAME));
    let mut writer = FramedWrite::new(wr, FrameCodec::new());
    // Subscribed before `HelloOk`, so no exit or removal after the handshake
    // is missed.
    let exits = node.exit_events();
    let removed = node.removed_events();
    let activity = node.activity_events();
    let refused = node.refused_events();
    let reported = node.report_events();
    let approval_events = node.approvals().events();
    let id = ConnId(NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed));
    let Some((client, caller)) =
        handshake(id, &node, session, &hello, &mut reader, &mut writer).await
    else {
        return;
    };
    // `?`: the name is the client's, so it is quoted and escaped.
    match caller {
        Caller::User => info!(conn = id.0, client = ?client, "connection opened"),
        Caller::Agent(agent) => {
            info!(conn = id.0, client = ?client, agent, "connection opened by an agent");
        }
    }
    reader.decoder_mut().set_max(MAX_FRAME);

    let node_for_scope = Arc::clone(&node);
    let (outbox, rx) = mpsc::channel(OUTBOX_CAPACITY);
    // Removals other connections caused, for the request loop to drop
    // this connection's state for them.
    let (removed_tx, mut removed_rx) = mpsc::unbounded_channel();
    let (approved_tx, mut approved_rx) = mpsc::unbounded_channel();
    let mut conn = Connection {
        node,
        shared,
        id,
        outbox,
        forwarders: HashMap::new(),
        attached: Arc::new(Mutex::new(HashSet::new())),
        removed_pending: HashSet::new(),
        log_follower: None,
        restarting: None,
        caller,
        scope: scope::Scope::new(&node_for_scope, caller),
        waits: Vec::new(),
        approvals: Vec::new(),
        approved_tx,
        approved: HashSet::new(),
    };
    // Boxed: kept out of every connection's future.
    Box::pin(conn.scope.warm()).await;
    let writer = tokio::spawn(write_frames(id, writer, rx));
    let tasks = Tasks(vec![
        tokio::spawn(forward_exits(
            exits,
            conn.outbox.clone(),
            Arc::clone(&conn.attached),
            conn.scope.clone(),
        )),
        tokio::spawn(forward_activity(
            activity,
            conn.outbox.clone(),
            conn.scope.clone(),
        )),
        tokio::spawn(scope::forward_refused(
            refused,
            conn.outbox.clone(),
            conn.scope.clone(),
        )),
        tokio::spawn(scope::forward_reported(
            reported,
            conn.outbox.clone(),
            conn.scope.clone(),
        )),
        tokio::spawn(approval::forward(
            approval_events,
            conn.outbox.clone(),
            conn.scope.clone(),
        )),
        tokio::spawn(forward_removed(
            removed,
            conn.outbox.clone(),
            id,
            removed_tx,
            conn.scope.clone(),
        )),
    ]);

    let reason = loop {
        tokio::select! {
            frame = reader.next() => match frame {
                None => break "client closed".to_string(),
                Some(Ok(Frame::Request(req))) => {
                    if !conn.handle(req).await {
                        break "write failed".to_string();
                    }
                    if conn.restarting.is_some() {
                        break "fleetd is restarting".to_string();
                    }
                }
                Some(Ok(_)) => break "unexpected frame from client".to_string(),
                Some(Err(e)) => break format!("read error: {e}"),
            },
            Some(agent_id) = removed_rx.recv() => conn.removed_elsewhere(agent_id).await,
            Some(req) = approved_rx.recv() => {
                conn.approved.insert(req.id);
                if !conn.handle(req).await {
                    break "write failed".to_string();
                }
            }
        }
    };
    info!(conn = id.0, reason, "connection closed");
    // Dropping `conn` and `tasks` aborts the forwarders (dropping their
    // subscriptions and outbox senders).
    let restarting = conn.restarting.take();
    drop(conn);
    drop(tasks);
    match restarting {
        // Boxed: kept out of every connection's future.
        Some(flushed) => Box::pin(flush_for_restart(writer, flushed)).await,
        // Closes the socket.
        None => writer.abort(),
    }
}

/// The writer ends once it has written what is queued, the
/// `RestartingDaemon` last: then the `exec` may go ahead.
async fn flush_for_restart(writer: JoinHandle<()>, flushed: std::sync::mpsc::SyncSender<()>) {
    let _ = tokio::time::timeout(RESTART_FLUSH, writer).await;
    let _ = flushed.try_send(());
}

/// Aborts its tasks when dropped.
struct Tasks(Vec<JoinHandle<()>>);

impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

struct Connection {
    node: Arc<Node>,
    shared: Arc<Shared>,
    id: ConnId,
    outbox: mpsc::Sender<Frame>,
    /// Output forwarder per attached agent.
    forwarders: HashMap<u64, JoinHandle<()>>,
    /// Agents whose exit event the forwarder delivers (in order after their
    /// output), so the exit-event forwarder skips them.
    attached: Arc<Mutex<HashSet<u64>>>,
    /// Agents another connection removed while their forwarder here was
    /// still running; dropped once it finishes (see
    /// [`Connection::removed_elsewhere`]).
    removed_pending: HashSet<u64>,
    /// Streams new log lines while the client follows the log.
    log_follower: Option<JoinHandle<()>>,
    /// Set once this connection's restart was answered `RestartingDaemon`:
    /// the connection ends, and tells the serve loop when the answer is
    /// written.
    restarting: Option<std::sync::mpsc::SyncSender<()>>,
    /// Who this connection is (ADR 0072), fixed at the handshake.
    caller: Caller,
    /// What the caller may see and touch (all for the user).
    scope: scope::Scope,
    /// Running `Wait`s, aborted with the connection.
    waits: Vec<JoinHandle<()>>,
    /// Spawns waiting for the user's approval (ADR 0079), aborted (so
    /// withdrawn) with the connection.
    approvals: Vec<JoinHandle<()>>,
    /// Where those hand an approved spawn back to the request loop.
    approved_tx: mpsc::UnboundedSender<Request>,
    /// Approved spawn requests being handled: they skip the approval.
    approved: HashSet<u64>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Leases tied to this connection go with it, and its queued asks
        // (ADR 0077). A restart does not reach here for the other
        // connections: `exec` ends them without destructors.
        if let Err(e) = self.node.leases().conn_closed(self.id, now_ms()) {
            warn!(conn = self.id.0, error = %e, "releasing a closed connection's leases failed");
        }
        for task in self.forwarders.values() {
            task.abort();
        }
        if let Some(task) = &self.log_follower {
            task.abort();
        }
        for task in self.waits.iter().chain(&self.approvals) {
            task.abort();
        }
    }
}

/// Runs a blocking node call on the blocking pool.
async fn blocking<T: Send + 'static>(
    node: &Arc<Node>,
    f: impl FnOnce(&Node) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let node = Arc::clone(node);
    tokio::task::spawn_blocking(move || f(&node))
        .await
        .map_err(io::Error::other)?
}

fn ack(r: io::Result<()>) -> response::Kind {
    match r {
        Ok(()) => response::Kind::Ack(Ack {}),
        Err(e) => error(e.to_string()),
    }
}

/// Searches every agent's output (ADR 0098) on the blocking pool.
async fn search_output(node: &Arc<Node>, r: SearchOutput) -> response::Kind {
    let Some(needle) = Needle::new(&r.text, r.case_sensitive) else {
        return error("search text is empty".into());
    };
    let (limit, per_agent) = (r.limit, r.per_agent);
    match blocking(node, move |n| n.search(needle, limit, per_agent)).await {
        Ok(found) => response::Kind::SearchHits(SearchHits {
            hits: found
                .hits
                .into_iter()
                .map(|h| SearchHit {
                    agent_id: h.agent_id,
                    line: h.line,
                    text: h.text,
                })
                .collect(),
            truncated: found.truncated,
        }),
        Err(e) => error(e.to_string()),
    }
}

fn error(message: String) -> response::Kind {
    response::Kind::Error(fleet_proto::Error { message })
}

impl Connection {
    /// Answers one request; `false` once the writer is gone.
    ///
    /// While a restart freezes the node every request is refused with
    /// "fleetd is restarting"; one already running holds the freeze off
    /// until it finishes, except input, which the freeze interrupts when it
    /// is stuck on an agent's full input queue.
    async fn handle(&mut self, req: Request) -> bool {
        self.sweep_removed().await;
        if let Caller::Agent(a) = self.caller {
            // Boxed: kept out of every request's future.
            let refused = Box::pin(scope::refusal(&self.scope, a, &req.kind));
            if let Some(message) = refused.await {
                return self.respond(req.id, error(message)).await;
            }
            if let (Some(request::Kind::Spawn(_)), Some(timeout)) =
                (&req.kind, self.node.spawn_approval())
                && !self.approved.remove(&req.id)
            {
                // Boxed: kept out of every request's future.
                return Box::pin(self.ask_approval(req, timeout)).await;
            }
        }
        // `Err` refuses the request. No await of its own: each one adds
        // its states to every request's future (see `admit`).
        let admitted = match &req.kind {
            Some(request::Kind::RestartDaemon(r)) => {
                let binary = r.binary.clone();
                // Boxed: kept out of every request's future.
                return Box::pin(self.restart_daemon(req.id, binary)).await;
            }
            Some(request::Kind::Input(_)) if self.shared.stopped.load(Ordering::SeqCst) => Err(()),
            Some(request::Kind::Input(_)) => Ok(None),
            // A long poll: not in flight, so it never holds off a restart
            // (which ends the connection, and the wait with it).
            Some(request::Kind::Wait(_)) if self.shared.stopped.load(Ordering::SeqCst) => Err(()),
            Some(request::Kind::Wait(_)) => Ok(None),
            _ => self.shared.admit().map(Some).ok_or(()),
        };
        let kind = match req.kind {
            _ if admitted.is_err() => restarting(),
            None => error("empty request".into()),
            Some(request::Kind::Spawn(spawn)) if spawn.attach => {
                return self.spawn_attached(req.id, *spawn).await;
            }
            Some(request::Kind::Spawn(spawn)) => {
                let caller = self.caller;
                match blocking(&self.node, move |n| n.spawn_by(caller, *spawn)).await {
                    Ok(info) => response::Kind::Spawned(Box::new(info)),
                    Err(e) => error(e.to_string()),
                }
            }
            Some(request::Kind::List(list)) => {
                let caller = self.caller;
                let listed = blocking(&self.node, move |n| {
                    Ok((n.list_for(caller, &list.selector)?, n.replacements()?))
                })
                .await;
                match listed {
                    Ok((agents, replaced)) => {
                        response::Kind::Agents(AgentList { agents, replaced })
                    }
                    Err(e) => error(e.to_string()),
                }
            }
            Some(request::Kind::Attach(a)) => {
                return self.attach(req.id, a.agent_id, a.observe).await;
            }
            Some(request::Kind::Detach(d)) => {
                self.detach(d.agent_id).await;
                response::Kind::Ack(Ack {})
            }
            Some(request::Kind::Input(input)) => {
                let conn = self.id;
                ack(blocking(&self.node, move |n| {
                    n.input(input.agent_id, conn, &input.data)
                })
                .await)
            }
            Some(request::Kind::Resize(r)) => {
                let conn = self.id;
                ack(blocking(&self.node, move |n| {
                    n.resize(r.agent_id, conn, r.cols, r.rows)
                })
                .await)
            }
            Some(request::Kind::Kill(k)) => {
                ack(blocking(&self.node, move |n| n.kill(k.agent_id)).await)
            }
            Some(request::Kind::ReadHistory(r)) => self.read_history(r).await,
            Some(request::Kind::SetLabels(l)) => ack(blocking(&self.node, move |n| {
                n.set_labels(l.agent_id, l.set, &l.remove)
            })
            .await),
            Some(request::Kind::ReadLogs(r)) => return self.read_logs(req.id, r).await,
            Some(request::Kind::StopLogs(_)) => {
                self.stop_logs();
                response::Kind::Ack(Ack {})
            }
            Some(request::Kind::Restart(r)) => return self.restart(req.id, r).await,
            Some(request::Kind::Forget(f)) => return self.forget(req.id, *f).await,
            Some(request::Kind::Report(r)) => {
                let caller = self.caller;
                let state = r.state();
                ack(blocking(&self.node, move |n| {
                    let question = r.question.as_deref().map(|text| QuestionIn {
                        text,
                        choices: &r.choices,
                    });
                    n.report_question_by(caller, state, &r.message, question)
                })
                .await)
            }
            Some(request::Kind::Answer(a)) => {
                let caller = self.caller;
                ack(blocking(&self.node, move |n| {
                    n.answer(caller, a.agent_id, a.question_id, &a.text)
                })
                .await)
            }
            Some(request::Kind::AcquireLease(a)) => return self.acquire_lease(req.id, *a).await,
            Some(request::Kind::ReleaseLease(r)) => {
                return self.release_lease(req.id, r.name).await;
            }
            Some(request::Kind::ListLeases(_)) => return self.list_leases(req.id).await,
            Some(request::Kind::RestartDaemon(_)) => unreachable!("handled above"),
            // Boxed: kept out of every request's future.
            Some(request::Kind::SendMessage(m)) => {
                Box::pin(wait::send_message(&self.node, self.caller, *m)).await
            }
            Some(request::Kind::ReadInbox(r)) => {
                Box::pin(wait::read_inbox(&self.node, self.caller, r)).await
            }
            Some(request::Kind::ListPendingSpawns(_)) => approval::list(&self.node),
            Some(request::Kind::DecideSpawn(d)) => approval::decide(&self.node, &d),
            Some(request::Kind::ListMessageEdges(r)) => {
                let (since, max) = (r.since_unix_ms, r.max as usize);
                match blocking(&self.node, move |n| n.message_edges(since, max)).await {
                    Ok(edges) => response::Kind::MessageEdges(edges),
                    Err(e) => error(e.to_string()),
                }
            }
            Some(request::Kind::SearchOutput(r)) => search_output(&self.node, r).await,
            Some(request::Kind::Wait(w)) => match self.start_wait(req.id, *w) {
                Ok(()) => return true,
                Err(message) => error(message),
            },
        };
        self.respond(req.id, kind).await
    }

    /// Starts waiting for the user's approval of an agent's spawn (see
    /// [`approval`]); it is answered once decided.
    async fn ask_approval(&mut self, req: Request, timeout_secs: u32) -> bool {
        let id = req.id;
        let asked = approval::ask(
            &self.node,
            self.caller,
            req,
            timeout_secs,
            self.outbox.clone(),
            self.approved_tx.clone(),
        )
        .await;
        match asked {
            Ok(task) => {
                self.approvals.retain(|t| !t.is_finished());
                self.approvals.push(task);
                true
            }
            Err(message) => self.respond(id, error(message)).await,
        }
    }

    /// Starts a `Wait` on its own task (see [`wait`]); it answers when it
    /// matches or times out, while this connection serves other requests.
    /// At most [`wait::MAX_WAITS_PER_CONNECTION`] at a time: each re-reads
    /// the store on every notice, so a looping client must not pile them up.
    fn start_wait(&mut self, id: u64, req: fleet_proto::Wait) -> Result<(), String> {
        self.waits.retain(|t| !t.is_finished());
        if self.waits.len() >= wait::MAX_WAITS_PER_CONNECTION {
            return Err(format!(
                "too many waits on this connection (at most {})",
                wait::MAX_WAITS_PER_CONNECTION
            ));
        }
        let notices = self.node.notices();
        self.waits.push(tokio::spawn(wait::run(
            Arc::clone(&self.node),
            self.caller,
            id,
            req,
            notices,
            self.outbox.clone(),
        )));
        Ok(())
    }

    /// Hands a restart to the serve loop and answers with its outcome. One
    /// at a time: while one is pending, another is refused at once.
    async fn restart_daemon(&mut self, id: u64, binary: String) -> bool {
        if self.shared.restart_pending.swap(true, Ordering::AcqRel) {
            return self
                .respond(id, error("a restart is already in progress".into()))
                .await;
        }
        info!(conn = self.id.0, "restart requested");
        let (reply, answer) = oneshot::channel();
        *lock(&self.shared.restart) = Some(RestartRequest { binary, reply });
        self.shared.restart_asked.notify_one();
        // Dropped unanswered only while the daemon stops.
        let stopping = || error("fleetd is stopping".into());
        match answer.await {
            Ok(RestartReply::Refused(reason)) => self.respond(id, error(reason)).await,
            Ok(RestartReply::Restarting { agents, flushed }) => {
                self.restarting = Some(flushed);
                let reply = response::Kind::RestartingDaemon(RestartingDaemon { agents });
                self.respond(id, reply).await
            }
            Err(_) => self.respond(id, stopping()).await,
        }
    }

    async fn respond(&self, id: u64, kind: response::Kind) -> bool {
        let frame = Frame::Response(Response {
            id,
            kind: Some(kind),
        });
        self.outbox.send(frame).await.is_ok()
    }

    /// Stops forwarding `agent_id`; once this returns, nothing more of its
    /// stream is queued. Dropping the subscription gives up the role.
    async fn detach(&mut self, agent_id: u64) {
        if self.forwarders.contains_key(&agent_id) {
            info!(agent = agent_id, conn = self.id.0, "detached");
        }
        self.stop_forwarder(agent_id).await;
        lock(&self.attached).remove(&agent_id);
    }

    /// Answers with the log's tail; with `follow`, then streams new lines
    /// (replacing an earlier follow).
    async fn read_logs(&mut self, id: u64, r: ReadLogs) -> bool {
        let Some(logs) = logging::logs() else {
            return self
                .respond(id, error("this fleetd has no log file".into()))
                .await;
        };
        self.stop_logs();
        let read = tokio::task::spawn_blocking(move || logs.read(r.tail, r.follow)).await;
        let (lines, follow) = match read {
            Ok(read) => read,
            Err(e) => return self.respond(id, error(e.to_string())).await,
        };
        if !self
            .respond(id, response::Kind::LogLines(LogLines { lines }))
            .await
        {
            return false;
        }
        if let Some(lines) = follow {
            self.log_follower = Some(tokio::spawn(forward_logs(lines, self.outbox.clone())));
        }
        true
    }

    fn stop_logs(&mut self) {
        if let Some(task) = self.log_follower.take() {
            task.abort();
        }
    }

    /// Stops the agent's forwarder (dropping its subscription), if any.
    async fn stop_forwarder(&mut self, agent_id: u64) {
        if let Some(task) = self.forwarders.remove(&agent_id) {
            task.abort();
            let _ = task.await;
        }
    }

    async fn read_history(&self, r: ReadHistory) -> response::Kind {
        let max_lines = r.max_lines.min(HISTORY_PAGE_MAX_LINES);
        let page = blocking(&self.node, move |n| {
            n.read_history(r.agent_id, r.generation, r.before_line, max_lines)
        })
        .await;
        match page {
            Ok(page) => response::Kind::History(HistoryPage {
                agent_id: r.agent_id,
                first_line: page.first_line,
                lines: page.lines,
            }),
            Err(e) => error(e.to_string()),
        }
    }

    /// Replaces any previous subscription to the agent: `AttachOk` is queued
    /// before any of the new subscription's output. The node replaces the
    /// subscription before the old forwarder stops, so a re-attaching
    /// primary keeps its role without a flap. An observing attach (ADR
    /// 0022) never takes or changes the primary role.
    async fn attach(&mut self, id: u64, agent_id: u64, observe: bool) -> bool {
        lock(&self.attached).insert(agent_id);
        let conn = self.id;
        let attached = blocking(&self.node, move |n| {
            if observe {
                n.observe(agent_id)
            } else {
                n.attach(agent_id, conn)
            }
        })
        .await;
        // The old subscription's stream has ended; whatever it queued
        // precedes `AttachOk`.
        self.stop_forwarder(agent_id).await;
        let (ok, sub) = match attached {
            Ok(attached) => attached,
            Err(e) => {
                lock(&self.attached).remove(&agent_id);
                return self.respond(id, error(e.to_string())).await;
            }
        };
        // A lagging old forwarder may have handed the exit event back.
        lock(&self.attached).insert(agent_id);
        info!(
            agent = agent_id,
            conn = self.id.0,
            primary = ok.primary,
            observe,
            "attached"
        );
        if !self
            .respond(id, response::Kind::Attached(Box::new(ok)))
            .await
        {
            return false;
        }
        self.forward(agent_id, sub);
        true
    }

    /// Spawns an agent attached: the reply is `SpawnedAttached`, queued
    /// before any of the agent's output.
    async fn spawn_attached(&mut self, id: u64, spawn: SpawnAgent) -> bool {
        // The agent is marked attached before it starts, so its exit is
        // announced only by its forwarder, after its output.
        let attached = Arc::clone(&self.attached);
        let conn = self.id;
        let caller = self.caller;
        let spawned = blocking(&self.node, move |n| {
            let mut started = None;
            let r = n.spawn_attached_by(caller, spawn, conn, |agent_id| {
                started = Some(agent_id);
                lock(&attached).insert(agent_id);
            });
            Ok((r, started))
        })
        .await;
        let (info, ok, sub): (_, AttachOk, _) = match spawned {
            Ok((Ok(spawned), _)) => spawned,
            Ok((Err(e), started)) => {
                // A failed spawn's id may be handed out again.
                if let Some(agent_id) = started {
                    lock(&self.attached).remove(&agent_id);
                }
                return self.respond(id, error(e.to_string())).await;
            }
            Err(e) => return self.respond(id, error(e.to_string())).await,
        };
        let agent_id = info.id;
        info!(
            agent = agent_id,
            conn = self.id.0,
            primary = ok.primary,
            "attached"
        );
        let reply = response::Kind::SpawnedAttached(Box::new(SpawnedAttached {
            agent: Some(info),
            attached: Some(ok),
        }));
        if !self.respond(id, reply).await {
            return false;
        }
        self.forward(agent_id, sub);
        true
    }

    /// Restarts an exited or lost agent, attached if asked (as
    /// [`Connection::spawn_attached`]). The old agent's `AgentRemoved` is
    /// queued right after the reply; other connections get it from the
    /// broadcast. If the node could not record the replacement, the old
    /// agent stays known and none is queued (the node announces none).
    async fn restart(&mut self, id: u64, r: RestartAgent) -> bool {
        let old_id = r.agent_id;
        let conn = self.id;
        let caller = self.caller;
        let (info, attached) = if r.attach {
            let attached = Arc::clone(&self.attached);
            let restarted = blocking(&self.node, move |n| {
                let mut started = None;
                let res = n.restart_attached_by(caller, old_id, r.cols, r.rows, conn, |agent_id| {
                    started = Some(agent_id);
                    lock(&attached).insert(agent_id);
                });
                Ok((res, started))
            })
            .await;
            match restarted {
                Ok((Ok((out, ok, sub)), _)) => (out, Some((ok, sub))),
                Ok((Err(e), started)) => {
                    if let Some(agent_id) = started {
                        lock(&self.attached).remove(&agent_id);
                    }
                    return self.respond(id, error(e.to_string())).await;
                }
                Err(e) => return self.respond(id, error(e.to_string())).await,
            }
        } else {
            match blocking(&self.node, move |n| {
                n.restart_by(caller, old_id, r.cols, r.rows, Some(conn))
            })
            .await
            {
                Ok(out) => (out, None),
                Err(e) => return self.respond(id, error(e.to_string())).await,
            }
        };
        let replaced = info.replaced;
        let info = info.info;
        let new_id = info.id;
        info!(agent = new_id, old = old_id, conn = self.id.0, "restarted");
        if replaced {
            self.forget_local(old_id).await;
        }
        let (ok, sub) = match attached {
            Some((ok, sub)) => (Some(ok), Some(sub)),
            None => (None, None),
        };
        let reply = response::Kind::Restarted(Box::new(Restarted {
            old_id,
            agent: Some(info),
            attached: ok,
        }));
        if !self.respond(id, reply).await {
            return false;
        }
        if replaced && !self.queue_removed(old_id, Some(new_id)).await {
            return false;
        }
        if let Some(sub) = sub {
            self.forward(new_id, sub);
        }
        true
    }

    /// Forgets exited or lost agents; their `AgentRemoved` events are queued
    /// right after the reply. A store error midway is answered with an
    /// `Error`, still followed by `AgentRemoved` for the agents forgotten
    /// before it (other connections got theirs from the broadcast).
    async fn forget(&mut self, id: u64, f: ForgetAgents) -> bool {
        let conn = self.id;
        let caller = self.caller;
        let outcome = blocking(&self.node, move |n| {
            Ok(n.forget_by(caller, &f.ids, f.exited_only, &f.selector, Some(conn)))
        })
        .await;
        let (forgotten, reply) = match outcome {
            Ok(Ok(outcome)) => (
                outcome.forgotten.clone(),
                response::Kind::Forgotten(Forgotten {
                    ids: outcome.forgotten,
                    refused: outcome.refused,
                }),
            ),
            Ok(Err(e)) => (e.partial.forgotten, error(e.error.to_string())),
            Err(e) => return self.respond(id, error(e.to_string())).await,
        };
        for &agent_id in &forgotten {
            self.forget_local(agent_id).await;
        }
        if !self.respond(id, reply).await {
            return false;
        }
        for agent_id in forgotten {
            if !self.queue_removed(agent_id, None).await {
                return false;
            }
        }
        true
    }

    /// Drops a forgotten agent from this connection: its forwarder (whose
    /// stream already ended with the exit) and its `attached` mark.
    async fn forget_local(&mut self, agent_id: u64) {
        self.stop_forwarder(agent_id).await;
        lock(&self.attached).remove(&agent_id);
    }

    /// Another connection removed `agent_id`: drops this connection's
    /// finished forwarder and `attached` mark for it, so a long-lived
    /// connection does not keep them. A forwarder still running (the
    /// removal can precede the exit) is left to deliver the exit and
    /// remembered, to be dropped by a later removal or request once done.
    async fn removed_elsewhere(&mut self, agent_id: u64) {
        self.removed_pending.insert(agent_id);
        self.sweep_removed().await;
    }

    /// Drops the state of removed agents whose forwarder has finished.
    async fn sweep_removed(&mut self) {
        if self.removed_pending.is_empty() {
            return;
        }
        let done: Vec<u64> = self
            .removed_pending
            .iter()
            .copied()
            .filter(|id| self.forwarders.get(id).is_none_or(JoinHandle::is_finished))
            .collect();
        for agent_id in done {
            self.removed_pending.remove(&agent_id);
            self.forget_local(agent_id).await;
        }
    }

    /// Queues this connection's own `AgentRemoved`, which its removal
    /// forwarder skips.
    async fn queue_removed(&self, agent_id: u64, replaced_by: Option<u64>) -> bool {
        self.outbox
            .send(removed_event(AgentRemoved {
                agent_id,
                replaced_by,
            }))
            .await
            .is_ok()
    }

    /// Starts forwarding `sub`, the agent's subscription.
    fn forward(&mut self, agent_id: u64, sub: Subscription) {
        let task = tokio::spawn(forward_output(
            Arc::clone(&self.node),
            self.id,
            agent_id,
            sub,
            self.outbox.clone(),
            Arc::clone(&self.attached),
        ));
        self.forwarders.insert(agent_id, task);
    }
}

fn exited_event(agent_id: u64, exit_code: i32) -> Frame {
    Frame::Event(Event {
        kind: Some(event::Kind::Exited(fleet_proto::AgentExited {
            agent_id,
            exit_code,
        })),
    })
}

/// Streams one agent's output, then its exit or `Event::Lagged`.
async fn forward_output(
    node: Arc<Node>,
    conn: ConnId,
    agent_id: u64,
    mut sub: Subscription,
    outbox: mpsc::Sender<Frame>,
    attached: Arc<Mutex<HashSet<u64>>>,
) {
    while let Some(item) = sub.recv().await {
        let frame = match item {
            StreamItem::Output(data) => Frame::Output { agent_id, data },
            StreamItem::Role(primary) => {
                info!(agent = agent_id, conn = conn.0, primary, "role changed");
                Frame::Event(Event {
                    kind: Some(event::Kind::Role(AttachRole { agent_id, primary })),
                })
            }
            StreamItem::Resized { cols, rows } => Frame::Event(Event {
                kind: Some(event::Kind::AgentResized(AgentResized {
                    agent_id,
                    cols: u32::from(cols),
                    rows: u32::from(rows),
                })),
            }),
            StreamItem::Exited(code) => {
                let _ = outbox.send(exited_event(agent_id, code)).await;
                return;
            }
            StreamItem::Lagged => {
                // Hand the exit event back to the exit-event forwarder. If
                // the agent already exited, that event was skipped (and its
                // `Exited` item discarded with the lag), so send it here. An
                // exit racing this hand-off may be announced twice.
                lock(&attached).remove(&agent_id);
                warn!(
                    agent = agent_id,
                    conn = conn.0,
                    "subscription dropped: the client fell behind (Lagged)"
                );
                let lagged = Frame::Event(Event {
                    kind: Some(event::Kind::Lagged(Lagged { agent_id })),
                });
                if outbox.send(lagged).await.is_err() {
                    return;
                }
                if let Some(code) = node.exit_code(agent_id) {
                    let _ = outbox.send(exited_event(agent_id, code)).await;
                }
                return;
            }
        };
        if outbox.send(frame).await.is_err() {
            return;
        }
    }
}

/// Announces exits of agents this connection is not attached to.
async fn forward_exits(
    mut exits: broadcast::Receiver<fleet_proto::AgentExited>,
    outbox: mpsc::Sender<Frame>,
    attached: Arc<Mutex<HashSet<u64>>>,
    scope: scope::Scope,
) {
    loop {
        match exits.recv().await {
            Ok(e) => {
                if lock(&attached).contains(&e.agent_id) || !scope.sees(e.agent_id).await {
                    continue;
                }
                if outbox
                    .send(exited_event(e.agent_id, e.exit_code))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(missed = n, "a connection missed exit events");
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

fn removed_event(e: AgentRemoved) -> Frame {
    Frame::Event(Event {
        kind: Some(event::Kind::AgentRemoved(e)),
    })
}

/// Announces removals this connection did not cause (its own are queued
/// after its response). Missed events are not resent: clients recover by
/// relisting, whose `replaced` map carries restarts. Each forwarded removal
/// is also handed to the request loop (`local`).
async fn forward_removed(
    mut removed: broadcast::Receiver<Removed>,
    outbox: mpsc::Sender<Frame>,
    own: ConnId,
    local: mpsc::UnboundedSender<u64>,
    scope: scope::Scope,
) {
    loop {
        match removed.recv().await {
            Ok(r) => {
                if r.origin == Some(own) {
                    continue;
                }
                // A forgotten agent's row is gone: only the cache (warmed
                // at the handshake, fed by its exit) still places it; a
                // restarted one is placed by its successor.
                let seen = scope.sees_now_cached(r.event.agent_id)
                    || match r.event.replaced_by {
                        Some(new) => scope.sees(new).await,
                        None => false,
                    };
                if !seen {
                    continue;
                }
                let agent_id = r.event.agent_id;
                if outbox.send(removed_event(r.event)).await.is_err() {
                    return;
                }
                let _ = local.send(agent_id);
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(
                    conn = own.0,
                    missed = n,
                    "a connection missed removal events"
                );
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Forwards every agent's activity. While the connection is backed up,
/// events wait in the receiver, which keeps only each agent's newest one.
/// An event is taken only once an outbox slot is reserved, and queued
/// without waiting, so it never follows that agent's exit event.
/// An agent caller gets only its descendants' activity.
async fn forward_activity(
    activity: ActivityReceiver,
    outbox: mpsc::Sender<Frame>,
    scope: scope::Scope,
) {
    loop {
        loop {
            let Ok(slot) = outbox.reserve().await else {
                return;
            };
            let delivered = activity.deliver_next(|a| {
                if scope.sees_now(a.agent_id) {
                    slot.send(Frame::Event(Event {
                        kind: Some(event::Kind::Activity(a)),
                    }));
                }
            });
            if !delivered {
                break;
            }
        }
        activity.changed().await;
    }
}

/// Streams new log lines (a follower that fell behind gets a marker line
/// from the log writer, see [`logging`]).
async fn forward_logs(mut lines: mpsc::Receiver<String>, outbox: mpsc::Sender<Frame>) {
    while let Some(line) = lines.recv().await {
        let frame = Frame::Event(Event {
            kind: Some(event::Kind::LogLine(LogLine { line })),
        });
        if outbox.send(frame).await.is_err() {
            return;
        }
    }
}

/// Writes queued frames, flushing whenever the queue runs dry.
async fn write_frames(conn: ConnId, mut writer: Writer, mut outbox: mpsc::Receiver<Frame>) {
    while let Some(frame) = outbox.recv().await {
        let mut next = Some(frame);
        while let Some(frame) = next {
            if let Err(e) = writer.feed(frame).await {
                debug!(conn = conn.0, error = %e, "write failed");
                return;
            }
            next = outbox.try_recv().ok();
        }
        if let Err(e) = writer.flush().await {
            debug!(conn = conn.0, error = %e, "write failed");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_node::store::Labels;

    async fn wait_until(mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "timed out");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn removal_elsewhere_drops_the_finished_forwarder() {
        let dir = tempfile::tempdir().unwrap();
        let node = Arc::new(Node::open(dir.path()).unwrap());
        let (mut conn, mut rx) = test_conn(Arc::clone(&node));
        let spawn = SpawnAgent {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
            attach: true,
            ..Default::default()
        };
        assert!(
            conn.handle(Request {
                id: 1,
                kind: Some(request::Kind::Spawn(Box::new(spawn))),
            })
            .await
        );
        let id = loop {
            if let Some(Frame::Event(Event {
                kind: Some(event::Kind::Exited(e)),
            })) = rx.recv().await
            {
                break e.agent_id;
            }
        };
        wait_until(|| conn.forwarders[&id].is_finished()).await;
        assert!(lock(&conn.attached).contains(&id));

        // Another connection forgets it (retried until the node has
        // retired the agent).
        let other = Some(ConnId(u64::MAX - 1));
        let n = Arc::clone(&node);
        wait_until(move || {
            n.forget(&[id], false, &Labels::new(), other)
                .unwrap()
                .forgotten
                == vec![id]
        })
        .await;
        conn.removed_elsewhere(id).await;
        assert!(conn.forwarders.is_empty());
        assert!(!lock(&conn.attached).contains(&id));
    }

    fn test_conn(node: Arc<Node>) -> (Connection, mpsc::Receiver<Frame>) {
        let (_, conn, rx) = test_server_conn(node);
        (conn, rx)
    }

    /// A server and one connection of it.
    fn test_server_conn(node: Arc<Node>) -> (Server, Connection, mpsc::Receiver<Frame>) {
        let (outbox, rx) = mpsc::channel(OUTBOX_CAPACITY);
        let server = Server::new(
            Arc::clone(&node),
            Identity::current(),
            RestoreSummary::default(),
        );
        let conn = Connection {
            node,
            shared: Arc::clone(&server.shared),
            id: ConnId(u64::MAX),
            outbox,
            forwarders: HashMap::new(),
            attached: Arc::new(Mutex::new(HashSet::new())),
            removed_pending: HashSet::new(),
            log_follower: None,
            restarting: None,
            caller: Caller::User,
            scope: scope::Scope::user(),
            waits: Vec::new(),
            approvals: Vec::new(),
            approved_tx: mpsc::unbounded_channel().0,
            approved: HashSet::new(),
        };
        (server, conn, rx)
    }

    /// The response to the next request sent, skipping other frames.
    async fn answer(
        conn: &mut Connection,
        rx: &mut mpsc::Receiver<Frame>,
        kind: request::Kind,
    ) -> response::Kind {
        assert!(
            conn.handle(Request {
                id: 9,
                kind: Some(kind)
            })
            .await
        );
        loop {
            if let Some(Frame::Response(r)) = rx.recv().await {
                return r.kind.unwrap();
            }
        }
    }

    fn is_restarting(kind: &response::Kind) -> bool {
        matches!(kind, response::Kind::Error(e) if e.message == "fleetd is restarting")
    }

    #[tokio::test]
    async fn requests_are_refused_while_stopped_for_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let node = Arc::new(Node::open(dir.path()).unwrap());
        let (server, mut conn, mut rx) = test_server_conn(node);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let stopped = server.restarts().stop_requests(deadline).unwrap();
        let resize = request::Kind::Resize(fleet_proto::Resize {
            agent_id: 1,
            cols: 80,
            rows: 24,
        });
        assert!(is_restarting(&answer(&mut conn, &mut rx, resize).await));
        let input = request::Kind::Input(fleet_proto::SendInput {
            agent_id: 1,
            data: b"x".to_vec(),
        });
        assert!(is_restarting(&answer(&mut conn, &mut rx, input).await));
        drop(stopped);
        let list = request::Kind::List(Default::default());
        assert!(matches!(
            answer(&mut conn, &mut rx, list).await,
            response::Kind::Agents(_)
        ));
    }

    #[tokio::test]
    async fn stopping_waits_for_requests_being_handled() {
        let dir = tempfile::tempdir().unwrap();
        let node = Arc::new(Node::open(dir.path()).unwrap());
        let (server, mut conn, mut rx) = test_server_conn(node);
        // A request being handled holds the stop off past its deadline...
        let handled = server.shared.admit().unwrap();
        let soon = std::time::Instant::now() + Duration::from_millis(50);
        assert!(server.restarts().stop_requests(soon).is_none());
        // ...after which requests are admitted again.
        drop(handled);
        let list = request::Kind::List(Default::default());
        assert!(matches!(
            answer(&mut conn, &mut rx, list).await,
            response::Kind::Agents(_)
        ));
        let later = std::time::Instant::now() + Duration::from_secs(1);
        assert!(server.restarts().stop_requests(later).is_some());
    }

    /// A stand-in forwarder for `id` that runs until the sender is used.
    fn running_forwarder(conn: &mut Connection, id: u64) -> tokio::sync::oneshot::Sender<()> {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        conn.forwarders.insert(
            id,
            tokio::spawn(async move {
                let _ = done_rx.await;
            }),
        );
        lock(&conn.attached).insert(id);
        done_tx
    }

    #[tokio::test]
    async fn removal_elsewhere_leaves_a_running_forwarder_until_a_later_removal() {
        let dir = tempfile::tempdir().unwrap();
        let (mut conn, _rx) = test_conn(Arc::new(Node::open(dir.path()).unwrap()));
        let done = running_forwarder(&mut conn, 7);

        conn.removed_elsewhere(7).await;
        assert!(conn.forwarders.contains_key(&7), "still delivering");
        assert!(lock(&conn.attached).contains(&7));

        done.send(()).unwrap();
        wait_until(|| conn.forwarders[&7].is_finished()).await;
        conn.removed_elsewhere(8).await;
        assert!(conn.forwarders.is_empty());
        assert!(!lock(&conn.attached).contains(&7));
    }

    #[tokio::test]
    async fn removal_elsewhere_leaves_a_running_forwarder_until_a_later_request() {
        let dir = tempfile::tempdir().unwrap();
        let (mut conn, mut rx) = test_conn(Arc::new(Node::open(dir.path()).unwrap()));
        let done = running_forwarder(&mut conn, 7);

        conn.removed_elsewhere(7).await;
        assert!(conn.forwarders.contains_key(&7), "still delivering");

        done.send(()).unwrap();
        wait_until(|| conn.forwarders[&7].is_finished()).await;
        assert!(
            conn.handle(Request {
                id: 1,
                kind: Some(request::Kind::List(Default::default())),
            })
            .await
        );
        assert!(matches!(rx.recv().await, Some(Frame::Response(_))));
        assert!(conn.forwarders.is_empty());
        assert!(!lock(&conn.attached).contains(&7));
    }

    fn wait_req(timeout_ms: u64, states: &[fleet_proto::WaitState]) -> request::Kind {
        request::Kind::Wait(Box::new(fleet_proto::Wait {
            agent_ids: Vec::new(),
            states: states.iter().map(|s| *s as i32).collect(),
            timeout_ms,
        }))
    }

    async fn next_response(rx: &mut mpsc::Receiver<Frame>) -> Response {
        loop {
            if let Some(Frame::Response(r)) = rx.recv().await {
                return r;
            }
        }
    }

    #[tokio::test]
    async fn a_wait_answers_on_a_message_while_the_connection_serves_on() {
        let dir = tempfile::tempdir().unwrap();
        let (mut conn, mut rx) = test_conn(Arc::new(Node::open(dir.path()).unwrap()));
        let wait = wait_req(10_000, &[fleet_proto::WaitState::Message]);
        assert!(
            conn.handle(Request {
                id: 1,
                kind: Some(wait)
            })
            .await
        );
        // The connection answers the next request before the wait.
        let send = request::Kind::SendMessage(Box::new(fleet_proto::SendMessage {
            body: "note to self".into(),
            to_user: true,
            ..Default::default()
        }));
        assert!(
            conn.handle(Request {
                id: 2,
                kind: Some(send)
            })
            .await
        );
        let first = next_response(&mut rx).await;
        assert_eq!(first.id, 2);
        assert!(matches!(first.kind, Some(response::Kind::MessagesSent(_))));
        let waited = next_response(&mut rx).await;
        assert_eq!(waited.id, 1);
        let Some(response::Kind::WaitResult(r)) = waited.kind else {
            panic!("{waited:?}");
        };
        assert_eq!(r.matches.len(), 1);
        assert_eq!(r.matches[0].message.as_ref().unwrap().body, "note to self");
    }

    #[tokio::test]
    async fn a_wait_times_out_with_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        let (mut conn, mut rx) = test_conn(Arc::new(Node::open(dir.path()).unwrap()));
        assert!(
            conn.handle(Request {
                id: 1,
                kind: Some(wait_req(50, &[]))
            })
            .await
        );
        let Some(response::Kind::WaitResult(r)) = next_response(&mut rx).await.kind else {
            panic!("no wait result");
        };
        assert!(r.matches.is_empty());
    }

    #[tokio::test]
    async fn waits_per_connection_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        let (mut conn, mut rx) = test_conn(Arc::new(Node::open(dir.path()).unwrap()));
        for id in 0..wait::MAX_WAITS_PER_CONNECTION as u64 {
            let kind = Some(wait_req(60_000, &[]));
            assert!(conn.handle(Request { id, kind }).await);
        }
        let kind = Some(wait_req(60_000, &[]));
        assert!(conn.handle(Request { id: 99, kind }).await);
        let refused = next_response(&mut rx).await;
        assert_eq!(refused.id, 99);
        assert!(
            matches!(&refused.kind, Some(response::Kind::Error(e)) if e.message.starts_with("too many waits")),
            "{refused:?}"
        );
        // A finished wait frees its slot.
        conn.waits[0].abort();
        wait_until(|| conn.waits[0].is_finished()).await;
        let kind = Some(wait_req(60_000, &[]));
        assert!(conn.handle(Request { id: 100, kind }).await);
        assert_eq!(conn.waits.len(), wait::MAX_WAITS_PER_CONNECTION);
    }

    #[tokio::test]
    async fn a_wait_does_not_hold_off_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (server, mut conn, _rx) = test_server_conn(Arc::new(Node::open(dir.path()).unwrap()));
        assert!(
            conn.handle(Request {
                id: 1,
                kind: Some(wait_req(60_000, &[]))
            })
            .await
        );
        let soon = std::time::Instant::now() + Duration::from_millis(500);
        assert!(server.restarts().stop_requests(soon).is_some());
    }

    #[tokio::test]
    async fn forward_removed_hands_other_connections_removals_to_the_loop() {
        let (tx, removed) = broadcast::channel(8);
        let (outbox, mut out_rx) = mpsc::channel(OUTBOX_CAPACITY);
        let (local, mut local_rx) = mpsc::unbounded_channel();
        let own = ConnId(1);
        let task = tokio::spawn(forward_removed(
            removed,
            outbox,
            own,
            local,
            scope::Scope::user(),
        ));
        let removal = |agent_id, origin| Removed {
            event: AgentRemoved {
                agent_id,
                replaced_by: None,
            },
            origin,
        };
        tx.send(removal(4, Some(own))).unwrap();
        tx.send(removal(5, Some(ConnId(2)))).unwrap();

        assert_eq!(local_rx.recv().await, Some(5), "own removal skipped");
        match out_rx.recv().await {
            Some(Frame::Event(Event {
                kind: Some(event::Kind::AgentRemoved(e)),
            })) => assert_eq!(e.agent_id, 5),
            other => panic!("unexpected {other:?}"),
        }
        task.abort();
    }
}
