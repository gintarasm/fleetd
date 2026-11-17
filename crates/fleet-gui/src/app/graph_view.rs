//! The graph view (⌘⇧G; spec §7, ADR 0073): the spawned-by tree of every
//! agent, full window, in place of the sidebar and panes.
//!
//! Cards are ordinary gpui elements, absolutely positioned, built only for
//! those in the viewport. One `canvas` paints the boxes and every edge.
//! The layout ([`crate::core::graph_layout`]) is computed again only when
//! its [`Signature`] changes (the tree, collapse, filter result), never on
//! activity; activity redraws the cards in view only.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use fleet_client::tree::Tree;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext, BorderStyle, Bounds, Context, Entity, EventEmitter, FocusHandle,
    FontWeight, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, ParentElement, PathBuilder, Pixels, Point, PromptButton, PromptLevel, Render,
    ScrollDelta, ScrollWheelEvent, SharedString, StatefulInteractiveElement, Styled, Subscription,
    Window, canvas, div, point, px, quad,
};

use super::app_state::AppState;
use super::controls;
use super::icons;
use super::sidebar_view::tab_title;
use super::status_dot::dot_color;
use super::text_field::TextField;
use super::theme::{Theme, Typography};
use crate::core::graph::{
    CardContent, CardDot, DotFill, card_content, filter_shown, git_root_with, project_title,
    removable, remove_question, stop_order, stop_question, tree_by_id, tree_of,
};
use crate::core::graph_layout::{
    BoxKind, Card, GraphLayout, LayoutInput, Signature, Step, Viewport, ZOOM_STEP, layout, step,
};
use crate::core::icons::Icon;
use crate::core::layout::Rect;
use crate::core::message_lines;
use crate::core::session::AgentRow;
use crate::core::sidebar::ended;
use crate::core::style::{chrome, radius, space, text};
use crate::core::target::Target;
use crate::core::zoom::Zoom;

/// Key context of the graph.
pub const GRAPH_CONTEXT: &str = "Graph";
/// The strip above the graph (filter field, hints).
const HEADER_HEIGHT: f32 = 44.0;
/// Below this zoom a card shows its name and dot only.
const COMPACT_ZOOM: f32 = 0.6;
/// A scroll with ⌘ or ⌃ held zooms by `exp(-dy * this)`.
const SCROLL_ZOOM: f32 = 0.01;

/// What the graph asks the workspace to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphEvent {
    /// ⏎ or a click: back to the panes with this agent focused.
    Open(u64),
    /// Esc: back to the panes.
    Close,
}

pub struct GraphView {
    app: Entity<AppState>,
    focus: FocusHandle,
    pub(super) filter: Entity<TextField>,
    /// Agents whose children are hidden (for the session).
    collapsed: HashSet<u64>,
    selected: Option<u64>,
    viewport: Viewport,
    /// Where the graph area was last painted (window coordinates).
    area: Rc<Cell<Bounds<Pixels>>>,
    layout: Rc<GraphLayout>,
    signature: Option<Signature>,
    /// The forest the layout was built from.
    forest: Rc<Tree>,
    /// Each agent's project, as of the last layout.
    projects: HashMap<u64, String>,
    /// Git roots found for a cwd (local target only).
    git_roots: HashMap<String, Option<String>>,
    /// A background drag: the last mouse position.
    drag: Option<Point<Pixels>>,
    /// Layouts computed so far (tests check activity does not lay out).
    pub(super) layouts: usize,
    /// The cards the last frame built (tests check culling).
    pub(super) built: Vec<u64>,
    /// Redraws while a new child's highlight shows (ADR 0079).
    new_child_timer: controls::RedrawTimer,
    _subs: Vec<Subscription>,
}

impl EventEmitter<GraphEvent> for GraphView {}

