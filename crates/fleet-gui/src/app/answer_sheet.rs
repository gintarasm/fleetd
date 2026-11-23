//! The Answer sheet (ADR 0078): an agent's open question with its choices
//! as buttons and a free-text field. A choice sends at once; Return or
//! Send sends the text; Escape cancels.

use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window,
    div, px,
};

use super::controls::{
    ButtonKind, button, error_text, field_label, move_focus, return_is_not_a_click, sheet_card,
    sheet_title, tab_stop,
};
use super::text_field::TextField;
use super::theme::{Typography, current as theme};
use crate::core::question::{OpenQuestion, check_answer};
use crate::core::style::{space, text};

pub enum AnswerEvent {
    Send {
        agent_id: u64,
        question: u64,
        text: String,
    },
    Cancel,
}

pub struct AnswerSheet {
    pub(crate) agent_id: u64,
    /// What the agent is, under the title (`claude · 4`).
    what: String,
    pub(crate) question: OpenQuestion,
    pub(crate) reply: Entity<TextField>,
    pub(crate) error: Option<String>,
}

impl EventEmitter<AnswerEvent> for AnswerSheet {}

impl AnswerSheet {
    pub fn new(
        agent_id: u64,
        what: String,
        question: OpenQuestion,
        cx: &mut Context<Self>,
    ) -> Self {
        AnswerSheet {
            agent_id,
            what,
            question,
            reply: cx.new(|cx| TextField::new(String::new(), "Your answer", cx).tab_stop()),
            error: None,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        window.focus(self.reply.read(cx).focus_handle());
    }

    fn send_text(&mut self, text: String, cx: &mut Context<Self>) {
        cx.emit(AnswerEvent::Send {
            agent_id: self.agent_id,
            question: self.question.id,
            text,
        });
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        match check_answer(self.reply.read(cx).text()) {
            Ok(text) => self.send_text(text, cx),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(AnswerEvent::Cancel);
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if move_focus(ev, window) {
            cx.stop_propagation();
            return;
        }
        match ev.keystroke.key.as_str() {
            "enter" => {
                self.submit(cx);
                cx.stop_propagation();
            }
            "escape" => {
                self.cancel(cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }
}

impl Render for AnswerSheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let choices: Vec<_> = self
            .question
            .choices
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let choice = c.clone();
                tab_stop(
                    button(
                        ("answer-choice", i),
                        SharedString::from(c.clone()),
                        &theme,
                        ButtonKind::Secondary,
                        true,
                    ),
                    &theme,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.send_text(choice.clone(), cx)))
            })
            .collect();
        return_is_not_a_click(sheet_card("answer-sheet", 420.0, &theme))
            .debug_selector(|| "answer-sheet".to_owned())
            .on_key_down(cx.listener(Self::key_down))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(sheet_title("Answer"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::XS))
                    .child(
                        div()
                            .type_style(text::BODY)
                            .text_color(theme.text)
                            .child(SharedString::from(self.question.text.clone())),
                    )
                    .child(
                        div()
                            .type_style(text::SUBHEADLINE)
                            .text_color(theme.tertiary)
                            .child(SharedString::from(self.what.clone())),
                    ),
            )
            .when_some((!choices.is_empty()).then_some(choices), |d, choices| {
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(space::XS))
                        .children(choices),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::SM - space::XXS))
                    .child(field_label("Or answer in your own words", &theme))
                    .child(self.reply.clone()),
            )
            .children(self.error.clone().map(|e| error_text(e, &theme)))
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
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        tab_stop(
                            button("send-answer", "Send", &theme, ButtonKind::Primary, true),
                            &theme,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            )
    }
}
