//! Commands on agents: duplicate, kill, restart and remove (one, the
//! selected rows, or every ended one).

use gpui::{App, AppContext, Context, PromptButton, PromptLevel, Window};

use super::{Modal, Workspace};
use crate::app::keymap::{
    DuplicatePane, EditLabelsPane, Kill, KillPane, KillSelected, Remove, RemoveExited, RemovePane,
    RemoveSelected, RenamePane, Restart, RestartPane, RestartSelected,
};
use crate::app::kill_sheet::{self, KillSheet};
use crate::app::output;
use crate::core::broadcast::target_name;
use crate::core::bulk::{BulkAction, BulkTargets};
use crate::core::kill::{KillFlow, kill_flow};
use crate::core::palette::remove_exited_prompt;
use crate::core::presets::Prefill;
use crate::core::session::PaneId;

impl Workspace {
    /// Duplicate Agent (row and pane menus, palette): the New agent sheet
    /// with `agent_id`'s command, folder and labels, and a new name. Works
    /// while it runs.
    pub fn duplicate_agent(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let Some(prefill) = self
            .app
            .read(cx)
            .agents
            .iter()
            .find(|r| r.info.id == agent_id)
            .map(|r| Prefill::duplicate(&r.info))
        else {
            return;
        };
        self.open_new_agent_with(prefill, window, cx);
    }

    /// Duplicate Agent… (palette): the focused pane's agent.
    pub(super) fn duplicate_pane(
        &mut self,
        _: &DuplicatePane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app = self.app.read(cx);
        if let Some(agent_id) = app.layout().agent(app.focused()) {
            self.duplicate_agent(agent_id, window, cx);
        }
    }

    /// The sidebar's selection (of several rows) split by what Kill,
    /// Restart and Remove fit (ADR 0099).
    pub(super) fn bulk_targets(&self, cx: &App) -> BulkTargets {
        if self.row_selection.ids().len() < 2 {
            return BulkTargets::default();
        }
        BulkTargets::of(self.row_selection.ids(), &self.app.read(cx).agents)
    }

    /// Kill / Restart / Remove for the selected agents each fits. Kill
    /// asks once, listing the names (unless `[gui] confirm_kill` is off);
    /// the others act at once, as their single-agent forms do.
    pub(super) fn bulk(&mut self, action: BulkAction, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let targets = self.bulk_targets(cx);
        let ids = action.targets(&targets).to_vec();
        if ids.is_empty() {
            return;
        }
        match action {
            BulkAction::Kill => {
                let app = self.app.read(cx);
                match kill_flow(app.config.confirm_kill) {
                    KillFlow::Kill => ids.iter().for_each(|&id| app.kill(id)),
                    KillFlow::Ask => {
                        let names = ids
                            .iter()
                            .filter_map(|id| app.agents.iter().find(|r| r.info.id == *id))
                            .map(target_name)
                            .collect();
                        let sheet = cx.new(|cx| KillSheet::many(ids, names, cx));
                        let sub = cx.subscribe_in(&sheet, window, Self::kill_event);
                        sheet.read(cx).focus(window);
                        self.modal = Some(Modal::Kill(sheet, sub));
                    }
                }
            }
            BulkAction::Restart => {
                let app = self.app.read(cx);
                ids.iter().for_each(|&id| app.restart(id));
            }
            BulkAction::Remove => self.app.read(cx).forget_many(ids),
        }
        cx.notify();
    }

    pub(super) fn kill_selected(
        &mut self,
        _: &KillSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bulk(BulkAction::Kill, window, cx);
    }

    pub(super) fn restart_selected(
        &mut self,
        _: &RestartSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bulk(BulkAction::Restart, window, cx);
    }

    pub(super) fn remove_selected(
        &mut self,
        _: &RemoveSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bulk(BulkAction::Remove, window, cx);
    }

    /// ⌘⇧W, a pane header's Stop or its menu's Kill Agent: kills `pane`'s
    /// agent (asking first) if it is running.
    pub(super) fn kill_in_pane(
        &mut self,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app = self.app.read(cx);
        if let Some(agent_id) = app.layout().agent(pane)
            && kill_sheet::running(app, agent_id)
        {
            self.kill(&Kill { agent_id }, window, cx);
        }
    }

    pub(super) fn kill_pane(&mut self, _: &KillPane, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.app.read(cx).focused();
        self.kill_in_pane(pane, window, cx);
    }

    /// Rename Agent… (palette): the focused pane's agent.
    pub(super) fn rename_pane(
        &mut self,
        _: &RenamePane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app = self.app.read(cx);
        if let Some(agent_id) = app.layout().agent(app.focused()) {
            self.open_rename(agent_id, window, cx);
        }
    }

    /// Edit Labels… (palette): the focused pane's agent.
    pub(super) fn edit_labels_pane(
        &mut self,
        _: &EditLabelsPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app = self.app.read(cx);
        if let Some(agent_id) = app.layout().agent(app.focused()) {
            self.open_labels(agent_id, window, cx);
        }
    }

    /// Remove Exited Agent in Pane (palette): forgets the focused pane's
    /// agent if it ended, as the pane's summary bar does.
    pub(super) fn remove_pane(&mut self, _: &RemovePane, _: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let app = self.app.read(cx);
        app.remove_in_pane(app.focused(), cx);
        cx.notify();
    }

    /// Restarts an ended agent from the row menu: no confirmation.
    pub(super) fn restart(&mut self, action: &Restart, _: &mut Window, cx: &mut Context<Self>) {
        self.app.read(cx).restart(action.agent_id);
        cx.notify();
    }

    /// Forgets an ended agent from the row menu: no confirmation.
    pub(super) fn remove(&mut self, action: &Remove, _: &mut Window, cx: &mut Context<Self>) {
        self.app.read(cx).forget(action.agent_id);
        cx.notify();
    }

    /// Remove Exited Agents (palette, app menu): asks first, naming how
    /// many will go; does nothing while none has ended. fleetd removes what
    /// has ended when the request lands and never a running agent. "Save
    /// Output First…" saves each one's output into a folder, then removes
    /// those saved (ADR 0086).
    pub(super) fn remove_exited(
        &mut self,
        _: &RemoveExited,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_modal() {
            return;
        }
        let count = self.app.read(cx).ended_count();
        let Some(question) = remove_exited_prompt(count) else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &question,
            Some("Their history and scrollback are deleted. Running agents are not touched."),
            &[
                PromptButton::new("Remove"),
                PromptButton::new("Save Output First…"),
                PromptButton::cancel("Cancel"),
            ],
            cx,
        );
        let app = self.app.clone();
        cx.spawn(async move |_, cx| match answer.await {
            Ok(0) => {
                let _ = app.update(cx, |a, _| a.clear_exited());
            }
            Ok(1) => {
                let _ = cx.update(|cx| {
                    let ids = app.read(cx).ended_ids();
                    output::save_ended_then_remove(app, ids, cx);
                });
            }
            _ => {}
        })
        .detach();
        cx.notify();
    }

    /// The sidebar's Clear exited: forgets every ended agent.
    pub fn clear_exited(&mut self, cx: &mut Context<Self>) {
        if self.app.read(cx).ended_count() > 0 {
            self.app.read(cx).clear_exited();
        }
    }

    pub(super) fn restart_pane(&mut self, _: &RestartPane, _: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        self.app.update(cx, |a, cx| a.restart_focused(cx));
    }
}
