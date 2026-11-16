//! The split chooser (issue #30): a pane opened by a split asks what to
//! start in it. 1 a new agent, 2 an agent not on screen, 3 a terminal;
//! Esc closes the pane.
#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fleet_gui::app::{AppState, InputSink, SessionApi, Theme, Workspace, bind_keys};
use fleet_gui::core::config::GuiConfig;
use fleet_gui::core::pane::PaneModel;
use fleet_gui::core::session::{AgentRow, PaneId, UiEvent};
use fleet_gui::core::spawn::{KIND_LABEL, SHELL_KIND, SpawnRequest};
use fleet_gui::core::split_chooser::Stage;
use fleet_gui::core::target::Target;
use fleet_proto::{AgentInfo, AgentState};
use fleet_term::Palette;
use gpui::{AppContext, Entity, Modifiers, TestAppContext, VisualTestContext};

#[derive(Clone, Debug, PartialEq)]
enum Call {
    Attach(PaneId, u64),
    Spawn(PaneId, SpawnRequest),
}

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Call>>,
    agents: Mutex<Vec<AgentRow>>,
}

impl InputSink for Fake {
    fn input(&self, _: PaneId, _: Vec<u8>) {}
    fn resize(&self, _: PaneId, _: u16, _: u16) {}
}

impl SessionApi for Fake {
    fn agents(&self) -> Vec<AgentRow> {
        self.agents.lock().unwrap().clone()
    }
    fn spawn(&self, req: SpawnRequest, _: u16, _: u16, pane: PaneId) -> Result<(), String> {
        self.calls.lock().unwrap().push(Call::Spawn(pane, req));
        Ok(())
    }
    fn kill(&self, _: u64) {}
    fn restart(&self, _: u64) {}
    fn restart_in_pane(&self, _: PaneId, _: u64, _: u16, _: u16) -> Result<(), String> {
        Ok(())
    }
    fn forget(&self, _: Vec<u64>) {}
    fn clear_exited(&self) {}
    fn rename(&self, _: u64, _: String) {}
    fn replaced(&self) -> HashMap<u64, u64> {
        HashMap::new()
    }
    fn open_pane(&self, _: PaneId, _: Option<u64>) -> Arc<Mutex<PaneModel>> {
        Arc::new(Mutex::new(PaneModel::new(Theme::dark().palette)))
    }
    fn attach(&self, pane: PaneId, agent_id: u64) {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Attach(pane, agent_id));
    }
    fn close_pane(&self, _: PaneId) {}
    fn set_palette(&self, _: Palette) {}
}

fn agent(id: u64, program: &str, cwd: &str, created: i64) -> AgentRow {
    let mut info = AgentInfo {
        id,
        program: program.into(),
        cwd: cwd.into(),
        created_unix_ms: created,
        ..Default::default()
    };
    info.set_state(AgentState::Running);
    AgentRow {
        info,
        last_line: String::new(),
        last_output_unix_ms: None,
    }
}

struct Setup<'a> {
    ws: Entity<Workspace>,
    app: Entity<AppState>,
    fake: Arc<Fake>,
    cx: &'a mut VisualTestContext,
    _dir: tempfile::TempDir,
    _ui: async_channel::Sender<UiEvent>,
}

