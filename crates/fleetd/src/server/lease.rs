//! Lease requests and the driver behind them (ADR 0077, spec §4).
//!
//! A waiting `AcquireLease` is answered from a task of its own, so the
//! connection keeps reading frames and notices when its client goes away;
//! closing the connection drops its waiters and releases what it holds
//! (see [`Connection`]'s `Drop`). The driver sleeps until the next TTL,
//! reclaim deadline or load poll, and never wakes while no lease needs time.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use fleet_node::lease::{Acquired, Info, Released, Releaser, Want, now_ms};
use fleet_node::node::Node;
use fleet_node::scope::{Caller, FORBIDDEN_PREFIX};
use fleet_proto::{
    Ack, AcquireLease, Frame, LeaseGranted, LeaseInfo, LeaseList, Response, response,
};
use tokio::sync::broadcast;
use tracing::warn;

use super::{Connection, blocking, error};
use crate::scope::Scope;

/// Longest lease name, in bytes.
const NAME_MAX: usize = 128;
/// A TTL is capped here (a year), so the arithmetic cannot overflow.
const TTL_MAX_SECS: u64 = 366 * 24 * 3600;

/// What is wrong with `a`, if anything (the message starts `usage:`).
fn invalid(a: &AcquireLease, caller: Caller) -> Option<String> {
    if a.name.is_empty() || a.name.len() > NAME_MAX || a.name.chars().any(char::is_control) {
        return Some(format!(
            "usage: a lease name is 1-{NAME_MAX} bytes without control characters"
        ));
    }
    if a.max_load.is_some_and(|m| m.is_nan() || m <= 0.0) {
        return Some("usage: --max-load must be a positive number".into());
    }
    // Nothing watches the user, so a lease they take for later (no command
    // to hold it) must end by itself.
    if matches!(caller, Caller::User) && !a.hold && a.ttl_secs == 0 {
        return Some("usage: a lease taken without a command needs a --ttl".into());
    }
    None
}

/// `info` as the connection may see it: an agent sees which agent holds a
/// lease only when that is itself or one of its descendants.
async fn view(scope: &Scope, caller: Caller, info: Info) -> LeaseInfo {
    let (holder, hidden) = match (info.holder, caller) {
        (None, _) => (None, false),
        (Some(h), Caller::Agent(a)) if h == a => (Some(h), false),
        (Some(h), Caller::Agent(_)) if !scope.sees(h).await => (None, true),
        (Some(h), _) => (Some(h), false),
    };
    LeaseInfo {
        name: info.name,
        held: info.held,
        holder,
        holder_hidden: hidden,
        acquired_unix_ms: info.acquired_ms,
        expires_unix_ms: info.expires_ms,
        held_by_connection: info.bound,
        waiters: info.waiters,
    }
}

fn granted(lease: LeaseInfo) -> response::Kind {
    response::Kind::LeaseGranted(LeaseGranted { lease: Some(lease) })
}

fn busy(name: &str, held: Option<&LeaseInfo>) -> String {
    match held {
        Some(l) if l.holder_hidden => format!("busy: lease {name:?} is held by another agent"),
        Some(LeaseInfo {
            holder: Some(h), ..
        }) => format!("busy: lease {name:?} is held by agent {h}"),
        Some(_) => format!("busy: lease {name:?} is held by the user"),
        None => format!("busy: others are waiting for lease {name:?}"),
    }
}

fn holder_of(caller: Caller) -> Option<u64> {
    match caller {
        Caller::Agent(a) => Some(a),
        Caller::User => None,
    }
}

