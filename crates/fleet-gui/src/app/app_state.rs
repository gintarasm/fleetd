//! What outlives the window (the app is resident): the session, the
//! layout, the panes and the saved state. [`AppState`] is a gpui global
//! entity; the [`Workspace`](super::workspace::Workspace) window draws it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use fleet_client::names::{live_names, name_of};
use fleet_proto::PendingSpawn;
use fleet_term::Palette;
use gpui::{App, AppContext, Context, Entity, Global, Task, WindowHandle};

use super::new_agent::dir_choices;
use super::pane_view::{InputSink, PaneChrome, PaneView};
use super::sidebar_view::{TabMark, program_name};
use super::theme::Theme;
use super::workspace::Workspace;
use crate::core::broadcast::{RETURN, RETURN_DELAY_MS, SendPlan, encode_text};
use crate::core::config::{GuiConfig, load_config};
use crate::core::drag_drop::DropAction;
use crate::core::drop_paste::{DROP_NEEDS_LOCAL, dropped_text};
use crate::core::ended::run_time;
use crate::core::feedback::{self, ExitNote, ExitToasts};
use crate::core::git;
use crate::core::layout::{Axis, Dir, Layout, SplitId};
use crate::core::new_children::NewChildren;
use crate::core::pane::{PaneModel, PaneState};
use crate::core::peek::PEEK_PANE;
use crate::core::presets::{Preset, detect_presets, usable};
use crate::core::question::open_question;
use crate::core::row_preview::only_activity_changed;
use crate::core::session::{AgentRow, ConnState, PaneId, Session, UiEvent, restart_failure_text};
use crate::core::settings::{Change, save_changes};
use crate::core::shortcuts::ClosedPanes;
use crate::core::sidebar::{GroupBy, ended, tilde, toggle_collapsed};
use crate::core::sidebar_tree::{removable_ended, toggle_disclosure};
use crate::core::spawn::{SpawnRequest, new_terminal, terminal_dir};
use crate::core::state::{
    GuiState, MAX_RECENT_COMMANDS, MAX_RECENT_DIRS, MAX_RECENT_TARGETS, push_recent,
};
use crate::core::status::{
    AgentStatus, Alerts, DotFill, alert_text, dot_fill, live_report, statuses,
};
use crate::core::style::chrome;
use crate::core::tabs::Tabs;
use crate::core::target::Target;
use crate::core::titlebar::{UpdateButton, update_button};
use crate::core::zoom::{FontZoom, Zoom};

/// Saved state is written once it has been still this long.
pub const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// A blocking job reading an agent's output as plain text (see
/// [`SessionApi::read_output`]).
pub type OutputJob = Box<dyn FnOnce() -> Result<String, String> + Send>;

/// A blocking job searching every agent's output (see
/// [`SessionApi::search_output`]).
pub type SearchJob = Box<dyn FnOnce() -> Result<fleet_proto::SearchHits, String> + Send>;

/// What the workspace needs from the session: [`Session`], or a recording
/// fake in tests.
pub trait SessionApi: InputSink {
    fn agents(&self) -> Vec<AgentRow>;
    /// The agents were listed ([`SessionApi::agents`] holds them).
    fn listed(&self) -> bool {
        false
    }
    /// How many `ListAgents` replies arrived.
    fn lists(&self) -> u64 {
        0
    }
    fn spawn(&self, req: SpawnRequest, cols: u16, rows: u16, pane: PaneId) -> Result<(), String>;
    fn kill(&self, agent_id: u64);
    /// Restarts an exited agent, unattached (panes follow `AgentRemoved`).
    fn restart(&self, agent_id: u64);
    /// Restarts an exited agent at `cols`x`rows`, attached in `pane`.
    fn restart_in_pane(
        &self,
        pane: PaneId,
        agent_id: u64,
        cols: u16,
        rows: u16,
    ) -> Result<(), String>;
    fn forget(&self, ids: Vec<u64>);
    /// A job reading the agent's whole output (fleetd's history, then the
    /// screen) as plain text; it blocks, so it runs off the UI thread.
    fn read_output(&self, _agent_id: u64) -> OutputJob {
        Box::new(|| Err("not connected to a fleetd".into()))
    }
    /// A job searching every agent's output for `query` (ADR 0098); it
    /// blocks, so it runs off the UI thread.
    fn search_output(&self, _query: String, _case_sensitive: bool) -> SearchJob {
        Box::new(|| Err("not connected to a fleetd".into()))
    }
    /// Sets an agent's name (ADR 0052).
    fn rename(&self, agent_id: u64, name: String);
    /// Sets, then removes, an agent's labels (Edit Labels, ADR 0087).
    fn set_labels(&self, _agent_id: u64, _set: HashMap<String, String>, _remove: Vec<String>) {}
    /// Types `data` into an agent, shown in a pane or not (Send to
    /// Agents…, ADR 0083).
    fn send_input(&self, _agent_id: u64, _data: Vec<u8>) {}
    /// Answers an agent's open question (ADR 0078).
    fn answer(&self, _agent_id: u64, _question: u64, _text: String) {}
    /// Forgets every exited or lost agent on the target.
    fn clear_exited(&self);
    /// The graph is shown or hidden: message lines are polled while shown
    /// (ADR 0089).
    fn watch_message_edges(&self, _on: bool) {}
    /// Who wrote to whom lately, oldest first.
    fn message_edges(&self) -> Vec<crate::core::message_lines::Edge> {
        Vec::new()
    }
    /// Restarts fleetd into its binary on disk, agents kept; the outcome
    /// arrives as a `UiEvent`.
    fn restart_daemon(&self) {}
    /// A [`SessionApi::restart_daemon`] is in flight (a further ask is
    /// ignored).
    fn restart_pending(&self) -> bool {
        false
    }
    /// The fleetd binary on disk differs from the running one (stats a
    /// file: not for every frame).
    fn update_available(&self) -> bool {
        false
    }
    /// The latest list's old id to newest successor.
    fn replaced(&self) -> HashMap<u64, u64>;
    /// Agents' spawns waiting for the user's approval (ADR 0079).
    fn pending_spawns(&self) -> Vec<PendingSpawn> {
        Vec::new()
    }
    /// Approves or denies a pending spawn.
    fn decide_spawn(&self, _id: u64, _approve: bool) {}
    fn open_pane(&self, pane: PaneId, agent_id: Option<u64>) -> Arc<Mutex<PaneModel>>;
    /// The peek's read-only connection to `agent_id` (ADR 0103), under
    /// [`PEEK_PANE`]; closed with `close_pane`.
    fn open_peek(&self, agent_id: u64) -> Arc<Mutex<PaneModel>> {
        self.open_pane(PEEK_PANE, Some(agent_id))
    }
    fn attach(&self, pane: PaneId, agent_id: u64);
    fn close_pane(&self, pane: PaneId);
    fn set_palette(&self, palette: Palette);
    /// Detaches every pane and closes every connection.
    fn shutdown(&self) {}
}

impl SessionApi for Session {
    fn agents(&self) -> Vec<AgentRow> {
        Session::agents(self)
    }

    fn pending_spawns(&self) -> Vec<PendingSpawn> {
        Session::pending_spawns(self)
    }

    fn decide_spawn(&self, id: u64, approve: bool) {
        Session::decide_spawn(self, id, approve);
    }

    fn listed(&self) -> bool {
        Session::listed(self)
    }

    fn lists(&self) -> u64 {
        Session::lists(self)
    }

    fn spawn(&self, req: SpawnRequest, cols: u16, rows: u16, pane: PaneId) -> Result<(), String> {
        Session::spawn(self, req, cols, rows, pane)
    }

    fn kill(&self, agent_id: u64) {
        Session::kill(self, agent_id);
    }

    fn restart(&self, agent_id: u64) {
        Session::restart(self, agent_id);
    }

    fn restart_in_pane(
        &self,
        pane: PaneId,
        agent_id: u64,
        cols: u16,
        rows: u16,
    ) -> Result<(), String> {
        Session::restart_in_pane(self, pane, agent_id, cols, rows)
    }

    fn forget(&self, ids: Vec<u64>) {
        Session::forget(self, ids);
    }

    fn read_output(&self, agent_id: u64) -> OutputJob {
        Box::new(Session::read_output(self, agent_id))
    }

    fn search_output(&self, query: String, case_sensitive: bool) -> SearchJob {
        Box::new(Session::search_output(self, query, case_sensitive))
    }

    fn rename(&self, agent_id: u64, name: String) {
        Session::rename(self, agent_id, name);
    }

    fn set_labels(&self, agent_id: u64, set: HashMap<String, String>, remove: Vec<String>) {
        Session::set_labels(self, agent_id, set, remove);
    }

    fn answer(&self, agent_id: u64, question: u64, text: String) {
        Session::answer(self, agent_id, question, text);
    }

    fn send_input(&self, agent_id: u64, data: Vec<u8>) {
        Session::send_input(self, agent_id, data);
    }

    fn clear_exited(&self) {
        Session::clear_exited(self);
    }

    fn watch_message_edges(&self, on: bool) {
        Session::watch_message_edges(self, on);
    }

    fn message_edges(&self) -> Vec<crate::core::message_lines::Edge> {
        Session::message_edges(self)
    }

    fn restart_daemon(&self) {
        Session::restart_daemon(self);
    }

