//! The graph's terminal panel (issue #168, ADR 0105): the selected card's
//! agent, live, docked to the right of the graph (over it in a narrow
//! window). It is the pane's terminal view, headless, on its own read-only
//! connection (`GRAPH_PANEL_PANE`), as the peek's (ADR 0103): the layout's
//! panes and tabs never change. Space or ⏎ opens it and gives it the keys;
//! the selection moves it to another agent; Esc in it gives the keys back
//! to the graph, and Esc in the graph closes it.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, CursorStyle, Entity, EventEmitter, FocusHandle,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement,
    Render, StatefulInteractiveElement, Styled, Subscription, Window, div, px,
};

use super::app_state::AppState;
use super::controls;
use super::graph_view::{GraphEvent, GraphView};
use super::keymap::{BackToGraph, GRAPH_PANEL_CONTEXT, OpenInMainView};
use super::pane_view::PaneView;
use super::peek_view::{PAD_H, PAD_W, chrome_of};
use super::status_dot::status_dot;
use super::theme::{Typography, current as theme};
use crate::core::graph_panel::{Placement, dragged, placement, width};
use crate::core::icons::Icon;
use crate::core::peek::fit_within;
use crate::core::session::lock;
use crate::core::style::{chrome, space, text};

/// The panel's title strip.
const TITLE_H: f32 = 32.0;
/// The divider's grab strip, on the panel's left edge.
const DIVIDER_W: f32 = 6.0;

/// What the panel asks the graph to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphPanelEvent {
    /// Esc: the keys go back to the graph; the panel stays.
    Back,
    /// The close button.
    Close,
    /// ⌘⏎ or Open in Pane: the graph closes and a pane shows the agent.
    OpenInPane(u64),
    /// Actions…: the agent's row menu.
    Menu(u64),
}

pub struct GraphPanel {
    app: Entity<AppState>,
    agent: u64,
    pub(crate) pane: Entity<PaneView>,
    /// The panel's width this frame (the graph sets it).
    width: f32,
}

impl EventEmitter<GraphPanelEvent> for GraphPanel {}

impl GraphPanel {
    /// Opens the panel's connection on `agent`.
    fn new(app: Entity<AppState>, agent: u64, cx: &mut Context<Self>) -> Self {
        let pane = app.update(cx, |a, cx| a.show_in_graph_panel(agent, cx));
        GraphPanel {
            app,
            agent,
            pane,
            width: 0.0,
        }
    }

    pub fn agent(&self) -> u64 {
        self.agent
    }

    /// Shows `agent` instead, on the same connection.
    fn show(&mut self, agent: u64, cx: &mut Context<Self>) {
        if agent == self.agent {
            return;
        }
        self.agent = agent;
        self.app
            .update(cx, |a, cx| a.show_in_graph_panel(agent, cx));
        cx.notify();
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.pane.read(cx).focus_handle().clone()
    }

    /// The panel's terminal has the keyboard.
    pub fn focused(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle(cx).contains_focused(window, cx)
    }

    fn back(&mut self, _: &BackToGraph, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(GraphPanelEvent::Back);
    }

    fn open_in_pane(&mut self, _: &OpenInMainView, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(GraphPanelEvent::OpenInPane(self.agent));
    }
}

