//! What a client reports after asking fleetd to restart into a new binary,
//! decided from the handshake of whichever daemon answers next. The CLI and
//! the GUI share this, so they agree on what counts as success and say the
//! same words.

use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use fleet_proto::{HelloOk, RestartDaemon, request, response};

use crate::{Connection, Error, Result};

/// How long a client waits for the restarted daemon to answer, from the old
/// daemon's `RestartingDaemon`.
pub const RESTART_WAIT: Duration = Duration::from_secs(10);

/// How long a client that does not block on the request (the GUI) waits
/// for the old daemon's answer to `RestartDaemon` before it gives up with
/// [`did_not_come_back`]: fleetd's check of the new binary (5 s) and its
/// freeze (3 s) bound the answer, plus a margin. [`RESTART_WAIT`] starts
/// only once the answer is in.
pub const RESTART_ANSWER_WAIT: Duration = Duration::from_secs(9);

/// What an old daemon says to a request it does not know, and what a client
/// says instead.
pub const TOO_OLD: &str = "this fleetd is too old to restart itself; stop it once (fleetd \
                           --force-stop, or scripts/install.sh --force-stop) and the next client \
                           starts the new one";

/// What a client says when no daemon answered within [`RESTART_WAIT`].
pub fn did_not_come_back() -> String {
    format!(
        "fleetd did not come back within {} s",
        RESTART_WAIT.as_secs()
    )
}

/// The failure line for a daemon that refused, or could not exec and thawed.
pub fn restart_failed(reason: &str) -> String {
    format!("fleetd restart failed: {reason}; the old fleetd is still running")
}

/// The verdict on a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestartOutcome {
    /// Every agent was kept, with proof.
    Restarted { kept: usize },
    /// Some agents could not be kept and were ended. `ids` is `None` when
    /// the new daemon cannot say which.
    Partial {
        kept: usize,
        ended: usize,
        ids: Option<Vec<u64>>,
    },
    /// The old daemon is still running; the reason it gave.
    Failed(String),
    /// Still the old, not yet thawed daemon: keep waiting.
    Pending,
}

impl RestartOutcome {
    /// `true` for [`RestartOutcome::Restarted`]: the only exit-0 verdict.
    pub fn is_success(&self) -> bool {
        matches!(self, RestartOutcome::Restarted { .. })
    }
}

/// The CLI's lines, verbatim; the GUI toasts use them too. [`Pending`]
/// prints nothing.
///
/// [`Pending`]: RestartOutcome::Pending
impl fmt::Display for RestartOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestartOutcome::Restarted { kept } => write!(f, "fleetd restarted: {kept} agents kept"),
            RestartOutcome::Partial { kept, ended, ids } => {
                let ids = match ids {
                    Some(ids) => ids.iter().map(u64::to_string).collect::<Vec<_>>().join(","),
                    None => "unknown".to_string(),
                };
                write!(
                    f,
                    "fleetd restarted: {kept} kept, {ended} could not be restored and were ended (ids {ids})"
                )
            }
            RestartOutcome::Failed(reason) => f.write_str(&restart_failed(reason)),
            RestartOutcome::Pending => Ok(()),
        }
    }
}

/// What the daemon that answered after a restart tells us.
///
/// `agents` is what the old daemon reported in `RestartingDaemon`. Success
/// needs positive proof: the instance changed, the summary names `old_instance`
/// as its predecessor, nothing failed or fell back, and exactly `agents` were
/// kept. A new instance without a summary (or one for another predecessor,
/// e.g. the exec'd binary crashed and a later client started a fresh daemon)
/// kept nothing we can vouch for, so it reads as all `agents` ended. A
/// `fell_back` daemon started fresh and ended every agent. Ids come from
/// `failed` when it lists any, else they are unknown.
pub fn restart_outcome(old_instance: u64, agents: u32, hello: &HelloOk) -> RestartOutcome {
    if hello.instance == old_instance {
        // exec failed after `RestartingDaemon`: the old daemon thawed.
        let err = hello.restore.as_deref().map_or("", |r| &r.restart_error);
        return if err.is_empty() {
            RestartOutcome::Pending
        } else {
            RestartOutcome::Failed(err.to_string())
        };
    }
    let agents = agents as usize;
    let partial = |kept: usize, ended: usize, failed: &[u64]| RestartOutcome::Partial {
        kept,
        ended,
        ids: (!failed.is_empty()).then(|| failed.to_vec()),
    };
    match hello.restore.as_deref() {
        Some(r) if r.previous_instance == old_instance => {
            if r.fell_back {
                partial(0, agents, &r.failed)
            } else if r.failed.is_empty() && r.kept.len() == agents {
                RestartOutcome::Restarted { kept: agents }
            } else {
                partial(r.kept.len(), agents.saturating_sub(r.kept.len()), &r.failed)
            }
        }
        _ => partial(0, agents, &[]),
    }
}

/// What asking a daemon to restart came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restart {
    /// The daemon predates `RestartDaemon` (it answered "empty request").
    TooOld,
    /// The daemon refused (it is still serving), or it restarted and the
    /// verdict is in. Never [`RestartOutcome::Pending`].
    Outcome(RestartOutcome),
}

