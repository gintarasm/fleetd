//! The panes: focus, splits and their choosers, maximize, zoom, pane
//! events and their output commands.

use gpui::{App, AppContext, Context, Entity, FocusHandle, Window};

use super::Workspace;
use crate::app::diff_pane::{DiffPane, DiffPaneEvent};
use crate::app::keymap::{ClosePane, ReopenClosedPane, RestartPane, ToggleMaximize};
use crate::app::output;
use crate::app::pane_view::{PaneEvent, PaneView};
use crate::app::split_chooser::{ChooserEvent, SplitChooserView};
use crate::core::layout::{Axis, Dir};
use crate::core::menus::PaneMenuItem;
use crate::core::question::open_question;
use crate::core::session::PaneId;
use crate::core::split_chooser::{choice_rows, off_screen_agents};
use crate::core::zoom::Zoom;

impl Workspace {
    /// `pane`'s split chooser, while it asks what to start there.
    pub fn chooser(&self, pane: PaneId) -> Option<&Entity<SplitChooserView>> {
        self.choosers.get(&pane).map(|(c, _, _)| c)
    }

    /// ⌘⇧T: the pane closed last, with its agent.
    pub(super) fn reopen_closed_pane(
        &mut self,
        _: &ReopenClosedPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_modal() {
            return;
        }
        if self.app.update(cx, |a, cx| a.reopen_closed(cx)).is_some() {
            self.focus_current(window, cx);
        }
    }

    pub(super) fn pane_focus_handle(&self, pane: PaneId, cx: &App) -> Option<FocusHandle> {
        let app = self.app.read(cx);
        if let Some(diff) = app.diff_pane(pane) {
            return Some(diff.read(cx).focus_handle().clone());
        }
        app.pane_view(pane)
            .map(|v| v.read(cx).focus_handle().clone())
    }

    pub(super) fn focus_pane(&self, pane: PaneId, window: &mut Window, cx: &mut App) {
        self.app.update(cx, |a, cx| a.set_focused(pane, cx));
        if let Some((chooser, _, _)) = self.choosers.get(&pane) {
            chooser.read(cx).focus(window, cx);
        } else if let Some(handle) = self.pane_focus_handle(pane, cx) {
            window.focus(&handle);
        }
    }

    pub(super) fn focus_current(&self, window: &mut Window, cx: &mut App) {
        let pane = self.app.read(cx).focused();
        self.focus_pane(pane, window, cx);
    }

    /// Splits the focused pane; the new pane asks what to start in it.
    pub(super) fn split(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        let origin = self.app.read(cx).focused();
        if let Some(pane) = self.app.update(cx, |a, cx| a.split(axis, cx)) {
            self.open_chooser(pane, origin, window, cx);
            self.focus_current(window, cx);
        }
    }

    /// The split chooser in `pane`: its choices start in `origin`'s
    /// folder, and its list leaves out the agents on screen.
    fn open_chooser(
        &mut self,
        pane: PaneId,
        origin: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let presets = self.app.update(cx, |a, _| a.presets());
        let app = self.app.read(cx);
        let dir = app.start_dir(origin);
        let agents = off_screen_agents(&app.agents, &app.on_screen(), &app.home);
        let rows = choice_rows(
            &app.config.default_command,
            &dir,
            &app.home,
            agents.len(),
            &presets,
        );
        let chooser = cx.new(|cx| SplitChooserView::new(rows, agents, cx));
        let sub = cx.subscribe_in(&chooser, window, move |this, _, event, window, cx| {
            this.chooser_event(pane, *event, window, cx);
        });
        self.choosers.insert(pane, (chooser, dir, sub));
        self.follow_choosers(window, cx);
    }

