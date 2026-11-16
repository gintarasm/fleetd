//! Mailbox requests (ADR 0076, spec §3): `fleet message`, `fleet inbox`,
//! and `fleet wait`, a server-side long poll.
//!
//! A wait runs as a task of its connection and answers through the
//! connection's outbox, so the connection keeps reading: a client that goes
//! away ends its waits (the connection aborts them), and a wait is never a
//! request in flight, so it does not hold off a daemon restart (the restart
//! closes the connection; the CLI reconnects and waits on).
//!
//! It wakes on the node's mailbox notices (an in-node broadcast, never the
//! PTY output path) and re-checks; a waiter that lags re-checks too.

use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use fleet_node::mailbox::{Notice, WaitState};
use fleet_node::node::{Node, WaitSpec, wait_state_from_proto};
use fleet_node::scope::Caller;
use fleet_proto::{Frame, Inbox, ReadInbox, Response, SendMessage, Wait, response};
use tokio::sync::{broadcast, mpsc};

/// Waits one connection may run at once; more are refused (exit 2 in the
/// CLI). Each re-reads the store on every notice.
pub(crate) const MAX_WAITS_PER_CONNECTION: usize = 16;

/// The longest wait one request may ask for.
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(24 * 3600);

fn error(message: String) -> response::Kind {
    response::Kind::Error(fleet_proto::Error { message })
}

/// The wait `req` asks for; `Err` with the refusal for a bad request.
pub(crate) fn spec(req: &Wait) -> Result<WaitSpec, String> {
    let states: HashSet<WaitState> = if req.states.is_empty() {
        WaitState::ALL.into_iter().collect()
    } else {
        req.states
            .iter()
            .map(|&s| wait_state_from_proto(s).ok_or_else(|| format!("unknown wait state {s}")))
            .collect::<Result<_, _>>()?
    };
    let targets = (!req.agent_ids.is_empty()).then(|| req.agent_ids.clone());
    Ok(WaitSpec { targets, states })
}

async fn blocking<T: Send + 'static>(
    node: &Arc<Node>,
    f: impl FnOnce(&Node) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let node = Arc::clone(node);
    tokio::task::spawn_blocking(move || f(&node))
        .await
        .map_err(io::Error::other)?
}

/// Answers a `SendMessage`.
pub(crate) async fn send_message(
    node: &Arc<Node>,
    caller: Caller,
    m: SendMessage,
) -> response::Kind {
    match blocking(node, move |n| n.send_message(caller, &m)).await {
        Ok(sent) => response::Kind::MessagesSent(Box::new(sent)),
        Err(e) => error(e.to_string()),
    }
}

/// Answers a `ReadInbox`: the caller's own mailbox.
pub(crate) async fn read_inbox(node: &Arc<Node>, caller: Caller, r: ReadInbox) -> response::Kind {
    let max = r.max as usize;
    match blocking(node, move |n| n.inbox(caller, r.since, max)).await {
        Ok((messages, cursor)) => response::Kind::Inbox(Box::new(Inbox { messages, cursor })),
        Err(e) => error(e.to_string()),
    }
}

/// Waits as `req` asks and queues the answer to request `id`. `notices`
/// is subscribed before the first check, so nothing after it is missed.
pub(crate) async fn run(
    node: Arc<Node>,
    caller: Caller,
    id: u64,
    req: Wait,
    mut notices: broadcast::Receiver<Notice>,
    outbox: mpsc::Sender<Frame>,
) {
    let kind = match spec(&req) {
        Err(e) => error(e),
        Ok(spec) => {
            let timeout = Duration::from_millis(req.timeout_ms).min(MAX_WAIT);
            match wait(&node, caller, spec, timeout, &mut notices).await {
                Ok(result) => response::Kind::WaitResult(Box::new(result)),
                Err(e) => error(e.to_string()),
            }
        }
    };
    let _ = outbox
        .send(Frame::Response(Response {
            id,
            kind: Some(kind),
        }))
        .await;
}

async fn wait(
    node: &Arc<Node>,
    caller: Caller,
    spec: WaitSpec,
    timeout: Duration,
    notices: &mut broadcast::Receiver<Notice>,
) -> io::Result<fleet_proto::WaitResult> {
    let deadline = tokio::time::Instant::now() + timeout;
    let spec = Arc::new(spec);
    let mut notice: Option<Notice> = None;
    loop {
        let s = Arc::clone(&spec);
        let matches = blocking(node, move |n| n.wait_check(caller, &s, notice.as_ref())).await?;
        if !matches.is_empty() {
            let s = Arc::clone(&spec);
            return blocking(node, move |n| n.wait_result(caller, &s, matches)).await;
        }
        notice = tokio::select! {
            got = notices.recv() => match got {
                Ok(n) => Some(n),
                // Missed some: the mailbox and named targets are re-read.
                Err(broadcast::error::RecvError::Lagged(_)) => None,
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(io::Error::other("fleetd is stopping"));
                }
            },
            () = tokio::time::sleep_until(deadline) => {
                let s = Arc::clone(&spec);
                return blocking(node, move |n| n.wait_result(caller, &s, Vec::new())).await;
            }
        };
    }
}
