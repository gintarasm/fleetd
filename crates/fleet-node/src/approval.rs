//! Spawn approval (ADR 0079): with `[node] spawn_approval = true`, an
//! agent's spawn waits here until the user approves or denies it, it
//! expires, or the asking connection goes away.
//!
//! Pending spawns live in memory only: nothing survives a daemon restart,
//! which refuses them ([`Approvals::refuse_all`]). The registry knows
//! nothing about agents or connections; the server asks, waits on the
//! [`Ticket`] with the timeout, and spawns on approval.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use fleet_proto::{PendingSpawn, SpawnDecided, SpawnDecision};
use tokio::sync::{broadcast, oneshot};

use crate::scope;

/// How many approval events a slow receiver may fall behind by.
const EVENT_CAPACITY: usize = 64;

/// A change to the pending spawns, as [`Approvals::events`] announces it.
#[derive(Clone, Debug, PartialEq)]
pub enum ApprovalEvent {
    /// A spawn started waiting.
    Pending(PendingSpawn),
    /// A pending spawn stopped waiting.
    Decided(SpawnDecided),
}

/// The pending spawns. Cheap to clone; every clone is the same registry.
#[derive(Clone)]
pub struct Approvals {
    shared: Arc<Shared>,
}

struct Shared {
    inner: Mutex<Inner>,
    events: broadcast::Sender<ApprovalEvent>,
}

#[derive(Default)]
struct Inner {
    /// The last id handed out.
    last: u64,
    pending: BTreeMap<u64, (PendingSpawn, oneshot::Sender<SpawnDecision>)>,
}

impl Default for Approvals {
    fn default() -> Self {
        Approvals::new()
    }
}

impl Approvals {
    pub fn new() -> Approvals {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Approvals {
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner::default()),
                events,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.shared.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Receives every change from now on.
    pub fn events(&self) -> broadcast::Receiver<ApprovalEvent> {
        self.shared.events.subscribe()
    }

    /// Records `spawn` (its `id` is assigned here) as pending and
    /// announces it. The ticket answers with the decision; dropping it
    /// undecided withdraws the request.
    pub fn ask(&self, mut spawn: PendingSpawn) -> Ticket {
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut inner = self.lock();
            inner.last += 1;
            spawn.id = inner.last;
            inner.pending.insert(spawn.id, (spawn.clone(), tx));
            spawn.id
        };
        let _ = self.shared.events.send(ApprovalEvent::Pending(spawn));
        Ticket {
            id,
            rx,
            approvals: self.clone(),
        }
    }

    /// The user's answer to pending spawn `id`; `NotFound` if it is not
    /// pending (decided, expired, withdrawn, or never asked).
    pub fn decide(&self, id: u64, approve: bool) -> io::Result<()> {
        let decision = if approve {
            SpawnDecision::SpawnApproved
        } else {
            SpawnDecision::SpawnDenied
        };
        if self.resolve(id, decision) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no pending spawn {id}"),
            ))
        }
    }

    /// The pending spawns, oldest first.
    pub fn list(&self) -> Vec<PendingSpawn> {
        self.lock()
            .pending
            .values()
            .map(|(spawn, _)| spawn.clone())
            .collect()
    }

    /// Ends every pending spawn with `decision` (a restart's refusal).
    pub fn refuse_all(&self, decision: SpawnDecision) {
        let ids: Vec<u64> = self.lock().pending.keys().copied().collect();
        for id in ids {
            self.resolve(id, decision);
        }
    }

    /// Ends pending spawn `id` with `decision`: tells its ticket and
    /// announces it. `false` if it was not pending.
    fn resolve(&self, id: u64, decision: SpawnDecision) -> bool {
        let Some((_, tx)) = self.lock().pending.remove(&id) else {
            return false;
        };
        // The ticket may be gone (withdrawn); the announcement still goes.
        let _ = tx.send(decision);
        let _ = self
            .shared
            .events
            .send(ApprovalEvent::Decided(SpawnDecided {
                id,
                decision: decision.into(),
            }));
        true
    }
}

/// One pending spawn's answer. Dropping it while it is pending withdraws
/// the request.
pub struct Ticket {
    id: u64,
    rx: oneshot::Receiver<SpawnDecision>,
    approvals: Approvals,
}

impl Ticket {
    /// The pending spawn's id.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Waits for the decision. Cancel-safe: the caller may time it out
    /// and then [`Ticket::expire`] it.
    pub async fn decision(&mut self) -> SpawnDecision {
        (&mut self.rx)
            .await
            .unwrap_or(SpawnDecision::SpawnWithdrawn)
    }

    /// The wait timed out: the spawn expires, unless a decision arrived
    /// just before, which is then the answer.
    pub fn expire(mut self) -> SpawnDecision {
        if self.approvals.resolve(self.id, SpawnDecision::SpawnExpired) {
            return SpawnDecision::SpawnExpired;
        }
        self.rx.try_recv().unwrap_or(SpawnDecision::SpawnWithdrawn)
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        // A no-op once decided.
        self.approvals
            .resolve(self.id, SpawnDecision::SpawnWithdrawn);
    }
}

