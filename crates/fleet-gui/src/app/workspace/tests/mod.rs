// Tests for the workspace window, split by area (#146); the shared
// fake session and setup helpers live here.

use super::{KillSheet, Modal, SendSheet, Workspace, now_ms};
use crate::app::app_state::{AppState, SAVE_DEBOUNCE, SessionApi};
use crate::app::keymap::{FocusPane1, FocusPane2, Kill, RemoveExited, SplitRight, bind_keys};
use crate::app::new_agent::NewAgentSheet;
use crate::app::pane_view::InputSink;
use crate::app::sidebar_view::TabMark;
use crate::app::text_field::TextField;
use crate::app::theme::Theme;
use crate::core::broadcast::{RETURN_DELAY_MS, SendPlan};
use crate::core::config::GuiConfig;
use crate::core::feedback;
use crate::core::layout::Layout;
use crate::core::menus::{MenuEntry, PaneMenuItem, RowMenuItem};
use crate::core::pane::PaneModel;
use crate::core::presets::Preset;
use crate::core::session::{AgentRow, ConnState, PaneId, UiEvent};
use crate::core::settings::{Change, MotionChoice};
use crate::core::shortcuts::{EmptyPane, key_glyphs};
use crate::core::spawn::SpawnRequest;
use crate::core::state::GuiState;
use crate::core::target::Target;
use fleet_proto::{AgentInfo, AgentState, AttachOk};
use fleet_term::Palette;
use gpui::{
    AppContext, ClipboardItem, Entity, Modifiers, MouseButton, Point, ScrollDelta,
    ScrollWheelEvent, TestAppContext, TouchPhase, VisualTestContext, point,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod agents;
mod focus;
mod graph;
mod kill;
mod menus;
mod modal;
mod new_agent;
mod output;
mod palette;
mod panes;
mod peek;
mod remove_exited;
mod restart;
mod send;
mod sheets;
mod shortcuts;
mod sidebar;
mod spawn_approval;
mod toasts;
mod window;

#[derive(Clone, Debug, PartialEq)]
enum Call {
    Open(PaneId, Option<u64>),
    Attach(PaneId, u64),
    Close(PaneId),
    Kill(u64),
    Restart(u64),
    RestartInPane {
        pane: PaneId,
        agent_id: u64,
        size: (u16, u16),
    },
    Forget(Vec<u64>),
    Rename(u64, String),
    SetLabels(u64, Vec<(String, String)>, Vec<String>),
    /// Typed into an agent (Send to Agents…).
    Send(u64, Vec<u8>),
    /// An answer to an agent's open question: agent, question id, text.
    Answer(u64, u64, String),
    ClearExited,
    /// Approved (`true`) or denied a pending spawn.
    Decide(u64, bool),
    Spawn {
        req: SpawnRequest,
        size: (u16, u16),
        pane: PaneId,
    },
}
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Call>>,
    inputs: Mutex<Vec<Vec<u8>>>,
    /// The same input, with the pane it went to.
    pane_inputs: Mutex<Vec<(PaneId, Vec<u8>)>>,
    agents: Mutex<Vec<AgentRow>>,
    models: Mutex<HashMap<PaneId, Arc<Mutex<PaneModel>>>>,
    spawn_error: Mutex<Option<String>>,
    listed: std::sync::atomic::AtomicBool,
    lists: std::sync::atomic::AtomicU64,
    replaced: Mutex<HashMap<u64, u64>>,
    update_available: std::sync::atomic::AtomicBool,
    daemon_restarts: std::sync::atomic::AtomicUsize,
    restart_pending: std::sync::atomic::AtomicBool,
    /// What `read_output` gives per agent (unset: an error).
    outputs: Mutex<HashMap<u64, Result<String, String>>>,
    /// What `search_output` gives, and the searches asked for.
    search_hits: Mutex<Vec<fleet_proto::SearchHit>>,
    searches: Mutex<Vec<(String, bool)>>,
    pending: Mutex<Vec<fleet_proto::PendingSpawn>>,
}
impl Fake {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn model(&self, pane: PaneId) -> Arc<Mutex<PaneModel>> {
        self.models.lock().unwrap()[&pane].clone()
    }
}
impl InputSink for Fake {
    fn input(&self, pane: PaneId, bytes: Vec<u8>) {
        self.pane_inputs.lock().unwrap().push((pane, bytes.clone()));
        self.inputs.lock().unwrap().push(bytes);
    }

    fn resize(&self, _pane: PaneId, _cols: u16, _rows: u16) {}
}
impl SessionApi for Fake {
    fn agents(&self) -> Vec<AgentRow> {
        self.agents.lock().unwrap().clone()
    }

