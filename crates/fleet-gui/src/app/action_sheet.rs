//! The action sheet (ADR 0094): Run Action…'s list of the daemon's
//! actions and the form rendered from the picked action's schema
//! (`core::action_form`), a confirmation (Archive Worktree…, a destructive
//! action), and a refusal with its "… Anyway". The workspace runs the
//! action and puts the sheet in the stage its result asks for.

use std::collections::HashMap;

use fleet_proto::{ActionSpec, ParamValue};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, EventEmitter, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, deferred, div, px,
};

use super::context_menu::{self, MenuLine};
use super::controls::{
    ButtonKind, appear, button, check_box, error_text, list_row, list_well, move_focus,
    return_is_not_a_click, sheet_card, tab_stop,
};
use super::text_field::TextField;
use super::theme::{Theme, Typography, current as theme};
use super::titlebar_view::sheet_drag_strip;
use crate::core::action_form::{ActionForm, FieldError, Input, with_retry};
use crate::core::style::{chrome, motion, radius, space, text};

/// The sheet's answer.
#[derive(Clone, Debug, PartialEq)]
pub enum ActionSheetEvent {
    /// Run the action with these params.
    Run {
        name: String,
        params: HashMap<String, ParamValue>,
    },
    Cancel,
}

/// What the sheet shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Stage {
    /// Run Action…: pick one of these.
    Choose(Vec<ActionSpec>),
    /// The picked action's form.
    Form,
    /// A question before running with `params` (Archive Worktree…, or a
    /// destructive action's form).
    Confirm {
        message: String,
        params: HashMap<String, ParamValue>,
    },
    /// The action is running.
    Running,
    /// The action refused: `reason`, and the params "… Anyway" resends.
    Refused {
        reason: String,
        params: HashMap<String, ParamValue>,
    },
}

/// A field's control.
enum Control {
    Text(Entity<TextField>),
    Check,
    /// A pop-up of these values (titles shown).
    Choice(Vec<(String, String)>),
}

pub struct ActionSheet {
    pub stage: Stage,
    /// The action (empty while choosing).
    pub name: String,
    pub title: String,
    /// The verb of the run button ("Run", "Archive").
    verb: String,
    pub form: Option<ActionForm>,
    controls: Vec<Control>,
    errors: Vec<FieldError>,
    /// The last failure (not a refusal).
    pub error: Option<String>,
    home: String,
    /// Agent choices for `agent` fields: id and name.
    agents: Vec<(u64, String)>,
    /// The agent an agent's context menu fills in.
    target: Option<u64>,
    focus: FocusHandle,
}

impl EventEmitter<ActionSheetEvent> for ActionSheet {}

impl ActionSheet {
    fn empty(home: String, agents: Vec<(u64, String)>, cx: &mut Context<Self>) -> Self {
        ActionSheet {
            stage: Stage::Running,
            name: String::new(),
            title: String::new(),
            verb: "Run".into(),
            form: None,
            controls: Vec::new(),
            errors: Vec::new(),
            error: None,
            home,
            agents,
            target: None,
            focus: cx.focus_handle(),
        }
    }

    /// Run Action…: the list of `actions`.
    pub fn choose(
        actions: Vec<ActionSpec>,
        home: String,
        agents: Vec<(u64, String)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut s = Self::empty(home, agents, cx);
        s.title = "Run Action".into();
        s.stage = Stage::Choose(actions);
        s
    }

    /// A question before running `name` with `params` (Archive Worktree…).
    pub fn confirm(
        name: String,
        title: String,
        message: String,
        verb: String,
        params: HashMap<String, ParamValue>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut s = Self::empty(String::new(), Vec::new(), cx);
        s.name = name;
        s.title = title;
        s.verb = verb;
        s.stage = Stage::Confirm { message, params };
        s
    }

    /// A refusal of `name` run with `params` (from the New agent sheet):
    /// `reason`, and "Run Anyway" with `retry` merged in.
    pub fn refused(
        name: String,
        title: String,
        reason: String,
        params: HashMap<String, ParamValue>,
        retry: &HashMap<String, ParamValue>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut s = Self::empty(String::new(), Vec::new(), cx);
        s.name = name;
        s.title = title;
        s.refuse(reason, params, retry, cx);
        s
    }

