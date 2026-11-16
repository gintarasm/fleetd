//! The Send to Agents sheet (⇧⌘M, issue #95, ADR 0083): a message box,
//! the running agents as a checkbox list found by name, folder or label,
//! and "Press Return after". ⌘Return or Send sends; a target that is
//! working gets a warning first, and Send again sends anyway. Tab moves
//! between the message and the agent filter; Escape cancels.

use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, App, AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Window, deferred, div, px,
};

use super::app_state::AppState;
use super::controls::{
    ButtonKind, appear, button, check_box, error_text, field_label, sheet_card, sheet_title,
};
use super::picker_view::{PickerEvent, PickerView};
use super::text_field::TextField;
use super::theme::{Typography, current as theme};
use super::titlebar_view::sheet_drag_strip;
use crate::core::broadcast::{
    SendForm, SendPlan, Submit, agent_key, key_agent, send_items, send_title, sendable, target_name,
};
use crate::core::status::AgentStatus;
use crate::core::style::{chrome, motion, space, text};

/// The message box's height in rows.
const MESSAGE_ROWS: usize = 5;
/// The sheet's width.
const WIDTH: f32 = 520.0;

pub enum SendEvent {
    Send(SendPlan),
    Cancel,
}

pub struct SendSheet {
    app: Entity<AppState>,
    pub(crate) message: Entity<TextField>,
    pub(crate) picker: Entity<PickerView>,
    pub(crate) form: SendForm,
    /// Targets working: shown until Send again, or a change.
    pub(crate) warning: Option<String>,
    /// Why Send did nothing.
    pub(crate) error: Option<&'static str>,
    _picked: Subscription,
}

impl EventEmitter<SendEvent> for SendSheet {}

impl SendSheet {
    /// The sheet over `app`'s running agents, `preselect` checked (those
    /// running).
    pub fn new(app: Entity<AppState>, preselect: &[u64], cx: &mut Context<Self>) -> Self {
        let (items, running) = {
            let a = app.read(cx);
            let items = send_items(&a.agents, &a.statuses, &a.home);
            let running: HashSet<u64> = a
                .agents
                .iter()
                .filter(|r| sendable(r))
                .map(|r| r.info.id)
                .collect();
            (items, running)
        };
        let form = SendForm::new(preselect.iter().copied().filter(|id| running.contains(id)));
        let checked = form.selected().iter().map(|&id| agent_key(id)).collect();
        let picker = cx.new(|cx| PickerView::multi(items, "Filter agents", checked, cx));
        let picked = cx.subscribe(&picker, |this: &mut Self, _, event, cx| match event {
            PickerEvent::Pick(key) => {
                if let Some(id) = key_agent(key) {
                    this.toggle(id, cx);
                }
            }
            PickerEvent::Dismiss => cx.emit(SendEvent::Cancel),
        });
        SendSheet {
            app,
            message: cx.new(|cx| TextField::multiline(String::new(), "Message", MESSAGE_ROWS, cx)),
            picker,
            form,
            warning: None,
            error: None,
            _picked: picked,
        }
    }

    /// The message box takes the keyboard.
    pub fn focus(&self, window: &mut Window, cx: &App) {
        window.focus(self.message.read(cx).focus_handle());
    }

    /// Checks agent `id`, or unchecks it.
    pub fn toggle(&mut self, id: u64, cx: &mut Context<Self>) {
        self.form.toggle(id);
        self.warning = None;
        self.error = None;
        self.sync_checked(cx);
        cx.notify();
    }

    fn sync_checked(&mut self, cx: &mut Context<Self>) {
        let keys: HashSet<String> = self
            .form
            .selected()
            .iter()
            .map(|&id| agent_key(id))
            .collect();
        self.picker.update(cx, |p, cx| p.set_checked(keys, cx));
    }

    fn toggle_return(&mut self, cx: &mut Context<Self>) {
        self.form.toggle_return();
        self.error = None;
        cx.notify();
    }

