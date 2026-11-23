//! The sidebar: its filter, groups, rows, peek and dropped files.

use gpui::{App, AppContext, Context, Entity, ExternalPaths, KeyDownEvent, Window};

use super::{Modal, Workspace, with_workspace};
use crate::app::context_menu::{self, MenuLine};
use crate::app::keymap::FocusFilter;
use crate::app::peek_view::{PeekEvent, PeekView};
use crate::app::sidebar_view::tab_title;
use crate::core::broadcast::RowClick;
use crate::core::labels::chip_filter;
use crate::core::menus::group_menu;
use crate::core::peek::sidebar_target;
use crate::core::session::PaneId;
use crate::core::sidebar::{
    Filter, GroupBy, SidebarGroup, StatusFilter, group_agents_by, narrow_groups,
};

impl Workspace {
    /// Files from Finder dropped on `agent`'s sidebar row.
    pub(crate) fn drop_paths_on_agent(
        &mut self,
        agent: u64,
        paths: &ExternalPaths,
        cx: &mut Context<Self>,
    ) {
        if self.has_modal() {
            return;
        }
        let paths: Vec<String> = paths
            .paths()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        self.app
            .update(cx, |a, cx| a.drop_paths_on_agent(agent, &paths, cx));
    }

    /// The peek, while it is open (ADR 0103).
    pub fn peek(&self) -> Option<&Entity<PeekView>> {
        match &self.modal {
            Some(Modal::Peek(p, _)) => Some(p),
            _ => None,
        }
    }

    /// Peeks at `agent_id` in a popover, without changing the layout. One
    /// peek at a time: another one open is replaced. Not over a sheet.
    pub fn open_peek(&mut self, agent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.peek().is_none() && self.has_modal() {
            return;
        }
        if !self
            .app
            .read(cx)
            .agents
            .iter()
            .any(|r| r.info.id == agent_id)
        {
            return;
        }
        let app = self.app.clone();
        let view = cx.new(|cx| PeekView::new(app, agent_id, cx));
        let sub = cx.subscribe_in(&view, window, Self::peek_event);
        view.read(cx).focus(window, cx);
        self.modal = Some(Modal::Peek(view, sub));
        cx.notify();
    }

    fn peek_event(
        &mut self,
        _: &Entity<PeekView>,
        event: &PeekEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match *event {
            PeekEvent::Close => self.close_modal(window, cx),
            PeekEvent::OpenInPane(id) => {
                self.close_modal(window, cx);
                if self.graph_shown {
                    self.graph_shown = false;
                    self.watch_message_lines(cx);
                    self.close_graph_panel(window, cx);
                }
                self.show_agent(id, window, cx);
                cx.notify();
            }
        }
    }

    /// The sidebar's groups under its filter and the target's collapsed
    /// groups.
    pub(super) fn sidebar_groups(&self, cx: &App) -> Vec<SidebarGroup> {
        let app = self.app.read(cx);
        let filter = Filter {
            query: self.filter_field.read(cx).text().to_owned(),
            status: self.status_filter,
        };
        let titles = &app.titles;
        narrow_groups(
            group_agents_by(&app.agents, &app.home, app.group_by()),
            &app.home,
            &filter,
            &|r| tab_title(r, titles),
            &app.statuses,
            &app.collapsed_groups(),
            app.disclosure(),
        )
    }

    /// ⌥⌘F: the keyboard goes to the sidebar's filter field.
    pub(super) fn focus_filter(
        &mut self,
        _: &FocusFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_modal() {
            return;
        }
        window.focus(self.filter_field.read(cx).focus_handle());
    }

    /// A click on the sidebar's tabs gives them the keyboard, so typing
    /// filters (a click on a tab then focuses its pane).
    pub(crate) fn focus_sidebar(&mut self, window: &mut Window, _: &mut Context<Self>) {
        window.focus(&self.sidebar_focus);
    }