    fn listed(&self) -> bool {
        self.listed.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn lists(&self) -> u64 {
        self.lists.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn update_available(&self) -> bool {
        self.update_available
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn restart_daemon(&self) {
        self.daemon_restarts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.restart_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn restart_pending(&self) -> bool {
        self.restart_pending
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn spawn(&self, req: SpawnRequest, cols: u16, rows: u16, pane: PaneId) -> Result<(), String> {
        if let Some(e) = self.spawn_error.lock().unwrap().clone() {
            return Err(e);
        }
        self.calls.lock().unwrap().push(Call::Spawn {
            req,
            size: (cols, rows),
            pane,
        });
        Ok(())
    }

    fn kill(&self, agent_id: u64) {
        self.calls.lock().unwrap().push(Call::Kill(agent_id));
    }

    fn restart(&self, agent_id: u64) {
        self.calls.lock().unwrap().push(Call::Restart(agent_id));
    }

    fn restart_in_pane(
        &self,
        pane: PaneId,
        agent_id: u64,
        cols: u16,
        rows: u16,
    ) -> Result<(), String> {
        self.calls.lock().unwrap().push(Call::RestartInPane {
            pane,
            agent_id,
            size: (cols, rows),
        });
        Ok(())
    }

    fn forget(&self, ids: Vec<u64>) {
        self.calls.lock().unwrap().push(Call::Forget(ids));
    }

    fn search_output(
        &self,
        query: String,
        case_sensitive: bool,
    ) -> crate::app::app_state::SearchJob {
        self.searches.lock().unwrap().push((query, case_sensitive));
        let hits = self.search_hits.lock().unwrap().clone();
        Box::new(move || {
            Ok(fleet_proto::SearchHits {
                hits,
                truncated: false,
            })
        })
    }

    fn read_output(&self, agent_id: u64) -> crate::app::app_state::OutputJob {
        let out = self.outputs.lock().unwrap().get(&agent_id).cloned();
        Box::new(move || out.unwrap_or_else(|| Err("no such agent".into())))
    }

    fn rename(&self, agent_id: u64, name: String) {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Rename(agent_id, name));
    }

    fn set_labels(&self, agent_id: u64, set: HashMap<String, String>, remove: Vec<String>) {
        let mut set: Vec<_> = set.into_iter().collect();
        set.sort();
        self.calls
            .lock()
            .unwrap()
            .push(Call::SetLabels(agent_id, set, remove));
    }

    fn answer(&self, agent_id: u64, question: u64, text: String) {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Answer(agent_id, question, text));
    }

    fn clear_exited(&self) {
        self.calls.lock().unwrap().push(Call::ClearExited);
    }

    fn pending_spawns(&self) -> Vec<fleet_proto::PendingSpawn> {
        self.pending.lock().unwrap().clone()
    }

    fn decide_spawn(&self, id: u64, approve: bool) {
        self.calls.lock().unwrap().push(Call::Decide(id, approve));
    }

    fn send_input(&self, agent_id: u64, data: Vec<u8>) {
        self.calls.lock().unwrap().push(Call::Send(agent_id, data));
    }

    fn replaced(&self) -> HashMap<u64, u64> {
        self.replaced.lock().unwrap().clone()
    }

    fn open_pane(&self, pane: PaneId, agent_id: Option<u64>) -> Arc<Mutex<PaneModel>> {
        self.calls.lock().unwrap().push(Call::Open(pane, agent_id));
        let model = Arc::new(Mutex::new(PaneModel::new(Theme::dark().palette)));
        self.models.lock().unwrap().insert(pane, model.clone());
        model
    }

    fn attach(&self, pane: PaneId, agent_id: u64) {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Attach(pane, agent_id));
    }

    fn close_pane(&self, pane: PaneId) {
        self.calls.lock().unwrap().push(Call::Close(pane));
    }

    fn set_palette(&self, _palette: Palette) {}
}
fn ended(id: u64, cwd: &str, created: i64, state: AgentState) -> AgentRow {
    let mut r = row(id, cwd, created);
    r.info.set_state(state);
    if state == AgentState::Exited {
        r.info.exit_code = Some(3);
    }
    r
}
fn row(id: u64, cwd: &str, created: i64) -> AgentRow {
    let mut info = AgentInfo {
        id,
        program: "sh".into(),
        cwd: cwd.into(),
        created_unix_ms: created,
        ..Default::default()
    };
    info.set_state(AgentState::Running);
    AgentRow {
        info,
        last_line: format!("line of {id}"),
        last_output_unix_ms: None,
    }
}
struct Setup<'a> {
    ws: Entity<Workspace>,
    app: Entity<AppState>,
    cx: &'a mut VisualTestContext,
    ui: async_channel::Sender<UiEvent>,
}
fn app_state(cx: &mut TestAppContext, fake: &Arc<Fake>, dir: &Path) -> Entity<AppState> {
    let (_tx, rx) = async_channel::unbounded();
    let session: Arc<dyn SessionApi> = fake.clone();
    let dir = dir.to_owned();
    cx.new(|cx| {
        AppState::new(
            session,
            rx,
            Target::Local,
            dir,
            GuiConfig::default(),
            Theme::dark(),
            cx,
        )
    })
}
fn setup<'a>(cx: &'a mut TestAppContext, fake: &Arc<Fake>, dir: &Path) -> Setup<'a> {
    setup_with(cx, fake, dir, GuiConfig::default())
}
fn setup_with<'a>(
    cx: &'a mut TestAppContext,
    fake: &Arc<Fake>,
    dir: &Path,
    config: GuiConfig,
) -> Setup<'a> {
    cx.update(bind_keys);
    // No presets unless the test names some: never this Mac's PATH.
    let config = GuiConfig {
        presets: config.presets.or(Some(Vec::new())),
        ..config
    };
    let (ui, rx) = async_channel::unbounded();
    let session: Arc<dyn SessionApi> = fake.clone();
    let dir = dir.to_owned();
    let app =
        cx.new(|cx| AppState::new(session, rx, Target::Local, dir, config, Theme::dark(), cx));
    cx.update(|cx| AppState::install(app.clone(), cx));
    let state = app.clone();
    let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(state, window, cx));
    // Focus events need an active window.
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    Setup { ws, app, cx, ui }
}
fn panes(s: &mut Setup<'_>) -> Vec<PaneId> {
    s.app.read_with(s.cx, |a, _| a.layout().panes())
}
fn focused(s: &mut Setup<'_>) -> PaneId {
    s.app.read_with(s.cx, |a, _| a.focused())
}
fn center(s: &mut Setup<'_>, selector: &'static str) -> Point<gpui::Pixels> {
    s.cx.run_until_parked();
    s.cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} is drawn"))
        .center()
}

