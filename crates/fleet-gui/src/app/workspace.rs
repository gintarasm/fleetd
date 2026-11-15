//! The workspace window: the sidebar, up to four split panes and the
//! sheets over them, drawn from [`AppState`].

use std::collections::HashMap;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{
    Action, App, AppContext, Bounds, Context, Entity, EntityId, FocusHandle, InteractiveElement,
    IntoElement, ParentElement, PromptButton, PromptLevel, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, TitlebarOptions, Window,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, deferred, div, point, px, relative,
    size,
};

use super::app_state::AppState;
use super::connect::{ConnectEvent, ConnectSheet, StartupAttempt, new_sheet, switch_target};
use super::context_menu::{self, MenuLine};
use super::controls::appear;
use super::keymap::{
    ClosePane, Copy, FocusDown, FocusLeft, FocusPane1, FocusPane2, FocusPane3, FocusPane4,
    FocusRight, FocusUp, Kill, NewAgent, NewTerminal, Paste, Remove, Restart, RestartPane,
    SplitDown, SplitRight, ToggleMaximize, WORKSPACE_CONTEXT,
};
use super::native;
use super::new_agent::{NewAgentSheet, SheetEvent};
use super::pane_view::{PaneEvent, PaneView};
use super::sidebar_view::{SidebarData, TabMark, render_sidebar};
use super::theme::{Theme, Typography, appearance_of};
use super::titlebar_view::{BarData, render_bar, sheet_drag_strip};
use crate::core::config::{GuiConfig, save_connection};
use crate::core::layout::{Axis, Dir, MAX_PANES, Rect};
use crate::core::menus::{
    MenuEntry, PaneMenuItem, PaneMenuState, RowMenuItem, pane_menu, row_menu,
};
use crate::core::session::PaneId;
use crate::core::settings::{MotionChoice, ThemeChoice};
use crate::core::sidebar::ended;
use crate::core::style::{chrome, motion, radius, space, text};
use crate::core::target::Target;
use crate::core::theme::Tokens;
use crate::core::titlebar::{Link, bar_title, window_title};

/// Opens the workspace window, or brings the open one to the front.
pub fn open_main_window(cx: &mut App) {
    let Some(app) = AppState::global(cx) else {
        return;
    };
    if let Some(handle) = app.read(cx).window
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }
    let lights = chrome::traffic_lights_origin(chrome::BAR_HEIGHT);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1200.), px(760.)),
            cx,
        ))),
        // The unified bar: our own, under AppKit's traffic lights.
        titlebar: Some(TitlebarOptions {
            title: Some("fleet".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(lights.0), px(lights.1))),
        }),
        window_min_size: Some(size(px(640.), px(400.))),
        // The desktop shows through, blurred, under the theme's window
        // tint; Reduce Transparency keeps the window opaque (read when the
        // window opens).
        window_background: if native::reduce_transparency() {
            WindowBackgroundAppearance::Opaque
        } else {
            WindowBackgroundAppearance::Blurred
        },
        ..Default::default()
    };
    let state = app.clone();
    match cx.open_window(options, |window, cx| {
        super::bench::window_opened(window);
        cx.new(|cx| Workspace::new(state, window, cx))
    }) {
        Ok(handle) => app.update(cx, |a, _| a.window = Some(handle)),
        Err(e) => eprintln!("fleet-gui: cannot open the window: {e}"),
    }
}

/// Sets the backdrop's blur radius (`[gui] blur`, points). AppKit builds
/// the blur a few frames after the window (or a new appearance) first
/// draws, so this tries on each of the next frames until the blur is
/// there, for a second at most.
fn stronger_blur(window: &mut Window, radius: f32) {
    blur_within(window, f64::from(radius), 60);
}

fn blur_within(window: &mut Window, radius: f64, frames: u32) {
    window.on_next_frame(move |window, _| {
        if native::set_backdrop_blur(window, radius) == 0 && frames > 1 {
            blur_within(window, radius, frames - 1);
        }
    });
}

/// The settings the window's look and motion follow (re-applied when one
/// changes).
#[derive(Clone, Copy, PartialEq)]
struct Look {
    theme: ThemeChoice,
    opacity: f32,
    blur: f32,
    motion: MotionChoice,
}

impl Look {
    fn of(c: &GuiConfig) -> Self {
        Look {
            theme: c.theme,
            opacity: c.window_opacity,
            blur: c.blur,
            motion: c.reduce_motion,
        }
    }
}

/// The window's root view, over [`AppState`].
pub struct Workspace {
    app: Entity<AppState>,
    sheet: Option<(Entity<NewAgentSheet>, Subscription)>,
    /// The connect sheet, over everything else.
    connect: Option<(Entity<ConnectSheet>, [Subscription; 2])>,
    /// The [`AppState::session_epoch`] last seen; a change closes the
    /// new-agent sheet, and a pick from a context menu opened before is
    /// dropped.
    epoch: u64,
    /// Each pane's view, its focus handle, and its focus-in and
    /// header-button subscriptions.
    pane_focus: HashMap<PaneId, (EntityId, FocusHandle, [Subscription; 2])>,
    /// The panes drawn by the last render.
    drawn: Vec<PaneId>,
    /// The window title last set.
    window_title: String,
    /// Motion is reduced: System Settings' Reduce Motion (read at launch
    /// and on appearance changes) unless `[gui] reduce_motion` overrides it.
    reduce_motion: bool,
    /// The settings the window last applied.
    look: Look,
    _subscriptions: Vec<Subscription>,
    _ticker: Task<()>,
}

