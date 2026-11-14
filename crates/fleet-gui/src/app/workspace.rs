//! The workspace window: the sidebar, up to four split panes and the
//! sheets over them, drawn from [`AppState`].

use std::collections::HashMap;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{
    App, AppContext, Bounds, Context, Entity, EntityId, FocusHandle, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Pixels, Point, PromptButton, PromptLevel, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Task, TitlebarOptions, Window,
    WindowBounds, WindowOptions, anchored, deferred, div, px, relative, size,
};

use super::app_state::AppState;
use super::keymap::{
    ClosePane, FocusDown, FocusLeft, FocusPane1, FocusPane2, FocusPane3, FocusPane4, FocusRight,
    FocusUp, Kill, NewAgent, SplitDown, SplitRight, ToggleMaximize,
};
use super::new_agent::{NewAgentSheet, SheetEvent};
use super::pane_view::{PaneEvent, PaneView};
use super::sidebar_view::{SidebarData, TabMark, render_sidebar};
use super::theme::Theme;
use crate::core::layout::{Axis, Dir, MAX_PANES, Rect};
use crate::core::session::PaneId;
use crate::core::target::Target;

/// Between pane cards, and around them.
const PANE_GAP: f32 = 8.0;

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
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1200.), px(760.)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("fleet".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let state = app.clone();
    match cx.open_window(options, |window, cx| {
        cx.new(|cx| Workspace::new(state, window, cx))
    }) {
        Ok(handle) => app.update(cx, |a, _| a.window = Some(handle)),
        Err(e) => eprintln!("fleet-gui: cannot open the window: {e}"),
    }
}

/// A right-click menu on a sidebar row.
struct RowMenu {
    agent_id: u64,
    at: Point<Pixels>,
}

/// The window's root view, over [`AppState`].
pub struct Workspace {
    app: Entity<AppState>,
    sheet: Option<(Entity<NewAgentSheet>, Subscription)>,
    menu: Option<RowMenu>,
    /// Focus-in and header-button subscriptions, by pane.
    pane_focus: HashMap<PaneId, (EntityId, [Subscription; 2])>,
    /// The panes drawn by the last render.
    drawn: Vec<PaneId>,
    _subscriptions: Vec<Subscription>,
    _ticker: Task<()>,
}