    fn restart_pending(&self) -> bool {
        Session::restart_pending(self)
    }

    fn update_available(&self) -> bool {
        Session::update_available(self)
    }

    fn replaced(&self) -> HashMap<u64, u64> {
        Session::replaced(self)
    }

    fn open_pane(&self, pane: PaneId, agent_id: Option<u64>) -> Arc<Mutex<PaneModel>> {
        Session::open_pane(self, pane, agent_id)
    }

    fn open_peek(&self, agent_id: u64) -> Arc<Mutex<PaneModel>> {
        Session::open_peek(self, agent_id)
    }

    fn attach(&self, pane: PaneId, agent_id: u64) {
        Session::attach(self, pane, agent_id);
    }

    fn close_pane(&self, pane: PaneId) {
        Session::close_pane(self, pane);
    }

    fn set_palette(&self, palette: Palette) {
        Session::set_palette(self, palette);
    }

    fn shutdown(&self) {
        Session::shutdown(self.clone());
    }
}

/// The session before any fleetd is connected: panes stay empty and
/// nothing reaches a daemon.
struct NoSession;

impl InputSink for NoSession {
    fn input(&self, _pane: PaneId, _bytes: Vec<u8>) {}
    fn resize(&self, _pane: PaneId, _cols: u16, _rows: u16) {}
}

impl SessionApi for NoSession {
    fn agents(&self) -> Vec<AgentRow> {
        Vec::new()
    }

    fn spawn(
        &self,
        _req: SpawnRequest,
        _cols: u16,
        _rows: u16,
        _pane: PaneId,
    ) -> Result<(), String> {
        Err("not connected to a fleetd".into())
    }

    fn kill(&self, _agent_id: u64) {}
    fn restart(&self, _agent_id: u64) {}

    fn restart_in_pane(
        &self,
        _pane: PaneId,
        _agent_id: u64,
        _cols: u16,
        _rows: u16,
    ) -> Result<(), String> {
        Err("not connected to a fleetd".into())
    }

    fn forget(&self, _ids: Vec<u64>) {}
    fn rename(&self, _agent_id: u64, _name: String) {}
    fn clear_exited(&self) {}

    fn replaced(&self) -> HashMap<u64, u64> {
        HashMap::new()
    }

    fn open_pane(&self, _pane: PaneId, _agent_id: Option<u64>) -> Arc<Mutex<PaneModel>> {
        Arc::new(Mutex::new(PaneModel::new(Theme::dark().palette)))
    }

    fn attach(&self, _pane: PaneId, _agent_id: u64) {}
    fn close_pane(&self, _pane: PaneId) {}
    fn set_palette(&self, _palette: Palette) {}
}

/// Applies the session's events from `ui`, in batches.
fn follow(ui: async_channel::Receiver<UiEvent>, cx: &mut Context<AppState>) -> Task<()> {
    cx.spawn(async move |this, cx| {
        while let Ok(first) = ui.recv().await {
            let batch: Vec<UiEvent> = std::iter::once(first)
                .chain(std::iter::from_fn(|| ui.try_recv().ok()))
                .collect();
            if this.update(cx, |s, cx| s.apply_events(batch, cx)).is_err() {
                break;
            }
        }
    })
}

fn lock(m: &Mutex<PaneModel>) -> MutexGuard<'_, PaneModel> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// The process-wide [`AppState`].
struct AppGlobal(Entity<AppState>);

impl Global for AppGlobal {}

/// Everything that outlives the window: closing it keeps the session and
/// every pane attached.
pub struct AppState {
    session: Arc<dyn SessionApi>,
    /// A fleetd is connected (false before the first connect).
    live: bool,
    /// Bumped by every [`AppState::replace_session`]: pane ids from before
    /// a switch name nothing.
    session_epoch: u64,
    pub(super) target: Target,
    state_dir: PathBuf,
    pub(super) home: PathBuf,
    pub(super) config: GuiConfig,
    /// ⌘+ / ⌘− / ⌘0 (window-wide, not saved).
    zoom: FontZoom,
    gui_state: GuiState,
    /// Which parents' children the user opened (true) or closed in the
    /// sidebar this session; the default is by child count (ADR 0074).
    disclosure: HashMap<u64, bool>,
    /// The workspace tabs, each a layout with its focused pane (ADR 0061).
    /// Every tab's panes are open; only the shown tab's are drawn.
    tabs: Tabs,
    panes: HashMap<PaneId, Entity<PaneView>>,
    /// The peek (ADR 0103), while open: its agent and its view, on a
    /// read-only connection outside the layout.
    peek: Option<(u64, Entity<PaneView>)>,
    /// The agents of panes closed lately, for ⌘⇧T (not saved).
    closed: ClosedPanes,
    /// The shown tab's pane filling the pane area, if any (not saved).
    pub(super) maximized: Option<PaneId>,
    /// OSC titles of the agents shown in panes.
    pub(super) titles: HashMap<u64, String>,
    pub(super) theme: Theme,
    pub(super) agents: Vec<AgentRow>,
    /// Who wrote to whom lately: the graph's message lines (ADR 0089).
    pub(super) message_edges: Vec<crate::core::message_lines::Edge>,
    /// Each non-shell agent's status (ADR 0042), as of the last
    /// [`AppState::refresh_status`].
    pub(super) statuses: HashMap<u64, AgentStatus>,
    /// Agents whose pane rang the bell where you were not looking; cleared
    /// once a pane showing it is focused in the active window.
    pub(super) bells: HashSet<u64>,
    /// Agents' spawns waiting for the user's approval (ADR 0079).
    pub(super) pending_spawns: Vec<PendingSpawn>,
    /// Children spawned while the GUI watched, highlighted briefly.
    pub(super) new_children: NewChildren,
    alerts: Alerts,
    /// The window is open and has focus: no notifications then.
    window_active: bool,
    /// The last error, shown until dismissed or replaced.
    pub(super) error: Option<String>,
    /// A neutral message (a successful restart), shown when there is no
    /// error; dismissed the same way.
    pub(super) notice: Option<String>,
    /// Agents that exited while the window watched (#44), each with a
    /// Restart; they go on their own after a while.
    pub(super) exit_toasts: ExitToasts,
    /// Removes the oldest exit toast when its time is up (no ticking).
    exit_expiry: Option<Task<()>>,
    /// The agent whose success is being celebrated, and since when.
    pub(super) celebrating: Option<(u64, Instant)>,
    /// `error` is a failure to connect, cleared once connected.
    error_is_connection: bool,
    /// A config error from launch, still shown after the first connect.
    config_error: Option<String>,
    /// Panes a spawn is on its way into (for the "starting…" text only).
    starting: HashSet<PaneId>,
    /// The directory of the spawn on its way into a pane; it becomes a
    /// recent directory once the pane shows the new agent.
    spawn_dirs: HashMap<PaneId, String>,
    /// The New agent sheet's command line of the spawn on its way into a
    /// pane; it becomes a recent command with the directory (ADR 0084).
    spawn_commands: HashMap<PaneId, String>,
    /// The presets found on `PATH` for this target, once asked for
    /// (`[gui] presets` unset; ADR 0084).
    detected_presets: Option<(Target, Vec<Preset>)>,
    /// Agents asked for in a pane (sidebar click) that it does not show
    /// yet; until then the pane's old agent is not adopted back.
    requested: HashMap<PaneId, u64>,
    /// Panes restarting the agent they show (by its old id) on their own
    /// connection: they show the new agent without attaching to it.
    restarting: HashMap<PaneId, u64>,
    /// Agents in the layout the latest list left out, with the number
    /// ([`SessionApi::lists`]) of the first list in a row that did.
    unlisted: HashMap<u64, u64>,
    /// [`SessionApi::lists`] when the layout last followed the list.
    last_list: u64,
    /// When the control connection retries, while it is down.
    reconnect_at: Option<Instant>,
    /// A newer fleetd binary is on disk (see [`AppState::refresh_update_available`]).
    pub(super) update_available: bool,
    /// The restart [`AppState::restart_daemon`] asked for is in flight.
    updating: bool,
    pub(super) window: Option<WindowHandle<Workspace>>,
    /// Saved state not yet written.
    dirty: bool,
    save_task: Option<Task<()>>,
    _events: Task<()>,
}