    /// Send: sends, or says why not, or warns about working targets.
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.message.read(cx).text().to_owned();
        let (working, names, running) = {
            let a = self.app.read(cx);
            let working: HashSet<u64> = a
                .statuses
                .iter()
                .filter(|(_, s)| **s == AgentStatus::Working)
                .map(|(id, _)| *id)
                .collect();
            let names: HashMap<u64, String> = a
                .agents
                .iter()
                .map(|r| (r.info.id, target_name(r)))
                .collect();
            let running: HashSet<u64> = a
                .agents
                .iter()
                .filter(|r| sendable(r))
                .map(|r| r.info.id)
                .collect();
            (working, names, running)
        };
        // Agents that ended since the sheet opened are not typed into.
        self.form.retain(|id| running.contains(&id));
        self.sync_checked(cx);
        let name = |id: u64| names.get(&id).cloned().unwrap_or_else(|| id.to_string());
        match self.form.submit(&text, &working, name) {
            Submit::Nothing(why) => {
                self.error = Some(why);
                self.warning = None;
            }
            Submit::Warn(w) => {
                self.error = None;
                self.warning = Some(w);
            }
            Submit::Send(plan) => cx.emit(SendEvent::Send(plan)),
        }
        cx.notify();
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        match ev.keystroke.key.as_str() {
            "enter" if m.platform => self.submit(cx),
            "escape" => cx.emit(SendEvent::Cancel),
            "tab" if !m.platform && !m.control && !m.alt => {
                let message = self.message.read(cx).focus_handle().clone();
                if message.is_focused(window) {
                    let filter = self.picker.read(cx).query_field().clone();
                    window.focus(filter.read(cx).focus_handle());
                } else {
                    window.focus(&message);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl Render for SendSheet {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let n = self.form.selected().len();
        let press_return = self.form.press_return;
        let checkbox = div()
            .id("send-return")
            .debug_selector(|| "send-return".to_owned())
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SM))
            .cursor_pointer()
            .type_style(text::CALLOUT)
            .child(check_box(press_return, &theme))
            .child("Press Return after")
            .on_click(cx.listener(|this, _, _, cx| this.toggle_return(cx)));
        let picked = match n {
            0 => "none checked".to_owned(),
            n => format!("{n} checked"),
        };
        let warning = self.warning.clone().map(|w| {
            div()
                .debug_selector(|| "send-warning".to_owned())
                .type_style(text::CALLOUT)
                .text_color(theme.warning)
                .child(SharedString::from(w))
        });
        let title = send_title(n, self.form.warning_shown());
        sheet_card("send-sheet", WIDTH, &theme)
            .debug_selector(|| "send-sheet".to_owned())
            .on_key_down(cx.listener(Self::key_down))
            // Clicks and the wheel stop here, never reaching the panes.
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(sheet_title("Send to Agents"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::SM - space::XXS))
                    .child(field_label("Message", &theme))
                    .child(self.message.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::SM - space::XXS))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .child(field_label("Running agents", &theme))
                            .child(
                                div()
                                    .debug_selector(move || format!("send-checked-{n}"))
                                    .type_style(text::SUBHEADLINE)
                                    .text_color(theme.tertiary)
                                    .child(SharedString::from(picked)),
                            ),
                    )
                    .child(self.picker.clone()),
            )
            .child(checkbox)
            .children(warning)
            .children(self.error.map(|e| error_text(e, &theme)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::SM))
                    .pt(px(space::XS))
                    .child(
                        div()
                            .flex_1()
                            .type_style(text::SUBHEADLINE)
                            .text_color(theme.tertiary)
                            .child("⌘↩ sends · Tab: filter"),
                    )
                    .child(
                        button("cancel", "Cancel", &theme, ButtonKind::Secondary, true)
                            .debug_selector(|| "send-cancel".to_owned())
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SendEvent::Cancel))),
                    )
                    .child(
                        button("send", title, &theme, ButtonKind::Primary, true)
                            .debug_selector(|| "send-confirm".to_owned())
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            )
    }
}

/// The sheet over the window, hung from the bar; the backdrop takes every
/// click and wheel event over the panes.
pub fn render_over(sheet: &Entity<SendSheet>, reduce_motion: bool) -> AnyElement {
    deferred(
        div()
            .id("send-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(chrome::BAR_HEIGHT + space::SM))
            .child(sheet_drag_strip("send-drag-strip"))
            .child(appear(
                div().child(sheet.clone()),
                "send-appear",
                -motion::SHEET_OFFSET,
                motion::SHEET_MS,
                reduce_motion,
            )),
    )
    .with_priority(2)
    .into_any_element()
}
