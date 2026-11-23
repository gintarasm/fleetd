//! The workspace window: the sidebar, up to four split panes and the
//! sheets over them, drawn from [`AppState`].

use std::collections::HashMap;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use fleet_client::names::name_of;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Action, Animation, AnimationExt, App, AppContext, Bounds, Context, Div, Entity, EntityId,
    ExternalPaths, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    ModifiersChangedEvent, MouseButton, MouseDownEvent, ParentElement, PromptButton, PromptLevel,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Task, TitlebarOptions,
    Window, WindowBackgroundAppearance, WindowBounds, WindowOptions, deferred, div, point, px,
    relative, size,
};

mod workflow_ui;

use super::action_sheet::{self, ActionSheet};
use super::answer_sheet::{AnswerEvent, AnswerSheet};
use super::app_state::AppState;
use super::connect::{ConnectEvent, ConnectSheet, StartupAttempt, new_sheet, switch_target};
use super::context_menu::{self, MenuLine};
use super::controls::{ButtonKind, ReducedMotion, appear, button};
use super::diff_pane::{DiffPane, DiffPaneEvent};
use super::graph_view::{GraphEvent, GraphView};
use super::keymap::{
    ClearScrollback, ClosePane, CloseTab, CommandPalette, Copy, CopyLastOutput, DuplicatePane,
    EditLabelsPane, FocusDown, FocusFilter, FocusLeft, FocusPane1, FocusPane2, FocusPane3,
    FocusPane4, FocusRight, FocusUp, Kill, KillPane, KillSelected, NewAgent, NewTab, NewTerminal,
    NextAgent, NextNeedsYou, NextTab, OpenOutput, Paste, PrevTab, PreviousAgent, Remove,
    RemoveExited, RemovePane, RemoveSelected, RenamePane, ReopenClosedPane, Restart, RestartPane,
    RestartSelected, SaveOutput, ScrollToBottom, ScrollToTop, SearchAllAgents, SelectAll,
    SelectTab, SendToAgents, ShowShortcuts, SplitDown, SplitRight, ToggleGraph, ToggleMaximize,
    WORKSPACE_CONTEXT, ZoomIn, ZoomOut, ZoomReset,
};
use super::kill_sheet::{self, KillEvent, KillSheet};
use super::labels_sheet::{LabelsEvent, LabelsSheet};
use super::native;
use super::new_agent::{NewAgentSheet, SheetEvent, SheetSetup};
use super::output;
use super::palette::{command_action, command_shortcut};
use super::pane_drag::PaneDrag;
use super::pane_view::{PaneEvent, PaneView};
use super::peek_view::{self, PeekEvent, PeekView};
use super::picker_view::{PickerEvent, PickerView};
use super::rename::{RenameEvent, RenameSheet};
use super::search_all_view::{SearchAllEvent, SearchAllView};
use super::send_sheet::{self, SendEvent, SendSheet};
use super::sidebar_view::{
    SidebarData, TabMark, program_name, render_pending, render_sidebar, tab_title,
};
use super::split_chooser::{ChooserEvent, SplitChooserView};
use super::split_dividers::SplitDrag;
use super::text_field::TextField;
use super::theme::{Theme, Typography, appearance_of};
use super::titlebar_view::{BarData, render_bar, sheet_drag_strip};
use super::{controls, keymap};
use crate::core::broadcast::{RowClick, RowSelection, sendable, target_name};
use crate::core::bulk::{BulkAction, BulkTargets};
use crate::core::config::{GuiConfig, save_confirm_kill, save_connection};
use crate::core::drag_drop::DropTarget;
use crate::core::feedback::{self, Tween};
use crate::core::glass::{Backdrop, GlassChoice, glass_tint};
use crate::core::kill::{KillFlow, kill_flow};
use crate::core::labels::chip_filter;
use crate::core::layout::{Axis, Dir, MAX_PANES, Rect};
use crate::core::menus::{
    MenuEntry, PaneMenuItem, PaneMenuState, RowMenuItem, bulk_entries, group_menu, pane_menu,
    row_menu,
};
use crate::core::palette::{Entry, PaletteState, palette_items, remove_exited_prompt};
use crate::core::peek::sidebar_target;
use crate::core::picker::Recents;
use crate::core::presets::{Prefill, preset_request};
use crate::core::question::open_question;
use crate::core::session::PaneId;
use crate::core::settings::{MotionChoice, ThemeChoice};
use crate::core::shortcuts::Section;
use crate::core::sidebar::{
    Filter, GroupBy, SidebarGroup, StatusFilter, ended, group_agents_by, narrow_groups,
    program_and_id,
};
use crate::core::sidebar_tree::removable_ended;
use crate::core::spawn::SpawnRequest;
use crate::core::split_chooser::{choice_rows, off_screen_agents};
use crate::core::status::Subject;
use crate::core::style::{chrome, motion, radius, space, text};
use crate::core::switcher::{AgentMru, start_row, switcher_items};
use crate::core::tabs::tab_label;
use crate::core::target::Target;
use crate::core::theme::Tokens;
use crate::core::titlebar::{Link, bar_title, window_title};
use crate::core::zoom::Zoom;

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
        // tint, or through liquid glass (`[gui] glass`, ADR 0049); Reduce
        // Transparency keeps the window opaque (read when the window opens).
        window_background: background_of(backdrop(app.read(cx).config())),
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

/// The workspace window's backdrop for `[gui] glass`, this macOS and
/// Reduce Transparency.
fn backdrop(config: &GuiConfig) -> Backdrop {
    Backdrop::resolve(
        config.glass,
        native::glass_available(),
        native::reduce_transparency(),
    )
}

