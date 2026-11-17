//! Inferred `waiting` (ADR 0088, issue #126): a sweep over running agents
//! that tells a parent when an agent has gone quiet at a prompt, and when
//! it has stopped being so.
//!
//! Going quiet produces no event, so something has to look. The sweep is
//! one thread, started by the first agent that runs and parked while none
//! does; it wakes every [`SWEEP_PERIOD`], reads each running agent's two
//! numbers under the pipeline lock and keeps the last line only for agents
//! that have been quiet. Nothing here runs on the PTY output path.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::Thread;
use std::time::Duration;

use fleet_proto::AgentInfo;
use fleet_proto::status::{AgentStatus, WAITING_QUIET_MS, inferred_status, live_report};
use tokio::sync::broadcast;
use tracing::warn;

use super::mailbox::post_status;
use super::{AgentPipeline, Agents, Node, agent_info, lock};
use crate::lease::now_ms;
use crate::mailbox::{Notice, Status};
use crate::store::Store;

/// How often the sweep looks while an agent runs. Detection of a prompt
/// that has been quiet for [`WAITING_QUIET_MS`] is up to this late.
pub const SWEEP_PERIOD: Duration = Duration::from_secs(5);

const SWEEP_STACK_BYTES: usize = 256 * 1024;

/// Whether `info` is inferred to be waiting: running, not a shell, no live
/// reported status (a report is the agent's own word and wins, ADR 0088),
/// quiet for [`WAITING_QUIET_MS`] at a prompt-like last line. The one rule
/// behind both the sweep and `fleet wait --state waiting`.
pub(super) fn is_waiting(info: &AgentInfo, last_line: &str, last_output_ms: i64, now: i64) -> bool {
    live_report_absent(info)
        && inferred_status(
            info,
            last_line,
            (last_output_ms > 0).then_some(last_output_ms),
            false,
            now,
            WAITING_QUIET_MS,
        ) == Some(AgentStatus::NeedsYou)
}

fn live_report_absent(info: &AgentInfo) -> bool {
    live_report(info).is_none()
}

/// What the sweep remembers between rounds.
#[derive(Default)]
struct Seen {
    /// Agents it has reported as inferred-waiting.
    waiting: HashSet<u64>,
    /// Each agent's last line, as of the output time it was read at, so a
    /// quiet agent's screen is not read again.
    lines: HashMap<u64, (i64, String)>,
}

pub(super) struct Sweeper {
    agents: Arc<Mutex<Agents>>,
    store: Arc<Mutex<Store>>,
    notices: broadcast::Sender<Notice>,
    seen: Mutex<Seen>,
    /// Started by the first running agent, so a node that never runs one
    /// has no thread; `None` if it could not be started.
    thread: OnceLock<Option<Thread>>,
    stop: AtomicBool,
}

impl Sweeper {
    pub(super) fn new(
        agents: Arc<Mutex<Agents>>,
        store: Arc<Mutex<Store>>,
        notices: broadcast::Sender<Notice>,
    ) -> Sweeper {
        Sweeper {
            agents,
            store,
            notices,
            seen: Mutex::new(Seen::default()),
            thread: OnceLock::new(),
            stop: AtomicBool::new(false),
        }
    }

    /// An agent started running: starts the thread (the first time) or
    /// wakes it from its park.
    pub(super) fn kick(self: &Arc<Self>) {
        if let Some(thread) = self.thread.get_or_init(|| self.start()) {
            thread.unpark();
        }
    }

    fn start(self: &Arc<Self>) -> Option<Thread> {
        let me = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("fleet-waiting".into())
            .stack_size(SWEEP_STACK_BYTES)
            .spawn(move || me.run());
        match spawned {
            Ok(handle) => Some(handle.thread().clone()),
            Err(e) => {
                warn!(error = %e, "starting the waiting sweep failed");
                None
            }
        }
    }

