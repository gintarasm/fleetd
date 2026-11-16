//! The agent switcher (⌃Tab, issue #42) in the window: most-recently-used
//! order, cycling, and ⌃'s release showing the choice in the focused pane.
#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fleet_gui::app::{AppState, InputSink, SessionApi, Theme, Workspace, bind_keys};
use fleet_gui::core::config::GuiConfig;
use fleet_gui::core::pane::PaneModel;
use fleet_gui::core::session::{AgentRow, PaneId, UiEvent};
use fleet_gui::core::spawn::SpawnRequest;
use fleet_gui::core::target::Target;
use fleet_proto::{AgentInfo, AgentState};
use fleet_term::Palette;
use gpui::{AppContext, Entity, Modifiers, TestAppContext, VisualTestContext};

#[derive(Clone, Debug, PartialEq)]
enum Call {
    Attach(PaneId, u64),
    Spawn(SpawnRequest),
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
    fn spawn(&self, req: SpawnRequest, _: u16, _: u16, _: PaneId) -> Result<(), String> {
        self.calls.lock().unwrap().push(Call::Spawn(req));
        Ok(())
    }
    fn kill(&self, _: u64) {}
    fn restart(&self, _: u64) {}
    fn restart_in_pane(&self, _: PaneId, _: u64, _: u16, _: u16) -> Result<(), String> {
        Ok(())
    }
    fn forget(&self, _: Vec<u64>) {}
    fn clear_exited(&self) {}
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
    fn open(&mut self) -> bool {
        self.ws.read_with(self.cx, |ws, _| ws.switcher().is_some())
    }

    /// The listed rows' titles, in order.
    fn titles(&mut self) -> Vec<String> {
        self.ws.read_with(self.cx, |ws, cx| {
            let p = ws.switcher().expect("switcher open").read(cx).picker();
            p.ranked()
                .iter()
                .map(|r| p.items()[r.item].title.clone())
                .collect()
        })
    }

    fn selected(&mut self) -> Option<String> {
        self.ws.read_with(self.cx, |ws, cx| {
            let p = ws.switcher().expect("switcher open").read(cx).picker();
            p.selected_item().map(|i| i.title.clone())
        })
    }

    fn focused(&mut self) -> PaneId {
        self.app.read_with(self.cx, |a, _| a.focused())
    }

