//! The session layer: one control connection (`gui`) for the sidebar,
//! spawn and kill, and one connection per pane ([`PaneLink`]). It turns
//! protocol traffic into calls on [`PaneModel`] and the sidebar rows, and
//! into [`UiEvent`] wake-ups for the UI.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fleet_client::names::NAME_LABEL;
use fleet_client::{
    Closer, Connection, FrameReader, RESTART_ANSWER_WAIT, RESTART_WAIT, RestartOutcome, Sender,
    TOO_OLD, did_not_come_back, restart_failed, restart_outcome,
};
use fleet_proto::{
    AgentInfo, AgentState, BinaryIdentity, DecideSpawn, ForgetAgents, Frame, HelloOk, KillAgent,
    ListAgents, ListMessageEdges, ListPendingSpawns, PendingSpawn, Response, RestartAgent,
    RestartDaemon, SearchHits, SearchOutput, SendInput, SetLabels, SpawnAgent, event, request,
    response,
};
use fleet_term::Palette;

use super::backoff::Backoff;
use super::launch::{find_tool, update_available};
use super::message_lines::{self, Edge as MessageEdge};
use super::pane::PaneModel;
use super::pane_link::PaneLink;
use super::search_all;
use super::spawn::SpawnRequest;
use super::target::{Connected, Target, connect};

/// How the control connection introduces itself.
pub const CLIENT_NAME: &str = "gui";
/// How a [`Session::read_output`] connection introduces itself.
const OUTPUT_CLIENT_NAME: &str = "gui-output";
/// How a [`Session::search_output`] connection introduces itself.
const SEARCH_CLIENT_NAME: &str = "gui-search";

/// What an old daemon answers a request it does not know with.
const UNKNOWN_REQUEST: &str = "empty request";
/// The verb tracking the pending-spawns list asked for on connect.
const LIST_PENDING_SPAWNS: &str = "list spawn requests";
/// Shown instead of [`UNKNOWN_REQUEST`]: the only requests the GUI sends
/// that an older fleetd lacks are restart and forget. [`error_text`] maps
/// "empty request" for every reply, whatever the request was: revisit this
/// when a new request kind is added.
pub(crate) const DAEMON_TOO_OLD: &str = "fleetd is too old to restart or remove agents";

/// The UI's text for an error reply.
pub(crate) fn error_text(message: String) -> String {
    if message == UNKNOWN_REQUEST {
        DAEMON_TOO_OLD.into()
    } else {
        message
    }
}

/// What the toast says for a [`UiEvent::DaemonRestartFailed`] reason: a
/// daemon's own reason gets the CLI's wrapper (it is still running); the
/// too-old and timed-out copy stands alone, as in the CLI.
pub fn restart_failure_text(reason: &str) -> String {
    if reason == TOO_OLD || reason == did_not_come_back() {
        reason.to_owned()
    } else {
        restart_failed(reason)
    }
}

/// The verb tracked for a `RestartDaemon` on the control connection.
const RESTART_DAEMON: &str = "restart fleetd";

/// What the session knows of the fleetd on its control connection.
#[derive(Default)]
struct DaemonState {
    /// From the latest handshake; `None` while disconnected.
    instance: Option<u64>,
    /// The binary that daemon runs from (`None` from an older fleetd).
    binary: Option<BinaryIdentity>,
    /// A restart asked for and not yet decided.
    restart: Option<PendingRestart>,
    /// Hands out [`PendingRestart::token`]s.
    restarts: u64,
}

/// A restart in flight. It ends when a handshake decides it
/// ([`restart_outcome`] other than `Pending`), the daemon refuses, or it
/// times out: [`RESTART_ANSWER_WAIT`] without the old daemon's answer, or
/// [`RESTART_WAIT`] from that answer (as the CLI waits) without a decision.
struct PendingRestart {
    /// Names this restart to its timer, so a later restart is not cut short
    /// by an earlier one's.
    token: u64,
    /// The instance whose handshake we saw before asking.
    old_instance: u64,
    /// `RestartingDaemon.agents`, once the old daemon has answered.
    agents: Option<u32>,
    /// When that answer arrived: [`RESTART_WAIT`] runs from here.
    answered_at: Option<Instant>,
}

impl DaemonState {
    /// A handshake arrived: remember it and, if a restart is waiting on
    /// the old daemon's answer, decide it. `Pending` (the old daemon, not
    /// yet thawed) keeps waiting.
    fn on_handshake(&mut self, hello: &HelloOk) -> Option<UiEvent> {
        self.instance = Some(hello.instance);
        self.binary = hello.binary.as_deref().cloned();
        let pending = self.restart.as_ref()?;
        let outcome = restart_outcome(pending.old_instance, pending.agents?, hello);
        let event = match outcome {
            RestartOutcome::Pending => return None,
            RestartOutcome::Failed(reason) => UiEvent::DaemonRestartFailed(reason),
            done => UiEvent::DaemonRestarted(done),
        };
        self.restart = None;
        Some(event)
    }
}

/// Sidebar re-list interval.
const RELIST: Duration = Duration::from_secs(5);
/// How often the shown graph asks who wrote to whom.
const EDGES_POLL: Duration = Duration::from_secs(2);
/// How long [`Session::shutdown`] waits for panes to detach before closing
/// their connections outright (gpui's quit budget is 100 ms).
const SHUTDOWN_GRACE: Duration = Duration::from_millis(60);

/// A wake-up for the UI, which coalesces them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiEvent {
    /// The sidebar rows changed; read [`Session::agents`].
    AgentsChanged,
    /// The message lines changed; read [`Session::message_edges`] (ADR 0089).
    MessageEdgesChanged,
    /// `agent_id` was forgotten, or restarted as `replaced_by`: panes and
    /// saved layouts showing it follow. Sent for the control connection's
    /// copy of `AgentRemoved` only.
    AgentRemoved {
        agent_id: u64,
        replaced_by: Option<u64>,
    },
    /// The pane's model changed (sent at most once per batch of frames).
    PaneChanged(PaneId),
    /// The pane's connection dropped while a spawn or restart into it was
    /// unanswered: the pane shows no agent until it is told which (the
    /// launch may or may not have happened; the control connection says).
    LaunchDropped(PaneId),
    /// The control connection's state.
    Connection(ConnState),
    /// Connecting failed (once per outage); moot once connected again.
    ConnectionError(String),
    Error(String),
    /// The restart [`Session::restart_daemon`] asked for finished and the
    /// new fleetd answered: [`RestartOutcome::Restarted`] when every agent
    /// was kept, [`RestartOutcome::Partial`] when some were ended. The
    /// toast text is the outcome's `Display`, which is the CLI's line.
    DaemonRestarted(RestartOutcome),
    /// The restart did not happen (the daemon refused or could not exec and
    /// is still running; it is too old; or none answered in time). Show it
    /// with [`restart_failure_text`].
    DaemonRestartFailed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected { retry_in: Duration },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PaneId(pub u32);

/// One sidebar row.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentRow {
    pub info: AgentInfo,
    pub last_line: String,
    pub last_output_unix_ms: Option<i64>,
}

fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Colours used until [`Session::set_palette`]: xterm's defaults.
const DEFAULT_PALETTE: Palette = Palette {
    fg: (0xe5, 0xe5, 0xe5),
    bg: (0x00, 0x00, 0x00),
    cursor: (0xe5, 0xe5, 0xe5),
    ansi: [
        (0x00, 0x00, 0x00),
        (0xcd, 0x00, 0x00),
        (0x00, 0xcd, 0x00),
        (0xcd, 0xcd, 0x00),
        (0x00, 0x00, 0xee),
        (0xcd, 0x00, 0xcd),
        (0x00, 0xcd, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x7f, 0x7f, 0x7f),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x5c, 0x5c, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ],
};

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What every connection of a session shares.
pub(crate) struct Ctx {
    pub(crate) target: Target,
    pub(crate) state_dir: PathBuf,
    ui: async_channel::Sender<UiEvent>,
}

impl Ctx {
    #[cfg(test)]
    pub(crate) fn for_test() -> Arc<Ctx> {
        Arc::new(Ctx {
            target: Target::Local,
            state_dir: PathBuf::from("/nonexistent"),
            ui: async_channel::unbounded().0,
        })
    }

    /// [`Ctx::for_test`], with the UI's end of the event channel.
    #[cfg(test)]
    pub(crate) fn for_test_with_events() -> (Arc<Ctx>, async_channel::Receiver<UiEvent>) {
        let (ui, rx) = async_channel::unbounded();
        let ctx = Arc::new(Ctx {
            target: Target::Local,
            state_dir: PathBuf::from("/nonexistent"),
            ui,
        });
        (ctx, rx)
    }

    /// Wakes the UI. The channel is unbounded (see [`Session::start`]), so
    /// only a closed one (the UI is gone) loses events.
    pub(crate) fn notify(&self, e: UiEvent) {
        let _ = self.ui.try_send(e);
    }
}

/// How to close one live connection from another thread.
pub(crate) struct ConnHandle {
    closer: Option<Closer>,
    child: Option<Child>,
}

impl ConnHandle {
    pub(crate) fn new(conn: &Connection, child: Option<Child>) -> Self {
        Self {
            closer: conn.closer(),
            child,
        }
    }

    /// Ends the connection: the reader then sees EOF.
    pub(crate) fn close(&mut self) {
        if let Some(c) = &self.closer {
            c.close();
        }
        if let Some(child) = &mut self.child {
            let _ = child.kill();
        }
    }

    /// Closes and reaps the ssh child, if any.
    pub(crate) fn finish(mut self) {
        self.close();
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
    }
}

/// A stop flag a backoff wait can be woken from.
#[derive(Default)]
pub(crate) struct Stop {
    stopped: Mutex<bool>,
    wake: Condvar,
}

impl Stop {
    pub(crate) fn set(&self) {
        *lock(&self.stopped) = true;
        self.wake.notify_all();
    }

    pub(crate) fn is_set(&self) -> bool {
        *lock(&self.stopped)
    }

