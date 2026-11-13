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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use fleet_node::history::HISTORY_PAGE_MAX_LINES;
use fleet_node::node::{ActivityReceiver, ConnId, Node, StreamItem, Subscription};
use fleet_proto::{
    Ack, AgentList, AttachOk, AttachRole, Event, Frame, FrameCodec, HandshakeReject, HelloOk,
    HistoryPage, Lagged, MAX_FRAME, PROTOCOL_VERSION, ReadHistory, Request, Response, SpawnAgent,
    SpawnedAttached, event, request, response,
};
use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::codec::{FramedRead, FramedWrite};

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

type Writer = FramedWrite<OwnedWriteHalf, FrameCodec>;
type Reader = FramedRead<OwnedReadHalf, FrameCodec>;

/// Source of connection ids, unique for the daemon's lifetime.
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) struct Server {
    node: Arc<Node>,
    slots: Arc<Semaphore>,
}

impl Server {
    pub(crate) fn new(node: Arc<Node>) -> Self {
        Server {
            node,
            slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        }
    }

    /// Serves `stream` on its own task, or rejects it when full.
    pub(crate) fn accept(&self, stream: UnixStream) {
        match Arc::clone(&self.slots).try_acquire_owned() {
            Ok(slot) => {
                tokio::spawn(connection(Arc::clone(&self.node), stream, slot));
            }
            Err(_) => {
                tokio::spawn(async move {
                    let (_, wr) = stream.into_split();
                    let mut writer = FramedWrite::new(wr, FrameCodec::new());
                    reject(&mut writer, "too many connections".into()).await;
                });
            }
        }
    }
}

async fn reject(writer: &mut Writer, reason: String) {
    let frame = Frame::Reject(HandshakeReject { reason });
    let _ = tokio::time::timeout(HANDSHAKE_WRITE_TIMEOUT, writer.send(frame)).await;
}

/// Reads the `Hello` and answers it; `true` if the connection may proceed.
async fn handshake(reader: &mut Reader, writer: &mut Writer) -> bool {
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, reader.next()).await {
        Err(_) => {
            eprintln!("fleetd: connection closed: no hello within {HELLO_TIMEOUT:?}");
            return false;
        }
        Ok(None) => return false,
        Ok(Some(Err(e))) => {
            eprintln!("fleetd: connection closed: bad hello: {e}");
            return false;
        }
        Ok(Some(Ok(Frame::Hello(hello)))) => hello,
        Ok(Some(Ok(_))) => {
            eprintln!("fleetd: connection closed: first frame is not a hello");
            return false;
        }
    };
    if hello.protocol_version != PROTOCOL_VERSION {
        let reason = format!(
            "unsupported protocol version {} (server speaks {PROTOCOL_VERSION})",
            hello.protocol_version
        );
        reject(writer, reason).await;
        return false;
    }
    let ok = Frame::HelloOk(HelloOk {
        protocol_version: PROTOCOL_VERSION,
        server: "fleetd".into(),
    });
    matches!(
        tokio::time::timeout(HANDSHAKE_WRITE_TIMEOUT, writer.send(ok)).await,
        Ok(Ok(()))
    )
}

