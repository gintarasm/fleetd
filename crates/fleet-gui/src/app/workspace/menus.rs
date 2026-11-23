//! The native context menus of sidebar rows and panes.

use gpui::{Action, App, Context, Window};

use super::{Workspace, with_workspace};
use crate::app::context_menu::{self, MenuLine};
use crate::app::keymap::{
    ClearScrollback, Copy, Kill, Paste, Remove, Restart, ScrollToBottom, ScrollToTop, SelectAll,
};
use crate::app::kill_sheet::{self};
use crate::app::output;
use crate::app::pane_view::PaneEvent;
use crate::core::bulk::BulkAction;
use crate::core::layout::MAX_PANES;
use crate::core::menus::{
    MenuEntry, PaneMenuItem, PaneMenuState, RowMenuItem, bulk_entries, pane_menu, row_menu,
};
use crate::core::session::PaneId;
use crate::core::sidebar::ended;
use crate::core::status::Subject;

impl Workspace {
    /// The sidebar row's native context menu (at the right-click being
    /// handled).
    pub fn open_row_menu(&mut self, agent_id: u64, cx: &mut Context<Self>) {
        let items = self.menu_items(agent_id, cx);
        let bulk = self.bulk_targets(cx);
        let lines = context_menu::lines(&items, |item| {
            let action = match item {
                RowMenuItem::KillSelected => Some(BulkAction::Kill),
                RowMenuItem::RestartSelected => Some(BulkAction::Restart),
                RowMenuItem::RemoveSelected => Some(BulkAction::Remove),
                _ => None,
            };
            MenuLine {
                title: action.map_or_else(|| item.title().to_owned(), |a| a.menu_title(&bulk)),
                enabled: action.is_none_or(|a| a.disabled_reason(&bulk).is_none()),
                shortcut: None,
                checked: false,
            }
        });
        let epoch = self.epoch;
        context_menu::show(lines, cx, move |index, cx| {
            if let Some(MenuEntry::Item(item)) = items.get(index).copied() {
                with_workspace(cx, |ws, window, cx| {
                    ws.row_menu_pick(agent_id, item, epoch, window, cx);
                });
            }
        });
    }

    /// An item picked from a row's menu opened in session `epoch`.
    pub(crate) fn row_menu_pick(
        &mut self,
        agent_id: u64,
        item: RowMenuItem,
        epoch: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if epoch != self.epoch || self.has_modal() {
            return;
        }
        // The agent may have changed since the menu opened (a Kill for an
        // agent that exited meanwhile): only what the menu offers now.
        let offered = self.menu_items(agent_id, cx);
        if !offered.contains(&MenuEntry::Item(item)) {
            return;
        }
        match item {
            RowMenuItem::OpenSplit => self.open_menu_split(agent_id, window, cx),
            RowMenuItem::Rename => self.open_rename(agent_id, window, cx),
            RowMenuItem::EditLabels => self.open_labels(agent_id, window, cx),
            RowMenuItem::Duplicate => self.duplicate_agent(agent_id, window, cx),
            RowMenuItem::SaveOutput => output::save_output(self.app.clone(), agent_id, cx),
            RowMenuItem::OpenOutput => output::open_output(self.app.clone(), agent_id, cx),
            RowMenuItem::SendTo | RowMenuItem::SendToSelected => {
                let targets = self.row_selection.targets_from(agent_id);
                self.open_send(&targets, window, cx);
            }
            RowMenuItem::Kill => self.kill(&Kill { agent_id }, window, cx),
            RowMenuItem::Restart => self.restart(&Restart { agent_id }, window, cx),
            RowMenuItem::Remove => self.remove(&Remove { agent_id }, window, cx),
            RowMenuItem::KillSelected => self.bulk(BulkAction::Kill, window, cx),
            RowMenuItem::RestartSelected => self.bulk(BulkAction::Restart, window, cx),
            RowMenuItem::RemoveSelected => self.bulk(BulkAction::Remove, window, cx),
            RowMenuItem::ShowChanges => self.show_changes(agent_id, window, cx),
            RowMenuItem::ArchiveWorktree => self.open_archive(agent_id, window, cx),
        }
    }