    /// Sleeps for `d` unless stopped first; `true` if stopped.
    pub(crate) fn wait(&self, d: Duration) -> bool {
        let deadline = Instant::now() + d;
        let mut stopped = lock(&self.stopped);
        while !*stopped {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            stopped = self
                .wake
                .wait_timeout(stopped, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        *stopped
    }
}

/// The frames that arrived together: the first one blocks, the rest were
/// already read. `Err` once the connection is gone (after the frames read
/// before it ended).
pub(crate) fn read_batch(reader: &mut FrameReader) -> Result<Vec<Frame>, ()> {
    let first = match reader.read_frame() {
        Ok(Some(f)) => f,
        _ => return Err(()),
    };
    let mut batch = vec![first];
    loop {
        match reader.buffered_frame() {
            Ok(Some(f)) => batch.push(f),
            Ok(None) => return Ok(batch),
            Err(_) => return Ok(batch),
        }
    }
}

/// How long activity for an id the last list did not show is kept.
const UNLISTED_ACTIVITY_TTL: Duration = Duration::from_secs(2 * RELIST.as_secs());

#[derive(Default)]
struct Sidebar {
    infos: BTreeMap<u64, AgentInfo>,
    /// `(last_line, last_output_unix_ms, received)` from `AgentActivity`.
    /// Unlisted ids are kept for [`UNLISTED_ACTIVITY_TTL`], so an agent
    /// spawned after a list was taken has its line once it is listed.
    activity: HashMap<u64, (String, i64, Instant)>,
    /// `AgentList.replaced` from the latest list: old id to its newest
    /// successor.
    replaced: HashMap<u64, u64>,
    /// A `ListAgents` was asked for because of an unknown id.
    list_requested: bool,
    /// A `ListAgents` reply arrived.
    listed: bool,
    /// How many `ListAgents` replies arrived.
    lists: u64,
    /// Agents' spawns waiting for the user's approval (ADR 0079), by id.
    pending_spawns: BTreeMap<u64, PendingSpawn>,
    /// The latest `MessageEdges` reply (only asked for while the graph is
    /// shown).
    edges: Vec<MessageEdge>,
}

impl Sidebar {
    /// Applies a `ListAgents` reply: the rows are replaced and activity
    /// for ids not listed is dropped unless it is recent.
    fn set_list(&mut self, agents: Vec<AgentInfo>, replaced: HashMap<u64, u64>, now: Instant) {
        self.replaced = replaced;
        self.infos = agents.into_iter().map(|a| (a.id, a)).collect();
        let infos = &self.infos;
        self.activity.retain(|id, a| {
            infos.contains_key(id) || now.duration_since(a.2) < UNLISTED_ACTIVITY_TTL
        });
        self.list_requested = false;
        self.listed = true;
        self.lists += 1;
    }

    /// `old` was removed, restarted as `new` if set: `replaced` keeps
    /// leading every old id to its newest successor (as the node's map
    /// does), and nothing leads to a forgotten id.
    fn follow_removed(&mut self, old: u64, new: Option<u64>) {
        match new {
            Some(new) => {
                for v in self.replaced.values_mut() {
                    if *v == old {
                        *v = new;
                    }
                }
                self.replaced.insert(old, new);
            }
            None => {
                self.replaced.remove(&old);
                self.replaced.retain(|_, v| *v != old);
            }
        }
    }
}

enum ControlCmd {
    Connected(Sender),
    Disconnected,
    List,
    /// Start or stop asking for message edges (the graph is shown or not).
    MessageEdges(bool),
    Kill(u64),
    Restart(u64),
    Forget(Vec<u64>),
    /// Sets, then removes, an agent's labels (Rename sets `name`, ADR
    /// 0052; Edit Labels the rest, ADR 0087), then lists. `verb` names it
    /// in an error.
    SetLabels {
        agent_id: u64,
        set: HashMap<String, String>,
        remove: Vec<String>,
        verb: &'static str,
    },
    /// Types into an agent (Send to Agents…, ADR 0083).
    Input(u64, Vec<u8>),
    /// Answers an agent's open question (ADR 0078); `question` names it.
    Answer {
        agent_id: u64,
        question: u64,
        text: String,
    },
    ClearExited,
    RestartDaemon(String),
    /// Approves or denies a pending spawn (ADR 0079).
    DecideSpawn(u64, bool),
    Stop,
}

struct Inner {
    ctx: Arc<Ctx>,
    sidebar: Arc<Mutex<Sidebar>>,
    control: Mutex<mpsc::Sender<ControlCmd>>,
    control_stop: Arc<Stop>,
    control_conn: Arc<Mutex<Option<ConnHandle>>>,
    daemon: Arc<Mutex<DaemonState>>,
    /// How long a restart waits for the new daemon ([`RESTART_WAIT`]; a
    /// test hook shortens it).
    restart_wait: Mutex<Duration>,
    /// How long a restart waits for the old daemon's answer
    /// ([`RESTART_ANSWER_WAIT`]; a test hook shortens it).
    restart_answer_wait: Mutex<Duration>,
    /// The local `fleetd` the update check and a restart use, in place of
    /// [`find_tool`]'s (a test hook sets it).
    fleetd: Mutex<Option<PathBuf>>,
    panes: Mutex<HashMap<PaneId, PaneLink>>,
    palette: Mutex<Palette>,
    shut_down: AtomicBool,
}

/// The GUI's connections to one target. Cheap to clone; every clone is
/// the same session.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

impl Session {
    /// Starts the control connection thread: it handshakes as `gui`, lists
    /// the agents, follows activity and exit events, re-lists every 5 s
    /// (and at once for an unknown id), and reconnects with [`Backoff`].
    ///
    /// `ui` must be an [`async_channel::unbounded`] channel's sender: events
    /// are sent with `try_send`, and a bounded channel that fills up would
    /// lose `Connection` and `Error` events.
    pub fn start(
        target: Target,
        state_dir: PathBuf,
        ui: async_channel::Sender<UiEvent>,
    ) -> Session {
        Session::start_with(target, state_dir, ui, None)
    }

    /// [`Session::start`], with `first` (already connected to `target`
    /// and handshaken, e.g. the startup probe's connection) as the first
    /// control connection; every later reconnect connects anew.
    pub fn start_with(
        target: Target,
        state_dir: PathBuf,
        ui: async_channel::Sender<UiEvent>,
        first: Option<Connected>,
    ) -> Session {
        let ctx = Arc::new(Ctx {
            target,
            state_dir,
            ui,
        });
        let (tx, rx) = mpsc::channel();
        let inner = Arc::new(Inner {
            ctx: Arc::clone(&ctx),
            sidebar: Arc::default(),
            control: Mutex::new(tx.clone()),
            control_stop: Arc::default(),
            control_conn: Arc::default(),
            daemon: Arc::default(),
            restart_wait: Mutex::new(RESTART_WAIT),
            restart_answer_wait: Mutex::new(RESTART_ANSWER_WAIT),
            fleetd: Mutex::default(),
            panes: Mutex::default(),
            palette: Mutex::new(DEFAULT_PALETTE),
            shut_down: AtomicBool::new(false),
        });
        let writer_ctx = Arc::clone(&ctx);
        let pending = ControlPending::default();
        let writer_pending = Arc::clone(&pending);
        thread::Builder::new()
            .name("gui-control-writer".into())
            .spawn(move || control_writer(&writer_ctx, &rx, &writer_pending))
            .expect("spawn control writer");
        let reader = ControlReader {
            first,
            ctx,
            sidebar: Arc::clone(&inner.sidebar),
            cmds: tx,
            stop: Arc::clone(&inner.control_stop),
            conn: Arc::clone(&inner.control_conn),
            daemon: Arc::clone(&inner.daemon),
            pending,
        };
        thread::Builder::new()
            .name("gui-control".into())
            .spawn(move || reader.run())
            .expect("spawn control reader");
        Session { inner }
    }

    /// A `ListAgents` reply arrived: [`Session::agents`] lists them.
    pub fn listed(&self) -> bool {
        lock(&self.inner.sidebar).listed
    }

    /// How many `ListAgents` replies arrived (on any connection).
    pub fn lists(&self) -> u64 {
        lock(&self.inner.sidebar).lists
    }

    /// Agents' spawns waiting for the user's approval (ADR 0079), oldest
    /// first.
    pub fn pending_spawns(&self) -> Vec<PendingSpawn> {
        lock(&self.inner.sidebar)
            .pending_spawns
            .values()
            .cloned()
            .collect()
    }

    /// Approves or denies pending spawn `id`; the change arrives as an
    /// event, a failure (it was no longer pending) as [`UiEvent::Error`].
    pub fn decide_spawn(&self, id: u64, approve: bool) {
        self.control(ControlCmd::DecideSpawn(id, approve));
    }

    /// A snapshot of the sidebar rows, by agent id.
    pub fn agents(&self) -> Vec<AgentRow> {
        let sb = lock(&self.inner.sidebar);
        sb.infos
            .values()
            .map(|info| {
                let activity = sb.activity.get(&info.id);
                AgentRow {
                    info: info.clone(),
                    last_line: activity.map(|a| a.0.clone()).unwrap_or_default(),
                    last_output_unix_ms: activity.map(|a| a.1),
                }
            })
            .collect()
    }

    /// Spawns an agent attached to `pane`, on the pane's own connection, so
    /// the pane's grid answers its startup queries. The result arrives as
    /// `SpawnedAttached` into that pane; a failure as [`UiEvent::Error`].
    pub fn spawn(
        &self,
        req: SpawnRequest,
        cols: u16,
        rows: u16,
        pane: PaneId,
    ) -> Result<(), String> {
        let spawn = SpawnAgent {
            program: req.program,
            args: req.args,
            cwd: req.cwd,
            labels: req.labels,
            cols: cols.into(),
            rows: rows.into(),
            attach: true,
            ..Default::default()
        };
        if self.inner.shut_down.load(Ordering::SeqCst) {
            return Err("the session is shut down".into());
        }
        match lock(&self.inner.panes).get(&pane) {
            Some(link) => {
                link.spawn(spawn);
                Ok(())
            }
            None => Err(format!("no pane {}", pane.0)),
        }
    }

    /// `AgentList.replaced` from the latest `ListAgents` reply: an old id
    /// to its newest successor, for panes that missed the `AgentRemoved`.
    pub fn replaced(&self) -> HashMap<u64, u64> {
        lock(&self.inner.sidebar).replaced.clone()
    }

    pub fn kill(&self, agent_id: u64) {
        self.control(ControlCmd::Kill(agent_id));
    }

    /// Restarts an exited agent on the control connection, unattached;
    /// panes showing it follow the `AgentRemoved` that names the new id.
    pub fn restart(&self, agent_id: u64) {
        self.control(ControlCmd::Restart(agent_id));
    }

    /// Restarts an exited agent at `cols`x`rows` on `pane`'s connection,
    /// attached: the pane shows the new agent once `Restarted` arrives; a
    /// failure shows the old one again, with a [`UiEvent::Error`].
    pub fn restart_in_pane(
        &self,
        pane: PaneId,
        agent_id: u64,
        cols: u16,
        rows: u16,
    ) -> Result<(), String> {
        if self.inner.shut_down.load(Ordering::SeqCst) {
            return Err("the session is shut down".into());
        }
        match lock(&self.inner.panes).get(&pane) {
            Some(link) => {
                link.restart(RestartAgent {
                    agent_id,
                    cols: cols.into(),
                    rows: rows.into(),
                    attach: true,
                });
                Ok(())
            }
            None => Err(format!("no pane {}", pane.0)),
        }
    }

    /// Renames an agent (its `name` label, ADR 0052); the sidebar follows
    /// with the list sent right after it.
    pub fn rename(&self, agent_id: u64, name: String) {
        self.control(ControlCmd::SetLabels {
            agent_id,
            set: HashMap::from([(NAME_LABEL.to_owned(), name)]),
            remove: Vec::new(),
            verb: "rename",
        });
    }

    /// Sets, then removes, an agent's labels (Edit Labels, ADR 0087); the
    /// sidebar follows with the list sent right after it.
    pub fn set_labels(&self, agent_id: u64, set: HashMap<String, String>, remove: Vec<String>) {
        self.control(ControlCmd::SetLabels {
            agent_id,
            set,
            remove,
            verb: "edit labels",
        });
    }

    /// Answers `agent_id`'s open question `question` as the user (ADR
    /// 0078). fleetd then closes it and tells every view with an
    /// `AgentReported` event; a refusal (the question was closed first) is
    /// shown as an error.
    pub fn answer(&self, agent_id: u64, question: u64, text: String) {
        self.control(ControlCmd::Answer {
            agent_id,
            question,
            text,
        });
    }

    /// Types `data` into an agent on the control connection, as `fleet
    /// send` does (Send to Agents…, ADR 0083); a refusal is shown as an
    /// error.
    pub fn send_input(&self, agent_id: u64, data: Vec<u8>) {
        self.control(ControlCmd::Input(agent_id, data));
    }

    /// Forgets these exited agents (running ones are refused by fleetd).
    pub fn forget(&self, ids: Vec<u64>) {
        self.control(ControlCmd::Forget(ids));
    }

    /// A job that reads `agent_id`'s whole output (its fleetd history,
    /// then its screen) as plain text, over a connection of its own, as
    /// `fleet history --plain` does (ADR 0086). It blocks: run it off the
    /// UI thread. Running and exited agents alike.
    pub fn read_output(
        &self,
        agent_id: u64,
    ) -> impl FnOnce() -> Result<String, String> + Send + use<> {
        let ctx = Arc::clone(&self.inner.ctx);
        move || {
            let mut c = connect(&ctx.target, &ctx.state_dir, OUTPUT_CLIENT_NAME)
                .map_err(|e| e.to_string())?;
            let read = fleet_client::history::read(&mut c.conn, agent_id, None);
            drop(c);
            read.map(|t| t.plain_text()).map_err(|e| e.to_string())
        }
    }

    /// A job that searches every agent's output for `query` (ADR 0098)
    /// over a connection of its own, as `fleet search` does. It blocks:
    /// run it off the UI thread.
    pub fn search_output(
        &self,
        query: String,
        case_sensitive: bool,
    ) -> impl FnOnce() -> Result<SearchHits, String> + Send + use<> {
        let ctx = Arc::clone(&self.inner.ctx);
        move || {
            let mut c = connect(&ctx.target, &ctx.state_dir, SEARCH_CLIENT_NAME)
                .map_err(|e| e.to_string())?;
            let req = SearchOutput {
                text: query,
                case_sensitive,
                limit: search_all::LIMIT,
                per_agent: search_all::PER_AGENT,
            };
            let hits = fleet_client::search::search(&mut c.conn, req);
            drop(c);
            hits.map_err(|e| match e.to_string() {
                m if m.contains(UNKNOWN_REQUEST) => {
                    "this fleetd is too old to search; restart it to update".to_owned()
                }
                m => m,
            })
        }
    }

    /// Restarts fleetd into the binary on disk, agents kept: the local
    /// `fleetd` next to this app (or on `PATH`) for a local target; for ssh
    /// none is named, so the remote daemon restarts onto its own binary's
    /// path, as now on disk. The outcome arrives as
    /// [`UiEvent::DaemonRestarted`] or [`UiEvent::DaemonRestartFailed`],
    /// decided on the next handshake from the instance seen now; the control
    /// connection reconnects by itself. A restart already in flight is not
    /// asked for twice.
    pub fn restart_daemon(&self) {
        let binary = match self.inner.ctx.target {
            Target::Local => self
                .local_fleetd()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Target::Ssh { .. } => String::new(),
        };
        let token = {
            let mut d = lock(&self.inner.daemon);
            if d.restart.is_some() {
                return;
            }
            let Some(old_instance) = d.instance else {
                drop(d);
                self.inner
                    .ctx
                    .notify(UiEvent::Error("not connected to fleetd".into()));
                return;
            };
            d.restarts += 1;
            let token = d.restarts;
            d.restart = Some(PendingRestart {
                token,
                old_instance,
                agents: None,
                answered_at: None,
            });
            token
        };
        let asked_at = Instant::now();
        let wait = *lock(&self.inner.restart_wait);
        let answer_wait = *lock(&self.inner.restart_answer_wait);
        let (ctx, daemon, stop) = (
            Arc::clone(&self.inner.ctx),
            Arc::clone(&self.inner.daemon),
            Arc::clone(&self.inner.control_stop),
        );
        let _ = thread::Builder::new()
            .name("gui-restart-timer".into())
            .spawn(move || {
                // The deadline moves once the answer arrives; the timer
                // looks again when the one it slept towards has passed.
                loop {
                    let mut d = lock(&daemon);
                    let deadline = match d.restart.as_ref() {
                        Some(p) if p.token == token => match p.answered_at {
                            Some(at) => at + wait,
                            None => asked_at + answer_wait,
                        },
                        // Decided, or a later restart's.
                        _ => return,
                    };
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        d.restart = None;
                        drop(d);
                        ctx.notify(UiEvent::DaemonRestartFailed(did_not_come_back()));
                        return;
                    }
                    drop(d);
                    if stop.wait(left) {
                        return;
                    }
                }
            });
        self.control(ControlCmd::RestartDaemon(binary));
    }

    /// Whether the fleetd binary on disk differs from the one the connected
    /// daemon runs ([`update_available`]). Always `false` for ssh targets,
    /// and before the first handshake or for an older fleetd that does not
    /// report its binary. This stats the file: ask on connect and on window
    /// focus, not every frame.
    pub fn update_available(&self) -> bool {
        let Some(running) = lock(&self.inner.daemon).binary.clone() else {
            return false;
        };
        update_available(
            &self.inner.ctx.target,
            &running,
            self.local_fleetd().as_deref(),
        )
    }

    /// A [`Session::restart_daemon`] is in flight: asked for and not yet
    /// decided, refused or timed out. A further ask is ignored meanwhile.
    pub fn restart_pending(&self) -> bool {
        lock(&self.inner.daemon).restart.is_some()
    }

    /// The local `fleetd` a restart goes onto and the update check stats.
    fn local_fleetd(&self) -> Option<PathBuf> {
        lock(&self.inner.fleetd)
            .clone()
            .or_else(|| find_tool("fleetd"))
    }

    /// The graph is shown (`true`) or hidden: while shown, who wrote to whom
    /// is asked for every [`EDGES_POLL`] (ADR 0089).
    pub fn watch_message_edges(&self, on: bool) {
        if !on {
            lock(&self.inner.sidebar).edges.clear();
        }
        self.control(ControlCmd::MessageEdges(on));
    }

    /// The latest message edges, oldest first (empty unless watching).
    pub fn message_edges(&self) -> Vec<MessageEdge> {
        lock(&self.inner.sidebar).edges.clone()
    }

    /// Forgets every exited or lost agent on the target.
    pub fn clear_exited(&self) {
        self.control(ControlCmd::ClearExited);
    }

    fn control(&self, cmd: ControlCmd) {
        let _ = lock(&self.inner.control).send(cmd);
    }

    /// Opens `pane` with its own connection (`gui-pane-<n>`), attached to
    /// `agent_id` if given. A pane already open under that id is closed.
    /// After [`Session::shutdown`] the model is not connected (it stays
    /// `Disconnected`).
    pub fn open_pane(&self, pane: PaneId, agent_id: Option<u64>) -> Arc<Mutex<PaneModel>> {
        let palette = *lock(&self.inner.palette);
        let mut panes = lock(&self.inner.panes);
        // Checked under the panes lock, which `shutdown` drains after
        // setting the flag, so no pane outlives it.
        if self.inner.shut_down.load(Ordering::SeqCst) {
            let mut model = PaneModel::new(palette);
            model.on_disconnected();
            return Arc::new(Mutex::new(model));
        }
        let link = PaneLink::open(Arc::clone(&self.inner.ctx), pane, agent_id, palette);
        let model = link.model();
        if let Some(old) = panes.insert(pane, link) {
            old.close();
        }
        model
    }

    /// Shows `agent_id` in `pane` (detaching what it showed).
    pub fn attach(&self, pane: PaneId, agent_id: u64) {
        if let Some(link) = lock(&self.inner.panes).get(&pane) {
            link.attach(agent_id);
        }
    }

    /// Detaches and drops the pane's connection; never kills the agent.
    pub fn close_pane(&self, pane: PaneId) {
        if let Some(link) = lock(&self.inner.panes).remove(&pane) {
            link.close();
        }
    }

    /// Sends input to the pane's agent; never blocks.
    pub fn input(&self, pane: PaneId, bytes: Vec<u8>) {
        if let Some(link) = lock(&self.inner.panes).get(&pane) {
            link.input(bytes);
        }
    }

    /// Asks for the next history page if the pane's view now needs one
    /// (after it scrolled).
    pub fn check_paging(&self, pane: PaneId) {
        if let Some(link) = lock(&self.inner.panes).get(&pane) {
            link.check_paging();
        }
    }

    /// Resizes the pane's agent, debounced by 50 ms (the last size wins).
    pub fn resize(&self, pane: PaneId, cols: u16, rows: u16) {
        if let Some(link) = lock(&self.inner.panes).get(&pane) {
            link.resize(cols, rows);
        }
    }

    /// The colours that answer the panes' colour queries.
    pub fn set_palette(&self, palette: Palette) {
        *lock(&self.inner.palette) = palette;
        for link in lock(&self.inner.panes).values() {
            link.set_palette(palette);
        }
    }

    /// Detaches every pane and closes every connection, waiting at most
    /// [`SHUTDOWN_GRACE`] for the detaches to go out.
    pub fn shutdown(self) {
        let inner = &self.inner;
        if inner.shut_down.swap(true, Ordering::SeqCst) {
            return;
        }
        inner.control_stop.set();
        let _ = lock(&inner.control).send(ControlCmd::Stop);
        if let Some(h) = lock(&inner.control_conn).as_mut() {
            h.close();
        }
        let links: Vec<PaneLink> = lock(&inner.panes).drain().map(|(_, l)| l).collect();
        for link in &links {
            link.close();
        }
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        for link in &links {
            link.wait_closed(deadline);
        }
        for link in &links {
            link.force_close();
        }
    }

    /// Test hook: how long a restart waits for the new daemon.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn set_restart_wait_for_test(&self, d: Duration) {
        *lock(&self.inner.restart_wait) = d;
    }