/// Asks the daemon on `client` to exec into `binary` (empty: the binary the
/// daemon runs from, as now on disk), then waits up to [`RESTART_WAIT`] for
/// whichever daemon answers on `sock` next and decides with
/// [`restart_outcome`]. `client_name` is how the reconnects introduce
/// themselves in the daemon's log. `Err` when the
/// request failed in some other way or no daemon came back in time
/// ([`did_not_come_back`]).
pub fn restart_daemon(
    mut client: Connection,
    sock: &Path,
    client_name: &str,
    binary: &str,
) -> Result<Restart> {
    let old_instance = client.hello().instance;
    let req = RestartDaemon {
        binary: binary.to_string(),
    };
    let agents = match client.call(request::Kind::RestartDaemon(req)) {
        Ok(response::Kind::RestartingDaemon(r)) => r.agents,
        Ok(_) => return Err(Error::Protocol("unexpected reply to restart".into())),
        Err(Error::Server(m)) if m == "empty request" => return Ok(Restart::TooOld),
        // The daemon refused before exec; it is still serving.
        Err(Error::Server(reason)) => {
            return Ok(Restart::Outcome(RestartOutcome::Failed(reason)));
        }
        Err(e) => return Err(e),
    };
    drop(client);
    let deadline = Instant::now() + RESTART_WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        // The handshake may block while the process is between exec and
        // serving, so it gets what is left of the budget.
        if let Ok(c) =
            Connection::unix_with_timeout(sock, client_name, left.max(Duration::from_millis(100)))
        {
            match restart_outcome(old_instance, agents, c.hello()) {
                RestartOutcome::Pending => {}
                done => return Ok(Restart::Outcome(done)),
            }
        }
        if Instant::now() >= deadline {
            return Err(Error::Protocol(did_not_come_back()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::RestoreSummary;

    fn hello(instance: u64, restore: Option<RestoreSummary>) -> HelloOk {
        HelloOk {
            instance,
            restore: restore.map(Box::new),
            ..Default::default()
        }
    }

    fn restore(previous: u64, kept: &[u64], failed: &[u64], fell_back: bool) -> RestoreSummary {
        RestoreSummary {
            previous_instance: previous,
            kept: kept.to_vec(),
            failed: failed.to_vec(),
            fell_back,
            ..Default::default()
        }
    }

    const PARTIAL: &str = "could not be restored and were ended";

    fn partial(kept: usize, ended: usize, ids: Option<&[u64]>) -> RestartOutcome {
        RestartOutcome::Partial {
            kept,
            ended,
            ids: ids.map(<[u64]>::to_vec),
        }
    }

    #[test]
    fn outcome_table() {
        // (old instance, agents, hello, expected outcome, expected line)
        type Case = (u64, u32, HelloOk, RestartOutcome, String);
        let cases: Vec<Case> = vec![
            // Success only with full proof.
            (
                5,
                2,
                hello(6, Some(restore(5, &[1, 2], &[], false))),
                RestartOutcome::Restarted { kept: 2 },
                "fleetd restarted: 2 agents kept".into(),
            ),
            (
                5,
                0,
                hello(6, Some(restore(5, &[], &[], false))),
                RestartOutcome::Restarted { kept: 0 },
                "fleetd restarted: 0 agents kept".into(),
            ),
            // No summary, or another predecessor: nothing vouched for.
            (
                5,
                2,
                hello(6, None),
                partial(0, 2, None),
                format!("fleetd restarted: 0 kept, 2 {PARTIAL} (ids unknown)"),
            ),
            (
                5,
                2,
                hello(7, Some(restore(6, &[1, 2], &[], false))),
                partial(0, 2, None),
                format!("fleetd restarted: 0 kept, 2 {PARTIAL} (ids unknown)"),
            ),
            // Failed ids listed.
            (
                5,
                3,
                hello(6, Some(restore(5, &[1], &[3, 7], false))),
                partial(1, 2, Some(&[3, 7])),
                format!("fleetd restarted: 1 kept, 2 {PARTIAL} (ids 3,7)"),
            ),
            // Fewer kept + failed than agents: the rest are unaccounted for.
            (
                5,
                3,
                hello(6, Some(restore(5, &[1], &[], false))),
                partial(1, 2, None),
                format!("fleetd restarted: 1 kept, 2 {PARTIAL} (ids unknown)"),
            ),
            // Fell back: everything ended.
            (
                5,
                3,
                hello(6, Some(restore(5, &[], &[], true))),
                partial(0, 3, None),
                format!("fleetd restarted: 0 kept, 3 {PARTIAL} (ids unknown)"),
            ),
            (
                5,
                3,
                hello(6, Some(restore(5, &[], &[1, 2, 3], true))),
                partial(0, 3, Some(&[1, 2, 3])),
                format!("fleetd restarted: 0 kept, 3 {PARTIAL} (ids 1,2,3)"),
            ),
            // Same instance: exec failed and thawed, or not thawed yet.
            (
                5,
                2,
                hello(
                    5,
                    Some(RestoreSummary {
                        restart_error: "exec failed".into(),
                        ..Default::default()
                    }),
                ),
                RestartOutcome::Failed("exec failed".into()),
                "fleetd restart failed: exec failed; the old fleetd is still running".into(),
            ),
            (5, 2, hello(5, None), RestartOutcome::Pending, String::new()),
            (
                5,
                2,
                hello(5, Some(restore(4, &[], &[], false))),
                RestartOutcome::Pending,
                String::new(),
            ),
        ];
        for (old, agents, h, want, line) in cases {
            let got = restart_outcome(old, agents, &h);
            assert_eq!(got, want, "{old} {agents} {h:?}");
            assert_eq!(got.to_string(), line, "{old} {agents} {h:?}");
            assert_eq!(
                got.is_success(),
                matches!(want, RestartOutcome::Restarted { .. })
            );
        }
    }
}
