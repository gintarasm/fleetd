//! The Rename sheet (ADR 0052): one field with the agent's name; Return
//! renames, Escape cancels.

use fleet_client::names::check_name;
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window,
    div, px,
};

use super::controls::{ButtonKind, button, error_text, field_label, sheet_card, sheet_title};
use super::text_field::TextField;
use super::theme::{Typography, current as theme};
use crate::core::style::{space, text};

pub enum RenameEvent {
    Rename { agent_id: u64, name: String },
    Cancel,
}

pub struct RenameSheet {
    pub(crate) agent_id: u64,
    /// What the agent is, under the title (`claude · 4`).
    what: String,
    pub(crate) name: Entity<TextField>,
    pub(crate) error: Option<String>,
}

impl EventEmitter<RenameEvent> for RenameSheet {}

impl RenameSheet {
    pub fn new(agent_id: u64, name: String, what: String, cx: &mut Context<Self>) -> Self {
        RenameSheet {
            agent_id,
            what,
            name: cx.new(|cx| TextField::new(name, "Name", cx)),
            error: None,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        window.focus(self.name.read(cx).focus_handle());
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        match check_name(self.name.read(cx).text()) {
            Ok(name) => cx.emit(RenameEvent::Rename {
                agent_id: self.agent_id,
                name,
            }),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(RenameEvent::Cancel);
    }

    fn key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
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

impl Render for RenameSheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        sheet_card("rename-sheet", 380.0, &theme)
            .debug_selector(|| "rename-sheet".to_owned())
            .on_key_down(cx.listener(Self::key_down))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(sheet_title("Rename agent"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::SM - space::XXS))
                    .child(field_label("Name", &theme))
                    .child(self.name.clone())
                    .child(
                        div()
                            .type_style(text::SUBHEADLINE)
                            .text_color(theme.tertiary)
                            .child(SharedString::from(self.what.clone())),
                    ),
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
                        button("cancel", "Cancel", &theme, ButtonKind::Secondary, true)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        button("rename", "Rename", &theme, ButtonKind::Primary, true)
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            )
    }
}