    /// A pane's native context menu: on its header, or on its grid
    /// (`on_grid`, adding Copy and Paste).
    pub(super) fn open_pane_menu(&mut self, pane: PaneId, on_grid: bool, cx: &mut Context<Self>) {
        let (state, subject) = {
            let app = self.app.read(cx);
            let view = app.pane_view(pane).map(|v| v.read(cx));
            let subject = app
                .layout()
                .agent(pane)
                .and_then(|id| app.agents.iter().find(|r| r.info.id == id))
                .map_or(Subject::Agent, |r| Subject::of(&r.info));
            let (can_scroll, scrolled, has_history) =
                view.map_or((false, false, false), |v| v.scroll_state());
            let state = PaneMenuState {
                on_grid,
                has_selection: view.is_some_and(|v| v.has_selection()),
                can_scroll,
                scrolled,
                has_history,
                can_split: app.layout().panes().len() < MAX_PANES,
                maximized: app.maximized == Some(pane),
                restartable: view.is_some_and(|v| v.restartable()),
                has_agent: app.layout().agent(pane).is_some(),
                killable: app
                    .layout()
                    .agent(pane)
                    .is_some_and(|id| kill_sheet::running(app, id)),
            };
            (state, subject)
        };
        let mut items = pane_menu(state);
        // Show Changes and Archive Worktree… before Close Pane.
        let agent = self.app.read(cx).layout().agent(pane);
        let close = items.len().saturating_sub(2);
        items.splice(close..close, self.workflow_pane_items(agent, cx));
        let lines = context_menu::lines(&items, |(item, enabled)| MenuLine {
            title: item.title(subject),
            enabled,
            shortcut: item.shortcut(),
            checked: false,
        });
        let epoch = self.epoch;
        context_menu::show(lines, cx, move |index, cx| {
            if let Some(MenuEntry::Item((item, true))) = items.get(index).copied() {
                with_workspace(cx, |ws, window, cx| {
                    ws.pane_menu_pick(pane, item, epoch, window, cx);
                });
            }
        });
    }

    /// An item picked from `pane`'s menu opened in session `epoch`.
    pub(crate) fn pane_menu_pick(
        &mut self,
        pane: PaneId,
        item: PaneMenuItem,
        epoch: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if epoch != self.epoch || self.has_modal() {
            return;
        }
        let event = match item {
            PaneMenuItem::Copy
            | PaneMenuItem::Paste
            | PaneMenuItem::SelectAll
            | PaneMenuItem::ScrollToTop
            | PaneMenuItem::ScrollToBottom
            | PaneMenuItem::ClearScrollback => {
                // The pane's own actions, as its keys perform them.
                self.focus_pane(pane, window, cx);
                let action: Box<dyn Action> = match item {
                    PaneMenuItem::Copy => Box::new(Copy),
                    PaneMenuItem::Paste => Box::new(Paste),
                    PaneMenuItem::SelectAll => Box::new(SelectAll),
                    PaneMenuItem::ScrollToTop => Box::new(ScrollToTop),
                    PaneMenuItem::ScrollToBottom => Box::new(ScrollToBottom),
                    _ => Box::new(ClearScrollback),
                };
                window.dispatch_action(action, cx);
                return;
            }
            PaneMenuItem::SplitRight => PaneEvent::SplitRight,
            PaneMenuItem::SplitDown => PaneEvent::SplitDown,
            PaneMenuItem::Maximize | PaneMenuItem::Restore => PaneEvent::ToggleMaximize,
            PaneMenuItem::Restart => PaneEvent::Restart,
            PaneMenuItem::Rename => PaneEvent::Rename,
            PaneMenuItem::EditLabels => PaneEvent::EditLabels,
            PaneMenuItem::Duplicate => PaneEvent::Duplicate,
            PaneMenuItem::Kill => PaneEvent::Kill,
            PaneMenuItem::Close => PaneEvent::Close,
            PaneMenuItem::ShowChanges | PaneMenuItem::ArchiveWorktree => {
                if let Some(agent) = self.app.read(cx).layout().agent(pane) {
                    if item == PaneMenuItem::ShowChanges {
                        self.show_changes(agent, window, cx);
                    } else {
                        self.open_archive(agent, window, cx);
                    }
                }
                return;
            }
            PaneMenuItem::CopyLastOutput | PaneMenuItem::SaveOutput | PaneMenuItem::OpenOutput => {
                self.output_in_pane(pane, item, cx);
                return;
            }
        };
        self.pane_event(pane, event, window, cx);
    }

    /// The row menu for `agent_id`: Open in New Split (while a pane can
    /// be added), then Kill for a running agent, or Restart and Remove for
    /// one that exited or is lost.
    pub(super) fn menu_items(&self, agent_id: u64, cx: &App) -> Vec<MenuEntry<RowMenuItem>> {
        let app = self.app.read(cx);
        let is_ended = app.agents.iter().any(|r| r.info.id == agent_id && ended(r));
        let in_selection = self.row_selection.targets_from(agent_id).len() > 1;
        let mut items = row_menu(
            app.layout().panes().len() < MAX_PANES,
            is_ended,
            in_selection,
        );
        if in_selection {
            items.extend(bulk_entries());
        }
        items.extend(self.workflow_row_items(agent_id, cx));
        items
    }
}
