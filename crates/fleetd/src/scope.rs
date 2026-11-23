//! Who a connection is, and what an agent's requests may touch (ADR 0072,
//! spec §1). The server asks this module at the handshake and for each
//! request from an agent; the user's requests never get here.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

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

/// What a connection may see and touch: everything for the user; for agent
/// `a`, its descendants (`D(a)`, spec §1). Shared by the request loop and
/// the event forwarders.
///
/// Answers are cached per agent id for the connection's life, both ways:
/// an agent's `fleet.parent` never changes (agents cannot relabel, and a
/// restart keeps the labels), so whether an id is in `D(a)` never changes
/// either. A miss (an agent spawned since) re-reads the tree.
#[derive(Clone)]
pub(crate) struct Scope(Option<Arc<View>>);

pub(crate) struct View {
    node: Arc<Node>,
    agent: u64,
    known: Mutex<HashMap<u64, bool>>,
}

impl Scope {
    /// The user's: everything.
    #[cfg(test)]
    pub(crate) fn user() -> Scope {
        Scope(None)
    }

    pub(crate) fn new(node: &Arc<Node>, caller: Caller) -> Scope {
        Scope(match caller {
            Caller::User => None,
            Caller::Agent(agent) => Some(Arc::new(View {
                node: Arc::clone(node),
                agent,
                known: Mutex::new(HashMap::new()),
            })),
        })
    }

    fn cached(view: &View, id: u64) -> Option<bool> {
        view.known
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .copied()
    }

    /// Records a fresh read of the tree: `D(a)` is in, `asked` (if not in
    /// it) is out. A failed read leaves the cache alone and answers no.
    fn learn(view: &View, mine: std::io::Result<HashSet<u64>>, asked: Option<u64>) -> bool {
        let mine = match mine {
            Ok(mine) => mine,
            Err(e) => {
                warn!(agent = view.agent, error = %e, "reading the agent tree failed");
                return false;
            }
        };
        let mut known = view.known.lock().unwrap_or_else(|e| e.into_inner());
        for &id in &mine {
            known.insert(id, true);
        }
        match asked {
            Some(id) => *known.entry(id).or_insert(false),
            None => true,
        }
    }

    /// Reads `D(a)` into the cache (at the handshake, so removals of
    /// agents already in the subtree are recognised).
    pub(crate) async fn warm(&self) {
        if let Some(view) = &self.0 {
            let v = Arc::clone(view);
            let mine =
                tokio::task::spawn_blocking(move || v.node.tree().map(|t| t.descendants(v.agent)))
                    .await
                    .unwrap_or_else(|e| Err(std::io::Error::other(e)));
            Self::learn(view, mine, None);
        }
    }

    /// Whether the connection may see agent `id`.
    pub(crate) async fn sees(&self, id: u64) -> bool {
        let Some(view) = &self.0 else {
            return true;
        };
        if let Some(known) = Self::cached(view, id) {
            return known;
        }
        let v = Arc::clone(view);
        let mine =
            tokio::task::spawn_blocking(move || v.node.tree().map(|t| t.descendants(v.agent)))
                .await
                .unwrap_or_else(|e| Err(std::io::Error::other(e)));
        Self::learn(view, mine, Some(id))
    }

    /// [`Scope::sees`] without awaiting, for a caller holding a lock: a
    /// miss reads the store on this thread (once per new agent id).
    pub(crate) fn sees_now(&self, id: u64) -> bool {
        let Some(view) = &self.0 else {
            return true;
        };
        if let Some(known) = Self::cached(view, id) {
            return known;
        }
        let mine = view.node.tree().map(|t| t.descendants(view.agent));
        Self::learn(view, mine, Some(id))
    }

    /// The cached answer only (yes for the user): for a forgotten agent,
    /// whose row can no longer be read.
    pub(crate) fn sees_now_cached(&self, id: u64) -> bool {
        match &self.0 {
            None => true,
            Some(view) => Self::cached(view, id).unwrap_or(false),
        }
    }

    /// Whether the connection hears of spawns waiting for approval: the
    /// user's only, since only the user approves (ADR 0079).
    pub(crate) fn sees_spawn_approvals(&self) -> bool {
        self.0.is_none()
    }

    /// Whether a refused spawn of `parent`'s is the connection's business:
    /// its own, or a descendant's.
    async fn sees_refusal(&self, parent: u64) -> bool {
        match &self.0 {
            Some(view) if view.agent == parent => true,
            _ => self.sees(parent).await,
        }
    }
}