async fn connection(node: Arc<Node>, stream: UnixStream, _slot: OwnedSemaphorePermit) {
    let (rd, wr) = stream.into_split();
    let mut reader = FramedRead::new(rd, FrameCodec::with_max(HELLO_MAX_FRAME));
    let mut writer = FramedWrite::new(wr, FrameCodec::new());
    // Subscribed before `HelloOk`, so no exit after the handshake is missed.
    let exits = node.exit_events();
    let activity = node.activity_events();
    if !handshake(&mut reader, &mut writer).await {
        return;
    }
    reader.decoder_mut().set_max(MAX_FRAME);

    let (outbox, rx) = mpsc::channel(OUTBOX_CAPACITY);
    let mut conn = Connection {
        node,
        id: ConnId(NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed)),
        outbox,
        forwarders: HashMap::new(),
        attached: Arc::new(Mutex::new(HashSet::new())),
    };
    let tasks = Tasks(vec![
        tokio::spawn(write_frames(writer, rx)),
        tokio::spawn(forward_exits(
            exits,
            conn.outbox.clone(),
            Arc::clone(&conn.attached),
        )),
        tokio::spawn(forward_activity(activity, conn.outbox.clone())),
    ]);

    while let Some(frame) = reader.next().await {
        match frame {
            Ok(Frame::Request(req)) => {
                if !conn.handle(req).await {
                    break;
                }
            }
            Ok(_) => {
                eprintln!("fleetd: connection closed: unexpected frame from client");
                break;
            }
            Err(e) => {
                eprintln!("fleetd: connection closed: {e}");
                break;
            }
        }
    }
    // Dropping `conn` and `tasks` aborts the forwarders (dropping their
    // subscriptions) and the writer, closing the socket.
    drop(conn);
    drop(tasks);
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
    id: ConnId,
    outbox: mpsc::Sender<Frame>,
    /// Output forwarder per attached agent.
    forwarders: HashMap<u64, JoinHandle<()>>,
    /// Agents whose exit event the forwarder delivers (in order after their
    /// output), so the exit-event forwarder skips them.
    attached: Arc<Mutex<HashSet<u64>>>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        for task in self.forwarders.values() {
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

fn error(message: String) -> response::Kind {
    response::Kind::Error(fleet_proto::Error { message })
}

impl Connection {
    /// Answers one request; `false` once the writer is gone.
    async fn handle(&mut self, req: Request) -> bool {
        let kind = match req.kind {
            None => error("empty request".into()),
            Some(request::Kind::Spawn(spawn)) if spawn.attach => {
                return self.spawn_attached(req.id, spawn).await;
            }
            Some(request::Kind::Spawn(spawn)) => {
                match blocking(&self.node, move |n| n.spawn(spawn)).await {
                    Ok(info) => response::Kind::Spawned(info),
                    Err(e) => error(e.to_string()),
                }
            }
            Some(request::Kind::List(list)) => {
                match blocking(&self.node, move |n| n.list_matching(&list.selector)).await {
                    Ok(agents) => response::Kind::Agents(AgentList { agents }),
                    Err(e) => error(e.to_string()),
                }
            }
            Some(request::Kind::Attach(a)) => return self.attach(req.id, a.agent_id).await,
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
        };
        self.respond(req.id, kind).await
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
        self.stop_forwarder(agent_id).await;
        lock(&self.attached).remove(&agent_id);
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
    /// primary keeps its role without a flap.
    async fn attach(&mut self, id: u64, agent_id: u64) -> bool {
        lock(&self.attached).insert(agent_id);
        let conn = self.id;
        let attached = blocking(&self.node, move |n| n.attach(agent_id, conn)).await;
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
        if !self.respond(id, response::Kind::Attached(ok)).await {
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
        let spawned = blocking(&self.node, move |n| {
            let mut started = None;
            let r = n.spawn_attached(spawn, conn, |agent_id| {
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
        let reply = response::Kind::SpawnedAttached(SpawnedAttached {
            agent: Some(info),
            attached: Some(ok),
        });
        if !self.respond(id, reply).await {
            return false;
        }
        self.forward(agent_id, sub);
        true
    }

    /// Starts forwarding `sub`, the agent's subscription.
    fn forward(&mut self, agent_id: u64, sub: Subscription) {
        let task = tokio::spawn(forward_output(
            Arc::clone(&self.node),
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
    agent_id: u64,
    mut sub: Subscription,
    outbox: mpsc::Sender<Frame>,
    attached: Arc<Mutex<HashSet<u64>>>,
) {
    while let Some(item) = sub.recv().await {
        let frame = match item {
            StreamItem::Output(data) => Frame::Output { agent_id, data },
            StreamItem::Role(primary) => Frame::Event(Event {
                kind: Some(event::Kind::Role(AttachRole { agent_id, primary })),
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
) {
    loop {
        match exits.recv().await {
            Ok(e) => {
                if lock(&attached).contains(&e.agent_id) {
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
                eprintln!("fleetd: client missed {n} exit events");
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Forwards every agent's activity. While the connection is backed up,
/// events wait in the receiver, which keeps only each agent's newest one.
/// An event is taken only once an outbox slot is reserved, and queued
/// without waiting, so it never follows that agent's exit event.
async fn forward_activity(activity: ActivityReceiver, outbox: mpsc::Sender<Frame>) {
    loop {
        loop {
            let Ok(slot) = outbox.reserve().await else {
                return;
            };
            let delivered = activity.deliver_next(|a| {
                slot.send(Frame::Event(Event {
                    kind: Some(event::Kind::Activity(a)),
                }));
            });
            if !delivered {
                break;
            }
        }
        activity.changed().await;
    }
}

/// Writes queued frames, flushing whenever the queue runs dry.
async fn write_frames(mut writer: Writer, mut outbox: mpsc::Receiver<Frame>) {
    while let Some(frame) = outbox.recv().await {
        let mut next = Some(frame);
        while let Some(frame) = next {
            if let Err(e) = writer.feed(frame).await {
                eprintln!("fleetd: write failed: {e}");
                return;
            }
            next = outbox.try_recv().ok();
        }
        if let Err(e) = writer.flush().await {
            eprintln!("fleetd: write failed: {e}");
            return;
        }
    }
}
