//! The session layer: one control connection (`gui`) for the sidebar,
//! spawn and kill, and one connection per pane ([`PaneLink`]). It turns
//! protocol traffic into calls on [`PaneModel`] and the sidebar rows, and
//! into [`UiEvent`] wake-ups for the UI.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use fleet_client::{Closer, Connection, FrameReader, Sender};
use fleet_proto::{
    AgentInfo, AgentState, Frame, KillAgent, ListAgents, Response, SpawnAgent, event, request,
    response,
};
use fleet_term::Palette;

use super::backoff::Backoff;
use super::pane::PaneModel;
use super::pane_link::PaneLink;
use super::target::{Connected, Target, connect};

/// How the control connection introduces itself.
pub const CLIENT_NAME: &str = "gui";

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
    /// The pane's model changed (sent at most once per batch of frames).
    PaneChanged(PaneId),
    /// The control connection's state.
    Connection(ConnState),
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
    /// A `ListAgents` was asked for because of an unknown id.
    list_requested: bool,
    /// A `ListAgents` reply arrived.
    listed: bool,
}

impl Sidebar {
    /// Applies a `ListAgents` reply: the rows are replaced and activity
    /// for ids not listed is dropped unless it is recent.
    fn set_list(&mut self, agents: Vec<AgentInfo>, now: Instant) {
        self.infos = agents.into_iter().map(|a| (a.id, a)).collect();
        let infos = &self.infos;
        self.activity.retain(|id, a| {
            infos.contains_key(id) || now.duration_since(a.2) < UNLISTED_ACTIVITY_TTL
        });
        self.list_requested = false;
        self.listed = true;
    }
}

enum ControlCmd {
    Connected(Sender),
    Disconnected,
    List,
    Kill(u64),
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
        thread::Builder::new()
            .name("gui-control-writer".into())
            .spawn(move || control_writer(&writer_ctx, &rx))
            .expect("spawn control writer");
        let reader = ControlReader {
            first,
            ctx,
            sidebar: Arc::clone(&inner.sidebar),
            cmds: tx,
            stop: Arc::clone(&inner.control_stop),
            conn: Arc::clone(&inner.control_conn),
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

    pub fn kill(&self, agent_id: u64) {
        let _ = lock(&self.inner.control).send(ControlCmd::Kill(agent_id));
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

/// The control connection's reader: connects, reads, reconnects.
struct ControlReader {
    /// Used instead of connecting, the first time.
    first: Option<Connected>,
    ctx: Arc<Ctx>,
    sidebar: Arc<Mutex<Sidebar>>,
    cmds: mpsc::Sender<ControlCmd>,
    stop: Arc<Stop>,
    conn: Arc<Mutex<Option<ConnHandle>>>,
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
                        let mut h = ConnHandle::new(&c.conn, c.child);
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
                        self.ctx.notify(UiEvent::Error(e.to_string()));
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
                        sb.set_list(list.agents, Instant::now());
                        changed = true;
                    }
                    Frame::Response(Response {
                        kind: Some(response::Kind::Error(e)),
                        ..
                    }) => self.ctx.notify(UiEvent::Error(e.message)),
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
/// [`RELIST`] and on request, and sends kills.
fn control_writer(ctx: &Ctx, rx: &mpsc::Receiver<ControlCmd>) {
    let mut sender: Option<Sender> = None;
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
                sender = Some(s);
                list(&mut sender, &mut next_list);
            }
            Some(ControlCmd::Disconnected) => sender = None,
            Some(ControlCmd::Kill(agent_id)) => match &mut sender {
                Some(s) => {
                    if s.send(request::Kind::Kill(KillAgent { agent_id })).is_err() {
                        sender = None;
                    }
                }
                None => ctx.notify(UiEvent::Error("not connected to fleetd".into())),
            },
            Some(ControlCmd::Stop) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        sb.set_list(vec![info(1)], now);
        let mut kept: Vec<u64> = sb.activity.keys().copied().collect();
        kept.sort_unstable();
        assert_eq!(kept, [1, 3]);
        assert!(!sb.list_requested);
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