fn background_of(backdrop: Backdrop) -> WindowBackgroundAppearance {
    match backdrop {
        Backdrop::Opaque => WindowBackgroundAppearance::Opaque,
        Backdrop::Blur | Backdrop::GlassChrome => WindowBackgroundAppearance::Blurred,
        Backdrop::GlassWindow => WindowBackgroundAppearance::Transparent,
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

/// The open agent switcher.
struct Switcher {
    view: Entity<PickerView>,
    /// ⌃ was seen down: its release picks.
    held: bool,
    _sub: Subscription,
}

/// What is open over the panes: one at a time (issue #143). Each holds
/// its view's subscriptions, which are kept, never read.
#[allow(dead_code)]
enum Modal {
    /// The New agent sheet.
    NewAgent(Entity<NewAgentSheet>, Subscription),
    /// The Rename sheet (ADR 0052).
    Rename(Entity<RenameSheet>, Subscription),
    /// The Answer sheet (ADR 0078).
    Answer(Entity<AnswerSheet>, Subscription),
    /// The Edit Labels sheet (ADR 0087).
    Labels(Entity<LabelsSheet>, Subscription),
    /// The question before a kill (issue #27).
    Kill(Entity<KillSheet>, Subscription),
    /// Send to Agents… (ADR 0083).
    Send(Entity<SendSheet>, Subscription),
    /// Search All Agents (⇧⌘F, ADR 0098).
    SearchAll(Entity<SearchAllView>, Subscription),
    /// The command palette (⌘P) and what its rows stand for.
    Palette(Entity<PickerView>, Vec<Entry>, Subscription),
    /// The agent switcher (⌃Tab).
    Switcher(Switcher),
    /// The peek (ADR 0103).
    Peek(Entity<PeekView>, Subscription),
    /// The ⌘/ cheat sheet, and its focus (for Escape).
    Shortcuts(Vec<Section>, FocusHandle),
    /// The connect sheet (drawn unless quiet).
    Connect(Entity<ConnectSheet>, [Subscription; 2]),
    /// Run Action…, Archive Worktree… or a refusal (ADR 0094).
    Action(Entity<ActionSheet>, Subscription),
}

impl Modal {
    /// Stays open when the session switches target: these name nothing
    /// of the old target's layout or agents.
    fn survives_target_switch(&self) -> bool {
        match self {
            Modal::Shortcuts(..) | Modal::Connect(..) | Modal::SearchAll(..) => true,
            Modal::NewAgent(..)
            | Modal::Rename(..)
            | Modal::Answer(..)
            | Modal::Labels(..)
            | Modal::Kill(..)
            | Modal::Send(..)
            | Modal::Palette(..)
            | Modal::Switcher(..)
            | Modal::Action(..)
            | Modal::Peek(..) => false,
        }
    }

    /// The agent it is about went from the list (or, for Answer, its
    /// question closed): the sheet would act on nothing.
    fn target_gone(&self, app: &AppState, cx: &App) -> bool {
        let info = |id: u64| app.agents.iter().find(|r| r.info.id == id).map(|r| &r.info);
        match self {
            Modal::Rename(s, _) => info(s.read(cx).agent_id).is_none(),
            Modal::Labels(s, _) => info(s.read(cx).agent_id).is_none(),
            Modal::Peek(p, _) => info(p.read(cx).agent()).is_none(),
            Modal::Answer(s, _) => {
                let s = s.read(cx);
                info(s.agent_id)
                    .and_then(open_question)
                    .is_none_or(|q| q.id != s.question.id)
            }
            Modal::Kill(s, _) => s.read(cx).agent_ids.iter().any(|&id| info(id).is_none()),
            Modal::NewAgent(..)
            | Modal::Send(..)
            | Modal::SearchAll(..)
            | Modal::Palette(..)
            | Modal::Switcher(..)
            | Modal::Shortcuts(..)
            | Modal::Action(..)
            | Modal::Connect(..) => false,
        }
    }
}

/// The settings the window's look and motion follow (re-applied when one
/// changes).
#[derive(Clone, Copy, PartialEq)]
struct Look {
    theme: ThemeChoice,
    opacity: f32,
    blur: f32,
    motion: MotionChoice,
    glass: GlassChoice,
}

impl Look {
    fn of(c: &GuiConfig) -> Self {
        Look {
            theme: c.theme,
            opacity: c.window_opacity,
            blur: c.blur,
            motion: c.reduce_motion,
            glass: c.glass,
        }
    }
}

/// The window's root view, over [`AppState`].
pub struct Workspace {
    app: Entity<AppState>,
    /// The one sheet, picker or popover over the panes, if any.
    modal: Option<Modal>,
    /// The sidebar rows picked with ⌘-click and ⇧-click (ADR 0083).
    row_selection: RowSelection,
    /// The palette's recent picks, most recent first.
    palette_recents: Recents,
    /// The split chooser of each pane a split opened that is still empty,
    /// with the folder its choices start in.
    choosers: HashMap<PaneId, (Entity<SplitChooserView>, String, Subscription)>,
    /// Some pane view may still hold a chooser (the per-frame sync is
    /// skipped otherwise).
    choosers_drawn: bool,
    /// The agents the focused pane showed, most recent first.
    agent_mru: AgentMru,
    /// The [`AppState::session_epoch`] last seen; a change closes the
    /// modal (unless it [survives](Modal::survives_target_switch)), and a pick from a context menu opened before is
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
    /// The sidebar's filter field (⌥⌘F), its status filter, and the
    /// tabs' focus (issue #40).
    pub(crate) filter_field: Entity<TextField>,
    status_filter: StatusFilter,
    sidebar_focus: FocusHandle,
    /// The sidebar groups and tabs the last frame drew (gpui's test
    /// selectors outlive the frame that drew them).
    #[cfg(test)]
    drawn_sidebar: Vec<(String, Vec<u64>)>,
    /// The settings the window last applied.
    look: Look,
    /// Panes easing in (opened or split), since when.
    entering: HashMap<PaneId, Instant>,
    /// Redraws while a new child's highlight shows (ADR 0079).
    new_child_timer: controls::RedrawTimer,
    /// What the window draws behind the content (blur, glass, opaque).
    backdrop: Backdrop,
    /// A split divider being dragged.
    split_drag: SplitDrag,
    /// A sidebar row or a pane header being dragged (ADR 0059).
    pub(super) pane_drag: PaneDrag,
    /// The graph view (⌘⇧G, ADR 0073), kept while hidden so its collapse,
    /// selection and viewport last the session; and whether it is shown
    /// in place of the sidebar and panes.
    graph: (Entity<GraphView>, Subscription),
    graph_shown: bool,
    /// Pinches reach the graph through it (gpui has no pinch event).
    _magnify: Option<native::MagnifyMonitor>,
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
                // The graph's collapse and selection name the old agents.
                this.graph = new_graph(&this.app, window, cx);
                // The new session watches message lines only when asked.
                this.watch_message_lines(cx);
                // The choosers' pane ids name panes of the old layout,
                // the picked rows agents of the old target.
                this.choosers.clear();
                this.row_selection = RowSelection::default();
                if this
                    .modal
                    .as_ref()
                    .is_some_and(|m| !m.survives_target_switch())
                {
                    this.close_modal(window, cx);
                }
            }
            // A sheet for an agent that went (or a question that closed).
            if this
                .modal
                .as_ref()
                .is_some_and(|m| m.target_gone(app.read(cx), cx))
            {
                this.close_modal(window, cx);
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
        let backdrop = backdrop(&config);
        if backdrop.blurred() {
            stronger_blur(window, config.blur);
        }
        let theme = window_theme(window, &config);
        native::set_glass(
            window,
            backdrop.regions(),
            Some(glass_tint(&window_tokens(window, &config))),
        );
        // Coming back to the window is when a fleetd installed meanwhile
        // is worth noticing (one stat, never per frame).
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            let active = window.is_window_active();
            this.app.update(cx, |a, cx| {
                a.set_window_active(active, cx);
                if active {
                    a.refresh_update_available(cx);
                }
            });
        });
        app.update(cx, |a, cx| {
            a.set_theme(theme, cx);
            a.set_window_active(window.is_window_active(), cx);
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
        let filter_field = cx.new(|cx| TextField::new(String::new(), "Filter", cx));
        let filtered = cx.observe(&filter_field, |_, _, cx| cx.notify());
        let graph = new_graph(&app, window, cx);
        let magnify = pinch_to_graph(window, cx);
        let ws = Workspace {
            app,
            _ticker: ticker,
            modal: None,
            row_selection: RowSelection::default(),
            palette_recents: Recents::default(),
            choosers: HashMap::new(),
            choosers_drawn: false,
            agent_mru: AgentMru::default(),
            epoch,
            pane_focus: HashMap::new(),
            drawn: Vec::new(),
            window_title: String::new(),
            reduce_motion: config.reduce_motion.resolve(native::reduce_motion()),
            look: Look::of(&config),
            entering: HashMap::new(),
            new_child_timer: controls::RedrawTimer::default(),
            backdrop,
            split_drag: SplitDrag::default(),
            pane_drag: PaneDrag::default(),
            filter_field,
            status_filter: StatusFilter::All,
            sidebar_focus: cx.focus_handle(),
            #[cfg(test)]
            drawn_sidebar: Vec::new(),
            graph,
            graph_shown: false,
            _magnify: magnify,
            _subscriptions: vec![observe, appearance, activation, filtered],
        };
        cx.set_global(ReducedMotion(ws.reduce_motion));
        ws.focus_pane(ws.app.read(cx).focused(), window, cx);
        ws
    }

    pub fn app(&self) -> &Entity<AppState> {
        &self.app
    }

    pub fn graph(&self) -> &Entity<GraphView> {
        &self.graph.0
    }

    /// The graph view is shown in place of the sidebar and panes.
    pub fn graph_shown(&self) -> bool {
        self.graph_shown
    }

    /// ⌘⇧G or the titlebar's graph button: switches between the panes and
    /// the graph view.
    pub fn toggle_graph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        self.graph_shown = !self.graph_shown;
        self.watch_message_lines(cx);
        self.close_graph_panel(window, cx);
        if self.graph_shown {
            self.graph.0.read(cx).focus(window);
        } else {
            self.focus_current(window, cx);
        }
        cx.notify();
    }

    /// Closes the graph's terminal panel (ADR 0105): the graph was hidden.
    fn close_graph_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.graph.0.update(cx, |g, cx| g.close_panel(window, cx));
    }

    /// The session asks for message edges only while the graph is shown.
    fn watch_message_lines(&self, cx: &mut Context<Self>) {
        let shown = self.graph_shown;
        self.app.update(cx, |a, cx| {
            a.watch_message_edges(shown);
            cx.notify();
        });
    }

    fn graph_event(
        &mut self,
        _: &Entity<GraphView>,
        event: &GraphEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.graph_shown {
            return;
        }
        // Answering and peeking stay in the graph.
        if let GraphEvent::Answer(id) = *event {
            self.open_answer(id, window, cx);
            return;
        }
        if let GraphEvent::Menu(id) = *event {
            self.open_row_menu(id, cx);
            return;
        }
        if let GraphEvent::Peek(id) = *event {
            self.open_peek(id, window, cx);
            return;
        }
        self.graph_shown = false;
        self.watch_message_lines(cx);
        self.close_graph_panel(window, cx);
        if let GraphEvent::Open(id) = *event {
            self.app.update(cx, |a, cx| a.reveal_agent(id, cx));
        }
        self.focus_current(window, cx);
        cx.notify();
    }

    /// The theme, blur and motion for the system's settings and ours.
    fn apply_look(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config = self.app.read(cx).config().clone();
        // An opaque window stays opaque (Reduce Transparency is read when
        // it opens); otherwise the glass setting applies live.
        let backdrop = match self.backdrop {
            Backdrop::Opaque => Backdrop::Opaque,
            _ => backdrop(&config),
        };
        if backdrop != self.backdrop {
            window.set_background_appearance(background_of(backdrop));
            self.backdrop = backdrop;
        }
        if backdrop.blurred() {
            stronger_blur(window, config.blur);
        }
        let theme = window_theme(window, &config);
        native::set_glass(
            window,
            backdrop.regions(),
            Some(glass_tint(&window_tokens(window, &config))),
        );
        self.reduce_motion = config.reduce_motion.resolve(native::reduce_motion());
        cx.set_global(ReducedMotion(self.reduce_motion));
        self.app.update(cx, |a, cx| a.set_theme(theme, cx));
        window.refresh();
    }

    /// What the window draws behind its content (`[gui] glass`, ADR 0049).
    pub fn backdrop(&self) -> Backdrop {
        self.backdrop
    }

    /// Motion is reduced (sheets and the toast appear without sliding).
    pub fn reduces_motion(&self) -> bool {
        self.reduce_motion
    }

    /// The panes easing in (opened or split just now).
    pub fn easing_panes(&self) -> Vec<PaneId> {
        self.entering.keys().copied().collect()
    }

    /// The panes the last frame drew (one while maximized).
    /// Where the agent or pane being dragged would drop now (ADR 0059).
    pub fn drop_target(&self) -> Option<DropTarget> {
        self.pane_drag.target()
    }

    pub fn drawn_panes(&self) -> &[PaneId] {
        &self.drawn
    }

    pub fn sheet(&self) -> Option<&Entity<NewAgentSheet>> {
        match &self.modal {
            Some(Modal::NewAgent(s, _)) => Some(s),
            _ => None,
        }
    }

    pub fn rename_sheet(&self) -> Option<&Entity<RenameSheet>> {
        match &self.modal {
            Some(Modal::Rename(s, _)) => Some(s),
            _ => None,
        }
    }

    pub fn labels_sheet(&self) -> Option<&Entity<LabelsSheet>> {
        match &self.modal {
            Some(Modal::Labels(s, _)) => Some(s),
            _ => None,
        }
    }

    /// The command palette, while it is open.
    pub fn palette(&self) -> Option<&Entity<PickerView>> {
        match &self.modal {
            Some(Modal::Palette(p, _, _)) => Some(p),
            _ => None,
        }
    }

    /// `pane`'s split chooser, while it asks what to start there.
    pub fn chooser(&self, pane: PaneId) -> Option<&Entity<SplitChooserView>> {
        self.choosers.get(&pane).map(|(c, _, _)| c)
    }

    /// Search All Agents, while it is open.
    pub fn search_all(&self) -> Option<&Entity<SearchAllView>> {
        match &self.modal {
            Some(Modal::SearchAll(v, _)) => Some(v),
            _ => None,
        }
    }

    /// The agent switcher, while it is open.
    pub fn switcher(&self) -> Option<&Entity<PickerView>> {
        match &self.modal {
            Some(Modal::Switcher(s)) => Some(&s.view),
            _ => None,
        }
    }

    pub fn connect_sheet(&self) -> Option<&Entity<ConnectSheet>> {
        match &self.modal {
            Some(Modal::Connect(s, _)) => Some(s),
            _ => None,
        }
    }

    /// The connect sheet is drawn (not a quiet startup attempt).
    pub fn connect_sheet_shown(&self, cx: &App) -> bool {
        self.connect_sheet()
            .is_some_and(|sheet| !sheet.read(cx).quiet)
    }

    /// A sheet is open over the panes.
    fn has_modal(&self) -> bool {
        self.modal.is_some()
    }

    /// The ⌘/ cheat sheet is shown.
    #[cfg(test)]
    fn shortcuts_shown(&self) -> bool {
        matches!(self.modal, Some(Modal::Shortcuts(..)))
    }

    /// Takes the modal down, without giving the keyboard back (the peek's
    /// connection is closed).
    fn drop_modal(&mut self, cx: &mut Context<Self>) {
        if let Some(Modal::Peek(..)) = self.modal.take() {
            self.app.update(cx, |a, cx| a.close_peek(cx));
        }
        cx.notify();
    }

    /// Closes the modal, if one is open, and gives the keyboard back: to
    /// the graph from a peek over it, to the focused pane otherwise.
    fn close_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(modal) = &self.modal else {
            return;
        };
        let to_graph = matches!(modal, Modal::Peek(..)) && self.graph_shown;
        self.drop_modal(cx);
        if to_graph {
            self.graph.0.read(cx).focus(window);
        } else {
            self.focus_current(window, cx);
        }
    }

    pub fn kill_sheet(&self) -> Option<&Entity<KillSheet>> {
        match &self.modal {
            Some(Modal::Kill(s, _)) => Some(s),
            _ => None,
        }
    }

    /// The Send to Agents sheet, while it is open.
    pub fn send_sheet(&self) -> Option<&Entity<SendSheet>> {
        match &self.modal {
            Some(Modal::Send(s, _)) => Some(s),
            _ => None,
        }
    }

    /// The sidebar rows picked with ⌘-click and ⇧-click.
    pub fn row_selection(&self) -> &RowSelection {
        &self.row_selection
    }

    /// ⌘/: shows the cheat sheet of every key binding, or hides it.
    pub fn toggle_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.modal, Some(Modal::Shortcuts(..))) {
            self.close_modal(window, cx);
        } else if !self.has_modal() {
            let handle = cx.focus_handle();
            window.focus(&handle);
            self.modal = Some(Modal::Shortcuts(keymap::cheat_sheet(cx), handle));
        }
        cx.notify();
    }

    /// ⌘⇧T: the pane closed last, with its agent.
    fn reopen_closed_pane(
        &mut self,
        _: &ReopenClosedPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_modal() {
            return;
        }
        if self.app.update(cx, |a, cx| a.reopen_closed(cx)).is_some() {
            self.focus_current(window, cx);
        }
    }

    /// Opens the connect sheet (or brings it up), showing `reason`.
    pub fn open_connect_sheet(
        &mut self,
        reason: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(Modal::Connect(sheet, _)) = &self.modal {
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
        // Over everything else: whatever was open closes.
        self.close_modal(window, cx);
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
        self.modal = Some(Modal::Connect(sheet.clone(), [events, shown]));
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
        self.close_modal(window, cx);
    }

    fn pane_focus_handle(&self, pane: PaneId, cx: &App) -> Option<FocusHandle> {
        let app = self.app.read(cx);
        if let Some(diff) = app.diff_pane(pane) {
            return Some(diff.read(cx).focus_handle().clone());
        }
        app.pane_view(pane)
            .map(|v| v.read(cx).focus_handle().clone())
    }

    fn focus_pane(&self, pane: PaneId, window: &mut Window, cx: &mut App) {
        self.app.update(cx, |a, cx| a.set_focused(pane, cx));
        if let Some((chooser, _, _)) = self.choosers.get(&pane) {
            chooser.read(cx).focus(window, cx);
        } else if let Some(handle) = self.pane_focus_handle(pane, cx) {
            window.focus(&handle);
        }
    }

    fn focus_current(&self, window: &mut Window, cx: &mut App) {
        let pane = self.app.read(cx).focused();
        self.focus_pane(pane, window, cx);
    }

    /// Splits the focused pane; the new pane asks what to start in it.
    fn split(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let origin = self.app.read(cx).focused();
        if let Some(pane) = self.app.update(cx, |a, cx| a.split(axis, cx)) {
            self.open_chooser(pane, origin, window, cx);
            self.focus_current(window, cx);
        }
    }

    /// The split chooser in `pane`: its choices start in `origin`'s
    /// folder, and its list leaves out the agents on screen.
    fn open_chooser(
        &mut self,
        pane: PaneId,
        origin: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let presets = self.app.update(cx, |a, _| a.presets());
        let app = self.app.read(cx);
        let dir = app.start_dir(origin);
        let agents = off_screen_agents(&app.agents, &app.on_screen(), &app.home);
        let rows = choice_rows(
            &app.config.default_command,
            &dir,
            &app.home,
            agents.len(),
            &presets,
        );
        let chooser = cx.new(|cx| SplitChooserView::new(rows, agents, cx));
        let sub = cx.subscribe_in(&chooser, window, move |this, _, event, window, cx| {
            this.chooser_event(pane, *event, window, cx);
        });
        self.choosers.insert(pane, (chooser, dir, sub));
        self.follow_choosers(window, cx);
    }

    /// A choice: the chooser goes, and its pane starts what was picked,
    /// shows the agent, or closes.
    fn chooser_event(
        &mut self,
        pane: PaneId,
        event: ChooserEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, dir, _)) = self.choosers.remove(&pane) else {
            return;
        };
        self.follow_choosers(window, cx);
        self.app.update(cx, |a, cx| a.set_focused(pane, cx));
        if self.app.read(cx).focused() != pane {
            return;
        }
        match event {
            ChooserEvent::NewAgent if self.app.read(cx).config.default_command.is_empty() => {
                self.open_new_agent(window, cx);
                return;
            }
            ChooserEvent::NewAgent => self.spawn_default_in(dir, window, cx),
            ChooserEvent::Preset(index) => self.spawn_preset_in(index, dir, cx),
            ChooserEvent::Terminal => {
                let result = self.app.update(cx, |a, cx| {
                    let req = a.terminal_request(dir);
                    a.spawn(req, cx)
                });
                if let Err(e) = result {
                    self.app.update(cx, |a, cx| a.show_error(e, cx));
                }
            }
            ChooserEvent::Agent(id) => self.show_agent(id, window, cx),
            ChooserEvent::Cancel => {
                self.app.update(cx, |a, cx| a.close_focused(cx));
            }
        }
        self.focus_current(window, cx);
        cx.notify();
    }

    /// Choosers go once their pane is gone or gets an agent; each pane
    /// view draws its own (a pane id can get a new view).
    fn follow_choosers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.choosers.is_empty() && !self.choosers_drawn {
            return;
        }
        let app = self.app.read(cx);
        // Every tab's: a hidden tab's chooser is there when it is shown again.
        let panes = app.tabs().panes();
        let mut refocus = false;
        self.choosers.retain(|pane, (chooser, _, _)| {
            let kept = panes.contains(pane) && !app.pane_filled(*pane);
            // The keyboard goes back to the pane the chooser was in.
            refocus |= !kept && chooser.read(cx).contains_focus(window, cx);
            kept
        });
        let views: Vec<(PaneId, Entity<PaneView>)> = panes
            .into_iter()
            .filter_map(|p| Some((p, app.pane_view(p)?.clone())))
            .collect();
        if refocus {
            cx.defer_in(window, |this, window, cx| this.focus_current(window, cx));
        }
        for (pane, view) in views {
            let chooser = self.choosers.get(&pane).map(|(c, _, _)| c.clone().into());
            view.update(cx, |v, cx| v.set_chooser(chooser, cx));
        }
        self.choosers_drawn = !self.choosers.is_empty();
    }

    fn close_pane(&mut self, _: &ClosePane, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let pane = self.app.read(cx).focused();
        self.choosers.remove(&pane);
        self.app.update(cx, |a, cx| a.close_focused(cx));
        self.focus_current(window, cx);
    }

    fn focus_dir(&mut self, dir: Dir, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        if let Some(pane) = self.app.read(cx).neighbour(dir) {
            self.focus_pane(pane, window, cx);
        }
    }

    fn focus_number(&mut self, n: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        if let Some(pane) = self.app.read(cx).pane_by_number(n) {
            self.focus_pane(pane, window, cx);
        }
    }

    fn new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.add_tab(window, cx);
    }

    /// ⌘⌥T (or the strip's New tab button): a new tab with one empty
    /// pane, focused.
    pub fn add_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        if self.app.update(cx, |a, cx| a.new_tab(cx)).is_some() {
            self.focus_current(window, cx);
        }
    }

    /// ⌘⌥W: closes the shown tab, detaching its agents.
    fn close_shown_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self.app.read(cx).tabs().active();
        self.close_tab(shown, window, cx);
    }

    /// Closes tab `i` (0-based; its close button), detaching its agents.
    pub fn close_tab(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        self.app.update(cx, |a, cx| a.close_tab(i, cx));
        self.focus_current(window, cx);
    }

    /// Shows tab `i` (0-based) and focuses its focused pane.
    pub fn select_tab(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        if self.app.update(cx, |a, cx| a.select_tab(i, cx)) {
            self.focus_current(window, cx);
        }
    }

    fn select_numbered_tab(&mut self, a: &SelectTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = a.number.checked_sub(1) {
            self.select_tab(i, window, cx);
        }
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_modal() && self.app.update(cx, |a, cx| a.next_tab(cx)) {
            self.focus_current(window, cx);
        }
    }

    fn prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_modal() && self.app.update(cx, |a, cx| a.prev_tab(cx)) {
            self.focus_current(window, cx);
        }
    }

    /// The strip's labels: each tab's number and its focused pane's program.
    fn tab_labels(&self, cx: &App) -> Vec<String> {
        let app = self.app.read(cx);
        let tabs = app.tabs();
        (0..tabs.len())
            .map(|i| {
                let program = tabs
                    .tab_focused(i)
                    .and_then(|p| tabs.agent(p))
                    .and_then(|id| app.agents.iter().find(|r| r.info.id == id))
                    .map(|r| program_name(&r.info.program));
                tab_label(i + 1, program.as_deref())
            })
            .collect()
    }

    /// Shows the agent in the focused pane (a sidebar click).
    pub fn show_agent(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.app.update(cx, |a, cx| a.show_agent(agent_id, cx));
        self.focus_current(window, cx);
    }

    /// Files from Finder dropped on `agent`'s sidebar row.
    pub(super) fn drop_paths_on_agent(
        &mut self,
        agent: u64,
        paths: &ExternalPaths,
        cx: &mut Context<Self>,
    ) {
        if self.has_modal() {
            return;
        }
        let paths: Vec<String> = paths
            .paths()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        self.app
            .update(cx, |a, cx| a.drop_paths_on_agent(agent, &paths, cx));
    }

    /// The peek, while it is open (ADR 0103).
    pub fn peek(&self) -> Option<&Entity<PeekView>> {
        match &self.modal {
            Some(Modal::Peek(p, _)) => Some(p),
            _ => None,
        }
    }

    /// Peeks at `agent_id` in a popover, without changing the layout. One
    /// peek at a time: another one open is replaced. Not over a sheet.
    pub fn open_peek(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.peek().is_none() && self.has_modal() {
            return;
        }
        if !self
            .app
            .read(cx)
            .agents
            .iter()
            .any(|r| r.info.id == agent_id)
        {
            return;
        }
        let app = self.app.clone();
        let view = cx.new(|cx| PeekView::new(app, agent_id, cx));
        let sub = cx.subscribe_in(&view, window, Self::peek_event);
        view.read(cx).focus(window, cx);
        self.modal = Some(Modal::Peek(view, sub));
        cx.notify();
    }

    fn peek_event(
        &mut self,
        _: &Entity<PeekView>,
        event: &PeekEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match *event {
            PeekEvent::Close => self.close_modal(window, cx),
            PeekEvent::OpenInPane(id) => {
                self.close_modal(window, cx);
                if self.graph_shown {
                    self.graph_shown = false;
                    self.watch_message_lines(cx);
                    self.close_graph_panel(window, cx);
                }
                self.show_agent(id, window, cx);
                cx.notify();
            }
        }
    }

    /// The sidebar's groups under its filter and the target's collapsed
    /// groups.
    fn sidebar_groups(&self, cx: &App) -> Vec<SidebarGroup> {
        let app = self.app.read(cx);
        let filter = Filter {
            query: self.filter_field.read(cx).text().to_owned(),
            status: self.status_filter,
        };
        let titles = &app.titles;
        narrow_groups(
            group_agents_by(&app.agents, &app.home, app.group_by()),
            &app.home,
            &filter,
            &|r| tab_title(r, titles),
            &app.statuses,
            &app.collapsed_groups(),
            app.disclosure(),
        )
    }

    /// ⌥⌘F: the keyboard goes to the sidebar's filter field.
    fn focus_filter(&mut self, _: &FocusFilter, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        window.focus(self.filter_field.read(cx).focus_handle());
    }

    /// A click on the sidebar's tabs gives them the keyboard, so typing
    /// filters (a click on a tab then focuses its pane).
    pub(super) fn focus_sidebar(&mut self, window: &mut Window, _: &mut Context<Self>) {
        window.focus(&self.sidebar_focus);
    }

    /// A key typed while the sidebar's tabs have the keyboard starts the
    /// filter with it.
    pub(super) fn sidebar_key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sidebar_focus.is_focused(window) {
            return;
        }
        let m = &ev.keystroke.modifiers;
        if m.platform || m.control {
            return;
        }
        if ev.keystroke.key == "escape" {
            self.row_selection.clear();
            self.focus_current(window, cx);
            cx.stop_propagation();
            return;
        }
        // Space peeks at the selected row, as Quick Look (ADR 0103).
        if ev.keystroke.key == "space"
            && !m.alt
            && !m.shift
            && let Some(id) = sidebar_target(&self.row_selection)
        {
            self.open_peek(id, window, cx);
            cx.stop_propagation();
            return;
        }
        let Some(text) = ev
            .keystroke
            .key_char
            .clone()
            .filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
        else {
            return;
        };
        let field = self.filter_field.clone();
        window.focus(field.read(cx).focus_handle());
        field.update(cx, |f, cx| f.insert(&text, cx));
        cx.stop_propagation();
    }

    /// In the filter field: Escape clears it, then gives the keyboard
    /// back to the focused pane; Enter shows the first agent listed.
    pub(super) fn filter_key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev.keystroke.key.as_str() {
            "escape" => {
                if self.filter_field.read(cx).text().is_empty() {
                    self.focus_current(window, cx);
                } else {
                    self.filter_field
                        .update(cx, |f, cx| f.set_text(String::new(), cx));
                }
                cx.stop_propagation();
            }
            "enter" => {
                let first = self
                    .sidebar_groups(cx)
                    .iter()
                    .find_map(|g| g.agents.first().map(|r| r.info.id));
                if let Some(id) = first {
                    self.show_agent(id, window, cx);
                }
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    pub(super) fn set_status_filter(&mut self, filter: StatusFilter, cx: &mut Context<Self>) {
        self.status_filter = filter;
        cx.notify();
    }

    pub(super) fn toggle_disclosure(&mut self, id: u64, children: usize, cx: &mut Context<Self>) {
        self.app
            .update(cx, |a, cx| a.toggle_disclosure(id, children, cx));
    }

    pub(super) fn toggle_group(&mut self, title: &str, cx: &mut Context<Self>) {
        self.app.update(cx, |a, cx| a.toggle_group(title, cx));
    }

    /// Groups the sidebar `by` folder or repository (ADR 0085).
    pub(crate) fn set_group_by(&mut self, by: GroupBy, cx: &mut Context<Self>) {
        self.app.update(cx, |a, cx| a.set_group_by(by, cx));
    }

    /// A group header's native menu (at the right-click being handled):
    /// group by folder or by repository.
    pub(super) fn open_group_menu(&mut self, cx: &mut Context<Self>) {
        let items = group_menu(self.app.read(cx).group_by());
        let lines = items
            .iter()
            .map(|&(by, checked)| {
                Some(MenuLine {
                    title: by.title().to_owned(),
                    enabled: true,
                    shortcut: None,
                    checked,
                })
            })
            .collect();
        context_menu::show(lines, cx, move |index, cx| {
            if let Some(&(by, _)) = items.get(index) {
                with_workspace(cx, |ws, _, cx| ws.set_group_by(by, cx));
            }
        });
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
        let bulk = self.bulk_targets(cx);
        let lines = context_menu::lines(&items, |item| {
            let action = match item {
                RowMenuItem::KillSelected => Some(BulkAction::Kill),
                RowMenuItem::RestartSelected => Some(BulkAction::Restart),
                RowMenuItem::RemoveSelected => Some(BulkAction::Remove),
                _ => None,
            };
            MenuLine {
                title: action.map_or_else(|| item.title().to_owned(), |a| a.menu_title(&bulk)),
                enabled: action.is_none_or(|a| a.disabled_reason(&bulk).is_none()),
                shortcut: None,
                checked: false,
            }
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
        if epoch != self.epoch || self.has_modal() {
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
            RowMenuItem::Rename => self.open_rename(agent_id, window, cx),
            RowMenuItem::EditLabels => self.open_labels(agent_id, window, cx),
            RowMenuItem::Duplicate => self.duplicate_agent(agent_id, window, cx),
            RowMenuItem::SaveOutput => output::save_output(self.app.clone(), agent_id, cx),
            RowMenuItem::OpenOutput => output::open_output(self.app.clone(), agent_id, cx),
            RowMenuItem::SendTo | RowMenuItem::SendToSelected => {
                let targets = self.row_selection.targets_from(agent_id);
                self.open_send(&targets, window, cx);
            }
            RowMenuItem::Kill => self.kill(&Kill { agent_id }, window, cx),
            RowMenuItem::Restart => self.restart(&Restart { agent_id }, window, cx),
            RowMenuItem::Remove => self.remove(&Remove { agent_id }, window, cx),
            RowMenuItem::KillSelected => self.bulk(BulkAction::Kill, window, cx),
            RowMenuItem::RestartSelected => self.bulk(BulkAction::Restart, window, cx),
            RowMenuItem::RemoveSelected => self.bulk(BulkAction::Remove, window, cx),
            RowMenuItem::ShowChanges => self.show_changes(agent_id, window, cx),
            RowMenuItem::ArchiveWorktree => self.open_archive(agent_id, window, cx),
        }
    }

    /// A pane's native context menu: on its header, or on its grid
    /// (`on_grid`, adding Copy and Paste).
    fn open_pane_menu(&mut self, pane: PaneId, on_grid: bool, cx: &mut Context<Self>) {
        let (state, subject) = {
            let app = self.app.read(cx);
            let view = app.pane_view(pane).map(|v| v.read(cx));
            let subject = app
                .layout()
                .agent(pane)
                .and_then(|id| app.agents.iter().find(|r| r.info.id == id))
                .map_or(Subject::Agent, |r| Subject::of(&r.info));
            let (can_scroll, scrolled, has_history) =
                view.map_or((false, false, false), |v| v.scroll_state());
            let state = PaneMenuState {
                on_grid,
                has_selection: view.is_some_and(|v| v.has_selection()),
                can_scroll,
                scrolled,
                has_history,
                can_split: app.layout().panes().len() < MAX_PANES,
                maximized: app.maximized == Some(pane),
                restartable: view.is_some_and(|v| v.restartable()),
                has_agent: app.layout().agent(pane).is_some(),
                killable: app
                    .layout()
                    .agent(pane)
                    .is_some_and(|id| kill_sheet::running(app, id)),
            };
            (state, subject)
        };
        let mut items = pane_menu(state);
        // Show Changes and Archive Worktree… before Close Pane.
        let agent = self.app.read(cx).layout().agent(pane);
        let close = items.len().saturating_sub(2);
        items.splice(close..close, self.workflow_pane_items(agent, cx));
        let lines = context_menu::lines(&items, |(item, enabled)| MenuLine {
            title: item.title(subject),
            enabled,
            shortcut: item.shortcut(),
            checked: false,
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
        if epoch != self.epoch || self.has_modal() {
            return;
        }
        let event = match item {
            PaneMenuItem::Copy
            | PaneMenuItem::Paste
            | PaneMenuItem::SelectAll
            | PaneMenuItem::ScrollToTop
            | PaneMenuItem::ScrollToBottom
            | PaneMenuItem::ClearScrollback => {
                // The pane's own actions, as its keys perform them.
                self.focus_pane(pane, window, cx);
                let action: Box<dyn Action> = match item {
                    PaneMenuItem::Copy => Box::new(Copy),
                    PaneMenuItem::Paste => Box::new(Paste),
                    PaneMenuItem::SelectAll => Box::new(SelectAll),
                    PaneMenuItem::ScrollToTop => Box::new(ScrollToTop),
                    PaneMenuItem::ScrollToBottom => Box::new(ScrollToBottom),
                    _ => Box::new(ClearScrollback),
                };
                window.dispatch_action(action, cx);
                return;
            }
            PaneMenuItem::SplitRight => PaneEvent::SplitRight,
            PaneMenuItem::SplitDown => PaneEvent::SplitDown,
            PaneMenuItem::Maximize | PaneMenuItem::Restore => PaneEvent::ToggleMaximize,
            PaneMenuItem::Restart => PaneEvent::Restart,
            PaneMenuItem::Rename => PaneEvent::Rename,
            PaneMenuItem::EditLabels => PaneEvent::EditLabels,
            PaneMenuItem::Duplicate => PaneEvent::Duplicate,
            PaneMenuItem::Kill => PaneEvent::Kill,
            PaneMenuItem::Close => PaneEvent::Close,
            PaneMenuItem::ShowChanges | PaneMenuItem::ArchiveWorktree => {
                if let Some(agent) = self.app.read(cx).layout().agent(pane) {
                    if item == PaneMenuItem::ShowChanges {
                        self.show_changes(agent, window, cx);
                    } else {
                        self.open_archive(agent, window, cx);
                    }
                }
                return;
            }
            PaneMenuItem::CopyLastOutput | PaneMenuItem::SaveOutput | PaneMenuItem::OpenOutput => {
                self.output_in_pane(pane, item, cx);
                return;
            }
        };
        self.pane_event(pane, event, window, cx);
    }

    pub fn open_new_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_new_agent_with(Prefill::default(), window, cx);
    }

    /// The New agent sheet, its fields prefilled from a preset or an agent
    /// being duplicated (ADR 0084). An open sheet is only focused.
    pub fn open_new_agent_with(
        &mut self,
        mut prefill: Prefill,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &self.modal {
            Some(Modal::Connect(..)) => return,
            Some(Modal::NewAgent(sheet, _)) => {
                sheet.read(cx).focus(window, cx);
                return;
            }
            // It opens over any other (which closes).
            _ => self.drop_modal(cx),
        }
        let presets = self.app.update(cx, |a, _| a.presets());
        let app = self.app.read(cx);
        // Settings › Agents: a default folder beats the recent one.
        if prefill.dir.is_none() && !app.config.default_dir.is_empty() {
            prefill.dir = Some(app.config.default_dir.clone());
        }
        let setup = SheetSetup {
            default_command: app.config.default_command.clone(),
            dirs: app.dir_choices(),
            recent_commands: app.recent_commands(),
            presets,
            prefill,
            worktree: self.worktree_setup(cx),
        };
        let target = app.target.clone();
        let taken = app.live_names();
        let sheet = cx.new(|cx| NewAgentSheet::new(setup, &target, taken, cx));
        let sub = cx.subscribe_in(&sheet, window, Self::sheet_event);
        sheet.read(cx).focus(window, cx);
        self.modal = Some(Modal::NewAgent(sheet, sub));
        cx.notify();
    }

    /// Opens the Rename sheet for an agent (a double-click on its name, or
    /// Rename… in a menu).
    pub fn open_rename(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let Some(info) = self
            .app
            .read(cx)
            .agents
            .iter()
            .find(|r| r.info.id == agent_id)
            .map(|r| r.info.clone())
        else {
            return;
        };
        let name = name_of(&info).unwrap_or_default().to_owned();
        let what = program_and_id(&info);
        let subject = Subject::of(&info);
        let sheet = cx.new(|cx| RenameSheet::new(agent_id, name, what, subject, cx));
        let sub = cx.subscribe_in(&sheet, window, Self::rename_event);
        sheet.read(cx).focus(window, cx);
        self.modal = Some(Modal::Rename(sheet, sub));
        cx.notify();
    }

    fn rename_event(
        &mut self,
        _sheet: &Entity<RenameSheet>,
        event: &RenameEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let RenameEvent::Rename { agent_id, name } = event {
            self.app.read(cx).rename(*agent_id, name.clone());
        }
        self.close_modal(window, cx);
    }

    /// Opens the Answer sheet for an agent's open question (Reply… on its
    /// row, header or graph card, ADR 0078).
    pub fn open_answer(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let Some(info) = self
            .app
            .read(cx)
            .agents
            .iter()
            .find(|r| r.info.id == agent_id)
            .map(|r| r.info.clone())
        else {
            return;
        };
        let Some(question) = open_question(&info) else {
            return;
        };
        let what = format!(
            "{} · {}",
            name_of(&info).unwrap_or(&info.program),
            program_and_id(&info)
        );
        let sheet = cx.new(|cx| AnswerSheet::new(agent_id, what, question, cx));
        let sub = cx.subscribe_in(&sheet, window, Self::answer_event);
        sheet.read(cx).focus(window, cx);
        self.modal = Some(Modal::Answer(sheet, sub));
        cx.notify();
    }

    pub fn answer_sheet(&self) -> Option<&Entity<AnswerSheet>> {
        match &self.modal {
            Some(Modal::Answer(s, _)) => Some(s),
            _ => None,
        }
    }

    fn answer_event(
        &mut self,
        _sheet: &Entity<AnswerSheet>,
        event: &AnswerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let AnswerEvent::Send {
            agent_id,
            question,
            text,
        } = event
        {
            self.app.read(cx).answer(*agent_id, *question, text.clone());
        }
        self.close_modal(window, cx);
    }

    /// A click on a choice button: answers question `question` of
    /// `agent_id` with it (ADR 0078).
    pub fn answer_with(
        &mut self,
        agent_id: u64,
        question: u64,
        text: String,
        cx: &mut Context<Self>,
    ) {
        self.app.read(cx).answer(agent_id, question, text);
    }

    /// Opens the Edit Labels sheet for an agent (Edit Labels… in the row
    /// and pane menus).
    pub fn open_labels(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let Some(info) = self
            .app
            .read(cx)
            .agents
            .iter()
            .find(|r| r.info.id == agent_id)
            .map(|r| r.info.clone())
        else {
            return;
        };
        let what = match name_of(&info) {
            Some(name) => format!("{name} · {}", program_and_id(&info)),
            None => program_and_id(&info),
        };
        let sheet = cx.new(|cx| LabelsSheet::new(agent_id, info.labels.clone(), what, cx));
        let sub = cx.subscribe_in(&sheet, window, Self::labels_event);
        sheet.read(cx).focus(window, cx);
        self.modal = Some(Modal::Labels(sheet, sub));
        cx.notify();
    }

    fn labels_event(
        &mut self,
        _sheet: &Entity<LabelsSheet>,
        event: &LabelsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let LabelsEvent::Save { agent_id, edit } = event
            && !edit.is_empty()
        {
            self.app
                .read(cx)
                .set_labels(*agent_id, edit.set.clone(), edit.remove.clone());
        }
        self.close_modal(window, cx);
    }

    /// Opens Send to Agents… with `preselect` checked (those running).
    /// Not over a sheet, and not without a running agent.
    pub fn open_send(&mut self, preselect: &[u64], window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        if !self.app.read(cx).agents.iter().any(sendable) {
            self.app.update(cx, |a, cx| {
                a.show_error("No running agent to send to.".to_owned(), cx);
            });
            return;
        }
        let app = self.app.clone();
        let sheet = cx.new(|cx| SendSheet::new(app, preselect, cx));
        let sub = cx.subscribe_in(&sheet, window, Self::send_event);
        sheet.read(cx).focus(window, cx);
        self.modal = Some(Modal::Send(sheet, sub));
        cx.notify();
    }

    /// ⇧⌘M: Send to Agents…, the sidebar's selection checked.
    fn send_to_agents(&mut self, _: &SendToAgents, window: &mut Window, cx: &mut Context<Self>) {
        let preselect = self.row_selection.ids().to_vec();
        self.open_send(&preselect, window, cx);
    }

    fn send_event(
        &mut self,
        _sheet: &Entity<SendSheet>,
        event: &SendEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let SendEvent::Send(plan) = event {
            self.app.update(cx, |a, cx| a.send_text(plan, cx));
        }
        self.close_modal(window, cx);
    }

    /// A click on a row's label chip: the sidebar filter gets `key=value`,
    /// or loses it on a second click.
    /// Approves or denies an agent's pending spawn (ADR 0079).
    pub(super) fn decide_spawn(&mut self, id: u64, approve: bool, cx: &mut Context<Self>) {
        self.app.read(cx).decide_spawn(id, approve);
    }

    pub(super) fn filter_by_label(&mut self, key: &str, value: &str, cx: &mut Context<Self>) {
        let query = chip_filter(self.filter_field.read(cx).text(), key, value);
        self.filter_field.update(cx, |f, cx| f.set_text(query, cx));
        cx.notify();
    }

    /// A click on a sidebar row: ⌘ or ⇧ held picks rows (ADR 0083);
    /// a plain click shows the agent in the focused pane.
    pub(super) fn row_click(
        &mut self,
        agent_id: u64,
        cmd: bool,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let order: Vec<u64> = self
            .sidebar_groups(cx)
            .iter()
            .flat_map(|g| g.agents.iter().map(|r| r.info.id))
            .collect();
        let current = {
            let app = self.app.read(cx);
            app.layout().agent(app.focused())
        };
        match self
            .row_selection
            .click(agent_id, cmd, shift, &order, current)
        {
            RowClick::Show => self.show_agent(agent_id, window, cx),
            RowClick::Selected => cx.notify(),
        }
    }

    fn new_agent(&mut self, _: &NewAgent, window: &mut Window, cx: &mut Context<Self>) {
        self.open_new_agent(window, cx);
    }

    /// ⌘T: a plain login shell, focused (see [`AppState::new_terminal`]).
    pub fn open_new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
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
        self.remember_worktree(sheet, cx);
        match event {
            SheetEvent::Cancel => {}
            SheetEvent::SpawnInWorktree {
                req,
                command,
                worktree,
            } => self.spawn_in_worktree(req, command, worktree, window, cx),
            SheetEvent::Spawn { req, command } => {
                let result = self.app.update(cx, |a, cx| {
                    a.spawn_from_sheet(req.clone(), command.clone(), cx)
                });
                if let Err(e) = result {
                    sheet.update(cx, |s, cx| s.set_error(e, cx));
                    return;
                }
            }
        }
        self.close_modal(window, cx);
    }

    /// ⌘P: opens the command palette (actions, agents, recent folders),
    /// or closes it if open. Not over a sheet.
    pub fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette().is_some() {
            self.close_modal(window, cx);
            return;
        }
        if self.has_modal() {
            return;
        }
        let state = self.palette_state(cx);
        let presets = self.app.update(cx, |a, _| a.presets());
        let app = self.app.read(cx);
        let mut rows = palette_items(
            |c| command_shortcut(c, window),
            &state,
            &presets,
            &app.agents,
            &app.dir_choices(),
            &app.home,
        );
        rows.extend(self.palette_changes(cx));
        let (entries, items): (Vec<Entry>, Vec<_>) = rows.into_iter().unzip();
        let recents = self.palette_recents.clone();
        let palette =
            cx.new(|cx| PickerView::new(items, recents, "Search actions, agents and folders", cx));
        let sub = cx.subscribe_in(&palette, window, Self::palette_event);
        palette.read(cx).focus(window, cx);
        self.modal = Some(Modal::Palette(palette, entries, sub));
        cx.notify();
    }

    /// What the palette's commands depend on: the shown tab's panes, the
    /// tabs, and the focused pane's agent (as its menu has them).
    fn palette_state(&self, cx: &App) -> PaletteState {
        let app = self.app.read(cx);
        let pane = app.focused();
        let view = app.pane_view(pane).map(|v| v.read(cx));
        let agent = app.layout().agent(pane);
        let bulk = self.bulk_targets(cx);
        PaletteState {
            panes: app.layout().panes().len(),
            tabs: app.tabs().len(),
            has_agent: agent.is_some(),
            running: agent.is_some_and(|id| kill_sheet::running(app, id)),
            restartable: view.is_some_and(|v| v.restartable()),
            has_selection: view.is_some_and(|v| v.has_selection()),
            any_running: app.agents.iter().any(sendable),
            selected_running: bulk.running.len(),
            selected_ended: bulk.ended.len(),
            can_show_changes: agent.is_some_and(|id| self.offers_changes(id, cx)),
            can_archive: agent.is_some_and(|id| self.offers_archive(id, cx)),
            has_actions: !app.actions().all().is_empty(),
        }
    }

    /// ⌘J: shows the next agent that needs you in the focused pane (ADR
    /// 0100); a short toast when none does. Not over a sheet.
    pub fn next_needs_you(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let target = {
            let app = self.app.read(cx);
            let current = app.layout().agent(app.focused());
            app.next_needing_you(current)
        };
        match target {
            Some(id) => self.show_agent(id, window, cx),
            None => self.app.update(cx, |a, cx| {
                a.show_notice("No agent needs you".to_owned(), cx)
            }),
        }
    }

    /// ⇧⌘F: opens Search All Agents (or closes it if open). Not over a
    /// sheet.
    fn search_all_agents(
        &mut self,
        _: &SearchAllAgents,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search_all().is_some() {
            self.close_modal(window, cx);
            return;
        }
        if self.has_modal() {
            return;
        }
        let app = self.app.clone();
        let view = cx.new(|cx| SearchAllView::new(app, cx));
        let sub = cx.subscribe_in(&view, window, Self::search_all_event);
        view.read(cx).focus(window, cx);
        self.modal = Some(Modal::SearchAll(view, sub));
        cx.notify();
    }

    /// A Search All Agents pick shows the agent in the focused pane and
    /// finds the line there.
    fn search_all_event(
        &mut self,
        _view: &Entity<SearchAllView>,
        event: &SearchAllEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search_all().is_none() {
            return;
        }
        self.close_modal(window, cx);
        let SearchAllEvent::Show {
            agent_id,
            line,
            query,
            case_sensitive,
        } = event
        else {
            return;
        };
        self.show_agent(*agent_id, window, cx);
        let app = self.app.read(cx);
        if let Some(view) = app.pane_view(app.focused()).cloned() {
            view.update(cx, |v, cx| {
                v.find_hit(query, *case_sensitive, *line, window, cx)
            });
        }
    }

    fn command_palette(&mut self, _: &CommandPalette, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_palette(window, cx);
    }

    /// A palette pick: an action is performed, an agent shown in the
    /// focused pane, a folder gets the default command.
    fn palette_event(
        &mut self,
        _palette: &Entity<PickerView>,
        event: &PickerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::Palette(_, entries, _)) = &mut self.modal else {
            return;
        };
        let entries = std::mem::take(entries);
        self.close_modal(window, cx);
        let PickerEvent::Pick(key) = event else {
            return;
        };
        let Some(entry) = entries.into_iter().find(|e| e.key() == *key) else {
            return;
        };
        self.palette_recents.touch(key);
        match entry {
            Entry::Command(c) => window.dispatch_action(command_action(c), cx),
            Entry::Agent(id) => self.show_agent(id, window, cx),
            Entry::Changes(id) => self.show_changes(id, window, cx),
            Entry::Folder(dir) => self.spawn_default_in(dir, window, cx),
            Entry::Preset(index) => {
                let presets = self.app.update(cx, |a, _| a.presets());
                if let Some(p) = presets.get(index) {
                    self.open_new_agent_with(Prefill::preset(p), window, cx);
                }
            }
        }
    }

    /// ⌃Tab (⌃⇧Tab, `backward`): opens the agent switcher on the agent
    /// used before the current one (on the least recent), or moves its
    /// selection while open. Not over a sheet or the palette.
    pub fn switch_agent(&mut self, backward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let delta = if backward { -1 } else { 1 };
        if let Some(Modal::Switcher(s)) = &self.modal {
            s.view.update(cx, |v, cx| v.move_selection(delta, cx));
            return;
        }
        if self.has_modal() {
            return;
        }
        let app = self.app.read(cx);
        let current = app.layout().agent(app.focused());
        self.agent_mru.observe(current);
        let items: Vec<_> = switcher_items(&app.agents, &app.home)
            .into_iter()
            .map(|(_, item)| item)
            .collect();
        if items.is_empty() {
            return;
        }
        let recents = self.agent_mru.recents();
        let view = cx.new(|cx| PickerView::list(items, recents, cx));
        view.update(cx, |v, cx| {
            let p = v.picker();
            let current_first = current.is_some_and(|id| {
                p.selected_item()
                    .is_some_and(|i| i.key == Entry::Agent(id).key())
            });
            let row = start_row(p.ranked().len(), current_first, backward);
            v.select(row, cx);
        });
        let sub = cx.subscribe_in(&view, window, Self::switcher_event);
        view.read(cx).focus(window, cx);
        self.modal = Some(Modal::Switcher(Switcher {
            view,
            held: window.modifiers().control,
            _sub: sub,
        }));
        cx.notify();
    }

    /// ⌃ released after a ⌃Tab: the selected agent is shown.
    fn switcher_modifiers(
        &mut self,
        ev: &ModifiersChangedEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::Switcher(s)) = &mut self.modal else {
            return;
        };
        if ev.modifiers.control {
            s.held = true;
        } else if s.held {
            let view = s.view.clone();
            view.update(cx, |v, cx| v.confirm(cx));
        }
    }

    /// A switcher pick shows the agent in the focused pane.
    fn switcher_event(
        &mut self,
        _view: &Entity<PickerView>,
        event: &PickerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.switcher().is_none() {
            return;
        }
        self.close_modal(window, cx);
        let PickerEvent::Pick(key) = event else {
            return;
        };
        let id = key
            .strip_prefix("agent:")
            .and_then(|id| id.parse::<u64>().ok());
        if let Some(id) = id {
            self.show_agent(id, window, cx);
        }
    }

    /// A new agent running `[gui] default_command` in `dir`, in the
    /// focused pane.
    fn spawn_default_in(&mut self, dir: String, window: &mut Window, cx: &mut Context<Self>) {
        let command = self.app.read(cx).config.default_command.clone();
        let result = match command.split_first() {
            Some((program, args)) => {
                let req = SpawnRequest {
                    program: program.clone(),
                    args: args.to_vec(),
                    cwd: dir,
                    labels: Default::default(),
                };
                self.app.update(cx, |a, cx| a.spawn(req, cx))
            }
            None => Err("set [gui] default_command to start an agent from a folder".to_owned()),
        };
        if let Err(e) = result {
            self.app.update(cx, |a, cx| a.show_error(e, cx));
        }
        self.focus_current(window, cx);
    }

    /// A preset (split chooser) started in `dir` in the focused pane, named
    /// as the sheet would name it.
    fn spawn_preset_in(&mut self, index: usize, dir: String, cx: &mut Context<Self>) {
        let presets = self.app.update(cx, |a, _| a.presets());
        let Some(preset) = presets.get(index) else {
            return;
        };
        let names = self.app.read(cx).live_names();
        let taken: Vec<&str> = names.iter().map(String::as_str).collect();
        let result = match preset_request(preset, dir, &taken) {
            Some(req) => self.app.update(cx, |a, cx| a.spawn(req, cx)),
            None => Err(format!("preset {:?} has no command", preset.name)),
        };
        if let Err(e) = result {
            self.app.update(cx, |a, cx| a.show_error(e, cx));
        }
    }

    /// Duplicate Agent (row and pane menus, palette): the New agent sheet
    /// with `agent_id`'s command, folder and labels, and a new name. Works
    /// while it runs.
    pub fn duplicate_agent(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let Some(prefill) = self
            .app
            .read(cx)
            .agents
            .iter()
            .find(|r| r.info.id == agent_id)
            .map(|r| Prefill::duplicate(&r.info))
        else {
            return;
        };
        self.open_new_agent_with(prefill, window, cx);
    }

    /// Duplicate Agent… (palette): the focused pane's agent.
    fn duplicate_pane(&mut self, _: &DuplicatePane, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.read(cx);
        if let Some(agent_id) = app.layout().agent(app.focused()) {
            self.duplicate_agent(agent_id, window, cx);
        }
    }

    /// Asks before killing (only the question's Kill kills), unless
    /// `[gui] confirm_kill` is off.
    fn kill(&mut self, action: &Kill, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let agent_id = action.agent_id;
        let app = self.app.read(cx);
        match kill_flow(app.config.confirm_kill) {
            KillFlow::Kill => app.kill(agent_id),
            KillFlow::Ask => {
                let question = kill_sheet::question(app, agent_id);
                let sheet = cx.new(|cx| KillSheet::new(agent_id, question, cx));
                let sub = cx.subscribe_in(&sheet, window, Self::kill_event);
                sheet.read(cx).focus(window);
                self.modal = Some(Modal::Kill(sheet, sub));
            }
        }
        cx.notify();
    }

    fn kill_event(
        &mut self,
        sheet: &Entity<KillSheet>,
        event: &KillEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let KillEvent::Kill { dont_ask } = *event {
            let agent_ids = sheet.read(cx).agent_ids.clone();
            self.app.update(cx, |a, _| {
                if dont_ask {
                    a.config.confirm_kill = false;
                    if let Err(e) = save_confirm_kill(a.state_dir(), false) {
                        eprintln!("fleet-gui: cannot save confirm_kill: {e}");
                    }
                }
                for id in agent_ids {
                    a.kill(id);
                }
            });
        }
        self.close_modal(window, cx);
    }

    /// The sidebar's selection (of several rows) split by what Kill,
    /// Restart and Remove fit (ADR 0099).
    fn bulk_targets(&self, cx: &App) -> BulkTargets {
        if self.row_selection.ids().len() < 2 {
            return BulkTargets::default();
        }
        BulkTargets::of(self.row_selection.ids(), &self.app.read(cx).agents)
    }

    /// Kill / Restart / Remove for the selected agents each fits. Kill
    /// asks once, listing the names (unless `[gui] confirm_kill` is off);
    /// the others act at once, as their single-agent forms do.
    fn bulk(&mut self, action: BulkAction, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let targets = self.bulk_targets(cx);
        let ids = action.targets(&targets).to_vec();
        if ids.is_empty() {
            return;
        }
        match action {
            BulkAction::Kill => {
                let app = self.app.read(cx);
                match kill_flow(app.config.confirm_kill) {
                    KillFlow::Kill => ids.iter().for_each(|&id| app.kill(id)),
                    KillFlow::Ask => {
                        let names = ids
                            .iter()
                            .filter_map(|id| app.agents.iter().find(|r| r.info.id == *id))
                            .map(target_name)
                            .collect();
                        let sheet = cx.new(|cx| KillSheet::many(ids, names, cx));
                        let sub = cx.subscribe_in(&sheet, window, Self::kill_event);
                        sheet.read(cx).focus(window);
                        self.modal = Some(Modal::Kill(sheet, sub));
                    }
                }
            }
            BulkAction::Restart => {
                let app = self.app.read(cx);
                ids.iter().for_each(|&id| app.restart(id));
            }
            BulkAction::Remove => self.app.read(cx).forget_many(ids),
        }
        cx.notify();
    }

    fn kill_selected(&mut self, _: &KillSelected, window: &mut Window, cx: &mut Context<Self>) {
        self.bulk(BulkAction::Kill, window, cx);
    }

    fn restart_selected(
        &mut self,
        _: &RestartSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bulk(BulkAction::Restart, window, cx);
    }

    fn remove_selected(&mut self, _: &RemoveSelected, window: &mut Window, cx: &mut Context<Self>) {
        self.bulk(BulkAction::Remove, window, cx);
    }

    /// ⌘⇧W, a pane header's Stop or its menu's Kill Agent: kills `pane`'s
    /// agent (asking first) if it is running.
    fn kill_in_pane(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.read(cx);
        if let Some(agent_id) = app.layout().agent(pane)
            && kill_sheet::running(app, agent_id)
        {
            self.kill(&Kill { agent_id }, window, cx);
        }
    }

    fn kill_pane(&mut self, _: &KillPane, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.app.read(cx).focused();
        self.kill_in_pane(pane, window, cx);
    }

    /// Rename Agent… (palette): the focused pane's agent.
    fn rename_pane(&mut self, _: &RenamePane, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.read(cx);
        if let Some(agent_id) = app.layout().agent(app.focused()) {
            self.open_rename(agent_id, window, cx);
        }
    }

    /// Edit Labels… (palette): the focused pane's agent.
    fn edit_labels_pane(
        &mut self,
        _: &EditLabelsPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app = self.app.read(cx);
        if let Some(agent_id) = app.layout().agent(app.focused()) {
            self.open_labels(agent_id, window, cx);
        }
    }

    /// Remove Exited Agent in Pane (palette): forgets the focused pane's
    /// agent if it ended, as the pane's summary bar does.
    fn remove_pane(&mut self, _: &RemovePane, _: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let app = self.app.read(cx);
        app.remove_in_pane(app.focused(), cx);
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

    /// Remove Exited Agents (palette, app menu): asks first, naming how
    /// many will go; does nothing while none has ended. fleetd removes what
    /// has ended when the request lands and never a running agent. "Save
    /// Output First…" saves each one's output into a folder, then removes
    /// those saved (ADR 0086).
    fn remove_exited(&mut self, _: &RemoveExited, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let count = self.app.read(cx).ended_count();
        let Some(question) = remove_exited_prompt(count) else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &question,
            Some("Their history and scrollback are deleted. Running agents are not touched."),
            &[
                PromptButton::new("Remove"),
                PromptButton::new("Save Output First…"),
                PromptButton::cancel("Cancel"),
            ],
            cx,
        );
        let app = self.app.clone();
        cx.spawn(async move |_, cx| match answer.await {
            Ok(0) => {
                let _ = app.update(cx, |a, _| a.clear_exited());
            }
            Ok(1) => {
                let _ = cx.update(|cx| {
                    let ids = app.read(cx).ended_ids();
                    output::save_ended_then_remove(app, ids, cx);
                });
            }
            _ => {}
        })
        .detach();
        cx.notify();
    }

    /// The agent the focused pane shows, unless a sheet is up.
    fn focused_agent(&self, cx: &App) -> Option<(PaneId, u64)> {
        if self.has_modal() {
            return None;
        }
        let app = self.app.read(cx);
        let pane = app.focused();
        Some((pane, app.layout().agent(pane)?))
    }

    /// Copy Last Output, Save Output… or Open Output in Editor for the
    /// agent `pane` shows (ADR 0086).
    fn output_in_pane(&mut self, pane: PaneId, item: PaneMenuItem, cx: &mut Context<Self>) {
        let Some(agent_id) = self.app.read(cx).layout().agent(pane) else {
            return;
        };
        match item {
            PaneMenuItem::CopyLastOutput => output::copy_last_output(&self.app, pane, cx),
            PaneMenuItem::SaveOutput => output::save_output(self.app.clone(), agent_id, cx),
            PaneMenuItem::OpenOutput => output::open_output(self.app.clone(), agent_id, cx),
            _ => {}
        }
    }

    /// ⇧⌘S and the palette: the focused pane's agent's output.
    fn output_focused(&mut self, item: PaneMenuItem, cx: &mut Context<Self>) {
        if let Some((pane, _)) = self.focused_agent(cx) {
            self.output_in_pane(pane, item, cx);
        }
    }

    /// The sidebar's Clear exited: forgets every ended agent.
    pub fn clear_exited(&mut self, cx: &mut Context<Self>) {
        if self.app.read(cx).ended_count() > 0 {
            self.app.read(cx).clear_exited();
        }
    }

    /// ⌘R: restarts the focused pane's agent in place if it ended.
    /// ⌘+ / ⌘− / ⌘0: the terminals' font, or the graph's scale while it
    /// is shown.
    fn zoom(&mut self, zoom: Zoom, window: &mut Window, cx: &mut Context<Self>) {
        if self.graph_shown {
            self.graph.0.update(cx, |g, cx| g.zoom(zoom, window, cx));
            return;
        }
        self.app.update(cx, |a, cx| a.zoom(zoom, cx));
    }

    fn restart_pane(&mut self, _: &RestartPane, _: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
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
            app.tabs()
                .panes()
                .into_iter()
                .filter_map(|p| Some((p, app.pane_view(p)?.clone())))
                .collect()
        };
        // A pane id can get a new view (closing the last pane, or its
        // agent removed): subscriptions follow the view, not the id, and
        // the keyboard follows a focused view's replacement.
        let diffs: Vec<(PaneId, Entity<DiffPane>)> = {
            let app = self.app.read(cx);
            app.tabs()
                .panes()
                .into_iter()
                .filter_map(|p| Some((p, app.diff_pane(p)?.clone())))
                .collect()
        };
        let mut refocus = false;
        self.pane_focus.retain(|p, (id, handle, _)| {
            let kept = views
                .iter()
                .any(|(pane, view)| pane == p && view.entity_id() == *id)
                || diffs
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
            let focus = cx.on_focus_in(&handle, window, move |this, window, cx| {
                this.app.update(cx, |a, cx| a.set_focused(pane, cx));
                // A click into the pane leaves the keys with its chooser.
                if let Some((chooser, _, _)) = this.choosers.get(&pane)
                    && !chooser.read(cx).contains_focus(window, cx)
                {
                    chooser.read(cx).focus(window, cx);
                }
            });
            let events = cx.subscribe_in(&view, window, move |this, _, event, window, cx| {
                this.pane_event(pane, *event, window, cx);
            });
            self.pane_focus
                .insert(pane, (view.entity_id(), handle, [focus, events]));
        }
        for (pane, view) in diffs {
            if self.pane_focus.contains_key(&pane) {
                continue;
            }
            let handle = view.read(cx).focus_handle().clone();
            let focus = cx.on_focus_in(&handle, window, move |this, _, cx| {
                this.app.update(cx, |a, cx| a.set_focused(pane, cx));
            });
            let events = cx.subscribe_in(&view, window, move |this, _, event, window, cx| {
                let DiffPaneEvent::Close = *event;
                this.app.update(cx, |a, cx| {
                    a.set_focused(pane, cx);
                    a.close_focused(cx);
                });
                this.focus_current(window, cx);
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
        if self.has_modal() {
            return;
        }
        self.focus_pane(pane, window, cx);
        match event {
            PaneEvent::NewAgent => self.open_new_agent(window, cx),
            PaneEvent::NewTerminal => self.open_new_terminal(window, cx),
            PaneEvent::InstallSkill => super::resident::install_agent_skill(cx),
            PaneEvent::ShowShortcuts => self.toggle_shortcuts(window, cx),
            PaneEvent::SplitRight => self.split(Axis::Horizontal, window, cx),
            PaneEvent::SplitDown => self.split(Axis::Vertical, window, cx),
            PaneEvent::ToggleMaximize => self.toggle_maximize(&ToggleMaximize, window, cx),
            PaneEvent::Close => self.close_pane(&ClosePane, window, cx),
            PaneEvent::Restart => self.restart_pane(&RestartPane, window, cx),
            PaneEvent::Rename => {
                if let Some(agent_id) = self.app.read(cx).layout().agent(pane) {
                    self.open_rename(agent_id, window, cx);
                }
            }
            PaneEvent::Answer => {
                if let Some(agent_id) = self.app.read(cx).layout().agent(pane) {
                    self.open_answer(agent_id, window, cx);
                }
            }
            PaneEvent::Choose(i) => {
                if let Some(agent_id) = self.app.read(cx).layout().agent(pane) {
                    let q = self
                        .app
                        .read(cx)
                        .agents
                        .iter()
                        .find(|r| r.info.id == agent_id)
                        .and_then(|r| open_question(&r.info));
                    if let Some(q) = q
                        && let Some(choice) = q.choices.get(i)
                    {
                        self.answer_with(agent_id, q.id, choice.clone(), cx);
                    }
                }
            }
            PaneEvent::EditLabels => {
                if let Some(agent_id) = self.app.read(cx).layout().agent(pane) {
                    self.open_labels(agent_id, window, cx);
                }
            }
            PaneEvent::Duplicate => {
                if let Some(agent_id) = self.app.read(cx).layout().agent(pane) {
                    self.duplicate_agent(agent_id, window, cx);
                }
            }
            PaneEvent::Kill => self.kill_in_pane(pane, window, cx),
            PaneEvent::Remove => self.app.read(cx).remove_in_pane(pane, cx),
            PaneEvent::ContextMenu { on_grid } => self.open_pane_menu(pane, on_grid, cx),
            PaneEvent::ShowChanges => {
                if let Some(agent) = self.app.read(cx).layout().agent(pane) {
                    self.show_changes(agent, window, cx);
                }
            }
            PaneEvent::Notice(text) => self
                .app
                .update(cx, |a, cx| a.show_notice(text.to_owned(), cx)),
        }
    }

    fn toggle_maximize(&mut self, _: &ToggleMaximize, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        self.app.update(cx, |a, cx| a.toggle_maximize(cx));
        self.focus_current(window, cx);
    }

    /// The pane cards, [`chrome::PANE_GAP`] apart and from the edges;
    /// only the maximized one when a pane is maximized (the others stay
    /// attached); then the split dividers' strips over the gaps.
    fn render_panes(&mut self, window: &mut Window, cx: &App) -> impl IntoElement + use<> {
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
            None => app.layout().rects(1.0, 1.0),
        };
        let drawn: Vec<PaneId> = rects.iter().map(|(p, _)| *p).collect();
        // New panes (opened, split, or shown again on restore) ease in.
        let now = Instant::now();
        if !self.reduce_motion {
            for pane in feedback::entering(&self.drawn, &drawn) {
                self.entering.insert(pane, now);
            }
        }
        self.drawn = drawn;
        self.entering.retain(|pane, at| {
            !self.reduce_motion
                && self.drawn.contains(pane)
                && Tween::new(*at, feedback::PANE_IN_MS).running(now)
        });
        if !self.entering.is_empty() {
            window.request_animation_frame();
        }
        let entering = &self.entering;
        let dividers = match app.maximized {
            Some(_) => Vec::new(),
            None => self.split_drag.render(app.layout(), &self.app),
        };
        let dragging = self.pane_drag.render(
            rects.clone(),
            app.layout().panes().len() < MAX_PANES,
            &self.app,
            &app.theme,
        );
        let children: Vec<_> = rects
            .into_iter()
            .filter_map(|(pane, r)| {
                let view: gpui::AnyView = match app.diff_pane(pane) {
                    Some(diff) => diff.clone().into(),
                    None => app.pane_view(pane)?.clone().into(),
                };
                // 1 when settled; grows from an inset while fading in.
                let t = entering
                    .get(&pane)
                    .map_or(1.0, |at| Tween::new(*at, feedback::PANE_IN_MS).value(now));
                let drag = self.pane_drag.clone();
                Some(
                    div()
                        .debug_selector(move || format!("pane-card-{}", pane.0))
                        .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, _, _| {
                            drag.press_card(pane, ev.position);
                        })
                        .absolute()
                        .left(relative(r.x))
                        .top(relative(r.y))
                        .w(relative(r.w))
                        .h(relative(r.h))
                        .p(px(
                            chrome::PANE_GAP / 2.0 + feedback::PANE_IN_INSET * (1.0 - t)
                        ))
                        .opacity(t)
                        .child(view),
                )
            })
            .collect();
        div().relative().flex_1().h_full().min_w_0().child(
            div()
                .absolute()
                .inset(px(chrome::PANE_GAP / 2.0))
                .children(children)
                .children(dividers)
                .children(dragging),
        )
    }

    /// The row menu for `agent_id`: Open in New Split (while a pane can
    /// be added), then Kill for a running agent, or Restart and Remove for
    /// one that exited or is lost.
    fn menu_items(&self, agent_id: u64, cx: &App) -> Vec<MenuEntry<RowMenuItem>> {
        let app = self.app.read(cx);
        let is_ended = app.agents.iter().any(|r| r.info.id == agent_id && ended(r));
        let in_selection = self.row_selection.targets_from(agent_id).len() > 1;
        let mut items = row_menu(
            app.layout().panes().len() < MAX_PANES,
            is_ended,
            in_selection,
        );
        if in_selection {
            items.extend(bulk_entries());
        }
        items.extend(self.workflow_row_items(agent_id, cx));
        items
    }

    /// The exit toasts (#44), bottom centre, newest at the bottom: what
    /// exited with its code, Restart and ✕. A success being celebrated
    /// gets a check and, with motion, a small burst.
    fn render_exit_toasts(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<Div> {
        let app = self.app.read(cx);
        let notes: Vec<_> = app.exit_toasts().cloned().collect();
        if notes.is_empty() {
            return None;
        }
        let celebrating = app.celebrating;
        let reduce_motion = self.reduce_motion;
        let theme = *theme;
        let mut column = div().flex().flex_col().items_center().gap(px(space::SM));
        for note in notes {
            let id = note.agent_id;
            let party = celebrating
                .is_some_and(|(a, at)| a == id && at.elapsed() < feedback::EXIT_TOAST_TTL);
            let restart = button(
                ("exit-restart", id),
                "Restart",
                &theme,
                ButtonKind::Secondary,
                true,
            )
            .debug_selector(move || format!("exit-toast-{id}-restart"))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.app.update(cx, |a, cx| a.restart_from_toast(id, cx));
            }));
            let close = div()
                .id(("exit-close", id))
                .debug_selector(move || format!("exit-toast-{id}-close"))
                .px(px(space::XS))
                .text_color(theme.secondary)
                .cursor_pointer()
                .hover(move |s| s.text_color(theme.text))
                .child("✕")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.app.update(cx, |a, cx| a.dismiss_exit(id, cx));
                }));
            let card = div()
                .id(("exit-toast", id))
                .debug_selector(move || format!("exit-toast-{id}"))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SM))
                .pl(px(space::MD))
                .pr(px(space::XS))
                .py(px(space::XS))
                .rounded(px(radius::MD))
                .border_1()
                .border_color(theme.border)
                .bg(theme.raised)
                .shadow_lg()
                .text_color(theme.text)
                .type_style(text::CALLOUT)
                .occlude()
                .children(party.then(|| celebration(id, &theme, reduce_motion)))
                .child(SharedString::from(note.text()))
                .child(restart)
                .child(close);
            column = column.child(appear(
                div().child(card),
                ("exit-toast-appear", id),
                motion::SHEET_OFFSET,
                motion::TOAST_MS,
                reduce_motion,
            ));
        }
        Some(
            div()
                .absolute()
                .bottom(px(space::LG))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(column),
        )
    }
}

