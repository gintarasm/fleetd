//! Search All Agents (⇧⌘F, ADR 0098): a picker whose rows are the node's
//! hits for the query, grouped by agent. Typing is debounced and at most
//! one search runs at a time ([`Requests`]); a pick asks the workspace to
//! show the agent and find the line.

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, IntoElement, Render, Subscription, Task, Window,
};

use super::app_state::AppState;
use super::picker_view::{PickerEvent, PickerQuery, PickerView};
use crate::core::find::Needle;
use crate::core::search_all::{
    DEBOUNCE, Requests, hit_rows, parse_hit_key, smart_case, status_text,
};

pub enum SearchAllEvent {
    /// Show `agent_id` and find `query` on `line` in its pane.
    Show {
        agent_id: u64,
        line: u64,
        query: String,
        case_sensitive: bool,
    },
    Dismiss,
}

pub struct SearchAllView {
    app: Entity<AppState>,
    picker: Entity<PickerView>,
    requests: Requests,
    /// The query whose hits are listed.
    listed: String,
    debounce: Option<Task<()>>,
    job: Option<Task<()>>,
    _subs: [Subscription; 2],
}

impl EventEmitter<SearchAllEvent> for SearchAllView {}

impl SearchAllView {
    pub fn new(app: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let status = status_text("", false, 0, false).unwrap_or_default();
        let picker = cx.new(|cx| PickerView::search("Search all agents' output", &status, cx));
        let subs = [
            cx.subscribe(&picker, |this: &mut Self, _, ev: &PickerQuery, cx| {
                this.query_changed(&ev.0, cx);
            }),
            cx.subscribe(&picker, |this: &mut Self, _, ev: &PickerEvent, cx| {
                this.picked(ev, cx);
            }),
        ];
        SearchAllView {
            app,
            picker,
            requests: Requests::default(),
            listed: String::new(),
            debounce: None,
            job: None,
            _subs: subs,
        }
    }

    pub fn picker(&self) -> &Entity<PickerView> {
        &self.picker
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        self.picker.read(cx).focus(window, cx);
    }

    fn query_changed(&mut self, query: &str, cx: &mut Context<Self>) {
        self.requests.set_query(query);
        if query.is_empty() {
            self.debounce = None;
            self.list(Vec::new(), "", false, cx);
            return;
        }
        self.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.send(cx));
        }));
    }

    /// Asks the node for the latest query, unless a search is running (its
    /// answer asks again).
    fn send(&mut self, cx: &mut Context<Self>) {
        let Some(query) = self.requests.to_send() else {
            return;
        };
        if self.picker.read(cx).picker().ranked().is_empty() {
            let status = status_text(&query, true, 0, false);
            self.picker.update(cx, |p, cx| {
                p.set_listed(Vec::new(), Vec::new(), Vec::new(), status, cx)
            });
        }
        let case = smart_case(&query);
        let job = self.app.read(cx).search_output(query.clone(), case);
        self.job = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { job() }).await;
            let _ = this.update(cx, |this, cx| {
                this.job = None;
                if this.requests.answered(&query) {
                    match result {
                        Ok(found) => this.list(found.hits, &query, found.truncated, cx),
                        Err(e) => {
                            this.listed.clear();
                            this.picker.update(cx, |p, cx| {
                                p.set_listed(Vec::new(), Vec::new(), Vec::new(), Some(e), cx)
                            });
                        }
                    }
                }
                this.send(cx);
            });
        }));
    }

    fn list(
        &mut self,
        hits: Vec<fleet_proto::SearchHit>,
        query: &str,
        truncated: bool,
        cx: &mut Context<Self>,
    ) {
        self.listed = query.to_owned();
        let status = status_text(query, false, hits.len(), truncated);
        let rows = Needle::new(query, smart_case(query)).map(|needle| {
            let app = self.app.read(cx);
            hit_rows(&hits, &app.agents, &needle, &app.home)
        });
        self.picker.update(cx, |p, cx| match rows {
            Some(r) => p.set_listed(r.items, r.hits, r.sections, status, cx),
            None => p.set_listed(Vec::new(), Vec::new(), Vec::new(), status, cx),
        });
    }

    fn picked(&mut self, ev: &PickerEvent, cx: &mut Context<Self>) {
        match ev {
            PickerEvent::Dismiss => cx.emit(SearchAllEvent::Dismiss),
            PickerEvent::Pick(key) => {
                if let Some((agent_id, line)) = parse_hit_key(key) {
                    let query = self.listed.clone();
                    cx.emit(SearchAllEvent::Show {
                        agent_id,
                        line,
                        case_sensitive: smart_case(&query),
                        query,
                    });
                }
            }
        }
    }
}

impl Render for SearchAllView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.picker.clone()
    }
}