impl Connection {
    pub(super) async fn acquire_lease(&mut self, id: u64, a: AcquireLease) -> bool {
        if let Some(message) = invalid(&a, self.caller) {
            return self.respond(id, error(message)).await;
        }
        let max_load = a.max_load;
        let want = Want {
            name: a.name.clone(),
            holder: holder_of(self.caller),
            key: a.holder_key,
            conn: self.id,
            bind: a.hold,
            ttl_ms: (a.ttl_secs > 0)
                .then(|| i64::try_from(a.ttl_secs.min(TTL_MAX_SECS) * 1000).unwrap_or(i64::MAX)),
            max_load,
        };
        let wait = a.wait;
        let outcome = blocking(&self.node, move |n| {
            n.leases().acquire(want, wait, now_ms())
        })
        .await;
        let kind = match outcome {
            Err(e) => error(e.to_string()),
            Ok(Acquired::Granted(info)) => granted(view(&self.scope, self.caller, info).await),
            Ok(Acquired::Busy(info)) => {
                let shown = match info {
                    Some(i) => Some(view(&self.scope, self.caller, i).await),
                    None => None,
                };
                error(busy(&a.name, shown.as_ref()))
            }
            Ok(Acquired::LoadTooHigh(load)) => error(format!(
                "busy: load average {load:.2} is not below {}",
                max_load.unwrap_or_default()
            )),
            Ok(Acquired::Queued(rx)) => {
                let outbox = self.outbox.clone();
                let scope = self.scope.clone();
                let caller = self.caller;
                tokio::spawn(async move {
                    // An error: the connection closed and left the queue.
                    let Ok(info) = rx.await else { return };
                    let frame = Frame::Response(Response {
                        id,
                        kind: Some(granted(view(&scope, caller, info).await)),
                    });
                    let _ = outbox.send(frame).await;
                });
                return true;
            }
        };
        self.respond(id, kind).await
    }

    pub(super) async fn release_lease(&mut self, id: u64, name: String) -> bool {
        let by = match self.caller {
            Caller::User => Releaser::User,
            Caller::Agent(a) => Releaser::Agent(a),
        };
        let n = name.clone();
        let kind = match blocking(&self.node, move |node| {
            node.leases().release(&n, by, now_ms())
        })
        .await
        {
            Err(e) => error(e.to_string()),
            Ok(Released::Released) => response::Kind::Ack(Ack {}),
            Ok(Released::NotHeld) => error(format!("not held: lease {name:?} is not held")),
            Ok(Released::NotYours(_)) => error(format!(
                "{FORBIDDEN_PREFIX} lease {name:?} is held by someone else"
            )),
        };
        self.respond(id, kind).await
    }

    pub(super) async fn list_leases(&mut self, id: u64) -> bool {
        let infos = self.node.leases().list();
        let mut leases = Vec::with_capacity(infos.len());
        for info in infos {
            leases.push(view(&self.scope, self.caller, info).await);
        }
        self.respond(id, response::Kind::Leases(LeaseList { leases }))
            .await
    }
}

/// Releases the leases of an agent that ended (and drops its waits), runs
/// what has a deadline, and grants waiters their turn. One per daemon.
pub(crate) async fn drive(node: Arc<Node>) {
    // Subscribed before the sweep, so an exit after it is not missed.
    let mut exits = node.exit_events();
    sweep(&node).await;
    loop {
        if let Err(e) = blocking(&node, |n| n.leases().tick(now_ms())).await {
            warn!(error = %e, "running the leases' deadlines failed");
        }
        let sleep = node.leases().next_wake(now_ms());
        tokio::select! {
            () = node.leases().woken().notified() => {}
            () = async {
                match sleep {
                    Some(ms) => {
                        let ms = u64::try_from(ms).unwrap_or(0);
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    }
                    None => std::future::pending().await,
                }
            } => {}
            exit = exits.recv() => match exit {
                Ok(e) => {
                    let id = e.agent_id;
                    if let Err(e) =
                        blocking(&node, move |n| n.leases().agent_gone(id, now_ms())).await
                    {
                        warn!(error = %e, "releasing an ended agent's leases failed");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => sweep(&node).await,
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

/// Releases every lease whose agent is not running.
async fn sweep(node: &Arc<Node>) {
    let swept = blocking(node, |n| {
        let running: HashSet<u64> = n
            .list()?
            .into_iter()
            .filter(|a| a.state() == fleet_proto::AgentState::Running)
            .map(|a| a.id)
            .collect();
        n.leases().agents_gone(|a| !running.contains(&a), now_ms())
    })
    .await;
    if let Err(e) = swept {
        warn!(error = %e, "sweeping the leases of ended agents failed");
    }
}
