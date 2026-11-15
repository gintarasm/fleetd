//! What outlives the window (the app is resident): the session, the
//! layout, the panes and the saved state. [`AppState`] is a gpui global
//! entity; the [`Workspace`](super::workspace::Workspace) window draws it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use fleet_term::Palette;
use gpui::{App, AppContext, Context, Entity, Global, Task, WindowHandle};

use super::new_agent::dir_choices;
use super::pane_view::{InputSink, PaneChrome, PaneView};
use super::sidebar_view::{TabMark, program_name};
use super::theme::Theme;
use super::workspace::Workspace;
use crate::core::config::GuiConfig;
use crate::core::layout::{Axis, Dir, Layout};
use crate::core::pane::{PaneModel, PaneState};
use crate::core::row_preview::only_activity_changed;
use crate::core::session::{AgentRow, ConnState, PaneId, Session, UiEvent, restart_failure_text};
use crate::core::sidebar::{ended, tilde};
use crate::core::spawn::{SpawnRequest, new_terminal, terminal_dir};
use crate::core::state::{GuiState, MAX_RECENT_DIRS, MAX_RECENT_TARGETS, push_recent};
use crate::core::status::{AgentStatus, Alerts, alert_text, statuses};
use crate::core::target::Target;
use crate::core::titlebar::{UpdateButton, update_button};