/// A check in the success colour; with motion, six dots burst out of it
/// and fade once ([`feedback::CELEBRATE_MS`]), then nothing moves.
fn celebration(agent_id: u64, theme: &Theme, reduce_motion: bool) -> gpui::AnyElement {
    const SIZE: f32 = 16.0;
    const DOTS: usize = 6;
    const DOT: f32 = 4.0;
    let check = div()
        .debug_selector(move || format!("exit-toast-{agent_id}-celebration"))
        .relative()
        .size(px(SIZE))
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme.success)
        .font_weight(gpui::FontWeight::BOLD)
        .child("✓");
    if reduce_motion {
        return check.into_any_element();
    }
    let colours = [
        theme.success,
        theme.accent,
        theme.success,
        theme.accent,
        theme.success,
        theme.accent,
    ];
    let mut burst = check;
    for (i, colour) in colours.into_iter().enumerate().take(DOTS) {
        let angle = i as f32 * std::f32::consts::TAU / DOTS as f32;
        let dot = div()
            .absolute()
            .size(px(DOT))
            .rounded_full()
            .bg(colour)
            .with_animation(
                ("celebrate", agent_id * DOTS as u64 + i as u64),
                Animation::new(Duration::from_millis(feedback::CELEBRATE_MS))
                    .with_easing(motion::ease_out),
                move |dot, t| {
                    let r = 2.0 + 12.0 * t;
                    let c = SIZE / 2.0 - DOT / 2.0;
                    dot.left(px(c + r * angle.cos()))
                        .top(px(c + r * angle.sin()))
                        .opacity(1.0 - t)
                },
            );
        burst = burst.child(dot);
    }
    burst.into_any_element()
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::bench::frame_started();
        self.follow_pane_focus(window, cx);
        self.follow_choosers(window, cx);
        {
            let app = self.app.read(cx);
            self.agent_mru.observe(app.layout().agent(app.focused()));
        }
        let theme = self.app.read(cx).theme;
        let agent_emoji = self.app.read(cx).config.agent_emoji;
        let (rows, error, notice, titles, marks, update, statuses, bells) = {
            let app = self.app.read(cx);
            (
                app.agents.clone(),
                app.error.clone(),
                app.notice.clone(),
                app.titles.clone(),
                app.tab_marks(),
                app.update_button(),
                app.statuses.clone(),
                app.bells.clone(),
            )
        };
        let (pending, fresh, next) = {
            let app = self.app.read(cx);
            let now = Instant::now();
            (
                app.pending_spawns.clone(),
                app.new_children.strengths(now, self.reduce_motion),
                app.new_children.next_change(now, self.reduce_motion),
            )
        };
        self.new_child_timer.schedule(next, window, cx);
        let mark = move |id: u64| marks.get(&id).copied().unwrap_or(TabMark::None);
        // Rows that went away leave the selection.
        self.row_selection
            .retain(|id| rows.iter().any(|r| r.info.id == id));
        let groups = self.sidebar_groups(cx);
        #[cfg(test)]
        {
            self.drawn_sidebar = groups
                .iter()
                .map(|g| {
                    (
                        g.title.clone(),
                        g.agents.iter().map(|r| r.info.id).collect(),
                    )
                })
                .collect();
        }
        let data = SidebarData {
            rows: &rows,
            now_ms: now_ms(),
            titles: &titles,
            mark: &mark,
            statuses: &statuses,
            bells: &bells,
            reduce_motion: self.reduce_motion,
            agent_emoji,
            groups,
            filter_field: self.filter_field.clone(),
            status_filter: self.status_filter,
            focus: self.sidebar_focus.clone(),
            selected: self.row_selection.ids(),
            pending: &pending,
            fresh: &fresh,
        };
        let sidebar = render_sidebar(data, &theme, cx);
        // The sidebar is hidden under the graph: its spawn requests float
        // over the graph instead (ADR 0079).
        let graph_pending = if self.graph_shown {
            render_pending(&pending, &rows, &titles, &theme, cx).map(|block| {
                div()
                    .id("graph-pending-spawns")
                    .absolute()
                    .top(px(chrome::BAR_HEIGHT + space::SM))
                    .right(px(space::LG))
                    .w(px(chrome::SIDEBAR_WIDTH))
                    .max_h(px(480.))
                    .overflow_y_scroll()
                    .pb(px(space::SM))
                    .rounded(px(radius::LG))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.raised)
                    .shadow_lg()
                    .occlude()
                    .child(block)
            })
        } else {
            None
        };
        let ended_rows = rows.iter().filter(|r| ended(r)).count();
        // What Clear exited removes (an ended parent of a running agent stays).
        let removable_rows = removable_ended(&rows);
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
                ended: removable_rows.len(),
                fullscreen: window.is_fullscreen(),
                update,
                tabs: self.tab_labels(cx),
                active_tab: app.tabs().active(),
                graph: self.graph_shown,
            }
        };
        let bar = render_bar(&bar, &theme, cx);
        super::bench::sidebar_drawn(window);
        let panes = self.render_panes(window, cx);
        let exits = self.render_exit_toasts(&theme, cx);
        // Under glass gpui leaves the tint to the glass (ADR 0049).
        let tint = self.backdrop.tint();
        // Without glass the root's one fill covers all and nothing is
        // wrapped (the default frame is unchanged).
        let (bar, sidebar, panes) = if tint.everywhere() {
            (
                bar.into_any_element(),
                sidebar.into_any_element(),
                panes.into_any_element(),
            )
        } else {
            let fill = |el: gpui::Div, on: bool| if on { el.bg(theme.window) } else { el };
            (
                fill(div().flex_none().child(bar), tint.chrome).into_any_element(),
                fill(div().flex_none().h_full().child(sidebar), tint.chrome).into_any_element(),
                fill(
                    div().flex_1().min_w_0().h_full().flex().child(panes),
                    tint.content,
                )
                .into_any_element(),
            )
        };
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
            .connect_sheet()
            .filter(|_| self.connect_sheet_shown(cx))
            .map(|sheet| {
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
        let shortcuts = match &self.modal {
            Some(Modal::Shortcuts(sections, handle)) => Some(render_shortcuts(
                sections,
                handle,
                &theme,
                reduce_motion,
                cx,
            )),
            _ => None,
        };
        let palette = self.palette().map(|palette| {
            // A click beside it closes it, never reaching the panes.
            deferred(
                div()
                    .id("palette-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_start()
                    .justify_center()
                    .pt(px(chrome::BAR_HEIGHT + space::XL))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            if this.palette().is_some() {
                                this.toggle_palette(window, cx);
                            }
                        }),
                    )
                    .child(appear(
                        div().child(palette.clone()),
                        "palette-appear",
                        -motion::SHEET_OFFSET,
                        motion::SHEET_MS,
                        reduce_motion,
                    )),
            )
            .with_priority(2)
        });
        let search_all = self.search_all().map(|view| {
            // Placed as the palette is; a click beside it closes it.
            deferred(
                div()
                    .id("search-all-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_start()
                    .justify_center()
                    .pt(px(chrome::BAR_HEIGHT + space::XL))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            if this.search_all().is_some() {
                                this.close_modal(window, cx);
                            }
                        }),
                    )
                    .child(appear(
                        div().child(view.clone()),
                        "search-all-appear",
                        -motion::SHEET_OFFSET,
                        motion::SHEET_MS,
                        reduce_motion,
                    )),
            )
            .with_priority(2)
        });
        let switcher = self.switcher().map(|view| {
            // Centred, like ⌘Tab; a click beside it cancels.
            deferred(
                div()
                    .id("switcher-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            if this.switcher().is_some() {
                                this.close_modal(window, cx);
                            }
                        }),
                    )
                    .child(appear(
                        div().child(view.clone()),
                        "switcher-appear",
                        -motion::SHEET_OFFSET,
                        motion::SHEET_MS,
                        reduce_motion,
                    )),
            )
            .with_priority(2)
        });
        let rename = self.rename_sheet().map(|sheet| {
            deferred(
                div()
                    .id("rename-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_start()
                    .justify_center()
                    .pt(px(chrome::BAR_HEIGHT + space::SM))
                    .child(sheet_drag_strip("rename-drag-strip"))
                    .child(appear(
                        div().child(sheet.clone()),
                        "rename-appear",
                        -motion::SHEET_OFFSET,
                        motion::SHEET_MS,
                        reduce_motion,
                    )),
            )
            .with_priority(2)
        });
        let answer = self.answer_sheet().map(|sheet| {
            deferred(
                div()
                    .id("answer-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_start()
                    .justify_center()
                    .pt(px(chrome::BAR_HEIGHT + space::SM))
                    .child(sheet_drag_strip("answer-drag-strip"))
                    .child(appear(
                        div().child(sheet.clone()),
                        "answer-appear",
                        -motion::SHEET_OFFSET,
                        motion::SHEET_MS,
                        reduce_motion,
                    )),
            )
            .with_priority(2)
        });
        let labels = self.labels_sheet().map(|sheet| {
            deferred(
                div()
                    .id("labels-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_start()
                    .justify_center()
                    .pt(px(chrome::BAR_HEIGHT + space::SM))
                    .child(sheet_drag_strip("labels-drag-strip"))
                    .child(appear(
                        div().child(sheet.clone()),
                        "labels-appear",
                        -motion::SHEET_OFFSET,
                        motion::SHEET_MS,
                        reduce_motion,
                    )),
            )
            .with_priority(2)
        });
        let sheet = self.sheet().map(|sheet| {
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
            .when(tint.everywhere(), |s| s.bg(theme.window))
            .text_color(theme.text)
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                // Esc ends a drag before it reaches the focused pane.
                if ev.keystroke.key == "escape" && this.pane_drag.cancel() {
                    cx.stop_propagation();
                    window.refresh();
                }
            }))
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
            .on_action(cx.listener(Self::focus_filter))
            .on_action(cx.listener(Self::kill))
            .on_action(cx.listener(Self::kill_pane))
            .on_action(cx.listener(Self::rename_pane))
            .on_action(cx.listener(Self::edit_labels_pane))
            .on_action(cx.listener(Self::duplicate_pane))
            .on_action(cx.listener(Self::remove_pane))
            .on_action(cx.listener(Self::restart))
            .on_action(cx.listener(Self::remove))
            .on_action(cx.listener(Self::remove_exited))
            .on_action(cx.listener(Self::kill_selected))
            .on_action(cx.listener(Self::restart_selected))
            .on_action(cx.listener(Self::remove_selected))
            .on_action(cx.listener(|this, _: &SaveOutput, _, cx| {
                this.output_focused(PaneMenuItem::SaveOutput, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenOutput, _, cx| {
                this.output_focused(PaneMenuItem::OpenOutput, cx)
            }))
            .on_action(cx.listener(|this, _: &CopyLastOutput, _, cx| {
                this.output_focused(PaneMenuItem::CopyLastOutput, cx)
            }))
            .on_action(cx.listener(Self::send_to_agents))
            .on_action(cx.listener(Self::search_all_agents))
            .on_action(cx.listener(|this, _: &NextNeedsYou, w, cx| this.next_needs_you(w, cx)))
            .on_action(cx.listener(Self::restart_pane))
            .on_action(cx.listener(|this, _: &ZoomIn, w, cx| this.zoom(Zoom::In, w, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, w, cx| this.zoom(Zoom::Out, w, cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, w, cx| this.zoom(Zoom::Reset, w, cx)))
            .on_action(cx.listener(|this, _: &ToggleGraph, w, cx| this.toggle_graph(w, cx)))
            .on_action(cx.listener(Self::reopen_closed_pane))
            .on_action(cx.listener(|this, _: &ShowShortcuts, w, cx| this.toggle_shortcuts(w, cx)))
            .on_action(cx.listener(Self::command_palette))
            .on_action(cx.listener(Self::show_changes_command))
            .on_action(cx.listener(Self::run_action_command))
            .on_action(cx.listener(Self::archive_worktree_command))
            .on_action(cx.listener(|this, _: &NextAgent, w, cx| this.switch_agent(false, w, cx)))
            .on_action(cx.listener(|this, _: &PreviousAgent, w, cx| this.switch_agent(true, w, cx)))
            .on_modifiers_changed(cx.listener(Self::switcher_modifiers))
            .on_action(cx.listener(Self::new_tab))
            .on_action(cx.listener(Self::close_shown_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::prev_tab))
            .on_action(cx.listener(Self::select_numbered_tab))
            .child(bar)
            .child(if self.graph_shown {
                fill_content(
                    div().flex().flex_1().min_h_0().child(self.graph.0.clone()),
                    (tint.content && !tint.everywhere()).then_some(theme.window),
                )
            } else {
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(sidebar)
                    .child(panes)
            })
            .children(exits)
            .children(graph_pending)
            .children(toast)
            .children(palette)
            .children(search_all)
            .children(switcher)
            .children(
                self.peek()
                    .map(|p| peek_view::render_over(p, reduce_motion)),
            )
            .children(sheet)
            .children(rename)
            .children(answer)
            .children(labels)
            .children(
                self.kill_sheet()
                    .map(|sheet| kill_sheet::render_over(sheet, reduce_motion)),
            )
            .children(
                self.send_sheet()
                    .map(|sheet| send_sheet::render_over(sheet, reduce_motion)),
            )
            .children(
                self.action_sheet()
                    .map(|sheet| action_sheet::render_over(sheet, reduce_motion)),
            )
            .children(shortcuts)
            .children(connect)
    }
}

/// The ⌘/ cheat sheet over the window: Escape, ⌘/ or a click outside
/// closes it.
fn render_shortcuts(
    sections: &[Section],
    handle: &FocusHandle,
    theme: &Theme,
    reduce_motion: bool,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let columns = sections.iter().map(|section| {
        div()
            .flex()
            .flex_col()
            .gap(px(space::XS))
            .min_w(px(220.))
            .child(
                div()
                    .type_style(text::SECTION)
                    .text_color(theme.secondary)
                    .pb(px(space::XXS))
                    .child(SharedString::from(section.title.clone())),
            )
            .children(section.rows.iter().map(|row| {
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .gap(px(space::LG))
                    .type_style(text::CALLOUT)
                    .child(SharedString::from(row.title.clone()))
                    .child(
                        div()
                            .text_color(theme.secondary)
                            .child(SharedString::from(row.keys.clone())),
                    )
            }))
    });
    let card = controls::sheet_card("shortcuts-card", 760.0, theme)
        .debug_selector(|| "shortcuts-sheet".to_owned())
        .max_h(relative(0.8))
        .overflow_y_scroll()
        // A click on the card is not one outside it.
        .on_click(|_, _, cx| cx.stop_propagation())
        .child(controls::sheet_title("Keyboard Shortcuts"))
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(space::XL))
                .children(columns),
        );
    deferred(
        div()
            .id("shortcuts-backdrop")
            .track_focus(handle)
            .on_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, window, cx| {
                if e.keystroke.key == "escape" {
                    this.toggle_shortcuts(window, cx);
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| this.toggle_shortcuts(window, cx)))
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(chrome::BAR_HEIGHT + space::SM))
            .child(sheet_drag_strip("shortcuts-drag-strip"))
            .child(appear(
                div().child(card),
                "shortcuts-appear",
                -motion::SHEET_OFFSET,
                motion::SHEET_MS,
                reduce_motion,
            )),
    )
    .with_priority(2)
}

/// The tokens for the window's appearance (or the one `[gui] theme`
/// picks), the tint at `[gui] window_opacity`.
fn window_tokens(window: &Window, config: &GuiConfig) -> Tokens {
    let appearance = config.theme.resolve(appearance_of(window.appearance()));
    Tokens::for_appearance(appearance).with_window_opacity(config.window_opacity)
}

/// The theme for the window's appearance (or the one `[gui] theme`
/// picks), its tint at `[gui] window_opacity`, in the user's accent
/// colour. Reduce Transparency makes it opaque.
fn window_theme(window: &Window, config: &GuiConfig) -> Theme {
    let tokens = window_tokens(window, config);
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

/// The graph view and its event subscription.
fn new_graph(
    app: &Entity<AppState>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> (Entity<GraphView>, Subscription) {
    let graph = cx.new(|cx| GraphView::new(app.clone(), cx));
    let sub = cx.subscribe_in(&graph, window, Workspace::graph_event);
    (graph, sub)
}

/// Pinches over the window zoom the graph while it is shown.
fn pinch_to_graph(
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<native::MagnifyMonitor> {
    let (tx, rx) = async_channel::unbounded::<(f32, (f32, f32))>();
    let monitor = native::monitor_magnify(move |m, at| {
        let _ = tx.try_send((m, at));
    })?;
    cx.spawn_in(window, async move |this, cx| {
        while let Ok((m, (x, y))) = rx.recv().await {
            let alive = this.update_in(cx, |this, window, cx| {
                if this.graph_shown {
                    let graph = this.graph.0.clone();
                    graph.update(cx, |g, cx| {
                        g.magnify(point(px(x), px(y)), 1.0 + m, window, cx)
                    });
                }
            });
            if alive.is_err() {
                break;
            }
        }
    })
    .detach();
    Some(monitor)
}

/// `el` filled with `bg` when the content is tinted on its own (glass).
fn fill_content(el: Div, bg: Option<gpui::Hsla>) -> Div {
    match bg {
        Some(bg) => el.bg(bg),
        None => el,
    }
}

pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use fleet_proto::{AgentInfo, AgentState, AttachOk};
    use fleet_term::Palette;
    use gpui::{
        AppContext, ClipboardItem, Entity, Modifiers, MouseButton, Point, ScrollDelta,
        ScrollWheelEvent, TestAppContext, TouchPhase, VisualTestContext, point,
    };

    use super::{SendSheet, Workspace, now_ms};
    use crate::app::app_state::{AppState, SAVE_DEBOUNCE, SessionApi};
    use crate::app::keymap::{FocusPane1, FocusPane2, Kill, RemoveExited, SplitRight, bind_keys};
    use crate::app::new_agent::NewAgentSheet;
    use crate::app::pane_view::InputSink;
    use crate::app::sidebar_view::TabMark;
    use crate::app::text_field::TextField;
    use crate::app::theme::Theme;
    use crate::core::broadcast::{RETURN_DELAY_MS, SendPlan};
    use crate::core::config::GuiConfig;
    use crate::core::feedback;
    use crate::core::layout::Layout;
    use crate::core::menus::{MenuEntry, PaneMenuItem, RowMenuItem};
    use crate::core::pane::PaneModel;
    use crate::core::presets::Preset;
    use crate::core::session::{AgentRow, ConnState, PaneId, UiEvent};
    use crate::core::settings::{Change, MotionChoice};
    use crate::core::shortcuts::{EmptyPane, key_glyphs};
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
        Rename(u64, String),
        SetLabels(u64, Vec<(String, String)>, Vec<String>),
        /// Typed into an agent (Send to Agents…).
        Send(u64, Vec<u8>),
        /// An answer to an agent's open question: agent, question id, text.
        Answer(u64, u64, String),
        ClearExited,
        /// Approved (`true`) or denied a pending spawn.
        Decide(u64, bool),
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
        /// The same input, with the pane it went to.
        pane_inputs: Mutex<Vec<(PaneId, Vec<u8>)>>,
        agents: Mutex<Vec<AgentRow>>,
        models: Mutex<HashMap<PaneId, Arc<Mutex<PaneModel>>>>,
        spawn_error: Mutex<Option<String>>,
        listed: std::sync::atomic::AtomicBool,
        lists: std::sync::atomic::AtomicU64,
        replaced: Mutex<HashMap<u64, u64>>,
        update_available: std::sync::atomic::AtomicBool,
        daemon_restarts: std::sync::atomic::AtomicUsize,
        restart_pending: std::sync::atomic::AtomicBool,
        /// What `read_output` gives per agent (unset: an error).
        outputs: Mutex<HashMap<u64, Result<String, String>>>,
        /// What `search_output` gives, and the searches asked for.
        search_hits: Mutex<Vec<fleet_proto::SearchHit>>,
        searches: Mutex<Vec<(String, bool)>>,
        pending: Mutex<Vec<fleet_proto::PendingSpawn>>,
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
        fn input(&self, pane: PaneId, bytes: Vec<u8>) {
            self.pane_inputs.lock().unwrap().push((pane, bytes.clone()));
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

        fn search_output(
            &self,
            query: String,
            case_sensitive: bool,
        ) -> crate::app::app_state::SearchJob {
            self.searches.lock().unwrap().push((query, case_sensitive));
            let hits = self.search_hits.lock().unwrap().clone();
            Box::new(move || {
                Ok(fleet_proto::SearchHits {
                    hits,
                    truncated: false,
                })
            })
        }

        fn read_output(&self, agent_id: u64) -> crate::app::app_state::OutputJob {
            let out = self.outputs.lock().unwrap().get(&agent_id).cloned();
            Box::new(move || out.unwrap_or_else(|| Err("no such agent".into())))
        }

        fn rename(&self, agent_id: u64, name: String) {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Rename(agent_id, name));
        }

        fn set_labels(&self, agent_id: u64, set: HashMap<String, String>, remove: Vec<String>) {
            let mut set: Vec<_> = set.into_iter().collect();
            set.sort();
            self.calls
                .lock()
                .unwrap()
                .push(Call::SetLabels(agent_id, set, remove));
        }

        fn answer(&self, agent_id: u64, question: u64, text: String) {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Answer(agent_id, question, text));
        }

        fn clear_exited(&self) {
            self.calls.lock().unwrap().push(Call::ClearExited);
        }

        fn pending_spawns(&self) -> Vec<fleet_proto::PendingSpawn> {
            self.pending.lock().unwrap().clone()
        }

        fn decide_spawn(&self, id: u64, approve: bool) {
            self.calls.lock().unwrap().push(Call::Decide(id, approve));
        }

        fn send_input(&self, agent_id: u64, data: Vec<u8>) {
            self.calls.lock().unwrap().push(Call::Send(agent_id, data));
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
        // No presets unless the test names some: never this Mac's PATH.
        let config = GuiConfig {
            presets: config.presets.or(Some(Vec::new())),
            ..config
        };
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

    #[gpui::test]
    fn files_dropped_on_a_row_are_typed_into_its_agent(cx: &mut TestAppContext) {
        let fake = Arc::new(Fake::default());
        let dir = tempfile::tempdir().unwrap();
        let mut s = setup(cx, &fake, dir.path());
        let rows = vec![
            row(1, "/w", 1),
            row(2, "/w", 2),
            ended(3, "/w", 3, AgentState::Exited),
        ];
        agents(&mut s, &fake, rows);
        s.ws.update_in(s.cx, |w, window, cx| w.show_agent(1, window, cx));
        s.cx.run_until_parked();
        let pane = focused(&mut s);
        fake.model(pane).lock().unwrap().on_output(b"\x1b[?2004h");
        fake.inputs.lock().unwrap().clear();
        let paths = vec!["/a b.png".to_owned(), "/c".to_owned()];

        // Shown in a pane: through it, bracketed as the agent asked.
        s.app
            .update(s.cx, |a, cx| a.drop_paths_on_agent(1, &paths, cx));
        let typed = b"\x1b[200~'/a b.png' /c\x1b[201~".to_vec();
        assert_eq!(*fake.inputs.lock().unwrap(), vec![typed]);

        // Not shown: on the control connection.
        s.app
            .update(s.cx, |a, cx| a.drop_paths_on_agent(2, &paths, cx));
        let sent = Call::Send(2, b"'/a b.png' /c".to_vec());
        assert!(fake.calls().contains(&sent), "{:?}", fake.calls());

        // An ended agent takes nothing.
        let before = fake.calls().len();
        s.app
            .update(s.cx, |a, cx| a.drop_paths_on_agent(3, &paths, cx));
        assert_eq!(fake.calls().len(), before);
        assert_eq!(fake.inputs.lock().unwrap().len(), 1);
    }

    #[gpui::test]
    fn a_pane_notice_shows_in_the_toast(cx: &mut TestAppContext) {
        let fake = Arc::new(Fake::default());
        let dir = tempfile::tempdir().unwrap();
        let mut s = setup(cx, &fake, dir.path());
        let pane = focused(&mut s);
        let view = s
            .app
            .read_with(s.cx, |a, _| a.pane_view(pane).cloned())
            .unwrap();
        view.update(s.cx, |v, cx| {
            v.chrome.has_agent = true;
            v.chrome.local = false;
            v.type_paths(&["/a".to_owned()], cx);
        });
        s.cx.run_until_parked();
        let notice = s.app.read_with(s.cx, |a, _| a.notice().map(str::to_owned));
        assert_eq!(
            notice.as_deref(),
            Some(crate::core::drop_paste::DROP_NEEDS_LOCAL)
        );
    }

    #[gpui::test]
    fn files_dropped_on_a_remote_agent_show_a_notice(cx: &mut TestAppContext) {
        let fake = Arc::new(Fake::default());
        *fake.agents.lock().unwrap() = vec![row(1, "/w", 1)];
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = async_channel::unbounded();
        let session: Arc<dyn SessionApi> = fake.clone();
        let target = Target::Ssh { dest: "box".into() };
        let path = dir.path().to_owned();
        let app = cx.new(|cx| {
            AppState::new(
                session,
                rx,
                target,
                path,
                GuiConfig::default(),
                Theme::dark(),
                cx,
            )
        });
        let paths = vec!["/a".to_owned()];
        app.update(cx, |a, cx| a.drop_paths_on_agent(1, &paths, cx));
        let notice = app.read_with(cx, |a, _| a.notice().map(str::to_owned));
        assert_eq!(
            notice.as_deref(),
            Some(crate::core::drop_paste::DROP_NEEDS_LOCAL)
        );
        assert!(!fake.calls().iter().any(|c| matches!(c, Call::Send(..))));
        assert!(fake.inputs.lock().unwrap().is_empty());
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
            // Within the pane: the new ones' split choosers have it.
            let has_focus = s.cx.update(|window, cx| {
                let view = app.read(cx).pane_view(*pane).unwrap().clone();
                view.read(cx).focus_handle().contains_focused(window, cx)
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

    fn kills(fake: &Fake) -> Vec<u64> {
        fake.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Kill(id) => Some(id),
                _ => None,
            })
            .collect()
    }

    /// The agent the kill question is open for.
    fn kill_asked(s: &mut Setup<'_>) -> Option<u64> {
        s.cx.run_until_parked();
        s.ws.read_with(s.cx, |w, cx| {
            w.kill_sheet()
                .and_then(|k| k.read(cx).agent_ids.first().copied())
        })
    }

    fn hover(s: &mut Setup<'_>, selector: &'static str) {
        let at = center(s, selector);
        s.cx.simulate_mouse_move(at, None, Modifiers::none());
        s.cx.run_until_parked();
    }

    /// A shell from New terminal is asked about as a shell, not an agent
    /// (ADR 0071).
    #[gpui::test]
    fn kill_asks_about_a_shell_as_a_shell(cx: &mut TestAppContext) {
        use crate::core::spawn::{KIND_LABEL, SHELL_KIND};
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let mut shell = row(4, "/w", 1);
        shell
            .info
            .labels
            .insert(KIND_LABEL.to_owned(), SHELL_KIND.to_owned());
        agents(&mut s, &fake, vec![shell]);
        s.cx.dispatch_action(Kill { agent_id: 4 });
        assert_eq!(kill_asked(&mut s), Some(4));
        let question = s.ws.read_with(s.cx, |w, cx| {
            w.kill_sheet().unwrap().read(cx).question.to_string()
        });
        assert_eq!(question, "Kill shell 4 (sh, /w)?");
    }

    #[gpui::test]
    fn kill_asks_in_a_sheet_and_only_kill_kills(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(7, "/w", 1)]);
        s.cx.dispatch_action(Kill { agent_id: 7 });
        assert_eq!(kill_asked(&mut s), Some(7));
        assert!(drawn(&mut s, "kill-sheet"));
        let question = s.ws.read_with(s.cx, |w, cx| {
            w.kill_sheet().unwrap().read(cx).question.to_string()
        });
        assert_eq!(question, "Kill agent 7 (sh, /w)?");
        // The sheet is modal: ⌘W does not close the pane meanwhile.
        s.cx.simulate_keystrokes("cmd-w");
        assert!(!fake.calls().contains(&Call::Close(PaneId(1))));
        click(&mut s, "kill-cancel");
        assert_eq!(kill_asked(&mut s), None);
        assert!(kills(&fake).is_empty());

        s.cx.dispatch_action(Kill { agent_id: 7 });
        click(&mut s, "kill-confirm");
        assert_eq!(kill_asked(&mut s), None);
        assert_eq!(kills(&fake), [7]);
        // Still asks next time; nothing was saved.
        assert!(s.app.read_with(s.cx, |a, _| a.config.confirm_kill));
        assert!(!dir.path().join("config.toml").exists());
    }

    #[gpui::test]
    fn escape_cancels_the_kill_and_return_kills(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(7, "/w", 1)]);
        s.cx.dispatch_action(Kill { agent_id: 7 });
        assert_eq!(kill_asked(&mut s), Some(7));
        s.cx.simulate_keystrokes("escape");
        assert_eq!(kill_asked(&mut s), None);
        assert!(kills(&fake).is_empty());
        s.cx.dispatch_action(Kill { agent_id: 7 });
        s.cx.simulate_keystrokes("enter");
        assert_eq!(kill_asked(&mut s), None);
        assert_eq!(kills(&fake), [7]);
        // The keyboard is back in the pane.
        let app = s.app.clone();
        let back = s.cx.update(|window, cx| {
            let view = app.read(cx).pane_view(PaneId(1)).unwrap().clone();
            view.read(cx).focus_handle().is_focused(window)
        });
        assert!(back);
    }

    #[gpui::test]
    fn dont_ask_again_is_saved_and_skips_the_question(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "# mine\n[gui]\nfont_size = 15\n",
        )
        .unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(7, "/w", 1), row(8, "/w", 2)]);
        s.cx.dispatch_action(Kill { agent_id: 7 });
        click(&mut s, "kill-dont-ask");
        // Checking it kills nothing by itself; Cancel would still cancel.
        assert!(kills(&fake).is_empty());
        click(&mut s, "kill-confirm");
        assert_eq!(kills(&fake), [7]);
        let config = crate::core::config::load_config(dir.path()).unwrap();
        assert!(!config.confirm_kill);
        assert_eq!(config.font_size, 15.0);
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("# mine"), "{text}");
        // From now on, no question.
        s.cx.dispatch_action(Kill { agent_id: 8 });
        assert_eq!(kill_asked(&mut s), None);
        assert_eq!(kills(&fake), [7, 8]);
    }

    /// A key pressed and released (gpui's keyboard click is on key up).
    fn press(s: &mut Setup<'_>, key: &str) {
        s.cx.simulate_keystrokes(key);
        s.cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse(key).unwrap(),
        });
        s.cx.run_until_parked();
    }

    fn dont_ask(s: &mut Setup<'_>) -> bool {
        let sheet =
            s.ws.read_with(s.cx, |w, _| w.kill_sheet().cloned())
                .unwrap();
        sheet.read_with(s.cx, |k, _| k.dont_ask)
    }

    #[gpui::test]
    fn kill_sheet_tabs_to_its_controls_and_space_clicks_them(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(7, "/w", 1)]);
        s.cx.dispatch_action(Kill { agent_id: 7 });
        // As it opens, Space toggles the checkbox.
        press(&mut s, "space");
        assert!(dont_ask(&mut s));
        press(&mut s, "space");
        assert!(!dont_ask(&mut s));
        // Tab: the checkbox, which Space toggles once.
        press(&mut s, "tab");
        press(&mut s, "space");
        assert!(dont_ask(&mut s));
        // Tab: Cancel, which Space presses.
        press(&mut s, "tab");
        press(&mut s, "space");
        assert_eq!(kill_asked(&mut s), None, "cancelled");
        assert!(kills(&fake).is_empty());

        // Return is the default button (Kill) even on a focused Cancel.
        s.cx.dispatch_action(Kill { agent_id: 7 });
        press(&mut s, "tab");
        press(&mut s, "tab");
        press(&mut s, "enter");
        assert_eq!(kills(&fake), [7]);
        assert!(
            crate::core::config::load_config(dir.path())
                .unwrap()
                .confirm_kill
        );
    }

    #[gpui::test]
    fn new_agent_sheet_tabs_through_its_fields(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        s.cx.simulate_keystrokes("cmd-n");
        s.cx.run_until_parked();
        let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
        let focused = |s: &mut Setup<'_>, f: fn(&NewAgentSheet) -> &Entity<TextField>| {
            s.cx.update(|window, cx| f(sheet.read(cx)).read(cx).focus_handle().is_focused(window))
        };
        assert!(focused(&mut s, |sh| &sh.command));
        press(&mut s, "tab");
        assert!(focused(&mut s, |sh| &sh.prompt));
        s.cx.simulate_input("fix it");
        let prompt = sheet.read_with(s.cx, |sh, cx| sh.prompt.read(cx).text().to_owned());
        assert_eq!(prompt, "fix it");
        press(&mut s, "tab");
        assert!(focused(&mut s, |sh| &sh.name));
        press(&mut s, "tab");
        assert!(focused(&mut s, |sh| &sh.labels));
        // Back past Name, Prompt, Command and Choose… to the directory.
        for _ in 0..5 {
            press(&mut s, "shift-tab");
        }
        assert!(focused(&mut s, |sh| &sh.dir));
        // ⌘Tab is not focus movement.
        s.cx.simulate_keystrokes("cmd-tab");
        assert!(focused(&mut s, |sh| &sh.dir));
    }

    #[gpui::test]
    fn confirm_kill_off_kills_at_once(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let config = GuiConfig {
            confirm_kill: false,
            ..GuiConfig::default()
        };
        let mut s = setup_with(cx, &fake, dir.path(), config);
        agents(&mut s, &fake, vec![row(7, "/w", 1)]);
        s.cx.dispatch_action(Kill { agent_id: 7 });
        assert_eq!(kill_asked(&mut s), None);
        assert_eq!(kills(&fake), [7]);
    }

    #[gpui::test]
    fn cmd_shift_w_kills_the_focused_panes_running_agent(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        // An empty pane: nothing to kill.
        s.cx.simulate_keystrokes("cmd-shift-w");
        assert_eq!(kill_asked(&mut s), None);
        agents(&mut s, &fake, vec![row(7, "/w", 1)]);
        click(&mut s, "tab-7");
        s.cx.simulate_keystrokes("cmd-shift-w");
        assert_eq!(kill_asked(&mut s), Some(7));
        s.cx.simulate_keystrokes("escape");
        // Not for an agent that exited.
        agents(&mut s, &fake, vec![ended(7, "/w", 1, AgentState::Exited)]);
        s.cx.simulate_keystrokes("cmd-shift-w");
        assert_eq!(kill_asked(&mut s), None);
        assert!(kills(&fake).is_empty());
        // The pane stays open either way.
        assert_eq!(panes(&mut s), [PaneId(1)]);
    }

    #[gpui::test]
    fn the_pane_header_offers_stop_or_restart_and_remove(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        assert!(!drawn(&mut s, "pane-1-stop"), "an empty pane has no Stop");
        agents(&mut s, &fake, vec![row(7, "/w", 1)]);
        click(&mut s, "tab-7");
        attach_model(&fake, PaneId(1), 7);
        assert!(!drawn(&mut s, "pane-1-remove"));
        // Always shown on the focused pane.
        click(&mut s, "pane-1-stop");
        assert_eq!(kill_asked(&mut s), Some(7));
        click(&mut s, "kill-confirm");
        assert_eq!(kills(&fake), [7]);
        // Close (×) still only detaches.
        assert!(!fake.calls().contains(&Call::Close(PaneId(1))));

        // Ended: Restart and Remove instead. (gpui's test frames keep the
        // bounds of elements drawn before, so Stop's absence is checked in
        // `core::kill`.)
        agents(&mut s, &fake, vec![ended(7, "/w", 1, AgentState::Exited)]);
        fake.model(PaneId(1)).lock().unwrap().on_exited(3);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        click(&mut s, "pane-1-restart-agent");
        assert!(
            fake.calls()
                .iter()
                .any(|c| matches!(c, Call::RestartInPane { agent_id: 7, .. })),
            "{:?}",
            fake.calls()
        );
        click(&mut s, "pane-1-remove");
        assert_eq!(fake.calls().last(), Some(&Call::Forget(vec![7])));
    }

    #[gpui::test]
    fn an_unfocused_panes_stop_shows_on_hover(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(7, "/w", 1)]);
        click(&mut s, "tab-7");
        // Pane 2 takes the focus; pane 1 keeps agent 7.
        s.cx.simulate_keystrokes("cmd-d");
        assert_eq!(focused(&mut s), PaneId(2));
        // Hovering the header shows the button; a click on it asks.
        hover(&mut s, "pane-1-stop");
        click(&mut s, "pane-1-stop");
        assert_eq!(kill_asked(&mut s), Some(7));
    }

    #[gpui::test]
    fn the_pane_menu_kills_its_agent(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(7, "/w", 1)]);
        click(&mut s, "tab-7");
        s.ws.update_in(s.cx, |ws, window, cx| {
            let epoch = ws.epoch;
            ws.pane_menu_pick(PaneId(1), PaneMenuItem::Kill, epoch, window, cx);
        });
        assert_eq!(kill_asked(&mut s), Some(7));
    }

    #[gpui::test]
    fn a_rows_buttons_act_on_hover_without_showing_the_agent(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(
            &mut s,
            &fake,
            vec![row(7, "/w", 1), ended(8, "/w", 2, AgentState::Exited)],
        );
        assert!(!drawn(&mut s, "tab-7-restart") && !drawn(&mut s, "tab-8-stop"));
        hover(&mut s, "tab-7");
        click(&mut s, "tab-7-stop");
        assert_eq!(kill_asked(&mut s), Some(7));
        s.cx.simulate_keystrokes("escape");
        // The row's own click did not run.
        assert!(!fake.calls().iter().any(|c| matches!(c, Call::Attach(_, 7))));
        hover(&mut s, "tab-8");
        click(&mut s, "tab-8-restart");
        assert_eq!(fake.calls().last(), Some(&Call::Restart(8)));
        hover(&mut s, "tab-8");
        click(&mut s, "tab-8-remove");
        assert_eq!(fake.calls().last(), Some(&Call::Forget(vec![8])));
        assert!(!fake.calls().iter().any(|c| matches!(c, Call::Attach(_, 8))));
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

        let name = sheet.read_with(s.cx, |sh, cx| sh.name.read(cx).text().to_owned());
        assert!(name.starts_with("brief-"), "{name}");
        s.cx.simulate_keystrokes("enter");
        assert!(fake.calls().contains(&Call::Spawn {
            req: SpawnRequest {
                program: "claude".into(),
                args: vec!["--append-system-prompt".into(), "be brief".into()],
                cwd: "/r1".into(),
                labels: HashMap::from([("name".into(), name)]),
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
        s.cx.run_until_parked();
        let name = sheet.read_with(s.cx, |sh, cx| sh.name.read(cx).text().to_owned());
        assert!(name.starts_with("echo-"), "{name}");
        s.cx.simulate_keystrokes("enter");

        let size = s.app.read_with(s.cx, |a, cx| {
            a.pane_view(PaneId(1)).unwrap().read(cx).grid().unwrap()
        });
        assert!(fake.calls().contains(&Call::Spawn {
            req: SpawnRequest {
                program: "sh".into(),
                args: vec!["-c".into(), "echo hi".into()],
                cwd: "/tmp".into(),
                labels: HashMap::from([("name".into(), name)]),
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

    fn sheet_text(
        s: &mut Setup<'_>,
        field: impl Fn(&NewAgentSheet) -> Entity<TextField>,
    ) -> String {
        let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
        let field = sheet.read_with(s.cx, |sh, _| field(sh));
        field.read_with(s.cx, |f, _| f.text().to_owned())
    }

    fn set_sheet_text(
        s: &mut Setup<'_>,
        field: impl Fn(&NewAgentSheet) -> Entity<TextField>,
        text: &str,
    ) {
        let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
        let field = sheet.read_with(s.cx, |sh, _| field(sh));
        let text = text.to_owned();
        field.update(s.cx, |f, cx| f.set_text(text, cx));
        s.cx.run_until_parked();
    }

    fn last_spawn(fake: &Fake) -> SpawnRequest {
        fake.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Spawn { req, .. } => Some(req),
                _ => None,
            })
            .next_back()
            .expect("a spawn")
    }

    /// The Name field follows the command and directory until the user
    /// edits it; the generated name avoids live agents' names; labels
    /// are `k=v` pairs.
    #[gpui::test]
    fn new_agent_name_follows_the_command_until_edited(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        s.cx.simulate_keystrokes("cmd-n");
        let name = |sh: &NewAgentSheet| sh.name.clone();
        let command = |sh: &NewAgentSheet| sh.command.clone();
        set_sheet_text(&mut s, |sh| sh.dir.clone(), "/w/fleetd");
        set_sheet_text(&mut s, command, "claude");
        assert!(sheet_text(&mut s, name).starts_with("fleetd-"));
        set_sheet_text(&mut s, command, "claude 'fix the SIGTERM race'");
        let generated = sheet_text(&mut s, name);
        assert!(generated.starts_with("sigterm-"), "{generated}");
        // Typed by the user: it stays.
        set_sheet_text(&mut s, name, "my-agent");
        set_sheet_text(&mut s, command, "claude 'tune LTO'");
        assert_eq!(sheet_text(&mut s, name), "my-agent");
        set_sheet_text(&mut s, |sh| sh.labels.clone(), "role=reviewer, team=core");
        s.cx.simulate_keystrokes("enter");
        let req = last_spawn(&fake);
        assert_eq!(req.args, ["tune LTO"]);
        assert_eq!(
            req.labels,
            HashMap::from([
                ("name".to_owned(), "my-agent".to_owned()),
                ("role".to_owned(), "reviewer".to_owned()),
                ("team".to_owned(), "core".to_owned()),
            ])
        );

        // A live agent has the name the command would get: another one.
        let mut taken = row(1, "/w/fleetd", 1);
        taken.info.labels.insert("name".into(), generated.clone());
        agents(&mut s, &fake, vec![taken]);
        s.cx.simulate_keystrokes("cmd-n");
        set_sheet_text(&mut s, |sh| sh.dir.clone(), "/w/fleetd");
        set_sheet_text(&mut s, command, "claude 'fix the SIGTERM race'");
        let other = sheet_text(&mut s, name);
        assert!(
            other.starts_with("sigterm-") && other != generated,
            "{other}"
        );
        // Bad labels or names keep the sheet open with the error.
        set_sheet_text(&mut s, |sh| sh.labels.clone(), "role");
        s.cx.simulate_keystrokes("enter");
        let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
        assert!(sheet.read_with(s.cx, |sh, _| sh.error.clone()).is_some());
        set_sheet_text(&mut s, |sh| sh.labels.clone(), "");
        set_sheet_text(&mut s, name, "12");
        s.cx.simulate_keystrokes("enter");
        assert!(s.ws.read_with(s.cx, |w, _| w.sheet().is_some()));
    }

    fn named(id: u64, name: &str, labels: &[(&str, &str)]) -> AgentRow {
        let mut r = row(id, "/w", id as i64);
        r.info.labels.insert("name".into(), name.into());
        for (k, v) in labels {
            r.info.labels.insert((*k).into(), (*v).into());
        }
        r
    }

    /// Rows show their labels as chips (not the name, project or
    /// `fleet.*`), and the pane header the name with `program · id`.
    #[gpui::test]
    fn rows_and_headers_show_names_and_chips(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let labels = [
            ("role", "reviewer"),
            ("project", "p"),
            ("fleet.parent", "2"),
        ];
        agents(
            &mut s,
            &fake,
            vec![named(1, "lto-falcon", &labels), row(2, "/w", 2)],
        );
        assert!(s.cx.debug_bounds("tab-1-name").is_some());
        assert!(s.cx.debug_bounds("tab-1-detail").is_some());
        assert!(s.cx.debug_bounds("tab-1-chip-role").is_some());
        for hidden in [
            "tab-1-chip-name",
            "tab-1-chip-project",
            "tab-1-chip-fleet.parent",
        ] {
            assert!(s.cx.debug_bounds(hidden).is_none(), "{hidden}");
        }
        assert!(s.cx.debug_bounds("tab-2-chip-role").is_none());
        click(&mut s, "tab-1");
        attach_model(&fake, PaneId(1), 1);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        let chrome = s.app.read_with(s.cx, |a, cx| {
            a.pane_view(PaneId(1)).unwrap().read(cx).chrome.clone()
        });
        assert_eq!(chrome.name.as_deref(), Some("lto-falcon"));
        assert_eq!(chrome.agent, Some(1));
        assert!(s.cx.debug_bounds("pane-1-name").is_some());
        assert!(s.cx.debug_bounds("pane-1-detail").is_some());
    }

    fn double_click(s: &mut Setup<'_>, selector: &'static str) {
        let at = center(s, selector);
        for click_count in [1, 2] {
            s.cx.simulate_event(gpui::MouseDownEvent {
                position: at,
                button: MouseButton::Left,
                modifiers: Modifiers::none(),
                click_count,
                first_mouse: false,
            });
            s.cx.simulate_event(gpui::MouseUpEvent {
                position: at,
                button: MouseButton::Left,
                modifiers: Modifiers::none(),
                click_count,
            });
        }
        s.cx.run_until_parked();
    }

    fn rename_to(s: &mut Setup<'_>, name: &str) {
        let sheet =
            s.ws.read_with(s.cx, |w, _| w.rename_sheet().cloned())
                .expect("rename sheet open");
        let field = sheet.read_with(s.cx, |sh, _| sh.name.clone());
        let name = name.to_owned();
        field.update(s.cx, |f, cx| f.set_text(name, cx));
        s.cx.simulate_keystrokes("enter");
    }

    fn asking(id: u64, question_id: u64) -> AgentRow {
        let mut r = row(id, "/w", id as i64);
        r.info.reported = Some(fleet_proto::Reported {
            state: fleet_proto::ReportedState::ReportedWaiting.into(),
            message: String::new(),
            at_unix_ms: 1,
            question: Some(fleet_proto::Question {
                id: question_id,
                text: "Which branch?".into(),
                choices: vec!["main".into(), "dev".into()],
            }),
        });
        r
    }

    /// ⌘J (ADR 0100): shows the agents that need you in turn, wrapping;
    /// with none, a short toast and the pane unchanged.
    #[gpui::test]
    fn cmd_j_walks_the_agents_that_need_you(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let shown = |s: &mut Setup<'_>| s.app.read_with(s.cx, |a, _| a.layout().agent(a.focused()));
        agents(&mut s, &fake, vec![row(2, "/w", 2)]);
        s.cx.simulate_keystrokes("cmd-j");
        assert_eq!(
            s.app.read_with(s.cx, |a, _| a.notice().map(str::to_owned)),
            Some("No agent needs you".to_owned())
        );
        assert_ne!(shown(&mut s), Some(2));

        agents(
            &mut s,
            &fake,
            vec![row(2, "/w", 2), asking(3, 9), asking(4, 10)],
        );
        s.cx.simulate_keystrokes("cmd-j");
        assert_eq!(shown(&mut s), Some(3));
        s.cx.simulate_keystrokes("cmd-j");
        assert_eq!(shown(&mut s), Some(4));
        s.cx.simulate_keystrokes("cmd-j");
        assert_eq!(shown(&mut s), Some(3));
    }

    /// A question (ADR 0078): choices answer from the row and from the
    /// pane header, Reply… opens the sheet for free text, and a question
    /// that is closed (or an ended agent's) shows nothing.
    #[gpui::test]
    fn a_question_is_answered_from_the_row_the_header_and_the_sheet(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![asking(3, 9), row(4, "/w", 4)]);
        assert!(s.cx.debug_bounds("row-question-3-Which branch?").is_some());
        assert!(s.cx.debug_bounds("row-reply-4").is_none());
        click(&mut s, "row-choice-3-dev");
        assert!(fake.calls().contains(&Call::Answer(3, 9, "dev".into())));
        // Not also the row's click: no pane took the agent.
        assert!(s.ws.read_with(s.cx, |w, _| w.answer_sheet().is_none()));

        // Reply…: the sheet with the text field.
        click(&mut s, "row-reply-3");
        let sheet =
            s.ws.read_with(s.cx, |w, _| w.answer_sheet().cloned())
                .expect("answer sheet open");
        // Nothing typed: refused, still open.
        s.cx.simulate_keystrokes("enter");
        assert!(sheet.read_with(s.cx, |sh, _| sh.error.clone()).is_some());
        let field = sheet.read_with(s.cx, |sh, _| sh.reply.clone());
        field.update(s.cx, |f, cx| f.set_text("something else".into(), cx));
        s.cx.simulate_keystrokes("enter");
        assert!(
            fake.calls()
                .contains(&Call::Answer(3, 9, "something else".into()))
        );
        assert!(s.ws.read_with(s.cx, |w, _| w.answer_sheet().is_none()));

        // A choice in the sheet; Escape cancels.
        click(&mut s, "row-reply-3");
        s.cx.simulate_keystrokes("escape");
        assert!(s.ws.read_with(s.cx, |w, _| w.answer_sheet().is_none()));

        // The pane header strip.
        attach_model(&fake, PaneId(1), 3);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        click(&mut s, "pane-choice-3-main");
        assert!(fake.calls().contains(&Call::Answer(3, 9, "main".into())));
    }

    /// A question that was answered (the agent reported on) or whose agent
    /// ended shows nothing.
    #[gpui::test]
    fn a_closed_question_shows_nothing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![asking(3, 9), asking(4, 10)]);
        assert!(s.cx.debug_bounds("row-reply-3").is_some());
        assert!(s.cx.debug_bounds("row-reply-4").is_some());
        let mut gone = asking(3, 9);
        gone.info.set_state(AgentState::Exited);
        agents(&mut s, &fake, vec![gone, row(4, "/w", 4)]);
        assert!(s.cx.debug_bounds("row-reply-3").is_none());
        assert!(s.cx.debug_bounds("row-reply-4").is_none());
    }

    /// Rename: a double-click on the name in the row or the header, or
    /// Rename… in the row and pane menus.
    #[gpui::test]
    fn rename_from_row_header_and_menus(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![named(3, "old-name", &[])]);
        double_click(&mut s, "tab-3-name");
        let sheet = s.ws.read_with(s.cx, |w, _| w.rename_sheet().cloned());
        let text = sheet
            .expect("rename sheet open")
            .read_with(s.cx, |sh, cx| sh.name.read(cx).text().to_owned());
        assert_eq!(text, "old-name");
        rename_to(&mut s, "  new-name ");
        assert!(fake.calls().contains(&Call::Rename(3, "new-name".into())));
        assert!(s.ws.read_with(s.cx, |w, _| w.rename_sheet().is_none()));

        // A bad name keeps the sheet open; Escape cancels.
        pick_row(&mut s, 3, RowMenuItem::Rename);
        rename_to(&mut s, "123");
        let sheet =
            s.ws.read_with(s.cx, |w, _| w.rename_sheet().cloned())
                .unwrap();
        assert!(sheet.read_with(s.cx, |sh, _| sh.error.clone()).is_some());
        s.cx.simulate_keystrokes("escape");
        assert!(s.ws.read_with(s.cx, |w, _| w.rename_sheet().is_none()));

        // The header: shown in pane 1.
        attach_model(&fake, PaneId(1), 3);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        double_click(&mut s, "pane-1-name");
        rename_to(&mut s, "from-header");
        assert!(
            fake.calls()
                .contains(&Call::Rename(3, "from-header".into()))
        );
        s.ws.update_in(s.cx, |ws, window, cx| {
            let epoch = ws.epoch;
            ws.pane_menu_pick(PaneId(1), PaneMenuItem::Rename, epoch, window, cx);
        });
        rename_to(&mut s, "from-menu");
        assert!(fake.calls().contains(&Call::Rename(3, "from-menu".into())));
    }

    fn labels_sheet(s: &mut Setup<'_>) -> Entity<super::LabelsSheet> {
        s.ws.read_with(s.cx, |w, _| w.labels_sheet().cloned())
            .expect("labels sheet open")
    }

    fn label_rows(s: &mut Setup<'_>) -> Vec<String> {
        labels_sheet(s).read_with(s.cx, |sh, cx| {
            sh.rows
                .iter()
                .map(|r| r.read(cx).text().to_owned())
                .collect()
        })
    }

    fn set_label_row(s: &mut Setup<'_>, index: usize, text: &str) {
        let field = labels_sheet(s).read_with(s.cx, |sh, _| sh.rows[index].clone());
        let text = text.to_owned();
        field.update(s.cx, |f, cx| f.set_text(text, cx));
    }

    /// Edit Labels… in the row and pane menus: the editable labels as
    /// rows (not the name or `fleet.*`); add, edit and remove go out as
    /// one `SetLabels`.
    #[gpui::test]
    fn edit_labels_from_row_and_pane_menus(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let labels = [("role", "coder"), ("team", "core"), ("fleet.parent", "2")];
        agents(&mut s, &fake, vec![named(3, "otter", &labels)]);
        pick_row(&mut s, 3, RowMenuItem::EditLabels);
        assert_eq!(label_rows(&mut s), ["role=coder", "team=core"]);
        assert!(s.cx.debug_bounds("labels-sheet").is_some());

        // Edit one, remove one, add one; Return saves.
        set_label_row(&mut s, 0, "role = reviewer");
        click(&mut s, "label-remove-1");
        assert_eq!(label_rows(&mut s), ["role = reviewer"]);
        labels_sheet(&mut s).update_in(s.cx, |sh, window, cx| sh.add_row(window, cx));
        set_label_row(&mut s, 1, "new=1");
        s.cx.simulate_keystrokes("enter");
        assert_eq!(
            fake.calls().last(),
            Some(&Call::SetLabels(
                3,
                vec![
                    ("new".into(), "1".into()),
                    ("role".into(), "reviewer".into())
                ],
                vec!["team".into()]
            ))
        );
        assert!(s.ws.read_with(s.cx, |w, _| w.labels_sheet().is_none()));

        // A bad row keeps the sheet open; Escape cancels; no change sends
        // nothing.
        let before = fake.calls().len();
        pick_row(&mut s, 3, RowMenuItem::EditLabels);
        set_label_row(&mut s, 0, "name=x");
        s.cx.simulate_keystrokes("enter");
        let sheet = labels_sheet(&mut s);
        assert!(sheet.read_with(s.cx, |sh, _| sh.error.clone()).is_some());
        s.cx.simulate_keystrokes("escape");
        assert!(s.ws.read_with(s.cx, |w, _| w.labels_sheet().is_none()));
        pick_row(&mut s, 3, RowMenuItem::EditLabels);
        s.cx.simulate_keystrokes("enter");
        assert!(s.ws.read_with(s.cx, |w, _| w.labels_sheet().is_none()));
        assert_eq!(fake.calls().len(), before);

        // The pane menu: shown in pane 1.
        attach_model(&fake, PaneId(1), 3);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        s.ws.update_in(s.cx, |ws, window, cx| {
            let epoch = ws.epoch;
            ws.pane_menu_pick(PaneId(1), PaneMenuItem::EditLabels, epoch, window, cx);
        });
        s.cx.run_until_parked();
        assert_eq!(label_rows(&mut s), ["role=coder", "team=core"]);
    }

    /// A click on a label chip filters the sidebar by it; a second click
    /// clears it.
    #[gpui::test]
    fn a_label_chip_filters_the_sidebar(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(
            &mut s,
            &fake,
            vec![
                named(1, "otter", &[("role", "reviewer")]),
                named(2, "falcon", &[("role", "coder")]),
            ],
        );
        click(&mut s, "tab-1-chip-role");
        let query = |s: &mut Setup<'_>| {
            s.ws.read_with(s.cx, |w, cx| w.filter_field.read(cx).text().to_owned())
        };
        assert_eq!(query(&mut s), "role=reviewer");
        assert_eq!(tabs_drawn(&mut s), [1], "filtered out");
        // The chip's click is not the row's: no pane shows the agent.
        let pane = focused(&mut s);
        assert_eq!(shown(&mut s, pane), None);
        click(&mut s, "tab-1-chip-role");
        assert_eq!(query(&mut s), "");
        let mut all = tabs_drawn(&mut s);
        all.sort();
        assert_eq!(all, [1, 2]);
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
    fn file_links_resolve_in_a_local_agents_directory_only(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(5, "/w", 1)]);
        click(&mut s, "tab-5");
        attach_model(&fake, PaneId(1), 5);
        s.cx.run_until_parked();
        let chrome = |s: &mut Setup<'_>| {
            s.app.read_with(s.cx, |a, cx| {
                let c = a.pane_view(PaneId(1)).unwrap().read(cx).chrome.clone();
                (c.cwd, c.link_dir)
            })
        };
        assert_eq!(chrome(&mut s), (Some("/w".into()), Some("/w".into())));

        // On another machine the agent's paths are not here.
        let other = Arc::new(Fake::default());
        let session: Arc<dyn SessionApi> = other.clone();
        let (_tx, rx) = async_channel::unbounded();
        let target = Target::Ssh {
            dest: "me@box".into(),
        };
        *other.agents.lock().unwrap() = vec![row(5, "/w", 1)];
        s.app
            .update(s.cx, |a, cx| a.replace_session(session, rx, target, cx));
        click(&mut s, "tab-5");
        attach_model(&other, PaneId(1), 5);
        s.cx.run_until_parked();
        assert_eq!(chrome(&mut s), (Some("/w".into()), None));
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

    /// Issue #41: a row shows the agent's last line under its name, and
    /// its run time, or how it ended, on the right.
    #[gpui::test]
    fn rows_show_last_line_and_run_time_or_exit(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let mut running = row(1, "/w", now_ms() - 125_000);
        running.last_line = "compiling\tfleetd\n".into();
        agents(
            &mut s,
            &fake,
            vec![
                running,
                ended(2, "/w", 2, AgentState::Exited),
                ended(3, "/w", 3, AgentState::Lost),
            ],
        );
        assert!(s.cx.debug_bounds("meta-1-2m").is_some());
        assert!(s.cx.debug_bounds("preview-1-compiling fleetd").is_some());
        assert!(s.cx.debug_bounds("meta-2-exit 3").is_some());
        assert!(s.cx.debug_bounds("meta-3-lost").is_some());
    }

    /// Issue #41: output alone redraws a row at most once a second (on the
    /// sidebar's tick); a change of state redraws at once.
    #[gpui::test]
    fn output_alone_waits_for_the_tick(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(1, "/w", 1)]);
        let notified = Arc::new(AtomicUsize::new(0));
        let n = notified.clone();
        let _sub = s.cx.update(|_, cx| {
            cx.observe(&s.app, move |_, _| {
                n.fetch_add(1, Ordering::SeqCst);
            })
        });

        let mut printed = row(1, "/w", 1);
        printed.last_line = "new output".into();
        // Quiet output: the status stays idle (a new status redraws at once).
        printed.last_output_unix_ms = Some(now_ms() - 60_000);
        agents(&mut s, &fake, vec![printed.clone()]);
        assert_eq!(notified.load(Ordering::SeqCst), 0);
        // The data is current (statuses read it); only the redraw waits.
        let line = s.app.read_with(s.cx, |a, _| a.agents[0].last_line.clone());
        assert_eq!(line, "new output");
        s.cx.executor().advance_clock(Duration::from_secs(1));
        s.cx.run_until_parked();
        assert!(s.cx.debug_bounds("preview-1-new output").is_some());

        let mut exited = printed;
        exited.info.set_state(AgentState::Exited);
        exited.info.exit_code = Some(0);
        agents(&mut s, &fake, vec![exited]);
        assert!(notified.load(Ordering::SeqCst) > 0);
        assert!(s.cx.debug_bounds("meta-1-exit 0").is_some());
    }

    /// Issue #32: a bell where you are not looking flashes the pane, marks
    /// the row and counts as needs you until the pane is seen; shells get
    /// no status dot.
    #[gpui::test]
    fn a_bell_marks_the_agent_until_its_pane_is_seen(cx: &mut TestAppContext) {
        use crate::core::status::AgentStatus;
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let mut shell = row(2, "/w", 2);
        shell.info.labels.insert("kind".into(), "shell".into());
        agents(&mut s, &fake, vec![row(1, "/w", 1), shell]);
        s.app.update(s.cx, |a, cx| a.show_agent(1, cx));
        fake.model(PaneId(1))
            .lock()
            .unwrap()
            .on_attached(&AttachOk {
                agent_id: 1,
                cols: 80,
                rows: 24,
                ..Default::default()
            });
        s.cx.run_until_parked();
        assert!(s.cx.debug_bounds("status-1-Idle").is_some());
        assert!(s.cx.debug_bounds("pane-1-status-Idle").is_some());
        assert!(s.cx.debug_bounds("status-2-Idle").is_none());

        let ring = |s: &mut Setup<'_>| {
            s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
            fake.model(PaneId(1)).lock().unwrap().on_output(b"ding\x07");
            s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
            s.cx.run_until_parked();
        };
        let flashing = |s: &mut Setup<'_>| {
            s.app.read_with(s.cx, |a, cx| {
                a.pane_view(PaneId(1)).unwrap().read(cx).flashing()
            })
        };
        let marked = |s: &mut Setup<'_>| {
            s.app.read_with(s.cx, |a, _| {
                (
                    a.bells.contains(&1),
                    a.statuses.get(&1).copied(),
                    a.needs_you_count(),
                )
            })
        };

        // Looking at it: the pane flashes, nothing is marked.
        ring(&mut s);
        assert!(flashing(&mut s));
        assert_eq!(marked(&mut s), (false, Some(AgentStatus::Idle), 0));

        // Away from the window: marked, and it needs you.
        s.app.update(s.cx, |a, cx| a.set_window_active(false, cx));
        ring(&mut s);
        assert_eq!(marked(&mut s), (true, Some(AgentStatus::NeedsYou), 1));
        assert!(s.cx.debug_bounds("status-1-Needs you").is_some());
        assert!(s.cx.debug_bounds("pane-1-status-Needs you").is_some());

        // Back at the window, with its pane focused: seen.
        s.app.update(s.cx, |a, cx| a.set_window_active(true, cx));
        s.cx.run_until_parked();
        assert_eq!(marked(&mut s), (false, Some(AgentStatus::Idle), 0));
    }

    /// Issue #33: each agent's colour, from its id, marks its row and its
    /// pane header; shells get one too.
    #[gpui::test]
    fn rows_and_pane_headers_carry_the_agent_colour(cx: &mut TestAppContext) {
        use crate::core::identity::agent_colour;
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let mut shell = row(2, "/w", 2);
        shell.info.labels.insert("kind".into(), "shell".into());
        agents(&mut s, &fake, vec![row(1, "/w", 1), shell]);
        s.app.update(s.cx, |a, cx| a.show_agent(1, cx));
        s.cx.run_until_parked();
        let (one, two) = (agent_colour(1).name, agent_colour(2).name);
        assert_ne!(one, two);
        assert!(
            s.cx.debug_bounds(leak(format!("identity-1-{one}")))
                .is_some()
        );
        assert!(
            s.cx.debug_bounds(leak(format!("identity-2-{two}")))
                .is_some()
        );
        assert!(
            s.cx.debug_bounds(leak(format!("pane-1-identity-{one}")))
                .is_some()
        );
        assert!(s.cx.debug_bounds("identity-1-emoji").is_none());
        assert!(s.cx.debug_bounds("pane-1-identity-emoji").is_none());
    }

    /// `[gui] agent_emoji` puts the agent's emoji in place of the swatch
    /// and the header icon.
    #[gpui::test]
    fn agent_emoji_replaces_the_swatch(cx: &mut TestAppContext) {
        use crate::core::identity::agent_colour;
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let config = GuiConfig {
            agent_emoji: true,
            ..GuiConfig::default()
        };
        let mut s = setup_with(cx, &fake, dir.path(), config);
        agents(&mut s, &fake, vec![row(1, "/w", 1)]);
        s.app.update(s.cx, |a, cx| a.show_agent(1, cx));
        s.cx.run_until_parked();
        assert!(s.cx.debug_bounds("identity-1-emoji").is_some());
        assert!(s.cx.debug_bounds("pane-1-identity-emoji").is_some());
        let one = agent_colour(1).name;
        assert!(
            s.cx.debug_bounds(leak(format!("identity-1-{one}")))
                .is_none()
        );
    }

    /// `debug_bounds` takes a `&'static str`.
    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
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
        let (open, epoch) =
            s.ws.read_with(s.cx, |ws, _| (ws.sheet().is_some(), ws.epoch));
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
        let closed = s.ws.read_with(s.cx, |ws, _| ws.sheet().is_none());
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
            [
                RowMenuItem::OpenSplit,
                RowMenuItem::Rename,
                RowMenuItem::EditLabels,
                RowMenuItem::Duplicate,
                RowMenuItem::SendTo,
                RowMenuItem::SaveOutput,
                RowMenuItem::OpenOutput,
                RowMenuItem::Kill
            ]
        );
        for id in [8, 6] {
            assert_eq!(
                row_items(&mut s, id),
                [
                    RowMenuItem::OpenSplit,
                    RowMenuItem::Rename,
                    RowMenuItem::EditLabels,
                    RowMenuItem::Duplicate,
                    RowMenuItem::SaveOutput,
                    RowMenuItem::OpenOutput,
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
        assert_eq!(kill_asked(&mut s), Some(7));
    }

    fn sends(fake: &Fake) -> Vec<(u64, Vec<u8>)> {
        fake.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Send(id, data) => Some((id, data)),
                _ => None,
            })
            .collect()
    }

    fn send_sheet(s: &mut Setup<'_>) -> Option<Entity<SendSheet>> {
        s.ws.read_with(s.cx, |w, _| w.send_sheet().cloned())
    }

    /// ⇧⌘M: the message, two agents checked from the filter (by label),
    /// Send; the text goes now, the Return a moment later.
    #[gpui::test]
    fn send_to_agents_types_into_the_checked_agents(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(
            &mut s,
            &fake,
            vec![
                named(1, "alpha", &[("role", "reviewer")]),
                named(2, "beta", &[("role", "reviewer")]),
                named(3, "gamma", &[("role", "coder")]),
                ended(4, "/w", 4, AgentState::Exited),
            ],
        );
        s.cx.simulate_keystrokes("cmd-shift-m");
        let sheet = send_sheet(&mut s).expect("send sheet open");
        // Running agents only.
        let listed = sheet.read_with(s.cx, |sh, cx| sh.picker.read(cx).picker().items().len());
        assert_eq!(listed, 3);
        // Return in the message box is a line break, not Send.
        s.cx.simulate_input("rebase");
        s.cx.simulate_keystrokes("enter");
        s.cx.simulate_input("on main");
        let text = sheet.read_with(s.cx, |sh, cx| sh.message.read(cx).text().to_owned());
        assert_eq!(text, "rebase\non main");
        // Tab to the filter; Return checks the best match.
        s.cx.simulate_keystrokes("tab");
        s.cx.simulate_input("beta");
        s.cx.simulate_keystrokes("enter");
        sheet.update(s.cx, |sh, cx| sh.toggle(1, cx));
        let picked = sheet.read_with(s.cx, |sh, _| sh.form.selected().to_vec());
        assert_eq!(picked, [2, 1]);
        s.cx.simulate_keystrokes("cmd-enter");
        assert!(send_sheet(&mut s).is_none(), "sent and closed");
        // As typed (no pane asked for bracketed paste), the Return later.
        assert_eq!(
            sends(&fake),
            [
                (2, b"rebase\non main".to_vec()),
                (1, b"rebase\non main".to_vec())
            ]
        );
        s.cx.executor()
            .advance_clock(std::time::Duration::from_millis(RETURN_DELAY_MS));
        s.cx.run_until_parked();
        assert_eq!(
            sends(&fake)[2..],
            [(2, b"\r".to_vec()), (1, b"\r".to_vec())]
        );
    }

    /// A working target is warned about first; Send again sends. With
    /// Return after off, only the text goes.
    #[gpui::test]
    fn send_warns_before_typing_into_a_working_agent(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let mut busy = named(5, "busy", &[]);
        busy.last_output_unix_ms = Some(now_ms());
        agents(&mut s, &fake, vec![busy, named(6, "calm", &[])]);
        pick_row(&mut s, 5, RowMenuItem::SendTo);
        let sheet = send_sheet(&mut s).expect("send sheet open");
        assert_eq!(
            sheet.read_with(s.cx, |sh, _| sh.form.selected().to_vec()),
            [5]
        );
        s.cx.simulate_input("stop");
        click(&mut s, "send-return");
        click(&mut s, "send-confirm");
        let warning = sheet.read_with(s.cx, |sh, _| sh.warning.clone());
        assert_eq!(
            warning.as_deref(),
            Some("busy is working: the text lands mid-turn. Send anyway?")
        );
        assert!(sends(&fake).is_empty(), "nothing sent yet");
        click(&mut s, "send-confirm");
        assert!(send_sheet(&mut s).is_none());
        s.cx.executor()
            .advance_clock(std::time::Duration::from_millis(RETURN_DELAY_MS));
        s.cx.run_until_parked();
        assert_eq!(sends(&fake), [(5, b"stop".to_vec())]);
        // Escape cancels without sending.
        s.cx.simulate_keystrokes("cmd-shift-m");
        assert!(send_sheet(&mut s).is_some());
        s.cx.simulate_keystrokes("escape");
        assert!(send_sheet(&mut s).is_none());
        assert_eq!(sends(&fake).len(), 1);
    }

    /// A multi-line text goes to an agent whose pane asked for bracketed
    /// paste as one paste.
    #[gpui::test]
    fn send_pastes_several_lines_where_the_pane_asks_for_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![named(3, "shown", &[])]);
        attach_model(&fake, PaneId(1), 3);
        fake.model(PaneId(1))
            .lock()
            .unwrap()
            .on_output(b"\x1b[?2004h");
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        s.app.update(s.cx, |a, cx| {
            a.send_text(
                &SendPlan {
                    text: "a\nb".into(),
                    targets: vec![3],
                    press_return: false,
                },
                cx,
            );
        });
        assert_eq!(sends(&fake), [(3, b"\x1b[200~a\rb\x1b[201~".to_vec())]);
    }

    /// ⌘-click and ⇧-click pick rows (not shown in a pane); the row menu
    /// then offers Send to Selected…, which checks them all.
    #[gpui::test]
    fn sidebar_rows_are_picked_and_sent_to(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(
            &mut s,
            &fake,
            vec![row(1, "/w", 1), row(2, "/w", 2), row(3, "/w", 3)],
        );
        let order: Vec<u64> = s.ws.read_with(s.cx, |w, _| {
            w.drawn_sidebar
                .iter()
                .flat_map(|(_, ids)| ids.clone())
                .collect()
        });
        let (first, last) = (order[0], order[2]);
        let cmd = Modifiers::command();
        let shift = Modifiers::shift();
        let at = center(&mut s, "tab-1");
        s.cx.simulate_click(at, cmd);
        s.cx.run_until_parked();
        assert_eq!(
            s.ws.read_with(s.cx, |w, _| w.row_selection().ids().to_vec()),
            [1]
        );
        assert_eq!(shown(&mut s, PaneId(1)), None, "a ⌘-click shows nothing");
        let tab = |id: u64| -> &'static str {
            match id {
                1 => "tab-1",
                2 => "tab-2",
                _ => "tab-3",
            }
        };
        let at = center(&mut s, tab(first));
        s.cx.simulate_click(at, Modifiers::none());
        s.cx.run_until_parked();
        assert!(s.ws.read_with(s.cx, |w, _| w.row_selection().is_empty()));
        assert_eq!(shown(&mut s, PaneId(1)), Some(first));
        let at = center(&mut s, tab(last));
        s.cx.simulate_click(at, shift);
        s.cx.run_until_parked();
        let picked =
            s.ws.read_with(s.cx, |w, _| w.row_selection().ids().to_vec());
        assert_eq!(picked, order);
        assert!(row_items(&mut s, 2).contains(&RowMenuItem::SendToSelected));
        pick_row(&mut s, 2, RowMenuItem::SendToSelected);
        let sheet = send_sheet(&mut s).expect("send sheet open");
        assert_eq!(
            sheet.read_with(s.cx, |sh, _| sh.form.selected().to_vec()),
            order
        );
        s.cx.simulate_keystrokes("escape");
        // Escape in the sidebar clears the selection.
        click(&mut s, tab(first));
        s.ws.update_in(s.cx, |w, window, cx| w.focus_sidebar(window, cx));
        let at = center(&mut s, tab(last));
        s.cx.simulate_click(at, cmd);
        s.cx.run_until_parked();
        assert!(!s.ws.read_with(s.cx, |w, _| w.row_selection().is_empty()));
        s.cx.simulate_keystrokes("escape");
        assert!(s.ws.read_with(s.cx, |w, _| w.row_selection().is_empty()));
    }

    /// Selects `ids` in the sidebar, as ⌘-clicks do.
    fn select_rows(s: &mut Setup<'_>, ids: &[u64]) {
        s.ws.update(s.cx, |w, _| {
            for &id in ids {
                w.row_selection.click(id, true, false, ids, None);
            }
        });
    }

    #[gpui::test]
    fn bulk_menu_items_come_with_a_selection_of_several(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(1, "/a", 1), row(2, "/b", 2)]);
        assert!(!row_items(&mut s, 1).contains(&RowMenuItem::KillSelected));
        select_rows(&mut s, &[1, 2]);
        let items = row_items(&mut s, 1);
        assert!(items.contains(&RowMenuItem::KillSelected));
        assert!(items.contains(&RowMenuItem::RestartSelected));
        assert!(items.contains(&RowMenuItem::RemoveSelected));
    }

    #[gpui::test]
    fn kill_selected_asks_once_and_kills_only_the_running(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(
            &mut s,
            &fake,
            vec![
                row(1, "/a", 1),
                row(2, "/b", 2),
                ended(3, "/c", 3, AgentState::Exited),
            ],
        );
        select_rows(&mut s, &[1, 2, 3]);
        pick_row(&mut s, 1, RowMenuItem::KillSelected);
        let question = s.ws.read_with(s.cx, |w, cx| {
            w.kill_sheet().unwrap().read(cx).question.to_string()
        });
        assert_eq!(question, "Kill 2 agents?");
        assert!(kills(&fake).is_empty());
        click(&mut s, "kill-confirm");
        assert_eq!(kill_asked(&mut s), None);
        assert_eq!(kills(&fake), [1, 2]);
    }

    #[gpui::test]
    fn restart_and_remove_selected_act_on_the_exited(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(
            &mut s,
            &fake,
            vec![
                row(1, "/a", 1),
                ended(2, "/b", 2, AgentState::Exited),
                ended(3, "/c", 3, AgentState::Lost),
            ],
        );
        select_rows(&mut s, &[1, 2, 3]);
        pick_row(&mut s, 1, RowMenuItem::RestartSelected);
        let restarts: Vec<_> = fake
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Restart(id) => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(restarts, [2, 3]);
        pick_row(&mut s, 1, RowMenuItem::RemoveSelected);
        assert_eq!(fake.calls().last(), Some(&Call::Forget(vec![2, 3])));
        assert_eq!(kill_asked(&mut s), None);
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
        assert_eq!(
            row_items(&mut s, 7),
            [
                RowMenuItem::Rename,
                RowMenuItem::EditLabels,
                RowMenuItem::Duplicate,
                RowMenuItem::SendTo,
                RowMenuItem::SaveOutput,
                RowMenuItem::OpenOutput,
                RowMenuItem::Kill
            ]
        );
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
        assert_eq!(kill_asked(&mut s), None, "no Kill for an exited agent");
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

    #[gpui::test]
    fn ended_pane_summary_copies_its_last_output_and_removes_its_agent(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(5, "/w", 1)]);
        click(&mut s, "tab-5");
        attach_model(&fake, PaneId(1), 5);
        fake.model(PaneId(1))
            .lock()
            .unwrap()
            .on_output(b"$ make\r\nbuild failed   \r\n");
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        // Running: no summary.
        assert!(!drawn(&mut s, "pane-1-ended"));
        assert!(!drawn(&mut s, "pane-1-remove"));

        fake.model(PaneId(1)).lock().unwrap().on_exited(3);
        let mut exited = ended(5, "/w", 1_000, AgentState::Exited);
        exited.info.exited_unix_ms = Some(193_000);
        agents(&mut s, &fake, vec![exited]);
        assert!(drawn(&mut s, "pane-1-ended"));
        // The bar is told how long the agent ran.
        let run_time = s.app.read_with(s.cx, |a, cx| {
            a.pane_view(PaneId(1)).unwrap().read(cx).chrome.run_time
        });
        assert_eq!(run_time, Some(Duration::from_secs(192)));

        s.cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
        click(&mut s, "pane-1-copy-output");
        let clip = s.cx.read_from_clipboard().and_then(|c| c.text());
        assert_eq!(clip.as_deref(), Some("$ make\nbuild failed"));

        click(&mut s, "pane-1-remove");
        assert_eq!(fake.calls().last(), Some(&Call::Forget(vec![5])));
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
        assert!(s.ws.read_with(s.cx, |ws, _| ws.sheet().is_some()));
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
        assert!(s.ws.read_with(s.cx, |ws, _| ws.sheet().is_some()));
    }

    #[gpui::test]
    fn remove_exited_asks_with_the_count_first(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        // Nothing ended: nothing is asked or sent.
        agents(&mut s, &fake, vec![row(7, "/w", 1)]);
        s.cx.dispatch_action(RemoveExited);
        assert!(!s.cx.has_pending_prompt());
        assert!(!fake.calls().contains(&Call::ClearExited));
        agents(
            &mut s,
            &fake,
            vec![
                row(7, "/w", 1),
                ended(8, "/w", 2, AgentState::Exited),
                ended(9, "/w", 3, AgentState::Lost),
            ],
        );
        s.cx.dispatch_action(RemoveExited);
        assert!(s.cx.has_pending_prompt());
        s.cx.simulate_prompt_answer("Cancel");
        s.cx.run_until_parked();
        assert!(!fake.calls().contains(&Call::ClearExited));
        s.cx.dispatch_action(RemoveExited);
        s.cx.simulate_prompt_answer("Remove");
        s.cx.run_until_parked();
        assert_eq!(fake.calls().last(), Some(&Call::ClearExited));
        // Never a per-id kill or forget of the running agent.
        assert!(!fake.calls().iter().any(|c| matches!(c, Call::Forget(_))));
    }

    #[gpui::test]
    fn remove_exited_from_the_palette_asks_first(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![ended(8, "/w", 2, AgentState::Exited)]);
        s.cx.simulate_keystrokes("cmd-p");
        s.cx.simulate_input("remove exited");
        s.cx.simulate_keystrokes("enter");
        assert!(s.cx.has_pending_prompt());
        s.cx.simulate_prompt_answer("Remove");
        s.cx.run_until_parked();
        assert_eq!(fake.calls().last(), Some(&Call::ClearExited));
    }

    fn child_of(mut r: AgentRow, parent: u64) -> AgentRow {
        r.info
            .labels
            .insert("fleet.parent".into(), parent.to_string());
        r
    }

    #[gpui::test]
    fn remove_exited_keeps_an_ended_parent_of_a_running_agent(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(
            &mut s,
            &fake,
            vec![
                // Ended, but its child runs: stays.
                ended(1, "/w", 1, AgentState::Exited),
                child_of(row(2, "/w", 2), 1),
                // An ended branch: goes, children and all.
                ended(3, "/w", 3, AgentState::Exited),
                child_of(ended(4, "/w", 4, AgentState::Lost), 3),
            ],
        );
        s.cx.dispatch_action(RemoveExited);
        assert!(s.cx.has_pending_prompt());
        s.cx.simulate_prompt_answer("Remove");
        s.cx.run_until_parked();
        let Some(Call::Forget(mut ids)) = fake.calls().last().cloned() else {
            panic!("no forget: {:?}", fake.calls());
        };
        ids.sort_unstable();
        assert_eq!(ids, [3, 4]);
        // Only a parent of a running agent is ended: nothing to remove.
        agents(
            &mut s,
            &fake,
            vec![
                ended(1, "/w", 1, AgentState::Exited),
                child_of(row(2, "/w", 2), 1),
            ],
        );
        let before = fake.calls().len();
        s.cx.dispatch_action(RemoveExited);
        assert!(!s.cx.has_pending_prompt());
        assert_eq!(fake.calls().len(), before);
    }

    #[gpui::test]
    fn children_are_drawn_under_their_parent_and_a_big_family_starts_collapsed(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let mut rows = vec![row(1, "/w", 1)];
        rows.extend((2..=4).map(|i| child_of(row(i, "/w", i64::try_from(i).unwrap()), 1)));
        agents(&mut s, &fake, rows.clone());
        assert_eq!(tabs_drawn(&mut s), [1, 2, 3, 4]);
        rows.push(child_of(row(5, "/w", 5), 1));
        agents(&mut s, &fake, rows);
        assert_eq!(tabs_drawn(&mut s), [1]);
        click(&mut s, "disclose-1");
        assert_eq!(tabs_drawn(&mut s), [1, 2, 3, 4, 5]);
        click(&mut s, "disclose-1");
        assert_eq!(tabs_drawn(&mut s), [1]);
    }

    /// The Search All Agents rows: (heading, title).
    fn search_rows(s: &mut Setup<'_>) -> Vec<(Option<String>, String)> {
        s.ws.read_with(s.cx, |w, cx| {
            let view = w.search_all().expect("search open").read(cx);
            let picker = view.picker().read(cx);
            let titles: Vec<String> = picker
                .picker()
                .items()
                .iter()
                .map(|i| i.title.clone())
                .collect();
            picker.sections().iter().cloned().zip(titles).collect()
        })
    }

    #[gpui::test]
    fn search_all_agents_lists_hits_by_agent_and_finds_the_pick(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(1, "/w", 1), row(2, "/w", 2)]);
        let hit = |agent_id, line, text: &str| fleet_proto::SearchHit {
            agent_id,
            line,
            text: text.into(),
        };
        *fake.search_hits.lock().unwrap() = vec![
            hit(2, 7, "  error: two"),
            hit(1, 3, "error in one"),
            hit(1, 1, "more error"),
        ];
        s.cx.simulate_keystrokes("cmd-shift-f");
        assert!(s.ws.read_with(s.cx, |w, _| w.search_all().is_some()));
        s.cx.simulate_input("err");
        s.cx.run_until_parked();
        assert!(fake.searches.lock().unwrap().is_empty(), "debounced");
        s.cx.simulate_input("or");
        s.cx.executor()
            .advance_clock(crate::core::search_all::DEBOUNCE);
        s.cx.run_until_parked();
        assert_eq!(
            *fake.searches.lock().unwrap(),
            [("error".to_owned(), false)],
            "one search, for the latest query"
        );
        let rows = search_rows(&mut s);
        let titles: Vec<&str> = rows.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(titles, ["error: two", "error in one", "more error"]);
        let headed: Vec<bool> = rows.iter().map(|(h, _)| h.is_some()).collect();
        assert_eq!(headed, [true, true, false], "grouped by agent");

        // The second row: agent 1 in the focused pane, find on line 3.
        s.cx.simulate_keystrokes("down enter");
        s.cx.run_until_parked();
        assert!(s.ws.read_with(s.cx, |w, _| w.search_all().is_none()));
        let pane = focused(&mut s);
        assert_eq!(
            s.app.read_with(s.cx, |a, _| a.layout().agent(pane)),
            Some(1)
        );
        let lines: Vec<bytes::Bytes> = ["error 0", "error 1", "x", "error 3", "error 4"]
            .iter()
            .map(|l| bytes::Bytes::from(l.as_bytes().to_vec()))
            .collect();
        fake.model(pane).lock().unwrap().on_attached(&AttachOk {
            agent_id: 1,
            cols: 80,
            rows: 24,
            primary: true,
            history_tail: lines,
            ..Default::default()
        });
        let view = s
            .app
            .read_with(s.cx, |a, _| a.pane_view(pane).cloned())
            .unwrap();
        // The model changed under the view: a redraw scans it.
        view.update(s.cx, |_, cx| cx.notify());
        s.cx.run_until_parked();
        let label = view.read_with(s.cx, |v, _| v.find_label());
        assert_eq!(label.as_deref(), Some("3 of 4"), "on line 3");
    }

    #[gpui::test]
    fn filter_agents_moved_to_option_command_f(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let rows = palette_rows(&mut s);
        assert!(rows.contains(&("Search All Agents…".to_owned(), Some("⇧⌘F".to_owned()))));
        assert!(rows.contains(&("Filter Agents".to_owned(), Some("⌥⌘F".to_owned()))));
        s.cx.simulate_keystrokes("cmd-shift-f escape");
        assert!(s.ws.read_with(s.cx, |w, _| w.search_all().is_none()));
    }

    /// The palette's rows now, as (title, shortcut); closes it again.
    fn palette_rows(s: &mut Setup<'_>) -> Vec<(String, Option<String>)> {
        s.cx.simulate_keystrokes("cmd-p");
        let rows = s.ws.read_with(s.cx, |w, cx| {
            let palette = w.palette().expect("palette open").read(cx);
            palette
                .picker()
                .items()
                .iter()
                .map(|i| (i.title.clone(), i.shortcut.clone()))
                .collect()
        });
        s.cx.simulate_keystrokes("escape");
        rows
    }

    fn has_row(rows: &[(String, Option<String>)], title: &str) -> bool {
        rows.iter().any(|(t, _)| t == title)
    }

    fn pick_in_palette(s: &mut Setup<'_>, query: &str) {
        s.cx.simulate_keystrokes("cmd-p");
        s.cx.simulate_input(query);
        s.cx.simulate_keystrokes("enter");
        s.cx.run_until_parked();
    }

    /// What acts on the focused pane's agent shows only while it has one,
    /// in the right state.
    #[gpui::test]
    fn agent_commands_follow_the_focused_pane(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(7, "/w", 1)]);
        let rows = palette_rows(&mut s);
        for title in [
            "Kill Agent…",
            "Rename Agent…",
            "Edit Labels…",
            "Find in Scrollback…",
            "Restart Agent in Pane",
            "Remove Exited Agent in Pane",
        ] {
            assert!(!has_row(&rows, title), "{title} with an empty pane");
        }
        assert!(has_row(&rows, "Zoom In") && has_row(&rows, "Settings…"));

        click(&mut s, "tab-7");
        let rows = palette_rows(&mut s);
        assert!(has_row(&rows, "Kill Agent…") && has_row(&rows, "Rename Agent…"));
        assert!(has_row(&rows, "Edit Labels…"));
        assert!(!has_row(&rows, "Restart Agent in Pane"));
        pick_in_palette(&mut s, "kill agent");
        assert_eq!(kill_asked(&mut s), Some(7));
        s.cx.simulate_keystrokes("escape");
    }

    #[gpui::test]
    fn rename_and_remove_from_the_palette(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![named(3, "old-name", &[])]);
        click(&mut s, "tab-3");
        pick_in_palette(&mut s, "rename agent");
        rename_to(&mut s, "from-palette");
        assert!(
            fake.calls()
                .contains(&Call::Rename(3, "from-palette".into()))
        );
        pick_in_palette(&mut s, "edit labels");
        assert!(s.ws.read_with(s.cx, |w, _| w.labels_sheet().is_some()));
        s.cx.simulate_keystrokes("escape");

        agents(&mut s, &fake, vec![ended(3, "/w", 3, AgentState::Exited)]);
        attach_model(&fake, PaneId(1), 3);
        fake.model(PaneId(1)).lock().unwrap().on_exited(3);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        let rows = palette_rows(&mut s);
        assert!(has_row(&rows, "Remove Exited Agent in Pane"));
        assert!(!has_row(&rows, "Kill Agent…"));
        pick_in_palette(&mut s, "remove exited agent in pane");
        assert!(!s.cx.has_pending_prompt());
        assert_eq!(fake.calls().last(), Some(&Call::Forget(vec![3])));
    }

    fn presets_config() -> GuiConfig {
        let preset = |name: &str, command: &[&str], labels: &[(&str, &str)]| Preset {
            name: name.into(),
            command: command.iter().map(|w| (*w).to_owned()).collect(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        };
        GuiConfig {
            presets: Some(vec![
                preset("Claude", &["claude"], &[]),
                preset(
                    "Review",
                    &["claude", "--model", "opus"],
                    &[("role", "reviewer")],
                ),
            ]),
            ..GuiConfig::default()
        }
    }

    /// A preset's button fills the command and labels; the prompt is the
    /// last argument and names the agent; the command becomes recent once
    /// the pane shows the agent.
    #[gpui::test]
    fn sheet_presets_prompt_and_recent_commands(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup_with(cx, &fake, dir.path(), presets_config());
        s.cx.simulate_keystrokes("cmd-n");
        set_sheet_text(&mut s, |sh| sh.dir.clone(), "/w/fleetd");
        click(&mut s, "preset-1");
        assert_eq!(
            sheet_text(&mut s, |sh| sh.command.clone()),
            "claude --model opus"
        );
        assert_eq!(sheet_text(&mut s, |sh| sh.labels.clone()), "role=reviewer");
        // The caret is in Prompt: typing goes there.
        s.cx.simulate_input("fix the SIGTERM race");
        assert_eq!(
            sheet_text(&mut s, |sh| sh.prompt.clone()),
            "fix the SIGTERM race"
        );
        let name = sheet_text(&mut s, |sh| sh.name.clone());
        assert!(name.starts_with("sigterm-"), "{name}");
        s.cx.simulate_keystrokes("enter");
        let req = last_spawn(&fake);
        assert_eq!(req.program, "claude");
        assert_eq!(req.args, ["--model", "opus", "fix the SIGTERM race"]);
        assert_eq!(req.labels["role"], "reviewer");
        assert_eq!(req.labels["name"], name);
        // Not recent until the pane shows the agent.
        let recent = |s: &mut Setup<'_>| s.app.read_with(s.cx, |a, _| a.recent_commands());
        assert!(recent(&mut s).is_empty());
        attach_model(&fake, PaneId(1), 9);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        assert_eq!(recent(&mut s), ["claude --model opus"]);
        // The next sheet offers it.
        s.cx.simulate_keystrokes("cmd-n");
        let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
        let commands = sheet.read_with(s.cx, |sh, _| sh.commands.clone());
        assert_eq!(commands, ["claude --model opus"]);
    }

    /// Duplicate in the row and pane menus and the palette: the sheet with
    /// the agent's command, folder and labels, and a new name.
    #[gpui::test]
    fn duplicate_prefills_the_sheet_from_a_running_agent(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let mut agent = named(3, "build-otter", &[("role", "fixer")]);
        agent.info.program = "claude".into();
        agent.info.args = vec!["fix the build".into()];
        agents(&mut s, &fake, vec![agent]);
        assert!(row_items(&mut s, 3).contains(&RowMenuItem::Duplicate));
        pick_row(&mut s, 3, RowMenuItem::Duplicate);
        assert_eq!(
            sheet_text(&mut s, |sh| sh.command.clone()),
            "claude 'fix the build'"
        );
        assert_eq!(sheet_text(&mut s, |sh| sh.dir.clone()), "/w");
        assert_eq!(sheet_text(&mut s, |sh| sh.labels.clone()), "role=fixer");
        let name = sheet_text(&mut s, |sh| sh.name.clone());
        assert!(!name.is_empty() && name != "build-otter", "{name}");
        s.cx.simulate_keystrokes("escape");
        assert!(s.ws.read_with(s.cx, |w, _| w.sheet().is_none()));

        // From the pane's menu, and the palette, on the pane's agent.
        click(&mut s, "tab-3");
        let epoch = s.ws.read_with(s.cx, |ws, _| ws.epoch);
        s.ws.update_in(s.cx, |ws, window, cx| {
            ws.pane_menu_pick(PaneId(1), PaneMenuItem::Duplicate, epoch, window, cx);
        });
        s.cx.run_until_parked();
        assert_eq!(sheet_text(&mut s, |sh| sh.labels.clone()), "role=fixer");
        s.cx.simulate_keystrokes("escape");
        pick_in_palette(&mut s, "duplicate agent");
        assert_eq!(
            sheet_text(&mut s, |sh| sh.command.clone()),
            "claude 'fix the build'"
        );
    }

    /// Presets in the palette open the sheet prefilled; in the split
    /// chooser they start at once in its folder.
    #[gpui::test]
    fn presets_in_the_palette_and_the_split_chooser(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup_with(cx, &fake, dir.path(), presets_config());
        let rows = palette_rows(&mut s);
        assert!(has_row(&rows, "New Claude…") && has_row(&rows, "New Review…"));
        pick_in_palette(&mut s, "new review");
        assert_eq!(
            sheet_text(&mut s, |sh| sh.command.clone()),
            "claude --model opus"
        );
        s.cx.simulate_keystrokes("escape");

        agents(&mut s, &fake, vec![row(7, "/w/fleetd", 1)]);
        click(&mut s, "tab-7");
        s.cx.simulate_keystrokes("cmd-d");
        let titles = s.ws.read_with(s.cx, |w, cx| {
            let chooser = w.chooser(PaneId(2)).expect("chooser").read(cx);
            chooser
                .chooser()
                .rows()
                .iter()
                .map(|r| r.title.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(titles[3..], ["Claude", "Review"]);
        s.cx.simulate_keystrokes("5");
        s.cx.run_until_parked();
        let req = last_spawn(&fake);
        assert_eq!(req.cwd, "/w/fleetd");
        assert_eq!(req.args, ["--model", "opus"]);
        assert_eq!(req.labels["role"], "reviewer");
        assert!(req.labels.contains_key("name"));
    }

    /// Tab commands show with more than one tab, each with its shortcut.
    #[gpui::test]
    fn tab_commands_show_their_shortcuts(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let rows = palette_rows(&mut s);
        assert!(has_row(&rows, "New Tab"));
        assert!(!has_row(&rows, "Show Next Tab") && !has_row(&rows, "Go to Tab 1"));
        s.cx.simulate_keystrokes("cmd-alt-t");
        let rows = palette_rows(&mut s);
        let shortcut = |title: &str| {
            rows.iter()
                .find(|(t, _)| t == title)
                .unwrap_or_else(|| panic!("{title} listed"))
                .1
                .clone()
        };
        assert_eq!(shortcut("Go to Tab 2").as_deref(), Some("⌥⌘2"));
        assert_eq!(shortcut("Show Next Tab").as_deref(), Some("⇧⌘]"));
        assert_eq!(shortcut("Close Tab").as_deref(), Some("⌥⌘W"));
        assert!(!has_row(&rows, "Go to Tab 3"));
        pick_in_palette(&mut s, "go to tab 1");
        assert_eq!(s.app.read_with(s.cx, |a, _| a.tabs().active()), 0);
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

    /// The tabs the last frame drew.
    fn tabs_drawn(s: &mut Setup<'_>) -> Vec<u64> {
        s.cx.run_until_parked();
        s.ws.read_with(s.cx, |w, _| {
            w.drawn_sidebar
                .iter()
                .flat_map(|(_, ids)| ids.clone())
                .collect()
        })
    }

    fn groups_drawn(s: &mut Setup<'_>) -> Vec<String> {
        s.cx.run_until_parked();
        s.ws.read_with(s.cx, |w, _| {
            w.drawn_sidebar.iter().map(|(t, _)| t.clone()).collect()
        })
    }

    fn project(mut r: AgentRow, p: &str) -> AgentRow {
        r.info.labels.insert("project".into(), p.into());
        r
    }

    fn filter_text(s: &mut Setup<'_>) -> String {
        s.ws.read_with(s.cx, |w, cx| w.filter_field.read(cx).text().to_owned())
    }

    fn filter_focused(s: &mut Setup<'_>) -> bool {
        s.ws.update_in(s.cx, |w, window, cx| {
            w.filter_field.read(cx).focus_handle().is_focused(window)
        })
    }

    fn pane_focused(s: &mut Setup<'_>) -> bool {
        let pane = focused(s);
        s.ws.update_in(s.cx, |w, window, cx| {
            w.pane_focus_handle(pane, cx)
                .is_some_and(|h| h.is_focused(window))
        })
    }

    #[gpui::test]
    fn cmd_shift_f_filters_by_name_cwd_and_labels_and_escape_clears(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let mut coder = project(row(2, "/srv/beta", 2), "beta");
        coder.info.labels.insert("role".into(), "coder".into());
        agents(
            &mut s,
            &fake,
            vec![project(row(1, "/srv/alpha", 1), "alpha"), coder],
        );
        assert_eq!(tabs_drawn(&mut s), [1, 2]);

        s.cx.simulate_keystrokes("cmd-alt-f");
        assert!(filter_focused(&mut s));
        s.cx.simulate_input("coder");
        assert_eq!(tabs_drawn(&mut s), [2]);
        assert!(drawn(&mut s, "tab-2"));
        assert_eq!(groups_drawn(&mut s), ["beta"]);

        s.ws.update(s.cx, |w, cx| {
            w.filter_field
                .update(cx, |f, cx| f.set_text("srv/alpha".into(), cx))
        });
        assert_eq!(tabs_drawn(&mut s), [1]);

        s.ws.update(s.cx, |w, cx| {
            w.filter_field
                .update(cx, |f, cx| f.set_text("nothing".into(), cx))
        });
        assert!(tabs_drawn(&mut s).is_empty());
        assert!(drawn(&mut s, "sidebar-no-match"));

        // Escape clears the text, then a second one gives the pane the keys.
        s.cx.simulate_keystrokes("escape");
        assert_eq!(filter_text(&mut s), "");
        assert_eq!(tabs_drawn(&mut s), [1, 2]);
        assert!(filter_focused(&mut s));
        s.cx.simulate_keystrokes("escape");
        assert!(pane_focused(&mut s));
    }

    #[gpui::test]
    fn enter_in_the_filter_shows_the_first_match(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(1, "/a", 1), row(2, "/b", 2)]);
        s.cx.simulate_keystrokes("cmd-alt-f");
        s.cx.simulate_input("/b");
        s.cx.simulate_keystrokes("enter");
        s.cx.run_until_parked();
        assert_eq!(fake.calls().last(), Some(&Call::Attach(PaneId(1), 2)));
        assert!(pane_focused(&mut s));
    }

    #[gpui::test]
    fn typing_in_the_sidebar_goes_to_the_filter(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(1, "/a", 1), row(2, "/b", 2)]);
        // A click on the sidebar's empty space below the tabs.
        let tabs = s.cx.debug_bounds("sidebar").expect("sidebar drawn");
        let at = point(tabs.left() + gpui::px(20.), tabs.bottom() - gpui::px(10.));
        s.cx.simulate_click(at, Modifiers::none());
        s.cx.run_until_parked();
        assert!(!pane_focused(&mut s));
        s.cx.simulate_keystrokes("b");
        assert!(filter_focused(&mut s));
        assert_eq!(filter_text(&mut s), "b");
        assert_eq!(tabs_drawn(&mut s), [2]);
    }

    #[gpui::test]
    fn status_filters_narrow_the_sidebar(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let mut quiet = row(3, "/c", 3);
        quiet.last_line = "Continue? (y/n)".into();
        quiet.last_output_unix_ms = Some(1);
        agents(
            &mut s,
            &fake,
            vec![
                row(1, "/a", 1),
                ended(2, "/b", 2, AgentState::Exited),
                quiet,
            ],
        );
        let tabs = |s: &mut Setup<'_>| {
            let mut t = tabs_drawn(s);
            t.sort_unstable();
            t
        };
        assert_eq!(tabs(&mut s), [1, 2, 3]);
        click(&mut s, "status-filter-Exited");
        assert_eq!(tabs(&mut s), [2]);
        click(&mut s, "status-filter-Running");
        assert_eq!(tabs(&mut s), [1, 3]);
        click(&mut s, "status-filter-Needs you");
        assert_eq!(tabs(&mut s), [3]);
        click(&mut s, "status-filter-All");
        assert_eq!(tabs(&mut s), [1, 2, 3]);
    }

    #[gpui::test]
    fn a_group_collapses_with_a_count_and_is_remembered(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(
            &mut s,
            &fake,
            vec![
                project(row(1, "/a", 1), "alpha"),
                project(row(2, "/a", 2), "alpha"),
                project(row(3, "/b", 3), "beta"),
            ],
        );
        assert!(!drawn(&mut s, "group-count-alpha"));
        click(&mut s, "group-alpha");
        assert_eq!(tabs_drawn(&mut s), [3]);
        assert_eq!(groups_drawn(&mut s), ["alpha", "beta"]);
        assert!(drawn(&mut s, "group-count-alpha"));
        let count = s.cx.debug_bounds("group-count-alpha-2");
        assert!(count.is_some(), "the count says 2");

        s.cx.executor().advance_clock(SAVE_DEBOUNCE);
        s.cx.run_until_parked();
        let saved = GuiState::load(dir.path()).collapsed_groups;
        assert_eq!(saved["local"].iter().collect::<Vec<_>>(), ["alpha"]);

        // Typing shows what matched inside it, without forgetting it.
        s.cx.simulate_keystrokes("cmd-alt-f");
        s.cx.simulate_input("/a");
        assert_eq!(tabs_drawn(&mut s), [2, 1]);
        s.cx.simulate_keystrokes("escape");
        assert_eq!(tabs_drawn(&mut s), [3]);

        click(&mut s, "group-alpha");
        assert_eq!(tabs_drawn(&mut s), [2, 1, 3]);
        assert!(
            s.app
                .read_with(s.cx, |a, _| a.collapsed_groups().is_empty())
        );
    }

    fn in_repo(mut r: AgentRow, repo: &str, worktree: &str, branch: &str) -> AgentRow {
        r.info.git = Some(fleet_proto::GitInfo {
            repo: repo.into(),
            worktree: worktree.into(),
            branch: branch.into(),
            detached: false,
        });
        r
    }

    /// ADR 0085: the row and the pane header show the git checkout; the
    /// sidebar groups by repository on request, remembered; the filter
    /// matches the branch.
    #[gpui::test]
    fn rows_show_the_branch_and_group_by_repository(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(
            &mut s,
            &fake,
            vec![
                in_repo(row(1, "/src/fleetd", 1), "fleetd", "", "main"),
                in_repo(row(2, "/wt/issue-97", 2), "fleetd", "issue-97", "feat/git"),
                row(3, "/notes", 3),
            ],
        );
        assert!(drawn(&mut s, "tab-1-git-fleetd · main"));
        assert!(drawn(&mut s, "tab-2-git-issue-97 · feat/git"));
        // Not in a checkout: program and id, as before.
        assert!(drawn(&mut s, "tab-3-detail"));
        assert_eq!(
            groups_drawn(&mut s),
            ["/notes", "/src/fleetd", "/wt/issue-97"]
        );

        s.app.update(s.cx, |a, cx| a.show_agent(2, cx));
        assert!(drawn(&mut s, "pane-1-git-fleetd (issue-97) · feat/git"));

        s.ws.update(s.cx, |ws, cx| {
            ws.set_group_by(crate::core::sidebar::GroupBy::Repo, cx)
        });
        assert_eq!(groups_drawn(&mut s), ["/notes", "fleetd"]);
        assert_eq!(tabs_drawn(&mut s), [3, 2, 1]);
        s.cx.executor().advance_clock(SAVE_DEBOUNCE);
        s.cx.run_until_parked();
        assert_eq!(
            GuiState::load(dir.path()).sidebar_group_by,
            crate::core::sidebar::GroupBy::Repo
        );

        s.cx.simulate_keystrokes("cmd-alt-f");
        s.cx.simulate_input("feat/git");
        assert_eq!(tabs_drawn(&mut s), [2]);
    }

    fn exited(id: u64, code: i32) -> AgentRow {
        let mut r = ended(id, "/w", id as i64, AgentState::Exited);
        r.info.exit_code = Some(code);
        r
    }

    fn exit_ids(s: &mut Setup<'_>) -> Vec<u64> {
        s.app
            .read_with(s.cx, |a, _| a.exit_toasts().map(|n| n.agent_id).collect())
    }

    #[gpui::test]
    fn an_exit_shows_a_toast_with_its_code_and_restart(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        // First seen already exited: not news.
        agents(&mut s, &fake, vec![exited(6, 1), row(7, "/w", 1)]);
        assert!(exit_ids(&mut s).is_empty());
        agents(&mut s, &fake, vec![exited(6, 1), exited(7, 3)]);
        assert_eq!(exit_ids(&mut s), vec![7]);
        let text = s
            .app
            .read_with(s.cx, |a, _| a.exit_toasts().next().unwrap().text());
        assert_eq!(text, "sh exited with code 3");
        assert!(s.cx.debug_bounds("exit-toast-7").is_some());
        assert!(
            s.cx.debug_bounds("exit-toast-7-celebration").is_none(),
            "no celebration for a failure, nor while it is off"
        );
        click(&mut s, "exit-toast-7-restart");
        assert!(fake.calls().contains(&Call::Restart(7)));
        assert!(exit_ids(&mut s).is_empty(), "Restart takes the toast away");
    }

    #[gpui::test]
    fn exit_toasts_close_go_on_their_own_and_follow_the_list(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(
            &mut s,
            &fake,
            vec![row(1, "/w", 1), row(2, "/w", 2), row(3, "/w", 3)],
        );
        agents(
            &mut s,
            &fake,
            vec![exited(1, 0), exited(2, 0), exited(3, 0)],
        );
        assert_eq!(exit_ids(&mut s), vec![1, 2, 3]);
        // Let the toasts finish sliding in: a click at their bounds
        // mid-animation can miss the close button.
        s.cx.executor().advance_clock(Duration::from_millis(
            crate::core::style::motion::TOAST_MS + 50,
        ));
        s.cx.run_until_parked();
        click(&mut s, "exit-toast-1-close");
        assert_eq!(exit_ids(&mut s), vec![2, 3]);
        // Agent 2 restarted elsewhere: it left the list, its toast goes.
        agents(&mut s, &fake, vec![exited(3, 0)]);
        assert_eq!(exit_ids(&mut s), vec![3]);
        s.cx.executor()
            .advance_clock(feedback::EXIT_TOAST_TTL + Duration::from_millis(10));
        s.cx.run_until_parked();
        assert!(exit_ids(&mut s).is_empty(), "the last one timed out");
    }

    #[gpui::test]
    fn celebration_needs_the_setting_and_code_zero(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let config = GuiConfig {
            celebrate: true,
            ..GuiConfig::default()
        };
        let mut s = setup_with(cx, &fake, dir.path(), config);
        agents(&mut s, &fake, vec![row(7, "/w", 1), row(8, "/w", 2)]);
        agents(&mut s, &fake, vec![exited(7, 0), exited(8, 2)]);
        s.cx.run_until_parked();
        assert!(s.cx.debug_bounds("exit-toast-7-celebration").is_some());
        assert!(s.cx.debug_bounds("exit-toast-8-celebration").is_none());
    }

    #[gpui::test]
    fn panes_report_focus_moving_between_them_and_window_activation(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        s.cx.dispatch_action(SplitRight);
        s.cx.run_until_parked();
        assert_eq!(focused(&mut s), PaneId(2));
        // Both programs ask for focus reports (DECSET 1004).
        for pane in [1, 2] {
            let model = fake.model(PaneId(pane));
            model.lock().unwrap().on_output(b"\x1b[?1004h");
        }
        let take = |fake: &Fake| {
            let mut sent = std::mem::take(&mut *fake.pane_inputs.lock().unwrap());
            sent.sort_by_key(|(pane, _)| pane.0);
            sent
        };
        take(&fake);
        let (focus_in, focus_out) = (b"\x1b[I".to_vec(), b"\x1b[O".to_vec());

        s.cx.dispatch_action(FocusPane1);
        s.cx.run_until_parked();
        assert_eq!(
            take(&fake),
            vec![
                (PaneId(1), focus_in.clone()),
                (PaneId(2), focus_out.clone())
            ]
        );

        // Only the focused pane hears of the window's activation.
        s.cx.deactivate_window();
        assert_eq!(take(&fake), vec![(PaneId(1), focus_out.clone())]);
        s.cx.update(|window, _| window.activate_window());
        s.cx.run_until_parked();
        assert_eq!(take(&fake), vec![(PaneId(1), focus_in.clone())]);

        // A pane whose program did not ask hears nothing.
        let model = fake.model(PaneId(1));
        model.lock().unwrap().on_output(b"\x1b[?1004l");
        s.cx.dispatch_action(FocusPane2);
        s.cx.run_until_parked();
        assert_eq!(take(&fake), vec![(PaneId(2), focus_in)]);
    }

    #[gpui::test]
    fn focus_mode_dims_the_panes_without_focus(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        // Reduced motion: the dim is there at once, no cross-fade to wait for.
        let config = GuiConfig {
            reduce_motion: MotionChoice::On,
            ..GuiConfig::default()
        };
        let mut s = setup_with(cx, &fake, dir.path(), config);
        s.cx.dispatch_action(SplitRight);
        s.cx.run_until_parked();
        assert!(s.cx.debug_bounds("pane-1-dim").is_none(), "off by default");
        s.app.update(s.cx, |a, cx| {
            a.change_settings(vec![Change::FocusMode(true)], cx)
                .unwrap();
        });
        s.cx.run_until_parked();
        assert_eq!(focused(&mut s), PaneId(2));
        assert!(s.cx.debug_bounds("pane-1-dim").is_some(), "applies live");
        assert!(
            s.cx.debug_bounds("pane-2-dim").is_none(),
            "the focused one is clear"
        );
        s.cx.dispatch_action(FocusPane1);
        s.cx.run_until_parked();
        assert!(s.cx.debug_bounds("pane-2-dim").is_some());
        // (gpui keeps the bounds of elements once drawn: the chrome says
        // which pane is dimmed from here on.)
        let chrome = |s: &mut Setup<'_>, pane: u32| {
            s.app.read_with(s.cx, |a, cx| {
                let c = &a.pane_view(PaneId(pane)).unwrap().read(cx).chrome;
                (c.dims, c.focused)
            })
        };
        assert_eq!(chrome(&mut s, 1), (true, true), "focused: clear");
        assert_eq!(chrome(&mut s, 2), (true, false), "dimmed");
    }

    #[gpui::test]
    fn no_pane_eases_in_at_launch_or_under_reduce_motion(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let config = GuiConfig {
            reduce_motion: MotionChoice::On,
            ..GuiConfig::default()
        };
        let mut s = setup_with(cx, &fake, dir.path(), config);
        assert!(s.ws.read_with(s.cx, |w, _| w.easing_panes()).is_empty());
        s.cx.dispatch_action(SplitRight);
        s.cx.run_until_parked();
        assert_eq!(panes(&mut s).len(), 2);
        assert!(s.ws.read_with(s.cx, |w, _| w.easing_panes()).is_empty());
        assert!(s.cx.update(|_, cx| crate::app::controls::reduces_motion(cx)));
    }

    // Issue #46: reopen closed pane, shortcut cheat sheet, first-run welcome.

    // gpui keeps the debug bounds of earlier frames: what went away is
    // checked on the state, not with `drawn`.
    fn shortcuts_shown(s: &mut Setup<'_>) -> bool {
        s.cx.run_until_parked();
        s.ws.read_with(s.cx, |ws, _| ws.shortcuts_shown())
    }

    fn welcomes(s: &mut Setup<'_>) -> bool {
        s.cx.run_until_parked();
        s.app.read_with(s.cx, |a, cx| {
            a.layout().panes().iter().any(|p| {
                let chrome = &a.pane_view(*p).unwrap().read(cx).chrome;
                crate::app::pane_view::empty_state(chrome) == EmptyPane::Welcome
            })
        })
    }

    fn listed(s: &mut Setup<'_>, fake: &Fake, rows: Vec<AgentRow>) {
        fake.listed.store(true, std::sync::atomic::Ordering::SeqCst);
        agents(s, fake, rows);
    }

    #[gpui::test]
    fn cmd_shift_t_reopens_the_last_closed_pane_with_its_agent(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        listed(&mut s, &fake, vec![row(1, "/a", 1), row(2, "/b", 2)]);
        click(&mut s, "tab-1");
        s.cx.simulate_keystrokes("cmd-d");
        click(&mut s, "tab-2");
        s.cx.simulate_keystrokes("cmd-w");
        assert_eq!(panes(&mut s), [PaneId(1)]);
        s.cx.simulate_keystrokes("cmd-shift-t");
        assert_eq!(panes(&mut s).len(), 2);
        let new = focused(&mut s);
        assert_eq!(shown(&mut s, new), Some(2));
        // Nothing more to reopen.
        s.cx.simulate_keystrokes("cmd-shift-t");
        assert_eq!(panes(&mut s).len(), 2);
        // Closing the last pane leaves it empty; ⌘⇧T fills it again.
        s.cx.simulate_keystrokes("cmd-w cmd-w");
        assert_eq!(panes(&mut s).len(), 1);
        let only = focused(&mut s);
        assert_eq!(shown(&mut s, only), None);
        s.cx.simulate_keystrokes("cmd-shift-t");
        assert_eq!(panes(&mut s).len(), 1);
        assert_eq!(shown(&mut s, only), Some(1));
    }

    #[gpui::test]
    fn cmd_shift_t_skips_agents_that_are_gone(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        listed(&mut s, &fake, vec![row(1, "/a", 1), row(2, "/b", 2)]);
        click(&mut s, "tab-1");
        s.cx.simulate_keystrokes("cmd-d");
        click(&mut s, "tab-2");
        s.cx.simulate_keystrokes("cmd-w");
        // Agent 2 was removed meanwhile.
        listed(&mut s, &fake, vec![row(1, "/a", 1)]);
        s.cx.simulate_keystrokes("cmd-shift-t");
        assert_eq!(panes(&mut s), [PaneId(1)]);
    }

    #[gpui::test]
    fn cmd_slash_shows_every_key_binding_and_escape_closes_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        assert!(!shortcuts_shown(&mut s));
        s.cx.simulate_keystrokes("cmd-/");
        assert!(drawn(&mut s, "shortcuts-sheet"));
        // Generated from the keymap: every binding's keys are on it.
        let (keys, sheet) = s.cx.update(|_, cx| {
            let keys: Vec<String> = cx
                .key_bindings()
                .borrow()
                .bindings()
                .map(|b| {
                    let strokes: Vec<String> = b.keystrokes().iter().map(|k| k.unparse()).collect();
                    key_glyphs(&strokes.join(" "))
                })
                .collect();
            (keys, super::super::keymap::cheat_sheet(cx))
        });
        assert!(keys.len() > 20, "{keys:?}");
        for k in &keys {
            assert!(
                sheet
                    .iter()
                    .any(|sec| sec.rows.iter().any(|r| &r.keys == k)),
                "{k} is missing from {sheet:?}"
            );
        }
        let window = &sheet[0];
        assert!(
            window
                .rows
                .iter()
                .any(|r| r.keys == "⇧⌘T" && r.title == "Reopen Closed Pane")
        );
        assert!(
            window
                .rows
                .iter()
                .any(|r| r.keys == "⌘/" && r.title == "Show Shortcuts")
        );
        // Panes ignore the keys meanwhile.
        s.cx.simulate_keystrokes("cmd-d");
        assert_eq!(panes(&mut s).len(), 1);
        s.cx.simulate_keystrokes("escape");
        assert!(!shortcuts_shown(&mut s));
        s.cx.simulate_keystrokes("cmd-/");
        assert!(shortcuts_shown(&mut s));
        s.cx.simulate_keystrokes("cmd-/");
        assert!(!shortcuts_shown(&mut s));
        // The keyboard is back in the pane.
        s.cx.simulate_keystrokes("cmd-d");
        assert_eq!(panes(&mut s).len(), 2);
    }

    #[gpui::test]
    fn first_run_welcome_offers_new_agent_terminal_and_shortcuts(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        // Before the list arrives: no welcome yet.
        assert!(!welcomes(&mut s));
        listed(&mut s, &fake, vec![]);
        assert!(welcomes(&mut s));
        assert!(drawn(&mut s, "welcome"));
        assert!(drawn(&mut s, "welcome-install-skill"));
        click(&mut s, "welcome-shortcuts");
        assert!(shortcuts_shown(&mut s));
        s.cx.simulate_keystrokes("escape");
        assert!(!shortcuts_shown(&mut s));
        click(&mut s, "welcome-new-agent");
        assert!(s.ws.read_with(s.cx, |ws, _| ws.sheet().is_some()));
        s.cx.simulate_keystrokes("escape");
        assert!(s.ws.read_with(s.cx, |ws, _| ws.sheet().is_none()));
        click(&mut s, "welcome-terminal");
        assert!(
            fake.calls().iter().any(|c| matches!(c, Call::Spawn { .. })),
            "{:?}",
            fake.calls()
        );
        // With an agent, an empty pane shows the hint only.
        listed(&mut s, &fake, vec![row(1, "/a", 1)]);
        assert!(!welcomes(&mut s));
    }

    /// The graph view (⌘⇧G, ADR 0073).
    mod graph {
        use super::*;
        use crate::app::graph_view::GraphView;
        use fleet_client::tree::PARENT_LABEL;

        fn child(id: u64, parent: u64) -> AgentRow {
            let mut r = row(id, "/w", id as i64);
            r.info
                .labels
                .insert(PARENT_LABEL.to_owned(), parent.to_string());
            r
        }

        fn ended_child(id: u64, parent: u64) -> AgentRow {
            let mut r = child(id, parent);
            r.info.set_state(AgentState::Exited);
            r.info.exit_code = Some(0);
            r
        }

        fn graph(s: &mut Setup<'_>) -> Entity<GraphView> {
            s.ws.read_with(s.cx, |w, _| w.graph().clone())
        }

        fn shown(s: &mut Setup<'_>) -> bool {
            s.ws.read_with(s.cx, |w, _| w.graph_shown())
        }

        fn selected(s: &mut Setup<'_>) -> Option<u64> {
            let g = graph(s);
            g.read_with(s.cx, |g, _| g.selected())
        }

        /// The last frame built `id`'s card (gpui's debug selectors outlive
        /// the frame that drew them).
        fn drawn(s: &mut Setup<'_>, id: u64) -> bool {
            s.cx.run_until_parked();
            let g = graph(s);
            s.ws.read_with(s.cx, |w, _| w.graph_shown())
                && g.read_with(s.cx, |g, _| g.built.contains(&id))
        }

        fn forgets(fake: &Fake) -> Vec<Vec<u64>> {
            fake.calls()
                .into_iter()
                .filter_map(|c| match c {
                    Call::Forget(ids) => Some(ids),
                    _ => None,
                })
                .collect()
        }

        fn open_graph<'a>(
            cx: &'a mut TestAppContext,
            fake: &Arc<Fake>,
            dir: &Path,
            rows: Vec<AgentRow>,
        ) -> Setup<'a> {
            let mut s = setup(cx, fake, dir);
            agents(&mut s, fake, rows);
            s.cx.simulate_keystrokes("cmd-shift-g");
            s.cx.run_until_parked();
            assert!(shown(&mut s));
            s
        }

        #[gpui::test]
        fn cmd_shift_g_switches_to_the_graph_and_esc_back(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let fake = Arc::new(Fake::default());
            let mut s = setup(cx, &fake, dir.path());
            agents(&mut s, &fake, vec![row(1, "/w", 1), child(2, 1)]);
            assert!(s.cx.debug_bounds("tab-1").is_some());
            // From a focused terminal (no find bar open).
            s.cx.simulate_keystrokes("cmd-shift-g");
            assert!(shown(&mut s));
            assert!(drawn(&mut s, 1) && drawn(&mut s, 2));
            assert!(s.cx.debug_bounds("tab-1").is_none(), "no sidebar");
            s.cx.simulate_keystrokes("escape");
            s.cx.run_until_parked();
            assert!(!shown(&mut s));
            assert!(s.cx.debug_bounds("tab-1").is_some());
            assert!(!drawn(&mut s, 1));
            // The titlebar button toggles it too.
            click(&mut s, "graph-toggle");
            assert!(shown(&mut s));
            click(&mut s, "graph-toggle");
            assert!(!shown(&mut s));
        }

        #[gpui::test]
        fn hovering_a_message_line_explains_it_and_a_click_selects_the_sender(
            cx: &mut TestAppContext,
        ) {
            use crate::core::message_lines::{Edge, Kind, route_all};
            let dir = tempfile::tempdir().unwrap();
            let fake = Arc::new(Fake::default());
            let mut s = open_graph(
                cx,
                &fake,
                dir.path(),
                vec![row(1, "/w", 1), child(2, 1), child(3, 1)],
            );
            let now = now_ms();
            s.app.update(s.cx, |a, cx| {
                a.message_edges = vec![Edge {
                    id: 1,
                    from: 3,
                    to: 1,
                    kind: Kind::Message,
                    at_ms: now - 2 * 60_000,
                }];
                cx.notify();
            });
            s.cx.run_until_parked();
            let g = graph(&mut s);
            let (route, vp) = g.read_with(s.cx, |g, _| {
                (route_all(g.layout(), &[(3, 1)])[0].clone(), g.viewport())
            });
            // The middle of the line's run along the column gap, on screen.
            let (a, b) = (route.points[1], route.points[2]);
            let (x, y) = vp.to_screen(((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0));
            let area = s.cx.debug_bounds("graph-area").unwrap().origin;
            let at = point(area.x + gpui::px(x + 2.0), area.y + gpui::px(y));
            s.cx.simulate_mouse_move(at, None, Modifiers::none());
            s.cx.run_until_parked();
            assert_eq!(g.read_with(s.cx, |g, _| g.hovered_line()), Some((3, 1)));
            assert!(s.cx.debug_bounds("graph-line-tip-3-1").is_some());
            // Over a card: the card, not the line.
            let card = s.cx.debug_bounds("graph-card-2").unwrap().center();
            s.cx.simulate_mouse_move(card, None, Modifiers::none());
            s.cx.run_until_parked();
            assert_eq!(g.read_with(s.cx, |g, _| g.hovered_line()), None);
            // Back on the line, a click selects the sender and stays in the
            // graph.
            s.cx.simulate_mouse_move(at, None, Modifiers::none());
            s.cx.simulate_click(at, Modifiers::none());
            s.cx.run_until_parked();
            assert_eq!(selected(&mut s), Some(3));
            assert!(shown(&mut s));
        }

        #[gpui::test]
        fn enter_opens_the_selected_agent_in_the_focused_pane(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let fake = Arc::new(Fake::default());
            let mut s = open_graph(
                cx,
                &fake,
                dir.path(),
                vec![row(1, "/w", 1), child(2, 1), child(3, 1)],
            );
            assert_eq!(selected(&mut s), Some(1));
            s.cx.simulate_keystrokes("right");
            assert_eq!(selected(&mut s), Some(2));
            s.cx.simulate_keystrokes("down");
            assert_eq!(selected(&mut s), Some(3));
            s.cx.simulate_keystrokes("left");
            assert_eq!(selected(&mut s), Some(1));
            // ⏎ opens the terminal panel; ⌘⏎ opens in the main view
            // (ADR 0105).
            s.cx.simulate_keystrokes("right down cmd-enter");
            s.cx.run_until_parked();
            assert!(!shown(&mut s));
            let pane = focused(&mut s);
            let agent = s.app.read_with(s.cx, |a, _| a.layout().agent(pane));
            assert_eq!(agent, Some(3));
            assert!(fake.calls().contains(&Call::Attach(pane, 3)));
        }

        #[gpui::test]
        fn opening_an_agent_already_in_a_pane_focuses_that_pane(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let fake = Arc::new(Fake::default());
            let mut s = setup(cx, &fake, dir.path());
            agents(&mut s, &fake, vec![row(1, "/w", 1), child(2, 1)]);
            let first = focused(&mut s);
            s.ws.update_in(s.cx, |w, window, cx| w.show_agent(1, window, cx));
            s.cx.simulate_keystrokes("cmd-d");
            let second = focused(&mut s);
            assert_ne!(first, second);
            let before = fake.calls().len();
            s.cx.simulate_keystrokes("cmd-shift-g");
            double_click(&mut s, "graph-card-1");
            assert!(!shown(&mut s));
            assert_eq!(focused(&mut s), first);
            let agent = s.app.read_with(s.cx, |a, _| a.layout().agent(second));
            assert_eq!(agent, None, "the other pane is not replaced");
            assert!(
                !fake.calls()[before..]
                    .iter()
                    .any(|c| matches!(c, Call::Attach(p, _) if *p == second)),
                "nothing attached in the other pane"
            );
        }

        #[gpui::test]
        fn backspace_stops_the_subtree_deepest_first_after_asking(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let fake = Arc::new(Fake::default());
            let rows = vec![
                row(1, "/w", 1),
                child(2, 1),
                child(3, 2),
                ended_child(4, 1),
                row(9, "/w", 9),
            ];
            let mut s = open_graph(cx, &fake, dir.path(), rows);
            s.cx.simulate_keystrokes("backspace");
            assert!(s.cx.has_pending_prompt());
            s.cx.simulate_prompt_answer("Cancel");
            s.cx.run_until_parked();
            assert!(kills(&fake).is_empty());
            s.cx.simulate_keystrokes("backspace");
            s.cx.simulate_prompt_answer("Stop");
            s.cx.run_until_parked();
            assert_eq!(kills(&fake), [3, 2, 1]);
            assert!(shown(&mut s), "the graph stays");
        }

        #[gpui::test]
        fn shift_backspace_removes_only_an_ended_subtree(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let fake = Arc::new(Fake::default());
            let mut five = ended(5, "/w", 5, AgentState::Exited);
            five.info.exit_code = Some(0);
            let rows = vec![row(1, "/w", 1), ended_child(2, 1), five, ended_child(6, 5)];
            let mut s = open_graph(cx, &fake, dir.path(), rows);
            // 1 runs: nothing.
            s.cx.simulate_keystrokes("shift-backspace");
            assert!(!s.cx.has_pending_prompt());
            assert!(forgets(&fake).is_empty());
            // 2 alone: removed without a question.
            s.cx.simulate_keystrokes("right shift-backspace");
            s.cx.run_until_parked();
            assert!(!s.cx.has_pending_prompt());
            assert_eq!(forgets(&fake), [vec![2]]);
            // 5 and 6: asked first, then one ForgetAgents.
            s.cx.simulate_keystrokes("left down shift-backspace");
            assert_eq!(selected(&mut s), Some(5));
            assert!(s.cx.has_pending_prompt());
            s.cx.simulate_prompt_answer("Remove");
            s.cx.run_until_parked();
            assert_eq!(forgets(&fake), [vec![2], vec![5, 6]]);
            assert!(kills(&fake).is_empty());
        }

        #[gpui::test]
        fn c_collapses_a_subtree(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let fake = Arc::new(Fake::default());
            let mut s = open_graph(
                cx,
                &fake,
                dir.path(),
                vec![row(1, "/w", 1), child(2, 1), child(3, 1)],
            );
            assert!(drawn(&mut s, 2));
            s.cx.simulate_keystrokes("c");
            assert!(!drawn(&mut s, 2) && !drawn(&mut s, 3) && drawn(&mut s, 1));
            let g = graph(&mut s);
            let card = g
                .read_with(s.cx, |g, _| g.layout().card(1).cloned())
                .unwrap();
            assert!(card.collapsed && card.children == 2);
            s.cx.simulate_keystrokes("c");
            assert!(drawn(&mut s, 2));
        }

        #[gpui::test]
        fn activity_redraws_without_laying_out_again(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let fake = Arc::new(Fake::default());
            let rows = vec![row(1, "/w", 1), child(2, 1)];
            let mut s = open_graph(cx, &fake, dir.path(), rows.clone());
            let g = graph(&mut s);
            let layouts = g.read_with(s.cx, |g, _| g.layouts);
            let mut busy = rows.clone();
            busy[1].last_line = "compiling…".into();
            busy[1].last_output_unix_ms = Some(now_ms());
            agents(&mut s, &fake, busy);
            assert_eq!(g.read_with(s.cx, |g, _| g.layouts), layouts);
            let mut more = rows;
            more.push(child(3, 2));
            agents(&mut s, &fake, more);
            assert_eq!(g.read_with(s.cx, |g, _| g.layouts), layouts + 1);
            assert!(drawn(&mut s, 3));
        }

        #[gpui::test]
        fn slash_filters_and_keeps_ancestors(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let fake = Arc::new(Fake::default());
            let mut reviewer = child(3, 2);
            reviewer
                .info
                .labels
                .insert("role".into(), "reviewer".into());
            let rows = vec![row(1, "/w", 1), child(2, 1), reviewer, row(4, "/w", 4)];
            let mut s = open_graph(cx, &fake, dir.path(), rows);
            s.cx.simulate_keystrokes("/");
            s.cx.run_until_parked();
            s.cx.simulate_input("role=reviewer");
            s.cx.run_until_parked();
            assert!(drawn(&mut s, 1) && drawn(&mut s, 2) && drawn(&mut s, 3));
            assert!(!drawn(&mut s, 4));
            // Esc clears the filter, a second gives the keys back.
            s.cx.simulate_keystrokes("escape");
            assert!(drawn(&mut s, 4));
            s.cx.simulate_keystrokes("escape escape");
            s.cx.run_until_parked();
            assert!(!shown(&mut s));
        }

        #[gpui::test]
        fn zoom_keys_scale_the_graph_not_the_terminal_font(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let fake = Arc::new(Fake::default());
            let mut s = open_graph(cx, &fake, dir.path(), vec![row(1, "/w", 1)]);
            let font = s.app.read_with(s.cx, |a, _| a.shown_font_size());
            s.cx.simulate_keystrokes("cmd-=");
            let g = graph(&mut s);
            let zoom = g.read_with(s.cx, |g, _| g.viewport().zoom);
            assert!((zoom - 1.2).abs() < 1e-4, "{zoom}");
            assert_eq!(s.app.read_with(s.cx, |a, _| a.shown_font_size()), font);
            s.cx.simulate_keystrokes("cmd-0");
            assert_eq!(g.read_with(s.cx, |g, _| g.viewport().zoom), 1.0);
        }

        #[gpui::test]
        fn only_cards_in_view_are_built(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let fake = Arc::new(Fake::default());
            let rows: Vec<AgentRow> = (1..=80).map(|i| row(i, "/w", i as i64)).collect();
            let mut s = open_graph(cx, &fake, dir.path(), rows);
            assert!(drawn(&mut s, 1));
            assert!(!drawn(&mut s, 80));
            let g = graph(&mut s);
            let built = g.read_with(s.cx, |g, _| g.built.len());
            assert!(built > 0 && built < 40, "{built}");
            // Arrows down to the last card scroll it into view.
            for _ in 0..79 {
                s.cx.simulate_keystrokes("down");
            }
            assert_eq!(selected(&mut s), Some(80));
            assert!(drawn(&mut s, 80) && !drawn(&mut s, 1));
        }
    }

    /// Agent 5 in pane 1, running, with `text` on screen; `read_output`
    /// gives its history.
    fn output_setup<'a>(cx: &'a mut TestAppContext, fake: &Arc<Fake>, dir: &Path) -> Setup<'a> {
        let mut s = setup(cx, fake, dir);
        let mut named = row(5, "/w", 1);
        named.info.labels.insert("name".into(), "otter".into());
        agents(&mut s, fake, vec![named]);
        click(&mut s, "tab-5");
        attach_model(fake, PaneId(1), 5);
        fake.model(PaneId(1))
            .lock()
            .unwrap()
            .on_output(b"\x1b[31merror\x1b[0m: boom   \r\n");
        fake.outputs
            .lock()
            .unwrap()
            .insert(5, Ok("old line\nerror: boom\n".into()));
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
        s
    }

    fn pick_pane(s: &mut Setup<'_>, item: PaneMenuItem) {
        s.ws.update_in(s.cx, |ws, window, cx| {
            let epoch = ws.epoch;
            ws.pane_menu_pick(PaneId(1), item, epoch, window, cx);
        });
        s.cx.run_until_parked();
    }

    #[gpui::test]
    fn save_output_writes_the_history_where_the_panel_says(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = output_setup(cx, &fake, dir.path());
        let file = dir.path().join("out.txt");
        // ⇧⌘S asks where, for the focused pane's agent.
        s.cx.simulate_keystrokes("cmd-shift-s");
        assert!(s.cx.did_prompt_for_new_path());
        let to = file.clone();
        s.cx.simulate_new_path_selection(|_| Some(to));
        s.cx.run_until_parked();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "old line\nerror: boom\n"
        );
        let notice = s.app.read_with(s.cx, |a, _| a.notice().map(str::to_owned));
        assert!(notice.unwrap().starts_with("Saved output to "));

        // Cancelled: nothing written.
        std::fs::remove_file(&file).unwrap();
        pick_pane(&mut s, PaneMenuItem::SaveOutput);
        s.cx.simulate_new_path_selection(|_| None);
        s.cx.run_until_parked();
        assert!(!file.exists());

        // Exited agents too, from the row menu; a failed read is shown.
        fake.model(PaneId(1)).lock().unwrap().on_exited(1);
        agents(&mut s, &fake, vec![ended(5, "/w", 1, AgentState::Exited)]);
        pick_row(&mut s, 5, RowMenuItem::SaveOutput);
        let to = file.clone();
        s.cx.simulate_new_path_selection(|_| Some(to));
        s.cx.run_until_parked();
        assert!(file.exists());
        fake.outputs.lock().unwrap().clear();
        pick_row(&mut s, 5, RowMenuItem::SaveOutput);
        let to = dir.path().join("other.txt");
        s.cx.simulate_new_path_selection(|_| Some(to));
        s.cx.run_until_parked();
        let error = s.app.read_with(s.cx, |a, _| a.error().map(str::to_owned));
        assert_eq!(
            error.as_deref(),
            Some("Cannot save the output of sh: no such agent")
        );
        assert!(!dir.path().join("other.txt").exists());
    }

    #[gpui::test]
    fn open_output_writes_a_temp_file_and_opens_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = output_setup(cx, &fake, dir.path());
        let opened: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let seen = opened.clone();
        s.cx.update(|_, cx| {
            cx.set_global(crate::app::output::Opener(Arc::new(move |p: &Path| {
                seen.lock().unwrap().push(p.to_owned());
                Ok(())
            })))
        });
        pick_pane(&mut s, PaneMenuItem::OpenOutput);
        let opened = opened.lock().unwrap().clone();
        assert_eq!(opened.len(), 1);
        assert!(opened[0].ends_with(format!(
            "fleet-output/otter-{}.txt",
            crate::core::output::today()
        )));
        assert_eq!(
            std::fs::read_to_string(&opened[0]).unwrap(),
            "old line\nerror: boom\n"
        );
    }

    #[gpui::test]
    fn copy_last_output_copies_the_screen_as_text(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = output_setup(cx, &fake, dir.path());
        s.cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
        pick_pane(&mut s, PaneMenuItem::CopyLastOutput);
        let clip = s.cx.read_from_clipboard().and_then(|c| c.text());
        assert_eq!(clip.as_deref(), Some("error: boom"));
    }

    #[gpui::test]
    fn save_output_first_removes_only_what_was_saved(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(
            &mut s,
            &fake,
            vec![
                ended(8, "/w", 2, AgentState::Exited),
                ended(9, "/w", 3, AgentState::Lost),
                ended(10, "/w", 4, AgentState::Exited),
            ],
        );
        fake.outputs.lock().unwrap().insert(8, Ok("eight\n".into()));
        fake.outputs.lock().unwrap().insert(10, Ok("ten\n".into()));
        // The question offers it.
        s.cx.dispatch_action(RemoveExited);
        let (_, detail) = s.cx.pending_prompt().unwrap();
        assert!(detail.contains("deleted"));
        s.cx.simulate_prompt_answer("Cancel");
        s.cx.run_until_parked();

        let out = dir.path().join("saved");
        std::fs::create_dir(&out).unwrap();
        let ids = s.app.read_with(s.cx, |a, _| a.ended_ids());
        assert_eq!(ids, [8, 9, 10]);
        let app = s.app.clone();
        let to = out.clone();
        s.cx.update(|_, cx| crate::app::output::save_all_then_remove(app, ids, to, cx));
        s.cx.run_until_parked();
        // All three are "sh": numbered, never overwritten.
        let date = crate::core::output::today();
        assert_eq!(
            std::fs::read_to_string(out.join(format!("sh-{date}.txt"))).unwrap(),
            "eight\n"
        );
        assert_eq!(
            std::fs::read_to_string(out.join(format!("sh-{date}-2.txt"))).unwrap(),
            "ten\n"
        );
        assert_eq!(fake.calls().last(), Some(&Call::Forget(vec![8, 10])));
        let error = s.app.read_with(s.cx, |a, _| a.error().map(str::to_owned));
        assert_eq!(
            error.as_deref(),
            Some("Cannot save the output of sh: no such agent. It was not removed.")
        );
    }

    #[gpui::test]
    fn a_pending_spawn_shows_who_asks_and_can_be_approved_or_denied(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        fake.listed.store(true, std::sync::atomic::Ordering::SeqCst);
        agents(&mut s, &fake, vec![row(1, "/w", 1)]);
        assert!(s.cx.debug_bounds("pending-spawns").is_none());
        let spawn = |id| fleet_proto::PendingSpawn {
            id,
            parent: 1,
            program: "claude".into(),
            args: vec!["-p".into(), "go".into()],
            cwd: "/w".into(),
            ..Default::default()
        };
        *fake.pending.lock().unwrap() = vec![spawn(4), spawn(5)];
        agents(&mut s, &fake, vec![row(1, "/w", 1)]);
        assert!(s.cx.debug_bounds("pending-spawn-4").is_some());
        assert!(s.cx.debug_bounds("pending-spawn-5").is_some());
        click(&mut s, "pending-spawn-4-approve");
        click(&mut s, "pending-spawn-5-deny");
        let calls = fake.calls();
        assert!(calls.contains(&Call::Decide(4, true)), "{calls:?}");
        assert!(calls.contains(&Call::Decide(5, false)), "{calls:?}");

        // Over the graph too, where the sidebar is hidden.
        click(&mut s, "graph-toggle");
        assert!(s.ws.read_with(s.cx, |w, _| w.graph_shown()));
        let card = s.cx.debug_bounds("pending-spawn-4").unwrap();
        let toggle = s.cx.debug_bounds("graph-toggle").unwrap();
        assert!(card.top() > toggle.bottom(), "below the titlebar");

        fake.pending.lock().unwrap().clear();
        agents(&mut s, &fake, vec![row(1, "/w", 1)]);
        assert!(
            s.app.read_with(s.cx, |a, _| a.pending_spawns.is_empty()),
            "a decided spawn leaves"
        );
    }

    #[gpui::test]
    fn a_child_spawned_while_watching_is_highlighted(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let child = |id: u64, parent: u64| {
            let mut r = row(id, "/w", id as i64);
            r.info
                .labels
                .insert("fleet.parent".to_owned(), parent.to_string());
            r
        };
        fake.listed.store(true, std::sync::atomic::Ordering::SeqCst);
        // What was there at the first list is not news.
        agents(&mut s, &fake, vec![row(1, "/w", 1), child(2, 1)]);
        assert!(s.cx.debug_bounds("tab-2").is_some());
        assert!(s.cx.debug_bounds("tab-2-new").is_none());
        agents(
            &mut s,
            &fake,
            vec![row(1, "/w", 1), child(2, 1), child(3, 2), row(4, "/w", 4)],
        );
        assert!(s.cx.debug_bounds("tab-3-new").is_some());
        assert!(s.cx.debug_bounds("tab-2-new").is_none());
        assert!(s.cx.debug_bounds("tab-4-new").is_none(), "not a child");
        s.cx.simulate_keystrokes("cmd-shift-g");
        s.cx.run_until_parked();
        assert!(s.cx.debug_bounds("graph-card-3-new").is_some());
        assert!(s.cx.debug_bounds("graph-card-2-new").is_none());
    }

    fn peeked(s: &mut Setup<'_>) -> Option<u64> {
        s.ws.read_with(s.cx, |w, cx| w.peek().map(|p| p.read(cx).agent()))
    }

    #[gpui::test]
    fn space_peeks_at_the_selected_row_without_changing_the_layout(cx: &mut TestAppContext) {
        use crate::core::peek::PEEK_PANE;
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(1, "/w", 1), row(2, "/w", 2)]);
        // No selection: Space starts the filter, as before.
        s.ws.update_in(s.cx, |w, window, cx| w.focus_sidebar(window, cx));
        s.cx.simulate_keystrokes("space");
        assert_eq!(peeked(&mut s), None);
        s.ws.update_in(s.cx, |w, window, cx| {
            w.filter_field
                .update(cx, |f, cx| f.set_text(String::new(), cx));
            w.focus_sidebar(window, cx);
        });
        let at = center(&mut s, "tab-2");
        s.cx.simulate_click(at, Modifiers::command());
        s.cx.run_until_parked();
        s.cx.simulate_keystrokes("space");
        s.cx.run_until_parked();
        assert_eq!(peeked(&mut s), Some(2));
        assert!(s.cx.debug_bounds("peek").is_some());
        assert!(fake.calls().contains(&Call::Open(PEEK_PANE, Some(2))));
        assert_eq!(shown(&mut s, PaneId(1)), None, "the layout is unchanged");
        // Typing goes to the agent, through the peek's connection.
        s.cx.simulate_keystrokes("y");
        assert!(
            fake.pane_inputs
                .lock()
                .unwrap()
                .contains(&(PEEK_PANE, b"y".to_vec()))
        );
        // Escape closes it, and does not reach the agent.
        s.cx.simulate_keystrokes("escape");
        s.cx.run_until_parked();
        assert_eq!(peeked(&mut s), None);
        assert!(fake.calls().contains(&Call::Close(PEEK_PANE)));
        assert!(
            !fake
                .pane_inputs
                .lock()
                .unwrap()
                .iter()
                .any(|(p, b)| *p == PEEK_PANE && b == b"\x1b")
        );
        assert_eq!(shown(&mut s, PaneId(1)), None);
        // ⌘Return opens it in the focused pane.
        s.ws.update_in(s.cx, |w, window, cx| w.open_peek(2, window, cx));
        s.cx.simulate_keystrokes("cmd-enter");
        s.cx.run_until_parked();
        assert_eq!(peeked(&mut s), None);
        assert_eq!(shown(&mut s, PaneId(1)), Some(2));
    }

    #[gpui::test]
    fn one_peek_at_a_time(cx: &mut TestAppContext) {
        use crate::core::peek::PEEK_PANE;
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(1, "/w", 1), row(2, "/w", 2)]);
        s.ws.update_in(s.cx, |w, window, cx| w.open_peek(1, window, cx));
        s.ws.update_in(s.cx, |w, window, cx| w.open_peek(2, window, cx));
        s.cx.run_until_parked();
        assert_eq!(peeked(&mut s), Some(2));
        let opens = fake
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::Open(p, _) if *p == PEEK_PANE))
            .count();
        assert_eq!(opens, 2);
        assert!(fake.calls().contains(&Call::Close(PEEK_PANE)));
        // An unknown agent is not peeked at.
        s.cx.simulate_keystrokes("escape");
        s.ws.update_in(s.cx, |w, window, cx| w.open_peek(9, window, cx));
        assert_eq!(peeked(&mut s), None);
    }

    #[gpui::test]
    fn resting_on_a_graph_card_peeks(cx: &mut TestAppContext) {
        use crate::core::peek::HOVER_HOLD;
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(&mut s, &fake, vec![row(1, "/w", 1), row(2, "/w", 2)]);
        s.cx.simulate_keystrokes("cmd-shift-g");
        s.cx.run_until_parked();
        let at = center(&mut s, "graph-card-2");
        s.cx.simulate_mouse_move(at, None, Modifiers::none());
        s.cx.executor().advance_clock(HOVER_HOLD / 2);
        s.cx.run_until_parked();
        assert_eq!(peeked(&mut s), None, "not yet");
        s.cx.executor().advance_clock(HOVER_HOLD);
        s.cx.run_until_parked();
        assert_eq!(peeked(&mut s), Some(2));
        // Escape goes back to the graph, still shown.
        s.cx.simulate_keystrokes("escape");
        s.cx.run_until_parked();
        assert_eq!(peeked(&mut s), None);
        assert!(s.cx.debug_bounds("graph-card-2").is_some());
    }

    fn pane_has_keyboard(s: &mut Setup<'_>, pane: PaneId) -> bool {
        let app = s.app.clone();
        s.cx.update(|window, cx| {
            let view = app.read(cx).pane_view(pane).unwrap().clone();
            view.read(cx).focus_handle().is_focused(window)
        })
    }

    /// Issue #143: Rename, Edit Labels, the kill question and the peek
    /// close when their agent is removed (and stay for another agent's
    /// removal); the keyboard goes back to the focused pane.
    #[gpui::test]
    fn a_sheet_closes_when_its_agent_goes(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        type Open = fn(&mut Workspace, &mut gpui::Window, &mut gpui::Context<Workspace>);
        type IsOpen = fn(&Workspace) -> bool;
        let cases: [(&str, Open, IsOpen); 4] = [
            (
                "rename",
                |w, window, cx| w.open_rename(3, window, cx),
                |w| w.rename_sheet().is_some(),
            ),
            (
                "labels",
                |w, window, cx| w.open_labels(3, window, cx),
                |w| w.labels_sheet().is_some(),
            ),
            (
                "kill",
                |w, window, cx| w.kill(&Kill { agent_id: 3 }, window, cx),
                |w| w.kill_sheet().is_some(),
            ),
            (
                "peek",
                |w, window, cx| w.open_peek(3, window, cx),
                |w| w.peek().is_some(),
            ),
        ];
        for (what, open, is_open) in cases {
            agents(&mut s, &fake, vec![row(3, "/w", 3), row(4, "/w", 4)]);
            s.ws.update_in(s.cx, open);
            s.cx.run_until_parked();
            assert!(s.ws.read_with(s.cx, |w, _| is_open(w)), "{what} opens");
            agents(&mut s, &fake, vec![row(3, "/w", 3)]);
            assert!(
                s.ws.read_with(s.cx, |w, _| is_open(w)),
                "{what} stays for another agent's removal"
            );
            agents(&mut s, &fake, vec![]);
            assert!(
                s.ws.read_with(s.cx, |w, _| !w.has_modal()),
                "{what} closes with its agent"
            );
            assert!(
                pane_has_keyboard(&mut s, PaneId(1)),
                "{what}: keyboard back"
            );
        }
    }

    /// Issue #143: the kill question for several agents closes when one of
    /// them is removed (its list of names would be wrong).
    #[gpui::test]
    fn the_bulk_kill_question_closes_when_one_of_its_agents_goes(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        agents(
            &mut s,
            &fake,
            vec![row(2, "/w", 2), row(3, "/w", 3), row(4, "/w", 4)],
        );
        s.ws.update_in(s.cx, |w, window, cx| {
            let sheet =
                cx.new(|cx| super::KillSheet::many(vec![2, 3], vec!["2".into(), "3".into()], cx));
            let sub = cx.subscribe_in(&sheet, window, Workspace::kill_event);
            w.modal = Some(super::Modal::Kill(sheet, sub));
        });
        agents(&mut s, &fake, vec![row(2, "/w", 2), row(3, "/w", 3)]);
        assert!(s.ws.read_with(s.cx, |w, _| w.kill_sheet().is_some()));
        agents(&mut s, &fake, vec![row(2, "/w", 2)]);
        assert!(s.ws.read_with(s.cx, |w, _| w.kill_sheet().is_none()));
        assert!(kills(&fake).is_empty());
    }

    /// Issue #143: the Answer sheet closes when its question closes (the
    /// agent reported on, or asked another), or its agent goes.
    #[gpui::test]
    fn the_answer_sheet_closes_with_its_question(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let mut s = setup(cx, &fake, dir.path());
        let open = |s: &mut Setup<'_>| {
            s.ws.update_in(s.cx, |w, window, cx| w.open_answer(3, window, cx));
            s.cx.run_until_parked();
            s.ws.read_with(s.cx, |w, _| w.answer_sheet().is_some())
        };
        let still_open =
            |s: &mut Setup<'_>| s.ws.read_with(s.cx, |w, _| w.answer_sheet().is_some());
        for after in [
            vec![asking(3, 10)],
            vec![row(3, "/w", 3)],
            vec![row(4, "/w", 4)],
        ] {
            agents(&mut s, &fake, vec![asking(3, 9), row(4, "/w", 4)]);
            assert!(open(&mut s));
            // Another agent going changes nothing.
            agents(&mut s, &fake, vec![asking(3, 9)]);
            assert!(still_open(&mut s));
            agents(&mut s, &fake, after);
            assert!(!still_open(&mut s));
            assert!(pane_has_keyboard(&mut s, PaneId(1)));
        }
        assert!(!fake.calls().iter().any(|c| matches!(c, Call::Answer(..))));
    }
}
