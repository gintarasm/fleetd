//! The graph's terminal panel (issue #168, ADR 0105): Space or ⏎ opens the
//! selected card's agent beside the graph; the selection drives it; keys
//! reach it only while it has the keyboard; Esc gives the keys back, then
//! closes it; ⌘⏎ still opens in the main view; closing detaches; the
//! layout never changes.
#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fleet_client::tree::PARENT_LABEL;
use fleet_gui::app::{AppState, InputSink, SessionApi, Theme, Workspace, bind_keys};
use fleet_gui::core::config::GuiConfig;
use fleet_gui::core::graph_panel::GRAPH_PANEL_PANE;
use fleet_gui::core::pane::PaneModel;
use fleet_gui::core::session::{AgentRow, PaneId, UiEvent};
use fleet_gui::core::spawn::SpawnRequest;
use fleet_gui::core::target::Target;
use fleet_proto::{AgentInfo, AgentState};
use fleet_term::Palette;
use gpui::{
    AppContext, Entity, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, TestAppContext,
    VisualTestContext, size,
};

#[derive(Clone, Debug, PartialEq)]
enum Call {
    Open(PaneId, Option<u64>),
    Attach(PaneId, u64),
    Close(PaneId),
}

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Call>>,
    inputs: Mutex<Vec<(PaneId, Vec<u8>)>>,
    agents: Mutex<Vec<AgentRow>>,
}

impl InputSink for Fake {
    fn input(&self, pane: PaneId, bytes: Vec<u8>) {
        self.inputs.lock().unwrap().push((pane, bytes));
    }
    fn resize(&self, _: PaneId, _: u16, _: u16) {}
}

impl SessionApi for Fake {
    fn agents(&self) -> Vec<AgentRow> {
        self.agents.lock().unwrap().clone()
    }
    fn spawn(&self, _: SpawnRequest, _: u16, _: u16, _: PaneId) -> Result<(), String> {
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
    fn open_pane(&self, pane: PaneId, agent: Option<u64>) -> Arc<Mutex<PaneModel>> {
        self.calls.lock().unwrap().push(Call::Open(pane, agent));
        Arc::new(Mutex::new(PaneModel::new(Theme::dark().palette)))
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
    fn set_palette(&self, _: Palette) {}
}

fn agent(id: u64, parent: Option<u64>) -> AgentRow {
    let mut info = AgentInfo {
        id,
        program: "sh".into(),
        cwd: "/w".into(),
        created_unix_ms: id as i64,
        ..Default::default()
    };
    if let Some(p) = parent {
        info.labels.insert(PARENT_LABEL.to_owned(), p.to_string());
    }
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
    ui: async_channel::Sender<UiEvent>,
}

impl Setup<'_> {
    fn keys(&mut self, keys: &str) {
        self.cx.simulate_keystrokes(keys);
        self.cx.run_until_parked();
    }

    fn graph_shown(&mut self) -> bool {
        self.ws.read_with(self.cx, |w, _| w.graph_shown())
    }

    fn selected(&mut self) -> Option<u64> {
        self.ws
            .read_with(self.cx, |w, cx| w.graph().read(cx).selected())
    }

    fn panel(&mut self) -> Option<u64> {
        self.ws
            .read_with(self.cx, |w, cx| w.graph().read(cx).panel_agent(cx))
    }

    fn panel_focused(&mut self) -> bool {
        let ws = self.ws.clone();
        self.cx
            .update(|window, cx| ws.read(cx).graph().read(cx).panel_focused(window, cx))
    }

    fn calls(&self) -> Vec<Call> {
        self.fake.calls.lock().unwrap().clone()
    }

    /// Input that went through the panel's connection.
    fn panel_input(&self) -> Vec<Vec<u8>> {
        self.fake
            .inputs
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| *p == GRAPH_PANEL_PANE)
            .map(|(_, b)| b.clone())
            .collect()
    }

    /// The layout's panes and what each shows.
    fn layout(&mut self) -> Vec<(PaneId, Option<u64>)> {
        self.app.read_with(self.cx, |a, _| {
            let l = a.layout();
            l.panes().into_iter().map(|p| (p, l.agent(p))).collect()
        })
    }

    fn focused_pane(&mut self) -> PaneId {
        self.app.read_with(self.cx, |a, _| a.focused())
    }

