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

use std::any::Any;
use std::io;
use std::os::fd::RawFd;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
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
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on stopping requests and freezing the node for a restart.
pub const FREEZE_DEADLINE: Duration = Duration::from_secs(3);

/// A restart and the outcome of its binary's check.
pub(crate) type Checked = (RestartRequest, PathBuf, Result<(), String>);

/// Opens the node from a predecessor's handoff record, then starts the
/// login-env capture: the handed-over terminals are claimed before the
/// capture opens a terminal of its own. `handed` (the pids the predecessor
/// passed) is settled once the node is open (see [`HandedAgents::opened`]); if
/// this fails or panics first, dropping it ends them all.
pub(crate) fn open_from_handoff(
    state_dir: PathBuf,
    rec: HandoffRecord,
    handed: HandedAgents,
    shell: Option<PathBuf>,
) -> JoinHandle<io::Result<(Node, RestoreSummary)>> {
    tokio::task::spawn_blocking(move || {
        let pid_of: Vec<(u64, i32)> = rec.agents.iter().map(|a| (a.id, a.pid)).collect();
        let (node, kept, failed) = Node::open_with_handoff(&state_dir, rec.agents)?;
        handoff::test_panic("FLEETD_TEST_PANIC_OPENING_HANDOFF");
        // A repeated id keeps its first entry (see `open_with_handoff`), so
        // the first pid listed for a kept id is the kept agent's.
        let kept_pids: Vec<i32> = kept
            .iter()
            .filter_map(|id| pid_of.iter().find(|(i, _)| i == id).map(|&(_, pid)| pid))
            .collect();
        // Before the capture starts a child of its own.
        handed.opened(&kept_pids);
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

/// The agents a predecessor's `exec` handed to this process, by the pids
/// it passed in [`handoff::HANDOFF_PIDS_ENV`]: its children, holding the
/// terminals it inherited. Until the node has opened from the record and
/// adopted those it lists ([`HandedAgents::opened`]), nothing else tracks them.
///
/// Dropped before that, because the start ended early (`serve` failed
/// before the node opened, opening it failed or panicked, or the runtime
/// could not be built), it ends every one that is still a child of this
/// process (SIGTERM to its group, SIGKILL after the shutdown grace, reaped)
/// and marks every agent the store has running as lost, so none runs on
/// untracked whichever way the process goes. It may block for that grace,
/// also on a runtime thread: only on a failed start.
pub(crate) struct HandedAgents {
    pids: Vec<i32>,
    state_dir: PathBuf,
}

impl HandedAgents {
    pub(crate) fn new(pids: Vec<i32>, state_dir: PathBuf) -> HandedAgents {
        HandedAgents { pids, state_dir }
    }

    /// The node is open with `kept` (the pids of the agents it adopted):
    /// ends every other handed pid still a child of this process, which a
    /// record listing fewer agents than the pids left to no one.
    fn opened(mut self, kept: &[i32]) {
        // Kept pids are left out before `end_children` looks at them: it
        // reaps an exited child, which would take a kept agent's exit status
        // from its reader.
        let extra: Vec<i32> = std::mem::take(&mut self.pids)
            .into_iter()
            .filter(|pid| !kept.contains(pid))
            .collect();
        let ended = handoff::end_children(&extra);
        if ended > 0 {
            warn!(
                agents = ended,
                "the handoff record left out running agents; ended them"
            );
        }
    }
}

impl Drop for HandedAgents {
    fn drop(&mut self) {
        if self.pids.is_empty() {
            return;
        }
        let ended = handoff::end_children(&self.pids);
        let lost = Node::mark_left_running_lost(&self.state_dir);
        warn!(agents = ended, lost = ?lost, "the handoff was not adopted; ended its agents");
    }
}

/// Resolves the restart's binary (none named: `own_binary`, the path this
/// daemon runs from) and checks it with `--handoff-check` on a blocking
/// thread (the serve loop accepts meanwhile); `None` if it was refused at
/// once.
pub(crate) fn check(
    restarts: &Restarts,
    req: RestartRequest,
    own_binary: &str,
) -> Option<JoinHandle<Checked>> {
    let binary = match handoff::resolve_binary(&req.binary, own_binary) {
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
    ///
    /// A panic in the restart is caught and answered like any failure (the
    /// handoff undone, see [`Restart::run`]): the requester gets an `Error`,
    /// not a dropped reply.
    pub(crate) fn finish(self, (req, binary, checked): Checked) -> JoinHandle<()> {
        tokio::task::spawn_blocking(move || {
            let mut reply = Some(req.reply);
            let reason = match checked {
                Ok(()) => {
                    info!("restart: check passed");
                    let run = AssertUnwindSafe(|| self.run(&binary, &mut reply));
                    std::panic::catch_unwind(run).unwrap_or_else(|panic| panicked(&*panic))
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
    /// answered yet. A panic once the node is frozen undoes the same on its
    /// way out ([`Undo`]): otherwise every reader would stay parked, input
    /// fail and every spawn be refused until the daemon was killed.
    fn run(
        &self,
        binary: &std::path::Path,
        reply: &mut Option<oneshot::Sender<RestartReply>>,
    ) -> String {
        let started = Instant::now();
        let frozen_unix_ms = unix_ms();
        let deadline = started + FREEZE_DEADLINE;
        // Dropped last: requests are admitted only once the node thawed.
        let Some(_stopped) = self.restarts.stop_requests(deadline) else {
            return "agents are starting; try again".into();
        };
        let agents = match self.node.prepare_handoff(deadline) {
            Ok(agents) => agents,
            // Nothing stays frozen (see `prepare_handoff`).
            Err(e) => return e.to_string(),
        };
        // From here every way out of `run` but the `exec` (a return or a
        // panic) undoes the handoff.
        let _undo = Undo {
            node: &self.node,
            state_dir: &self.state_dir,
        };
        handoff::test_panic("FLEETD_TEST_PANIC_AFTER_FREEZE");
        let mut keep = vec![self.listener_fd, self.lock_fd];
        keep.extend(agents.iter().filter_map(|a| match a.terminal {
            Some(handoff_agent::Terminal::Fd(fd)) => Some(fd),
            _ => None,
        }));
        let pids: Vec<i32> = agents.iter().map(|a| a.pid).collect();
        let count = agents.len() as u32;
        let mut rec = HandoffRecord {
            version: HANDOFF_VERSION,
            listener_fd: self.listener_fd,
            lock_fd: self.lock_fd,
            previous_instance: self.instance,
            agents,
            frozen_unix_ms,
        };
        handoff::test_short_record(&mut rec);
        match handoff::write_record(&self.state_dir, &rec) {
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
        }
    }
}

/// Undoes a prepared handoff when dropped: removes the record, if written,
/// and thaws the node.
struct Undo<'a> {
    node: &'a Node,
    state_dir: &'a Path,
}

impl Drop for Undo<'_> {
    fn drop(&mut self) {
        handoff::remove_stale(self.state_dir);
        self.node.thaw();
    }
}

/// The reason given for a restart that panicked.
fn panicked(panic: &(dyn Any + Send)) -> String {
    let what = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a panic");
    format!("internal error ({what})")
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}