/// Saved state is written once it has been still this long.
pub const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

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
    /// Forgets every exited or lost agent on the target.
    fn clear_exited(&self);
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
    fn open_pane(&self, pane: PaneId, agent_id: Option<u64>) -> Arc<Mutex<PaneModel>>;
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

    fn clear_exited(&self) {
        Session::clear_exited(self);
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

/// `target`'s saved layout, else one empty pane.
fn saved_layout(state: &GuiState, target: &Target) -> Layout {
    state
        .layouts
        .get(&target.to_string())
        .map_or_else(Layout::single, Layout::from_saved)
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
    gui_state: GuiState,
    pub(super) layout: Layout,
    panes: HashMap<PaneId, Entity<PaneView>>,
    focused: PaneId,
    /// The pane filling the pane area, if any (not saved).
    pub(super) maximized: Option<PaneId>,
    /// OSC titles of the agents shown in panes.
    pub(super) titles: HashMap<u64, String>,
    pub(super) theme: Theme,
    pub(super) agents: Vec<AgentRow>,
    /// Each non-shell agent's status (ADR 0042), as of the last
    /// [`AppState::refresh_status`].
    pub(super) statuses: HashMap<u64, AgentStatus>,
    /// Agents whose pane rang the bell where you were not looking; cleared
    /// once a pane showing it is focused in the active window.
    pub(super) bells: HashSet<u64>,
    alerts: Alerts,
    /// The window is open and has focus: no notifications then.
    window_active: bool,
    /// The last error, shown until dismissed or replaced.
    pub(super) error: Option<String>,
    /// A neutral message (a successful restart), shown when there is no
    /// error; dismissed the same way.
    pub(super) notice: Option<String>,
    /// `error` is a failure to connect, cleared once connected.
    error_is_connection: bool,
    /// A config error from launch, still shown after the first connect.
    config_error: Option<String>,
    /// Panes a spawn is on its way into (for the "starting…" text only).
    starting: HashSet<PaneId>,
    /// The directory of the spawn on its way into a pane; it becomes a
    /// recent directory once the pane shows the new agent.
    spawn_dirs: HashMap<PaneId, String>,
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
        let layout = saved_layout(&gui_state, &target);
        session.set_palette(theme.palette);
        cx.set_global(theme);
        let events = follow(ui, cx);
        let focused = layout.panes()[0];
        let mut s = AppState {
            agents: session.agents(),
            session,
            live: true,
            session_epoch: 0,
            target,
            state_dir,
            home: home_dir(),
            config,
            gui_state,
            layout,
            panes: HashMap::new(),
            focused,
            maximized: None,
            titles: HashMap::new(),
            theme,
            statuses: HashMap::new(),
            bells: HashSet::new(),
            alerts: Alerts::default(),
            window_active: false,
            error: None,
            error_is_connection: false,
            config_error: None,
            notice: None,
            starting: HashSet::new(),
            spawn_dirs: HashMap::new(),
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
        for pane in s.layout.panes() {
            let agent = s.layout.agent(pane);
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
        s.layout = Layout::single();
        s.focused = s.layout.panes()[0];
        s.open(s.focused, None, cx);
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
        self.layout = saved_layout(&self.gui_state, &self.target);
        self.panes.clear();
        self.focused = self.layout.panes()[0];
        self.maximized = None;
        self.titles.clear();
        // The new target's waiting and ended agents are not news.
        self.statuses.clear();
        self.bells.clear();
        self.alerts = Alerts::default();
        // A launch config error outlives the first connect only.
        self.error = self.config_error.take();
        self.error_is_connection = false;
        self.starting.clear();
        self.spawn_dirs.clear();
        self.requested.clear();
        self.restarting.clear();
        self.unlisted.clear();
        self.last_list = 0;
        self.reconnect_at = None;
        self.update_available = self.session.update_available();
        self.updating = false;
        self.agents = self.session.agents();
        for pane in self.layout.panes() {
            let agent = self.layout.agent(pane);
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

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn focused(&self) -> PaneId {
        self.focused
    }

    pub fn pane_view(&self, pane: PaneId) -> Option<&Entity<PaneView>> {
        self.panes.get(&pane)
    }

    pub fn gui_state(&self) -> &GuiState {
        &self.gui_state
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
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
        let config = &self.config;
        let view = cx.new(|cx| PaneView::new(pane, model, sink, config, cx));
        self.panes.insert(pane, view);
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
                    self.agents = rows;
                    let agents = &self.agents;
                    self.bells
                        .retain(|id| agents.iter().any(|r| r.info.id == *id));
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
                            && !(self.window_active && pane == self.focused)
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
        if self.layout.agent(pane) == Some(shown) {
            return false;
        }
        self.layout.set_agent(pane, Some(shown));
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
        true
    }

    /// `old` was removed, restarted as `new` if set: every pane showing it
    /// shows `new` (attaching unless it started `new` itself) or becomes
    /// empty. True if the layout changed.
    fn follow_removed(&mut self, old: u64, new: Option<u64>, cx: &mut Context<Self>) -> bool {
        let panes = self.layout.replace_agent(old, new);
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
        if let Some(agent) = self.layout.agent(pane) {
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
        for pane in self.layout.panes() {
            let Some(id) = self.layout.agent(pane) else {
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
                self.layout.set_agent(pane, None);
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
            let (title, body) = alert_text(&name, status, row.info.exit_code, detail);
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
        if let Some(id) = self.layout.agent(self.focused)
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
        for (i, pane) in self.layout.panes().into_iter().enumerate() {
            let agent = self.layout.agent(pane);
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
                starting: self.starting.contains(&pane),
                reconnect_in,
                focused: pane == self.focused,
                maximized: self.maximized == Some(pane),
                status: agent.and_then(|id| self.statuses.get(&id).copied()),
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
            let key = self.target_key();
            self.gui_state.layouts.insert(key, self.layout.to_saved());
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
        if self.focused != pane && self.panes.contains_key(&pane) {
            self.focused = pane;
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
            None if self.layout.panes().len() > 1 => Some(self.focused),
            None => None,
        };
        cx.notify();
    }

    /// How each agent's tab relates to the panes.
    pub fn tab_marks(&self) -> HashMap<u64, TabMark> {
        let mut marks = HashMap::new();
        for pane in self.layout.panes() {
            if let Some(id) = self.layout.agent(pane) {
                let mark = if pane == self.focused {
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
        let new = self.layout.split(self.focused, axis)?;
        self.maximized = None;
        self.open(new, None, cx);
        self.focused = new;
        self.layout_changed(cx);
        Some(new)
    }

    /// Closes the focused pane, detaching its agent (never killing it).
    /// The last pane stays, empty.
    pub fn close_focused(&mut self, cx: &mut Context<Self>) {
        let pane = self.focused;
        let Some(next) = self.layout.close(pane) else {
            return;
        };
        self.session.close_pane(pane);
        self.panes.remove(&pane);
        self.maximized = None;
        self.forget_pending(pane);
        if next == pane {
            // The last pane: a fresh, empty connection in its place.
            self.open(pane, None, cx);
        }
        self.focused = next;
        self.layout_changed(cx);
    }

    /// The pane `n` (1-based, reading order).
    pub fn pane_by_number(&self, n: usize) -> Option<PaneId> {
        self.layout.panes().get(n.checked_sub(1)?).copied()
    }

    pub fn neighbour(&self, dir: Dir) -> Option<PaneId> {
        self.layout.neighbour(self.focused, dir, 1.0, 1.0)
    }

    /// Shows `agent_id` in the focused pane (attaching again if the pane
    /// is meant to show it but does not, for example after a failed spawn).
    pub fn show_agent(&mut self, agent_id: u64, cx: &mut Context<Self>) {
        let pane = self.focused;
        if self.layout.agent(pane) == Some(agent_id) && self.pane_shows(pane, agent_id, cx) {
            return;
        }
        self.forget_pending(pane);
        self.requested.insert(pane, agent_id);
        self.layout.set_agent(pane, Some(agent_id));
        self.session.attach(pane, agent_id);
        self.layout_changed(cx);
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
        self.requested.remove(&pane);
        self.restarting.remove(&pane);
    }

    /// Splits the focused pane to the right and shows `agent_id` there.
    pub fn open_in_new_split(&mut self, agent_id: u64, cx: &mut Context<Self>) -> Option<PaneId> {
        let new = self.layout.split(self.focused, Axis::Horizontal)?;
        self.maximized = None;
        self.layout.set_agent(new, Some(agent_id));
        self.open(new, Some(agent_id), cx);
        self.focused = new;
        self.layout_changed(cx);
        Some(new)
    }

    /// Spawns into the focused pane at its size. The directory becomes a
    /// recent one once the pane shows the new agent.
    pub fn spawn(&mut self, req: SpawnRequest, cx: &mut Context<Self>) -> Result<(), String> {
        let pane = self.focused;
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

    /// New terminal: a login shell (`kind=shell`) in the focused agent's
    /// folder, else the last-used one, else home (a remote daemon's own
    /// directory, as it is unknown here). It opens in a new split when the
    /// focused pane shows an agent and one fits, else in the focused pane,
    /// and that pane is focused.
    pub fn new_terminal(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let focused_agent = self.layout.agent(self.focused);
        let focused_cwd = focused_agent
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
        let cwd = terminal_dir(focused_cwd, recent, &home);
        let shell = std::env::var("SHELL").ok();
        let req = new_terminal(&self.target, shell.as_deref(), cwd);
        if focused_agent.is_some() {
            self.split(Axis::Horizontal, cx);
        }
        self.spawn(req, cx)
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

    /// Restarts an exited agent from the sidebar: panes showing it follow.
    pub fn restart(&self, agent_id: u64) {
        self.session.restart(agent_id);
    }

    pub fn forget(&self, agent_id: u64) {
        self.session.forget(vec![agent_id]);
    }

    pub fn clear_exited(&self) {
        self.session.clear_exited();
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

    /// Shows `text` in the error toast, until dismissed.
    pub(super) fn show_error(&mut self, text: String, cx: &mut Context<Self>) {
        eprintln!("fleet-gui: {text}");
        self.error = Some(text);
        self.error_is_connection = false;
        cx.notify();
    }

    /// Exited or lost agents in the list.
    pub fn ended_count(&self) -> usize {
        self.agents.iter().filter(|r| ended(r)).count()
    }

    /// Restarts the agent the focused pane shows, in that pane at its
    /// size, if it exited or is lost; otherwise does nothing.
    pub fn restart_focused(&mut self, cx: &mut Context<Self>) {
        let pane = self.focused;
        let Some(view) = self.panes.get(&pane) else {
            return;
        };
        let (state, shown) = {
            let m = lock(view.read(cx).model_handle());
            (m.state(), m.agent_id())
        };
        let agent = match state {
            // The final screen on show is that agent's.
            PaneState::Exited(_) => shown,
            // The attach failed: the agent is the one the layout names.
            PaneState::Lost => self.layout.agent(pane).or(shown),
            _ => None,
        };
        let Some(agent) = agent else {
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

    pub(super) fn label(&self, agent_id: u64) -> String {
        match self.agents.iter().find(|r| r.info.id == agent_id) {
            Some(r) => format!(
                "{} in {}",
                program_name(&r.info.program),
                tilde(&r.info.cwd, &self.home)
            ),
            None => format!("agent {agent_id}"),
        }
    }
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