/// The `forbidden:` message a spawn gets for `decision`; `None` when it
/// was approved. `timeout_secs` names the wait in an expiry.
pub fn refusal(decision: SpawnDecision, timeout_secs: u32) -> Option<String> {
    let why = match decision {
        SpawnDecision::SpawnApproved => return None,
        SpawnDecision::SpawnDenied => String::new(),
        SpawnDecision::SpawnExpired => format!(" (no answer within {})", duration(timeout_secs)),
        SpawnDecision::SpawnRestarting => " (fleetd is restarting)".to_owned(),
        SpawnDecision::SpawnWithdrawn | SpawnDecision::Unspecified => " (withdrawn)".to_owned(),
    };
    Some(format!(
        "{} spawn not approved{why}",
        scope::FORBIDDEN_PREFIX
    ))
}

/// `300` as `5m`, `90` as `90s`.
fn duration(secs: u32) -> String {
    if secs >= 60 && secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn(parent: u64) -> PendingSpawn {
        PendingSpawn {
            parent,
            program: "claude".into(),
            ..Default::default()
        }
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn ask_lists_and_announces_then_approval_answers_the_ticket() {
        let a = Approvals::new();
        let mut events = a.events();
        let mut t = a.ask(spawn(7));
        assert_eq!(t.id(), 1);
        let listed = a.list();
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].id, listed[0].parent), (1, 7));
        assert_eq!(
            events.try_recv().unwrap(),
            ApprovalEvent::Pending(listed[0].clone())
        );
        a.decide(1, true).unwrap();
        assert_eq!(block_on(t.decision()), SpawnDecision::SpawnApproved);
        assert!(a.list().is_empty());
        assert_eq!(
            events.try_recv().unwrap(),
            ApprovalEvent::Decided(SpawnDecided {
                id: 1,
                decision: SpawnDecision::SpawnApproved.into()
            })
        );
        drop(t);
        assert!(
            events.try_recv().is_err(),
            "a decided ticket's drop is silent"
        );
    }

    #[test]
    fn deny_and_unknown_ids() {
        let a = Approvals::new();
        let mut t = a.ask(spawn(1));
        a.decide(t.id(), false).unwrap();
        assert_eq!(block_on(t.decision()), SpawnDecision::SpawnDenied);
        let e = a.decide(t.id(), true).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert_eq!(e.to_string(), "no pending spawn 1");
        assert!(a.decide(99, true).is_err());
    }

    #[test]
    fn dropping_a_ticket_withdraws_it() {
        let a = Approvals::new();
        let t = a.ask(spawn(1));
        let mut events = a.events();
        drop(t);
        assert!(a.list().is_empty());
        assert_eq!(
            events.try_recv().unwrap(),
            ApprovalEvent::Decided(SpawnDecided {
                id: 1,
                decision: SpawnDecision::SpawnWithdrawn.into()
            })
        );
    }

    #[test]
    fn expiry_unless_decided_first() {
        let a = Approvals::new();
        let t = a.ask(spawn(1));
        assert_eq!(t.expire(), SpawnDecision::SpawnExpired);
        assert!(a.list().is_empty());
        let t = a.ask(spawn(1));
        assert_eq!(t.id(), 2, "ids are not reused");
        a.decide(2, true).unwrap();
        assert_eq!(t.expire(), SpawnDecision::SpawnApproved);
    }

    #[test]
    fn refuse_all_ends_every_pending_spawn() {
        let a = Approvals::new();
        let mut t1 = a.ask(spawn(1));
        let mut t2 = a.ask(spawn(2));
        a.refuse_all(SpawnDecision::SpawnRestarting);
        assert!(a.list().is_empty());
        assert_eq!(block_on(t1.decision()), SpawnDecision::SpawnRestarting);
        assert_eq!(block_on(t2.decision()), SpawnDecision::SpawnRestarting);
    }

    #[test]
    fn refusal_messages() {
        assert_eq!(refusal(SpawnDecision::SpawnApproved, 300), None);
        assert_eq!(
            refusal(SpawnDecision::SpawnDenied, 300).unwrap(),
            "forbidden: spawn not approved"
        );
        assert_eq!(
            refusal(SpawnDecision::SpawnExpired, 300).unwrap(),
            "forbidden: spawn not approved (no answer within 5m)"
        );
        assert_eq!(
            refusal(SpawnDecision::SpawnExpired, 1).unwrap(),
            "forbidden: spawn not approved (no answer within 1s)"
        );
        assert_eq!(
            refusal(SpawnDecision::SpawnRestarting, 300).unwrap(),
            "forbidden: spawn not approved (fleetd is restarting)"
        );
    }
}
