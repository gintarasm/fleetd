//! Peek (issue #142, ADR 0103): a popover with an agent's live screen,
//! over the window, without changing the layout. Space on the sidebar's
//! selected row, or the pointer resting on a graph card, opens it. Typing
//! goes to the agent; Escape (or a click outside) closes it; ⌘Return
//! opens the agent in the focused pane.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Render, Styled, Window, deferred, div, px,
};

use super::app_state::AppState;
use super::controls::appear;
use super::pane_view::{PaneChrome, PaneView};
use super::status_dot::status_dot;
use super::theme::{Typography, current as theme};
use crate::core::peek::{PeekKey, fit, peek_key};
use crate::core::session::lock;
use crate::core::sidebar::{ended, program_name};
use crate::core::status::{AgentStatus, DotFill, dot_fill};
use crate::core::style::{motion, radius, space, text};
use crate::core::target::Target;
use fleet_client::names::name_of;

/// The key context while the peek has the keyboard.
pub const PEEK_CONTEXT: &str = "Peek";

/// The title strip's height.
const TITLE_H: f32 = 28.0;
/// What the pane draws around its grid when headless: its side margins,
/// bottom margin and border, plus a pixel so the last column fits.
pub(super) const PAD_W: f32 = 2.0 * space::SM + 3.0;
pub(super) const PAD_H: f32 = space::SM - space::XXS + 3.0;

/// What the peek asks the workspace to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeekEvent {
    /// Escape or a click outside.
    Close,
    /// ⌘Return: show the agent in the focused pane.
    OpenInPane(u64),
}

pub struct PeekView {
    app: Entity<AppState>,
    agent: u64,
    pub(crate) pane: Entity<PaneView>,
}

impl EventEmitter<PeekEvent> for PeekView {}

impl PeekView {
    /// Opens the peek's connection on `agent` (closing any other peek).
    pub fn new(app: Entity<AppState>, agent: u64, cx: &mut Context<Self>) -> Self {
        let pane = app.update(cx, |a, cx| a.open_peek(agent, cx));
        PeekView { app, agent, pane }
    }

    pub fn agent(&self) -> u64 {
        self.agent
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.pane.read(cx).focus_handle().clone()
    }

    /// Gives the peek the keyboard.
    pub fn focus(&self, window: &mut Window, cx: &App) {
        window.focus(&self.focus_handle(cx));
    }

    /// Escape and ⌘Return, before the pane types them to the agent.
    fn key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        match peek_key(&ev.keystroke.key, m.platform, m.control, m.alt, m.shift) {
            PeekKey::Close => cx.emit(PeekEvent::Close),
            PeekKey::OpenInPane => cx.emit(PeekEvent::OpenInPane(self.agent)),
            PeekKey::Type => return,
        }
        cx.stop_propagation();
    }
}

/// A headless pane's header facts for `id`: the peek's and the graph
/// panel's (ADR 0105), which draw their own title strip from them.
pub(super) fn chrome_of(app: &AppState, id: u64) -> PaneChrome {
    let row = app.agents.iter().find(|r| r.info.id == id);
    let status = app.statuses.get(&id).copied();
    PaneChrome {
        number: 0,
        has_agent: true,
        agent_ended: row.is_some_and(ended),
        program: row.map(|r| program_name(&r.info.program)),
        name: row.and_then(|r| name_of(&r.info)).map(str::to_owned),
        link_dir: row
            .filter(|_| app.target == Target::Local)
            .map(|r| r.info.cwd.clone()),
        focused: true,
        status,
        status_fill: row
            .zip(status)
            .map_or(DotFill::Hollow, |(r, s)| dot_fill(&r.info, s)),
        agent: Some(id),
        local: app.target == Target::Local,
        ..PaneChrome::default()
    }
}

impl Render for PeekView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let chrome = chrome_of(self.app.read(cx), self.agent);
        let title = chrome
            .name
            .clone()
            .or_else(|| chrome.program.clone())
            .unwrap_or_else(|| format!("agent {}", self.agent));
        let status = chrome.status;
        let fill = chrome.status_fill;
        // The agent's grid (an observer never resizes it) at a font that
        // fits the window.
        let base = self.app.read(cx).pane_font_size();
        let (grid, cell) = {
            let pane = self.pane.read(cx);
            let grid = lock(pane.model_handle()).terminal().size();
            (grid, pane.cell_size(base, window))
        };
        let viewport = window.viewport_size();
        let room = (
            f32::from(viewport.width) - PAD_W,
            f32::from(viewport.height) - TITLE_H - PAD_H,
        );
        let f = fit(grid, cell, base, room);
        self.pane.update(cx, |p, cx| {
            p.set_font_size(f.font, cx);
            p.set_chrome(chrome, cx);
        });
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let hint = |keys: &'static str, what: &'static str| {
            div()
                .flex()
                .flex_row()
                .gap(px(space::XXS))
                .child(div().text_color(theme.text).child(keys))
                .child(what)
        };
        let title_bar = div()
            .h(px(TITLE_H))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SM))
            .px(px(space::SM))
            .type_style(text::CALLOUT)
            .text_color(theme.secondary)
            .when_some(status, |d, s: AgentStatus| {
                d.child(status_dot(s, fill, &theme, now_ms, true))
            })
            .child(
                div()
                    .text_color(theme.text)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(format!("#{}", self.agent))
            .child(div().flex_1())
            .child(hint("⌘⏎", "Open in Pane"))
            .child(hint("esc", "Close"));
        div()
            .id("peek")
            .debug_selector(|| "peek".to_owned())
            .key_context(PEEK_CONTEXT)
            .capture_key_down(cx.listener(Self::key_down))
            // A click inside is not a click outside.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .flex()
            .flex_col()
            .rounded(px(radius::LG + 2.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.raised)
            .shadow_lg()
            .overflow_hidden()
            .child(title_bar)
            .child(
                div()
                    .w(px(f.width + PAD_W))
                    .h(px(f.height + PAD_H))
                    .child(self.pane.clone()),
            )
    }
}

/// The peek over the window, centred; a click beside it closes it.
pub fn render_over(peek: &Entity<PeekView>, reduce_motion: bool) -> AnyElement {
    let target = peek.clone();
    deferred(
        div()
            .id("peek-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                target.update(cx, |_, cx| cx.emit(PeekEvent::Close));
            })
            .child(appear(
                div().child(peek.clone()),
                "peek-appear",
                -motion::SHEET_OFFSET,
                motion::SHEET_MS,
                reduce_motion,
            )),
    )
    .with_priority(2)
    .into_any_element()
}
