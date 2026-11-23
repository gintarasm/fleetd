//! The graph view (⌘⇧G, ADR 0073): showing and hiding it, its events.

use gpui::{AppContext, Context, Entity, Subscription, Window, point, px};

use super::Workspace;
use crate::app::app_state::AppState;
use crate::app::graph_view::{GraphEvent, GraphView};
use crate::app::native;

impl Workspace {
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
    pub(super) fn close_graph_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.graph.0.update(cx, |g, cx| g.close_panel(window, cx));
    }

    /// The session asks for message edges only while the graph is shown.
    pub(super) fn watch_message_lines(&self, cx: &mut Context<Self>) {
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
}

/// The graph view and its event subscription.
pub(super) fn new_graph(
    app: &Entity<AppState>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> (Entity<GraphView>, Subscription) {
    let graph = cx.new(|cx| GraphView::new(app.clone(), cx));
    let sub = cx.subscribe_in(&graph, window, Workspace::graph_event);
    (graph, sub)
}

/// Pinches over the window zoom the graph while it is shown.
pub(super) fn pinch_to_graph(
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