impl Workspace {
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&app, |_, _, cx| cx.notify());
        let appearance = cx.observe_window_appearance(window, |this, window, cx| {
            let theme = Theme::for_appearance(window.appearance());
            this.app.update(cx, |a, cx| a.set_theme(theme, cx));
            window.refresh();
        });
        let theme = Theme::for_appearance(window.appearance());
        app.update(cx, |a, cx| a.set_theme(theme, cx));
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
        let ws = Workspace {
            app,
            _ticker: ticker,
            sheet: None,
            menu: None,
            pane_focus: HashMap::new(),
            drawn: Vec::new(),
            _subscriptions: vec![observe, appearance],
        };
        ws.focus_pane(ws.app.read(cx).focused(), window, cx);
        ws
    }

    pub fn app(&self) -> &Entity<AppState> {
        &self.app
    }

    /// The panes the last frame drew (one while maximized).
    pub fn drawn_panes(&self) -> &[PaneId] {
        &self.drawn
    }

    pub fn sheet(&self) -> Option<&Entity<NewAgentSheet>> {
        self.sheet.as_ref().map(|(s, _)| s)
    }

    /// A sheet or a menu is open over the panes.
    fn modal(&self) -> bool {
        self.sheet.is_some() || self.menu.is_some()
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

    pub fn open_row_menu(&mut self, agent_id: u64, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.menu = Some(RowMenu { agent_id, at });
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        cx.notify();
    }

    pub fn open_new_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        if let Some((sheet, _)) = &self.sheet {
            sheet.read(cx).focus(window, cx);
            return;
        }
        let app = self.app.read(cx);
        let dirs = app.dir_choices();
        let can_choose = app.target == Target::Local;
        let command = app.config.default_command.clone();
        let sheet = cx.new(|cx| NewAgentSheet::new(&command, dirs, can_choose, cx));
        let sub = cx.subscribe_in(&sheet, window, Self::sheet_event);
        sheet.read(cx).focus(window, cx);
        self.sheet = Some((sheet, sub));
        cx.notify();
    }

    fn new_agent(&mut self, _: &NewAgent, window: &mut Window, cx: &mut Context<Self>) {
        self.open_new_agent(window, cx);
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
        self.menu = None;
        let agent_id = action.agent_id;
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

    fn open_menu_split(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
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
        // A pane id can get a new view (closing the last pane replaces it):
        // subscriptions follow the view, not the id.
        self.pane_focus.retain(|p, (id, _)| {
            views
                .iter()
                .any(|(pane, view)| pane == p && view.entity_id() == *id)
        });
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
                .insert(pane, (view.entity_id(), [focus, events]));
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
        }
    }

    fn toggle_maximize(&mut self, _: &ToggleMaximize, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal() {
            return;
        }
        self.app.update(cx, |a, cx| a.toggle_maximize(cx));
        self.focus_current(window, cx);
    }

    /// The pane cards, 8 px apart and from the edges; only the maximized
    /// one when a pane is maximized (the others stay attached).
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
                        .p(px(PANE_GAP / 2.0))
                        .child(view),
                )
            })
            .collect();
        div().relative().flex_1().h_full().min_w_0().child(
            div()
                .absolute()
                .inset(px(PANE_GAP / 2.0))
                .children(children),
        )
    }

    fn render_menu(
        &self,
        menu: &RowMenu,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let agent_id = menu.agent_id;
        let can_split = self.app.read(cx).layout.panes().len() < MAX_PANES;
        let item = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px(px(10.))
                .py(px(4.))
                .rounded(px(3.))
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg()))
                .child(label)
        };
        let mut items = div()
            .id("row-menu")
            .occlude()
            .flex()
            .flex_col()
            .p(px(4.))
            .min_w(px(160.))
            .rounded(px(6.))
            .border_1()
            .border_color(theme.dim_text)
            .bg(theme.header_bg)
            .text_color(theme.fg())
            .text_size(px(13.));
        if can_split {
            items = items.child(item("open-split", "Open in new split").on_click(
                cx.listener(move |this, _, window, cx| this.open_menu_split(agent_id, window, cx)),
            ));
        }
        items = items.child(item("kill", "Kill").on_click(
            cx.listener(move |this, _, window, cx| this.kill(&Kill { agent_id }, window, cx)),
        ));
        // A click anywhere else closes the menu.
        let backdrop = div()
            .id("menu-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close_menu(cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| this.close_menu(cx)),
            );
        deferred(
            div()
                .absolute()
                .inset_0()
                .child(backdrop)
                .child(anchored().position(menu.at).child(items)),
        )
        .with_priority(1)
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.follow_pane_focus(window, cx);
        let theme = self.app.read(cx).theme;
        let (rows, home, error, titles, marks) = {
            let app = self.app.read(cx);
            (
                app.agents.clone(),
                app.home.clone(),
                app.error.clone(),
                app.titles.clone(),
                app.tab_marks(),
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
        let panes = self.render_panes(cx);
        let toast = error.map(|e| {
            div()
                .id("error")
                .debug_selector(|| "error-toast".to_owned())
                .absolute()
                .bottom(px(12.))
                .right(px(12.))
                .max_w(px(480.))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(6.))
                .border_1()
                .border_color(theme.resolve(fleet_term::Color::Named(1), true))
                .bg(theme.header_bg)
                .text_color(theme.fg())
                .text_size(px(12.))
                .occlude()
                .cursor_pointer()
                .child(SharedString::from(format!("{e}   ✕")))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.app.update(cx, |a, cx| a.dismiss_error(cx));
                }))
        });
        let menu = self
            .menu
            .as_ref()
            .map(|m| self.render_menu(m, &theme, cx).into_any_element());
        let sheet = self.sheet.as_ref().map(|(sheet, _)| {
            // The backdrop takes every click and wheel event over the panes.
            deferred(
                div()
                    .id("sheet-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .justify_center()
                    .pt(px(80.))
                    .child(sheet.clone()),
            )
            .with_priority(2)
        });
        div()
            .id("workspace")
            .size_full()
            .relative()
            .flex()
            .flex_row()
            .bg(theme.window_bg)
            .text_color(theme.fg())
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
            .on_action(cx.listener(Self::kill))
            .child(sidebar)
            .child(panes)
            .children(toast)
            .children(menu)
            .children(sheet)
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
    use crate::core::pane::PaneModel;
    use crate::core::session::{AgentRow, PaneId, UiEvent};
    use crate::core::state::GuiState;
    use crate::core::target::Target;

    #[derive(Clone, Debug, PartialEq)]
    enum Call {
        Open(PaneId, Option<u64>),
        Attach(PaneId, u64),
        Close(PaneId),
        Kill(u64),
        Spawn {
            program: String,
            args: Vec<String>,
            cwd: String,
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

        fn spawn(
            &self,
            program: String,
            args: Vec<String>,
            cwd: String,
            cols: u16,
            rows: u16,
            pane: PaneId,
        ) -> Result<(), String> {
            if let Some(e) = self.spawn_error.lock().unwrap().clone() {
                return Err(e);
            }
            self.calls.lock().unwrap().push(Call::Spawn {
                program,
                args,
                cwd,
                size: (cols, rows),
                pane,
            });
            Ok(())
        }

        fn kill(&self, agent_id: u64) {
            self.calls.lock().unwrap().push(Call::Kill(agent_id));
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
            program: "claude".into(),
            args: vec!["--append-system-prompt".into(), "be brief".into()],
            cwd: "/r1".into(),
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
            program: "sh".into(),
            args: vec!["-c".into(), "echo hi".into()],
            cwd: "/tmp".into(),
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
        // Pane 1's button acts on pane 1, though pane 2 has the focus.
        click(&mut s, "pane-1-split-down");
        assert_eq!(panes(&mut s), [PaneId(1), PaneId(3), PaneId(2)]);
        // Selectors go by pane number: pane id 3 is number 2.
        click(&mut s, "pane-2-close");
        assert_eq!(panes(&mut s), [PaneId(1), PaneId(2)]);
        assert_eq!(fake.calls().last(), Some(&Call::Close(PaneId(3))));
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
}