impl GraphView {
    pub fn new(app: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| TextField::new(String::new(), "Filter (name, status, label)", cx));
        let subs = vec![
            cx.observe(&filter, |_, _, cx| cx.notify()),
            cx.observe(&app, |_, _, cx| cx.notify()),
        ];
        GraphView {
            app,
            focus: cx.focus_handle(),
            filter,
            collapsed: HashSet::new(),
            selected: None,
            viewport: Viewport::default(),
            area: Rc::new(Cell::new(Bounds::default())),
            layout: Rc::default(),
            signature: None,
            forest: Rc::default(),
            projects: HashMap::new(),
            git_roots: HashMap::new(),
            drag: None,
            layouts: 0,
            built: Vec::new(),
            new_child_timer: controls::RedrawTimer::default(),
            _subs: subs,
        }
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus);
    }

    pub fn selected(&self) -> Option<u64> {
        self.selected
    }

    pub fn layout(&self) -> &GraphLayout {
        &self.layout
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    /// The nearest `.git` above `cwd`, looked up once per cwd and only for
    /// a local fleetd (a remote one's files are not on this Mac).
    fn git_root(&mut self, cwd: &str, local: bool) -> Option<String> {
        if !local {
            return None;
        }
        self.git_roots
            .entry(cwd.to_owned())
            .or_insert_with(|| git_root_with(cwd, &|p: &Path| p.exists()))
            .clone()
    }

    /// Lays out again if the tree, collapse or filter result changed.
    fn refresh_layout(&mut self, cx: &App) {
        let app = self.app.read(cx);
        let rows = app.agents.clone();
        let local = *app.target() == Target::Local;
        let home = app.home.clone();
        let machine = machine_title(app.target());
        let titles = app.titles.clone();
        let statuses = app.statuses.clone();
        let forest = match super::bench::synthetic_parents(&rows) {
            Some(nodes) => tree_by_id(nodes),
            None => tree_of(&rows),
        };
        let mut projects = HashMap::with_capacity(rows.len());
        for r in &rows {
            let root = self.git_root(&r.info.cwd, local);
            let title = project_title(&r.info, &home, &|_| root.clone());
            projects.insert(r.info.id, title);
        }
        let query = self.filter.read(cx).text().to_owned();
        let shown = filter_shown(
            &forest,
            &rows,
            &query,
            &home,
            &|r| tab_title(r, &titles),
            &|id| statuses.get(&id).map(|s| s.label()),
        );
        // A tree goes in its root's project box.
        let project = |id: u64| {
            projects
                .get(&forest.root_of(id))
                .cloned()
                .unwrap_or_default()
        };
        let sig = Signature::new(&machine, &forest, &project, &self.collapsed, shown.as_ref());
        if self.signature.as_ref() != Some(&sig) {
            self.layout = Rc::new(layout(&LayoutInput {
                forest: &forest,
                machine: &machine,
                project: &project,
                collapsed: &self.collapsed,
                shown: shown.as_ref(),
            }));
            self.signature = Some(sig);
            self.forest = Rc::new(forest);
            self.layouts += 1;
        }
        self.projects = projects;
        if self
            .selected
            .is_none_or(|id| self.layout.card(id).is_none())
        {
            self.selected = self.layout.cards.first().map(|c| c.id);
        }
    }

    /// The graph area's size: as last painted, else the window below the
    /// bar and the header.
    fn area_size(&self, window: &Window) -> (f32, f32) {
        let b = self.area.get();
        if b.size.width > px(0.) && b.size.height > px(0.) {
            return (f32::from(b.size.width), f32::from(b.size.height));
        }
        let v = window.viewport_size();
        (
            f32::from(v.width),
            (f32::from(v.height) - chrome::BAR_HEIGHT - HEADER_HEIGHT).max(0.0),
        )
    }

    fn select(&mut self, id: u64, window: &Window, cx: &mut Context<Self>) {
        self.selected = Some(id);
        if let Some(card) = self.layout.card(id) {
            let size = self.area_size(window);
            self.viewport.reveal(&card.rect, size);
        }
        cx.notify();
    }

    fn step(&mut self, dir: Step, window: &Window, cx: &mut Context<Self>) {
        let next = match self.selected {
            Some(at) => step(&self.layout, at, dir),
            None => self.layout.cards.first().map(|c| c.id),
        };
        if let Some(id) = next {
            self.select(id, window, cx);
        }
    }

    /// Space: hides or shows the selected agent's children.
    pub fn toggle_collapse(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected else { return };
        if self.forest.children(id).is_empty() {
            return;
        }
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
        }
        cx.notify();
    }

    fn rows_by_id(&self, cx: &App) -> HashMap<u64, AgentRow> {
        self.app
            .read(cx)
            .agents
            .iter()
            .map(|r| (r.info.id, r.clone()))
            .collect()
    }

    fn name_of(&self, id: u64, cx: &App) -> String {
        let app = self.app.read(cx);
        app.agents
            .iter()
            .find(|r| r.info.id == id)
            .map_or_else(|| format!("agent {id}"), |r| tab_title(r, &app.titles))
    }

    /// ⌫: stops the selected agent's subtree after asking: a `KillAgent`
    /// per running agent, deepest first (ADR 0071's escalation applies to
    /// each).
    pub fn stop_subtree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.selected else { return };
        let rows = self.rows_by_id(cx);
        let running = |id: u64| rows.get(&id).is_some_and(|r| !ended(r));
        let ids = stop_order(&self.forest, root, &running);
        if ids.is_empty() {
            return;
        }
        let question = stop_question(&self.name_of(root, cx), ids.len());
        let answer = window.prompt(
            PromptLevel::Warning,
            &question,
            Some("Each is stopped, deepest first. Ended agents stay until removed."),
            &[PromptButton::new("Stop"), PromptButton::cancel("Cancel")],
            cx,
        );
        let app = self.app.clone();
        cx.spawn(async move |_, cx| {
            if answer.await == Ok(0) {
                let _ = app.update(cx, |a, _| {
                    for id in ids {
                        a.kill(id);
                    }
                });
            }
        })
        .detach();
    }

    /// ⇧⌫: removes the selected subtree when everything in it has ended
    /// (asking first when it is more than one agent).
    pub fn remove_subtree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.selected else { return };
        let rows = self.rows_by_id(cx);
        let is_ended = |id: u64| rows.get(&id).is_some_and(ended);
        let Some(ids) = removable(&self.forest, root, &is_ended) else {
            return;
        };
        let app = self.app.clone();
        match remove_question(&self.name_of(root, cx), ids.len()) {
            None => app.read(cx).forget_many(ids),
            Some(question) => {
                let answer = window.prompt(
                    PromptLevel::Warning,
                    &question,
                    Some("Their history and scrollback are deleted."),
                    &[PromptButton::new("Remove"), PromptButton::cancel("Cancel")],
                    cx,
                );
                cx.spawn(async move |_, cx| {
                    if answer.await == Ok(0) {
                        let _ = app.update(cx, |a, _| a.forget_many(ids));
                    }
                })
                .detach();
            }
        }
    }

    /// ⌘+ / ⌘− / ⌘0 while the graph is shown, about the area's centre.
    pub fn zoom(&mut self, zoom: Zoom, window: &Window, cx: &mut Context<Self>) {
        let (w, h) = self.area_size(window);
        let centre = (w / 2.0, h / 2.0);
        let factor = match zoom {
            Zoom::In => ZOOM_STEP,
            Zoom::Out => 1.0 / ZOOM_STEP,
            Zoom::Reset => 1.0 / self.viewport.zoom,
        };
        self.zoom_at(centre, factor, window, cx);
    }

    /// A pinch (`factor` > 1 zooms in) at `at`, window coordinates.
    pub fn magnify(
        &mut self,
        at: Point<Pixels>,
        factor: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let local = self.to_area(at);
        self.zoom_at(local, factor, window, cx);
    }

    fn zoom_at(&mut self, at: (f32, f32), factor: f32, window: &Window, cx: &mut Context<Self>) {
        self.viewport.zoom_at(at, factor);
        let size = self.area_size(window);
        self.viewport
            .clamp((self.layout.width, self.layout.height), size);
        cx.notify();
    }

    fn to_area(&self, p: Point<Pixels>) -> (f32, f32) {
        let o = self.area.get().origin;
        (f32::from(p.x - o.x), f32::from(p.y - o.y))
    }

    fn open(&mut self, id: u64, cx: &mut Context<Self>) {
        self.selected = Some(id);
        cx.emit(GraphEvent::Open(id));
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        if m.platform || m.control || m.alt || !self.focus.is_focused(window) {
            return;
        }
        let key = ev.keystroke.key.as_str();
        match (key, m.shift) {
            ("left", _) => self.step(Step::Left, window, cx),
            ("right", _) => self.step(Step::Right, window, cx),
            ("up", _) => self.step(Step::Up, window, cx),
            ("down", _) => self.step(Step::Down, window, cx),
            ("enter", _) => {
                if let Some(id) = self.selected {
                    self.open(id, cx);
                }
            }
            ("backspace", false) | ("delete", false) => self.stop_subtree(window, cx),
            ("backspace", true) | ("delete", true) => self.remove_subtree(window, cx),
            ("space", _) => self.toggle_collapse(cx),
            ("/", _) => window.focus(self.filter.read(cx).focus_handle()),
            ("escape", _) => {
                if self.filter.read(cx).text().is_empty() {
                    cx.emit(GraphEvent::Close);
                } else {
                    self.filter
                        .update(cx, |f, cx| f.set_text(String::new(), cx));
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    /// In the filter: Esc clears it (then gives the keys back to the
    /// graph), ⏎ goes back to the graph on the first match.
    fn filter_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "escape" => {
                if self.filter.read(cx).text().is_empty() {
                    window.focus(&self.focus);
                } else {
                    self.filter
                        .update(cx, |f, cx| f.set_text(String::new(), cx));
                }
            }
            "enter" | "down" => {
                self.refresh_layout(cx);
                window.focus(&self.focus);
                cx.notify();
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn scroll(&mut self, ev: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        let (dx, dy) = match ev.delta {
            ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
            ScrollDelta::Lines(l) => (l.x * 20.0, l.y * 20.0),
        };
        if ev.modifiers.platform || ev.modifiers.control {
            let at = self.to_area(ev.position);
            self.zoom_at(at, (dy * SCROLL_ZOOM).exp(), window, cx);
        } else {
            self.viewport.pan_by((dx, dy));
            let size = self.area_size(window);
            self.viewport
                .clamp((self.layout.width, self.layout.height), size);
            cx.notify();
        }
        cx.stop_propagation();
    }

    /// `fresh`: the new-child highlight's strength, 0 to 1 (ADR 0079).
    fn render_card(
        &self,
        card: &Card,
        c: CardContent,
        fresh: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = card.id;
        let z = self.viewport.zoom;
        let r = self.viewport.rect_to_screen(&card.rect);
        let selected = self.selected == Some(id);
        let size = |s: f32| px(s * z);
        let compact = z < COMPACT_ZOOM;
        let border = if selected {
            theme.accent
        } else if fresh > 0.0 {
            theme.accent.opacity(0.4 + 0.6 * fresh)
        } else if c.waiting {
            theme.warning
        } else {
            theme.border
        };
        let line = |t: crate::core::style::TextStyle| (size(t.size), size(t.line_height));
        let (head, head_lh) = line(text::HEADLINE);
        let (cap, cap_lh) = line(text::CAPTION);
        let dot = c.dot.map(|d| card_dot(d, z, theme));
        let name_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(size(space::XS))
            .children(dot)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(head)
                    .line_height(head_lh)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(c.name)),
            )
            .children(c.run_time.map(|t| {
                div()
                    .flex_none()
                    .text_size(cap)
                    .line_height(cap_lh)
                    .text_color(theme.secondary)
                    .child(SharedString::from(t))
            }));
        let meta = (!compact).then(|| {
            div()
                .truncate()
                .text_size(cap)
                .line_height(cap_lh)
                .text_color(theme.secondary)
                .child(SharedString::from(format!("{} · {}", c.program, c.project)))
        });
        let activity = (!compact && !c.activity.is_empty()).then(|| {
            div()
                .truncate()
                .text_size(cap)
                .line_height(cap_lh)
                .text_color(theme.tertiary)
                .child(SharedString::from(c.activity))
        });
        let hidden = c.hidden.map(|h| {
            div()
                .absolute()
                .right(size(space::SM))
                .bottom(size(space::XS))
                .px(size(space::XS))
                .rounded(size(radius::SM))
                .bg(theme.selected)
                .text_size(cap)
                .line_height(cap_lh)
                .text_color(theme.secondary)
                .child(SharedString::from(format!("▸ {h}")))
        });
        div()
            .id(("graph-card", id))
            .debug_selector(move || format!("graph-card-{id}"))
            .absolute()
            .left(px(r.x))
            .top(px(r.y))
            .w(px(r.w))
            .h(px(r.h))
            .flex()
            .flex_col()
            .gap(size(space::XXS))
            .p(size(space::SM))
            .rounded(size(radius::LG))
            .overflow_hidden()
            .bg(if fresh > 0.0 {
                theme.card.blend(theme.accent.opacity(0.22 * fresh))
            } else if c.waiting {
                theme.card.blend(theme.warning.opacity(0.12))
            } else {
                theme.card
            })
            .when(fresh > 0.0, |d| {
                d.debug_selector(move || format!("graph-card-{id}-new"))
            })
            .border_color(border)
            .when(selected, |d| d.border_2())
            .when(!selected, |d| d.border_1())
            .when(c.ended, |d| d.opacity(0.5))
            .text_color(theme.text)
            .cursor_pointer()
            .hover(|s| s.border_color(theme.accent.opacity(0.6)))
            // Not a pan.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| this.open(id, cx)))
            .child(name_row)
            .children(meta)
            .children(activity)
            .children(hidden)
            .into_any_element()
    }
}

/// The machine box's title.
fn machine_title(target: &Target) -> String {
    match target {
        Target::Local => "This Mac".to_owned(),
        Target::Ssh { dest } => dest.clone(),
    }
}

/// A status dot: hollow for an inferred status, solid for a reported one
/// (M3-D).
fn card_dot(d: CardDot, zoom: f32, theme: &Theme) -> gpui::Div {
    let colour = dot_color(d.status, theme);
    let dot = div().flex_none().size(px(8.0 * zoom)).rounded_full();
    match d.fill {
        DotFill::Solid => dot.bg(colour),
        DotFill::Hollow if zoom >= 1.5 => dot.border_2().border_color(colour),
        DotFill::Hollow => dot.border_1().border_color(colour),
    }
}

fn to_px(o: Point<Pixels>, p: (f32, f32)) -> Point<Pixels> {
    point(o.x + px(p.0), o.y + px(p.1))
}

fn bounds_of(o: Point<Pixels>, r: &Rect) -> Bounds<Pixels> {
    Bounds::new(to_px(o, (r.x, r.y)), gpui::size(px(r.w), px(r.h)))
}

impl Render for GraphView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_layout(cx);
        super::bench::graph_rendered(window);
        let app = self.app.read(cx);
        let theme = app.theme;
        let now = super::workspace::now_ms();
        let size = self.area_size(window);
        let view = self.viewport.world_rect(size);
        let vp = self.viewport;
        let layout = self.layout.clone();

        // Boxes and edges, in screen coordinates relative to the area.
        let boxes: Vec<(BoxKind, Rect)> = layout
            .boxes_in(&view)
            .map(|b| (b.kind, vp.rect_to_screen(&b.rect)))
            .collect();
        let rows: HashMap<u64, &AgentRow> = app.agents.iter().map(|r| (r.info.id, r)).collect();
        let mut live_edges = Vec::new();
        let mut ended_edges = Vec::new();
        for e in layout.edges_in(&view) {
            let pts = e.points.map(|p| vp.to_screen(p));
            if rows.get(&e.child).is_some_and(|r| ended(r)) {
                ended_edges.push(pts);
            } else {
                live_edges.push(pts);
            }
        }
        // Message lines (ADR 0089): the recent ones between cards the layout
        // has, culled to the view, one path per fade step.
        let mut msg_lines: [Vec<[(f32, f32); 2]>; message_lines::STEPS] = Default::default();
        for line in message_lines::lines(&layout, &app.message_edges, now, &view) {
            for seg in line.segments {
                msg_lines[line.step].push(seg.map(|p| vp.to_screen(p)));
            }
        }
        let msg_colour = theme.accent;
        let area = self.area.clone();
        let (box_fill, box_border, edge, edge_ended) = (
            theme.hover.opacity(0.35),
            theme.separator,
            theme.secondary.opacity(0.7),
            theme.tertiary.opacity(0.4),
        );
        let z = vp.zoom;
        let paint = canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let old = area.replace(bounds);
                if old.size != bounds.size && old.size.width > px(0.) {
                    // Culling used the old size.
                    window.refresh();
                }
                let o = bounds.origin;
                for (kind, r) in &boxes {
                    let corner = px(radius::LG * z);
                    let fill = match kind {
                        BoxKind::Machine => box_fill.opacity(0.0),
                        BoxKind::Project => box_fill,
                    };
                    window.paint_quad(quad(
                        bounds_of(o, r),
                        corner,
                        fill,
                        px(1.),
                        box_border,
                        match kind {
                            BoxKind::Machine => BorderStyle::Dashed,
                            BoxKind::Project => BorderStyle::Solid,
                        },
                    ));
                }
                for (lines, colour) in [(&ended_edges, edge_ended), (&live_edges, edge)] {
                    if lines.is_empty() {
                        continue;
                    }
                    let mut path = PathBuilder::stroke(px((1.5 * z).max(1.0)));
                    for pts in lines.iter() {
                        path.move_to(to_px(o, pts[0]));
                        for &p in &pts[1..] {
                            path.line_to(to_px(o, p));
                        }
                    }
                    if let Ok(path) = path.build() {
                        window.paint_path(path, colour);
                    }
                }
                // Oldest (faintest) first, so fresh lines lie on top.
                for (step, segs) in msg_lines.iter().enumerate().rev() {
                    if segs.is_empty() {
                        continue;
                    }
                    let mut path = PathBuilder::stroke(px((1.25 * z).max(1.0)));
                    for seg in segs {
                        path.move_to(to_px(o, seg[0]));
                        path.line_to(to_px(o, seg[1]));
                    }
                    if let Ok(path) = path.build() {
                        window
                            .paint_path(path, msg_colour.opacity(message_lines::opacity_of(step)));
                    }
                }
            },
        )
        .absolute()
        .size_full();

        let titles: Vec<AnyElement> = layout
            .boxes_in(&view)
            .map(|b| {
                let r = vp.rect_to_screen(&b.rect);
                let (pad, ts) = (space::SM * z, text::CAPTION);
                div()
                    .absolute()
                    .left(px(r.x + pad))
                    .top(px(r.y + pad / 2.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::XS * z))
                    .text_size(px(ts.size * z))
                    .line_height(px(ts.line_height * z))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.secondary)
                    .when(b.kind == BoxKind::Machine, |d| {
                        d.child(
                            icons::sized(Icon::Server, space::MD * z).text_color(theme.secondary),
                        )
                    })
                    .child(SharedString::from(b.title.clone()))
                    .into_any_element()
            })
            .collect();

        let statuses = app.statuses.clone();
        let titles_by_id = app.titles.clone();
        let contents: Vec<(Card, CardContent)> = layout
            .cards_in(&view)
            .filter_map(|card| {
                let row = rows.get(&card.id)?;
                let content = card_content(
                    row,
                    tab_title(row, &titles_by_id),
                    self.projects.get(&card.id).cloned().unwrap_or_default(),
                    statuses.get(&card.id).copied(),
                    card.collapsed.then_some(card.children),
                    now,
                );
                Some((card.clone(), content))
            })
            .collect();
        self.built = contents.iter().map(|(c, _)| c.id).collect();
        let empty = layout.is_empty();
        let filtering = !self.filter.read(cx).text().trim().is_empty();
        let (fresh, next) = {
            let app = self.app.read(cx);
            let (at, reduce) = (Instant::now(), controls::reduces_motion(cx));
            (
                app.new_children.strengths(at, reduce),
                app.new_children.next_change(at, reduce),
            )
        };
        let mut timer = std::mem::take(&mut self.new_child_timer);
        timer.schedule(next, window, cx);
        self.new_child_timer = timer;
        let cards: Vec<AnyElement> = contents
            .into_iter()
            .map(|(card, content)| {
                let f = fresh.get(&card.id).copied().unwrap_or(0.0);
                self.render_card(&card, content, f, &theme, cx)
            })
            .collect();
        let placeholder = empty.then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .type_style(text::BODY)
                .text_color(theme.tertiary)
                .child(if filtering {
                    "No agent matches the filter"
                } else {
                    "No agents"
                })
        });
        // Painted last: the end of the graph's frame (bench).
        let end = canvas(
            |_, _, _| {},
            |_, _, window, _| super::bench::graph_painted(window),
        )
        .absolute()
        .w(px(0.))
        .h(px(0.));

        let header = div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(px(space::MD))
            .h(px(HEADER_HEIGHT))
            .px(px(space::LG))
            .border_b_1()
            .border_color(theme.separator)
            .child(
                div()
                    .w(px(chrome::SIDEBAR_WIDTH))
                    .on_key_down(cx.listener(Self::filter_key_down))
                    .child(self.filter.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .type_style(text::CAPTION)
                    .text_color(theme.tertiary)
                    .child(
                        "Arrows move · ⏎ open · Space collapse · ⌫ stop subtree · ⇧⌫ remove ended · / filter · Esc back",
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .type_style(text::CAPTION)
                    .text_color(theme.secondary)
                    .child(SharedString::from(format!("{:.0}%", vp.zoom * 100.0))),
            );

        let area = div()
            .id("graph-area")
            .debug_selector(|| "graph-area".to_owned())
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .on_scroll_wheel(cx.listener(Self::scroll))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, _| {
                    this.drag = Some(ev.position);
                    window.focus(&this.focus);
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                let Some(last) = this.drag else { return };
                if ev.pressed_button != Some(MouseButton::Left) {
                    this.drag = None;
                    return;
                }
                let d = ev.position - last;
                this.drag = Some(ev.position);
                this.viewport.pan_by((f32::from(d.x), f32::from(d.y)));
                let size = this.area_size(window);
                this.viewport
                    .clamp((this.layout.width, this.layout.height), size);
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag = None),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag = None),
            )
            .child(paint)
            .children(titles)
            .children(cards)
            .children(placeholder)
            .child(end);

        div()
            .id("graph")
            .key_context(GRAPH_CONTEXT)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(area)
    }
}
