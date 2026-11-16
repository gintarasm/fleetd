//! The split chooser's view (issue #30): drawn inside the empty pane a
//! split opened, in place of its "Select an agent" text. Keys 1/2/3 (or
//! ↑/↓ and Enter, or a click) pick, and 4 to 9 a preset's row; Esc closes the pane. Existing agent
//! swaps the choices for the [`PickerView`] over the agents off screen;
//! Esc there goes back to the choices.

use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Window, div, px,
};

use super::picker_view::{PickerEvent, PickerView};
use super::theme::{Typography, current as theme};
use crate::core::picker::{PickerItem, Recents};
use crate::core::split_chooser::{Choice, ChoiceRow, Outcome, SplitChooser, Stage};
use crate::core::style::{radius, space, text};

/// What the chooser asks the workspace to do with its pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChooserEvent {
    /// `[gui] default_command` in the chooser's folder.
    NewAgent,
    /// The preset at this index of `AppState::presets`, in the chooser's
    /// folder (ADR 0084).
    Preset(usize),
    /// A login shell in the chooser's folder.
    Terminal,
    /// Show this agent.
    Agent(u64),
    /// Close the pane: the split is cancelled.
    Cancel,
}

pub struct SplitChooserView {
    chooser: SplitChooser,
    /// The agents Existing agent offers, by picker key.
    agents: Vec<(u64, PickerItem)>,
    list: Option<(Entity<PickerView>, Subscription)>,
    focus: FocusHandle,
}

impl EventEmitter<ChooserEvent> for SplitChooserView {}

impl SplitChooserView {
    pub fn new(
        rows: Vec<ChoiceRow>,
        agents: Vec<(u64, PickerItem)>,
        cx: &mut Context<Self>,
    ) -> Self {
        SplitChooserView {
            chooser: SplitChooser::new(rows),
            agents,
            list: None,
            focus: cx.focus_handle(),
        }
    }

    pub fn chooser(&self) -> &SplitChooser {
        &self.chooser
    }

    /// The agent list, while it shows.
    pub fn agent_list(&self) -> Option<&Entity<PickerView>> {
        self.list.as_ref().map(|(l, _)| l)
    }

    /// Focus goes to the choices, or to the list's query.
    pub fn focus(&self, window: &mut Window, cx: &App) {
        match &self.list {
            Some((list, _)) => list.read(cx).focus(window, cx),
            None => window.focus(&self.focus),
        }
    }

