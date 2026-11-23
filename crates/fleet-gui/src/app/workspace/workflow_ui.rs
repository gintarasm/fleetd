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
use crate::app::new_agent::NewAgentSheet;
use crate::core::menus::{MenuEntry, PaneMenuItem, RowMenuItem, workflow_entries};
use crate::core::palette::{Entry, changes_items};
use crate::core::picker::PickerItem;
use crate::core::sidebar::display_name;
use crate::core::spawn::SpawnRequest;
use crate::core::worktree::{
    ARCHIVE, NEW_AGENT, NewWorktree, WORKTREE_LABEL, archive_params, can_archive, can_show_changes,
    new_agent_params,
};
use fleet_client::names::NAME_LABEL;

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
        let path = row
            .info
            .labels
            .get(WORKTREE_LABEL)
            .cloned()
            .unwrap_or_default();
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
                self.app
                    .update(cx, |a, cx| a.open_view(call, title, result, cx));
                self.focus_current(window, cx);
            }
            Ok((Some(Outcome::Refused(r)), _)) => match sheet {
                Some(sheet) => {
                    sheet.update(cx, |s, cx| s.refuse(r.reason, call.params, &r.retry, cx))
                }
                None => {
                    let sheet = cx.new(|cx| {
                        ActionSheet::refused(call.name, title, r.reason, call.params, &r.retry, cx)
                    });
                    self.open_action_sheet(sheet, window, cx);
                }
            },
            Ok((None, _)) => {
                self.action_failed(sheet, "the action answered nothing".into(), call, cx)
            }
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

impl Workspace {
    /// The New agent sheet's "New worktree": offered while the daemon has
    /// the action, checked as last used on this target.
    pub(super) fn worktree_setup(&self, cx: &gpui::App) -> Option<bool> {
        let app = self.app.read(cx);
        app.actions()
            .has(NEW_AGENT)
            .then(|| app.new_worktree_remembered())
    }

    /// The sheet answered: its checkbox is remembered for the target.
    pub(super) fn remember_worktree(&self, sheet: &Entity<NewAgentSheet>, cx: &mut Context<Self>) {
        let on = sheet
            .read(cx)
            .worktree
            .as_ref()
            .filter(|w| w.in_checkout)
            .map(|w| w.on);
        if let Some(on) = on {
            self.app.update(cx, |a, cx| a.remember_new_worktree(on, cx));
        }
    }