impl Workspace {
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // A switch (from any path) restores another layout: the new-agent
        // sheet and an open context menu would target panes that are gone.
        let observe = cx.observe_in(&app, window, |this, app, window, cx| {
            let epoch = app.read(cx).session_epoch();
            if epoch != this.epoch {
                this.epoch = epoch;
                if this.sheet.take().is_some() {
                    this.focus_current(window, cx);
                }
            }
            // Settings changed the look: apply it now.
            let look = Look::of(app.read(cx).config());
            if look != this.look {
                this.look = look;
                this.apply_look(window, cx);
            }
            cx.notify();
        });
        let appearance = cx.observe_window_appearance(window, |this, window, cx| {
            this.apply_look(window, cx);
        });
        let config = app.read(cx).config().clone();
        stronger_blur(window, config.blur);
        let theme = window_theme(window, &config);
        // Coming back to the window is when a fleetd installed meanwhile
        // is worth noticing (one stat, never per frame).
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.app.update(cx, |a, cx| a.refresh_update_available(cx));
            }
        });
        app.update(cx, |a, cx| {
            a.set_theme(theme, cx);
            // The list is often in before the window: draw it at once.
            a.sync_agents();
        });
        // `since` in the sidebar and the reconnect countdown age by the
        // second; the timer lives as long as the window.
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let ok = this.update(cx, |this, cx| {
                    this.app.update(cx, |a, cx| a.tick(cx));
                    if this.app.read(cx).has_agents() {
                        cx.notify();
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        });
        let epoch = app.read(cx).session_epoch();
        let ws = Workspace {
            app,
            _ticker: ticker,
            sheet: None,
            connect: None,
            epoch,
            pane_focus: HashMap::new(),
            drawn: Vec::new(),
            window_title: String::new(),
            reduce_motion: config.reduce_motion.resolve(native::reduce_motion()),
            look: Look::of(&config),
            _subscriptions: vec![observe, appearance, activation],
        };
        ws.focus_pane(ws.app.read(cx).focused(), window, cx);
        ws
    }

    pub fn app(&self) -> &Entity<AppState> {
        &self.app
    }

    /// The theme, blur and motion for the system's settings and ours.
    fn apply_look(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config = self.app.read(cx).config().clone();
        stronger_blur(window, config.blur);
        let theme = window_theme(window, &config);
        self.reduce_motion = config.reduce_motion.resolve(native::reduce_motion());
        self.app.update(cx, |a, cx| a.set_theme(theme, cx));
        window.refresh();
    }

    /// Motion is reduced (sheets and the toast appear without sliding).
    pub fn reduces_motion(&self) -> bool {
        self.reduce_motion
    }

    /// The panes the last frame drew (one while maximized).
    pub fn drawn_panes(&self) -> &[PaneId] {
        &self.drawn
    }

    pub fn sheet(&self) -> Option<&Entity<NewAgentSheet>> {
        self.sheet.as_ref().map(|(s, _)| s)
    }

    pub fn connect_sheet(&self) -> Option<&Entity<ConnectSheet>> {
        self.connect.as_ref().map(|(s, _)| s)
    }

    /// The connect sheet is drawn (not a quiet startup attempt).
    pub fn connect_sheet_shown(&self, cx: &App) -> bool {
        self.connect
            .as_ref()
            .is_some_and(|(sheet, _)| !sheet.read(cx).quiet)
    }

    /// A sheet is open over the panes.
    fn modal(&self) -> bool {
        self.sheet.is_some() || self.connect.is_some()
    }

    /// Opens the connect sheet (or brings it up), showing `reason`.
    pub fn open_connect_sheet(
        &mut self,
        reason: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sheet = None;
        if let Some((sheet, _)) = &self.connect {
            let sheet = sheet.clone();
            sheet.update(cx, |s, cx| {
                if reason.is_some() {
                    s.reason = reason;
                }
                s.quiet = false;
                cx.notify();
            });
            sheet.read(cx).focus(window, cx);
            cx.notify();
            return;
        }
        let Some(sheet) = self.install_connect_sheet(reason, window, cx) else {
            return;
        };
        sheet.read(cx).focus(window, cx);
    }

    /// The launch: connects to `saved` (or a local fleetd that answers),
    /// showing the connect sheet if that takes a while or fails.
    pub fn start_up(&mut self, saved: Option<Target>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sheet) = self.install_connect_sheet(None, window, cx) {
            sheet.update(cx, |s, cx| s.startup(saved, cx));
        }
    }

    /// [`Workspace::start_up`] for a connect that started before the
    /// window opened.
    pub fn start_up_from(
        &mut self,
        attempt: StartupAttempt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(sheet) = self.install_connect_sheet(None, window, cx) {
            sheet.update(cx, |s, cx| s.startup_from(attempt, cx));
        }
    }

    fn install_connect_sheet(
        &mut self,
        reason: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ConnectSheet>> {
        let sheet = new_sheet(reason, cx)?;
        let events = cx.subscribe_in(&sheet, window, Self::connect_event);
        // A quiet startup sheet appearing takes the keyboard.
        let shown = cx.observe_in(&sheet, window, |_, sheet, window, cx| {
            let visible = !sheet.read(cx).quiet;
            let field = sheet.read(cx).remote.read(cx).focus_handle().clone();
            if visible && !field.contains_focused(window, cx) {
                window.focus(&field);
            }
            cx.notify();
        });
        self.connect = Some((sheet.clone(), [events, shown]));
        cx.notify();
        Some(sheet)
    }

    fn connect_event(
        &mut self,
        _sheet: &Entity<ConnectSheet>,
        event: &ConnectEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ConnectEvent::Connected { target, save } => {
                if *save {
                    let dir = self.app.read(cx).state_dir().to_owned();
                    if let Err(e) = save_connection(&dir, target) {
                        eprintln!("fleet-gui: cannot save the connection: {e}");
                    }
                }
                switch_target(target.clone(), cx);
            }
            ConnectEvent::Dismiss => {}
        }
        self.connect = None;
        self.focus_current(window, cx);
        cx.notify();
    }

    fn pane_focus_handle(&self, pane: PaneId, cx: &App) -> Option<FocusHandle> {
        let app = self.app.read(cx);
        app.pane_view(pane)
            .map(|v| v.read(cx).focus_handle().clone())
    }

    fn focus_pane(&self, pane: PaneId, window: &mut Window, cx: &mut App) {
        self.app.update(cx, |a, cx| a.set_focused(pane, cx));
        if let Some(handle) = self.pane_focus_handle(pane, cx) {
            window.focus(&handle);
        }
    }

    fn focus_current(&self, window: &mut Window, cx: &mut App) {
        let pane = self.app.read(cx).focused();
        self.focus_pane(pane, window, cx);
    }

    fn split(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal() {
            return;
        }
        if self.app.update(cx, |a, cx| a.split(axis, cx)).is_some() {
            self.focus_current(window, cx);
        }
    }

    fn close_pane(&mut self, _: &ClosePane, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal() {
            return;
        }
        self.app.update(cx, |a, cx| a.close_focused(cx));
        self.focus_current(window, cx);
    }

    fn focus_dir(&mut self, dir: Dir, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal() {
            return;
        }
        if let Some(pane) = self.app.read(cx).neighbour(dir) {
            self.focus_pane(pane, window, cx);
        }
    }

    fn focus_number(&mut self, n: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal() {
            return;
        }
        if let Some(pane) = self.app.read(cx).pane_by_number(n) {
            self.focus_pane(pane, window, cx);
        }
    }

    /// Shows the agent in the focused pane (a sidebar click).
    pub fn show_agent(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.app.update(cx, |a, cx| a.show_agent(agent_id, cx));
        self.focus_current(window, cx);
    }

    /// The `input` bench's panes: `agent` in the focused pane (returned)
    /// and each of `extra` in a new split; the focus returns to the first.
    pub(super) fn bench_open(
        &mut self,
        agent: u64,
        extra: &[u64],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PaneId {
        let pane = self.app.read(cx).focused();
        self.show_agent(agent, window, cx);
        for &id in extra {
            self.app.update(cx, |a, cx| a.open_in_new_split(id, cx));
        }
        self.focus_pane(pane, window, cx);
        pane
    }

    /// The sidebar row's native context menu (at the right-click being
    /// handled).
    pub fn open_row_menu(&mut self, agent_id: u64, cx: &mut Context<Self>) {
        let items = self.menu_items(agent_id, cx);
        let lines = context_menu::lines(&items, |item| MenuLine {
            title: item.title().to_owned(),
            enabled: true,
            shortcut: None,
        });
        let epoch = self.epoch;
        context_menu::show(lines, cx, move |index, cx| {
            if let Some(MenuEntry::Item(item)) = items.get(index).copied() {
                with_workspace(cx, |ws, window, cx| {
                    ws.row_menu_pick(agent_id, item, epoch, window, cx);
                });
            }
        });
    }

    /// An item picked from a row's menu opened in session `epoch`.
    pub(crate) fn row_menu_pick(
        &mut self,
        agent_id: u64,
        item: RowMenuItem,
        epoch: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if epoch != self.epoch || self.modal() {
            return;
        }
        // The agent may have changed since the menu opened (a Kill for an
        // agent that exited meanwhile): only what the menu offers now.
        let offered = self.menu_items(agent_id, cx);
        if !offered.contains(&MenuEntry::Item(item)) {
            return;
        }
        match item {
            RowMenuItem::OpenSplit => self.open_menu_split(agent_id, window, cx),
            RowMenuItem::Kill => self.kill(&Kill { agent_id }, window, cx),
            RowMenuItem::Restart => self.restart(&Restart { agent_id }, window, cx),
            RowMenuItem::Remove => self.remove(&Remove { agent_id }, window, cx),
        }
    }

    /// A pane's native context menu: on its header, or on its grid
    /// (`on_grid`, adding Copy and Paste).
    fn open_pane_menu(&mut self, pane: PaneId, on_grid: bool, cx: &mut Context<Self>) {
        let state = {
            let app = self.app.read(cx);
            let view = app.pane_view(pane).map(|v| v.read(cx));
            PaneMenuState {
                on_grid,
                has_selection: view.is_some_and(|v| v.has_selection()),
                can_split: app.layout.panes().len() < MAX_PANES,
                maximized: app.maximized == Some(pane),
                restartable: view.is_some_and(|v| v.restartable()),
            }
        };
        let items = pane_menu(state);
        let lines = context_menu::lines(&items, |(item, enabled)| MenuLine {
            title: item.title().to_owned(),
            enabled,
            shortcut: item.shortcut(),
        });
        let epoch = self.epoch;
        context_menu::show(lines, cx, move |index, cx| {
            if let Some(MenuEntry::Item((item, true))) = items.get(index).copied() {
                with_workspace(cx, |ws, window, cx| {
                    ws.pane_menu_pick(pane, item, epoch, window, cx);
                });
            }
        });
    }

    /// An item picked from `pane`'s menu opened in session `epoch`.
    pub(crate) fn pane_menu_pick(
        &mut self,
        pane: PaneId,
        item: PaneMenuItem,
        epoch: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if epoch != self.epoch || self.modal() {
            return;
        }
        let event = match item {
            PaneMenuItem::Copy | PaneMenuItem::Paste => {
                self.focus_pane(pane, window, cx);
                let action: Box<dyn Action> = if item == PaneMenuItem::Copy {
                    Box::new(Copy)
                } else {
                    Box::new(Paste)
                };
                window.dispatch_action(action, cx);
                return;
            }
            PaneMenuItem::SplitRight => PaneEvent::SplitRight,
            PaneMenuItem::SplitDown => PaneEvent::SplitDown,
            PaneMenuItem::Maximize | PaneMenuItem::Restore => PaneEvent::ToggleMaximize,
            PaneMenuItem::Restart => PaneEvent::Restart,
            PaneMenuItem::Close => PaneEvent::Close,
        };
        self.pane_event(pane, event, window, cx);
    }

    pub fn open_new_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connect.is_some() {
            return;
        }
        if let Some((sheet, _)) = &self.sheet {
            sheet.read(cx).focus(window, cx);
            return;
        }
        let app = self.app.read(cx);
        let dirs = app.dir_choices();
        let target = app.target.clone();
        let command = app.config.default_command.clone();
        let default_dir = app.config.default_dir.clone();
        let sheet = cx.new(|cx| {
            let s = NewAgentSheet::new(&command, dirs, &target, cx);
            // Settings › Agents: a default folder beats the recent one.
            if !default_dir.is_empty() {
                s.dir.update(cx, |f, cx| f.set_text(default_dir, cx));
            }
            s
        });
        let sub = cx.subscribe_in(&sheet, window, Self::sheet_event);
        sheet.read(cx).focus(window, cx);
        self.sheet = Some((sheet, sub));
        cx.notify();
    }

    fn new_agent(&mut self, _: &NewAgent, window: &mut Window, cx: &mut Context<Self>) {
        self.open_new_agent(window, cx);
    }

    /// ⌘T: a plain login shell, focused (see [`AppState::new_terminal`]).
    pub fn open_new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal() {
            return;
        }
        if let Err(e) = self.app.update(cx, |a, cx| a.new_terminal(cx)) {
            self.app.update(cx, |a, cx| a.show_error(e, cx));
        }
        self.focus_current(window, cx);
        cx.notify();
    }

    fn new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        self.open_new_terminal(window, cx);
    }

    fn sheet_event(
        &mut self,
        sheet: &Entity<NewAgentSheet>,
        event: &SheetEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SheetEvent::Cancel => {}
            SheetEvent::Spawn(req) => {
                let result = self.app.update(cx, |a, cx| a.spawn(req.clone(), cx));
                if let Err(e) = result {
                    sheet.update(cx, |s, cx| s.set_error(e, cx));
                    return;
                }
            }
        }
        self.sheet = None;
        self.focus_current(window, cx);
        cx.notify();
    }

    /// Asks before killing: only Kill kills.
    fn kill(&mut self, action: &Kill, window: &mut Window, cx: &mut Context<Self>) {
        let agent_id = action.agent_id;
        // Settings › Behaviour can turn the question off.
        if !self.app.read(cx).config().confirm_kill {
            self.app.read(cx).kill(agent_id);
            cx.notify();
            return;
        }
        let label = self.app.read(cx).label(agent_id);
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Kill {label}?"),
            Some("Its process is stopped. Closing a pane only detaches."),
            &[PromptButton::new("Kill"), PromptButton::cancel("Cancel")],
            cx,
        );
        let app = self.app.clone();
        cx.spawn(async move |_, cx| {
            if answer.await == Ok(0) {
                let _ = app.update(cx, |a, _| a.kill(agent_id));
            }
        })
        .detach();
        cx.notify();
    }

    /// Restarts an ended agent from the row menu: no confirmation.
    fn restart(&mut self, action: &Restart, _: &mut Window, cx: &mut Context<Self>) {
        self.app.read(cx).restart(action.agent_id);
        cx.notify();
    }

    /// Forgets an ended agent from the row menu: no confirmation.
    fn remove(&mut self, action: &Remove, _: &mut Window, cx: &mut Context<Self>) {
        self.app.read(cx).forget(action.agent_id);
        cx.notify();
    }

    /// The sidebar's Clear exited: forgets every ended agent.
    pub fn clear_exited(&mut self, cx: &mut Context<Self>) {
        if self.app.read(cx).ended_count() > 0 {
            self.app.read(cx).clear_exited();
        }
    }

    /// ⌘R: restarts the focused pane's agent in place if it ended.
    fn restart_pane(&mut self, _: &RestartPane, _: &mut Window, cx: &mut Context<Self>) {
        if self.modal() {
            return;
        }
        self.app.update(cx, |a, cx| a.restart_focused(cx));
    }

    fn open_menu_split(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .app
            .update(cx, |a, cx| a.open_in_new_split(agent_id, cx))
            .is_some()
        {
            self.focus_current(window, cx);
        }
        cx.notify();
    }

    /// Keeps the app's focused pane in step with clicks into panes.
    fn follow_pane_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let views: Vec<(PaneId, Entity<PaneView>)> = {
            let app = self.app.read(cx);
            app.layout
                .panes()
                .into_iter()
                .filter_map(|p| Some((p, app.pane_view(p)?.clone())))
                .collect()
        };
        // A pane id can get a new view (closing the last pane, or its
        // agent removed): subscriptions follow the view, not the id, and
        // the keyboard follows a focused view's replacement.
        let mut refocus = false;
        self.pane_focus.retain(|p, (id, handle, _)| {
            let kept = views
                .iter()
                .any(|(pane, view)| pane == p && view.entity_id() == *id);
            refocus |= !kept && handle.is_focused(window);
            kept
        });
        if refocus {
            cx.defer_in(window, |this, window, cx| this.focus_current(window, cx));
        }
        for (pane, view) in views {
            if self.pane_focus.contains_key(&pane) {
                continue;
            }
            let handle = view.read(cx).focus_handle().clone();
            let focus = cx.on_focus_in(&handle, window, move |this, _, cx| {
                this.app.update(cx, |a, cx| a.set_focused(pane, cx));
            });
            let events = cx.subscribe_in(&view, window, move |this, _, event, window, cx| {
                this.pane_event(pane, *event, window, cx);
            });
            self.pane_focus
                .insert(pane, (view.entity_id(), handle, [focus, events]));
        }
    }

    /// A pane's header button: acts on that pane.
    fn pane_event(
        &mut self,
        pane: PaneId,
        event: PaneEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal() {
            return;
        }
        self.focus_pane(pane, window, cx);
        match event {
            PaneEvent::SplitRight => self.split(Axis::Horizontal, window, cx),
            PaneEvent::SplitDown => self.split(Axis::Vertical, window, cx),
            PaneEvent::ToggleMaximize => self.toggle_maximize(&ToggleMaximize, window, cx),
            PaneEvent::Close => self.close_pane(&ClosePane, window, cx),
            PaneEvent::Restart => self.restart_pane(&RestartPane, window, cx),
            PaneEvent::ContextMenu { on_grid } => self.open_pane_menu(pane, on_grid, cx),
        }
    }

    fn toggle_maximize(&mut self, _: &ToggleMaximize, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal() {
            return;
        }
        self.app.update(cx, |a, cx| a.toggle_maximize(cx));
        self.focus_current(window, cx);
    }

    /// The pane cards, [`chrome::PANE_GAP`] apart and from the edges;
    /// only the maximized one when a pane is maximized (the others stay
    /// attached).
    fn render_panes(&mut self, cx: &App) -> impl IntoElement + use<> {
        let app = self.app.read(cx);
        let rects = match app.maximized {
            Some(pane) => vec![(
                pane,
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 1.0,
                    h: 1.0,
                },
            )],
            None => app.layout.rects(1.0, 1.0),
        };
        self.drawn = rects.iter().map(|(p, _)| *p).collect();
        let children: Vec<_> = rects
            .into_iter()
            .filter_map(|(pane, r)| {
                let view = app.pane_view(pane)?.clone();
                Some(
                    div()
                        .debug_selector(move || format!("pane-card-{}", pane.0))
                        .absolute()
                        .left(relative(r.x))
                        .top(relative(r.y))
                        .w(relative(r.w))
                        .h(relative(r.h))
                        .p(px(chrome::PANE_GAP / 2.0))
                        .child(view),
                )
            })
            .collect();
        div().relative().flex_1().h_full().min_w_0().child(
            div()
                .absolute()
                .inset(px(chrome::PANE_GAP / 2.0))
                .children(children),
        )
    }

    /// The row menu for `agent_id`: Open in New Split (while a pane can
    /// be added), then Kill for a running agent, or Restart and Remove for
    /// one that exited or is lost.
    fn menu_items(&self, agent_id: u64, cx: &App) -> Vec<MenuEntry<RowMenuItem>> {
        let app = self.app.read(cx);
        let is_ended = app.agents.iter().any(|r| r.info.id == agent_id && ended(r));
        row_menu(app.layout.panes().len() < MAX_PANES, is_ended)
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.follow_pane_focus(window, cx);
        let theme = self.app.read(cx).theme;
        let (rows, home, error, notice, titles, marks, update) = {
            let app = self.app.read(cx);
            (
                app.agents.clone(),
                app.home.clone(),
                app.error.clone(),
                app.notice.clone(),
                app.titles.clone(),
                app.tab_marks(),
                app.update_button(),
            )
        };
        let mark = move |id: u64| marks.get(&id).copied().unwrap_or(TabMark::None);
        let data = SidebarData {
            rows: &rows,
            home: &home,
            now_ms: now_ms(),
            titles: &titles,
            mark: &mark,
        };
        let sidebar = render_sidebar(&data, &theme, cx);
        let ended_rows = rows.iter().filter(|r| ended(r)).count();
        let bar = {
            let app = self.app.read(cx);
            let link = if !app.is_live() {
                Link::Connecting
            } else if app.is_reconnecting() {
                Link::Reconnecting
            } else {
                Link::Live
            };
            let title = bar_title(app.target(), link, rows.len() - ended_rows, ended_rows);
            let window_title = window_title(&title);
            if self.window_title != window_title {
                window.set_window_title(&window_title);
                self.window_title = window_title;
            }
            BarData {
                title,
                ended: ended_rows,
                fullscreen: window.is_fullscreen(),
                update,
            }
        };
        let bar = render_bar(&bar, &theme, cx);
        super::bench::sidebar_drawn(window);
        let panes = self.render_panes(cx);
        let reduce_motion = self.reduce_motion;
        // An error wins over a neutral notice; the notice has no red border.
        let is_error = error.is_some();
        let toast = error.or(notice).map(|e| {
            let card = div()
                .id("error")
                .debug_selector(|| "error-toast".to_owned())
                .max_w(px(480.))
                .px(px(space::MD))
                .py(px(space::SM))
                .rounded(px(radius::MD))
                .border_1()
                .border_color(if is_error {
                    theme.danger.opacity(0.6)
                } else {
                    theme.border
                })
                .bg(theme.raised)
                .shadow_lg()
                .text_color(theme.text)
                .type_style(text::CALLOUT)
                .occlude()
                .cursor_pointer()
                .child(SharedString::from(format!("{e}   ✕")))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.app.update(cx, |a, cx| a.dismiss_error(cx));
                }));
            div()
                .absolute()
                .bottom(px(space::LG))
                .right(px(space::LG))
                .child(appear(
                    div().child(card),
                    "toast-appear",
                    motion::SHEET_OFFSET,
                    motion::TOAST_MS,
                    reduce_motion,
                ))
        });
        let connect = self
            .connect
            .as_ref()
            .filter(|_| self.connect_sheet_shown(cx))
            .map(|(sheet, _)| {
                deferred(
                    div()
                        .id("connect-backdrop")
                        .absolute()
                        .inset_0()
                        .occlude()
                        .bg(gpui::black().opacity(0.35))
                        .flex()
                        // A card sized to its content, not a full-height column.
                        .items_start()
                        .justify_center()
                        .pt(px(chrome::BAR_HEIGHT + space::XL))
                        .child(sheet_drag_strip("connect-drag-strip"))
                        .child(appear(
                            div().child(sheet.clone()),
                            "connect-appear",
                            -motion::SHEET_OFFSET,
                            motion::SHEET_MS,
                            reduce_motion,
                        )),
                )
                .with_priority(3)
            });
        let sheet = self.sheet.as_ref().map(|(sheet, _)| {
            // The backdrop takes every click and wheel event over the panes.
            deferred(
                div()
                    .id("sheet-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_start()
                    .justify_center()
                    // Hung from the bar, like an AppKit sheet.
                    .pt(px(chrome::BAR_HEIGHT + space::SM))
                    .child(sheet_drag_strip("sheet-drag-strip"))
                    .child(appear(
                        div().child(sheet.clone()),
                        "sheet-appear",
                        -motion::SHEET_OFFSET,
                        motion::SHEET_MS,
                        reduce_motion,
                    )),
            )
            .with_priority(2)
        });
        div()
            .id("workspace")
            .key_context(WORKSPACE_CONTEXT)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme.window)
            .text_color(theme.text)
            .on_action(cx.listener(Self::toggle_maximize))
            .on_action(
                cx.listener(|this, _: &SplitRight, w, cx| this.split(Axis::Horizontal, w, cx)),
            )
            .on_action(cx.listener(|this, _: &SplitDown, w, cx| this.split(Axis::Vertical, w, cx)))
            .on_action(cx.listener(Self::close_pane))
            .on_action(cx.listener(|this, _: &FocusLeft, w, cx| this.focus_dir(Dir::Left, w, cx)))
            .on_action(cx.listener(|this, _: &FocusRight, w, cx| this.focus_dir(Dir::Right, w, cx)))
            .on_action(cx.listener(|this, _: &FocusUp, w, cx| this.focus_dir(Dir::Up, w, cx)))
            .on_action(cx.listener(|this, _: &FocusDown, w, cx| this.focus_dir(Dir::Down, w, cx)))
            .on_action(cx.listener(|this, _: &FocusPane1, w, cx| this.focus_number(1, w, cx)))
            .on_action(cx.listener(|this, _: &FocusPane2, w, cx| this.focus_number(2, w, cx)))
            .on_action(cx.listener(|this, _: &FocusPane3, w, cx| this.focus_number(3, w, cx)))
            .on_action(cx.listener(|this, _: &FocusPane4, w, cx| this.focus_number(4, w, cx)))
            .on_action(cx.listener(Self::new_agent))
            .on_action(cx.listener(Self::new_terminal))
            .on_action(cx.listener(Self::kill))
            .on_action(cx.listener(Self::restart))
            .on_action(cx.listener(Self::remove))
            .on_action(cx.listener(Self::restart_pane))
            .child(bar)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(sidebar)
                    .child(panes),
            )
            .children(toast)
            .children(sheet)
            .children(connect)
    }
}