impl Setup<'_> {
    fn panes(&mut self) -> Vec<PaneId> {
        self.app.read_with(self.cx, |a, _| a.layout().panes())
    }

    fn focused(&mut self) -> PaneId {
        self.app.read_with(self.cx, |a, _| a.focused())
    }

    /// The stage of `pane`'s chooser, if it has one.
    fn chooser(&mut self, pane: PaneId) -> Option<Stage> {
        self.ws.read_with(self.cx, |ws, cx| {
            ws.chooser(pane).map(|c| c.read(cx).chooser().stage())
        })
    }

    /// The agent list's titles, in order.
    fn listed(&mut self, pane: PaneId) -> Vec<String> {
        self.ws.read_with(self.cx, |ws, cx| {
            let list = ws.chooser(pane).unwrap().read(cx).agent_list().unwrap();
            let p = list.read(cx).picker();
            p.ranked()
                .iter()
                .map(|r| p.items()[r.item].title.clone())
                .collect()
        })
    }

    fn chooser_focused(&mut self, pane: PaneId) -> bool {
        let ws = self.ws.clone();
        self.cx.update(|window, cx| {
            ws.read(cx)
                .chooser(pane)
                .is_some_and(|c| c.read(cx).contains_focus(window, cx))
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.fake.calls.lock().unwrap().clone()
    }

    fn spawns(&self) -> Vec<(PaneId, SpawnRequest)> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Spawn(p, r) => Some((p, r)),
                Call::Attach(..) => None,
            })
            .collect()
    }

    fn type_text(&mut self, text: &str) {
        self.cx.simulate_input(text);
        self.cx.run_until_parked();
    }

    fn keys(&mut self, keys: &str) {
        self.cx.simulate_keystrokes(keys);
        self.cx.run_until_parked();
    }

    /// Shows `agent` in the focused pane, as a sidebar click does.
    fn show(&mut self, agent: u64) {
        self.ws
            .update_in(self.cx, |ws, window, cx| ws.show_agent(agent, window, cx));
        self.cx.run_until_parked();
    }

    /// Pane 1 shows agent 1 (in /work/alpha), then ⌘D: the new pane.
    fn split_from_alpha(&mut self) -> PaneId {
        self.show(1);
        self.keys("cmd-d");
        let pane = self.focused();
        assert_eq!(self.panes().len(), 2);
        pane
    }
}

fn setup(cx: &mut TestAppContext, agents: Vec<AgentRow>) -> Setup<'_> {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    *fake.agents.lock().unwrap() = agents;
    cx.update(bind_keys);
    let (ui, rx) = async_channel::unbounded();
    let session: Arc<dyn SessionApi> = fake.clone();
    let state_dir = dir.path().to_owned();
    let app = cx.new(|cx| {
        AppState::new(
            session,
            rx,
            Target::Local,
            state_dir,
            GuiConfig::default(),
            Theme::dark(),
            cx,
        )
    });
    cx.update(|cx| AppState::install(app.clone(), cx));
    let state = app.clone();
    let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(state, window, cx));
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    Setup {
        ws,
        app,
        fake,
        cx,
        _dir: dir,
        _ui: ui,
    }
}

fn fleet() -> Vec<AgentRow> {
    vec![
        agent(1, "sh", "/work/alpha", 1),
        agent(2, "claude", "/work/beta", 2),
        agent(3, "codex", "/work/gamma", 3),
    ]
}

#[gpui::test]
fn a_split_asks_what_to_start_and_the_first_pane_does_not(cx: &mut TestAppContext) {
    let mut s = setup(cx, fleet());
    let first = s.focused();
    assert_eq!(s.chooser(first), None);
    let pane = s.split_from_alpha();
    assert_ne!(pane, first);
    assert_eq!(s.chooser(pane), Some(Stage::Choices));
    assert!(s.chooser_focused(pane));
    assert!(s.cx.debug_bounds("split-chooser").is_some());
}

#[gpui::test]
fn one_starts_the_default_command_in_the_split_panes_folder(cx: &mut TestAppContext) {
    let mut s = setup(cx, fleet());
    let pane = s.split_from_alpha();
    s.keys("1");
    let spawns = s.spawns();
    assert_eq!(spawns.len(), 1);
    let (into, req) = &spawns[0];
    assert_eq!(*into, pane);
    assert_eq!(req.program, "claude");
    assert_eq!(req.cwd, "/work/alpha");
    assert_eq!(s.chooser(pane), None);
    assert_eq!(s.panes().len(), 2);
    assert_eq!(s.focused(), pane);
}