    /// The New agent sheet's spawn into a new worktree: runs
    /// `git-worktree.new-agent` in the folder's repository; the new agent
    /// shows in the focused pane, a refusal offers "… Anyway".
    pub(super) fn spawn_in_worktree(
        &mut self,
        req: &SpawnRequest,
        _command: &str,
        worktree: &NewWorktree,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app = self.app.read(cx);
        let repo = app.expand_dir(&req.cwd);
        let mut labels = req.labels.clone();
        let name = labels.remove(NAME_LABEL);
        let mut argv = vec![req.program.clone()];
        argv.extend(req.args.iter().cloned());
        let params = new_agent_params(worktree, &repo, argv, name.as_deref(), labels);
        let call = RunAction {
            name: NEW_AGENT.to_owned(),
            params,
            cwd: repo,
        };
        self.run_call(
            call,
            "New Agent in Worktree".to_owned(),
            Origin::NewAgent,
            window,
            cx,
        );
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

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use fleet_proto::action_result::Outcome;
    use fleet_proto::param_value::Value;
    use fleet_proto::{
        ActionDone, ActionRefused, ActionResult, ActionSpawned, ActionSpec, AgentInfo, AgentState,
        DiffView, FileDiff, FileStatus, GitInfo, Hunk, ParamKind, ParamSpec, ParamValue, RunAction,
        TableRow, TableView, View, view,
    };
    use fleet_term::Palette;
    use gpui::{AppContext, Entity, TestAppContext, VisualTestContext};

    use super::super::Workspace;
    use crate::app::action_sheet::Stage;
    use crate::app::app_state::{ActionJob, ActionsJob, AppState, SessionApi};
    use crate::app::diff_pane::diff_result;
    use crate::app::keymap::bind_keys;
    use crate::app::pane_view::InputSink;
    use crate::app::theme::Theme;
    use crate::core::config::GuiConfig;
    use crate::core::pane::PaneModel;
    use crate::core::session::{AgentRow, PaneId, UiEvent};
    use crate::core::spawn::SpawnRequest;
    use crate::core::target::Target;
    use crate::core::worktree::{ARCHIVE, Actions, DIFF, NEW_AGENT, WORKTREE_LABEL};

    /// A session that records what the workspace asks and answers actions
    /// from a script.
    #[derive(Default)]
    struct Fake {
        agents: Mutex<Vec<AgentRow>>,
        /// Action calls, in order.
        calls: Mutex<Vec<RunAction>>,
        /// Scripted results by action name, in order; a diff fixture once
        /// a diff's script runs out.
        results: Mutex<HashMap<String, VecDeque<Result<ActionResult, String>>>>,
        restarts: Mutex<Vec<(PaneId, u64)>>,
        spawns: Mutex<Vec<SpawnRequest>>,
        actions: Mutex<Option<Actions>>,
        models: Mutex<HashMap<PaneId, Arc<Mutex<PaneModel>>>>,
    }

    impl Fake {
        fn calls(&self) -> Vec<RunAction> {
            self.calls.lock().unwrap().clone()
        }

        fn script(&self, name: &str, r: Result<ActionResult, String>) {
            self.results
                .lock()
                .unwrap()
                .entry(name.to_owned())
                .or_default()
                .push_back(r);
        }
    }

    impl InputSink for Fake {
        fn input(&self, _: PaneId, _: Vec<u8>) {}
        fn resize(&self, _: PaneId, _: u16, _: u16) {}
    }

    impl SessionApi for Fake {
        fn agents(&self) -> Vec<AgentRow> {
            self.agents.lock().unwrap().clone()
        }
        fn listed(&self) -> bool {
            true
        }
        fn spawn(&self, req: SpawnRequest, _: u16, _: u16, _: PaneId) -> Result<(), String> {
            self.spawns.lock().unwrap().push(req);
            Ok(())
        }
        fn kill(&self, _: u64) {}
        fn restart(&self, _: u64) {}
        fn restart_in_pane(&self, pane: PaneId, id: u64, _: u16, _: u16) -> Result<(), String> {
            self.restarts.lock().unwrap().push((pane, id));
            Ok(())
        }
        fn forget(&self, _: Vec<u64>) {}
        fn rename(&self, _: u64, _: String) {}
        fn clear_exited(&self) {}
        fn replaced(&self) -> HashMap<u64, u64> {
            HashMap::new()
        }
        fn open_pane(&self, pane: PaneId, _: Option<u64>) -> Arc<Mutex<PaneModel>> {
            let model = Arc::new(Mutex::new(PaneModel::new(Theme::dark().palette)));
            self.models.lock().unwrap().insert(pane, model.clone());
            model
        }
        fn attach(&self, _: PaneId, _: u64) {}
        fn close_pane(&self, _: PaneId) {}
        fn set_palette(&self, _: Palette) {}
        fn list_actions(&self) -> ActionsJob {
            let a = self.actions.lock().unwrap().clone();
            Box::new(move || a.ok_or_else(|| "down".to_owned()))
        }
        fn run_action(&self, call: RunAction) -> ActionJob {
            self.calls.lock().unwrap().push(call.clone());
            let scripted = self
                .results
                .lock()
                .unwrap()
                .get_mut(&call.name)
                .and_then(VecDeque::pop_front);
            let r = scripted.unwrap_or_else(|| Ok(diff_result(fixture(3))));
            Box::new(move || r)
        }
    }

    fn fixture(lines: usize) -> DiffView {
        let body: String = (0..lines).map(|i| format!("+line {i}\n")).collect();
        DiffView {
            title: "wt · fleet/x".into(),
            from_label: "origin/main".into(),
            to_label: "working tree".into(),
            files_changed: 2,
            additions: lines as u64,
            files: vec![
                FileDiff {
                    new_path: "a.rs".into(),
                    status: FileStatus::FileAdded as i32,
                    additions: lines as u32,
                    hunks: vec![Hunk {
                        new_start: 1,
                        new_lines: lines as u32,
                        body: body.into_bytes(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                FileDiff {
                    old_path: "big.rs".into(),
                    new_path: "big.rs".into(),
                    status: FileStatus::FileModified as i32,
                    hunks_omitted: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn spec(name: &str, params: Vec<ParamSpec>) -> ActionSpec {
        ActionSpec {
            name: name.into(),
            title: name.into(),
            params,
            target_param: "agent".into(),
            ..Default::default()
        }
    }

    fn all_actions() -> Actions {
        let agent = ParamSpec {
            key: "agent".into(),
            kind: ParamKind::ParamAgent as i32,
            ..Default::default()
        };
        let slug = ParamSpec {
            key: "slug".into(),
            kind: ParamKind::ParamString as i32,
            required: true,
            ..Default::default()
        };
        Actions::Listed(vec![
            spec(DIFF, vec![agent.clone()]),
            spec(ARCHIVE, vec![agent]),
            spec(NEW_AGENT, vec![slug]),
            spec("demo.ls", vec![]),
        ])
    }

    fn row(id: u64, state: AgentState, git: bool) -> AgentRow {
        let mut info = AgentInfo {
            id,
            program: "claude".into(),
            cwd: "/w".into(),
            created_unix_ms: id as i64,
            git: git.then(|| GitInfo {
                repo: "fleetd".into(),
                worktree: "wt".into(),
                branch: "fleet/x".into(),
                detached: false,
            }),
            ..Default::default()
        };
        info.set_state(state);
        if state == AgentState::Exited {
            info.exit_code = Some(0);
        }
        AgentRow {
            info,
            last_line: String::new(),
            last_output_unix_ms: None,
        }
    }

    struct Setup<'a> {
        ws: Entity<Workspace>,
        app: Entity<AppState>,
        cx: &'a mut VisualTestContext,
        ui: async_channel::Sender<UiEvent>,
        fake: Arc<Fake>,
        _dir: tempfile::TempDir,
    }

    fn setup(cx: &mut TestAppContext, rows: Vec<AgentRow>, actions: Actions) -> Setup<'_> {
        cx.update(bind_keys);
        let fake = Arc::new(Fake::default());
        *fake.agents.lock().unwrap() = rows;
        let dir = tempfile::tempdir().unwrap();
        let (ui, rx) = async_channel::unbounded();
        let session: Arc<dyn SessionApi> = fake.clone();
        let path = dir.path().to_owned();
        let config = GuiConfig {
            presets: Some(Vec::new()),
            ..GuiConfig::default()
        };
        let app = cx.new(|cx| {
            let mut a = AppState::new(session, rx, Target::Local, path, config, Theme::dark(), cx);
            a.set_actions(actions);
            a
        });
        cx.update(|cx| AppState::install(app.clone(), cx));
        let state = app.clone();
        let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(state, window, cx));
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        ui.try_send(UiEvent::AgentsChanged).unwrap();
        cx.run_until_parked();
        Setup {
            ws,
            app,
            cx,
            ui,
            fake,
            _dir: dir,
        }
    }

    impl Setup<'_> {
        fn show(&mut self, id: u64) {
            self.ws
                .update_in(self.cx, |w, window, cx| w.show_agent(id, window, cx));
            self.cx.run_until_parked();
        }

        fn panes(&mut self) -> Vec<PaneId> {
            self.app.read_with(self.cx, |a, _| a.layout().panes())
        }

        fn focused(&mut self) -> PaneId {
            self.app.read_with(self.cx, |a, _| a.focused())
        }

        fn diff(&mut self, pane: PaneId) -> Option<Entity<crate::app::diff_pane::DiffPane>> {
            self.app
                .read_with(self.cx, |a, _| a.diff_pane(pane).cloned())
        }

        fn set_rows(&mut self, rows: Vec<AgentRow>) {
            *self.fake.agents.lock().unwrap() = rows;
            self.ui.try_send(UiEvent::AgentsChanged).unwrap();
            self.cx.run_until_parked();
        }

        fn diff_calls(&self) -> usize {
            self.fake.calls().iter().filter(|c| c.name == DIFF).count()
        }
    }

    fn param(call: &RunAction, key: &str) -> Option<Value> {
        call.params.get(key).and_then(|v| v.value.clone())
    }

    #[gpui::test]
    fn show_changes_opens_a_diff_beside_the_agent_and_focuses_it_again(cx: &mut TestAppContext) {
        let mut s = setup(cx, vec![row(1, AgentState::Running, true)], all_actions());
        s.show(1);
        let terminal = s.focused();
        s.cx.simulate_keystrokes("cmd-shift-c");
        s.cx.run_until_parked();
        let panes = s.panes();
        assert_eq!(panes.len(), 2);
        let diff_pane = s.focused();
        assert_ne!(diff_pane, terminal);
        let diff = s.diff(diff_pane).expect("a diff pane");
        let (agent, rows, files) = diff.read_with(s.cx, |d, _| {
            let doc = d.doc().unwrap();
            (d.agent_id, doc.len(), doc.files().len())
        });
        assert_eq!((agent, files), (1, 2));
        // a.rs: header + hunk + 3 lines; big.rs collapsed: its header.
        assert_eq!(rows, 6);
        let call = &s.fake.calls()[0];
        assert_eq!(param(call, "agent"), Some(Value::Int(1)));
        assert_eq!(param(call, "against"), Some(Value::String("base".into())));
        // Saved without the diff pane: a relaunch shows the terminal only.
        s.app.update(s.cx, |a, _| a.save_now());

        // From the terminal again: the same diff pane, refreshed.
        s.app.update(s.cx, |a, cx| a.set_focused(terminal, cx));
        s.ws.update_in(s.cx, |w, window, cx| w.show_changes(1, window, cx));
        s.cx.run_until_parked();
        assert_eq!(s.panes().len(), 2);
        assert_eq!(s.focused(), diff_pane);
        assert_eq!(s.diff_calls(), 2);
    }

    #[gpui::test]
    fn the_saved_layout_leaves_the_diff_pane_out(cx: &mut TestAppContext) {
        let mut s = setup(cx, vec![row(1, AgentState::Running, true)], all_actions());
        s.show(1);
        s.ws.update_in(s.cx, |w, window, cx| w.show_changes(1, window, cx));
        s.cx.run_until_parked();
        let saved = s
            .app
            .read_with(s.cx, |a, _| a.gui_state().saved_tabs(&Target::Local));
        assert_eq!(saved.panes().len(), 1);
        assert_eq!(saved.agent(saved.panes()[0]), Some(1));
    }

    #[gpui::test]
    fn workflow_items_hide_for_a_daemon_without_actions(cx: &mut TestAppContext) {
        let mut s = setup(
            cx,
            vec![row(1, AgentState::Running, true)],
            Actions::Unsupported,
        );
        s.show(1);
        s.cx.simulate_keystrokes("cmd-shift-c");
        s.cx.run_until_parked();
        assert_eq!(s.panes().len(), 1);
        let (rows, state) = s.ws.read_with(s.cx, |w, cx| {
            (w.workflow_row_items(1, cx), w.palette_state(cx))
        });
        assert!(rows.is_empty());
        assert!(!state.can_show_changes && !state.has_actions);
        assert!(s.fake.calls().is_empty());
    }

    #[gpui::test]
    fn list_actions_unknown_request_reads_as_unsupported(cx: &mut TestAppContext) {
        let s = setup(cx, vec![], Actions::Unknown);
        *s.fake.actions.lock().unwrap() = Some(Actions::Unsupported);
        s.app.update(s.cx, |a, cx| a.list_actions(cx));
        s.cx.run_until_parked();
        let actions = s.app.read_with(s.cx, |a, _| a.actions().clone());
        assert_eq!(actions, Actions::Unsupported);
    }

    #[gpui::test]
    fn no_show_changes_outside_a_checkout(cx: &mut TestAppContext) {
        let mut s = setup(cx, vec![row(1, AgentState::Running, false)], all_actions());
        s.show(1);
        s.cx.simulate_keystrokes("cmd-shift-c");
        s.cx.run_until_parked();
        assert_eq!(s.panes().len(), 1);
    }

    #[gpui::test]
    fn cmd_r_refreshes_a_diff_pane_and_restarts_a_terminal(cx: &mut TestAppContext) {
        let mut s = setup(cx, vec![row(1, AgentState::Exited, true)], all_actions());
        s.show(1);
        let terminal = s.focused();
        s.ws.update_in(s.cx, |w, window, cx| w.show_changes(1, window, cx));
        s.cx.run_until_parked();
        assert_eq!(s.diff_calls(), 1);
        s.cx.simulate_keystrokes("cmd-r");
        s.cx.run_until_parked();
        assert_eq!(s.diff_calls(), 2, "⌘R in the diff pane refreshes");
        assert!(s.fake.restarts.lock().unwrap().is_empty());

        s.ws.update_in(s.cx, |w, window, cx| w.focus_pane(terminal, window, cx));
        // The terminal's agent is gone: ⌘R there restarts it.
        s.fake.models.lock().unwrap()[&terminal]
            .lock()
            .unwrap()
            .on_lost();
        s.cx.run_until_parked();
        s.cx.simulate_keystrokes("cmd-r");
        s.cx.run_until_parked();
        assert_eq!(s.diff_calls(), 2);
        assert_eq!(*s.fake.restarts.lock().unwrap(), [(terminal, 1)]);
    }

    #[gpui::test]
    fn a_shown_diff_refreshes_once_when_its_agent_finishes(cx: &mut TestAppContext) {
        let mut s = setup(cx, vec![row(1, AgentState::Running, true)], all_actions());
        s.show(1);
        s.ws.update_in(s.cx, |w, window, cx| w.show_changes(1, window, cx));
        s.cx.run_until_parked();
        assert_eq!(s.diff_calls(), 1);
        // Output alone does nothing.
        let mut busy = row(1, AgentState::Running, true);
        busy.last_line = "compiling".into();
        busy.last_output_unix_ms = Some(super::super::now_ms());
        s.set_rows(vec![busy]);
        assert_eq!(s.diff_calls(), 1);
        s.set_rows(vec![row(1, AgentState::Exited, true)]);
        assert_eq!(s.diff_calls(), 2);
        s.set_rows(vec![row(1, AgentState::Exited, true)]);
        assert_eq!(s.diff_calls(), 2, "once per transition");
    }

    #[gpui::test]
    fn the_diff_pane_keys_collapse_move_and_load_omitted_files(cx: &mut TestAppContext) {
        let mut s = setup(cx, vec![row(1, AgentState::Running, true)], all_actions());
        s.show(1);
        s.ws.update_in(s.cx, |w, window, cx| w.show_changes(1, window, cx));
        s.cx.run_until_parked();
        let pane = s.focused();
        let diff = s.diff(pane).unwrap();
        // Space collapses the selected file (a.rs).
        s.cx.simulate_keystrokes("space");
        s.cx.run_until_parked();
        assert_eq!(diff.read_with(s.cx, |d, _| d.doc().unwrap().len()), 2);
        // ↓ to big.rs; space loads it with the `file` param.
        let mut one = fixture(2);
        one.files.truncate(1);
        one.files[0].new_path = "big.rs".into();
        s.fake.script(DIFF, Ok(diff_result(one)));
        s.cx.simulate_keystrokes("down space");
        s.cx.run_until_parked();
        let last = s.fake.calls().last().cloned().unwrap();
        assert_eq!(param(&last, "file"), Some(Value::String("big.rs".into())));
        let (selected, rows) =
            diff.read_with(s.cx, |d, _| (d.selected_file, d.doc().unwrap().len()));
        assert_eq!(selected, 1);
        // a.rs collapsed (1) + big.rs header, hunk and 2 lines.
        assert_eq!(rows, 5);
        // ⌘A ⌘C copies the shown rows as a unified diff.
        s.cx.simulate_keystrokes("cmd-a cmd-c");
        let copied = s.cx.read_from_clipboard().and_then(|c| c.text()).unwrap();
        assert!(copied.contains("+line 1\n"), "{copied}");
    }

    #[gpui::test]
    fn archive_asks_then_offers_archive_anyway_on_a_refusal(cx: &mut TestAppContext) {
        let mut ended = row(1, AgentState::Exited, true);
        ended
            .info
            .labels
            .insert(WORKTREE_LABEL.into(), "/wt/x".into());
        let s = setup(cx, vec![ended], all_actions());
        assert_eq!(
            s.ws.read_with(s.cx, |w, cx| w.workflow_row_items(1, cx))
                .len(),
            3
        );
        s.fake.script(
            ARCHIVE,
            Ok(ActionResult {
                outcome: Some(Outcome::Refused(ActionRefused {
                    reason: "uncommitted changes".into(),
                    retry: HashMap::from([(
                        "force".to_owned(),
                        ParamValue {
                            value: Some(Value::Bool(true)),
                        },
                    )]),
                })),
            }),
        );
        s.fake.script(
            ARCHIVE,
            Ok(ActionResult {
                outcome: Some(Outcome::Done(ActionDone {
                    message: "archived /wt/x".into(),
                })),
            }),
        );
        s.ws.update_in(s.cx, |w, window, cx| w.open_archive(1, window, cx));
        s.cx.run_until_parked();
        let sheet =
            s.ws.read_with(s.cx, |w, _| w.action_sheet().cloned())
                .unwrap();
        assert!(matches!(
            sheet.read_with(s.cx, |x, _| x.stage.clone()),
            Stage::Confirm { .. }
        ));
        sheet.update(s.cx, |x, cx| x.submit(cx));
        s.cx.run_until_parked();
        let stage = sheet.read_with(s.cx, |x, _| x.stage.clone());
        assert!(
            matches!(&stage, Stage::Refused { reason, .. } if reason == "uncommitted changes"),
            "{stage:?}"
        );
        assert_eq!(param(&s.fake.calls()[0], "force"), None);
        sheet.update(s.cx, |x, cx| x.submit(cx));
        s.cx.run_until_parked();
        let second = &s.fake.calls()[1];
        assert_eq!(param(second, "force"), Some(Value::Bool(true)));
        assert_eq!(param(second, "agent"), Some(Value::Int(1)));
        assert!(s.ws.read_with(s.cx, |w, _| w.action_sheet().is_none()));
        let notice = s.app.read_with(s.cx, |a, _| a.notice().map(str::to_owned));
        assert_eq!(notice.as_deref(), Some("archived /wt/x"));
    }

    #[gpui::test]
    fn run_action_renders_a_form_and_handles_each_result(cx: &mut TestAppContext) {
        let mut s = setup(cx, vec![row(1, AgentState::Running, true)], all_actions());
        let open_form = |s: &mut Setup<'_>, name: &str| {
            s.cx.dispatch_action(crate::app::keymap::RunAction);
            s.cx.run_until_parked();
            let sheet =
                s.ws.read_with(s.cx, |w, _| w.action_sheet().cloned())
                    .unwrap();
            let Stage::Choose(list) = sheet.read_with(s.cx, |x, _| x.stage.clone()) else {
                panic!("a list");
            };
            let spec = list.iter().find(|a| a.name == name).unwrap().clone();
            sheet.update(s.cx, |x, cx| x.pick(&spec, None, cx));
            sheet
        };
        // A required field is checked before anything runs.
        let sheet = open_form(&mut s, NEW_AGENT);
        sheet.update(s.cx, |x, cx| x.submit(cx));
        s.cx.run_until_parked();
        assert!(s.fake.calls().is_empty());
        // Spawned: the new agent shows in the focused pane.
        let mut spawned = AgentInfo {
            id: 9,
            ..Default::default()
        };
        spawned.set_state(AgentState::Running);
        s.fake.script(
            NEW_AGENT,
            Ok(ActionResult {
                outcome: Some(Outcome::Spawned(Box::new(ActionSpawned {
                    agent: Some(spawned),
                    message: String::new(),
                }))),
            }),
        );
        sheet.update(s.cx, |x, cx| {
            x.set_field("slug", "fix-1", cx);
            x.submit(cx);
        });
        s.cx.run_until_parked();
        assert_eq!(
            param(&s.fake.calls()[0], "slug"),
            Some(Value::String("fix-1".into()))
        );
        let shown = s.app.read_with(s.cx, |a, _| a.layout().agent(a.focused()));
        assert_eq!(shown, Some(9));
        // A table view opens a view pane, which refreshes by running the
        // call again.
        let table = TableView {
            title: "worktrees".into(),
            columns: vec!["path".into()],
            rows: vec![TableRow {
                cells: vec!["/wt/a".into()],
                ..Default::default()
            }],
        };
        let result = ActionResult {
            outcome: Some(Outcome::View(View {
                kind: Some(view::Kind::Table(table)),
            })),
        };
        s.fake.script("demo.ls", Ok(result.clone()));
        s.fake.script("demo.ls", Ok(result));
        let sheet = open_form(&mut s, "demo.ls");
        sheet.update(s.cx, |x, cx| x.submit(cx));
        s.cx.run_until_parked();
        let pane = s.focused();
        let viewp = s.diff(pane).expect("a view pane");
        let lines = viewp.read_with(s.cx, |v, _| v.text().map(<[String]>::to_vec));
        assert_eq!(lines, Some(vec!["PATH".to_owned(), "/wt/a".to_owned()]));
        s.cx.simulate_keystrokes("cmd-r");
        s.cx.run_until_parked();
        let ls = s
            .fake
            .calls()
            .iter()
            .filter(|c| c.name == "demo.ls")
            .count();
        assert_eq!(ls, 2);
        // An error stays in the sheet.
        s.fake.script("demo.ls", Err("forbidden: no".into()));
        let sheet = open_form(&mut s, "demo.ls");
        sheet.update(s.cx, |x, cx| x.submit(cx));
        s.cx.run_until_parked();
        let error = sheet.read_with(s.cx, |x, _| x.error.clone());
        assert_eq!(error.as_deref(), Some("forbidden: no"));
    }

    #[gpui::test]
    fn the_new_agent_sheet_spawns_into_a_new_worktree(cx: &mut TestAppContext) {
        let s = setup(cx, vec![], all_actions());
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        let dir = repo.path().to_string_lossy().into_owned();
        s.ws.update_in(s.cx, |w, window, cx| w.open_new_agent(window, cx));
        s.cx.run_until_parked();
        let sheet =
            s.ws.read_with(s.cx, |w, _| w.sheet.as_ref().map(|(x, _)| x.clone()))
                .unwrap();
        sheet.update(s.cx, |x, cx| {
            x.dir.update(cx, |f, cx| f.set_text(dir.clone(), cx));
            x.command
                .update(cx, |f, cx| f.set_text("claude --model big".into(), cx));
        });
        s.cx.run_until_parked();
        // Typed after the generated name: the slug follows it.
        sheet.update(s.cx, |x, cx| {
            x.name.update(cx, |f, cx| f.set_text("Fix It".into(), cx));
        });
        s.cx.run_until_parked();
        let shown = sheet.read_with(s.cx, |x, _| x.worktree.as_ref().map(|w| w.in_checkout));
        assert_eq!(shown, Some(true));
        sheet.update(s.cx, |x, cx| {
            x.toggle_worktree(cx);
            x.worktree
                .as_ref()
                .unwrap()
                .base
                .update(cx, |f, cx| f.set_text("origin/dev".into(), cx));
            x.submit(cx);
        });
        s.cx.run_until_parked();
        assert!(
            s.fake.spawns.lock().unwrap().is_empty(),
            "not a plain spawn"
        );
        let call = s.fake.calls().pop().unwrap();
        assert_eq!(call.name, NEW_AGENT);
        assert_eq!(param(&call, "slug"), Some(Value::String("fix-it".into())));
        assert_eq!(
            param(&call, "base"),
            Some(Value::String("origin/dev".into()))
        );
        assert_eq!(param(&call, "repo"), Some(Value::String(dir.clone())));
        assert_eq!(param(&call, "name"), Some(Value::String("Fix It".into())));
        assert_eq!(
            param(&call, "command"),
            Some(Value::List(fleet_proto::StringList {
                items: vec!["claude".into(), "--model".into(), "big".into()]
            }))
        );
        // Remembered for the target: the next sheet starts checked.
        let remembered = s.app.read_with(s.cx, |a, _| a.new_worktree_remembered());
        assert!(remembered);
    }
}
