//! The workspace window: the sidebar, up to four split panes and the
//! sheets over them, drawn from [`AppState`].

use std::collections::HashMap;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{
    App, AppContext, Bounds, Context, Entity, EntityId, FocusHandle, Subscription, Task,
    TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds, WindowOptions, point, px,
    size,
};

mod agents;
mod graph;
mod menus;
mod palette;
mod panes;
mod render;
mod sheets;
mod sidebar;
mod tabs;
mod workflow_ui;

use self::graph::{new_graph, pinch_to_graph};

use super::action_sheet::ActionSheet;
use super::answer_sheet::AnswerSheet;
use super::app_state::AppState;
use super::connect::ConnectSheet;
use super::controls;
use super::controls::ReducedMotion;
use super::graph_view::GraphView;
use super::kill_sheet::KillSheet;
use super::labels_sheet::LabelsSheet;
use super::native;
use super::new_agent::NewAgentSheet;
use super::pane_drag::PaneDrag;
use super::peek_view::PeekView;
use super::picker_view::PickerView;
use super::rename::RenameSheet;
use super::search_all_view::SearchAllView;
use super::send_sheet::SendSheet;
use super::split_chooser::SplitChooserView;
use super::split_dividers::SplitDrag;
use super::text_field::TextField;
use super::theme::{Theme, appearance_of};
use crate::core::broadcast::RowSelection;
use crate::core::config::GuiConfig;
use crate::core::drag_drop::DropTarget;
use crate::core::glass::{Backdrop, GlassChoice, glass_tint};
use crate::core::palette::Entry;
use crate::core::picker::Recents;
use crate::core::question::open_question;
use crate::core::session::PaneId;
use crate::core::settings::{MotionChoice, ThemeChoice};
use crate::core::shortcuts::Section;
use crate::core::sidebar::StatusFilter;
use crate::core::style::chrome;
use crate::core::switcher::AgentMru;
use crate::core::theme::Tokens;

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
    /// What the last frame handed the sidebar for each row (#147).
    #[cfg(test)]
    drawn_rows: Vec<DrawnRow>,
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
            #[cfg(test)]
            drawn_rows: Vec::new(),
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

    /// The sidebar rows picked with ⌘-click and ⇧-click.
    pub fn row_selection(&self) -> &RowSelection {
        &self.row_selection
    }

    /// Shows the agent in the focused pane (a sidebar click).
    pub fn show_agent(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.app.update(cx, |a, cx| a.show_agent(agent_id, cx));
        self.focus_current(window, cx);
    }
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

pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests;

/// One sidebar row as a frame saw it: a test reads it to catch the sidebar
/// drawing from stale data (#147).
#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
struct DrawnRow {
    id: u64,
    title: String,
    status: Option<crate::core::status::AgentStatus>,
    bell: bool,
    mark: crate::app::sidebar_view::TabMark,
}

#[cfg(test)]
impl DrawnRow {
    fn of(data: &crate::app::sidebar_view::SidebarData<'_>) -> Vec<DrawnRow> {
        data.rows
            .iter()
            .map(|r| {
                let id = r.info.id;
                DrawnRow {
                    id,
                    title: crate::app::sidebar_view::tab_title(r, data.titles),
                    status: data.statuses.get(&id).copied(),
                    bell: data.bells.contains(&id),
                    mark: data.marks.get(id),
                }
            })
            .collect()
    }
}
