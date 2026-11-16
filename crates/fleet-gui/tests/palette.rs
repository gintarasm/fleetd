//! The command palette (⌘P, issue #29) in the window: what it lists, the
//! keys and the mouse, and what a pick does.
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
    fn palette_open(&mut self) -> bool {
        self.ws.read_with(self.cx, |ws, _| ws.palette().is_some())
    }

    /// The listed rows' titles, in order.
    fn titles(&mut self) -> Vec<String> {
        self.ws.read_with(self.cx, |ws, cx| {
            let p = ws.palette().expect("palette open").read(cx).picker();
            p.ranked()
                .iter()
                .map(|r| p.items()[r.item].title.clone())
                .collect()
        })
    }

    fn selected(&mut self) -> Option<String> {
        self.ws.read_with(self.cx, |ws, cx| {
            let p = ws.palette().expect("palette open").read(cx).picker();
            p.selected_item().map(|i| i.title.clone())
        })
    }

    fn shortcut(&mut self, title: &str) -> Option<String> {
        self.ws.read_with(self.cx, |ws, cx| {
            let p = ws.palette().expect("palette open").read(cx).picker();
            p.items()
                .iter()
                .find(|i| i.title == title)
                .unwrap()
                .shortcut
                .clone()
        })
    }

    fn panes(&mut self) -> usize {
        self.app.read_with(self.cx, |a, _| a.layout().panes().len())
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

    fn type_text(&mut self, text: &str) {
        self.cx.simulate_input(text);
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

#[gpui::test]
fn cmd_p_lists_actions_with_shortcuts_and_esc_closes(cx: &mut TestAppContext) {
    let mut s = setup(cx, vec![]);
    assert!(!s.palette_open());
    s.keys("cmd-p");
    assert!(s.palette_open());
    assert!(s.titles().contains(&"Split Right".to_owned()));
    assert_eq!(s.shortcut("Split Right").as_deref(), Some("⌘D"));
    assert_eq!(s.shortcut("Split Down").as_deref(), Some("⇧⌘D"));
    assert_eq!(
        s.shortcut("Maximize or Restore Pane").as_deref(),
        Some("⇧⌘↩")
    );
    assert_eq!(s.shortcut("Remove Exited Agents…"), None);
    s.keys("escape");
    assert!(!s.palette_open());
    assert!(s.pane_focused());
}

#[gpui::test]
fn cmd_p_again_closes_it(cx: &mut TestAppContext) {
    let mut s = setup(cx, vec![]);
    s.keys("cmd-p");
    s.keys("cmd-p");
    assert!(!s.palette_open());
    assert!(s.pane_focused());
}

#[gpui::test]
fn typing_filters_and_enter_performs_the_action(cx: &mut TestAppContext) {
    let mut s = setup(cx, vec![]);
    assert_eq!(s.panes(), 1);
    s.keys("cmd-p");
    s.type_text("split right");
    assert_eq!(s.selected().as_deref(), Some("Split Right"));
    s.keys("enter");
    assert!(!s.palette_open());
    assert_eq!(s.panes(), 2);
}

#[gpui::test]
fn arrows_move_the_selection(cx: &mut TestAppContext) {
    let mut s = setup(cx, vec![]);
    s.keys("cmd-p");
    s.type_text("split");
    let titles = s.titles();
    assert_eq!(s.selected(), Some(titles[0].clone()));
    s.keys("down");
    assert_eq!(s.selected(), Some(titles[1].clone()));
    s.keys("up up");
    assert_eq!(s.selected(), titles.last().cloned());
    s.keys("ctrl-n");
    assert_eq!(s.selected(), Some(titles[0].clone()));
}

#[gpui::test]
fn picking_an_agent_shows_it_in_the_focused_pane(cx: &mut TestAppContext) {
    let mut s = setup(
        cx,
        vec![
            agent(1, "sh", "/work/alpha", 1),
            agent(2, "claude", "/work/beta", 2),
        ],
    );
    s.keys("cmd-p");
    s.type_text("beta");
    assert_eq!(s.selected().as_deref(), Some("claude in /work/beta"));
    s.keys("enter");
    let pane = s.focused();
    assert!(s.calls().contains(&Call::Attach(pane, 2)));
    assert!(s.pane_focused());
}

#[gpui::test]
fn picking_a_folder_spawns_the_default_command_there(cx: &mut TestAppContext) {
    let mut s = setup(cx, vec![agent(1, "sh", "/work/alpha", 1)]);
    s.keys("cmd-p");
    s.type_text("alpha new agent");
    assert_eq!(s.selected().as_deref(), Some("/work/alpha"));
    s.keys("enter");
    let spawns: Vec<SpawnRequest> = s
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Spawn(r) => Some(r),
            Call::Attach(..) => None,
        })
        .collect();
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].program, "claude");
    assert!(spawns[0].args.is_empty());
    assert_eq!(spawns[0].cwd, "/work/alpha");
}

#[gpui::test]
fn the_last_pick_comes_first_next_time(cx: &mut TestAppContext) {
    let mut s = setup(cx, vec![agent(1, "sh", "/work/alpha", 1)]);
    s.keys("cmd-p");
    let first = s.titles()[0].clone();
    assert_ne!(first, "sh in /work/alpha");
    s.type_text("alpha");
    s.keys("enter");
    s.keys("cmd-p");
    assert_eq!(s.titles()[0], "sh in /work/alpha");
    assert_eq!(s.selected().as_deref(), Some("sh in /work/alpha"));
}

#[gpui::test]
fn a_click_picks_a_row_and_a_click_beside_closes(cx: &mut TestAppContext) {
    let mut s = setup(cx, vec![]);
    s.keys("cmd-p");
    s.type_text("split");
    let second = s.titles()[1].clone();
    let at =
        s.cx.debug_bounds("picker-row-1")
            .expect("row drawn")
            .center();
    s.cx.simulate_mouse_move(at, None, Modifiers::none());
    assert_eq!(s.selected(), Some(second));
    s.cx.simulate_click(at, Modifiers::none());
    s.cx.run_until_parked();
    assert!(!s.palette_open());
    assert_eq!(s.panes(), 2);

    s.keys("cmd-p");
    let card = s.cx.debug_bounds("picker").expect("picker drawn");
    let beside = gpui::point(card.left() - gpui::px(40.), card.bottom() + gpui::px(40.));
    s.cx.simulate_click(beside, Modifiers::none());
    s.cx.run_until_parked();
    assert!(!s.palette_open());
    assert_eq!(s.panes(), 2);
}

#[gpui::test]
fn not_over_the_new_agent_sheet(cx: &mut TestAppContext) {
    let mut s = setup(cx, vec![]);
    s.keys("cmd-n");
    assert!(s.ws.read_with(s.cx, |ws, _| ws.sheet().is_some()));
    s.keys("cmd-p");
    assert!(!s.palette_open());
}
