//! The Edit Labels sheet (issue #99, ADR 0087): one `key=value` field per
//! label, each with a remove button, and Add Label; the name and the
//! core's `fleet.*` labels are listed read-only. Return saves, Escape
//! cancels.

use std::collections::HashMap;

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window,
    div, px,
};

use super::controls::{ButtonKind, button, error_text, field_label, sheet_card, sheet_title};
use super::icons;
use super::text_field::TextField;
use super::theme::{Typography, current as theme};
use crate::core::icons::Icon;
use crate::core::labels::{LabelEdit, editable_labels, label_edit, read_only_labels};
use crate::core::style::{chrome, radius, space, text};

pub enum LabelsEvent {
    Save { agent_id: u64, edit: LabelEdit },
    Cancel,
}

pub struct LabelsSheet {
    pub(crate) agent_id: u64,
    /// What the agent is, under the title (`otter · claude · 4`).
    what: String,
    /// The agent's labels when the sheet opened: what a save compares to.
    old: HashMap<String, String>,
    /// The name and the `fleet.*` labels, shown as `key=value`.
    read_only: Vec<String>,
    pub(crate) rows: Vec<Entity<TextField>>,
    pub(crate) error: Option<String>,
}

impl EventEmitter<LabelsEvent> for LabelsSheet {}

impl LabelsSheet {
    pub fn new(
        agent_id: u64,
        labels: HashMap<String, String>,
        what: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut rows: Vec<_> = editable_labels(&labels)
            .into_iter()
            .map(|(k, v)| new_row(format!("{k}={v}"), cx))
            .collect();
        if rows.is_empty() {
            rows.push(new_row(String::new(), cx));
        }
        LabelsSheet {
            agent_id,
            what,
            read_only: read_only_labels(&labels)
                .into_iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect(),
            old: labels,
            rows,
            error: None,
        }
    }

    /// Focus goes to the first row.
    pub fn focus(&self, window: &mut Window, cx: &App) {
        if let Some(row) = self.rows.first() {
            window.focus(row.read(cx).focus_handle());
        }
    }

    pub fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = new_row(String::new(), cx);
        window.focus(row.read(cx).focus_handle());
        self.rows.push(row);
        cx.notify();
    }

    pub fn remove_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.rows.len() {
            self.rows.remove(index);
            cx.notify();
        }
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let rows: Vec<String> = self
            .rows
            .iter()
            .map(|r| r.read(cx).text().to_owned())
            .collect();
        match label_edit(&self.old, &rows) {
            Ok(edit) => cx.emit(LabelsEvent::Save {
                agent_id: self.agent_id,
                edit,
            }),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(LabelsEvent::Cancel);
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

fn new_row(text: String, cx: &mut Context<LabelsSheet>) -> Entity<TextField> {
    cx.new(|cx| TextField::new(text, "key=value", cx))
}

impl Render for LabelsSheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let rows = self.rows.iter().enumerate().map(|(i, field)| {
            let remove = div()
                .id(("label-remove", i))
                .debug_selector(move || format!("label-remove-{i}"))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(chrome::ICON_BUTTON))
                .rounded(px(radius::MD))
                .cursor_pointer()
                .text_color(theme.secondary)
                .hover(move |s| s.bg(theme.hover).text_color(theme.text))
                .active(move |s| s.bg(theme.pressed))
                .child(icons::icon(Icon::Close))
                .on_click(cx.listener(move |this, _, _, cx| this.remove_row(i, cx)));
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::XS))
                .child(div().flex_1().child(field.clone()))
                .child(remove)
        });
        let read_only = (!self.read_only.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(space::XXS))
                .child(field_label("Read-only", &theme))
                .children(self.read_only.iter().map(|l| {
                    div()
                        .type_style(text::CALLOUT)
                        .text_color(theme.tertiary)
                        .truncate()
                        .child(SharedString::from(l.clone()))
                }))
                .child(
                    div()
                        .type_style(text::SUBHEADLINE)
                        .text_color(theme.tertiary)
                        .child("The name changes with Rename…; fleet.* labels are fleetd's."),
                )
        });
        sheet_card("labels-sheet", 420.0, &theme)
            .debug_selector(|| "labels-sheet".to_owned())
            .on_key_down(cx.listener(Self::key_down))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(sheet_title("Edit labels"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::SM - space::XXS))
                    .child(field_label("Labels", &theme))
                    .children(rows)
                    .child(
                        div().flex().flex_row().child(
                            button(
                                "add-label",
                                "Add Label",
                                &theme,
                                ButtonKind::Secondary,
                                true,
                            )
                            .on_click(cx.listener(|this, _, window, cx| this.add_row(window, cx))),
                        ),
                    )
                    .child(
                        div()
                            .type_style(text::SUBHEADLINE)
                            .text_color(theme.tertiary)
                            .child(SharedString::from(self.what.clone())),
                    ),
            )
            .children(read_only)
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
                        button("save", "Save", &theme, ButtonKind::Primary, true)
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            )
    }
}