impl AppState {
    /// Restores the target's saved layout, opening (and attaching) its
    /// panes, and follows the session's events from `ui`.
    pub fn new(
        session: Arc<dyn SessionApi>,
        ui: async_channel::Receiver<UiEvent>,
        target: Target,
        state_dir: PathBuf,
        config: GuiConfig,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Self {
        let gui_state = GuiState::load(&state_dir);
        let tabs = gui_state.saved_tabs(&target);
        session.set_palette(theme.palette);
        cx.set_global(theme);
        let events = follow(ui, cx);
        let mut s = AppState {
            agents: session.agents(),
            message_edges: Vec::new(),
            session,
            live: true,
            session_epoch: 0,
            target,
            state_dir,
            home: home_dir(),
            config,
            gui_state,
            tabs,
            panes: HashMap::new(),
            peek: None,
            zoom: FontZoom::default(),
            disclosure: HashMap::new(),
            closed: ClosedPanes::default(),
            maximized: None,
            titles: HashMap::new(),
            theme,
            statuses: HashMap::new(),
            bells: HashSet::new(),
            pending_spawns: Vec::new(),
            new_children: NewChildren::default(),
            alerts: Alerts::default(),
            window_active: false,
            error: None,
            error_is_connection: false,
            config_error: None,
            notice: None,
            exit_toasts: ExitToasts::default(),
            exit_expiry: None,
            celebrating: None,
            starting: HashSet::new(),
            spawn_dirs: HashMap::new(),
            spawn_commands: HashMap::new(),
            detected_presets: None,
            requested: HashMap::new(),
            restarting: HashMap::new(),
            unlisted: HashMap::new(),
            last_list: 0,
            reconnect_at: None,
            update_available: false,
            updating: false,
            window: None,
            dirty: false,
            save_task: None,
            _events: events,
        };
        for pane in s.tabs.panes() {
            let agent = s.tabs.agent(pane);
            s.open(pane, agent, cx);
        }
        s.refresh_chrome(cx);
        s
    }

    /// No fleetd yet: one empty pane until [`AppState::replace_session`].
    pub fn without_session(
        target: Target,
        state_dir: PathBuf,
        config: GuiConfig,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Self {
        let (_tx, rx) = async_channel::unbounded();
        let mut s = AppState::new(
            Arc::new(NoSession),
            rx,
            target,
            state_dir,
            config,
            theme,
            cx,
        );
        s.live = false;
        s.panes.clear();
        s.tabs = Tabs::single();
        let pane = s.tabs.focused();
        s.open(pane, None, cx);
        s.refresh_chrome(cx);
        s
    }

    /// Switches to `session` (connected to `target`): the old session
    /// detaches everything, and the target's saved layout is restored,
    /// its panes attached. The target becomes the most recent one.
    pub fn replace_session(
        &mut self,
        session: Arc<dyn SessionApi>,
        ui: async_channel::Receiver<UiEvent>,
        target: Target,
        cx: &mut Context<Self>,
    ) {
        let old = std::mem::replace(&mut self.session, session);
        old.shutdown();
        self.live = true;
        self.session_epoch += 1;
        self.target = target;
        self.session.set_palette(self.theme.palette);
        self._events = follow(ui, cx);
        self.tabs = self.gui_state.saved_tabs(&self.target);
        self.panes.clear();
        // The old session closed the peek's connection with the rest.
        self.peek = None;
        self.maximized = None;
        self.closed.clear();
        self.disclosure.clear();
        self.titles.clear();
        // The old target's message lines and notice are not the new one's.
        self.message_edges.clear();
        self.notice = None;
        // The new target's waiting and ended agents are not news.
        self.statuses.clear();
        self.bells.clear();
        self.pending_spawns.clear();
        // The new target's children are not news either.
        self.new_children = NewChildren::default();
        self.alerts = Alerts::default();
        // A launch config error outlives the first connect only.
        self.error = self.config_error.take();
        self.error_is_connection = false;
        self.exit_toasts = ExitToasts::default();
        self.exit_expiry = None;
        self.celebrating = None;
        self.starting.clear();
        self.spawn_dirs.clear();
        self.spawn_commands.clear();
        self.requested.clear();
        self.restarting.clear();
        self.unlisted.clear();
        self.last_list = 0;
        self.reconnect_at = None;
        self.update_available = self.session.update_available();
        self.updating = false;
        self.agents = self.session.agents();
        for pane in self.tabs.panes() {
            let agent = self.tabs.agent(pane);
            self.open(pane, agent, cx);
        }
        let target = self.target.clone();
        push_recent(
            &mut self.gui_state.recent_targets,
            target,
            MAX_RECENT_TARGETS,
        );
        self.schedule_save(cx);
        self.refresh_chrome(cx);
        cx.notify();
    }

    /// Detaches everything (at quit).
    pub fn shutdown_session(&self) {
        self.session.shutdown();
    }

    /// A fleetd is connected.
    pub fn is_live(&self) -> bool {
        self.live
    }

    /// The control connection is down and being retried.
    pub fn is_reconnecting(&self) -> bool {
        self.reconnect_at.is_some()
    }

    /// Changes whenever the session (and so the layout) is replaced.
    pub fn session_epoch(&self) -> u64 {
        self.session_epoch
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    pub fn state_dir(&self) -> &std::path::Path {
        &self.state_dir
    }

    /// Makes `state` the process-wide app state.
    pub fn install(state: Entity<AppState>, cx: &mut App) {
        cx.set_global(AppGlobal(state));
    }

    pub fn global(cx: &App) -> Option<Entity<AppState>> {
        cx.try_global::<AppGlobal>().map(|g| g.0.clone())
    }

    /// The shown tab's layout.
    pub fn layout(&self) -> &Layout {
        self.tabs.layout()
    }

    pub fn tabs(&self) -> &Tabs {
        &self.tabs
    }

    /// The shown tab's focused pane.
    pub fn focused(&self) -> PaneId {
        self.tabs.focused()
    }

    pub fn pane_view(&self, pane: PaneId) -> Option<&Entity<PaneView>> {
        self.panes.get(&pane)
    }

    /// The peek's agent and view, while one is open (ADR 0103).
    pub fn peek(&self) -> Option<(u64, &Entity<PaneView>)> {
        self.peek.as_ref().map(|(id, v)| (*id, v))
    }

    /// Peeks at `agent_id`: one peek at a time, so one already open
    /// closes first. Nothing in the layout changes.
    pub(super) fn open_peek(&mut self, agent_id: u64, cx: &mut Context<Self>) -> Entity<PaneView> {
        self.close_peek(cx);
        let model = self.session.open_peek(agent_id);
        let sink: Arc<dyn InputSink> = self.session.clone();
        let config = self.pane_config();
        let view = cx.new(|cx| {
            let mut v = PaneView::new(PEEK_PANE, model, sink, &config, cx);
            v.headless = true;
            v
        });
        self.peek = Some((agent_id, view.clone()));
        cx.notify();
        view
    }

    /// Closes the peek's connection (never the agent).
    pub(super) fn close_peek(&mut self, cx: &mut Context<Self>) {
        if self.peek.take().is_some() {
            self.session.close_pane(PEEK_PANE);
            cx.notify();
        }
    }

    /// The panes' font size, zoom included (the peek shrinks from it).
    pub(super) fn pane_font_size(&self) -> f32 {
        self.pane_config().font_size
    }

    pub fn gui_state(&self) -> &GuiState {
        &self.gui_state
    }

    /// The sidebar groups this target shows collapsed.
    pub fn collapsed_groups(&self) -> BTreeSet<String> {
        self.gui_state
            .collapsed_groups
            .get(&self.target_key())
            .cloned()
            .unwrap_or_default()
    }

    /// Collapses or expands the sidebar group `title`, remembered per
    /// target.
    pub fn toggle_group(&mut self, title: &str, cx: &mut Context<Self>) {
        let key = self.target_key();
        let set = self
            .gui_state
            .collapsed_groups
            .entry(key.clone())
            .or_default();
        toggle_collapsed(set, title);
        if set.is_empty() {
            self.gui_state.collapsed_groups.remove(&key);
        }
        self.schedule_save(cx);
        cx.notify();
    }

    /// What the user opened or closed in the sidebar tree.
    pub fn disclosure(&self) -> &HashMap<u64, bool> {
        &self.disclosure
    }

    /// Opens or closes the children of agent `id` (which has `children`
    /// rows under it as drawn).
    pub fn toggle_disclosure(&mut self, id: u64, children: usize, cx: &mut Context<Self>) {
        toggle_disclosure(&mut self.disclosure, id, children);
        cx.notify();
    }

    /// What the sidebar groups by (ADR 0085).
    pub fn group_by(&self) -> GroupBy {
        self.gui_state.sidebar_group_by
    }

    /// Groups the sidebar `by` folder or repository, remembered for
    /// every target.
    pub fn set_group_by(&mut self, by: GroupBy, cx: &mut Context<Self>) {
        if self.gui_state.sidebar_group_by == by {
            return;
        }
        self.gui_state.sidebar_group_by = by;
        self.schedule_save(cx);
        cx.notify();
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Who wrote to whom lately on the current target (graph view).
    pub fn message_edges(&self) -> &[crate::core::message_lines::Edge] {
        &self.message_edges
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    fn target_key(&self) -> String {
        self.target.to_string()
    }

    fn open(&mut self, pane: PaneId, agent: Option<u64>, cx: &mut Context<Self>) {
        let model = self.session.open_pane(pane, agent);
        let sink: Arc<dyn InputSink> = self.session.clone();
        let config = self.pane_config();
        let view = cx.new(|cx| PaneView::new(pane, model, sink, &config, cx));
        self.panes.insert(pane, view);
    }

    /// Asks the session for message lines (the graph is shown) or stops.
    pub(super) fn watch_message_edges(&mut self, on: bool) {
        self.session.watch_message_edges(on);
        if !on {
            self.message_edges.clear();
        }
    }

    fn apply_events(&mut self, batch: Vec<UiEvent>, cx: &mut Context<Self>) {
        let mut status = false;
        let mut chrome = false;
        let mut layout = false;
        for event in batch {
            match event {
                UiEvent::AgentsChanged => {
                    let rows = self.session.agents();
                    // Output alone is drawn on the next one-second tick
                    // (issue #41): a printing agent's row redraws at most
                    // once a second.
                    let quiet = only_activity_changed(&self.agents, &rows);
                    let before = std::mem::replace(&mut self.agents, rows);
                    if self.live {
                        let exited = feedback::newly_exited(&before, &self.agents);
                        self.show_exits(exited, cx);
                    }
                    let agents = &self.agents;
                    self.bells
                        .retain(|id| agents.iter().any(|r| r.info.id == *id));
                    self.titles
                        .retain(|id, _| agents.iter().any(|r| r.info.id == *id));
                    if self.session.listed() {
                        self.new_children.observe(
                            &self.agents,
                            &self.session.replaced(),
                            Instant::now(),
                        );
                    }
                    let pending = self.session.pending_spawns();
                    if pending != self.pending_spawns {
                        self.pending_spawns = pending;
                        chrome = true;
                        cx.notify();
                    }
                    status = true;
                    if self.live {
                        super::bench::agents_listed();
                        if self.session.listed() {
                            layout |= self.follow_list(cx);
                        }
                    }
                    if !quiet {
                        chrome = true;
                        cx.notify();
                    }
                }
                UiEvent::MessageEdgesChanged => {
                    self.message_edges = self.session.message_edges();
                    cx.notify();
                }
                UiEvent::PaneChanged(PEEK_PANE) => {
                    if let Some((_, view)) = &self.peek {
                        view.update(cx, |_, cx| cx.notify());
                    }
                }
                UiEvent::PaneChanged(pane) => {
                    let Some(view) = self.panes.get(&pane).cloned() else {
                        continue;
                    };
                    layout |= self.adopt_shown_agent(pane, &view, cx);
                    let (agent, title, rang) = {
                        let mut model = lock(view.read(cx).model_handle());
                        (model.agent_id(), model.title(), model.take_bell())
                    };
                    if rang {
                        view.update(cx, |v, cx| v.flash(cx));
                        if let Some(id) = agent
                            && !(self.window_active && pane == self.tabs.focused())
                        {
                            status |= self.bells.insert(id);
                        }
                    }
                    if let Some(id) = agent {
                        let changed = match title {
                            Some(t) => self.titles.insert(id, t.clone()).as_ref() != Some(&t),
                            None => self.titles.remove(&id).is_some(),
                        };
                        if changed {
                            cx.notify();
                        }
                    }
                    view.update(cx, |_, cx| cx.notify());
                }
                UiEvent::AgentRemoved {
                    agent_id,
                    replaced_by,
                } => layout |= self.follow_removed(agent_id, replaced_by, cx),
                UiEvent::LaunchDropped(pane) => self.launch_dropped(pane, cx),
                UiEvent::Connection(state) => {
                    self.reconnect_at = match state {
                        ConnState::Disconnected { retry_in } => Some(Instant::now() + retry_in),
                        _ => None,
                    };
                    if state == ConnState::Connected {
                        self.refresh_update_available(cx);
                    }
                    if state == ConnState::Connected && self.error_is_connection {
                        self.error = None;
                        self.error_is_connection = false;
                        cx.notify();
                    }
                    chrome = true;
                }
                UiEvent::ConnectionError(e) => {
                    eprintln!("fleet-gui: {e}");
                    self.error = Some(e);
                    self.error_is_connection = true;
                    cx.notify();
                }
                UiEvent::DaemonRestarted(outcome) => {
                    self.updating = self.session.restart_pending();
                    // The new daemon may be older or newer than the disk again.
                    self.refresh_update_available(cx);
                    // Success is good news: a neutral toast. Ended agents
                    // make it a problem: the error toast.
                    if outcome.is_success() {
                        eprintln!("fleet-gui: {outcome}");
                        self.notice = Some(outcome.to_string());
                        cx.notify();
                    } else {
                        self.show_error(outcome.to_string(), cx);
                    }
                    chrome = true;
                }
                UiEvent::DaemonRestartFailed(reason) => {
                    self.updating = self.session.restart_pending();
                    self.show_error(restart_failure_text(&reason), cx);
                    chrome = true;
                }
                UiEvent::Error(e) => {
                    eprintln!("fleet-gui: {e}");
                    // A spawn on its way may be what failed: stop saying
                    // "starting…". Should it land after all, the pane still
                    // adopts its agent. A pane restart may have failed too:
                    // its pane then attaches to a successor it is told of.
                    self.starting.clear();
                    self.restarting.clear();
                    self.error = Some(e);
                    self.error_is_connection = false;
                    chrome = true;
                    cx.notify();
                }
            }
        }
        if status {
            self.refresh_status(cx);
        }
        if layout {
            self.layout_changed(cx);
        } else if chrome {
            self.refresh_chrome(cx);
        }
    }

    /// The layout follows the agent a pane shows when it changed under it
    /// (a spawn landed), except while an attach the user asked for is on
    /// its way. True if the layout changed.
    fn adopt_shown_agent(
        &mut self,
        pane: PaneId,
        view: &Entity<PaneView>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(shown) = lock(view.read(cx).model_handle()).agent_id() else {
            return false;
        };
        if let Some(&want) = self.requested.get(&pane) {
            if want != shown {
                return false;
            }
            self.requested.remove(&pane);
        }
        if self.tabs.agent(pane) == Some(shown) {
            return false;
        }
        self.tabs.set_agent(pane, Some(shown));
        self.starting.remove(&pane);
        self.restarting.remove(&pane);
        if let Some(dir) = self.spawn_dirs.remove(&pane) {
            let dirs = self
                .gui_state
                .recent_dirs
                .entry(self.target.to_string())
                .or_default();
            push_recent(dirs, dir, MAX_RECENT_DIRS);
        }
        if let Some(line) = self.spawn_commands.remove(&pane) {
            let commands = self
                .gui_state
                .recent_commands
                .entry(self.target.to_string())
                .or_default();
            push_recent(commands, line, MAX_RECENT_COMMANDS);
        }
        true
    }

    /// `old` was removed, restarted as `new` if set: every pane showing it
    /// shows `new` (attaching unless it started `new` itself) or becomes
    /// empty. True if the layout changed.
    fn follow_removed(&mut self, old: u64, new: Option<u64>, cx: &mut Context<Self>) -> bool {
        let panes = self.tabs.replace_agent(old, new);
        for &pane in &panes {
            let restarted_here = self.restarting.get(&pane) == Some(&old);
            self.forget_pending(pane);
            match new {
                Some(new) if self.pane_has(pane, new, cx) => {}
                Some(new) => {
                    if !restarted_here {
                        self.session.attach(pane, new);
                    }
                    // Until the pane shows `new`, `old` is not adopted back.
                    self.requested.insert(pane, new);
                }
                None => self.open(pane, None, cx),
            }
        }
        !panes.is_empty()
    }

    /// `pane`'s connection dropped before its spawn or restart was
    /// answered: it shows the agent the layout names (the successor, if a
    /// restart was announced meanwhile; else the old agent, until one is).
    fn launch_dropped(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if !self.panes.contains_key(&pane) {
            return;
        }
        self.forget_pending(pane);
        if let Some(agent) = self.tabs.agent(pane) {
            self.requested.insert(pane, agent);
            self.session.attach(pane, agent);
        }
        self.refresh_chrome(cx);
        cx.notify();
    }

    /// After a list: panes follow restarts they missed, and a pane whose
    /// agent the node neither lists nor replaced becomes empty, unless a
    /// spawn, restart or attach is on its way into it. A lost pane is
    /// emptied at once; an exited one (its final screen) only once two
    /// lists in a row left its agent out, since an agent spawned after the
    /// first was taken may have exited already. True if the layout changed.
    fn follow_list(&mut self, cx: &mut Context<Self>) -> bool {
        let replaced = self.session.replaced();
        let list = self.session.lists();
        let fresh = std::mem::replace(&mut self.last_list, list) != list;
        let mut unlisted = HashMap::new();
        let mut changed = false;
        for pane in self.tabs.panes() {
            let Some(id) = self.tabs.agent(pane) else {
                continue;
            };
            if let Some(&new) = replaced.get(&id) {
                changed |= self.follow_removed(id, Some(new), cx);
                continue;
            }
            if self.agents.iter().any(|r| r.info.id == id) {
                continue;
            }
            // `requested` names the layout's agent, so a request for `id`
            // itself does not count: its failed attach is what `Lost` says.
            let on_its_way = self.starting.contains(&pane)
                || self.restarting.contains_key(&pane)
                || self.requested.get(&pane).is_some_and(|&want| want != id);
            if on_its_way {
                continue;
            }
            let (state, shown) = match self.panes.get(&pane) {
                Some(v) => {
                    let m = lock(v.read(cx).model_handle());
                    (m.state(), m.agent_id())
                }
                None => continue,
            };
            let gone = match state {
                PaneState::Lost => true,
                PaneState::Exited(_) if shown == Some(id) => {
                    // Counted from a list, not from events applied to an
                    // older one (which predates agents spawned since).
                    let first = match self.unlisted.get(&id) {
                        Some(&n) => n,
                        None if fresh => list,
                        None => continue,
                    };
                    unlisted.insert(id, first);
                    first < list
                }
                _ => false,
            };
            if gone {
                self.tabs.set_agent(pane, None);
                self.forget_pending(pane);
                self.open(pane, None, cx);
                changed = true;
            }
        }
        self.unlisted = unlisted;
        changed
    }

    /// `pane`'s grid shows `agent_id` (in any state).
    fn pane_has(&self, pane: PaneId, agent_id: u64, cx: &App) -> bool {
        self.panes
            .get(&pane)
            .is_some_and(|v| lock(v.read(cx).model_handle()).agent_id() == Some(agent_id))
    }

    /// Takes the session's agent list if it arrived before its event was
    /// applied (the window's first frame then draws it).
    pub fn sync_agents(&mut self) {
        if self.live && self.session.listed() {
            self.agents = self.session.agents();
            super::bench::agents_listed();
        }
    }

    pub fn has_agents(&self) -> bool {
        !self.agents.is_empty()
    }

    /// Once a second: the reconnect countdown, and statuses that age
    /// (working turns quiet).
    pub fn tick(&mut self, cx: &mut Context<Self>) {
        if self.reconnect_at.is_some() {
            self.refresh_chrome(cx);
        }
        self.refresh_status(cx);
    }

    /// Recomputes the statuses (ADR 0042); on a change redraws, and sends
    /// the notifications it calls for while the window is not focused (if
    /// they are on).
    pub fn refresh_status(&mut self, cx: &mut Context<Self>) {
        let now = statuses(&self.agents, &self.bells, unix_ms());
        let alerts = self.alerts.update(&now);
        if now != self.statuses {
            self.statuses = now;
            self.refresh_chrome(cx);
            cx.notify();
        }
        if !self.config.notifications || (self.window_active && self.window.is_some()) {
            return;
        }
        for (id, status) in alerts {
            let Some(row) = self.agents.iter().find(|r| r.info.id == id) else {
                continue;
            };
            let name = self
                .titles
                .get(&id)
                .filter(|t| !t.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| program_name(&row.info.program));
            let cwd = tilde(&row.info.cwd, &self.home);
            let detail = match status {
                AgentStatus::NeedsYou if !row.last_line.trim().is_empty() => row.last_line.trim(),
                _ => cwd.as_str(),
            };
            let (title, mut body) = alert_text(&name, status, row.info.exit_code, detail);
            // What the agent said about itself beats its last line.
            if let Some(r) = live_report(&row.info) {
                body = if r.message.is_empty() {
                    cwd.clone()
                } else {
                    r.message.clone()
                };
            }
            super::attention::post(self.session_epoch, id, &title, &body, cx);
        }
    }

    /// Agents that need you, for the Dock badge and the menu bar.
    pub fn needs_you_count(&self) -> usize {
        crate::core::status::needs_you_count(&self.statuses)
    }

    /// The window gained or lost focus (or closed).
    pub fn set_window_active(&mut self, active: bool, cx: &mut Context<Self>) {
        self.window_active = active;
        self.see_focused(cx);
    }

    /// A bell in the focused pane is seen once the window is active.
    fn see_focused(&mut self, cx: &mut Context<Self>) {
        if !self.window_active {
            return;
        }
        if let Some(id) = self.tabs.agent(self.tabs.focused())
            && self.bells.remove(&id)
        {
            self.refresh_status(cx);
        }
    }

    /// Turns notifications on or off and saves it to `config.toml`.
    pub fn toggle_notifications(&mut self, cx: &mut Context<Self>) {
        let on = !self.config.notifications;
        self.config.notifications = on;
        if let Err(e) = crate::core::config::save_notifications(&self.state_dir, on) {
            self.error = Some(format!("cannot save notifications setting: {e}"));
        }
        if on {
            super::attention::request_permission(cx);
        }
        cx.notify();
    }

    /// Tells each pane its number, agent and connection facts.
    fn refresh_chrome(&mut self, cx: &mut Context<Self>) {
        let reconnect_in = self
            .reconnect_at
            .map(|at| at.saturating_duration_since(Instant::now()));
        let drawn = if self.maximized.is_some() {
            1
        } else {
            self.tabs.layout().panes().len()
        };
        let dims = feedback::dims(self.config.focus_mode, drawn);
        let no_agents = self.live && self.session.listed() && self.agents.is_empty();
        // Every tab's panes: a hidden one is right when its tab is shown.
        let shown = self.tabs.active();
        let every_tab: Vec<_> = (0..self.tabs.len())
            .flat_map(|t| {
                let layout = self.tabs.tab_layout(t).expect("tab in range");
                let focused = self.tabs.tab_focused(t);
                let panes = layout.panes().into_iter().enumerate();
                panes.map(move |(i, pane)| (t, i, pane, focused == Some(pane)))
            })
            .collect();
        for (t, i, pane, focused) in every_tab {
            let agent = self.tabs.agent(pane);
            let row = agent.and_then(|id| self.agents.iter().find(|r| r.info.id == id));
            let chrome = PaneChrome {
                number: i + 1,
                has_agent: agent.is_some(),
                agent_ended: row.is_some_and(ended),
                program: match (row, agent) {
                    (Some(r), _) => Some(program_name(&r.info.program)),
                    (None, Some(id)) => Some(format!("agent {id}")),
                    (None, None) => None,
                },
                cwd: row.map(|r| tilde(&r.info.cwd, &self.home)),
                git: row.and_then(|r| git::header_text(&r.info)),
                name: row.and_then(|r| name_of(&r.info)).map(str::to_owned),
                link_dir: row
                    .filter(|_| self.target == Target::Local)
                    .map(|r| r.info.cwd.clone()),
                starting: self.starting.contains(&pane),
                reconnect_in,
                focused,
                maximized: t == shown && self.maximized == Some(pane),
                dims,
                status: agent.and_then(|id| self.statuses.get(&id).copied()),
                status_fill: row
                    .zip(agent.and_then(|id| self.statuses.get(&id)))
                    .map_or(DotFill::Hollow, |(r, s)| dot_fill(&r.info, *s)),
                agent,
                agent_emoji: self.config.agent_emoji,
                local: self.target == Target::Local,
                no_agents,
                run_time: row.and_then(|r| run_time(r.info.created_unix_ms, r.info.exited_unix_ms)),
                question: row.and_then(|r| open_question(&r.info)),
            };
            if let Some(view) = self.panes.get(&pane) {
                view.update(cx, |v, cx| v.set_chrome(chrome, cx));
            }
        }
    }

    /// The layout changed: redraw, and save it after [`SAVE_DEBOUNCE`].
    fn layout_changed(&mut self, cx: &mut Context<Self>) {
        // Unconnected, the layout is a placeholder: the target's saved one stays.
        if self.live {
            self.gui_state.set_tabs(&self.target, &self.tabs);
            self.schedule_save(cx);
        }
        self.refresh_chrome(cx);
        self.see_focused(cx);
        cx.notify();
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.dirty = true;
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            let _ = this.update(cx, |s, _| s.save_now());
        }));
    }

    /// Writes the saved state if it changed (also at quit).
    pub fn save_now(&mut self) {
        if !std::mem::take(&mut self.dirty) {
            return;
        }
        if let Err(e) = self.gui_state.save(&self.state_dir) {
            eprintln!("fleet-gui: cannot save the GUI state: {e}");
        }
    }

    pub fn set_focused(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if self.panes.contains_key(&pane) && self.tabs.set_focused(pane) {
            // Focus moving elsewhere restores the split view.
            if self.maximized.is_some_and(|m| m != pane) {
                self.maximized = None;
            }
            self.refresh_chrome(cx);
            self.see_focused(cx);
            cx.notify();
        }
    }

    pub fn maximized(&self) -> Option<PaneId> {
        self.maximized
    }

    /// Maximizes the focused pane, or restores the split view.
    pub fn toggle_maximize(&mut self, cx: &mut Context<Self>) {
        self.maximized = match self.maximized {
            Some(_) => None,
            None if self.tabs.layout().panes().len() > 1 => Some(self.tabs.focused()),
            None => None,
        };
        cx.notify();
    }

    /// How each agent's tab relates to the panes.
    pub fn tab_marks(&self) -> HashMap<u64, TabMark> {
        let mut marks = HashMap::new();
        let layout = self.tabs.layout();
        for pane in layout.panes() {
            if let Some(id) = layout.agent(pane) {
                let mark = if pane == self.tabs.focused() {
                    TabMark::Focused
                } else {
                    TabMark::Shown
                };
                let slot = marks.entry(id).or_insert(mark);
                if mark == TabMark::Focused {
                    *slot = mark;
                }
            }
        }
        marks
    }

    /// Splits the focused pane; the new, empty pane gets the focus. `None`
    /// at [`MAX_PANES`].
    pub fn split(&mut self, axis: Axis, cx: &mut Context<Self>) -> Option<PaneId> {
        let new = self.tabs.split(axis)?;
        self.maximized = None;
        self.open(new, None, cx);
        self.layout_changed(cx);
        Some(new)
    }

    /// Moves `split`'s divider to `at` (pixels along its axis, in a
    /// `w` × `h` pane area), every pane keeping [`chrome::PANE_MIN`].
    pub fn move_divider(
        &mut self,
        split: SplitId,
        at: f32,
        w: f32,
        h: f32,
        cx: &mut Context<Self>,
    ) {
        if self
            .tabs
            .layout_mut()
            .move_divider(split, at, w, h, chrome::PANE_MIN)
        {
            self.layout_changed(cx);
        }
    }

    /// Evens `split` again (a double-click on its divider).
    pub fn reset_divider(&mut self, split: SplitId, cx: &mut Context<Self>) {
        if self.tabs.layout_mut().reset_divider(split) {
            self.layout_changed(cx);
        }
    }

    /// Closes the focused pane, detaching its agent (never killing it).
    /// The last pane stays, empty.
    pub fn close_focused(&mut self, cx: &mut Context<Self>) {
        let agent = self.tabs.agent(self.tabs.focused());
        let Some(pane) = self.tabs.close_focused() else {
            return;
        };
        let next = self.tabs.focused();
        if let Some(id) = agent {
            self.closed.push(id);
        }
        self.session.close_pane(pane);
        self.panes.remove(&pane);
        self.maximized = None;
        self.forget_pending(pane);
        if next == pane {
            // The last pane: a fresh, empty connection in its place.
            self.open(pane, None, cx);
        }
        self.layout_changed(cx);
    }

    /// ⌘⇧T: the agent of the pane closed last that is still listed, in
    /// the focused pane if it is empty, else in a new split. `None` when
    /// there is none or no pane fits (it then stays to reopen).
    pub fn reopen_closed(&mut self, cx: &mut Context<Self>) -> Option<PaneId> {
        if !self.session.listed() {
            return None;
        }
        let agents = &self.agents;
        let id = self
            .closed
            .pop(|id| agents.iter().any(|r| r.info.id == id))?;
        let pane = self.tabs.focused();
        if self.tabs.agent(pane).is_none() && !self.starting.contains(&pane) {
            self.show_agent(id, cx);
            return Some(pane);
        }
        let new = self.open_in_new_split(id, cx);
        if new.is_none() {
            self.closed.push(id);
        }
        new
    }

    /// A new tab right of the shown one, with one empty pane, shown and
    /// focused. `None` at [`MAX_TABS`](crate::core::tabs::MAX_TABS).
    pub fn new_tab(&mut self, cx: &mut Context<Self>) -> Option<PaneId> {
        let pane = self.tabs.new_tab()?;
        self.maximized = None;
        self.open(pane, None, cx);
        self.layout_changed(cx);
        Some(pane)
    }

    /// Closes tab `i`, detaching its panes' agents (never killing them);
    /// ⌘⇧T reopens them, last pane first. The last tab stays.
    pub fn close_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        let shown = i == self.tabs.active();
        let agents: Vec<u64> = match self.tabs.tab_layout(i) {
            Some(l) => l.panes().into_iter().filter_map(|p| l.agent(p)).collect(),
            None => return,
        };
        let Some(panes) = self.tabs.close_tab(i) else {
            return;
        };
        for id in agents {
            self.closed.push(id);
        }
        for pane in panes {
            self.panes.remove(&pane);
            self.session.close_pane(pane);
            self.forget_pending(pane);
        }
        if shown {
            self.maximized = None;
        }
        self.layout_changed(cx);
    }

    /// Shows tab `i` (0-based); `true` if that changed the shown tab.
    pub fn select_tab(&mut self, i: usize, cx: &mut Context<Self>) -> bool {
        let changed = self.tabs.select(i);
        self.tab_shown(changed, cx)
    }

    /// Shows the next tab, wrapping around.
    pub fn next_tab(&mut self, cx: &mut Context<Self>) -> bool {
        let changed = self.tabs.select_next();
        self.tab_shown(changed, cx)
    }

    /// Shows the previous tab, wrapping around.
    pub fn prev_tab(&mut self, cx: &mut Context<Self>) -> bool {
        let changed = self.tabs.select_prev();
        self.tab_shown(changed, cx)
    }

    /// Another tab is shown: no pane is maximized, and which is shown is saved.
    fn tab_shown(&mut self, changed: bool, cx: &mut Context<Self>) -> bool {
        if changed {
            self.maximized = None;
            self.layout_changed(cx);
        }
        changed
    }

    /// The pane `n` (1-based, reading order).
    pub fn pane_by_number(&self, n: usize) -> Option<PaneId> {
        self.tabs.layout().panes().get(n.checked_sub(1)?).copied()
    }

    pub fn neighbour(&self, dir: Dir) -> Option<PaneId> {
        let tabs = &self.tabs;
        tabs.layout().neighbour(tabs.focused(), dir, 1.0, 1.0)
    }

    /// Shows `agent_id` in the focused pane (attaching again if the pane
    /// is meant to show it but does not, for example after a failed spawn).
    pub fn show_agent(&mut self, agent_id: u64, cx: &mut Context<Self>) {
        let pane = self.tabs.focused();
        if self.tabs.agent(pane) == Some(agent_id) && self.pane_shows(pane, agent_id, cx) {
            return;
        }
        self.forget_pending(pane);
        self.requested.insert(pane, agent_id);
        self.tabs.set_agent(pane, Some(agent_id));
        self.session.attach(pane, agent_id);
        self.layout_changed(cx);
    }

    /// The graph's ⏎ or click (spec §7): focuses a pane already showing
    /// `agent_id` (in any tab, selecting it), else shows it in the focused
    /// pane.
    pub fn reveal_agent(&mut self, agent_id: u64, cx: &mut Context<Self>) {
        match crate::core::graph::pane_showing(&self.tabs, agent_id) {
            Some((tab, pane)) => {
                self.select_tab(tab, cx);
                self.set_focused(pane, cx);
                self.show_agent(agent_id, cx);
            }
            None => self.show_agent(agent_id, cx),
        }
    }

    /// `pane`'s grid shows `agent_id`, live or exited.
    fn pane_shows(&self, pane: PaneId, agent_id: u64, cx: &App) -> bool {
        self.panes.get(&pane).is_some_and(|v| {
            let m = lock(v.read(cx).model_handle());
            m.agent_id() == Some(agent_id)
                && matches!(m.state(), PaneState::Live | PaneState::Exited(_))
        })
    }

    /// Drops what was on its way into `pane`.
    fn forget_pending(&mut self, pane: PaneId) {
        self.starting.remove(&pane);
        self.spawn_dirs.remove(&pane);
        self.spawn_commands.remove(&pane);
        self.requested.remove(&pane);
        self.restarting.remove(&pane);
    }

    /// Splits the focused pane to the right and shows `agent_id` there.
    pub fn open_in_new_split(&mut self, agent_id: u64, cx: &mut Context<Self>) -> Option<PaneId> {
        self.open_beside(self.tabs.focused(), Dir::Right, agent_id, cx)
    }

    /// Splits `pane`, the new pane on its `dir` side showing `agent_id`
    /// and focused. `None` at [`MAX_PANES`] or for an unknown pane.
    fn open_beside(
        &mut self,
        pane: PaneId,
        dir: Dir,
        agent_id: u64,
        cx: &mut Context<Self>,
    ) -> Option<PaneId> {
        let new = self.tabs.split_toward(pane, dir)?;
        self.maximized = None;
        self.tabs.set_agent(new, Some(agent_id));
        self.open(new, Some(agent_id), cx);
        self.layout_changed(cx);
        Some(new)
    }

    /// Carries out a drag and drop (ADR 0059). The pane the agent or the
    /// pane lands in gets the focus, and is returned; `None` if nothing
    /// changed (a pane or split gone meanwhile, or no room for a pane).
    pub fn drop_on(&mut self, action: DropAction, cx: &mut Context<Self>) -> Option<PaneId> {
        match action {
            DropAction::Show { pane, agent } => {
                if !self.panes.contains_key(&pane) {
                    return None;
                }
                self.set_focused(pane, cx);
                self.show_agent(agent, cx);
                Some(pane)
            }
            DropAction::SplitShow { pane, dir, agent } => self.open_beside(pane, dir, agent, cx),
            DropAction::Swap { pane, with } => {
                let swapped = self.tabs.layout_mut().swap(pane, with);
                swapped.then(|| self.moved(pane, cx))
            }
            DropAction::Move { pane, target, dir } => {
                let moved = self.tabs.layout_mut().move_pane(pane, target, dir);
                moved.then(|| self.moved(pane, cx))
            }
        }
    }

    /// `pane` moved in the layout: it gets the focus, in the split view.
    fn moved(&mut self, pane: PaneId, cx: &mut Context<Self>) -> PaneId {
        self.maximized = None;
        self.tabs.focus(pane);
        self.layout_changed(cx);
        pane
    }

    /// Spawns into the focused pane at its size. The directory becomes a
    /// recent one once the pane shows the new agent.
    pub fn spawn(&mut self, req: SpawnRequest, cx: &mut Context<Self>) -> Result<(), String> {
        let pane = self.tabs.focused();
        let (cols, rows) = self
            .panes
            .get(&pane)
            .and_then(|v| v.read(cx).grid())
            .unwrap_or((80, 24));
        let cwd = self.expand(&req.cwd);
        let req = SpawnRequest {
            cwd: cwd.clone(),
            ..req
        };
        self.session.spawn(req, cols, rows, pane)?;
        self.forget_pending(pane);
        self.starting.insert(pane);
        self.spawn_dirs.insert(pane, cwd);
        self.refresh_chrome(cx);
        cx.notify();
        Ok(())
    }

    /// A spawn from the New agent sheet: as [`AppState::spawn`], and
    /// `command` (as typed, without the prompt) becomes a recent command
    /// once the pane shows the new agent.
    pub fn spawn_from_sheet(
        &mut self,
        req: SpawnRequest,
        command: String,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.spawn(req, cx)?;
        let command = command.trim().to_owned();
        if !command.is_empty() {
            self.spawn_commands.insert(self.tabs.focused(), command);
        }
        Ok(())
    }

    /// New terminal: a login shell (`kind=shell`) in the focused agent's
    /// folder, else the last-used one, else home (a remote daemon's own
    /// directory, as it is unknown here). It opens in a new split when the
    /// focused pane shows an agent and one fits, else in the focused pane,
    /// and that pane is focused.
    pub fn new_terminal(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let focused = self.tabs.focused();
        let focused_agent = self.tabs.agent(focused);
        let cwd = self.start_dir(focused);
        let req = self.terminal_request(cwd);
        if focused_agent.is_some() {
            self.split(Axis::Horizontal, cx);
        }
        self.spawn(req, cx)
    }

    /// New terminal's request: a login shell in `cwd`.
    pub(super) fn terminal_request(&self, cwd: String) -> SpawnRequest {
        let shell = std::env::var("SHELL").ok();
        let infos: Vec<_> = self.agents.iter().map(|r| r.info.clone()).collect();
        new_terminal(&self.target, shell.as_deref(), cwd, &live_names(&infos))
    }

    /// Where something started next to `pane` runs: its agent's folder,
    /// else the last-used one, else home (a remote daemon's own directory,
    /// as it is unknown here).
    pub(super) fn start_dir(&self, pane: PaneId) -> String {
        let cwd = self
            .tabs
            .agent(pane)
            .and_then(|id| self.agents.iter().find(|r| r.info.id == id))
            .map(|r| r.info.cwd.as_str());
        let recent = self
            .gui_state
            .recent_dirs
            .get(&self.target.to_string())
            .map_or(&[][..], Vec::as_slice);
        let home = match self.target {
            Target::Local => self.home.to_string_lossy().into_owned(),
            Target::Ssh { .. } => String::new(),
        };
        terminal_dir(cwd, recent, &home)
    }

    /// `pane` shows an agent, or a spawn is on its way into it.
    pub(super) fn pane_filled(&self, pane: PaneId) -> bool {
        self.tabs.agent(pane).is_some() || self.starting.contains(&pane)
    }

    /// The agents some pane of the shown tab shows.
    pub(super) fn on_screen(&self) -> HashSet<u64> {
        let layout = self.tabs.layout();
        layout
            .panes()
            .into_iter()
            .filter_map(|p| layout.agent(p))
            .collect()
    }

    /// `~` is the home directory for a local daemon; remote paths are the
    /// remote host's, passed as typed.
    fn expand(&self, dir: &str) -> String {
        if self.target != Target::Local {
            return dir.to_owned();
        }
        match dir.strip_prefix('~') {
            Some("") => self.home.to_string_lossy().into_owned(),
            Some(rest) if rest.starts_with('/') => {
                format!("{}{rest}", self.home.to_string_lossy())
            }
            _ => dir.to_owned(),
        }
    }

    pub fn kill(&self, agent_id: u64) {
        self.session.kill(agent_id);
    }

    /// Approves or denies an agent's pending spawn (ADR 0079).
    pub fn decide_spawn(&self, id: u64, approve: bool) {
        self.session.decide_spawn(id, approve);
    }

    /// Restarts an exited agent from the sidebar: panes showing it follow.
    pub fn restart(&self, agent_id: u64) {
        self.session.restart(agent_id);
    }

    pub fn forget(&self, agent_id: u64) {
        self.session.forget(vec![agent_id]);
    }

    /// Remove subtree (graph): one `ForgetAgents` with every id.
    pub fn forget_many(&self, ids: Vec<u64>) {
        if !ids.is_empty() {
            self.session.forget(ids);
        }
    }

    pub fn rename(&self, agent_id: u64, name: String) {
        self.session.rename(agent_id, name);
    }

    pub fn set_labels(&self, agent_id: u64, set: HashMap<String, String>, remove: Vec<String>) {
        self.session.set_labels(agent_id, set, remove);
    }

    /// Answers `agent_id`'s open question (ADR 0078).
    pub fn answer(&self, agent_id: u64, question: u64, text: String) {
        self.session.answer(agent_id, question, text);
    }

    /// A job reading `agent_id`'s output as plain text (ADR 0086).
    pub(super) fn read_output(&self, agent_id: u64) -> OutputJob {
        self.session.read_output(agent_id)
    }

    /// A job searching every agent's output (ADR 0098).
    pub(super) fn search_output(&self, query: String, case_sensitive: bool) -> SearchJob {
        self.session.search_output(query, case_sensitive)
    }

    /// Forgets these agents (fleetd refuses running ones).
    pub(super) fn forget_all(&self, ids: Vec<u64>) {
        if !ids.is_empty() {
            self.session.forget(ids);
        }
    }

    /// Shows `text` in the neutral toast, until dismissed.
    pub fn show_notice(&mut self, text: String, cx: &mut Context<Self>) {
        self.notice = Some(text);
        cx.notify();
    }

    /// Types `data` into an agent (Send to Agents…, ADR 0083).
    pub fn send_input(&self, agent_id: u64, data: Vec<u8>) {
        self.session.send_input(agent_id, data);
    }

    /// Send to Agents… (ADR 0083): types the plan's text into each
    /// target, as `fleet send` does (a bracketed paste for several lines
    /// where a pane says the agent wants one), then the Return,
    /// [`RETURN_DELAY_MS`] later if there was text.
    pub fn send_text(&mut self, plan: &SendPlan, cx: &mut Context<Self>) {
        if !plan.text.is_empty() {
            for &id in &plan.targets {
                let bracketed = self.bracketed_paste(id, cx).unwrap_or(false);
                self.send_input(id, encode_text(&plan.text, bracketed));
            }
        }
        if !plan.press_return {
            return;
        }
        let targets = plan.targets.clone();
        if !plan.return_delayed() {
            for id in targets {
                self.send_input(id, RETURN.to_vec());
            }
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(RETURN_DELAY_MS))
                .await;
            let _ = this.update(cx, |a, _| {
                for id in targets {
                    a.send_input(id, RETURN.to_vec());
                }
            });
        })
        .detach();
    }

    /// Whether a pane showing `agent_id` (in any tab) has bracketed paste
    /// on; `None` when no pane shows it.
    pub fn bracketed_paste(&self, agent_id: u64, cx: &App) -> Option<bool> {
        self.panes
            .iter()
            .filter(|(pane, _)| self.tabs.agent(**pane) == Some(agent_id))
            .map(|(_, view)| view.read(cx).bracketed_paste())
            .reduce(|a, b| a || b)
    }

    /// The summary bar's Remove: forgets the agent `pane` shows if it
    /// exited or is lost; otherwise does nothing. The pane empties when the
    /// node announces the removal.
    pub fn remove_in_pane(&self, pane: PaneId, cx: &App) {
        if let Some(agent) = self.ended_agent(pane, cx) {
            self.forget(agent);
        }
    }

    /// The agent `pane` shows, if it exited or is lost.
    fn ended_agent(&self, pane: PaneId, cx: &App) -> Option<u64> {
        let view = self.panes.get(&pane)?;
        let (state, shown) = {
            let m = lock(view.read(cx).model_handle());
            (m.state(), m.agent_id())
        };
        match state {
            // The final screen on show is that agent's.
            PaneState::Exited(_) => shown,
            // The attach failed: the agent is the one the layout names.
            PaneState::Lost => self.tabs.agent(pane).or(shown),
            _ => None,
        }
    }

    /// Clear exited. Every ended agent goes in one request fleetd answers
    /// under its lock (ADR 0070), unless an ended agent has a running
    /// descendant: that one stays (ADR 0074), so the ones that may go are
    /// named.
    pub fn clear_exited(&self) {
        let removable = removable_ended(&self.agents);
        let ended = self.agents.iter().filter(|r| ended(r)).count();
        if removable.len() == ended {
            self.session.clear_exited();
        } else if !removable.is_empty() {
            self.session.forget(removable);
        }
    }

    /// Restarts fleetd into its binary on disk; agents keep running and
    /// the outcome is shown as a toast. Asked again while one is in flight
    /// (a double click), it does nothing.
    pub fn restart_daemon(&mut self, cx: &mut Context<Self>) {
        if self.updating {
            return;
        }
        self.session.restart_daemon();
        self.updating = self.session.restart_pending();
        cx.notify();
    }

    /// The bar's Update fleetd button, if it shows.
    pub fn update_button(&self) -> Option<UpdateButton> {
        update_button(self.update_available, self.updating)
    }

    /// Re-checks whether a newer fleetd is on disk. Stats a file, so it runs
    /// on connect and when the window gains focus, never per frame.
    pub fn refresh_update_available(&mut self, cx: &mut Context<Self>) {
        let available = self.session.update_available();
        if available != self.update_available {
            self.update_available = available;
            cx.notify();
        }
    }

    /// Files dropped from Finder on `agent`'s sidebar row: their quoted
    /// paths are typed into it (ADR 0081), through a pane of the shown
    /// tab showing it (the focused one first: bracketed paste as it
    /// asked), else on the control connection, unbracketed
    /// (as Send to Agents…, so a refusal shows an error). Not for an
    /// ended agent; on a remote target, a notice instead.
    pub fn drop_paths_on_agent(&mut self, agent: u64, paths: &[String], cx: &mut Context<Self>) {
        if self.target != Target::Local {
            self.show_notice(DROP_NEEDS_LOCAL.to_owned(), cx);
            return;
        }
        let running = self.agents.iter().any(|r| r.info.id == agent && !ended(r));
        let Some(text) = dropped_text(paths).filter(|_| running) else {
            return;
        };
        let focused = self.tabs.focused();
        let pane = self
            .tabs
            .layout()
            .panes()
            .into_iter()
            .filter(|p| self.tabs.agent(*p) == Some(agent))
            .min_by_key(|p| *p != focused);
        match pane.and_then(|p| self.panes.get(&p)) {
            Some(view) => view.update(cx, |v, cx| v.paste_text(&text, cx)),
            None => self.session.send_input(agent, text.into_bytes()),
        }
    }

    /// Shows `text` in the error toast, until dismissed.
    pub(super) fn show_error(&mut self, text: String, cx: &mut Context<Self>) {
        eprintln!("fleet-gui: {text}");
        self.error = Some(text);
        self.error_is_connection = false;
        cx.notify();
    }

    /// Exited or lost agents Clear exited removes: not an ended parent
    /// while one of its descendants runs (ADR 0074).
    pub fn ended_count(&self) -> usize {
        removable_ended(&self.agents).len()
    }

    /// The ids of the exited or lost agents Clear exited removes: not an
    /// ended parent while one of its descendants runs (ADR 0074).
    pub fn ended_ids(&self) -> Vec<u64> {
        removable_ended(&self.agents)
    }

    /// Restarts the agent the focused pane shows, in that pane at its
    /// size, if it exited or is lost; otherwise does nothing.
    pub fn restart_focused(&mut self, cx: &mut Context<Self>) {
        let pane = self.tabs.focused();
        let Some(agent) = self.ended_agent(pane, cx) else {
            return;
        };
        let Some(view) = self.panes.get(&pane) else {
            return;
        };
        let (cols, rows) = view.read(cx).grid().unwrap_or((80, 24));
        match self.session.restart_in_pane(pane, agent, cols, rows) {
            Ok(()) => {
                self.forget_pending(pane);
                self.restarting.insert(pane, agent);
            }
            Err(e) => {
                self.error = Some(e);
                self.error_is_connection = false;
                cx.notify();
            }
        }
    }

    /// The New agent sheet's directories.
    pub fn dir_choices(&self) -> Vec<String> {
        let recent = self
            .gui_state
            .recent_dirs
            .get(&self.target.to_string())
            .map_or(&[][..], Vec::as_slice);
        dir_choices(recent, &self.agents)
    }

    /// The New agent sheet's recent commands for the target, most recent
    /// first.
    pub fn recent_commands(&self) -> Vec<String> {
        self.gui_state
            .recent_commands
            .get(&self.target.to_string())
            .cloned()
            .unwrap_or_default()
    }

    /// The command presets (ADR 0084): `[gui] presets`, else the agents
    /// found on `PATH`, looked for once per target on first use (a few
    /// `stat`s, never per frame).
    pub fn presets(&mut self) -> Vec<Preset> {
        if let Some(presets) = &self.config.presets {
            return usable(presets);
        }
        match &self.detected_presets {
            Some((target, presets)) if *target == self.target => presets.clone(),
            _ => {
                let path = std::env::var_os("PATH").unwrap_or_default();
                let found = detect_presets(&self.target, &path, &self.home);
                self.detected_presets = Some((self.target.clone(), found.clone()));
                found
            }
        }
    }

    /// The presets come from `PATH` (no `[gui] presets`).
    pub fn presets_detected(&self) -> bool {
        self.config.presets.is_none()
    }

    pub fn config(&self) -> &GuiConfig {
        &self.config
    }

    /// The theme the windows draw with.
    pub fn theme(&self) -> Theme {
        self.theme
    }

    /// Settings changes: the changed keys are written to `config.toml`,
    /// then take effect at once (the panes' font and Option as Meta here;
    /// the window's look and motion follow through its observer). Nothing
    /// changes if the file cannot be written.
    pub fn change_settings(
        &mut self,
        changes: Vec<Change>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let mut next = self.config.clone();
        let keys: Vec<&str> = changes
            .into_iter()
            .filter_map(|c| c.apply(&mut next))
            .collect();
        if keys.is_empty() {
            return Ok(());
        }
        save_changes(&self.state_dir, &next, &keys)
            .map_err(|e| format!("cannot save the settings: {e}"))?;
        if keys.contains(&"theme") {
            super::native::set_app_appearance(next.theme);
        }
        self.set_config(next, cx);
        Ok(())
    }

    /// Reads `config.toml` again (the Settings window opening): edits made
    /// to the file meanwhile take effect. A malformed file is left for the
    /// next launch to report; the settings in use stay.
    pub fn reload_config(&mut self, cx: &mut Context<Self>) {
        if let Ok(c) = load_config(&self.state_dir)
            && c != self.config
        {
            if c.theme != self.config.theme {
                super::native::set_app_appearance(c.theme);
            }
            self.set_config(c, cx);
        }
    }

    fn set_config(&mut self, config: GuiConfig, cx: &mut Context<Self>) {
        self.config = config;
        self.apply_pane_config(cx);
        // Focus mode dims the panes without focus.
        self.refresh_chrome(cx);
        cx.notify();
    }

    /// The exit toasts shown, oldest first.
    pub fn exit_toasts(&self) -> impl Iterator<Item = &ExitNote> {
        self.exit_toasts.items()
    }

    /// Agents that exited just now: a toast each (with Restart), and the
    /// celebration for a success when it is turned on. Toasts whose agent
    /// was restarted or removed meanwhile go.
    fn show_exits(&mut self, exited: Vec<ExitNote>, cx: &mut Context<Self>) {
        let now = Instant::now();
        let mut changed = !exited.is_empty();
        for note in exited {
            if feedback::celebrates(self.config.celebrate, &note) {
                super::native::play_sound(super::native::CELEBRATION_SOUND);
                self.celebrating = Some((note.agent_id, now));
            }
            self.exit_toasts.push(note, now);
        }
        changed |= self.exit_toasts.prune(&self.agents, now);
        if changed {
            self.schedule_exit_expiry(cx);
            cx.notify();
        }
    }

    /// The config the panes follow: the settings, with the zoomed size.
    fn pane_config(&self) -> GuiConfig {
        GuiConfig {
            font_size: self.zoom.size(self.config.font_size),
            ..self.config.clone()
        }
    }

    fn apply_pane_config(&mut self, cx: &mut Context<Self>) {
        let config = self.pane_config();
        for view in self.panes.values() {
            view.update(cx, |v, cx| v.apply_config(&config, cx));
        }
    }

    /// ⌘+ / ⌘− / ⌘0: every pane's font follows at once (the next layout
    /// reflows the grid and resizes the agents). Not saved.
    pub fn zoom(&mut self, zoom: Zoom, cx: &mut Context<Self>) {
        if self.zoom.apply(zoom, self.config.font_size) {
            self.apply_pane_config(cx);
            cx.notify();
        }
    }

    /// Wakes once, when the oldest exit toast is due to go.
    fn schedule_exit_expiry(&mut self, cx: &mut Context<Self>) {
        self.exit_expiry = self.exit_toasts.next_expiry().map(|at| {
            cx.spawn(async move |this, cx| {
                let wait = at.saturating_duration_since(Instant::now());
                cx.background_executor().timer(wait).await;
                let _ = this.update(cx, |s, cx| {
                    // The timer waited until `at`: prune as of then at least.
                    if s.exit_toasts.prune(&s.agents, Instant::now().max(at)) {
                        cx.notify();
                    }
                    s.schedule_exit_expiry(cx);
                });
            })
        });
    }

    /// The exit toast's ✕.
    pub fn dismiss_exit(&mut self, agent_id: u64, cx: &mut Context<Self>) {
        if self.exit_toasts.dismiss(agent_id) {
            self.schedule_exit_expiry(cx);
            cx.notify();
        }
    }

    /// The exit toast's Restart: the agent again (panes showing it
    /// follow), and the toast goes.
    pub fn restart_from_toast(&mut self, agent_id: u64, cx: &mut Context<Self>) {
        self.restart(agent_id);
        self.dismiss_exit(agent_id, cx);
    }

    /// The terminal font size the panes show (the configured one, zoomed).
    pub fn shown_font_size(&self) -> f32 {
        self.zoom.size(self.config.font_size)
    }

    pub fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        if self.theme == theme {
            return;
        }
        self.theme = theme;
        self.session.set_palette(theme.palette);
        cx.set_global(theme);
        cx.notify();
    }

    /// Shows a config error from launch (also after the first connect).
    pub fn set_config_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.config_error = Some(message.clone());
        self.error = Some(message);
        self.error_is_connection = false;
        cx.notify();
    }

    pub(super) fn dismiss_error(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.notice = None;
        self.error_is_connection = false;
        cx.notify();
    }

    /// The live agents' names, which a generated name avoids.
    pub(super) fn live_names(&self) -> Vec<String> {
        let infos: Vec<_> = self.agents.iter().map(|r| r.info.clone()).collect();
        live_names(&infos).into_iter().map(str::to_owned).collect()
    }
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
