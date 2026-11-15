//! The restart coordinator's work, apart from the serve loop that drives
//! it (see `Daemon::serve`): checking the new binary, stopping requests,
//! freezing, writing the record and the `exec`, and opening the node from a
//! handoff.
//!
//! Everything here runs on blocking threads as plain synchronous code, the
//! serve loop only awaiting a `JoinHandle`, and sits in a module of its own.
//! Both keep the code small and out of the pages a normal start executes:
//! the same steps as futures cost several times the machine code (state
//! machines, their drop glue, timeouts), and code that shares pages with
//! startup code is paged in on every start, where the idle daemon's
//! resident memory is gated (ADR 0014).

use std::io;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fleet_node::env::{LOGIN_ENV_TIMEOUT, LoginEnv};
use fleet_node::node::Node;
use fleet_proto::handoff::{HandoffRecord, handoff_agent};
use fleet_proto::{HANDOFF_VERSION, RestoreSummary};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::handoff;
use crate::server::{RESTART_FLUSH, RestartReply, RestartRequest, Restarts};

/// Bound on the new binary's `--handoff-check`.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(2);
/// Bound on stopping requests and freezing the node for a restart.
pub const FREEZE_DEADLINE: Duration = Duration::from_secs(3);

/// A restart and the outcome of its binary's check.
pub(crate) type Checked = (RestartRequest, PathBuf, Result<(), String>);

/// Opens the node from a predecessor's handoff record, then starts the
/// login-env capture: the handed-over terminals are claimed before the
/// capture opens a terminal of its own.
pub(crate) fn open_from_handoff(
    state_dir: PathBuf,
    rec: HandoffRecord,
    shell: Option<PathBuf>,
) -> JoinHandle<io::Result<(Node, RestoreSummary)>> {
    tokio::task::spawn_blocking(move || {
        let (node, kept, failed) = Node::open_with_handoff(&state_dir, rec.agents)?;
        let login_env = LoginEnv::capture_in_background(shell, LOGIN_ENV_TIMEOUT);
        info!(
            kept = kept.len(),
            failed = failed.len(),
            frozen_ms = unix_ms().saturating_sub(rec.frozen_unix_ms),
            "restarted from a handoff; serving"
        );
        let restore = RestoreSummary {
            previous_instance: rec.previous_instance,
            kept,
            failed,
            ..Default::default()
        };
        Ok((node.with_base_env(login_env), restore))
    })
}

/// Resolves the restart's binary and checks it with `--handoff-check` on
/// a blocking thread (the serve loop accepts meanwhile); `None` if it was
/// refused at once.
pub(crate) fn check(restarts: &Restarts, req: RestartRequest) -> Option<JoinHandle<Checked>> {
    let binary = match handoff::resolve_binary(&req.binary) {
        Ok(binary) => binary,
        Err(reason) => {
            refuse(restarts, req.reply, &reason);
            return None;
        }
    };
    info!(binary = ?binary, "restart: checking the new binary");
    Some(tokio::task::spawn_blocking(move || {
        let checked = handoff::run_check(&binary, CHECK_TIMEOUT);
        if checked.is_ok() {
            handoff::test_delay_after_check();
        }
        (req, binary, checked)
    }))
}

/// Answers a restart with an `Error`: it failed, and the daemon serves on.
fn refuse(restarts: &Restarts, reply: oneshot::Sender<RestartReply>, reason: &str) {
    warn!(reason, "restart refused; serving on");
    let _ = reply.send(RestartReply::Refused(reason.to_owned()));
    restarts.failed(reason);
}

/// What a restart needs once its binary is checked. Only the serve loop
/// runs one (it is the single restart coordinator, the only caller of
/// [`Node::prepare_handoff`] and [`Node::thaw`]).
#[derive(Clone)]
pub(crate) struct Restart {
    pub(crate) restarts: Restarts,
    pub(crate) node: Arc<Node>,
    pub(crate) state_dir: PathBuf,
    pub(crate) instance: u64,
    pub(crate) listener_fd: RawFd,
    pub(crate) lock_fd: RawFd,
}

impl Restart {
    /// Runs a checked restart on a blocking thread (see [`Restart::run`]).
    /// The handle finishes only if the restart failed, once the requester
    /// is answered and the failure recorded. The serve loop awaits it
    /// without accepting: connections wait in the listener's backlog for
    /// whichever daemon serves next.
    pub(crate) fn finish(self, (req, binary, checked): Checked) -> JoinHandle<()> {
        tokio::task::spawn_blocking(move || {
            let mut reply = Some(req.reply);
            let reason = match checked {
                Ok(()) => {
                    info!("restart: check passed");
                    self.run(&binary, &mut reply)
                }
                Err(reason) => reason,
            };
            match reply {
                Some(reply) => refuse(&self.restarts, reply, &reason),
                None => {
                    warn!(reason, "restart failed after its answer; serving on");
                    self.restarts.failed(&reason);
                }
            }
        })
    }

    /// Stops requests, freezes the node, writes the handoff record, answers
    /// the requester `RestartingDaemon` (taking `reply`), waits for that to
    /// be written (at most [`RESTART_FLUSH`]) and `exec`s `binary`.
    ///
    /// Returns only if something failed before or at the `exec`, with the
    /// reason, everything undone: the node thawed, requests admitted again,
    /// no record left. `reply` is still there if the requester was not
    /// answered yet.
    fn run(
        &self,
        binary: &std::path::Path,
        reply: &mut Option<oneshot::Sender<RestartReply>>,
    ) -> String {
        let started = Instant::now();
        let frozen_unix_ms = unix_ms();
        let deadline = started + FREEZE_DEADLINE;
        let Some(stopped) = self.restarts.stop_requests(deadline) else {
            return "agents are starting; try again".into();
        };
        let agents = match self.node.prepare_handoff(deadline) {
            Ok(agents) => agents,
            // Nothing stays frozen (see `prepare_handoff`).
            Err(e) => return e.to_string(),
        };
        let mut keep = vec![self.listener_fd, self.lock_fd];
        keep.extend(agents.iter().filter_map(|a| match a.terminal {
            Some(handoff_agent::Terminal::Fd(fd)) => Some(fd),
            _ => None,
        }));
        let pids: Vec<i32> = agents.iter().map(|a| a.pid).collect();
        let count = agents.len() as u32;
        let rec = HandoffRecord {
            version: HANDOFF_VERSION,
            listener_fd: self.listener_fd,
            lock_fd: self.lock_fd,
            previous_instance: self.instance,
            agents,
            frozen_unix_ms,
        };
        let reason = match handoff::write_record(&self.state_dir, &rec) {
            Ok(record_fd) => {
                drop(rec);
                handoff::test_corrupt_record(&self.state_dir);
                info!(
                    agents = count,
                    froze_in_ms = started.elapsed().as_millis() as u64,
                    "restart: agents frozen and recorded; exec"
                );
                let (flushed, written) = std::sync::mpsc::sync_channel(1);
                if let Some(r) = reply.take() {
                    let restarting = RestartReply::Restarting {
                        agents: count,
                        flushed,
                    };
                    // A requester that has gone does not stop the restart.
                    if r.send(restarting).is_ok() {
                        let _ = written.recv_timeout(RESTART_FLUSH);
                    }
                }
                let e = handoff::exec(binary, &self.state_dir, record_fd, &keep, &pids);
                format!("exec {}: {e}", binary.display())
            }
            Err(e) => format!("writing the handoff record: {e}"),
        };
        handoff::remove_stale(&self.state_dir);
        self.node.thaw();
        drop(stopped);
        reason
    }
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}
