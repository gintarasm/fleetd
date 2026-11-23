//! The question asked before an agent is killed (issue #27): "Kill agent
//! 7 (claude, ~/fleetd)?", or "Kill shell 4 (zsh, ~/src)?" (ADR 0071) with Kill / Cancel and a "Don't ask again"
//! checkbox, which turns `[gui] confirm_kill` off.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Window, deferred, div, px,
};

use super::app_state::AppState;
use super::controls::{
    ButtonKind, appear, button, move_focus, return_is_not_a_click, sheet_card, tab_stop,
};
use super::icons;
use super::theme::{Typography, current as theme};
use super::titlebar_view::sheet_drag_strip;
use crate::core::icons::Icon;
use crate::core::kill::kill_question;
use crate::core::sidebar::{program_name, tilde};
use crate::core::status::Subject;
use crate::core::style::{chrome, motion, radius, space, text};

/// The sheet's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillEvent {
    /// Kill; `dont_ask`: the checkbox was checked.
    Kill {
        dont_ask: bool,
    },
    Cancel,
}

pub struct KillSheet {
    pub(crate) agent_id: u64,
    pub(crate) question: SharedString,
    pub(crate) dont_ask: bool,
    focus: FocusHandle,
}

impl EventEmitter<KillEvent> for KillSheet {}

/// The question for `agent_id`, from the agent list.
pub fn question(app: &AppState, agent_id: u64) -> String {
    let row = app.agents.iter().find(|r| r.info.id == agent_id);
    let program = row.map(|r| program_name(&r.info.program));
    let cwd = row.map(|r| tilde(&r.info.cwd, &app.home));
    let subject = row.map_or(Subject::Agent, |r| Subject::of(&r.info));
    kill_question(subject, agent_id, program.as_deref(), cwd.as_deref())
}

impl KillSheet {
    pub fn new(agent_id: u64, question: String, cx: &mut Context<Self>) -> Self {
        KillSheet {
            agent_id,
            question: question.into(),
            dont_ask: false,
            focus: cx.focus_handle(),
        }
    }

    /// Takes the keyboard: Return kills, Escape cancels.
    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus);
    }

    fn answer(&mut self, event: KillEvent, cx: &mut Context<Self>) {
        cx.emit(event);
    }

    fn kill(&mut self, cx: &mut Context<Self>) {
        let dont_ask = self.dont_ask;
        self.answer(KillEvent::Kill { dont_ask }, cx);
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        self.dont_ask = !self.dont_ask;
        cx.notify();
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if move_focus(ev, window) {
            cx.stop_propagation();
            return;
        }
        match ev.keystroke.key.as_str() {
            "enter" => self.kill(cx),
            "escape" => self.answer(KillEvent::Cancel, cx),
            // A focused control takes Space itself (ADR 0101).
            "space" if self.focus.is_focused(window) => self.toggle(cx),
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl Render for KillSheet {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let checked = self.dont_ask;
        let box_ = div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(14.))
            .rounded(px(radius::SM))
            .border_1()
            .border_color(if checked { theme.accent } else { theme.border })
            .when(checked, |b| {
                b.bg(theme.accent)
                    .child(icons::sized(Icon::Check, 11.0).text_color(gpui::white()))
            })
            .when(!checked, |b| b.bg(theme.field));
        let checkbox = tab_stop(div().id("dont-ask"), &theme)
            .debug_selector(|| "kill-dont-ask".to_owned())
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SM))
            .cursor_pointer()
            .type_style(text::CALLOUT)
            .child(box_)
            .child("Don't ask again")
            .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)));
        return_is_not_a_click(sheet_card("kill-sheet", 380.0, &theme))
            .debug_selector(|| "kill-sheet".to_owned())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            // Clicks and the wheel stop here, never reaching the panes.
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .type_style(text::HEADLINE)
                    .child(self.question.clone()),
            )
            .child(
                div()
                    .type_style(text::CALLOUT)
                    .text_color(theme.secondary)
                    .child("Its process is stopped. Closing a pane only detaches."),
            )
            .child(checkbox)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(space::SM))
                    .pt(px(space::XS))
                    .child(
                        tab_stop(
                            button("cancel", "Cancel", &theme, ButtonKind::Secondary, true),
                            &theme,
                        )
                        .debug_selector(|| "kill-cancel".to_owned())
                        .on_click(cx.listener(|this, _, _, cx| this.answer(KillEvent::Cancel, cx))),
                    )
                    .child(
                        tab_stop(
                            button("kill", "Kill", &theme, ButtonKind::Primary, true),
                            &theme,
                        )
                        .debug_selector(|| "kill-confirm".to_owned())
                        .on_click(cx.listener(|this, _, _, cx| this.kill(cx))),
                    ),
            )
    }
}

/// The sheet over the window, hung from the bar; the backdrop takes every
/// click and wheel event over the panes.
pub fn render_over(sheet: &Entity<KillSheet>, reduce_motion: bool) -> AnyElement {
    deferred(
        div()
            .id("kill-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(chrome::BAR_HEIGHT + space::SM))
            .child(sheet_drag_strip("kill-drag-strip"))
            .child(appear(
                div().child(sheet.clone()),
                "kill-appear",
                -motion::SHEET_OFFSET,
                motion::SHEET_MS,
                reduce_motion,
            )),
    )
    .with_priority(2)
    .into_any_element()
}

/// The list says `agent_id` is running (Stop and ⌘⇧W act only then).
pub fn running(app: &AppState, agent_id: u64) -> bool {
    app.agents
        .iter()
        .any(|r| r.info.id == agent_id && !crate::core::sidebar::ended(r))
}
