//! The pane's find bar (ADR 0053): open, search off the UI thread, step,
//! close.

use std::sync::Arc;

use gpui::{AppContext, Context, Entity, KeyDownEvent, Subscription, Window};

use super::{PaneView, lock};
use crate::app::keymap::{Find, FindNext, FindPrevious};
use crate::app::text_field::TextField;
use crate::core::find::status_label;

/// History lines a find step scans, off the UI thread, holding the pane's
/// model lock (about a millisecond; ADR 0053).
const FIND_STEP_LINES: usize = 2000;

/// The find bar's field and its subscription.
pub(super) struct FindBar {
    pub(super) field: Entity<TextField>,
    _changes: Subscription,
}

impl PaneView {
    /// ⌘F: opens the find bar (or focuses it) with the last query
    /// selected.
    pub(super) fn open_find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.is_none() {
            let query = self.find_query.clone();
            let field = cx.new(|cx| TextField::new(query, "Find", cx));
            let changes = cx.observe(&field, |v, field, cx| {
                let text = field.read(cx).text();
                if text != v.find_query {
                    v.find_query = text.to_owned();
                    v.search(cx);
                }
            });
            self.find = Some(FindBar {
                field,
                _changes: changes,
            });
            self.search(cx);
        }
        if let Some(bar) = &self.find {
            bar.field.update(cx, |f, cx| f.select_all_text(cx));
            window.focus(&bar.field.read(cx).focus_handle().clone());
        }
        cx.notify();
    }

    /// Opens the find bar on `query` and goes to its match on `line` (or
    /// the next below it), paging older history in as needed: a Search All
    /// Agents pick (ADR 0098).
    pub fn find_hit(
        &mut self,
        query: &str,
        case_sensitive: bool,
        line: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.find_query = query.to_owned();
        self.find_case = case_sensitive;
        self.find = None;
        self.open_find(&Find, window, cx);
        self.model().find_open_at(query, case_sensitive, line);
        self.find_task = None;
        self.continue_search(cx);
        cx.notify();
    }

    /// Starts searching for the find bar's query.
    fn search(&mut self, cx: &mut Context<Self>) {
        self.model().find_open(&self.find_query, self.find_case);
        self.find_task = None;
        self.continue_search(cx);
        cx.notify();
    }

    /// Scans what is left to scan in steps on the background executor, each
    /// under the model lock, redrawing as matches come in. Nothing here
    /// runs while drawing.
    pub(super) fn continue_search(&mut self, cx: &mut Context<Self>) {
        if self.find_task.is_some() || !self.model().find_pending() {
            return;
        }
        let model = Arc::clone(&self.model);
        self.find_task = Some(cx.spawn(async move |view, cx| {
            loop {
                let m = Arc::clone(&model);
                let step = cx
                    .background_executor()
                    .spawn(async move { lock(&m).find_step(FIND_STEP_LINES) })
                    .await;
                let go_on = view.update(cx, |v, cx| {
                    if step.changed {
                        cx.notify();
                    }
                    if !step.more {
                        v.find_task = None;
                        // At the oldest loaded line: page in older history.
                        v.sink.check_paging(v.pane);
                    }
                    step.more
                });
                if !matches!(go_on, Ok(true)) {
                    break;
                }
            }
        }));
    }

    /// The next match up (Enter, ⌘G) or down (⇧Enter, ⌘⇧G).
    pub(super) fn step_find(&mut self, up: bool, cx: &mut Context<Self>) {
        if self.find.is_none() {
            return;
        }
        if self.model().find_next(up) {
            self.sink.check_paging(self.pane);
            cx.notify();
        }
    }

    pub(super) fn find_next(&mut self, _: &FindNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step_find(true, cx);
    }

    pub(super) fn find_previous(
        &mut self,
        _: &FindPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_find(false, cx);
    }

    pub(super) fn toggle_find_case(&mut self, cx: &mut Context<Self>) {
        self.find_case = !self.find_case;
        self.search(cx);
    }

    /// Esc: closes the find bar; the keys go back to the terminal.
    pub(super) fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find = None;
        self.find_task = None;
        self.model().find_close();
        window.focus(&self.focus);
        cx.notify();
    }

    pub(super) fn find_key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let m = &ev.keystroke.modifiers;
        match ev.keystroke.key.as_str() {
            "enter" if !m.platform => self.step_find(!m.shift, cx),
            "escape" => self.close_find(window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    /// The find bar's count, while it is open.
    pub(crate) fn find_label(&self) -> Option<String> {
        self.find.as_ref()?;
        let status = self.model().find_status()?;
        Some(status_label(status, !self.find_query.is_empty()))
    }
}