fn click(s: &mut Setup<'_>, selector: &'static str) {
    let at = center(s, selector);
    s.cx.simulate_click(at, Modifiers::none());
    s.cx.run_until_parked();
}

fn agents(s: &mut Setup<'_>, fake: &Fake, rows: Vec<AgentRow>) {
    *fake.agents.lock().unwrap() = rows;
    s.ui.try_send(UiEvent::AgentsChanged).unwrap();
    s.cx.run_until_parked();
}

fn kills(fake: &Fake) -> Vec<u64> {
    fake.calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Kill(id) => Some(id),
            _ => None,
        })
        .collect()
}

/// The agent the kill question is open for.
fn kill_asked(s: &mut Setup<'_>) -> Option<u64> {
    s.cx.run_until_parked();
    s.ws.read_with(s.cx, |w, cx| {
        w.kill_sheet()
            .and_then(|k| k.read(cx).agent_ids.first().copied())
    })
}

fn sheet_text(s: &mut Setup<'_>, field: impl Fn(&NewAgentSheet) -> Entity<TextField>) -> String {
    let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
    let field = sheet.read_with(s.cx, |sh, _| field(sh));
    field.read_with(s.cx, |f, _| f.text().to_owned())
}

fn set_sheet_text(
    s: &mut Setup<'_>,
    field: impl Fn(&NewAgentSheet) -> Entity<TextField>,
    text: &str,
) {
    let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
    let field = sheet.read_with(s.cx, |sh, _| field(sh));
    let text = text.to_owned();
    field.update(s.cx, |f, cx| f.set_text(text, cx));
    s.cx.run_until_parked();
}

fn last_spawn(fake: &Fake) -> SpawnRequest {
    fake.calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Spawn { req, .. } => Some(req),
            _ => None,
        })
        .next_back()
        .expect("a spawn")
}

fn named(id: u64, name: &str, labels: &[(&str, &str)]) -> AgentRow {
    let mut r = row(id, "/w", id as i64);
    r.info.labels.insert("name".into(), name.into());
    for (k, v) in labels {
        r.info.labels.insert((*k).into(), (*v).into());
    }
    r
}

