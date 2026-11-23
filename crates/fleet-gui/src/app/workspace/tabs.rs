//! The tab strip's commands: new, close, select, next and previous.

use gpui::{App, Context, Window};

use super::Workspace;
use crate::app::keymap::{CloseTab, NewTab, NextTab, PrevTab, SelectTab};
use crate::app::sidebar_view::program_name;
use crate::core::tabs::tab_label;

impl Workspace {
    pub(super) fn new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.add_tab(window, cx);
    }

    /// ⌘⌥T (or the strip's New tab button): a new tab with one empty
    /// pane, focused.
    pub fn add_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        if self.app.update(cx, |a, cx| a.new_tab(cx)).is_some() {
            self.focus_current(window, cx);
        }
    }

    /// ⌘⌥W: closes the shown tab, detaching its agents.
    pub(super) fn close_shown_tab(
        &mut self,
        _: &CloseTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = self.app.read(cx).tabs().active();
        self.close_tab(shown, window, cx);
    }

    /// Closes tab `i` (0-based; its close button), detaching its agents.
    pub fn close_tab(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        self.app.update(cx, |a, cx| a.close_tab(i, cx));
        self.focus_current(window, cx);
    }

    /// Shows tab `i` (0-based) and focuses its focused pane.
    pub fn select_tab(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_modal() {
            return;
        }
        if self.app.update(cx, |a, cx| a.select_tab(i, cx)) {
            self.focus_current(window, cx);
        }
    }

    pub(super) fn select_numbered_tab(
        &mut self,
        a: &SelectTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(i) = a.number.checked_sub(1) {
            self.select_tab(i, window, cx);
        }
    }

    pub(super) fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_modal() && self.app.update(cx, |a, cx| a.next_tab(cx)) {
            self.focus_current(window, cx);
        }
    }

    pub(super) fn prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_modal() && self.app.update(cx, |a, cx| a.prev_tab(cx)) {
            self.focus_current(window, cx);
        }
    }

    /// The strip's labels: each tab's number and its focused pane's program.
    pub(super) fn tab_labels(&self, cx: &App) -> Vec<String> {
        let app = self.app.read(cx);
        let tabs = app.tabs();
        (0..tabs.len())
            .map(|i| {
                let program = tabs
                    .tab_focused(i)
                    .and_then(|p| tabs.agent(p))
                    .and_then(|id| app.agents.iter().find(|r| r.info.id == id))
                    .map(|r| program_name(&r.info.program));
                tab_label(i + 1, program.as_deref())
            })
            .collect()
    }
}