    /// A key typed while the sidebar's tabs have the keyboard starts the
    /// filter with it.
    pub(crate) fn sidebar_key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sidebar_focus.is_focused(window) {
            return;
        }
        let m = &ev.keystroke.modifiers;
        if m.platform || m.control {
            return;
        }
        if ev.keystroke.key == "escape" {
            self.row_selection.clear();
            self.focus_current(window, cx);
            cx.stop_propagation();
            return;
        }
        // Space peeks at the selected row, as Quick Look (ADR 0103).
        if ev.keystroke.key == "space"
            && !m.alt
            && !m.shift
            && let Some(id) = sidebar_target(&self.row_selection)
        {
            self.open_peek(id, window, cx);
            cx.stop_propagation();
            return;
        }
        let Some(text) = ev
            .keystroke
            .key_char
            .clone()
            .filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
        else {
            return;
        };
        let field = self.filter_field.clone();
        window.focus(field.read(cx).focus_handle());
        field.update(cx, |f, cx| f.insert(&text, cx));
        cx.stop_propagation();
    }

    /// In the filter field: Escape clears it, then gives the keyboard
    /// back to the focused pane; Enter shows the first agent listed.
    pub(crate) fn filter_key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev.keystroke.key.as_str() {
            "escape" => {
                if self.filter_field.read(cx).text().is_empty() {
                    self.focus_current(window, cx);
                } else {
                    self.filter_field
                        .update(cx, |f, cx| f.set_text(String::new(), cx));
                }
                cx.stop_propagation();
            }
            "enter" => {
                let first = self
                    .sidebar_groups(cx)
                    .iter()
                    .find_map(|g| g.agents.first().map(|r| r.info.id));
                if let Some(id) = first {
                    self.show_agent(id, window, cx);
                }
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    pub(crate) fn set_status_filter(&mut self, filter: StatusFilter, cx: &mut Context<Self>) {
        self.status_filter = filter;
        cx.notify();
    }

    pub(crate) fn toggle_disclosure(&mut self, id: u64, children: usize, cx: &mut Context<Self>) {
        self.app
            .update(cx, |a, cx| a.toggle_disclosure(id, children, cx));
    }

    pub(crate) fn toggle_group(&mut self, title: &str, cx: &mut Context<Self>) {
        self.app.update(cx, |a, cx| a.toggle_group(title, cx));
    }

    /// Groups the sidebar `by` folder or repository (ADR 0085).
    pub(crate) fn set_group_by(&mut self, by: GroupBy, cx: &mut Context<Self>) {
        self.app.update(cx, |a, cx| a.set_group_by(by, cx));
    }

    /// A group header's native menu (at the right-click being handled):
    /// group by folder or by repository.
    pub(crate) fn open_group_menu(&mut self, cx: &mut Context<Self>) {
        let items = group_menu(self.app.read(cx).group_by());
        let lines = items
            .iter()
            .map(|&(by, checked)| {
                Some(MenuLine {
                    title: by.title().to_owned(),
                    enabled: true,
                    shortcut: None,
                    checked,
                })
            })
            .collect();
        context_menu::show(lines, cx, move |index, cx| {
            if let Some(&(by, _)) = items.get(index) {
                with_workspace(cx, |ws, _, cx| ws.set_group_by(by, cx));
            }
        });
    }

    /// The `input` bench's panes: `agent` in the focused pane (returned)
    /// and each of `extra` in a new split; the focus returns to the first.
    pub(crate) fn bench_open(
        &mut self,
        agent: u64,
        extra: &[u64],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PaneId {
        let pane = self.app.read(cx).focused();
        self.show_agent(agent, window, cx);
        for &id in extra {
            self.app.update(cx, |a, cx| a.open_in_new_split(id, cx));
        }
        self.focus_pane(pane, window, cx);
        pane
    }

    /// A click on a row's label chip: the sidebar filter gets `key=value`,
    /// or loses it on a second click.
    /// Approves or denies an agent's pending spawn (ADR 0079).
    pub(crate) fn decide_spawn(&mut self, id: u64, approve: bool, cx: &mut Context<Self>) {
        self.app.read(cx).decide_spawn(id, approve);
    }

    pub(crate) fn filter_by_label(&mut self, key: &str, value: &str, cx: &mut Context<Self>) {
        let query = chip_filter(self.filter_field.read(cx).text(), key, value);
        self.filter_field.update(cx, |f, cx| f.set_text(query, cx));
        cx.notify();
    }

    /// A click on a sidebar row: ⌘ or ⇧ held picks rows (ADR 0083);
    /// a plain click shows the agent in the focused pane.
    pub(crate) fn row_click(
        &mut self,
        agent_id: u64,
        cmd: bool,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let order: Vec<u64> = self
            .sidebar_groups(cx)
            .iter()
            .flat_map(|g| g.agents.iter().map(|r| r.info.id))
            .collect();
        let current = {
            let app = self.app.read(cx);
            app.layout().agent(app.focused())
        };
        match self
            .row_selection
            .click(agent_id, cmd, shift, &order, current)
        {
            RowClick::Show => self.show_agent(agent_id, window, cx),
            RowClick::Selected => cx.notify(),
        }
    }
}