    pub(super) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(Some(thread)) = self.thread.get() {
            thread.unpark();
        }
    }

    /// Sweeps every [`SWEEP_PERIOD`] while an agent runs, parks while none
    /// does (a spawn unparks it; the unpark token survives a spawn that
    /// lands between the last sweep and the park).
    fn run(&self) {
        while !self.stop.load(Ordering::Acquire) {
            if self.sweep(now_ms()) {
                std::thread::park_timeout(SWEEP_PERIOD);
            } else {
                std::thread::park();
            }
        }
    }

    /// One round at `now` (Unix ms): posts the transitions into and out of
    /// inferred `waiting`. `false` if no agent runs.
    pub(super) fn sweep(&self, now: i64) -> bool {
        let live: Vec<(u64, AgentPipeline)> = lock(&self.agents)
            .live
            .iter()
            .map(|(id, l)| (*id, l.pipeline.clone()))
            .collect();
        if live.is_empty() {
            *lock(&self.seen) = Seen::default();
            return false;
        }
        let mut seen = lock(&self.seen);
        let ids: HashSet<u64> = live.iter().map(|(id, _)| *id).collect();
        seen.lines.retain(|id, _| ids.contains(id));

        let mut waiting = HashSet::new();
        for (id, pipeline) in &live {
            let at = pipeline.last_output_unix_ms();
            // Never printed, or printed within the quiet period: working
            // or idle, not waiting; the screen need not be read.
            if at <= 0 || now - at < WAITING_QUIET_MS {
                continue;
            }
            if seen.lines.get(id).is_none_or(|(read_at, _)| *read_at != at) {
                let Some((line, at)) = pipeline.activity() else {
                    continue;
                };
                seen.lines.insert(*id, (at, line));
            }
            let (_, line) = &seen.lines[id];
            // The store only for an agent that looks like it waits.
            if !fleet_proto::status::looks_like_prompt(line) {
                continue;
            }
            let Some(info) = self.info(*id) else { continue };
            if is_waiting(&info, line, at, now) {
                waiting.insert(*id);
            }
        }

        let entered: Vec<u64> = waiting.difference(&seen.waiting).copied().collect();
        let left: Vec<u64> = seen
            .waiting
            .difference(&waiting)
            .filter(|id| ids.contains(id))
            .copied()
            .collect();
        seen.waiting = waiting;
        drop(seen);
        for id in entered {
            self.post(id, Status::Waiting);
        }
        for id in left {
            // An agent that has since reported speaks for itself; an exited
            // one is no longer in `live`.
            if self.info(id).is_some_and(|i| live_report(&i).is_none()) {
                self.post(id, Status::Working);
            }
        }
        true
    }

    fn info(&self, id: u64) -> Option<AgentInfo> {
        lock(&self.store).agent(id).ok().flatten().map(agent_info)
    }

    fn post(&self, id: u64, status: Status) {
        if let Err(e) = post_status(&self.store, &self.notices, id, status, "", true) {
            warn!(agent = id, error = %e, "posting an inferred status failed");
        }
    }
}

impl Node {
    /// Runs one waiting sweep as of `now_ms` (Unix ms); `false` if no agent
    /// runs. The sweep runs by itself every [`SWEEP_PERIOD`]; this is for
    /// tests, which supply the clock.
    #[doc(hidden)]
    pub fn sweep_waiting_at(&self, now_ms: i64) -> bool {
        self.sweeper.sweep(now_ms)
    }

    /// The running agent's last non-blank line and last output time (Unix
    /// ms; 0: none yet); `None` if it does not run here.
    pub fn last_activity(&self, id: u64) -> Option<(String, i64)> {
        let live = lock(&self.agents).live.get(&id).cloned()?;
        live.pipeline.activity()
    }

    /// [`Node::last_activity`] of each of `ids` that runs and has printed.
    pub(super) fn live_activity(
        &self,
        ids: impl Iterator<Item = u64>,
    ) -> HashMap<u64, (String, i64)> {
        ids.filter_map(|id| Some((id, self.last_activity(id)?)))
            .filter(|(_, (_, at))| *at > 0)
            .collect()
    }
}
