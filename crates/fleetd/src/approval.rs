//! Spawn approval in the server (ADR 0079): an agent's `Spawn` waits for
//! the user when `[node] spawn_approval = true`.
//!
//! The wait runs on a task of its own, so the asking connection keeps
//! serving other requests and is never "in flight" (a daemon restart is
//! not held off by it; the restart refuses it instead). On approval the
//! request goes back through the connection's loop and spawns as any
//! other, the quotas checked again; otherwise it is answered with
//! `forbidden: spawn not approved …`. The task is aborted with the
//! connection, which withdraws the request.

use std::sync::Arc;
use std::time::Duration;

use fleet_node::approval::{self, ApprovalEvent};
use fleet_node::lease::now_ms;
use fleet_node::node::Node;
use fleet_node::scope::{Caller, forbidden};
use fleet_proto::{
    Ack, DecideSpawn, Event, Frame, PendingSpawns, Request, Response, event, request, response,
};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::info;

use crate::scope::Scope;

/// How many of one agent's spawns may wait for approval at once (over all
/// its connections): a looping agent must not bury the user in requests.
pub(crate) const MAX_PENDING_PER_AGENT: usize = 8;

fn error(message: String) -> response::Kind {
    response::Kind::Error(fleet_proto::Error { message })
}

/// Asks the user to approve `req` (an agent's `Spawn`) and starts the task
/// that waits for the answer: it hands an approved request to `approved`
/// and answers a refused one on `outbox`. `Err` answers the request at once
/// (a quota, a label error, too many pending).
pub(crate) async fn ask(
    node: &Arc<Node>,
    caller: Caller,
    req: Request,
    timeout_secs: u32,
    outbox: mpsc::Sender<Frame>,
    approved: mpsc::UnboundedSender<Request>,
) -> Result<JoinHandle<()>, String> {
    let Some(request::Kind::Spawn(spawn)) = &req.kind else {
        return Err("not a spawn".into());
    };
    let Caller::Agent(a) = caller else {
        return Err("only an agent's spawn waits for approval".into());
    };
    let spawn = (**spawn).clone();
    let n = Arc::clone(node);
    let mut pending = tokio::task::spawn_blocking(move || n.precheck_spawn(caller, &spawn))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let approvals = node.approvals();
    if approvals.list().iter().filter(|p| p.parent == a).count() >= MAX_PENDING_PER_AGENT {
        return Err(forbidden(format!(
            "{MAX_PENDING_PER_AGENT} spawns of agent {a} already wait for approval"
        ))
        .to_string());
    }
    let now = now_ms();
    pending.asked_unix_ms = now;
    pending.expires_unix_ms = now.saturating_add(i64::from(timeout_secs) * 1000);
    let program = pending.program.clone();
    let mut ticket = approvals.ask(pending);
    info!(agent = a, pending = ticket.id(), program = ?program, "spawn waits for approval");
    Ok(tokio::spawn(async move {
        let wait = Duration::from_secs(u64::from(timeout_secs));
        let decision = match tokio::time::timeout(wait, ticket.decision()).await {
            Ok(decision) => decision,
            Err(_) => ticket.expire(),
        };
        info!(agent = a, decision = ?decision, "spawn approval decided");
        match approval::refusal(decision, timeout_secs) {
            None => {
                let _ = approved.send(req);
            }
            Some(message) => {
                let frame = Frame::Response(Response {
                    id: req.id,
                    kind: Some(error(message)),
                });
                let _ = outbox.send(frame).await;
            }
        }
    }))
}

/// The pending spawns, for `ListPendingSpawns` (the user's only: the scope
/// table refuses agents).
pub(crate) fn list(node: &Node) -> response::Kind {
    response::Kind::PendingSpawns(PendingSpawns {
        pending: node.approvals().list(),
    })
}

/// The user's answer to a pending spawn, for `DecideSpawn`.
pub(crate) fn decide(node: &Node, d: &DecideSpawn) -> response::Kind {
    match node.approvals().decide(d.id, d.approve) {
        Ok(()) => {
            info!(
                pending = d.id,
                approve = d.approve,
                "spawn approval answered"
            );
            response::Kind::Ack(Ack {})
        }
        Err(e) => error(e.to_string()),
    }
}

/// Forwards pending spawns and their decisions to the connection: the
/// user's only (only the user approves, ADR 0079).
pub(crate) async fn forward(
    mut events: broadcast::Receiver<ApprovalEvent>,
    outbox: mpsc::Sender<Frame>,
    scope: Scope,
) {
    if !scope.sees_spawn_approvals() {
        return;
    }
    loop {
        let kind = match events.recv().await {
            Ok(ApprovalEvent::Pending(p)) => event::Kind::SpawnPending(p),
            Ok(ApprovalEvent::Decided(d)) => event::Kind::SpawnDecided(d),
            // A client that missed some re-lists (the GUI does on connect).
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let frame = Frame::Event(Event { kind: Some(kind) });
        if outbox.send(frame).await.is_err() {
            return;
        }
    }
}