    fn click(&mut self, selector: &'static str, count: usize) {
        self.cx.run_until_parked();
        let at = self
            .cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"))
            .center();
        for click_count in 1..=count {
            self.cx.simulate_event(MouseDownEvent {
                position: at,
                button: MouseButton::Left,
                modifiers: Modifiers::none(),
                click_count,
                first_mouse: false,
            });
            self.cx.simulate_event(MouseUpEvent {
                position: at,
                button: MouseButton::Left,
                modifiers: Modifiers::none(),
                click_count,
            });
        }
        self.cx.run_until_parked();
    }
}

/// The graph shown over agents 1 → {2, 3}.
fn setup(cx: &mut TestAppContext) -> Setup<'_> {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    *fake.agents.lock().unwrap() = vec![agent(1, None), agent(2, Some(1)), agent(3, Some(1))];
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
            GuiConfig {
                presets: Some(Vec::new()),
                ..GuiConfig::default()
            },
            Theme::dark(),
            cx,
        )
    });
    cx.update(|cx| AppState::install(app.clone(), cx));
    let state = app.clone();
    let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(state, window, cx));
    cx.update(|window, _| window.activate_window());
    cx.simulate_resize(size(gpui::px(1600.), gpui::px(1000.)));
    cx.run_until_parked();
    let mut s = Setup {
        ws,
        app,
        fake,
        cx,
        _dir: dir,
        ui,
    };
    s.keys("cmd-shift-g");
    assert!(s.graph_shown());
    assert_eq!(s.selected(), Some(1));
    s
}

#[gpui::test]
fn space_opens_the_panel_and_the_selection_drives_it(cx: &mut TestAppContext) {
    let mut s = setup(cx);
    let layout = s.layout();
    let before = s.calls().len();
    assert_eq!(s.panel(), None, "closed when the graph opens");
    assert!(s.cx.debug_bounds("graph-panel").is_none());

    s.keys("space");
    assert_eq!(s.panel(), Some(1));
    assert!(s.cx.debug_bounds("graph-panel").is_some());
    assert!(s.calls().contains(&Call::Open(GRAPH_PANEL_PANE, Some(1))));
    assert!(s.panel_focused(), "the panel has the keys");
    assert!(s.graph_shown(), "the graph stays");
    assert!(s.cx.debug_bounds("graph-card-1").is_some());

    // Typing goes to the agent through the panel's connection.
    s.keys("y");
    assert_eq!(s.panel_input(), [b"y".to_vec()]);

    // Esc gives the keys back to the graph; the panel stays, and Esc is
    // not typed to the agent.
    s.keys("escape");
    assert!(!s.panel_focused());
    assert_eq!(s.panel(), Some(1));
    assert_eq!(s.panel_input(), [b"y".to_vec()]);

    // The arrows move the selection, and the panel follows on the same
    // connection; the keys stay with the graph.
    s.keys("right");
    assert_eq!(s.selected(), Some(2));
    assert_eq!(s.panel(), Some(2));
    assert!(s.calls().contains(&Call::Attach(GRAPH_PANEL_PANE, 2)));
    assert!(!s.panel_focused());
    s.keys("x");
    assert_eq!(
        s.panel_input(),
        [b"y".to_vec()],
        "keys reach the panel only while it has them"
    );

    // ⏎ gives the open panel the keys again.
    s.keys("enter");
    assert!(s.panel_focused());
    assert_eq!(s.panel(), Some(2));

    // Esc, then Esc again: the panel closes and detaches; the graph stays.
    s.keys("escape escape");
    assert_eq!(s.panel(), None);
    assert!(s.calls().contains(&Call::Close(GRAPH_PANEL_PANE)));
    assert!(s.graph_shown());
    // A third leaves the graph.
    s.keys("escape");
    assert!(!s.graph_shown());

    assert_eq!(s.layout(), layout, "the layout is unchanged");
    assert!(
        !s.calls()[before..]
            .iter()
            .any(|c| matches!(c, Call::Attach(p, _) | Call::Open(p, _) if *p != GRAPH_PANEL_PANE)),
        "no layout pane attached or opened"
    );
}