/// What agent `a` (the scope's) may not do with `kind`: the refusal's
/// message, or `None` when the request may go on (spec §1's table).
pub(crate) async fn refusal(scope: &Scope, a: u64, kind: &Option<request::Kind>) -> Option<String> {
    use request::Kind;
    let targets: Vec<u64> = match kind.as_ref()? {
        // Becomes a's child; the node narrows labels and applies quotas.
        Kind::Spawn(_) => return None,
        // Filtered to a's descendants by the node.
        Kind::List(_) => return None,
        // Only drops this connection's own subscription.
        Kind::Detach(_) => return None,
        // Self only by construction: it names no agent, the node takes the
        // caller's.
        Kind::Report(_) => return None,
        // Leases are open to any caller (ADR 0034, 0077): any name, no
        // descendants check. The node answers a release by a non-holder.
        Kind::AcquireLease(_) | Kind::ReleaseLease(_) | Kind::ListLeases(_) => return None,
        Kind::SetLabels(_) => return Some(forbidden("agents cannot change labels")),
        Kind::ReadLogs(_) | Kind::StopLogs(_) => {
            return Some(forbidden("agents cannot read fleetd's log"));
        }
        Kind::RestartDaemon(_) => return Some(forbidden("agents cannot restart fleetd")),
        // Only the user's words close a question (ADR 0078).
        Kind::Answer(_) => return Some(forbidden("only the user answers questions")),
        // Only the user approves spawns (ADR 0079).
        Kind::ListPendingSpawns(_) | Kind::DecideSpawn(_) => {
            return Some(forbidden("only the user approves spawns"));
        }
        // Reads every mailbox's senders and recipients: the GUI's (ADR 0089).
        Kind::ListMessageEdges(_) => return Some(forbidden("agents cannot read every mailbox")),
        // Reads every agent's output: the user's search (ADR 0098).
        Kind::SearchOutput(_) => {
            return Some(forbidden("agents cannot search every agent's output"));
        }
        // The node primitives are for the user and for workflows (ADR 0091,
        // 0025): an agent already has a shell.
        Kind::Exec(_) | Kind::ReadFile(_) | Kind::ListDir(_) => {
            return Some(forbidden("agents cannot use the node's exec, read or list"));
        }
        Kind::Attach(r) => vec![r.agent_id],
        Kind::Input(r) => vec![r.agent_id],
        Kind::Resize(r) => vec![r.agent_id],
        Kind::Kill(r) => vec![r.agent_id],
        Kind::ReadHistory(r) => vec![r.agent_id],
        Kind::Restart(r) => vec![r.agent_id],
        // With `exited_only`, the node keeps to a's descendants.
        Kind::Forget(r) => r.ids.clone(),
        // The node checks the recipients (D(a), a's parent, the user) and
        // keeps a broadcast to D(a) (ADR 0076).
        Kind::SendMessage(_) => return None,
        // The caller's own mailbox.
        Kind::ReadInbox(_) => return None,
        // Workflows (ADR 0092): the host checks the action's callers, its
        // params' scope (`agent` params in D(a)) and runs it as a.
        Kind::ListActions(_) | Kind::RunAction(_) => return None,
        // Named agents must be in D(a); none means D(a).
        Kind::Wait(r) => r.agent_ids.clone(),
    };
    for id in targets {
        if !scope.sees(id).await {
            return Some(forbidden(&format!(
                "agent {id} is not a descendant of agent {a}"
            )));
        }
    }
    None
}

/// Forwards status reports of agents in its scope to the connection as
/// `AgentReported` events (a report is screen-adjacent text: it must not
/// bypass the descendants-only rule, ADR 0072).
pub(crate) async fn forward_reported(
    mut reported: broadcast::Receiver<fleet_proto::AgentReported>,
    outbox: mpsc::Sender<Frame>,
    scope: Scope,
) {
    loop {
        match reported.recv().await {
            Ok(ev) => {
                if !scope.sees(ev.agent_id).await {
                    continue;
                }
                let frame = Frame::Event(Event {
                    kind: Some(event::Kind::Reported(Box::new(ev))),
                });
                if outbox.send(frame).await.is_err() {
                    return;
                }
            }
            // The next list (or report) carries the latest; a miss only
            // delays a dot.
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Forwards refused spawns (in its scope) to the connection as `SpawnRefused` events.
pub(crate) async fn forward_refused(
    mut refused: broadcast::Receiver<SpawnRefused>,
    outbox: mpsc::Sender<Frame>,
    scope: Scope,
) {
    loop {
        match refused.recv().await {
            Ok(ev) => {
                if !scope.sees_refusal(ev.parent).await {
                    continue;
                }
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