#[gpui::test]
fn three_starts_a_terminal_in_the_pane_without_another_split(cx: &mut TestAppContext) {
    let mut s = setup(cx, fleet());
    let pane = s.split_from_alpha();
    s.keys("3");
    let spawns = s.spawns();
    assert_eq!(spawns.len(), 1);
    let (into, req) = &spawns[0];
    assert_eq!(*into, pane);
    assert_eq!(req.labels[KIND_LABEL], SHELL_KIND);
    assert_eq!(req.args, ["-l"]);
    assert_eq!(req.cwd, "/work/alpha");
    assert_eq!(s.panes().len(), 2);
    assert_eq!(s.chooser(pane), None);
}

#[gpui::test]
fn two_lists_the_agents_off_screen_and_enter_shows_one(cx: &mut TestAppContext) {
    let mut s = setup(cx, fleet());
    let pane = s.split_from_alpha();
    s.keys("2");
    assert_eq!(s.chooser(pane), Some(Stage::Agents));
    // Agent 1 is on screen; the others newest first.
    assert_eq!(
        s.listed(pane),
        ["codex in /work/gamma", "claude in /work/beta"]
    );
    // Digits are the query's now.
    s.type_text("beta");
    assert_eq!(s.listed(pane), ["claude in /work/beta"]);
    s.keys("enter");
    assert!(s.calls().contains(&Call::Attach(pane, 2)));
    assert_eq!(s.chooser(pane), None);
    assert!(s.spawns().is_empty());
}

#[gpui::test]
fn esc_closes_the_new_pane_and_from_the_list_goes_back_first(cx: &mut TestAppContext) {
    let mut s = setup(cx, fleet());
    let first = s.focused();
    let pane = s.split_from_alpha();
    s.keys("2");
    s.keys("escape");
    assert_eq!(s.chooser(pane), Some(Stage::Choices));
    assert!(s.chooser_focused(pane));
    s.keys("escape");
    assert_eq!(s.panes(), [first]);
    assert_eq!(s.focused(), first);
    assert!(s.spawns().is_empty());
}

#[gpui::test]
fn with_every_agent_on_screen_two_does_nothing(cx: &mut TestAppContext) {
    let mut s = setup(cx, vec![agent(1, "sh", "/work/alpha", 1)]);
    let pane = s.split_from_alpha();
    s.keys("2");
    assert_eq!(s.chooser(pane), Some(Stage::Choices));
}

#[gpui::test]
fn a_click_picks_a_choice(cx: &mut TestAppContext) {
    let mut s = setup(cx, fleet());
    let pane = s.split_from_alpha();
    let at = s.cx.debug_bounds("split-choice-3").expect("drawn").center();
    s.cx.simulate_click(at, Modifiers::none());
    s.cx.run_until_parked();
    assert_eq!(s.spawns().len(), 1);
    assert_eq!(s.chooser(pane), None);
}

#[gpui::test]
fn an_agent_shown_from_the_sidebar_replaces_the_chooser(cx: &mut TestAppContext) {
    let mut s = setup(cx, fleet());
    let pane = s.split_from_alpha();
    s.show(3);
    assert!(s.calls().contains(&Call::Attach(pane, 3)));
    assert_eq!(s.chooser(pane), None);
}

#[gpui::test]
fn a_click_into_the_pane_keeps_the_keys_with_the_chooser(cx: &mut TestAppContext) {
    let mut s = setup(cx, fleet());
    let pane = s.split_from_alpha();
    let card = s.cx.debug_bounds("pane-card-2").expect("drawn");
    let corner = gpui::point(card.left() + gpui::px(20.), card.bottom() - gpui::px(20.));
    s.cx.simulate_click(corner, Modifiers::none());
    s.cx.run_until_parked();
    assert!(s.chooser_focused(pane));
    s.keys("3");
    assert_eq!(s.spawns().len(), 1);
}