    fn pane_focused(&mut self) -> bool {
        let pane = self.focused();
        self.cx.update(|window, cx| {
            let app = AppState::global(cx).unwrap();
            let view = app.read(cx).pane_view(pane).unwrap().clone();
            view.read(cx).focus_handle().is_focused(window)
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.fake.calls.lock().unwrap().clone()
    }

    /// The agent the focused pane shows.
    fn shown(&mut self) -> Option<u64> {
        self.app
            .read_with(self.cx, |a, _| a.layout().agent(a.focused()))
    }

    /// A sidebar click on `id`.
    fn show(&mut self, id: u64) {
        let ws = self.ws.clone();
        self.cx
            .update(|window, cx| ws.update(cx, |ws, cx| ws.show_agent(id, window, cx)));
        self.cx.run_until_parked();
    }

    fn hold_ctrl(&mut self, shift: bool) {
        let mods = Modifiers {
            control: true,
            shift,
            ..Modifiers::none()
        };
        self.cx.simulate_modifiers_change(mods);
        self.cx.run_until_parked();
    }

    fn release(&mut self) {
        self.cx.simulate_modifiers_change(Modifiers::none());
        self.cx.run_until_parked();
    }

    fn keys(&mut self, keys: &str) {
        self.cx.simulate_keystrokes(keys);
        self.cx.run_until_parked();
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

/// Agents 1, 2, 3, shown in that order (so 3 is the most recent).
fn three(cx: &mut TestAppContext) -> Setup<'_> {
    let mut s = setup(
        cx,
        vec![
            agent(1, "sh", "/work/one", 1),
            agent(2, "sh", "/work/two", 2),
            agent(3, "sh", "/work/three", 3),
        ],
    );
    s.show(1);
    s.show(2);
    s.show(3);
    s
}

#[gpui::test]
fn ctrl_tab_lists_by_recent_use_and_release_shows_the_previous(cx: &mut TestAppContext) {
    let mut s = three(cx);
    s.hold_ctrl(false);
    s.keys("ctrl-tab");
    assert!(s.open());
    assert_eq!(
        s.titles(),
        ["sh in /work/three", "sh in /work/two", "sh in /work/one"]
    );
    assert_eq!(s.selected().as_deref(), Some("sh in /work/two"));
    s.release();
    assert!(!s.open());
    assert_eq!(s.shown(), Some(2));
    let pane = s.focused();
    assert!(s.calls().contains(&Call::Attach(pane, 2)));
    assert!(s.pane_focused());

    // Now 2 is the most recent: another press goes back to 3.
    s.hold_ctrl(false);
    s.keys("ctrl-tab");
    assert_eq!(s.selected().as_deref(), Some("sh in /work/three"));
    s.release();
    assert_eq!(s.shown(), Some(3));
}

#[gpui::test]
fn repeated_presses_cycle_and_shift_goes_back(cx: &mut TestAppContext) {
    let mut s = three(cx);
    s.hold_ctrl(false);
    s.keys("ctrl-tab ctrl-tab");
    assert_eq!(s.selected().as_deref(), Some("sh in /work/one"));
    s.keys("ctrl-tab");
    assert_eq!(s.selected().as_deref(), Some("sh in /work/three"));
    // Pressing shift while ⌃ is held does not pick.
    s.hold_ctrl(true);
    s.keys("ctrl-shift-tab");
    assert!(s.open());
    assert_eq!(s.selected().as_deref(), Some("sh in /work/one"));
    s.release();
    assert_eq!(s.shown(), Some(1));
}

#[gpui::test]
fn ctrl_shift_tab_starts_at_the_least_recent(cx: &mut TestAppContext) {
    let mut s = three(cx);
    s.hold_ctrl(true);
    s.keys("ctrl-shift-tab");
    assert_eq!(s.selected().as_deref(), Some("sh in /work/one"));
}

#[gpui::test]
fn escape_cancels(cx: &mut TestAppContext) {
    let mut s = three(cx);
    s.hold_ctrl(false);
    s.keys("ctrl-tab");
    s.keys("escape");
    assert!(!s.open());
    s.release();
    assert_eq!(s.shown(), Some(3));
    assert!(s.pane_focused());
}

#[gpui::test]
fn rows_show_id_status_and_last_line(cx: &mut TestAppContext) {
    let mut busy = agent(4, "claude", "/work/four", 4);
    busy.last_line = "compiling fleet-gui".into();
    let mut s = setup(cx, vec![agent(1, "sh", "/work/one", 1), busy]);
    s.keys("ctrl-tab");
    let (detail, preview) = s.ws.read_with(s.cx, |ws, cx| {
        let p = ws.switcher().unwrap().read(cx).picker();
        let item = p.items().iter().find(|i| i.key == "agent:4").unwrap();
        (item.detail.clone(), item.preview.clone())
    });
    assert_eq!(detail, "#4 · /work/four · running");
    assert_eq!(preview, "compiling fleet-gui");
    assert!(s.cx.debug_bounds("picker-preview-0").is_some());
}

#[gpui::test]
fn nothing_to_switch_to(cx: &mut TestAppContext) {
    let mut s = setup(cx, vec![]);
    s.keys("ctrl-tab");
    assert!(!s.open());
}

#[gpui::test]
fn not_over_the_new_agent_sheet(cx: &mut TestAppContext) {
    let mut s = three(cx);
    s.keys("cmd-n");
    assert!(s.ws.read_with(s.cx, |ws, _| ws.sheet().is_some()));
    s.keys("ctrl-tab");
    assert!(!s.open());
}
