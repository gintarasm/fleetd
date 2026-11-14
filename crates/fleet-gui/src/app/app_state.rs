//! What outlives the window (the app is resident): the session, the
//! layout, the panes and the saved state. [`AppState`] is a gpui global
//! entity; the [`Workspace`](super::workspace::Workspace) window draws it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use fleet_term::Palette;
use gpui::{App, AppContext, Context, Entity, Global, Task, WindowHandle};

use super::new_agent::{SpawnRequest, dir_choices};
use super::pane_view::{InputSink, PaneChrome, PaneView};
use super::sidebar_view::{TabMark, program_name};
use super::theme::Theme;
use super::workspace::Workspace;
use crate::core::config::GuiConfig;
use crate::core::layout::{Axis, Dir, Layout};
use crate::core::pane::{PaneModel, PaneState};
use crate::core::session::{AgentRow, ConnState, PaneId, Session, UiEvent};
use crate::core::sidebar::tilde;
use crate::core::state::{GuiState, MAX_RECENT_DIRS, MAX_RECENT_TARGETS, push_recent};
use crate::core::target::Target;

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
    fn spawn(
        &self,
        program: String,
        args: Vec<String>,
        cwd: String,
        cols: u16,
        rows: u16,
        pane: PaneId,
    ) -> Result<(), String>;
    fn kill(&self, agent_id: u64);
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

    fn spawn(
        &self,
        program: String,
        args: Vec<String>,
        cwd: String,
        cols: u16,
        rows: u16,
        pane: PaneId,
    ) -> Result<(), String> {
        Session::spawn(self, program, args, cwd, cols, rows, pane)
    }

    fn kill(&self, agent_id: u64) {
        Session::kill(self, agent_id);
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
        _program: String,
        _args: Vec<String>,
        _cwd: String,
        _cols: u16,
        _rows: u16,
        _pane: PaneId,
    ) -> Result<(), String> {
        Err("not connected to a fleetd".into())
    }

    fn kill(&self, _agent_id: u64) {}

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
    /// The last error, shown until dismissed or replaced.
    pub(super) error: Option<String>,
    /// Panes a spawn is on its way into (for the "starting…" text only).
    starting: HashSet<PaneId>,
    /// The directory of the spawn on its way into a pane; it becomes a
    /// recent directory once the pane shows the new agent.
    spawn_dirs: HashMap<PaneId, String>,
    /// Agents asked for in a pane (sidebar click) that it does not show
    /// yet; until then the pane's old agent is not adopted back.
    requested: HashMap<PaneId, u64>,
    /// When the control connection retries, while it is down.
    reconnect_at: Option<Instant>,
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
            error: None,
            starting: HashSet::new(),
            spawn_dirs: HashMap::new(),
            requested: HashMap::new(),
            reconnect_at: None,
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
        self.error = None;
        self.starting.clear();
        self.spawn_dirs.clear();
        self.requested.clear();
        self.reconnect_at = None;
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
        let mut chrome = false;
        let mut layout = false;
        for event in batch {
            match event {
                UiEvent::AgentsChanged => {
                    self.agents = self.session.agents();
                    if self.live {
                        super::bench::agents_listed();
                    }
                    chrome = true;
                    cx.notify();
                }
                UiEvent::PaneChanged(pane) => {
                    let Some(view) = self.panes.get(&pane).cloned() else {
                        continue;
                    };
                    layout |= self.adopt_shown_agent(pane, &view, cx);
                    let (agent, title) = {
                        let model = lock(view.read(cx).model_handle());
                        (model.agent_id(), model.title())
                    };
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
                UiEvent::Connection(state) => {
                    self.reconnect_at = match state {
                        ConnState::Disconnected { retry_in } => Some(Instant::now() + retry_in),
                        _ => None,
                    };
                    chrome = true;
                }
                UiEvent::Error(e) => {
                    eprintln!("fleet-gui: {e}");
                    // A spawn on its way may be what failed: stop saying
                    // "starting…". Should it land after all, the pane still
                    // adopts its agent.
                    self.starting.clear();
                    self.error = Some(e);
                    chrome = true;
                    cx.notify();
                }
            }
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

    /// Once a second: the reconnect countdown.
    pub fn tick(&mut self, cx: &mut Context<Self>) {
        if self.reconnect_at.is_some() {
            self.refresh_chrome(cx);
        }
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
                program: match (row, agent) {
                    (Some(r), _) => Some(program_name(&r.info.program)),
                    (None, Some(id)) => Some(format!("agent {id}")),
                    (None, None) => None,
                },
                cwd: row.map(|r| tilde(&r.info.cwd, &self.home)),
                starting: self.starting.contains(&pane),
                reconnect_in,
                focused: pane == self.focused,
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
        self.session
            .spawn(req.program, req.args, cwd.clone(), cols, rows, pane)?;
        self.forget_pending(pane);
        self.starting.insert(pane);
        self.spawn_dirs.insert(pane, cwd);
        self.refresh_chrome(cx);
        cx.notify();
        Ok(())
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

    pub(super) fn dismiss_error(&mut self, cx: &mut Context<Self>) {
        self.error = None;
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