    /// The form for `spec`, its target param filled with `target`.
    pub fn pick(&mut self, spec: &ActionSpec, target: Option<u64>, cx: &mut Context<Self>) {
        let mut form = ActionForm::new(spec);
        if let Some(id) = target {
            form.target(spec, id);
        }
        self.target = target;
        self.name = form.name.clone();
        self.title = form.title.clone();
        self.controls = form
            .fields
            .iter()
            .map(|f| match &f.input {
                Input::Check => Control::Check,
                Input::Choice(c) => {
                    let mut values: Vec<_> = c.iter().map(|v| (v.clone(), v.clone())).collect();
                    if !f.required {
                        values.insert(0, (String::new(), "—".into()));
                    }
                    Control::Choice(values)
                }
                Input::Agent => {
                    let mut values: Vec<_> = self
                        .agents
                        .iter()
                        .map(|(id, name)| (id.to_string(), format!("{name} · {id}")))
                        .collect();
                    if !f.required {
                        values.insert(0, (String::new(), "—".into()));
                    }
                    Control::Choice(values)
                }
                Input::Lines => {
                    let (t, p) = (f.text.clone(), f.placeholder.clone());
                    Control::Text(cx.new(|cx| TextField::multiline(t, &p, 4, cx).tab_stop()))
                }
                Input::Line | Input::Folder => {
                    let (t, p) = (f.text.clone(), f.placeholder.clone());
                    Control::Text(cx.new(|cx| TextField::new(t, &p, cx).tab_stop()))
                }
            })
            .collect();
        self.form = Some(form);
        self.errors.clear();
        self.error = None;
        self.verb = "Run".into();
        self.stage = Stage::Form;
        cx.notify();
    }

    /// The action is running: the buttons wait.
    pub fn running(&mut self, cx: &mut Context<Self>) {
        self.stage = Stage::Running;
        self.error = None;
        cx.notify();
    }

    /// The action refused: shows `reason`; "… Anyway" resends `params`
    /// with `retry`.
    pub fn refuse(
        &mut self,
        reason: String,
        params: HashMap<String, ParamValue>,
        retry: &HashMap<String, ParamValue>,
        cx: &mut Context<Self>,
    ) {
        self.stage = Stage::Refused {
            reason,
            params: with_retry(params, retry),
        };
        cx.notify();
    }

    /// The action failed: the form (or question) again, with `error`.
    pub fn failed(
        &mut self,
        error: String,
        params: HashMap<String, ParamValue>,
        cx: &mut Context<Self>,
    ) {
        self.stage = if self.form.is_some() {
            Stage::Form
        } else {
            Stage::Confirm {
                message: String::new(),
                params,
            }
        };
        self.error = Some(error);
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        match self.controls.iter().find_map(|c| match c {
            Control::Text(t) if self.stage == Stage::Form => Some(t),
            _ => None,
        }) {
            Some(t) => window.focus(t.read(cx).focus_handle()),
            None => window.focus(&self.focus),
        }
    }

    /// Sets field `key` as if typed or chosen (tests).
    pub fn set_field(&mut self, key: &str, value: &str, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else { return };
        let Some(i) = form.field(key) else { return };
        match &self.controls[i] {
            Control::Text(t) => t.update(cx, |t, cx| t.set_text(value.to_owned(), cx)),
            Control::Check => form.fields[i].checked = value == "true",
            Control::Choice(_) => form.fields[i].text = value.to_owned(),
        }
        cx.notify();
    }

    /// What the form would send, or its errors (shown).
    fn form_params(&mut self, cx: &App) -> Option<HashMap<String, ParamValue>> {
        let form = self.form.as_mut()?;
        for (f, c) in form.fields.iter_mut().zip(&self.controls) {
            if let Control::Text(t) = c {
                f.text = t.read(cx).text().to_owned();
            }
        }
        match form.params(&self.home) {
            Ok(p) => {
                self.errors.clear();
                Some(p)
            }
            Err(e) => {
                self.errors = e;
                None
            }
        }
    }

