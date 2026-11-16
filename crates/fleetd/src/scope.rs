//! Who a connection is, and what an agent's requests may touch (ADR 0072,
//! spec §1). The server asks this module at the handshake and for each
//! request from an agent; the user's requests never get here.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use fleet_node::node::Node;
use fleet_node::scope::{Caller, FORBIDDEN_PREFIX, Quotas};
use fleet_proto::{Event, Frame, SpawnRefused, event, request};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};
use tracing::warn;

/// The session id of the process at the other end of `stream`: its pid's
/// `getsid`. `None` if the kernel does not say, or the process is gone.
pub(crate) fn peer_session(stream: &UnixStream) -> Option<u32> {
    let pid = stream.peer_cred().ok()?.pid()?;
    if pid <= 0 {
        return None;
    }
    // SAFETY: getsid has no memory-safety preconditions.
    let sid = unsafe { libc::getsid(pid) };
    u32::try_from(sid).ok().filter(|&s| s > 0)
}

/// The caller of a connection from its peer's session and its `Hello`
/// token; `Err` refuses the handshake with the reason.
pub(crate) async fn identify(
    node: &Arc<Node>,
    session: Option<u32>,
    token: String,
) -> Result<Caller, String> {
    let node = Arc::clone(node);
    tokio::task::spawn_blocking(move || {
        let token = (!token.is_empty()).then_some(token);
        node.resolve_caller(session, token.as_deref())
    })
    .await
    .unwrap_or_else(|e| Err(format!("identifying the caller failed: {e}")))
}

/// The quotas in `<state_dir>/config.toml` (`[node]`), defaults when the
/// file is missing; a line it cannot use is logged and skipped.
pub(crate) fn read_quotas(state_dir: &Path) -> Quotas {
    let path = state_dir.join("config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Quotas::default(),
        Err(e) => {
            warn!(path = ?path, error = %e, "reading the config failed; default quotas");
            return Quotas::default();
        }
    };
    let (quotas, warnings) = Quotas::from_config(&text);
    for w in warnings {
        warn!("{w}");
    }
    quotas
}

fn forbidden(what: &str) -> String {
    format!("{FORBIDDEN_PREFIX} {what}")
}

/// What agent `a` may not do with `kind`: the refusal's message, or `None`
/// when the request may go on (spec §1's table). `in_scope` caches the
/// agents already found among `a`'s descendants: an agent never leaves a
/// subtree (its `fleet.parent` cannot change), so a hit needs no lookup.
pub(crate) async fn refusal(
    node: &Arc<Node>,
    a: u64,
    in_scope: &mut HashSet<u64>,
    kind: &Option<request::Kind>,
) -> Option<String> {
    use request::Kind;
    let targets: Vec<u64> = match kind.as_ref()? {
        // Becomes a's child; the node narrows labels and applies quotas.
        Kind::Spawn(_) => return None,
        // Filtered to a's descendants by the node.
        Kind::List(_) => return None,
        // Only drops this connection's own subscription.
        Kind::Detach(_) => return None,
        Kind::SetLabels(_) => return Some(forbidden("agents cannot change labels")),
        Kind::ReadLogs(_) | Kind::StopLogs(_) => {
            return Some(forbidden("agents cannot read fleetd's log"));
        }
        Kind::RestartDaemon(_) => return Some(forbidden("agents cannot restart fleetd")),
        Kind::Attach(r) => vec![r.agent_id],
        Kind::Input(r) => vec![r.agent_id],
        Kind::Resize(r) => vec![r.agent_id],
        Kind::Kill(r) => vec![r.agent_id],
        Kind::ReadHistory(r) => vec![r.agent_id],
        Kind::Restart(r) => vec![r.agent_id],
        // With `exited_only`, the node keeps to a's descendants.
        Kind::Forget(r) => r.ids.clone(),
    };
    let missing: Vec<u64> = targets
        .into_iter()
        .filter(|id| !in_scope.contains(id))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let node = Arc::clone(node);
    let mine = tokio::task::spawn_blocking(move || node.tree().map(|t| t.descendants(a)))
        .await
        .map_err(std::io::Error::other)
        .and_then(|r| r);
    let mine = match mine {
        Ok(mine) => mine,
        Err(e) => return Some(format!("checking the caller's scope failed: {e}")),
    };
    if let Some(id) = missing.iter().find(|id| !mine.contains(id)) {
        return Some(forbidden(&format!(
            "agent {id} is not a descendant of agent {a}"
        )));
    }
    in_scope.extend(mine);
    None
}

/// Forwards refused spawns to the connection as `SpawnRefused` events.
pub(crate) async fn forward_refused(
    mut refused: broadcast::Receiver<SpawnRefused>,
    outbox: mpsc::Sender<Frame>,
) {
    loop {
        match refused.recv().await {
            Ok(ev) => {
                let frame = Frame::Event(Event {
                    kind: Some(event::Kind::SpawnRefused(ev)),
                });
                if outbox.send(frame).await.is_err() {
                    return;
                }
            }
            // A refusal is a few seconds' notice; missing some is fine.
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}