    /// Test hook: the local `fleetd` on disk, in place of the one next to
    /// this app or on `PATH`.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn set_fleetd_for_test(&self, path: PathBuf) {
        *lock(&self.inner.fleetd) = Some(path);
    }

    /// Test hook: how long a restart waits for the old daemon's answer.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn set_restart_answer_wait_for_test(&self, d: Duration) {
        *lock(&self.inner.restart_answer_wait) = d;
    }

    /// Test hook: the pane's reader stops reading for `d` (to force a
    /// `Lagged`).
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn pause_reader_for_test(&self, pane: PaneId, d: Duration) {
        if let Some(link) = lock(&self.inner.panes).get(&pane) {
            link.pause_reader(d);
        }
    }

    /// Test hook: how many `Lagged` events the pane re-attached after.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn lag_count_for_test(&self, pane: PaneId) -> u32 {
        lock(&self.inner.panes)
            .get(&pane)
            .map_or(0, PaneLink::lag_count)
    }
}

/// Restarts and forgets sent on the control connection, by request id
/// (ascending), with the verb that names them in an error.
type ControlPending = Arc<Mutex<VecDeque<(u64, &'static str)>>>;

/// The verb of the tracked request answered by response `id` (responses
/// come in request order, so older entries are done).
fn take_verb(pending: &mut VecDeque<(u64, &'static str)>, id: u64) -> Option<&'static str> {
    while pending.front().is_some_and(|(p, _)| *p < id) {
        pending.pop_front();
    }
    match pending.front() {
        Some(&(p, verb)) if p == id => {
            pending.pop_front();
            Some(verb)
        }
        _ => None,
    }
}

/// The control connection's reader: connects, reads, reconnects.
struct ControlReader {
    /// Used instead of connecting, the first time.
    first: Option<Connected>,
    ctx: Arc<Ctx>,
    sidebar: Arc<Mutex<Sidebar>>,
    cmds: mpsc::Sender<ControlCmd>,
    stop: Arc<Stop>,
    conn: Arc<Mutex<Option<ConnHandle>>>,
    daemon: Arc<Mutex<DaemonState>>,
    pending: ControlPending,
}

impl ControlReader {
    fn run(mut self) {
        let mut backoff = Backoff::new();
        // One error per outage, not one per retry.
        let mut reported = false;
        while !self.stop.is_set() {
            self.ctx.notify(UiEvent::Connection(ConnState::Connecting));
            let attempt = match self.first.take() {
                Some(c) => Ok(c),
                None => connect(&self.ctx.target, &self.ctx.state_dir, CLIENT_NAME),
            };
            match attempt {
                Ok(c) => {
                    backoff.reset();
                    reported = false;
                    {
                        let mut slot = lock(&self.conn);
                        let mut h = ConnHandle::new(&c.conn, c.child.take());
                        if self.stop.is_set() {
                            h.close();
                            drop(slot);
                            h.finish();
                            return;
                        }
                        *slot = Some(h);
                    }
                    let decided = lock(&self.daemon).on_handshake(c.conn.hello());
                    let (mut reader, sender) = c.conn.split();
                    let _ = self.cmds.send(ControlCmd::Connected(sender));
                    if let Some(event) = decided {
                        self.ctx.notify(event);
                    }
                    self.ctx.notify(UiEvent::Connection(ConnState::Connected));
                    while let Ok(batch) = read_batch(&mut reader) {
                        self.apply(batch);
                    }
                    let _ = self.cmds.send(ControlCmd::Disconnected);
                    lock(&self.daemon).instance = None;
                    if let Some(h) = lock(&self.conn).take() {
                        h.finish();
                    }
                }
                Err(e) => {
                    if !reported {
                        reported = true;
                        self.ctx.notify(UiEvent::ConnectionError(e.to_string()));
                    }
                }
            }
            if self.stop.is_set() {
                return;
            }
            let retry_in = backoff.next_delay();
            self.ctx
                .notify(UiEvent::Connection(ConnState::Disconnected { retry_in }));
            if self.stop.wait(retry_in) {
                return;
            }
        }
    }

    fn apply(&self, batch: Vec<Frame>) {
        let mut changed = false;
        let mut relist = false;
        let mut edges_changed = false;
        {
            let mut sb = lock(&self.sidebar);
            for frame in batch {
                // Every response settles its tracked request, if any.
                let verb = match &frame {
                    Frame::Response(Response { id, .. }) => {
                        take_verb(&mut lock(&self.pending), *id)
                    }
                    _ => None,
                };
                match frame {
                    Frame::Response(Response {
                        kind: Some(response::Kind::Agents(list)),
                        ..
                    }) => {
                        sb.set_list(list.agents, list.replaced, Instant::now());
                        changed = true;
                    }
                    Frame::Response(Response {
                        kind: Some(response::Kind::PendingSpawns(p)),
                        ..
                    }) => {
                        sb.pending_spawns = p.pending.into_iter().map(|p| (p.id, p)).collect();
                        changed = true;
                    }
                    // A daemon from before spawn approval (ADR 0079) has
                    // none to list: nothing to say.
                    Frame::Response(Response {
                        kind: Some(response::Kind::Error(_)),
                        ..
                    }) if verb == Some(LIST_PENDING_SPAWNS) => {}
                    Frame::Response(Response {
                        kind: Some(response::Kind::MessageEdges(list)),
                        ..
                    }) => {
                        let edges: Vec<MessageEdge> = list
                            .edges
                            .iter()
                            .map(|e| MessageEdge {
                                id: e.id,
                                from: e.sender,
                                to: e.recipient,
                                at_ms: e.created_unix_ms,
                            })
                            .collect();
                        if sb.edges != edges {
                            sb.edges = edges;
                            edges_changed = true;
                        }
                    }
                    Frame::Response(Response {
                        kind: Some(response::Kind::RestartingDaemon(r)),
                        ..
                    }) => {
                        // The old daemon is about to exec; the next handshake
                        // decides the outcome against this count.
                        if let Some(p) = lock(&self.daemon).restart.as_mut() {
                            p.agents = Some(r.agents);
                            p.answered_at = Some(Instant::now());
                        }
                    }
                    Frame::Response(Response {
                        kind: Some(response::Kind::Error(e)),
                        ..
                    }) if verb == Some(RESTART_DAEMON) => {
                        // Refused before exec: the daemon is still serving.
                        lock(&self.daemon).restart = None;
                        let reason = if e.message == UNKNOWN_REQUEST {
                            TOO_OLD.to_owned()
                        } else {
                            e.message
                        };
                        self.ctx.notify(UiEvent::DaemonRestartFailed(reason));
                    }
                    Frame::Response(Response {
                        kind: Some(response::Kind::Error(e)),
                        ..
                    }) => {
                        let text = error_text(e.message);
                        let text = match verb {
                            Some(verb) if text != DAEMON_TOO_OLD => {
                                format!("{verb} failed: {text}")
                            }
                            _ => text,
                        };
                        self.ctx.notify(UiEvent::Error(text));
                    }
                    Frame::Response(Response {
                        kind: Some(response::Kind::Forgotten(f)),
                        ..
                    }) => {
                        let n = f.refused.len();
                        if n > 0 {
                            let agents = if n == 1 { "agent" } else { "agents" };
                            self.ctx.notify(UiEvent::Error(format!(
                                "{n} {agents} not removed (running or unknown)"
                            )));
                        }
                    }
                    Frame::Event(ev) => match ev.kind {
                        Some(event::Kind::Activity(a)) => {
                            let known = sb.infos.contains_key(&a.agent_id);
                            sb.activity.insert(
                                a.agent_id,
                                (a.last_line, a.last_output_unix_ms, Instant::now()),
                            );
                            changed |= known;
                            relist |= !known;
                        }
                        Some(event::Kind::Exited(e)) => match sb.infos.get_mut(&e.agent_id) {
                            Some(info) => {
                                info.set_state(AgentState::Exited);
                                info.exit_code = Some(e.exit_code);
                                // Until the next list brings the node's
                                // (an older node lists none).
                                info.exited_unix_ms.get_or_insert_with(unix_ms_now);
                                changed = true;
                            }
                            None => relist = true,
                        },
                        Some(event::Kind::Reported(r)) => match sb.infos.get_mut(&r.agent_id) {
                            Some(info) => {
                                info.reported = r.reported;
                                changed = true;
                            }
                            None => relist = true,
                        },
                        Some(event::Kind::SpawnPending(p)) => {
                            sb.pending_spawns.insert(p.id, *p);
                            changed = true;
                        }
                        Some(event::Kind::SpawnDecided(d)) => {
                            changed |= sb.pending_spawns.remove(&d.id).is_some();
                        }
                        Some(event::Kind::AgentRemoved(r)) => {
                            sb.infos.remove(&r.agent_id);
                            sb.activity.remove(&r.agent_id);
                            sb.follow_removed(r.agent_id, r.replaced_by);
                            changed = true;
                            self.ctx.notify(UiEvent::AgentRemoved {
                                agent_id: r.agent_id,
                                replaced_by: r.replaced_by,
                            });
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            if relist && !sb.list_requested {
                sb.list_requested = true;
            } else {
                relist = false;
            }
        }
        if relist {
            let _ = self.cmds.send(ControlCmd::List);
        }
        if changed {
            self.ctx.notify(UiEvent::AgentsChanged);
        }
        if edges_changed {
            self.ctx.notify(UiEvent::MessageEdgesChanged);
        }
    }
}

/// Owns the control connection's [`Sender`]: lists on connect, every
/// [`RELIST`] and on request, and sends kills, restarts and forgets.
fn control_writer(
    ctx: &Ctx,
    rx: &mpsc::Receiver<ControlCmd>,
    pending: &Mutex<VecDeque<(u64, &'static str)>>,
) {
    let mut sender: Option<Sender> = None;
    // A failed write drops the sender (the reader sees the end and
    // reconnects); with none, the user is told.
    let send = |sender: &mut Option<Sender>, req: request::Kind| match sender {
        Some(s) => {
            if s.send(req).is_err() {
                *sender = None;
            }
        }
        None => ctx.notify(UiEvent::Error("not connected to fleetd".into())),
    };
    // For polls: with no connection there is nobody to tell.
    let send_quiet = |sender: &mut Option<Sender>, req: request::Kind| {
        if let Some(s) = sender
            && s.send(req).is_err()
        {
            *sender = None;
        }
    };
    // A restart or forget is recorded before it is sent, so its reply finds
    // the verb.
    let send_tracked = |sender: &mut Option<Sender>, verb: &'static str, req: request::Kind| {
        if let Some(s) = sender {
            lock(pending).push_back((s.next_id(), verb));
        }
        send(sender, req);
    };
    let mut next_list = Instant::now() + RELIST;
    // Some(next poll) while the graph is shown.
    let mut next_edges: Option<Instant> = None;
    loop {
        let wake = next_edges.map_or(next_list, |e| e.min(next_list));
        let cmd = match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
            Ok(cmd) => Some(cmd),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let list = |sender: &mut Option<Sender>, next_list: &mut Instant| {
            *next_list = Instant::now() + RELIST;
            if let Some(s) = sender {
                let req = request::Kind::List(ListAgents::default());
                if s.send(req).is_err() {
                    *sender = None;
                }
            }
        };
        // Poll before anything else, so a busy control channel cannot starve
        // the lines.
        if let Some(at) = next_edges
            && Instant::now() >= at
        {
            next_edges = Some(Instant::now() + EDGES_POLL);
            let since = unix_ms_now() - message_lines::WINDOW_MS;
            let req = ListMessageEdges {
                since_unix_ms: since,
                max: 0,
            };
            send_quiet(&mut sender, request::Kind::ListMessageEdges(req));
        }
        match cmd {
            None if Instant::now() < next_list => {}
            None | Some(ControlCmd::List) => list(&mut sender, &mut next_list),
            Some(ControlCmd::MessageEdges(on)) => {
                next_edges = on.then(Instant::now);
            }
            Some(ControlCmd::Connected(s)) => {
                lock(pending).clear();
                sender = Some(s);
                list(&mut sender, &mut next_list);
                // Events keep the pending spawns current from here on.
                send_tracked(
                    &mut sender,
                    LIST_PENDING_SPAWNS,
                    request::Kind::ListPendingSpawns(ListPendingSpawns {}),
                );
            }
            Some(ControlCmd::Disconnected) => sender = None,
            Some(ControlCmd::Kill(agent_id)) => {
                send(&mut sender, request::Kind::Kill(KillAgent { agent_id }));
            }
            Some(ControlCmd::Restart(agent_id)) => {
                let restart = RestartAgent {
                    agent_id,
                    ..Default::default()
                };
                send_tracked(&mut sender, "restart", request::Kind::Restart(restart));
            }
            Some(ControlCmd::Forget(ids)) => {
                let forget = ForgetAgents {
                    ids,
                    ..Default::default()
                };
                send_tracked(
                    &mut sender,
                    "remove",
                    request::Kind::Forget(Box::new(forget)),
                );
            }
            Some(ControlCmd::SetLabels {
                agent_id,
                set,
                remove,
                verb,
            }) => {
                let set = SetLabels {
                    agent_id,
                    set,
                    remove,
                };
                send_tracked(&mut sender, verb, request::Kind::SetLabels(set));
                list(&mut sender, &mut next_list);
            }
            Some(ControlCmd::Answer {
                agent_id,
                question,
                text,
            }) => {
                let answer = fleet_proto::AnswerQuestion {
                    agent_id,
                    text,
                    question_id: Some(question),
                };
                send_tracked(&mut sender, "answer", request::Kind::Answer(answer));
            }
            Some(ControlCmd::Input(agent_id, data)) => {
                let input = SendInput { agent_id, data };
                send_tracked(&mut sender, "send", request::Kind::Input(input));
            }
            Some(ControlCmd::RestartDaemon(binary)) => {
                send_tracked(
                    &mut sender,
                    RESTART_DAEMON,
                    request::Kind::RestartDaemon(RestartDaemon { binary }),
                );
            }
            Some(ControlCmd::ClearExited) => {
                let forget = ForgetAgents {
                    exited_only: true,
                    ..Default::default()
                };
                send_tracked(
                    &mut sender,
                    "remove",
                    request::Kind::Forget(Box::new(forget)),
                );
            }
            Some(ControlCmd::DecideSpawn(id, approve)) => {
                let verb = if approve { "approve" } else { "deny" };
                let decide = DecideSpawn { id, approve };
                send_tracked(&mut sender, verb, request::Kind::DecideSpawn(decide));
            }
            Some(ControlCmd::Stop) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    fn hello_of(instance: u64, restore: Option<fleet_proto::RestoreSummary>) -> HelloOk {
        HelloOk {
            instance,
            restore: restore.map(Box::new),
            ..Default::default()
        }
    }

    fn pending(old_instance: u64, agents: Option<u32>) -> DaemonState {
        DaemonState {
            restart: Some(PendingRestart {
                token: 1,
                old_instance,
                agents,
                answered_at: agents.map(|_| Instant::now()),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn handshake_attribution_table() {
        use fleet_proto::RestoreSummary;
        let kept = |previous: u64, ids: &[u64]| RestoreSummary {
            previous_instance: previous,
            kept: ids.to_vec(),
            ..Default::default()
        };
        let thawed = RestoreSummary {
            restart_error: "exec failed".into(),
            ..Default::default()
        };
        // (state, handshake, event, restart still pending afterwards)
        type Case = (DaemonState, HelloOk, Option<UiEvent>, bool);
        let cases: Vec<Case> = vec![
            // A handshake before RestartingDaemon (agents unknown) decides
            // nothing, whatever it says.
            (
                pending(5, None),
                hello_of(6, Some(kept(5, &[1, 2]))),
                None,
                true,
            ),
            (
                pending(5, None),
                hello_of(5, Some(thawed.clone())),
                None,
                true,
            ),
            // No restart in flight: nothing to report.
            (
                DaemonState::default(),
                hello_of(6, Some(kept(5, &[1]))),
                None,
                false,
            ),
            // Decided.
            (
                pending(5, Some(2)),
                hello_of(6, Some(kept(5, &[1, 2]))),
                Some(UiEvent::DaemonRestarted(RestartOutcome::Restarted {
                    kept: 2,
                })),
                false,
            ),
            (
                pending(5, Some(2)),
                hello_of(6, None),
                Some(UiEvent::DaemonRestarted(RestartOutcome::Partial {
                    kept: 0,
                    ended: 2,
                    ids: None,
                })),
                false,
            ),
            (
                pending(5, Some(2)),
                hello_of(5, Some(thawed)),
                Some(UiEvent::DaemonRestartFailed("exec failed".into())),
                false,
            ),
            // The old daemon, not yet thawed: keep waiting.
            (pending(5, Some(2)), hello_of(5, None), None, true),
        ];
        for (mut state, hello, want, still_pending) in cases {
            assert_eq!(state.on_handshake(&hello), want, "{hello:?}");
            assert_eq!(state.restart.is_some(), still_pending, "{hello:?}");
            // Every handshake is remembered.
            assert_eq!(state.instance, Some(hello.instance));
        }
    }

    use super::*;
    use fleet_proto::{AgentList, AgentRemoved, Event};

    fn info(id: u64) -> AgentInfo {
        AgentInfo {
            id,
            ..Default::default()
        }
    }

    #[test]
    fn list_prunes_activity_of_unlisted_ids_unless_recent() {
        let now = Instant::now();
        let mut sb = Sidebar::default();
        sb.activity
            .insert(1, ("listed".into(), 1, now - RELIST * 4));
        sb.activity.insert(2, ("gone".into(), 1, now - RELIST * 4));
        sb.activity.insert(3, ("new".into(), 1, now));
        sb.list_requested = true;
        sb.set_list(vec![info(1)], HashMap::new(), now);
        let mut kept: Vec<u64> = sb.activity.keys().copied().collect();
        kept.sort_unstable();
        assert_eq!(kept, [1, 3]);
        assert!(!sb.list_requested);
    }

    #[test]
    fn every_list_is_counted() {
        let mut sb = Sidebar::default();
        assert_eq!(sb.lists, 0);
        sb.set_list(vec![info(1)], HashMap::new(), Instant::now());
        sb.set_list(vec![], HashMap::new(), Instant::now());
        assert_eq!(sb.lists, 2);
    }

    /// A control reader on its own sidebar, and the UI's end of its events.
    fn reader() -> (ControlReader, async_channel::Receiver<UiEvent>) {
        let (ctx, events) = Ctx::for_test_with_events();
        let r = ControlReader {
            first: None,
            ctx,
            sidebar: Arc::default(),
            cmds: mpsc::channel().0,
            stop: Arc::default(),
            conn: Arc::default(),
            daemon: Arc::default(),
            pending: Arc::default(),
        };
        (r, events)
    }

    fn drain(events: &async_channel::Receiver<UiEvent>) -> Vec<UiEvent> {
        std::iter::from_fn(|| events.try_recv().ok()).collect()
    }

    fn list_reply(agents: Vec<AgentInfo>, replaced: HashMap<u64, u64>) -> Frame {
        Frame::Response(Response {
            id: 1,
            kind: Some(response::Kind::Agents(AgentList { agents, replaced })),
        })
    }

    #[test]
    fn pending_spawns_follow_the_list_and_events() {
        use fleet_proto::{PendingSpawns, SpawnDecided};
        let (r, events) = reader();
        let p = |id| PendingSpawn {
            id,
            parent: 1,
            ..Default::default()
        };
        let ids = |r: &ControlReader| {
            lock(&r.sidebar)
                .pending_spawns
                .keys()
                .copied()
                .collect::<Vec<_>>()
        };
        r.apply(vec![Frame::Response(Response {
            id: 2,
            kind: Some(response::Kind::PendingSpawns(PendingSpawns {
                pending: vec![p(1), p(2)],
            })),
        })]);
        assert_eq!(ids(&r), [1, 2]);
        r.apply(vec![Frame::Event(Event {
            kind: Some(event::Kind::SpawnPending(Box::new(p(3)))),
        })]);
        drain(&events);
        r.apply(vec![Frame::Event(Event {
            kind: Some(event::Kind::SpawnDecided(SpawnDecided {
                id: 1,
                decision: 1,
            })),
        })]);
        assert_eq!(ids(&r), [2, 3]);
        assert_eq!(drain(&events), [UiEvent::AgentsChanged]);
        // An old daemon's refusal of the list is not shown.
        lock(&r.pending).push_back((7, LIST_PENDING_SPAWNS));
        r.apply(vec![Frame::Response(Response {
            id: 7,
            kind: Some(response::Kind::Error(fleet_proto::Error {
                message: UNKNOWN_REQUEST.into(),
            })),
        })]);
        assert!(drain(&events).is_empty());
    }

    #[test]
    fn removed_event_drops_row_and_notifies() {
        let (r, events) = reader();
        r.apply(vec![list_reply(vec![info(5), info(6)], HashMap::new())]);
        lock(&r.sidebar)
            .activity
            .insert(5, ("line".into(), 1, Instant::now()));
        drain(&events);
        r.apply(vec![Frame::Event(Event {
            kind: Some(event::Kind::AgentRemoved(AgentRemoved {
                agent_id: 5,
                replaced_by: Some(9),
            })),
        })]);
        let sb = lock(&r.sidebar);
        assert_eq!(sb.infos.keys().copied().collect::<Vec<_>>(), [6]);
        assert!(!sb.activity.contains_key(&5));
        drop(sb);
        let got = drain(&events);
        assert!(
            got.contains(&UiEvent::AgentRemoved {
                agent_id: 5,
                replaced_by: Some(9)
            }),
            "{got:?}"
        );
        assert!(got.contains(&UiEvent::AgentsChanged), "{got:?}");
    }

    fn removed(agent_id: u64, replaced_by: Option<u64>) -> Frame {
        Frame::Event(Event {
            kind: Some(event::Kind::AgentRemoved(AgentRemoved {
                agent_id,
                replaced_by,
            })),
        })
    }

    #[test]
    fn removed_event_collapses_the_replaced_chain() {
        let (r, _events) = reader();
        r.apply(vec![list_reply(
            vec![info(5), info(7)],
            HashMap::from([(1, 5), (2, 7)]),
        )]);
        // 5 restarted as 9: 1 -> 5 becomes 1 -> 9, and 5 -> 9 is added.
        r.apply(vec![removed(5, Some(9))]);
        assert_eq!(
            lock(&r.sidebar).replaced,
            HashMap::from([(1, 9), (5, 9), (2, 7)])
        );
        // 7 forgotten: nothing leads to it any more.
        r.apply(vec![removed(7, None)]);
        assert_eq!(lock(&r.sidebar).replaced, HashMap::from([(1, 9), (5, 9)]));
    }

    #[test]
    fn list_reply_keeps_replaced_map() {
        let mut sb = Sidebar::default();
        sb.set_list(vec![info(3)], HashMap::from([(1, 3)]), Instant::now());
        assert_eq!(sb.replaced, HashMap::from([(1, 3)]));
    }

    fn error_reply(id: u64, message: &str) -> Frame {
        Frame::Response(Response {
            id,
            kind: Some(response::Kind::Error(fleet_proto::Error {
                message: message.into(),
            })),
        })
    }

    #[test]
    fn restart_and_forget_errors_name_the_request() {
        let (r, events) = reader();
        lock(&r.pending).extend([(1, "restart"), (3, "remove"), (4, "restart")]);
        r.apply(vec![
            error_reply(1, "agent 5 is running"),
            error_reply(2, "unknown agent 6"),
            error_reply(3, "injected"),
            error_reply(4, "empty request"),
        ]);
        assert_eq!(
            drain(&events),
            [
                UiEvent::Error("restart failed: agent 5 is running".into()),
                UiEvent::Error("unknown agent 6".into()),
                UiEvent::Error("remove failed: injected".into()),
                UiEvent::Error("fleetd is too old to restart or remove agents".into()),
            ]
        );
        assert!(lock(&r.pending).is_empty());
    }

    #[test]
    fn every_response_settles_its_pending_verb() {
        let (r, events) = reader();
        lock(&r.pending).extend([(1, "restart"), (2, "remove")]);
        r.apply(vec![Frame::Response(Response {
            id: 1,
            kind: Some(response::Kind::Restarted(Box::new(
                fleet_proto::Restarted {
                    old_id: 5,
                    agent: Some(info(6)),
                    attached: None,
                },
            ))),
        })]);
        assert_eq!(
            lock(&r.pending).iter().copied().collect::<Vec<_>>(),
            [(2, "remove")]
        );
        r.apply(vec![Frame::Response(Response {
            id: 2,
            kind: Some(response::Kind::Ack(fleet_proto::Ack {})),
        })]);
        assert!(lock(&r.pending).is_empty());
        assert!(
            !drain(&events)
                .iter()
                .any(|e| matches!(e, UiEvent::Error(_)))
        );
    }

    #[test]
    fn forgotten_with_refused_ids_is_an_error() {
        let (r, events) = reader();
        let forgotten = |ids: Vec<u64>, refused: Vec<u64>| {
            Frame::Response(Response {
                id: 1,
                kind: Some(response::Kind::Forgotten(fleet_proto::Forgotten {
                    ids,
                    refused,
                })),
            })
        };
        r.apply(vec![forgotten(vec![1], vec![])]);
        assert_eq!(drain(&events), []);
        r.apply(vec![forgotten(vec![1], vec![2, 3])]);
        assert_eq!(
            drain(&events),
            [UiEvent::Error(
                "2 agents not removed (running or unknown)".into()
            )]
        );
    }

    #[test]
    fn empty_request_error_says_daemon_too_old() {
        let (r, events) = reader();
        r.apply(vec![Frame::Response(Response {
            id: 1,
            kind: Some(response::Kind::Error(fleet_proto::Error {
                message: "empty request".into(),
            })),
        })]);
        assert_eq!(
            drain(&events),
            [UiEvent::Error(
                "fleetd is too old to restart or remove agents".into()
            )]
        );
    }

    #[test]
    fn panes_and_spawns_are_refused_after_shutdown() {
        let dir = std::env::temp_dir().join(format!("fleet-gui-no-daemon-{}", std::process::id()));
        let (tx, _rx) = async_channel::unbounded();
        let s = Session::start(Target::Local, dir, tx);
        let handle = s.clone();
        s.shutdown();
        let model = handle.open_pane(PaneId(1), Some(1));
        assert_eq!(
            lock(&model).state(),
            super::super::pane::PaneState::Disconnected
        );
        assert!(lock(&handle.inner.panes).is_empty());
        let req = SpawnRequest {
            program: "/bin/true".into(),
            cwd: "/".into(),
            ..Default::default()
        };
        let r = handle.spawn(req, 80, 24, PaneId(1));
        assert!(r.is_err());
    }
}