fn double_click(s: &mut Setup<'_>, selector: &'static str) {
    let at = center(s, selector);
    for click_count in [1, 2] {
        s.cx.simulate_event(gpui::MouseDownEvent {
            position: at,
            button: MouseButton::Left,
            modifiers: Modifiers::none(),
            click_count,
            first_mouse: false,
        });
        s.cx.simulate_event(gpui::MouseUpEvent {
            position: at,
            button: MouseButton::Left,
            modifiers: Modifiers::none(),
            click_count,
        });
    }
    s.cx.run_until_parked();
}

fn rename_to(s: &mut Setup<'_>, name: &str) {
    let sheet =
        s.ws.read_with(s.cx, |w, _| w.rename_sheet().cloned())
            .expect("rename sheet open");
    let field = sheet.read_with(s.cx, |sh, _| sh.name.clone());
    let name = name.to_owned();
    field.update(s.cx, |f, cx| f.set_text(name, cx));
    s.cx.simulate_keystrokes("enter");
}

fn asking(id: u64, question_id: u64) -> AgentRow {
    let mut r = row(id, "/w", id as i64);
    r.info.reported = Some(fleet_proto::Reported {
        state: fleet_proto::ReportedState::ReportedWaiting.into(),
        message: String::new(),
        at_unix_ms: 1,
        question: Some(fleet_proto::Question {
            id: question_id,
            text: "Which branch?".into(),
            choices: vec!["main".into(), "dev".into()],
        }),
    });
    r
}

fn labels_sheet(s: &mut Setup<'_>) -> Entity<super::LabelsSheet> {
    s.ws.read_with(s.cx, |w, _| w.labels_sheet().cloned())
        .expect("labels sheet open")
}

fn spawn_via_sheet(s: &mut Setup<'_>, dir: &str) {
    s.cx.simulate_keystrokes("cmd-n");
    let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
    let field = sheet.read_with(s.cx, |sh, _| sh.dir.clone());
    let dir = dir.to_owned();
    field.update(s.cx, |f, cx| f.set_text(dir, cx));
    s.cx.simulate_keystrokes("enter");
}

fn attach_model(fake: &Fake, pane: PaneId, agent_id: u64) {
    fake.model(pane).lock().unwrap().on_attached(&AttachOk {
        agent_id,
        cols: 80,
        rows: 24,
        primary: true,
        ..Default::default()
    });
}

fn drawn(s: &mut Setup<'_>, selector: &'static str) -> bool {
    s.cx.run_until_parked();
    s.cx.debug_bounds(selector).is_some()
}

/// Picks `item` from `agent_id`'s row menu (the native menu itself
/// cannot be shown headless).
fn pick_row(s: &mut Setup<'_>, agent_id: u64, item: RowMenuItem) {
    s.ws.update_in(s.cx, |ws, window, cx| {
        let epoch = ws.epoch;
        ws.row_menu_pick(agent_id, item, epoch, window, cx);
    });
    s.cx.run_until_parked();
}

fn row_items(s: &mut Setup<'_>, agent_id: u64) -> Vec<RowMenuItem> {
    s.ws.read_with(s.cx, |w, cx| w.menu_items(agent_id, cx))
        .into_iter()
        .filter_map(|e| match e {
            MenuEntry::Item(i) => Some(i),
            MenuEntry::Separator => None,
        })
        .collect()
}

fn shown(s: &mut Setup<'_>, pane: PaneId) -> Option<u64> {
    s.app.read_with(s.cx, |a, _| a.layout().agent(pane))
}

fn sends(fake: &Fake) -> Vec<(u64, Vec<u8>)> {
    fake.calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Send(id, data) => Some((id, data)),
            _ => None,
        })
        .collect()
}

/// A `ListAgents` reply with `rows`.
fn list(s: &mut Setup<'_>, fake: &Fake, rows: Vec<AgentRow>) {
    fake.listed.store(true, std::sync::atomic::Ordering::SeqCst);
    fake.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    agents(s, fake, rows);
}

/// The tabs the last frame drew.
fn tabs_drawn(s: &mut Setup<'_>) -> Vec<u64> {
    s.cx.run_until_parked();
    s.ws.read_with(s.cx, |w, _| {
        w.drawn_sidebar
            .iter()
            .flat_map(|(_, ids)| ids.clone())
            .collect()
    })
}

fn project(mut r: AgentRow, p: &str) -> AgentRow {
    r.info.labels.insert("project".into(), p.into());
    r
}

fn exited(id: u64, code: i32) -> AgentRow {
    let mut r = ended(id, "/w", id as i64, AgentState::Exited);
    r.info.exit_code = Some(code);
    r
}

fn listed(s: &mut Setup<'_>, fake: &Fake, rows: Vec<AgentRow>) {
    fake.listed.store(true, std::sync::atomic::Ordering::SeqCst);
    agents(s, fake, rows);
}