    /// A choice: the chooser goes, and its pane starts what was picked,
    /// shows the agent, or closes.
    fn chooser_event(
        &mut self,
        pane: PaneId,
        event: ChooserEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, dir, _)) = self.choosers.remove(&pane) else {
            return;
        };
        self.follow_choosers(window, cx);
        self.app.update(cx, |a, cx| a.set_focused(pane, cx));
        if self.app.read(cx).focused() != pane {
            return;
        }
        match event {
            ChooserEvent::NewAgent if self.app.read(cx).config.default_command.is_empty() => {
                self.open_new_agent(window, cx);
                return;
            }
            ChooserEvent::NewAgent => self.spawn_default_in(dir, window, cx),
            ChooserEvent::Preset(index) => self.spawn_preset_in(index, dir, cx),
            ChooserEvent::Terminal => {
                let result = self.app.update(cx, |a, cx| {
                    let req = a.terminal_request(dir);
                    a.spawn(req, cx)
                });
                if let Err(e) = result {
                    self.app.update(cx, |a, cx| a.show_error(e, cx));
                }
            }
            ChooserEvent::Agent(id) => self.show_agent(id, window, cx),
            ChooserEvent::Cancel => {
                self.app.update(cx, |a, cx| a.close_focused(cx));
            }
        }
        self.focus_current(window, cx);
        cx.notify();
    }

    /// Choosers go once their pane is gone or gets an agent; each pane
    /// view draws its own (a pane id can get a new view).
    pub(super) fn follow_choosers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.choosers.is_empty() && !self.choosers_drawn {
            return;
        }
        let app = self.app.read(cx);
        // Every tab's: a hidden tab's chooser is there when it is shown again.
        let panes = app.tabs().panes();
        let mut refocus = false;
        self.choosers.retain(|pane, (chooser, _, _)| {
            let kept = panes.contains(pane) && !app.pane_filled(*pane);
            // The keyboard goes back to the pane the chooser was in.
            refocus |= !kept && chooser.read(cx).contains_focus(window, cx);
            kept
        });
        let views: Vec<(PaneId, Entity<PaneView>)> = panes
            .into_iter()
            .filter_map(|p| Some((p, app.pane_view(p)?.clone())))
            .collect();
        if refocus {
            cx.defer_in(window, |this, window, cx| this.focus_current(window, cx));
        }
        for (pane, view) in views {
            let chooser = self.choosers.get(&pane).map(|(c, _, _)| c.clone().into());
            view.update(cx, |v, cx| v.set_chooser(chooser, cx));
        }
        self.choosers_drawn = !self.choosers.is_empty();
    }

    pub(super) fn close_pane(
        &mut self,
        _: &ClosePane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_modal() {
            return;
        }
        let pane = self.app.read(cx).focused();
        self.choosers.remove(&pane);
        self.app.update(cx, |a, cx| a.close_focused(cx));
        self.focus_current(window, cx);
    }

    pub(super) fn focus_dir(&mut self, dir: Dir, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        if let Some(pane) = self.app.read(cx).neighbour(dir) {
            self.focus_pane(pane, window, cx);
        }
    }

    pub(super) fn focus_number(&mut self, n: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        if let Some(pane) = self.app.read(cx).pane_by_number(n) {
            self.focus_pane(pane, window, cx);
        }
    }

    /// The agent the focused pane shows, unless a sheet is up.
    fn focused_agent(&self, cx: &App) -> Option<(PaneId, u64)> {
        if self.has_modal() {
            return None;
        }
        let app = self.app.read(cx);
        let pane = app.focused();
        Some((pane, app.layout().agent(pane)?))
    }

    /// Copy Last Output, Save Output… or Open Output in Editor for the
    /// agent `pane` shows (ADR 0086).
    pub(super) fn output_in_pane(
        &mut self,
        pane: PaneId,
        item: PaneMenuItem,
        cx: &mut Context<Self>,
    ) {
        let Some(agent_id) = self.app.read(cx).layout().agent(pane) else {
            return;
        };
        match item {
            PaneMenuItem::CopyLastOutput => output::copy_last_output(&self.app, pane, cx),
            PaneMenuItem::SaveOutput => output::save_output(self.app.clone(), agent_id, cx),
            PaneMenuItem::OpenOutput => output::open_output(self.app.clone(), agent_id, cx),
            _ => {}
        }
    }

    /// ⇧⌘S and the palette: the focused pane's agent's output.
    pub(super) fn output_focused(&mut self, item: PaneMenuItem, cx: &mut Context<Self>) {
        if let Some((pane, _)) = self.focused_agent(cx) {
            self.output_in_pane(pane, item, cx);
        }
    }

    /// ⌘R: restarts the focused pane's agent in place if it ended.
    /// ⌘+ / ⌘− / ⌘0: the terminals' font, or the graph's scale while it
    /// is shown.
    pub(super) fn zoom(&mut self, zoom: Zoom, window: &mut Window, cx: &mut Context<Self>) {
        if self.graph_shown {
            self.graph.0.update(cx, |g, cx| g.zoom(zoom, window, cx));
            return;
        }
        self.app.update(cx, |a, cx| a.zoom(zoom, cx));
    }

    pub(super) fn open_menu_split(
        &mut self,
        agent_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .app
            .update(cx, |a, cx| a.open_in_new_split(agent_id, cx))
            .is_some()
        {
            self.focus_current(window, cx);
        }
        cx.notify();
    }

    /// Keeps the app's focused pane in step with clicks into panes.
    pub(super) fn follow_pane_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let views: Vec<(PaneId, Entity<PaneView>)> = {
            let app = self.app.read(cx);
            app.tabs()
                .panes()
                .into_iter()
                .filter_map(|p| Some((p, app.pane_view(p)?.clone())))
                .collect()
        };
        // A pane id can get a new view (closing the last pane, or its
        // agent removed): subscriptions follow the view, not the id, and
        // the keyboard follows a focused view's replacement.
        let diffs: Vec<(PaneId, Entity<DiffPane>)> = {
            let app = self.app.read(cx);
            app.tabs()
                .panes()
                .into_iter()
                .filter_map(|p| Some((p, app.diff_pane(p)?.clone())))
                .collect()
        };
        let mut refocus = false;
        self.pane_focus.retain(|p, (id, handle, _)| {
            let kept = views
                .iter()
                .any(|(pane, view)| pane == p && view.entity_id() == *id)
                || diffs
                    .iter()
                    .any(|(pane, view)| pane == p && view.entity_id() == *id);
            refocus |= !kept && handle.is_focused(window);
            kept
        });
        if refocus {
            cx.defer_in(window, |this, window, cx| this.focus_current(window, cx));
        }
        for (pane, view) in views {
            if self.pane_focus.contains_key(&pane) {
                continue;
            }
            let handle = view.read(cx).focus_handle().clone();
            let focus = cx.on_focus_in(&handle, window, move |this, window, cx| {
                this.app.update(cx, |a, cx| a.set_focused(pane, cx));
                // A click into the pane leaves the keys with its chooser.
                if let Some((chooser, _, _)) = this.choosers.get(&pane)
                    && !chooser.read(cx).contains_focus(window, cx)
                {
                    chooser.read(cx).focus(window, cx);
                }
            });
            let events = cx.subscribe_in(&view, window, move |this, _, event, window, cx| {
                this.pane_event(pane, *event, window, cx);
            });
            self.pane_focus
                .insert(pane, (view.entity_id(), handle, [focus, events]));
        }
        for (pane, view) in diffs {
            if self.pane_focus.contains_key(&pane) {
                continue;
            }
            let handle = view.read(cx).focus_handle().clone();
            let focus = cx.on_focus_in(&handle, window, move |this, _, cx| {
                this.app.update(cx, |a, cx| a.set_focused(pane, cx));
            });
            let events = cx.subscribe_in(&view, window, move |this, _, event, window, cx| {
                let DiffPaneEvent::Close = *event;
                this.app.update(cx, |a, cx| {
                    a.set_focused(pane, cx);
                    a.close_focused(cx);
                });
                this.focus_current(window, cx);
            });
            self.pane_focus
                .insert(pane, (view.entity_id(), handle, [focus, events]));
        }
    }

    /// A pane's header button: acts on that pane.
    pub(super) fn pane_event(
        &mut self,
        pane: PaneId,
        event: PaneEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_modal() {
            return;
        }
        self.focus_pane(pane, window, cx);
        match event {
            PaneEvent::NewAgent => self.open_new_agent(window, cx),
            PaneEvent::NewTerminal => self.open_new_terminal(window, cx),
            PaneEvent::InstallSkill => crate::app::resident::install_agent_skill(cx),
            PaneEvent::ShowShortcuts => self.toggle_shortcuts(window, cx),
            PaneEvent::SplitRight => self.split(Axis::Horizontal, window, cx),
            PaneEvent::SplitDown => self.split(Axis::Vertical, window, cx),
            PaneEvent::ToggleMaximize => self.toggle_maximize(&ToggleMaximize, window, cx),
            PaneEvent::Close => self.close_pane(&ClosePane, window, cx),
            PaneEvent::Restart => self.restart_pane(&RestartPane, window, cx),
            PaneEvent::Rename => {
                if let Some(agent_id) = self.app.read(cx).layout().agent(pane) {
                    self.open_rename(agent_id, window, cx);
                }
            }
            PaneEvent::Answer => {
                if let Some(agent_id) = self.app.read(cx).layout().agent(pane) {
                    self.open_answer(agent_id, window, cx);
                }
            }
            PaneEvent::Choose(i) => {
                if let Some(agent_id) = self.app.read(cx).layout().agent(pane) {
                    let q = self
                        .app
                        .read(cx)
                        .agents
                        .iter()
                        .find(|r| r.info.id == agent_id)
                        .and_then(|r| open_question(&r.info));
                    if let Some(q) = q
                        && let Some(choice) = q.choices.get(i)
                    {
                        self.answer_with(agent_id, q.id, choice.clone(), cx);
                    }
                }
            }
            PaneEvent::EditLabels => {
                if let Some(agent_id) = self.app.read(cx).layout().agent(pane) {
                    self.open_labels(agent_id, window, cx);
                }
            }
            PaneEvent::Duplicate => {
                if let Some(agent_id) = self.app.read(cx).layout().agent(pane) {
                    self.duplicate_agent(agent_id, window, cx);
                }
            }
            PaneEvent::Kill => self.kill_in_pane(pane, window, cx),
            PaneEvent::Remove => self.app.read(cx).remove_in_pane(pane, cx),
            PaneEvent::ContextMenu { on_grid } => self.open_pane_menu(pane, on_grid, cx),
            PaneEvent::ShowChanges => {
                if let Some(agent) = self.app.read(cx).layout().agent(pane) {
                    self.show_changes(agent, window, cx);
                }
            }
            PaneEvent::Notice(text) => self
                .app
                .update(cx, |a, cx| a.show_notice(text.to_owned(), cx)),
        }
    }

    pub(super) fn toggle_maximize(
        &mut self,
        _: &ToggleMaximize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_modal() {
            return;
        }
        self.app.update(cx, |a, cx| a.toggle_maximize(cx));
        self.focus_current(window, cx);
    }
}
