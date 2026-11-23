//! The workspace's side of workflows (ADR 0094): Show Changes, Run
//! Action…, Archive Worktree…, the New agent sheet's worktree spawn, and
//! what each action result does. Kept out of `workspace.rs`, which only
//! calls in here.

use std::collections::HashMap;

use fleet_proto::action_result::Outcome;
use fleet_proto::param_value::Value;
use fleet_proto::{ActionResult, ParamValue, RunAction};
use gpui::{AppContext, Context, Entity, Window};

use super::Workspace;
use crate::app::action_sheet::{ActionSheet, ActionSheetEvent};
use crate::app::keymap::{ArchiveWorktree, RunAction as RunActionCommand, ShowChanges};
use crate::core::menus::{MenuEntry, PaneMenuItem, RowMenuItem, workflow_entries};
use crate::core::palette::{Entry, changes_items};
use crate::core::picker::PickerItem;
use crate::core::sidebar::display_name;
use crate::core::worktree::{ARCHIVE, WORKTREE_LABEL, archive_params, can_archive, can_show_changes};

/// Where an action's result goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// The action sheet is open for it.
    Sheet,
    /// The New agent sheet's worktree spawn (that sheet is gone).
    NewAgent,
}

impl Workspace {
    /// ⇧⌘C: the focused pane's agent's changes.
    pub(super) fn show_changes_command(
        &mut self,
        _: &ShowChanges,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal() {
            return;
        }
        let app = self.app.read(cx);
        let Some(agent) = app.layout().agent(app.focused()) else {
            return;
        };
        if self.offers_changes(agent, cx) {
            self.show_changes(agent, window, cx);
        }
    }

    /// Show Changes for `agent_id` (menus, palette, ended summary).
    pub fn show_changes(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.graph_shown {
            self.graph_shown = false;
            self.watch_message_lines(cx);
        }
        self.app.update(cx, |a, cx| a.show_changes(agent_id, cx));
        self.focus_current(window, cx);
        cx.notify();
    }

    /// `agent_id` is in a git checkout and the daemon has the diff action.
    pub(super) fn offers_changes(&self, agent_id: u64, cx: &gpui::App) -> bool {
        let app = self.app.read(cx);
        app.agents
            .iter()
            .any(|r| r.info.id == agent_id && can_show_changes(&r.info, app.actions()))
    }

    /// `agent_id` ended in a worktree the workflow made.
    pub(super) fn offers_archive(&self, agent_id: u64, cx: &gpui::App) -> bool {
        let app = self.app.read(cx);
        app.agents
            .iter()
            .any(|r| r.info.id == agent_id && can_archive(&r.info, app.actions()))
    }

    /// What a row's (and a graph card's) menu adds for `agent_id`.
    pub(super) fn workflow_row_items(
        &self,
        agent_id: u64,
        cx: &gpui::App,
    ) -> Vec<MenuEntry<RowMenuItem>> {
        workflow_entries(
            self.offers_changes(agent_id, cx),
            self.offers_archive(agent_id, cx),
            (RowMenuItem::ShowChanges, RowMenuItem::ArchiveWorktree),
        )
    }

    /// What a pane's menu adds for its agent.
    pub(super) fn workflow_pane_items(
        &self,
        agent: Option<u64>,
        cx: &gpui::App,
    ) -> Vec<MenuEntry<(PaneMenuItem, bool)>> {
        let Some(id) = agent else { return Vec::new() };
        workflow_entries(
            self.offers_changes(id, cx),
            self.offers_archive(id, cx),
            (
                (PaneMenuItem::ShowChanges, true),
                (PaneMenuItem::ArchiveWorktree, true),
            ),
        )
    }

    /// The palette's "Show Changes: <agent>" rows.
    pub(super) fn palette_changes(&self, cx: &gpui::App) -> Vec<(Entry, PickerItem)> {
        let app = self.app.read(cx);
        let rows: Vec<_> = app
            .agents
            .iter()
            .filter(|r| can_show_changes(&r.info, app.actions()))
            .collect();
        changes_items(&rows, &app.home)
    }

    /// The palette's Run Action…: the daemon's actions, to pick one.
    pub(super) fn run_action_command(
        &mut self,
        _: &RunActionCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal() {
            return;
        }
        let app = self.app.read(cx);
        let actions = app.actions().all().to_vec();
        if actions.is_empty() {
            return;
        }
        let home = app.home.to_string_lossy().into_owned();
        let agents = agent_choices(app);
        let sheet = cx.new(|cx| ActionSheet::choose(actions, home, agents, cx));
        self.open_action_sheet(sheet, window, cx);
    }

    /// The palette's Archive Worktree…: the focused pane's agent's.
    pub(super) fn archive_worktree_command(
        &mut self,
        _: &ArchiveWorktree,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app = self.app.read(cx);
        if let Some(agent) = app.layout().agent(app.focused()) {
            self.open_archive(agent, window, cx);
        }
    }

