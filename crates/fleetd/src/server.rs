//! Protocol connections: handshake, then requests answered in order while
//! attached agents' output and exit events stream alongside.
//!
//! Each connection has one writer task owning the socket's write half; the
//! request loop, the per-agent forwarders and the exit-event forwarder feed it
//! through a bounded channel. A client that stops reading thus stalls its
//! forwarders, its subscriptions fill up, and it is told to re-attach
//! (`lagged: re-attach`) instead of slowing the agent down.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use fleet_node::node::{Node, StreamItem, Subscription};
use fleet_proto::{
    Ack, AgentList, Event, Frame, FrameCodec, HandshakeReject, HelloOk, MAX_FRAME,
    PROTOCOL_VERSION, Request, Response, event, request, response,
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

const LAGGED: &str = "lagged: re-attach";

type Writer = FramedWrite<OwnedWriteHalf, FrameCodec>;
type Reader = FramedRead<OwnedReadHalf, FrameCodec>;

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
    if !handshake(&mut reader, &mut writer).await {
        return;
    }
    reader.decoder_mut().set_max(MAX_FRAME);

    let (outbox, rx) = mpsc::channel(OUTBOX_CAPACITY);
    let mut conn = Connection {
        node,
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
            Some(request::Kind::Spawn(spawn)) => {
                match blocking(&self.node, move |n| n.spawn(spawn)).await {
                    Ok(info) => response::Kind::Spawned(info),
                    Err(e) => error(e.to_string()),
                }
            }
            Some(request::Kind::List(_)) => match blocking(&self.node, |n| n.list()).await {
                Ok(agents) => response::Kind::Agents(AgentList { agents }),
                Err(e) => error(e.to_string()),
            },
            Some(request::Kind::Attach(a)) => return self.attach(req.id, a.agent_id).await,
            Some(request::Kind::Detach(d)) => {
                self.detach(d.agent_id).await;
                response::Kind::Ack(Ack {})
            }
            Some(request::Kind::Input(input)) => {
                ack(blocking(&self.node, move |n| n.input(input.agent_id, &input.data)).await)
            }
            Some(request::Kind::Resize(r)) => {
                ack(blocking(&self.node, move |n| n.resize(r.agent_id, r.cols, r.rows)).await)
            }
            Some(request::Kind::Kill(k)) => {
                ack(blocking(&self.node, move |n| n.kill(k.agent_id)).await)
            }
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
    /// stream is queued.
    async fn detach(&mut self, agent_id: u64) {
        if let Some(task) = self.forwarders.remove(&agent_id) {
            task.abort();
            let _ = task.await;
        }
        lock(&self.attached).remove(&agent_id);
    }

    /// Replaces any previous subscription to the agent: `AttachOk` is queued
    /// before any of the new subscription's output.
    async fn attach(&mut self, id: u64, agent_id: u64) -> bool {
        self.detach(agent_id).await;
        lock(&self.attached).insert(agent_id);
        let (ok, sub) = match blocking(&self.node, move |n| n.attach(agent_id)).await {
            Ok(attached) => attached,
            Err(e) => {
                lock(&self.attached).remove(&agent_id);
                return self.respond(id, error(e.to_string())).await;
            }
        };
        if !self.respond(id, response::Kind::Attached(ok)).await {
            return false;
        }
        let task = tokio::spawn(forward_output(
            Arc::clone(&self.node),
            agent_id,
            sub,
            self.outbox.clone(),
            Arc::clone(&self.attached),
        ));
        self.forwarders.insert(agent_id, task);
        true
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

/// Streams one agent's output, then its exit or a lag notice.
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
                let lagged = Frame::Response(Response {
                    id: 0,
                    kind: Some(error(LAGGED.into())),
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