impl Render for GraphPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let id = self.agent;
        let chrome_facts = chrome_of(self.app.read(cx), id);
        let title = chrome_facts
            .name
            .clone()
            .or_else(|| chrome_facts.program.clone())
            .unwrap_or_else(|| format!("agent {id}"));
        let (status, fill, ended) = (
            chrome_facts.status,
            chrome_facts.status_fill,
            chrome_facts.agent_ended,
        );
        // The agent's grid (an observer never resizes it) at a font that
        // fits the panel.
        let base = self.app.read(cx).pane_font_size();
        let (grid, cell) = {
            let pane = self.pane.read(cx);
            let grid = lock(pane.model_handle()).terminal().size();
            (grid, pane.cell_size(base, window))
        };
        let viewport = window.viewport_size();
        let room = (
            self.width - DIVIDER_W - PAD_W,
            f32::from(viewport.height) - chrome::BAR_HEIGHT - TITLE_H - PAD_H,
        );
        let f = fit_within(grid, cell, base, room);
        self.pane.update(cx, |p, cx| {
            p.set_font_size(f.font, cx);
            p.set_chrome(chrome_facts, cx);
        });
        let now_ms = super::workspace::now_ms();
        let link = |sel: &'static str, label: &'static str| {
            div()
                .id(sel)
                .debug_selector(move || sel.to_owned())
                .px(px(space::XS))
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(move |s| s.bg(theme.hover).text_color(theme.text))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(label)
        };
        let title_bar = div()
            .h(px(TITLE_H))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SM))
            .pl(px(space::SM))
            .pr(px(space::XS))
            .border_b_1()
            .border_color(theme.separator)
            .type_style(text::CALLOUT)
            .text_color(theme.secondary)
            .when_some(status, |d, s| {
                d.child(status_dot(s, fill, &theme, now_ms, true))
            })
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.text)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(format!("#{id}"))
            .when(ended, |d| {
                d.child(
                    div()
                        .debug_selector(|| "graph-panel-exited".to_owned())
                        .px(px(space::XS))
                        .rounded(px(4.0))
                        .bg(theme.hover)
                        .type_style(text::CAPTION)
                        .child("Exited"),
                )
            })
            .child(div().flex_1())
            .child(
                link("graph-panel-open", "Open in Pane ⌘⏎").on_click(
                    cx.listener(move |_, _, _, cx| cx.emit(GraphPanelEvent::OpenInPane(id))),
                ),
            )
            .child(
                link("graph-panel-actions", "Actions…")
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(GraphPanelEvent::Menu(id)))),
            )
            .child(
                controls::icon_button("graph-panel-close", Icon::Close, &theme, true)
                    .debug_selector(|| "graph-panel-close".to_owned())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(GraphPanelEvent::Close))),
            );
        div()
            .id("graph-panel")
            .debug_selector(|| "graph-panel".to_owned())
            .key_context(GRAPH_PANEL_CONTEXT)
            .on_action(cx.listener(Self::back))
            .on_action(cx.listener(Self::open_in_pane))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.raised)
            .child(title_bar)
            .child(
                div().flex_1().min_h_0().overflow_hidden().child(
                    div()
                        .w(px(f.width + PAD_W))
                        .h(px(f.height + PAD_H))
                        .child(self.pane.clone()),
                ),
            )
    }
}

/// The graph's panel: the view while open, and the width the divider was
/// dragged to this session.
pub struct PanelSlot {
    app: Entity<AppState>,
    view: Option<(Entity<GraphPanel>, Subscription)>,
    /// The width dragged to (kept while the panel closes and opens again).
    wanted: Option<f32>,
    /// A divider drag: the width and the pointer's x when it began.
    drag: Option<(f32, f32)>,
}

impl PanelSlot {
    pub fn new(app: Entity<AppState>) -> Self {
        PanelSlot {
            app,
            view: None,
            wanted: None,
            drag: None,
        }
    }

    pub fn view(&self) -> Option<&Entity<GraphPanel>> {
        self.view.as_ref().map(|(v, _)| v)
    }

    pub fn is_open(&self) -> bool {
        self.view.is_some()
    }
}

impl GraphView {
    /// The panel's agent, while it is open.
    pub fn panel_agent(&self, cx: &App) -> Option<u64> {
        self.panel.view().map(|v| v.read(cx).agent())
    }

    /// The panel's terminal has the keyboard.
    pub fn panel_focused(&self, window: &Window, cx: &App) -> bool {
        self.panel
            .view()
            .is_some_and(|v| v.read(cx).focused(window, cx))
    }