    /// Archive Worktree… for `agent_id`: asks first.
    pub fn open_archive(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal() || !self.offers_archive(agent_id, cx) {
            return;
        }
        let app = self.app.read(cx);
        let Some(row) = app.agents.iter().find(|r| r.info.id == agent_id) else {
            return;
        };
        let path = row.info.labels.get(WORKTREE_LABEL).cloned().unwrap_or_default();
        let message = format!(
            "Archive the worktree of {}? {} is removed, and its branch is deleted if it is merged.",
            display_name(&row.info),
            crate::core::sidebar::tilde(&path, &app.home),
        );
        let sheet = cx.new(|cx| {
            ActionSheet::confirm(
                ARCHIVE.to_owned(),
                "Archive Worktree".to_owned(),
                message,
                "Archive".to_owned(),
                archive_params(agent_id),
                cx,
            )
        });
        self.open_action_sheet(sheet, window, cx);
    }

    fn open_action_sheet(
        &mut self,
        sheet: Entity<ActionSheet>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sub = cx.subscribe_in(&sheet, window, Self::action_sheet_event);
        sheet.read(cx).focus(window, cx);
        self.action_sheet = Some((sheet, sub));
        cx.notify();
    }

    /// The action sheet, while it is open.
    pub fn action_sheet(&self) -> Option<&Entity<ActionSheet>> {
        self.action_sheet.as_ref().map(|(s, _)| s)
    }

    fn close_action_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.action_sheet.take().is_some() {
            self.focus_current(window, cx);
            cx.notify();
        }
    }

    fn action_sheet_event(
        &mut self,
        sheet: &Entity<ActionSheet>,
        event: &ActionSheetEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ActionSheetEvent::Cancel => self.close_action_sheet(window, cx),
            ActionSheetEvent::Run { name, params } => {
                sheet.update(cx, |s, cx| s.running(cx));
                let call = RunAction {
                    name: name.clone(),
                    params: params.clone(),
                    cwd: String::new(),
                };
                let title = sheet.read(cx).title.clone();
                self.run_call(call, title, Origin::Sheet, window, cx);
            }
        }
    }

    /// Runs `call` off the UI thread; its result goes where `origin` says.
    pub(crate) fn run_call(
        &mut self,
        call: RunAction,
        title: String,
        origin: Origin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let job = self.app.read(cx).run_action_job(call.clone());
        let epoch = self.epoch;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_executor().spawn(async move { job() }).await;
            let _ = this.update_in(cx, |ws, window, cx| {
                if ws.epoch == epoch {
                    ws.action_done(call, title, origin, result, window, cx);
                }
            });
        })
        .detach();
    }

    /// An action's result: `done` is a toast, `spawned` shows the new
    /// agent, `view` opens a view pane, `refused` offers "… Anyway", and an
    /// error goes back to the sheet (or the error toast).
    pub(crate) fn action_done(
        &mut self,
        call: RunAction,
        title: String,
        origin: Origin,
        result: Result<ActionResult, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sheet = match origin {
            Origin::Sheet => self.action_sheet().cloned(),
            Origin::NewAgent => None,
        };
        match result.map(|r| (r.outcome.clone(), r)) {
            Ok((Some(Outcome::Done(d)), _)) => {
                self.close_action_sheet(window, cx);
                let text = if d.message.is_empty() {
                    format!("{title}: done")
                } else {
                    d.message
                };
                self.app.update(cx, |a, cx| a.show_notice(text, cx));
            }
            Ok((Some(Outcome::Spawned(s)), _)) => {
                self.close_action_sheet(window, cx);
                if let Some(agent) = &s.agent {
                    self.show_agent(agent.id, window, cx);
                }
                if !s.message.is_empty() {
                    self.app.update(cx, |a, cx| a.show_notice(s.message, cx));
                }
            }
            Ok((Some(Outcome::View(_)), result)) => {
                self.close_action_sheet(window, cx);
                self.app.update(cx, |a, cx| a.open_view(call, title, result, cx));
                self.focus_current(window, cx);
            }
            Ok((Some(Outcome::Refused(r)), _)) => match sheet {
                Some(sheet) => sheet.update(cx, |s, cx| s.refuse(r.reason, call.params, &r.retry, cx)),
                None => {
                    let sheet = cx.new(|cx| {
                        ActionSheet::refused(call.name, title, r.reason, call.params, &r.retry, cx)
                    });
                    self.open_action_sheet(sheet, window, cx);
                }
            },
            Ok((None, _)) => self.action_failed(sheet, "the action answered nothing".into(), call, cx),
            Err(e) => self.action_failed(sheet, e, call, cx),
        }
    }

    fn action_failed(
        &mut self,
        sheet: Option<Entity<ActionSheet>>,
        error: String,
        call: RunAction,
        cx: &mut Context<Self>,
    ) {
        match sheet {
            Some(sheet) => sheet.update(cx, |s, cx| s.failed(error, call.params, cx)),
            None => self.app.update(cx, |a, cx| a.show_error(error, cx)),
        }
    }
}

/// The agents an `agent` field offers: id and name, newest first.
fn agent_choices(app: &crate::app::app_state::AppState) -> Vec<(u64, String)> {
    let mut rows: Vec<_> = app.agents.iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.info.created_unix_ms));
    rows.iter()
        .map(|r| (r.info.id, display_name(&r.info)))
        .collect()
}

/// `params[key]` as an agent id, when it is one.
#[allow(dead_code)]
pub(crate) fn agent_param(params: &HashMap<String, ParamValue>, key: &str) -> Option<u64> {
    match params.get(key)?.value.as_ref()? {
        Value::Int(n) => u64::try_from(*n).ok(),
        _ => None,
    }
}