#[gpui::test]
fn a_click_selects_and_the_panel_follows_a_double_click_opens_in_a_pane(cx: &mut TestAppContext) {
    let mut s = setup(cx);
    s.click("graph-card-3", 1);
    assert_eq!(s.selected(), Some(3));
    assert!(s.graph_shown(), "a click no longer leaves the graph");
    assert_eq!(s.panel(), None, "nor opens the panel");
    s.keys("space");
    assert_eq!(s.panel(), Some(3));
    s.click("graph-card-2", 1);
    assert_eq!(s.panel(), Some(2));
    assert!(!s.panel_focused(), "the graph has the keys after a click");

    s.click("graph-card-1", 2);
    assert!(!s.graph_shown());
    assert_eq!(s.panel(), None, "leaving the graph closes the panel");
    assert!(s.calls().contains(&Call::Close(GRAPH_PANEL_PANE)));
    let pane = s.focused_pane();
    let shown = s.app.read_with(s.cx, |a, _| a.layout().agent(pane));
    assert_eq!(shown, Some(1));
}

#[gpui::test]
fn cmd_return_opens_in_the_main_view_from_the_graph_or_the_panel(cx: &mut TestAppContext) {
    let mut s = setup(cx);
    s.keys("right space");
    assert!(s.panel_focused());
    s.keys("cmd-enter");
    assert!(!s.graph_shown());
    assert_eq!(s.panel(), None);
    assert!(s.calls().contains(&Call::Close(GRAPH_PANEL_PANE)));
    let pane = s.focused_pane();
    assert_eq!(
        s.app.read_with(s.cx, |a, _| a.layout().agent(pane)),
        Some(2)
    );
    // The panel's cmd-enter was not typed to the agent.
    assert!(s.panel_input().is_empty());

    // From the graph, with the panel closed.
    s.keys("cmd-shift-g");
    assert_eq!(s.panel(), None, "closed when the graph opens again");
    s.keys("down cmd-enter");
    assert!(!s.graph_shown());
}

#[gpui::test]
fn the_close_button_detaches_and_a_reopen_attaches_again(cx: &mut TestAppContext) {
    let mut s = setup(cx);
    s.keys("space");
    assert!(s.cx.debug_bounds("graph-panel-close").is_some());
    s.click("graph-panel-close", 1);
    assert_eq!(s.panel(), None);
    assert!(s.calls().contains(&Call::Close(GRAPH_PANEL_PANE)));
    assert!(s.graph_shown());
    s.keys("space");
    let opens = s
        .calls()
        .iter()
        .filter(|c| matches!(c, Call::Open(p, _) if *p == GRAPH_PANEL_PANE))
        .count();
    assert_eq!(opens, 2);
}

#[gpui::test]
fn space_in_the_filter_is_a_space(cx: &mut TestAppContext) {
    let mut s = setup(cx);
    s.keys("/");
    s.keys("space");
    assert_eq!(s.panel(), None, "Space in the filter types");
    s.keys("escape escape space");
    assert_eq!(s.panel(), Some(1), "back in the graph, Space opens it");
}

#[gpui::test]
fn a_narrow_window_puts_the_panel_over_the_graph(cx: &mut TestAppContext) {
    let mut s = setup(cx);
    s.keys("space");
    assert!(s.cx.debug_bounds("graph-panel").is_some());
    assert!(s.cx.debug_bounds("graph-panel-overlay").is_none());
    s.cx.simulate_resize(size(gpui::px(700.), gpui::px(700.)));

    s.cx.run_until_parked();
    s.cx.update(|window, _| window.refresh());
    s.cx.run_until_parked();
    assert!(s.cx.debug_bounds("graph-panel-overlay").is_some());
}

#[gpui::test]
fn an_agent_that_exits_keeps_its_panel_marked_exited(cx: &mut TestAppContext) {
    let mut s = setup(cx);
    s.keys("space");
    assert!(s.cx.debug_bounds("graph-panel-exited").is_none());
    {
        let mut agents = s.fake.agents.lock().unwrap();
        agents[0].info.set_state(AgentState::Exited);
        agents[0].info.exit_code = Some(0);
    }
    s.ui.try_send(UiEvent::AgentsChanged).unwrap();
    s.cx.run_until_parked();
    s.cx.update(|window, _| window.refresh());
    s.cx.run_until_parked();
    assert_eq!(s.panel(), Some(1), "the panel stays on the last screen");
    assert!(s.cx.debug_bounds("graph-panel-exited").is_some());
    assert!(!s.calls().contains(&Call::Close(GRAPH_PANEL_PANE)));
}