    /// Space or ⏎: opens the panel on the selected card's agent, or gives
    /// the open panel the keys.
    pub fn open_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected() else { return };
        let view = match self.panel.view() {
            Some(v) => {
                let v = v.clone();
                v.update(cx, |p, cx| p.show(id, cx));
                v
            }
            None => {
                let app = self.panel.app.clone();
                let v = cx.new(|cx| GraphPanel::new(app, id, cx));
                let sub = cx.subscribe_in(&v, window, Self::panel_event);
                self.panel.view = Some((v.clone(), sub));
                v
            }
        };
        window.focus(&view.read(cx).focus_handle(cx));
        cx.notify();
    }

    /// The selection moved: an open panel follows it (the keys stay where
    /// they are).
    pub(super) fn panel_follow(&mut self, cx: &mut Context<Self>) {
        if let (Some(v), Some(id)) = (self.panel.view(), self.selected()) {
            v.clone().update(cx, |p, cx| p.show(id, cx));
        }
    }

    /// Closes the panel and its connection (never the agent); the graph
    /// takes the keys back if the panel had them.
    pub fn close_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((view, _)) = self.panel.view.take() else {
            return;
        };
        self.panel.drag = None;
        if view.read(cx).focused(window, cx) {
            self.focus(window);
        }
        self.panel.app.update(cx, |a, cx| a.close_graph_panel(cx));
        cx.notify();
    }

    fn panel_event(
        &mut self,
        _: &Entity<GraphPanel>,
        event: &GraphPanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match *event {
            GraphPanelEvent::Back => self.focus(window),
            GraphPanelEvent::Close => {
                self.close_panel(window, cx);
                self.focus(window);
            }
            GraphPanelEvent::OpenInPane(id) => cx.emit(GraphEvent::Open(id)),
            GraphPanelEvent::Menu(id) => cx.emit(GraphEvent::Menu(id)),
        }
    }

    /// The graph, with the panel beside it (or over it) while open.
    pub(super) fn with_panel(
        &mut self,
        graph: impl IntoElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(view) = self.panel.view().cloned() else {
            return graph.into_any_element();
        };
        let theme = theme(cx);
        let window_w = f32::from(window.viewport_size().width);
        let w = width(self.panel.wanted, window_w);
        view.update(cx, |p, _| p.width = w);
        let divider = div()
            .id("graph-panel-divider")
            .debug_selector(|| "graph-panel-divider".to_owned())
            .absolute()
            .left_0()
            .top_0()
            .bottom_0()
            .w(px(DIVIDER_W))
            .cursor(CursorStyle::ResizeLeftRight)
            .hover(move |s| s.bg(theme.accent.opacity(0.4)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    if ev.click_count >= 2 {
                        this.panel.wanted = None;
                        this.panel.drag = None;
                    } else {
                        let start = width(this.panel.wanted, window_w);
                        this.panel.drag = Some((start, f32::from(ev.position.x)));
                    }
                    window.refresh();
                }),
            );
        let panel = div()
            .id("graph-panel-dock")
            .relative()
            .flex_none()
            .w(px(w))
            .h_full()
            .occlude()
            .border_l_1()
            .border_color(theme.separator)
            .shadow_lg()
            .child(div().size_full().pl(px(DIVIDER_W)).child(view))
            .child(divider);
        let overlay = placement(window_w, w) == Placement::Overlay;
        let out = div()
            .size_full()
            .relative()
            .flex()
            .flex_row()
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, window, cx| {
                let Some((start, from)) = this.panel.drag else {
                    return;
                };
                if ev.pressed_button != Some(MouseButton::Left) {
                    this.panel.drag = None;
                    return;
                }
                let w = dragged(start, from, f32::from(ev.position.x), window_w);
                this.panel.wanted = Some(w);
                window.set_window_cursor_style(CursorStyle::ResizeLeftRight);
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.panel.drag = None),
            )
            .child(div().flex_1().min_w_0().h_full().child(graph));
        if overlay {
            out.child(
                div()
                    .debug_selector(|| "graph-panel-overlay".to_owned())
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .child(panel),
            )
            .into_any_element()
        } else {
            out.child(panel).into_any_element()
        }
    }
}
