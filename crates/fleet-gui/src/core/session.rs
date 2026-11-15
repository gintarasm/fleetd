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
use std::time::{Duration, Instant};

use fleet_client::{Closer, Connection, FrameReader, Sender};
use fleet_proto::{
    AgentInfo, AgentState, ForgetAgents, Frame, KillAgent, ListAgents, Response, RestartAgent,
    SpawnAgent, event, request, response,
};
use fleet_term::Palette;

use super::backoff::Backoff;
use super::pane::PaneModel;
use super::pane_link::PaneLink;
use super::target::{Connected, Target, connect};

/// How the control connection introduces itself.
pub const CLIENT_NAME: &str = "gui";

/// What an old daemon answers a request it does not know with.
const UNKNOWN_REQUEST: &str = "empty request";
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

/// Sidebar re-list interval.
const RELIST: Duration = Duration::from_secs(5);
/// How long [`Session::shutdown`] waits for panes to detach before closing
/// their connections outright (gpui's quit budget is 100 ms).
const SHUTDOWN_GRACE: Duration = Duration::from_millis(60);

/// A wake-up for the UI, which coalesces them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiEvent {
    /// The sidebar rows changed; read [`Session::agents`].
    AgentsChanged,
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
    Kill(u64),
    Restart(u64),
    Forget(Vec<u64>),
    ClearExited,
    Stop,
}

struct Inner {
    ctx: Arc<Ctx>,
    sidebar: Arc<Mutex<Sidebar>>,
    control: Mutex<mpsc::Sender<ControlCmd>>,
    control_stop: Arc<Stop>,
    control_conn: Arc<Mutex<Option<ConnHandle>>>,
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
        program: String,
        args: Vec<String>,
        cwd: String,
        cols: u16,
        rows: u16,
        pane: PaneId,
    ) -> Result<(), String> {
        let spawn = SpawnAgent {
            program,
            args,
            cwd,
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

    /// Forgets these exited agents (running ones are refused by fleetd).
    pub fn forget(&self, ids: Vec<u64>) {
        self.control(ControlCmd::Forget(ids));
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
                    let (mut reader, sender) = c.conn.split();
                    let _ = self.cmds.send(ControlCmd::Connected(sender));
                    self.ctx.notify(UiEvent::Connection(ConnState::Connected));
                    while let Ok(batch) = read_batch(&mut reader) {
                        self.apply(batch);
                    }
                    let _ = self.cmds.send(ControlCmd::Disconnected);
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
        {
            let mut sb = lock(&self.sidebar);
            for frame in batch {
                match frame {
                    Frame::Response(Response {
                        kind: Some(response::Kind::Agents(list)),
                        ..
                    }) => {
                        sb.set_list(list.agents, list.replaced, Instant::now());
                        changed = true;
                    }
                    Frame::Response(Response {
                        id,
                        kind: Some(response::Kind::Error(e)),
                    }) => {
                        let text = error_text(e.message);
                        let text = match take_verb(&mut lock(&self.pending), id) {
                            Some(verb) if text != DAEMON_TOO_OLD => {
                                format!("{verb} failed: {text}")
                            }
                            _ => text,
                        };
                        self.ctx.notify(UiEvent::Error(text));
                    }
                    Frame::Response(Response {
                        id,
                        kind: Some(response::Kind::Forgotten(f)),
                    }) => {
                        take_verb(&mut lock(&self.pending), id);
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
                                changed = true;
                            }
                            None => relist = true,
                        },
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
    // A restart or forget is recorded before it is sent, so its reply finds
    // the verb.
    let send_tracked = |sender: &mut Option<Sender>, verb: &'static str, req: request::Kind| {
        if let Some(s) = sender {
            lock(pending).push_back((s.next_id(), verb));
        }
        send(sender, req);
    };
    let mut next_list = Instant::now() + RELIST;
    loop {
        let cmd = match rx.recv_timeout(next_list.saturating_duration_since(Instant::now())) {
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
        match cmd {
            None | Some(ControlCmd::List) => list(&mut sender, &mut next_list),
            Some(ControlCmd::Connected(s)) => {
                lock(pending).clear();
                sender = Some(s);
                list(&mut sender, &mut next_list);
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
                send_tracked(&mut sender, "remove", request::Kind::Forget(forget));
            }
            Some(ControlCmd::ClearExited) => {
                let forget = ForgetAgents {
                    exited_only: true,
                    ..Default::default()
                };
                send_tracked(&mut sender, "remove", request::Kind::Forget(forget));
            }
            Some(ControlCmd::Stop) => return,
        }
    }
}

#[cfg(test)]
mod tests {
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
        let r = handle.spawn(
            "/bin/true".into(),
            Vec::new(),
            "/".into(),
            80,
            24,
            PaneId(1),
        );
        assert!(r.is_err());
    }
}