    /// The chooser (or its list) has the keyboard.
    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
    }

    fn apply(&mut self, outcome: Outcome, window: &mut Window, cx: &mut Context<Self>) {
        match outcome {
            Outcome::Start(Choice::NewAgent) => cx.emit(ChooserEvent::NewAgent),
            Outcome::Start(Choice::Terminal) => cx.emit(ChooserEvent::Terminal),
            Outcome::Start(Choice::Preset(i)) => cx.emit(ChooserEvent::Preset(i)),
            Outcome::Start(Choice::Agent) | Outcome::Stay => {}
            Outcome::ShowAgents => self.open_list(window, cx),
            Outcome::Back => {
                self.list = None;
                window.focus(&self.focus);
            }
            Outcome::Cancel => cx.emit(ChooserEvent::Cancel),
        }
        cx.notify();
    }

    fn open_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.agents.iter().map(|(_, i)| i.clone()).collect();
        let list = cx.new(|cx| PickerView::new(items, Recents::default(), "Search agents", cx));
        let sub = cx.subscribe_in(&list, window, Self::list_event);
        list.read(cx).focus(window, cx);
        self.list = Some((list, sub));
    }

    fn list_event(
        &mut self,
        _list: &Entity<PickerView>,
        event: &PickerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PickerEvent::Dismiss => {
                let outcome = self.chooser.back();
                self.apply(outcome, window, cx);
            }
            PickerEvent::Pick(key) => {
                if let Some((id, _)) = self.agents.iter().find(|(_, i)| i.key == *key) {
                    cx.emit(ChooserEvent::Agent(*id));
                }
            }
        }
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        // The list's query takes its own keys (and their text).
        if m.platform || m.alt || m.control || self.chooser.stage() != Stage::Choices {
            return;
        }
        // Plain keys never reach the empty pane under the choices.
        cx.stop_propagation();
        if m.shift {
            return;
        }
        if let Some(outcome) = self.chooser.key(&ev.keystroke.key) {
            self.apply(outcome, window, cx);
        }
    }

    fn render_choice(
        &self,
        row: usize,
        choice: &ChoiceRow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = theme(cx);
        let selected = row == self.chooser.selected() && choice.enabled;
        let (fg, dim) = match (selected, choice.enabled) {
            (true, _) => (gpui::white(), gpui::white().opacity(0.8)),
            (false, true) => (t.text, t.secondary),
            (false, false) => (t.tertiary, t.tertiary),
        };
        let number = choice.choice.number();
        let pick = choice.choice;
        div()
            .id(("split-choice", number))
            .debug_selector(move || format!("split-choice-{number}"))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SM))
            .px(px(space::SM))
            .py(px(space::XS + space::XXS))
            .rounded(px(radius::MD))
            .text_color(fg)
            .when(selected, |r| r.bg(t.accent))
            .when(choice.enabled, |r| {
                r.cursor_pointer()
                    .when(!selected, |r| r.hover(move |s| s.bg(t.hover)))
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if this.chooser.selected() != row {
                            this.chooser.select(row);
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(outcome) = this.chooser.pick(pick) {
                            this.apply(outcome, window, cx);
                        }
                    }))
            })
            // The key, like a menu's shortcut but in front.
            .child(
                div()
                    .flex_none()
                    .w(px(space::LG + space::XS))
                    .h(px(space::LG + space::XS))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(radius::SM))
                    .border_1()
                    .border_color(if selected {
                        gpui::white().opacity(0.5)
                    } else {
                        t.border
                    })
                    .type_style(text::CALLOUT)
                    .text_color(dim)
                    .child(SharedString::from(number.to_string())),
            )
            .child(
                div()
                    .flex_none()
                    .child(SharedString::from(choice.title.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .type_style(text::CALLOUT)
                    .text_color(dim)
                    .child(SharedString::from(choice.detail.clone())),
            )
    }
}

impl Render for SplitChooserView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let body = match (&self.list, self.chooser.stage()) {
            (Some((list, _)), Stage::Agents) => {
                div().w_full().child(list.clone()).into_any_element()
            }
            _ => {
                let rows: Vec<_> = self
                    .chooser
                    .rows()
                    .to_vec()
                    .iter()
                    .enumerate()
                    .map(|(i, r)| self.render_choice(i, r, cx).into_any_element())
                    .collect();
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(space::XXS))
                    .p(px(space::SM))
                    .rounded(px(radius::LG))
                    .border_1()
                    .border_color(t.border)
                    .bg(t.raised)
                    .child(
                        div()
                            .px(px(space::SM))
                            .pb(px(space::XS))
                            .type_style(text::HEADLINE)
                            .text_color(t.text)
                            .child("Start in this pane"),
                    )
                    .children(rows)
                    .child(
                        div()
                            .px(px(space::SM))
                            .pt(px(space::XS))
                            .type_style(text::CAPTION)
                            .text_color(t.tertiary)
                            .child("esc closes the pane"),
                    )
                    .into_any_element()
            }
        };
        div()
            .id("split-chooser")
            .debug_selector(|| "split-chooser".to_owned())
            // The list's query is inside: it counts as the chooser's focus.
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .p(px(space::MD))
            .type_style(text::BODY)
            .text_color(t.text)
            // Clicks stay here, never reaching the grid under it.
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(div().w_full().max_w(px(420.)).child(body))
    }
}