    /// The run button (or Return).
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let params = match &self.stage {
            Stage::Form => {
                let Some(params) = self.form_params(cx) else {
                    cx.notify();
                    return;
                };
                if self.form.as_ref().is_some_and(|f| f.destructive) {
                    self.stage = Stage::Confirm {
                        message: format!("Run {}? It cannot be undone.", self.title),
                        params,
                    };
                    cx.notify();
                    return;
                }
                params
            }
            Stage::Confirm { params, .. } | Stage::Refused { params, .. } => params.clone(),
            Stage::Choose(_) | Stage::Running => return,
        };
        cx.emit(ActionSheetEvent::Run {
            name: self.name.clone(),
            params,
        });
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(ActionSheetEvent::Cancel);
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if move_focus(ev, window) {
            cx.stop_propagation();
            return;
        }
        match ev.keystroke.key.as_str() {
            "enter" if !matches!(self.stage, Stage::Choose(_)) => self.submit(cx),
            "escape" => self.cancel(cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn pop_up(&mut self, i: usize, cx: &mut Context<Self>) {
        let (Some(form), Some(Control::Choice(values))) = (&self.form, self.controls.get(i)) else {
            return;
        };
        let current = form.fields[i].text.clone();
        let lines = values
            .iter()
            .map(|(v, title)| {
                Some(MenuLine {
                    title: title.clone(),
                    enabled: true,
                    shortcut: None,
                    checked: *v == current,
                })
            })
            .collect();
        let values: Vec<String> = values.iter().map(|(v, _)| v.clone()).collect();
        let this = cx.entity().downgrade();
        context_menu::show(lines, cx, move |pick, cx| {
            if let (Some(v), Some(this)) = (values.get(pick), this.upgrade()) {
                let v = v.clone();
                this.update(cx, |s, cx| {
                    if let Some(f) = s.form.as_mut().and_then(|f| f.fields.get_mut(i)) {
                        f.text = v;
                    }
                    cx.notify();
                });
            }
        });
    }

    fn render_field(&self, i: usize, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let form = self.form.as_ref().expect("a form");
        let f = &form.fields[i];
        let t = *theme;
        let label = div()
            .type_style(text::SUBHEADLINE)
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.secondary)
            .child(SharedString::from(if f.required {
                format!("{} *", f.title)
            } else {
                f.title.clone()
            }));
        let control: AnyElement = match &self.controls[i] {
            Control::Text(field) => field.clone().into_any_element(),
            Control::Check => {
                let checked = f.checked;
                tab_stop(div().id(("check", i)), theme)
                    .debug_selector(move || format!("action-field-{i}"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::SM))
                    .cursor_pointer()
                    .type_style(text::CALLOUT)
                    .child(check_box(checked, theme))
                    .child(SharedString::from(f.title.clone()))
                    .on_click(cx.listener(move |s, _, _, cx| {
                        if let Some(f) = s.form.as_mut().and_then(|f| f.fields.get_mut(i)) {
                            f.checked = !f.checked;
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Control::Choice(values) => {
                let shown = values
                    .iter()
                    .find(|(v, _)| *v == f.text)
                    .map_or_else(|| f.text.clone(), |(_, title)| title.clone());
                tab_stop(div().id(("choice", i)), theme)
                    .debug_selector(move || format!("action-field-{i}"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .h(px(chrome::CONTROL_HEIGHT))
                    .px(px(space::SM))
                    .rounded(px(radius::MD))
                    .border_1()
                    .border_color(t.border)
                    .bg(t.field)
                    .type_style(text::BODY)
                    .cursor_pointer()
                    .child(SharedString::from(shown))
                    .child(div().text_color(t.tertiary).child("\u{2304}"))
                    .on_click(cx.listener(move |s, _, _, cx| s.pop_up(i, cx)))
                    .into_any_element()
            }
        };
        let error = self
            .errors
            .iter()
            .find(|e| e.field == i)
            .map(|e| error_text(SharedString::from(e.message.clone()), theme));
        div()
            .flex()
            .flex_col()
            .gap(px(space::XXS))
            .when(!matches!(self.controls[i], Control::Check), |d| {
                d.child(label)
            })
            .child(control)
            .when(!f.help.is_empty(), |d| {
                d.child(
                    div()
                        .type_style(text::SUBHEADLINE)
                        .text_color(theme.tertiary)
                        .child(SharedString::from(f.help.clone())),
                )
            })
            .children(error)
            .into_any_element()
    }

    fn buttons(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (verb, enabled) = match &self.stage {
            Stage::Refused { .. } => (format!("{} Anyway", self.verb), true),
            Stage::Running => ("Running…".to_owned(), false),
            _ => (self.verb.clone(), true),
        };
        let choosing = matches!(self.stage, Stage::Choose(_));
        div()
            .flex()
            .flex_row()
            .justify_end()
            .gap(px(space::SM))
            .pt(px(space::XS))
            .child(
                tab_stop(
                    button("cancel", "Cancel", theme, ButtonKind::Secondary, true),
                    theme,
                )
                .debug_selector(|| "action-cancel".to_owned())
                .on_click(cx.listener(|s, _, _, cx| s.cancel(cx))),
            )
            .when(!choosing, |d| {
                d.child(
                    tab_stop(
                        button("run", verb, theme, ButtonKind::Primary, enabled),
                        theme,
                    )
                    .debug_selector(|| "action-run".to_owned())
                    .on_click(cx.listener(|s, _, _, cx| s.submit(cx))),
                )
            })
    }
}

impl Render for ActionSheet {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let body: AnyElement = match &self.stage {
            Stage::Choose(actions) => {
                let rows = actions.iter().enumerate().map(|(i, a)| {
                    let spec = a.clone();
                    let title = if a.title.is_empty() {
                        a.name.clone()
                    } else {
                        a.title.clone()
                    };
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            list_row(("action", i), title, &theme, true)
                                .debug_selector(move || format!("action-choice-{i}"))
                                .on_click(cx.listener(move |s, _, window, cx| {
                                    let target = s.target;
                                    s.pick(&spec, target, cx);
                                    s.focus(window, cx);
                                })),
                        )
                        .child(
                            div()
                                .px(px(space::SM))
                                .type_style(text::SUBHEADLINE)
                                .text_color(theme.tertiary)
                                .truncate()
                                .child(SharedString::from(if a.description.is_empty() {
                                    a.name.clone()
                                } else {
                                    format!("{} · {}", a.name, a.description)
                                })),
                        )
                });
                list_well("action-list", 360.0, &theme)
                    .children(rows)
                    .into_any_element()
            }
            Stage::Form => {
                let fields: Vec<AnyElement> = (0..self.controls.len())
                    .map(|i| self.render_field(i, &theme, cx))
                    .collect();
                let description = self
                    .form
                    .as_ref()
                    .map(|f| f.description.clone())
                    .filter(|d| !d.is_empty());
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::MD))
                    .children(description.map(|d| {
                        div()
                            .type_style(text::CALLOUT)
                            .text_color(theme.secondary)
                            .child(SharedString::from(d))
                    }))
                    .children(fields)
                    .into_any_element()
            }
            Stage::Confirm { message, .. } => div()
                .type_style(text::CALLOUT)
                .text_color(theme.secondary)
                .child(SharedString::from(message.clone()))
                .into_any_element(),
            Stage::Refused { reason, .. } => div()
                .debug_selector(|| "action-refused".to_owned())
                .type_style(text::CALLOUT)
                .text_color(theme.text)
                .child(SharedString::from(reason.clone()))
                .into_any_element(),
            Stage::Running => div()
                .type_style(text::CALLOUT)
                .text_color(theme.secondary)
                .child("Running…")
                .into_any_element(),
        };
        let buttons = self.buttons(&theme, cx);
        return_is_not_a_click(sheet_card("action-sheet", 440.0, &theme))
            .debug_selector(|| "action-sheet".to_owned())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .type_style(text::TITLE)
                    .child(SharedString::from(self.title.clone())),
            )
            .child(body)
            .children(
                self.error
                    .clone()
                    .map(|e| error_text(SharedString::from(e), &theme)),
            )
            .child(buttons)
    }
}

/// The sheet over the window, hung from the bar.
pub fn render_over(sheet: &Entity<ActionSheet>, reduce_motion: bool) -> AnyElement {
    deferred(
        div()
            .id("action-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(chrome::BAR_HEIGHT + space::SM))
            .child(sheet_drag_strip("action-drag-strip"))
            .child(appear(
                div().child(sheet.clone()),
                "action-appear",
                -motion::SHEET_OFFSET,
                motion::SHEET_MS,
                reduce_motion,
            )),
    )
    .with_priority(2)
    .into_any_element()
}