/// The theme for the window's appearance (or the one `[gui] theme`
/// picks), its tint at `[gui] window_opacity`, in the user's accent
/// colour. Reduce Transparency makes it opaque.
fn window_theme(window: &Window, config: &GuiConfig) -> Theme {
    let appearance = config.theme.resolve(appearance_of(window.appearance()));
    let tokens = Tokens::for_appearance(appearance).with_window_opacity(config.window_opacity);
    let theme = if native::reduce_transparency() {
        Theme::from_tokens(&tokens.opaque())
    } else {
        Theme::from_tokens(&tokens)
    };
    match native::accent_color() {
        Some((r, g, b)) => theme.with_accent(gpui::Rgba { r, g, b, a: 1.0 }.into()),
        None => theme,
    }
}

/// Runs `f` on the workspace window, if it is open (a context menu's
/// pick arrives with only the app).
fn with_workspace(
    cx: &mut App,
    f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>),
) {
    if let Some(handle) = AppState::global(cx).and_then(|a| a.read(cx).window) {
        let _ = handle.update(cx, f);
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use fleet_proto::{AgentInfo, AgentState, AttachOk};
    use fleet_term::Palette;
    use gpui::{
        AppContext, Entity, Modifiers, MouseButton, Point, ScrollDelta, ScrollWheelEvent,
        TestAppContext, TouchPhase, VisualTestContext, point,
    };

    use super::Workspace;
    use crate::app::app_state::{AppState, SAVE_DEBOUNCE, SessionApi};
    use crate::app::keymap::{Kill, bind_keys};
    use crate::app::pane_view::InputSink;
    use crate::app::sidebar_view::TabMark;
    use crate::app::theme::Theme;
    use crate::core::config::GuiConfig;
    use crate::core::layout::Layout;
    use crate::core::menus::{MenuEntry, PaneMenuItem, RowMenuItem};
    use crate::core::pane::PaneModel;
    use crate::core::session::{AgentRow, ConnState, PaneId, UiEvent};
    use crate::core::spawn::SpawnRequest;
    use crate::core::state::GuiState;
    use crate::core::target::Target;

    #[derive(Clone, Debug, PartialEq)]
    enum Call {
        Open(PaneId, Option<u64>),
        Attach(PaneId, u64),
        Close(PaneId),
        Kill(u64),
        Restart(u64),
        RestartInPane {
            pane: PaneId,
            agent_id: u64,
            size: (u16, u16),
        },
        Forget(Vec<u64>),
        ClearExited,
        Spawn {
            req: SpawnRequest,
            size: (u16, u16),
            pane: PaneId,
        },
    }

    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<Call>>,
        inputs: Mutex<Vec<Vec<u8>>>,
        agents: Mutex<Vec<AgentRow>>,
        models: Mutex<HashMap<PaneId, Arc<Mutex<PaneModel>>>>,
        spawn_error: Mutex<Option<String>>,
        listed: std::sync::atomic::AtomicBool,
        lists: std::sync::atomic::AtomicU64,
        replaced: Mutex<HashMap<u64, u64>>,
        update_available: std::sync::atomic::AtomicBool,
        daemon_restarts: std::sync::atomic::AtomicUsize,
        restart_pending: std::sync::atomic::AtomicBool,
    }

    impl Fake {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn model(&self, pane: PaneId) -> Arc<Mutex<PaneModel>> {
            self.models.lock().unwrap()[&pane].clone()
        }
    }

    impl InputSink for Fake {
        fn input(&self, _pane: PaneId, bytes: Vec<u8>) {
            self.inputs.lock().unwrap().push(bytes);
        }

        fn resize(&self, _pane: PaneId, _cols: u16, _rows: u16) {}
    }

    impl SessionApi for Fake {
        fn agents(&self) -> Vec<AgentRow> {
            self.agents.lock().unwrap().clone()
        }

        fn listed(&self) -> bool {
            self.listed.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn lists(&self) -> u64 {
            self.lists.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn update_available(&self) -> bool {
            self.update_available
                .load(std::sync::atomic::Ordering::SeqCst)
        }

        fn restart_daemon(&self) {
            self.daemon_restarts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.restart_pending
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn restart_pending(&self) -> bool {
            self.restart_pending
                .load(std::sync::atomic::Ordering::SeqCst)
        }

        fn spawn(
            &self,
            req: SpawnRequest,
            cols: u16,
            rows: u16,
            pane: PaneId,
        ) -> Result<(), String> {
            if let Some(e) = self.spawn_error.lock().unwrap().clone() {
                return Err(e);
            }
            self.calls.lock().unwrap().push(Call::Spawn {
                req,
                size: (cols, rows),
                pane,
            });
            Ok(())
        }

        fn kill(&self, agent_id: u64) {
            self.calls.lock().unwrap().push(Call::Kill(agent_id));
        }

        fn restart(&self, agent_id: u64) {
            self.calls.lock().unwrap().push(Call::Restart(agent_id));
        }

        fn restart_in_pane(
            &self,
            pane: PaneId,
            agent_id: u64,
            cols: u16,
            rows: u16,
        ) -> Result<(), String> {
            self.calls.lock().unwrap().push(Call::RestartInPane {
                pane,
                agent_id,
                size: (cols, rows),
            });
            Ok(())
        }

        fn forget(&self, ids: Vec<u64>) {
            self.calls.lock().unwrap().push(Call::Forget(ids));
        }

        fn clear_exited(&self) {
            self.calls.lock().unwrap().push(Call::ClearExited);
        }

        fn replaced(&self) -> HashMap<u64, u64> {
            self.replaced.lock().unwrap().clone()
        }

        fn open_pane(&self, pane: PaneId, agent_id: Option<u64>) -> Arc<Mutex<PaneModel>> {
            self.calls.lock().unwrap().push(Call::Open(pane, agent_id));
            let model = Arc::new(Mutex::new(PaneModel::new(Theme::dark().palette)));
            self.models.lock().unwrap().insert(pane, model.clone());
            model
        }

        fn attach(&self, pane: PaneId, agent_id: u64) {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Attach(pane, agent_id));
        }

        fn close_pane(&self, pane: PaneId) {
            self.calls.lock().unwrap().push(Call::Close(pane));
        }

        fn set_palette(&self, _palette: Palette) {}
    }

    fn ended(id: u64, cwd: &str, created: i64, state: AgentState) -> AgentRow {
        let mut r = row(id, cwd, created);
        r.info.set_state(state);
        if state == AgentState::Exited {
            r.info.exit_code = Some(3);
        }
        r
    }

    fn row(id: u64, cwd: &str, created: i64) -> AgentRow {
        let mut info = AgentInfo {
            id,
            program: "sh".into(),
            cwd: cwd.into(),
            created_unix_ms: created,
            ..Default::default()
        };
        info.set_state(AgentState::Running);
        AgentRow {
            info,
            last_line: format!("line of {id}"),
            last_output_unix_ms: None,
        }
    }

    struct Setup<'a> {
        ws: Entity<Workspace>,
        app: Entity<AppState>,
        cx: &'a mut VisualTestContext,
        ui: async_channel::Sender<UiEvent>,
    }

    fn app_state(cx: &mut TestAppContext, fake: &Arc<Fake>, dir: &Path) -> Entity<AppState> {
        let (_tx, rx) = async_channel::unbounded();
        let session: Arc<dyn SessionApi> = fake.clone();
        let dir = dir.to_owned();
        cx.new(|cx| {
            AppState::new(
                session,
                rx,
                Target::Local,
                dir,
                GuiConfig::default(),
                Theme::dark(),
                cx,
            )
        })
    }

    fn setup<'a>(cx: &'a mut TestAppContext, fake: &Arc<Fake>, dir: &Path) -> Setup<'a> {
        setup_with(cx, fake, dir, GuiConfig::default())
    }

    fn setup_with<'a>(
        cx: &'a mut TestAppContext,
        fake: &Arc<Fake>,
        dir: &Path,
        config: GuiConfig,
    ) -> Setup<'a> {
        cx.update(bind_keys);
        let (ui, rx) = async_channel::unbounded();
        let session: Arc<dyn SessionApi> = fake.clone();
        let dir = dir.to_owned();
        let app =
            cx.new(|cx| AppState::new(session, rx, Target::Local, dir, config, Theme::dark(), cx));
        cx.update(|cx| AppState::install(app.clone(), cx));
        let state = app.clone();
        let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(state, window, cx));
        // Focus events need an active window.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        Setup { ws, app, cx, ui }
    }

    fn panes(s: &mut Setup<'_>) -> Vec<PaneId> {
        s.app.read_with(s.cx, |a, _| a.layout().panes())
    }

    fn focused(s: &mut Setup<'_>) -> PaneId {
        s.app.read_with(s.cx, |a, _| a.focused())
    }

    fn center(s: &mut Setup<'_>, selector: &'static str) -> Point<gpui::Pixels> {
        s.cx.run_until_parked();
        s.cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"))
            .center()
    }

    fn click(s: &mut Setup<'_>, selector: &'static str) {
        let at = center(s, selector);
        s.cx.simulate_click(at, Modifiers::none());
        s.cx.run_until_parked();
    }

    fn agents(s: &mut Setup<'_>, fake: &Fake, rows: Vec<AgentRow>) {
        *fake.agents.lock().unwrap() = rows;
        s.ui.try_send(UiEvent::AgentsChanged).unwrap();
        s.cx.run_until_parked();
    }

    #[gpui::test]
    fn agents_listed_before_the_window_are_in_its_first_frame(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let app = app_state(cx, &fake, dir.path());
        assert!(!app.read_with(cx, |a, _| a.has_agents()));
        // The list arrives while the window opens, its event not yet applied.
        *fake.agents.lock().unwrap() = vec![row(1, "/a", 1), row(2, "/b", 2)];
        fake.listed.store(true, std::sync::atomic::Ordering::SeqCst);
        let state = app.clone();
        let (_ws, cx) = cx.add_window_view(|window, cx| Workspace::new(state, window, cx));
        let ids: Vec<u64> = app.read_with(cx, |a, _| a.agents.iter().map(|r| r.info.id).collect());
        assert_eq!(ids, [1, 2]);
        cx.run_until_parked();
        assert!(cx.debug_bounds("tab-1").is_some() && cx.debug_bounds("tab-2").is_some());
    }

    #[gpui::test]
    fn cmd_d_splits_and_cmd_w_detaches(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        assert_eq!(panes(&mut s).len(), 1);
        s.cx.simulate_keystrokes("cmd-d");
        let p = panes(&mut s);
        assert_eq!(p, [PaneId(1), PaneId(2)]);
        assert_eq!(focused(&mut s), PaneId(2));
        s.cx.simulate_keystrokes("cmd-w");
        assert_eq!(panes(&mut s), [PaneId(1)]);
        let calls = fake.calls();
        assert!(calls.contains(&Call::Close(PaneId(2))), "{calls:?}");
        assert!(!calls.iter().any(|c| matches!(c, Call::Kill(_))));
        // cmd-shift-d splits down.
        s.cx.simulate_keystrokes("cmd-shift-d");
        assert_eq!(panes(&mut s).len(), 2);
    }

    #[gpui::test]
    fn fifth_split_is_refused(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        s.cx.simulate_keystrokes("cmd-d cmd-shift-d cmd-d");
        assert_eq!(panes(&mut s).len(), 4);
        let opens = |f: &Fake| {
            f.calls()
                .iter()
                .filter(|c| matches!(c, Call::Open(..)))
                .count()
        };
        let before = opens(&fake);
        s.cx.simulate_keystrokes("cmd-d");
        assert_eq!(panes(&mut s).len(), 4);
        assert_eq!(opens(&fake), before);
    }

    #[gpui::test]
    fn cmd_number_focuses_pane(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        s.cx.simulate_keystrokes("cmd-d cmd-shift-d");
        let p = panes(&mut s);
        assert_eq!(p.len(), 3);
        for (n, pane) in p.iter().enumerate() {
            s.cx.simulate_keystrokes(&format!("cmd-{}", n + 1));
            assert_eq!(focused(&mut s), *pane);
            let app = s.app.clone();
            let has_focus = s.cx.update(|window, cx| {
                let view = app.read(cx).pane_view(*pane).unwrap().clone();
                view.read(cx).focus_handle().is_focused(window)
            });
            assert!(has_focus, "pane {n} has keyboard focus");
        }
        // Arrows move between neighbours: 1 | (2 over 3).
        s.cx.simulate_keystrokes("cmd-1 cmd-alt-right");
        assert_eq!(focused(&mut s), p[1]);
        s.cx.simulate_keystrokes("cmd-alt-down");
        assert_eq!(focused(&mut s), p[2]);
        s.cx.simulate_keystrokes("cmd-4");
        assert_eq!(focused(&mut s), p[2]);
    }

    #[gpui::test]
    fn clicking_sidebar_row_attaches_in_focused_pane(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(7, "/w", 1)]);
        s.cx.simulate_keystrokes("cmd-d cmd-1");
        click(&mut s, "tab-7");
        assert!(fake.calls().contains(&Call::Attach(PaneId(1), 7)));
        let shown = s.app.read_with(s.cx, |a, _| a.layout().agent(PaneId(1)));
        assert_eq!(shown, Some(7));
        assert_eq!(focused(&mut s), PaneId(1));
    }

    #[gpui::test]
    fn clicking_the_shown_agent_reattaches_a_pane_not_showing_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(7, "/w", 1)]);
        let attaches = |f: &Fake| {
            f.calls()
                .iter()
                .filter(|c| **c == Call::Attach(PaneId(1), 7))
                .count()
        };
        click(&mut s, "tab-7");
        assert_eq!(attaches(&fake), 1);
        // The layout names agent 7 but the pane does not show it (a failed
        // spawn left it attaching): clicking attaches again.
        click(&mut s, "tab-7");
        assert_eq!(attaches(&fake), 2);
        // Shown: clicking again does nothing.
        attach_model(&fake, PaneId(1), 7);
        click(&mut s, "tab-7");
        assert_eq!(attaches(&fake), 2);
    }

    #[gpui::test]
    fn kill_requires_confirmation(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let s = setup(cx, &fake, dir.path());
        s.cx.dispatch_action(Kill { agent_id: 7 });
        assert!(s.cx.has_pending_prompt());
        s.cx.simulate_prompt_answer("Cancel");
        s.cx.run_until_parked();
        assert!(!fake.calls().iter().any(|c| matches!(c, Call::Kill(_))));

        s.cx.dispatch_action(Kill { agent_id: 7 });
        s.cx.simulate_prompt_answer("Kill");
        s.cx.run_until_parked();
        let kills: Vec<_> = fake
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Kill(_)))
            .collect();
        assert_eq!(kills, [Call::Kill(7)]);
    }

    #[gpui::test]
    fn kill_asks_nothing_when_settings_turn_confirmation_off(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let config = GuiConfig {
            confirm_kill: false,
            ..GuiConfig::default()
        };
        let s = setup_with(cx, &fake, dir.path(), config);
        s.cx.dispatch_action(Kill { agent_id: 7 });
        s.cx.run_until_parked();
        assert!(!s.cx.has_pending_prompt());
        assert!(fake.calls().contains(&Call::Kill(7)));
    }

    #[gpui::test]
    fn new_agent_starts_in_the_default_folder(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut state = GuiState::default();
        state.recent_dirs.insert("local".into(), vec!["/r1".into()]);
        state.save(dir.path()).unwrap();
        let fake = Arc::new(Fake::default());
        let config = GuiConfig {
            default_dir: "/projects".into(),
            ..GuiConfig::default()
        };
        let s = setup_with(cx, &fake, dir.path(), config);
        s.cx.simulate_keystrokes("cmd-n");
        let sheet =
            s.ws.read_with(s.cx, |w, _| w.sheet().cloned())
                .expect("sheet open");
        let (dir_text, dirs) = sheet.read_with(s.cx, |sh, cx| {
            (sh.dir.read(cx).text().to_owned(), sh.dirs.clone())
        });
        assert_eq!(dir_text, "/projects");
        // The recent ones are still offered.
        assert_eq!(dirs, ["/r1"]);
    }

    #[gpui::test]
    fn new_agent_prefills_default_command(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut state = GuiState::default();
        state
            .recent_dirs
            .insert("local".into(), vec!["/r1".into(), "/a".into()]);
        state.save(dir.path()).unwrap();
        let fake = Arc::new(Fake::default());
        let config = GuiConfig {
            default_command: vec![
                "claude".into(),
                "--append-system-prompt".into(),
                "be brief".into(),
            ],
            ..GuiConfig::default()
        };
        let mut s = setup_with(cx, &fake, dir.path(), config);
        agents(
            &mut s,
            &fake,
            vec![row(1, "/a", 1), row(2, "/b", 3), row(3, "/c", 2)],
        );
        s.cx.simulate_keystrokes("cmd-n");
        let sheet =
            s.ws.read_with(s.cx, |w, _| w.sheet().cloned())
                .expect("sheet open");
        let (command, dirs, dir_text, can_choose) = sheet.read_with(s.cx, |sh, cx| {
            (
                sh.command.read(cx).text().to_owned(),
                sh.dirs.clone(),
                sh.dir.read(cx).text().to_owned(),
                sh.can_choose,
            )
        });
        // Shell-quoted, so the argument with a space survives the parse.
        assert_eq!(command, "claude --append-system-prompt 'be brief'");
        assert_eq!(dirs, ["/r1", "/a", "/b", "/c"]);
        assert_eq!(dir_text, "/r1");
        assert!(can_choose);

        s.cx.simulate_keystrokes("enter");
        assert!(fake.calls().contains(&Call::Spawn {
            req: SpawnRequest {
                program: "claude".into(),
                args: vec!["--append-system-prompt".into(), "be brief".into()],
                cwd: "/r1".into(),
                labels: Default::default(),
            },
            size: s.app.read_with(s.cx, |a, cx| {
                a.pane_view(PaneId(1)).unwrap().read(cx).grid().unwrap()
            }),
            pane: PaneId(1),
        }));

        // Again, typing a command with quoting into the command field.
        s.cx.simulate_keystrokes("cmd-n");
        let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
        let field = sheet.read_with(s.cx, |sh, _| sh.command.clone());
        field.update(s.cx, |f, cx| f.set_text(String::new(), cx));
        s.cx.simulate_input("sh -c 'echo hi'");
        let dir_field = sheet.read_with(s.cx, |sh, _| sh.dir.clone());
        dir_field.update(s.cx, |f, cx| f.set_text("/tmp".into(), cx));
        s.cx.simulate_keystrokes("enter");

        let size = s.app.read_with(s.cx, |a, cx| {
            a.pane_view(PaneId(1)).unwrap().read(cx).grid().unwrap()
        });
        assert!(fake.calls().contains(&Call::Spawn {
            req: SpawnRequest {
                program: "sh".into(),
                args: vec!["-c".into(), "echo hi".into()],
                cwd: "/tmp".into(),
                labels: Default::default(),
            },
            size,
            pane: PaneId(1),
        }));
        assert!(s.ws.read_with(s.cx, |w, _| w.sheet().is_none()));
        let recent = |s: &mut Setup<'_>| {
            s.app
                .read_with(s.cx, |a, _| a.gui_state().recent_dirs["local"].clone())
        };
        // Not yet: the spawn has not landed.
        assert_eq!(recent(&mut s), ["/r1", "/a"]);

        // The spawned agent lands in the pane: the layout records it.
        fake.model(PaneId(1))
            .lock()
            .unwrap()
            .on_attached(&AttachOk {
                agent_id: 9,
                cols: 80,
                rows: 24,
                primary: true,
                ..Default::default()
            });
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        let shown = s.app.read_with(s.cx, |a, _| a.layout().agent(PaneId(1)));
        assert_eq!(shown, Some(9));
        assert_eq!(recent(&mut s), ["/tmp", "/r1", "/a"]);
    }

    /// ⌘T: a login shell labelled `kind=shell`, in the last-used folder
    /// for an empty pane, then in the focused agent's folder, in a new
    /// split that takes the focus.
    #[gpui::test]
    fn new_terminal_spawns_a_shell_and_focuses_it(cx: &mut TestAppContext) {
        use crate::core::spawn::{KIND_LABEL, SHELL_KIND};
        let dir = tempfile::tempdir().unwrap();
        let mut state = GuiState::default();
        state.recent_dirs.insert("local".into(), vec!["/r1".into()]);
        state.save(dir.path()).unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup_with(cx, &fake, dir.path(), GuiConfig::default());
        let spawns = |fake: &Fake| -> Vec<(SpawnRequest, PaneId)> {
            fake.calls()
                .into_iter()
                .filter_map(|c| match c {
                    Call::Spawn { req, pane, .. } => Some((req, pane)),
                    _ => None,
                })
                .collect()
        };

        s.cx.simulate_keystrokes("cmd-t");
        let got = spawns(&fake);
        assert_eq!(got.len(), 1);
        let (req, pane) = &got[0];
        assert_eq!((req.cwd.as_str(), *pane), ("/r1", PaneId(1)));
        assert_eq!(req.args, ["-l"]);
        assert_eq!(req.labels[KIND_LABEL], SHELL_KIND);
        assert!(s.ws.read_with(s.cx, |w, _| w.sheet().is_none()));

        // Pane 1 now shows an agent in /a: the next one splits.
        agents(&mut s, &fake, vec![row(9, "/a", 1)]);
        attach_model(&fake, PaneId(1), 9);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        s.cx.simulate_keystrokes("cmd-t");
        let got = spawns(&fake);
        assert_eq!(got.len(), 2);
        let (req, pane) = &got[1];
        assert_eq!((req.cwd.as_str(), *pane), ("/a", PaneId(2)));
        let (focused, shown) = s
            .app
            .read_with(s.cx, |a, _| (a.focused(), a.layout().agent(PaneId(1))));
        assert_eq!((focused, shown), (PaneId(2), Some(9)));
    }

    fn spawn_via_sheet(s: &mut Setup<'_>, dir: &str) {
        s.cx.simulate_keystrokes("cmd-n");
        let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
        let field = sheet.read_with(s.cx, |sh, _| sh.dir.clone());
        let dir = dir.to_owned();
        field.update(s.cx, |f, cx| f.set_text(dir, cx));
        s.cx.simulate_keystrokes("enter");
    }

    fn attach_model(fake: &Fake, pane: PaneId, agent_id: u64) {
        fake.model(pane).lock().unwrap().on_attached(&AttachOk {
            agent_id,
            cols: 80,
            rows: 24,
            primary: true,
            ..Default::default()
        });
    }

    #[gpui::test]
    fn spawn_is_adopted_though_another_spawn_failed(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        spawn_via_sheet(&mut s, "/one");
        s.cx.simulate_keystrokes("cmd-d");
        spawn_via_sheet(&mut s, "/two");
        // Pane 1's spawn fails; the error is not pane-specific.
        s.ui.try_send(UiEvent::Error(
            "spawn failed: /one: no such directory".into(),
        ))
        .unwrap();
        s.cx.run_until_parked();
        // Pane 2's spawn lands afterwards.
        attach_model(&fake, PaneId(2), 9);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(2))).unwrap();
        s.cx.run_until_parked();
        let (one, two, recent) = s.app.read_with(s.cx, |a, _| {
            (
                a.layout().agent(PaneId(1)),
                a.layout().agent(PaneId(2)),
                a.gui_state()
                    .recent_dirs
                    .get("local")
                    .cloned()
                    .unwrap_or_default(),
            )
        });
        assert_eq!((one, two), (None, Some(9)));
        assert_eq!(recent, ["/two"]);
        let chrome = s.app.read_with(s.cx, |a, cx| {
            a.pane_view(PaneId(2)).unwrap().read(cx).chrome.clone()
        });
        assert!(chrome.has_agent && !chrome.starting);
    }

    #[gpui::test]
    fn requested_attach_is_not_reverted_by_the_old_agent(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(5, "/w", 1), row(7, "/w", 2)]);
        click(&mut s, "tab-5");
        attach_model(&fake, PaneId(1), 5);
        click(&mut s, "tab-7");
        // Output from agent 5 still arrives before the attach reply.
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        let shown = |s: &mut Setup<'_>| s.app.read_with(s.cx, |a, _| a.layout().agent(PaneId(1)));
        assert_eq!(shown(&mut s), Some(7));
        attach_model(&fake, PaneId(1), 7);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        assert_eq!(shown(&mut s), Some(7));
    }

    #[gpui::test]
    fn header_buttons_work_after_closing_the_last_pane(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        s.cx.simulate_keystrokes("cmd-w");
        assert_eq!(panes(&mut s), [PaneId(1)]);
        assert!(fake.calls().contains(&Call::Close(PaneId(1))));
        click(&mut s, "pane-1-split-right");
        assert_eq!(panes(&mut s).len(), 2);
        // Click-to-focus still follows the new view.
        let grid = s.app.read_with(s.cx, |a, cx| {
            a.pane_view(PaneId(1)).unwrap().read(cx).layout.unwrap()
        });
        let at = point(
            grid.origin.x + grid.cell_w * 2.0,
            grid.origin.y + grid.line_h * 2.0,
        );
        s.cx.simulate_click(at, Modifiers::none());
        s.cx.run_until_parked();
        assert_eq!(focused(&mut s), PaneId(1));
    }

    #[gpui::test]
    fn failed_spawn_is_shown(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let s = setup(cx, &fake, dir.path());
        *fake.spawn_error.lock().unwrap() = Some("no pane 1".into());
        s.cx.simulate_keystrokes("cmd-n");
        let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
        let dir_field = sheet.read_with(s.cx, |sh, _| sh.dir.clone());
        dir_field.update(s.cx, |f, cx| f.set_text("/tmp".into(), cx));
        s.cx.simulate_keystrokes("enter");
        // The sheet stays open with the error.
        let error = sheet.read_with(s.cx, |sh, _| sh.error.clone());
        assert_eq!(error.as_deref(), Some("no pane 1"));
        assert!(s.ws.read_with(s.cx, |w, _| w.sheet().is_some()));

        // A failure reported later by the session shows as the error toast.
        s.ui.try_send(UiEvent::Error("spawn failed: no such directory".into()))
            .unwrap();
        s.cx.run_until_parked();
        let shown = s.app.read_with(s.cx, |a, _| a.error().map(str::to_owned));
        assert_eq!(shown.as_deref(), Some("spawn failed: no such directory"));
        assert!(s.cx.debug_bounds("error-toast").is_some());
    }

    #[gpui::test]
    fn update_button_restarts_once_and_outcomes_show_as_toasts(cx: &mut TestAppContext) {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let s = setup(cx, &fake, dir.path());
        let shown = |s: &Setup<'_>| s.app.read_with(s.cx, |a, _| a.error().map(str::to_owned));
        let button = |s: &Setup<'_>| s.app.read_with(s.cx, |a, _| a.update_button());
        assert!(s.cx.debug_bounds("fleetd-update").is_none());
        assert_eq!(button(&s), None);

        // A newer binary is noticed on the next connect: the bar offers
        // Update fleetd.
        fake.update_available.store(true, Ordering::SeqCst);
        s.ui.try_send(UiEvent::Connection(ConnState::Connected))
            .unwrap();
        s.cx.run_until_parked();
        assert_eq!(button(&s).map(|b| b.label), Some("Update fleetd"));
        let update = s.cx.debug_bounds("fleetd-update").unwrap();
        let bar = s.cx.debug_bounds("titlebar").unwrap();
        assert!(bar.contains(&update.center()), "the button is in the bar");

        // A double click asks the session once; the button says it is
        // running and does nothing more.
        s.cx.simulate_click(update.center(), Modifiers::none());
        s.cx.run_until_parked();
        s.cx.simulate_click(update.center(), Modifiers::none());
        s.cx.run_until_parked();
        assert_eq!(fake.daemon_restarts.load(Ordering::SeqCst), 1);
        assert_eq!(
            button(&s),
            Some(crate::core::titlebar::UpdateButton {
                label: "Updating fleetd…",
                enabled: false
            })
        );

        // After the restart the binary matches again (gpui keeps the bounds
        // of elements it once drew, so the app's state is what is checked);
        // the toast says what happened, in the CLI's words.
        fake.restart_pending.store(false, Ordering::SeqCst);
        fake.update_available.store(false, Ordering::SeqCst);
        s.ui.try_send(UiEvent::DaemonRestarted(
            fleet_client::RestartOutcome::Restarted { kept: 2 },
        ))
        .unwrap();
        s.cx.run_until_parked();
        // Success is a neutral notice, not an error.
        assert_eq!(shown(&s), None);
        let notice = s.app.read_with(s.cx, |a, _| a.notice().map(str::to_owned));
        assert_eq!(notice.as_deref(), Some("fleetd restarted: 2 agents kept"));
        assert!(s.cx.debug_bounds("error-toast").is_some());
        assert!(!s.app.read_with(s.cx, |a, _| a.update_available));
        assert_eq!(button(&s), None, "the button goes away");
        s.ui.try_send(UiEvent::DaemonRestarted(
            fleet_client::RestartOutcome::Partial {
                kept: 1,
                ended: 1,
                ids: Some(vec![3]),
            },
        ))
        .unwrap();
        s.cx.run_until_parked();
        assert_eq!(
            shown(&s).as_deref(),
            Some("fleetd restarted: 1 kept, 1 could not be restored and were ended (ids 3)")
        );
        s.ui.try_send(UiEvent::DaemonRestartFailed("exec failed".into()))
            .unwrap();
        s.cx.run_until_parked();
        assert_eq!(
            shown(&s).as_deref(),
            Some("fleetd restart failed: exec failed; the old fleetd is still running")
        );
        s.ui.try_send(UiEvent::DaemonRestartFailed(
            fleet_client::did_not_come_back(),
        ))
        .unwrap();
        s.cx.run_until_parked();
        assert_eq!(
            shown(&s).as_deref(),
            Some("fleetd did not come back within 10 s")
        );
    }

    #[gpui::test]
    fn new_agent_sheet_is_as_tall_as_its_content(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let s = setup(cx, &fake, dir.path());
        s.cx.simulate_keystrokes("cmd-n");
        s.cx.run_until_parked();
        let sheet = s.cx.debug_bounds("new-agent-sheet").unwrap();
        let window = s.cx.update(|window, _| window.viewport_size());
        assert!(
            sheet.size.height < window.height / 2.0,
            "sheet height {:?} of {:?}",
            sheet.size.height,
            window.height
        );
    }

    #[gpui::test]
    fn reconnecting_clears_the_connection_error_only(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let s = setup(cx, &fake, dir.path());
        let shown = |s: &Setup<'_>| s.app.read_with(s.cx, |a, _| a.error().map(str::to_owned));

        s.ui.try_send(UiEvent::ConnectionError(
            "fleetd is not running on this Mac".into(),
        ))
        .unwrap();
        s.cx.run_until_parked();
        assert_eq!(
            shown(&s).as_deref(),
            Some("fleetd is not running on this Mac")
        );
        s.ui.try_send(UiEvent::Connection(ConnState::Connected))
            .unwrap();
        s.cx.run_until_parked();
        assert_eq!(shown(&s), None);
        assert!(s.cx.debug_bounds("error-toast").is_none());

        // Any other error outlives a reconnect.
        s.ui.try_send(UiEvent::Error("spawn failed: no such directory".into()))
            .unwrap();
        s.ui.try_send(UiEvent::Connection(ConnState::Connected))
            .unwrap();
        s.cx.run_until_parked();
        assert_eq!(
            shown(&s).as_deref(),
            Some("spawn failed: no such directory")
        );
    }

    #[gpui::test]
    fn click_in_sheet_over_a_pane_does_not_reach_the_pane(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        {
            let model = fake.model(PaneId(1));
            let mut m = model.lock().unwrap();
            m.on_attached(&AttachOk {
                agent_id: 1,
                cols: 80,
                rows: 24,
                history_tail: (0..50)
                    .map(|i| format!("h{i}").into_bytes().into())
                    .collect(),
                ..Default::default()
            });
            // Mouse reporting on: a click reaching the pane would be sent.
            m.on_output(b"hello\x1b[?1000h\x1b[?1006h");
        }
        s.cx.simulate_keystrokes("cmd-n");
        let at = center(&mut s, "new-agent-sheet");
        let grid = s.app.read_with(s.cx, |a, cx| {
            a.pane_view(PaneId(1)).unwrap().read(cx).layout.unwrap()
        });
        assert!(
            at.x > grid.origin.x && at.y > grid.origin.y,
            "the sheet is over the pane"
        );
        fake.inputs.lock().unwrap().clear();
        s.cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
        s.cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
        s.cx.simulate_event(ScrollWheelEvent {
            position: at,
            delta: ScrollDelta::Lines(point(0.0, 3.0)),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        // Shift-drag would select if it reached the pane.
        s.cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::shift());
        s.cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::shift());
        s.cx.run_until_parked();
        assert!(fake.inputs.lock().unwrap().is_empty());
        let model = fake.model(PaneId(1));
        let m = model.lock().unwrap();
        assert_eq!(m.selection_text(), None);
        assert_eq!(m.scroll_offset(), 0);
    }

    #[gpui::test]
    fn layout_persists_per_target(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(7, "/w", 1), row(8, "/w", 2)]);
        click(&mut s, "tab-7");
        s.cx.simulate_keystrokes("cmd-d");
        click(&mut s, "tab-8");
        let saved = s.app.read_with(s.cx, |a, _| a.layout().to_saved());
        // Debounced: nothing written yet.
        assert!(GuiState::load(dir.path()).layouts.is_empty());
        s.cx.executor().advance_clock(SAVE_DEBOUNCE);
        s.cx.run_until_parked();
        assert_eq!(GuiState::load(dir.path()).layouts["local"], saved);

        // A new app restores it and re-attaches each pane's agent.
        let fake2 = Arc::new(Fake::default());
        let app2 = app_state(s.cx, &fake2, dir.path());
        assert_eq!(
            fake2.calls(),
            [
                Call::Open(PaneId(1), Some(7)),
                Call::Open(PaneId(2), Some(8))
            ]
        );
        let restored = app2.read_with(s.cx, |a, _| a.layout().to_saved());
        assert_eq!(restored, saved);
    }

    #[gpui::test]
    fn closing_the_window_keeps_the_panes_attached(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let s = setup(cx, &fake, dir.path());
        s.cx.simulate_keystrokes("cmd-d");
        s.cx.update(|window, _| window.remove_window());
        s.cx.run_until_parked();
        assert!(!fake.calls().iter().any(|c| matches!(c, Call::Close(_))));
        let alive = s.app.read_with(s.cx, |a, _| {
            a.layout().panes().iter().all(|p| a.pane_view(*p).is_some())
        });
        assert!(alive);
    }

    #[gpui::test]
    fn switching_target_closes_the_sheet_and_drops_stale_menu_picks(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![ended(7, "/w", 1, AgentState::Exited)]);
        s.cx.simulate_keystrokes("cmd-d cmd-n");
        let (open, epoch) = s.ws.read_with(s.cx, |ws, _| (ws.sheet.is_some(), ws.epoch));
        assert!(open);
        // A direct switch (not through the connect sheet).
        let other = Arc::new(Fake::default());
        let session: Arc<dyn SessionApi> = other.clone();
        let (_tx, rx) = async_channel::unbounded();
        let target = Target::Ssh {
            dest: "me@box".into(),
        };
        s.app
            .update(s.cx, |a, cx| a.replace_session(session, rx, target, cx));
        s.cx.run_until_parked();
        let closed = s.ws.read_with(s.cx, |ws, _| ws.sheet.is_none());
        assert!(closed, "the sheet targeted the old layout's panes");
        // A pick from a menu opened before the switch does nothing.
        s.ws.update_in(s.cx, |ws, window, cx| {
            ws.row_menu_pick(7, RowMenuItem::Remove, epoch, window, cx);
            ws.pane_menu_pick(PaneId(1), PaneMenuItem::Close, epoch, window, cx);
        });
        s.cx.run_until_parked();
        assert!(!fake.calls().iter().any(|c| matches!(c, Call::Forget(_))));
        assert!(!other.calls().contains(&Call::Forget(vec![7])));
    }

    #[gpui::test]
    fn maximize_toggles_one_pane_and_keeps_the_others(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let drawn = |s: &mut Setup<'_>| {
            s.cx.run_until_parked();
            s.ws.read_with(s.cx, |w, _| w.drawn_panes().to_vec())
        };
        s.cx.simulate_keystrokes("cmd-d");
        assert_eq!(drawn(&mut s), [PaneId(1), PaneId(2)]);
        s.cx.simulate_keystrokes("cmd-shift-enter");
        assert_eq!(s.app.read_with(s.cx, |a, _| a.maximized()), Some(PaneId(2)));
        assert_eq!(drawn(&mut s), [PaneId(2)]);
        // The hidden pane stays attached.
        assert_eq!(panes(&mut s).len(), 2);
        assert!(
            s.app
                .read_with(s.cx, |a, _| a.pane_view(PaneId(1)).is_some())
        );
        assert!(!fake.calls().iter().any(|c| matches!(c, Call::Close(_))));
        // The header button toggles back.
        click(&mut s, "pane-2-maximize");
        assert_eq!(s.app.read_with(s.cx, |a, _| a.maximized()), None);
        assert_eq!(drawn(&mut s), [PaneId(1), PaneId(2)]);
        // And maximizes too.
        click(&mut s, "pane-1-maximize");
        assert_eq!(drawn(&mut s), [PaneId(1)]);
    }

    #[gpui::test]
    fn header_buttons_split_and_close_their_pane(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        click(&mut s, "pane-1-split-right");
        assert_eq!(panes(&mut s), [PaneId(1), PaneId(2)]);
        click(&mut s, "pane-2-split-down");
        assert_eq!(panes(&mut s), [PaneId(1), PaneId(2), PaneId(3)]);
        // Pane 1's button acts on pane 1, though pane 3 has the focus.
        click(&mut s, "pane-1-split-down");
        // Numbered in reading order: the new bottom-left pane (id 4) is 3.
        assert_eq!(panes(&mut s), [PaneId(1), PaneId(2), PaneId(4), PaneId(3)]);
        // Selectors go by pane number: number 3 is pane id 4.
        click(&mut s, "pane-3-close");
        assert_eq!(panes(&mut s), [PaneId(1), PaneId(2), PaneId(3)]);
        assert_eq!(fake.calls().last(), Some(&Call::Close(PaneId(4))));
    }

    #[gpui::test]
    fn focused_tab_highlight_follows_the_focused_pane(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(
            &mut s,
            &fake,
            vec![row(7, "/w", 1), row(8, "/w", 2), row(9, "/w", 3)],
        );
        click(&mut s, "tab-7");
        s.cx.simulate_keystrokes("cmd-d");
        click(&mut s, "tab-8");
        let marks = |s: &mut Setup<'_>| s.app.read_with(s.cx, |a, _| a.tab_marks());
        let m = marks(&mut s);
        assert_eq!(m.get(&8), Some(&TabMark::Focused));
        assert_eq!(m.get(&7), Some(&TabMark::Shown));
        assert_eq!(m.get(&9), None);
        s.cx.simulate_keystrokes("cmd-1");
        let m = marks(&mut s);
        assert_eq!(m.get(&7), Some(&TabMark::Focused));
        assert_eq!(m.get(&8), Some(&TabMark::Shown));
        // Clicking into a pane moves the highlight too.
        let grid = s.app.read_with(s.cx, |a, cx| {
            a.pane_view(PaneId(2)).unwrap().read(cx).layout.unwrap()
        });
        let at = point(
            grid.origin.x + grid.cell_w * 2.0,
            grid.origin.y + grid.line_h * 2.0,
        );
        s.cx.simulate_click(at, Modifiers::none());
        s.cx.run_until_parked();
        let app = s.app.clone();
        let f = s.cx.update(|window, cx| {
            let view = app.read(cx).pane_view(PaneId(2)).unwrap().clone();
            view.read(cx).focus_handle().is_focused(window)
        });
        assert!(f, "pane 2 focused by the click");
        assert_eq!(marks(&mut s).get(&8), Some(&TabMark::Focused));
    }

    fn drawn(s: &mut Setup<'_>, selector: &'static str) -> bool {
        s.cx.run_until_parked();
        s.cx.debug_bounds(selector).is_some()
    }

    /// Picks `item` from `agent_id`'s row menu (the native menu itself
    /// cannot be shown headless).
    fn pick_row(s: &mut Setup<'_>, agent_id: u64, item: RowMenuItem) {
        s.ws.update_in(s.cx, |ws, window, cx| {
            let epoch = ws.epoch;
            ws.row_menu_pick(agent_id, item, epoch, window, cx);
        });
        s.cx.run_until_parked();
    }

    fn row_items(s: &mut Setup<'_>, agent_id: u64) -> Vec<RowMenuItem> {
        s.ws.read_with(s.cx, |w, cx| w.menu_items(agent_id, cx))
            .into_iter()
            .filter_map(|e| match e {
                MenuEntry::Item(i) => Some(i),
                MenuEntry::Separator => None,
            })
            .collect()
    }

    fn shown(s: &mut Setup<'_>, pane: PaneId) -> Option<u64> {
        s.app.read_with(s.cx, |a, _| a.layout().agent(pane))
    }

    #[gpui::test]
    fn row_menu_offers_restart_and_remove_for_exited(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(
            &mut s,
            &fake,
            vec![
                row(7, "/w", 1),
                ended(8, "/w", 2, AgentState::Exited),
                ended(6, "/w", 3, AgentState::Lost),
            ],
        );
        assert_eq!(
            row_items(&mut s, 7),
            [RowMenuItem::OpenSplit, RowMenuItem::Kill]
        );
        for id in [8, 6] {
            assert_eq!(
                row_items(&mut s, id),
                [
                    RowMenuItem::OpenSplit,
                    RowMenuItem::Restart,
                    RowMenuItem::Remove
                ]
            );
        }
        // Restart asks for nothing: it goes out at once.
        pick_row(&mut s, 6, RowMenuItem::Restart);
        assert!(!s.cx.has_pending_prompt());
        assert_eq!(fake.calls().last(), Some(&Call::Restart(6)));
        // Kill asks first.
        pick_row(&mut s, 7, RowMenuItem::Kill);
        assert!(s.cx.has_pending_prompt());
    }

    #[gpui::test]
    fn row_menu_open_in_new_split_adds_a_pane(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(7, "/w", 1)]);
        pick_row(&mut s, 7, RowMenuItem::OpenSplit);
        assert_eq!(panes(&mut s).len(), 2);
        let pane = focused(&mut s);
        assert_eq!(shown(&mut s, pane), Some(7));
        // With four panes the menu no longer offers it.
        s.cx.simulate_keystrokes("cmd-d cmd-d");
        assert_eq!(row_items(&mut s, 7), [RowMenuItem::Kill]);
    }

    #[gpui::test]
    fn row_menu_pick_is_checked_against_the_agent_now(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(7, "/w", 1)]);
        // The menu offered Kill; the agent exited before the pick.
        agents(&mut s, &fake, vec![ended(7, "/w", 1, AgentState::Exited)]);
        pick_row(&mut s, 7, RowMenuItem::Kill);
        assert!(!s.cx.has_pending_prompt(), "no Kill for an exited agent");
        // Remove picked for an agent the list now shows running.
        agents(&mut s, &fake, vec![row(8, "/w", 2)]);
        pick_row(&mut s, 8, RowMenuItem::Remove);
        assert!(!fake.calls().iter().any(|c| matches!(c, Call::Forget(_))));
        // An unknown agent offers no Restart either.
        pick_row(&mut s, 99, RowMenuItem::Restart);
        assert!(!fake.calls().contains(&Call::Restart(99)));
    }

    #[gpui::test]
    fn pane_menu_restart_restarts_the_ended_agent_in_place(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(5, "/w", 1)]);
        click(&mut s, "tab-5");
        attach_model(&fake, PaneId(1), 5);
        fake.model(PaneId(1)).lock().unwrap().on_exited(3);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        s.ws.update_in(s.cx, |ws, window, cx| {
            let epoch = ws.epoch;
            ws.pane_menu_pick(PaneId(1), PaneMenuItem::Restart, epoch, window, cx);
        });
        s.cx.run_until_parked();
        let size = s.app.read_with(s.cx, |a, cx| {
            a.pane_view(PaneId(1)).unwrap().read(cx).grid().unwrap()
        });
        assert!(fake.calls().contains(&Call::RestartInPane {
            pane: PaneId(1),
            agent_id: 5,
            size,
        }));
    }

    #[gpui::test]
    fn pane_menu_picks_act_on_their_pane(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        s.cx.simulate_keystrokes("cmd-d");
        let [first, second] = panes(&mut s)[..] else {
            panic!("two panes")
        };
        let pick = |s: &mut Setup<'_>, pane, item| {
            s.ws.update_in(s.cx, |ws, window, cx| {
                let epoch = ws.epoch;
                ws.pane_menu_pick(pane, item, epoch, window, cx);
            });
            s.cx.run_until_parked();
        };
        pick(&mut s, first, PaneMenuItem::Maximize);
        assert_eq!(s.app.read_with(s.cx, |a, _| a.maximized()), Some(first));
        pick(&mut s, first, PaneMenuItem::Restore);
        assert_eq!(s.app.read_with(s.cx, |a, _| a.maximized()), None);
        pick(&mut s, second, PaneMenuItem::Close);
        assert_eq!(panes(&mut s), [first]);
        pick(&mut s, first, PaneMenuItem::SplitDown);
        assert_eq!(panes(&mut s).len(), 2);
    }

    #[gpui::test]
    fn remove_calls_forget(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![ended(8, "/w", 2, AgentState::Exited)]);
        pick_row(&mut s, 8, RowMenuItem::Remove);
        assert!(!s.cx.has_pending_prompt());
        assert_eq!(fake.calls().last(), Some(&Call::Forget(vec![8])));
    }

    #[gpui::test]
    fn cmd_r_restarts_exited_pane_in_place(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(5, "/w", 1)]);
        click(&mut s, "tab-5");
        attach_model(&fake, PaneId(1), 5);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        let restarts = |f: &Fake| {
            f.calls()
                .into_iter()
                .filter(|c| matches!(c, Call::RestartInPane { .. }))
                .collect::<Vec<_>>()
        };
        // Live: cmd-r does nothing, and there is no Restart button.
        s.cx.simulate_keystrokes("cmd-r");
        assert!(restarts(&fake).is_empty());
        assert!(!drawn(&mut s, "pane-1-restart"));

        fake.model(PaneId(1)).lock().unwrap().on_exited(3);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        s.cx.simulate_keystrokes("cmd-r");
        let size = s.app.read_with(s.cx, |a, cx| {
            a.pane_view(PaneId(1)).unwrap().read(cx).grid().unwrap()
        });
        let want = Call::RestartInPane {
            pane: PaneId(1),
            agent_id: 5,
            size,
        };
        assert_eq!(restarts(&fake), std::slice::from_ref(&want));
        // The exit banner's button does the same.
        click(&mut s, "pane-1-restart");
        assert_eq!(restarts(&fake), [want.clone(), want]);

        // The pane started the new agent itself: it is not attached again
        // when the control connection names the successor.
        s.ui.try_send(UiEvent::AgentRemoved {
            agent_id: 5,
            replaced_by: Some(9),
        })
        .unwrap();
        s.cx.run_until_parked();
        assert_eq!(shown(&mut s, PaneId(1)), Some(9));
        assert!(!fake.calls().contains(&Call::Attach(PaneId(1), 9)));
        // Output from the old agent before the reply does not revert it.
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        assert_eq!(shown(&mut s, PaneId(1)), Some(9));
    }

    /// Pane 1 shows agent 5, exited, and cmd-r restarts it in place.
    fn restart_in_pane_1(s: &mut Setup<'_>, fake: &Fake) {
        agents(s, fake, vec![ended(5, "/w", 1, AgentState::Exited)]);
        click(s, "tab-5");
        attach_model(fake, PaneId(1), 5);
        fake.model(PaneId(1)).lock().unwrap().on_exited(3);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        s.cx.simulate_keystrokes("cmd-r");
        assert!(
            fake.calls()
                .iter()
                .any(|c| matches!(c, Call::RestartInPane { agent_id: 5, .. }))
        );
    }

    #[gpui::test]
    fn pane_whose_restart_reply_was_lost_attaches_the_successor(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        restart_in_pane_1(&mut s, &fake);
        // The control connection names the successor; the pane expects its
        // own reply to show it, so nothing attaches yet.
        s.ui.try_send(UiEvent::AgentRemoved {
            agent_id: 5,
            replaced_by: Some(9),
        })
        .unwrap();
        s.cx.run_until_parked();
        assert!(!fake.calls().contains(&Call::Attach(PaneId(1), 9)));
        // That reply was lost with the pane's connection.
        s.ui.try_send(UiEvent::LaunchDropped(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        assert_eq!(fake.calls().last(), Some(&Call::Attach(PaneId(1), 9)));
        assert_eq!(shown(&mut s, PaneId(1)), Some(9));
    }

    #[gpui::test]
    fn pane_whose_restart_reply_was_lost_follows_a_later_successor(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        restart_in_pane_1(&mut s, &fake);
        let attaches = |f: &Fake| {
            f.calls()
                .into_iter()
                .filter(|c| matches!(c, Call::Attach(PaneId(1), _)))
                .count()
        };
        let before = attaches(&fake);
        // The reply is lost before the successor is announced: the pane
        // shows the agent its layout names again...
        s.ui.try_send(UiEvent::LaunchDropped(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        assert_eq!(fake.calls().last(), Some(&Call::Attach(PaneId(1), 5)));
        assert_eq!(attaches(&fake), before + 1);
        // ...and attaches the successor once it is.
        s.ui.try_send(UiEvent::AgentRemoved {
            agent_id: 5,
            replaced_by: Some(9),
        })
        .unwrap();
        s.cx.run_until_parked();
        assert_eq!(fake.calls().last(), Some(&Call::Attach(PaneId(1), 9)));
        assert_eq!(shown(&mut s, PaneId(1)), Some(9));
    }

    #[gpui::test]
    fn agent_removed_moves_pane_to_successor(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(5, "/w", 1), row(7, "/w", 2)]);
        click(&mut s, "tab-5");
        attach_model(&fake, PaneId(1), 5);
        s.cx.simulate_keystrokes("cmd-d");
        click(&mut s, "tab-7");
        s.ui.try_send(UiEvent::AgentRemoved {
            agent_id: 5,
            replaced_by: Some(9),
        })
        .unwrap();
        s.cx.run_until_parked();
        assert_eq!(shown(&mut s, PaneId(1)), Some(9));
        assert_eq!(shown(&mut s, PaneId(2)), Some(7));
        assert!(fake.calls().contains(&Call::Attach(PaneId(1), 9)));
        // Output from the old agent before the attach does not revert it.
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        assert_eq!(shown(&mut s, PaneId(1)), Some(9));
        s.cx.executor().advance_clock(SAVE_DEBOUNCE);
        s.cx.run_until_parked();
        let saved = s.app.read_with(s.cx, |a, _| a.layout().to_saved());
        assert_eq!(GuiState::load(dir.path()).layouts["local"], saved);
        assert_eq!(Layout::from_saved(&saved).agent(PaneId(1)), Some(9));

        // Forgotten: the pane is emptied, on a fresh connection.
        s.ui.try_send(UiEvent::AgentRemoved {
            agent_id: 7,
            replaced_by: None,
        })
        .unwrap();
        s.cx.run_until_parked();
        assert_eq!(shown(&mut s, PaneId(2)), None);
        assert_eq!(fake.calls().last(), Some(&Call::Open(PaneId(2), None)));
        // The focused pane keeps the keyboard though its view was replaced.
        let app = s.app.clone();
        let has_focus = s.cx.update(|window, cx| {
            let view = app.read(cx).pane_view(PaneId(2)).unwrap().clone();
            view.read(cx).focus_handle().is_focused(window)
        });
        assert!(has_focus);
    }

    #[gpui::test]
    fn list_follows_replacement_into_layout(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(5, "/w", 1), row(6, "/w", 2)]);
        click(&mut s, "tab-5");
        attach_model(&fake, PaneId(1), 5);
        s.cx.simulate_keystrokes("cmd-d");
        click(&mut s, "tab-6");
        // Agent 6 is gone from the node (a lost attach), 5 was restarted
        // as 9 while the GUI did not hear of it.
        fake.model(PaneId(2)).lock().unwrap().on_lost();
        *fake.replaced.lock().unwrap() = HashMap::from([(5, 9)]);
        fake.listed.store(true, std::sync::atomic::Ordering::SeqCst);
        agents(&mut s, &fake, vec![row(9, "/w", 3)]);
        assert_eq!(shown(&mut s, PaneId(1)), Some(9));
        assert!(fake.calls().contains(&Call::Attach(PaneId(1), 9)));
        assert_eq!(shown(&mut s, PaneId(2)), None);
        assert_eq!(fake.calls().last(), Some(&Call::Open(PaneId(2), None)));
    }

    /// A `ListAgents` reply with `rows`.
    fn list(s: &mut Setup<'_>, fake: &Fake, rows: Vec<AgentRow>) {
        fake.listed.store(true, std::sync::atomic::Ordering::SeqCst);
        fake.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        agents(s, fake, rows);
    }

    #[gpui::test]
    fn exited_pane_unlisted_by_two_lists_is_emptied(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        list(&mut s, &fake, vec![ended(8, "/w", 1, AgentState::Exited)]);
        click(&mut s, "tab-8");
        attach_model(&fake, PaneId(1), 8);
        fake.model(PaneId(1)).lock().unwrap().on_exited(3);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        // Agent 8 is forgotten while the GUI misses the `AgentRemoved`.
        // One list without it might just predate it (a new agent that
        // exited at once): the pane keeps its final screen...
        list(&mut s, &fake, vec![]);
        assert_eq!(shown(&mut s, PaneId(1)), Some(8));
        // ...as it does for events that are not lists.
        agents(&mut s, &fake, vec![]);
        assert_eq!(shown(&mut s, PaneId(1)), Some(8));
        // The next list confirms it is gone.
        list(&mut s, &fake, vec![]);
        assert_eq!(shown(&mut s, PaneId(1)), None);
        assert_eq!(fake.calls().last(), Some(&Call::Open(PaneId(1), None)));
    }

    #[gpui::test]
    fn exited_pane_listed_again_is_kept(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        list(&mut s, &fake, vec![]);
        // A spawn lands and its agent exits before any list shows it.
        spawn_via_sheet(&mut s, "/w");
        attach_model(&fake, PaneId(1), 9);
        fake.model(PaneId(1)).lock().unwrap().on_exited(0);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        assert_eq!(shown(&mut s, PaneId(1)), Some(9));
        list(&mut s, &fake, vec![]);
        list(&mut s, &fake, vec![ended(9, "/w", 1, AgentState::Exited)]);
        // Absent once, then listed: absent once more is not two in a row.
        list(&mut s, &fake, vec![]);
        assert_eq!(shown(&mut s, PaneId(1)), Some(9));
    }

    #[gpui::test]
    fn unlisted_pane_with_a_restart_on_its_way_is_kept(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        list(&mut s, &fake, vec![ended(6, "/w", 1, AgentState::Lost)]);
        s.app.update(s.cx, |a, cx| a.show_agent(6, cx));
        fake.model(PaneId(1)).lock().unwrap().on_lost();
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        s.cx.simulate_keystrokes("cmd-r");
        assert!(
            fake.calls()
                .iter()
                .any(|c| matches!(c, Call::RestartInPane { agent_id: 6, .. }))
        );
        // Lists taken before the restart's successor is known.
        list(&mut s, &fake, vec![]);
        list(&mut s, &fake, vec![]);
        assert_eq!(shown(&mut s, PaneId(1)), Some(6));
    }

    #[gpui::test]
    fn bar_new_agent_button_opens_the_sheet_and_the_title_counts_agents(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(
            &mut s,
            &fake,
            vec![row(7, "/w", 1), ended(8, "/w", 2, AgentState::Exited)],
        );
        assert!(drawn(&mut s, "titlebar") && drawn(&mut s, "bar-target"));
        let title = s.ws.read_with(s.cx, |ws, _| ws.window_title.clone());
        assert_eq!(title, "fleet — This Mac");
        // The bar sits above the sidebar's first tab.
        let bar = s.cx.debug_bounds("titlebar").unwrap();
        let tab = s.cx.debug_bounds("tab-7").unwrap();
        assert!(bar.bottom() <= tab.top());
        click(&mut s, "new-agent");
        assert!(s.ws.read_with(s.cx, |ws, _| ws.sheet.is_some()));
    }

    #[gpui::test]
    fn the_bar_stays_a_drag_strip_under_a_sheet(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![ended(8, "/w", 2, AgentState::Exited)]);
        let bar = s.cx.debug_bounds("titlebar").unwrap();
        let clear = center(&mut s, "clear-exited");
        s.cx.simulate_keystrokes("cmd-n");
        assert!(drawn(&mut s, "sheet-drag-strip"));
        let strip = s.cx.debug_bounds("sheet-drag-strip").unwrap();
        assert_eq!(
            (strip.top(), strip.size.height),
            (bar.top(), bar.size.height)
        );
        assert!(strip.size.width >= bar.size.width);
        // The bar's buttons are inert under it; the sheet stays.
        s.cx.simulate_click(clear, Modifiers::none());
        s.cx.run_until_parked();
        assert!(!fake.calls().contains(&Call::ClearExited));
        assert!(s.ws.read_with(s.cx, |ws, _| ws.sheet.is_some()));
    }

    #[gpui::test]
    fn clear_exited_disabled_without_exited_rows(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(7, "/w", 1)]);
        click(&mut s, "clear-exited");
        assert!(!fake.calls().contains(&Call::ClearExited));
        agents(
            &mut s,
            &fake,
            vec![row(7, "/w", 1), ended(8, "/w", 2, AgentState::Exited)],
        );
        click(&mut s, "clear-exited");
        assert!(!s.cx.has_pending_prompt());
        assert_eq!(fake.calls().last(), Some(&Call::ClearExited));
    }
}
