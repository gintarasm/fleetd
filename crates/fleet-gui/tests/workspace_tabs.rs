//! Workspace tabs (issue #43, ADR 0061): the keys, the strip in the bar,
//! a layout per tab, and the tabs saved per target.
#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use fleet_gui::app::{AppState, InputSink, SessionApi, Theme, Workspace, bind_keys};
use fleet_gui::core::config::GuiConfig;
use fleet_gui::core::pane::PaneModel;
use fleet_gui::core::session::{AgentRow, PaneId};
use fleet_gui::core::spawn::SpawnRequest;
use fleet_gui::core::state::GuiState;
use fleet_gui::core::target::Target;
use fleet_term::Palette;
use gpui::{AppContext, Entity, TestAppContext, VisualTestContext};

/// A session with no agents that records closed panes and spawns.
#[derive(Default)]
struct Recorder {
    closed: Mutex<Vec<PaneId>>,
    spawned: Mutex<Vec<PaneId>>,
}

impl InputSink for Recorder {
    fn input(&self, _: PaneId, _: Vec<u8>) {}
    fn resize(&self, _: PaneId, _: u16, _: u16) {}
}

impl SessionApi for Recorder {
    fn agents(&self) -> Vec<AgentRow> {
        Vec::new()
    }
    fn spawn(&self, _: SpawnRequest, _: u16, _: u16, pane: PaneId) -> Result<(), String> {
        self.spawned.lock().unwrap().push(pane);
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
    fn attach(&self, _: PaneId, _: u64) {}
    fn close_pane(&self, pane: PaneId) {
        self.closed.lock().unwrap().push(pane);
    }
    fn set_palette(&self, _: Palette) {}
}

struct Setup<'a> {
    app: Entity<AppState>,
    ws: Entity<Workspace>,
    cx: &'a mut VisualTestContext,
}

fn app_state(cx: &mut TestAppContext, session: Arc<Recorder>, dir: &Path) -> Entity<AppState> {
    let (_ui, rx) = async_channel::unbounded();
    let session: Arc<dyn SessionApi> = session;
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

fn setup<'a>(cx: &'a mut TestAppContext, session: Arc<Recorder>, dir: &Path) -> Setup<'a> {
    cx.update(bind_keys);
    let app = app_state(cx, session, dir);
    cx.update(|cx| AppState::install(app.clone(), cx));
    let state = app.clone();
    let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(state, window, cx));
    cx.run_until_parked();
    Setup { app, ws, cx }
}

fn keys(s: &mut Setup<'_>, keys: &str) {
    s.cx.simulate_keystrokes(keys);
    s.cx.run_until_parked();
}

/// (tab count, shown tab, focused pane).
fn tabs(s: &mut Setup<'_>) -> (usize, usize, PaneId) {
    s.app.read_with(s.cx, |a, _| {
        (a.tabs().len(), a.tabs().active(), a.focused())
    })
}

fn drawn(s: &mut Setup<'_>) -> Vec<PaneId> {
    s.ws.read_with(s.cx, |w, _| w.drawn_panes().to_vec())
}

fn has_keyboard_focus(s: &mut Setup<'_>, pane: PaneId) -> bool {
    let app = s.app.clone();
    s.cx.update(|window, cx| {
        let view = app.read(cx).pane_view(pane).unwrap().clone();
        // The pane's view, or the split chooser drawn in it.
        view.read(cx).focus_handle().contains_focused(window, cx)
    })
}

fn shown(s: &mut Setup<'_>, selector: &'static str) -> bool {
    s.cx.run_until_parked();
    s.cx.debug_bounds(selector).is_some()
}

fn click(s: &mut Setup<'_>, selector: &'static str) {
    s.cx.run_until_parked();
    let at =
        s.cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"))
            .center();
    s.cx.simulate_click(at, gpui::Modifiers::none());
    s.cx.run_until_parked();
}

/// A click on a tab chip's label (its left end; the ✕ is at its right).
fn click_chip(s: &mut Setup<'_>, selector: &'static str) {
    s.cx.run_until_parked();
    let b =
        s.cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"));
    let at = gpui::point(b.origin.x + gpui::px(6.), b.center().y);
    s.cx.simulate_click(at, gpui::Modifiers::none());
    s.cx.run_until_parked();
}

#[gpui::test]
fn cmd_alt_t_opens_a_tab_with_its_own_empty_pane(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = setup(cx, Arc::default(), dir.path());
    keys(&mut s, "cmd-d");
    assert_eq!(drawn(&mut s), [PaneId(1), PaneId(2)]);
    keys(&mut s, "cmd-alt-t");
    assert_eq!(tabs(&mut s), (2, 1, PaneId(3)));
    assert_eq!(drawn(&mut s), [PaneId(3)]);
    assert!(has_keyboard_focus(&mut s, PaneId(3)));
    // The first tab's panes stay open behind it.
    let kept = s.app.read_with(s.cx, |a, _| {
        a.pane_view(PaneId(1)).is_some() && a.pane_view(PaneId(2)).is_some()
    });
    assert!(kept);
}

#[gpui::test]
fn next_previous_and_numbered_keys_switch_tabs_and_focus(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = setup(cx, Arc::default(), dir.path());
    keys(&mut s, "cmd-d"); // tab 1: 1 | 2, pane 2 focused
    keys(&mut s, "cmd-alt-t"); // tab 2: 3
    keys(&mut s, "cmd-alt-t"); // tab 3: 4
    assert_eq!(tabs(&mut s), (3, 2, PaneId(4)));
    keys(&mut s, "cmd-shift-]");
    assert_eq!(tabs(&mut s), (3, 0, PaneId(2)), "wraps to the first tab");
    assert!(has_keyboard_focus(&mut s, PaneId(2)));
    assert_eq!(drawn(&mut s), [PaneId(1), PaneId(2)]);
    keys(&mut s, "cmd-shift-[");
    assert_eq!(tabs(&mut s), (3, 2, PaneId(4)), "wraps to the last tab");
    keys(&mut s, "cmd-alt-2");
    assert_eq!(tabs(&mut s), (3, 1, PaneId(3)));
    assert!(has_keyboard_focus(&mut s, PaneId(3)));
    keys(&mut s, "cmd-alt-9");
    assert_eq!(tabs(&mut s), (3, 1, PaneId(3)), "no ninth tab");
}

#[gpui::test]
fn cmd_alt_w_closes_the_tab_and_detaches_its_panes(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let session = Arc::new(Recorder::default());
    let mut s = setup(cx, session.clone(), dir.path());
    keys(&mut s, "cmd-alt-t"); // tab 2: 2
    keys(&mut s, "cmd-d"); // tab 2: 2 | 3
    keys(&mut s, "cmd-alt-w");
    assert_eq!(tabs(&mut s), (1, 0, PaneId(1)));
    assert_eq!(*session.closed.lock().unwrap(), [PaneId(2), PaneId(3)]);
    let gone = s
        .app
        .read_with(s.cx, |a, _| a.pane_view(PaneId(2)).is_none());
    assert!(gone);
    assert!(has_keyboard_focus(&mut s, PaneId(1)));
    // The last tab stays.
    keys(&mut s, "cmd-alt-w");
    assert_eq!(tabs(&mut s), (1, 0, PaneId(1)));
    assert_eq!(session.closed.lock().unwrap().len(), 2);
}

#[gpui::test]
fn pane_keys_act_within_the_shown_tab(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let session = Arc::new(Recorder::default());
    let mut s = setup(cx, session.clone(), dir.path());
    keys(&mut s, "cmd-alt-t"); // tab 2: 2
    keys(&mut s, "cmd-d"); // 2 | 3
    keys(&mut s, "cmd-1");
    assert_eq!(
        tabs(&mut s),
        (2, 1, PaneId(2)),
        "⌘1 is the tab's first pane"
    );
    keys(&mut s, "cmd-2");
    assert_eq!(tabs(&mut s), (2, 1, PaneId(3)));
    // ⌘W closes the pane, not the tab.
    keys(&mut s, "cmd-w");
    assert_eq!(tabs(&mut s), (2, 1, PaneId(2)));
    assert_eq!(*session.closed.lock().unwrap(), [PaneId(3)]);
    // ⌘T still opens a terminal, in the shown tab.
    keys(&mut s, "cmd-t");
    assert_eq!(*session.spawned.lock().unwrap(), [PaneId(2)]);
    // Tab 1 kept its single pane.
    let first = s
        .app
        .read_with(s.cx, |a, _| a.tabs().tab_layout(0).unwrap().panes());
    assert_eq!(first, [PaneId(1)]);
}

#[gpui::test]
fn the_bar_shows_a_strip_of_tabs_once_there_are_two(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let session = Arc::new(Recorder::default());
    let mut s = setup(cx, session.clone(), dir.path());
    assert!(!shown(&mut s, "workspace-tab-1"), "one tab: no strip");
    keys(&mut s, "cmd-alt-t");
    assert!(shown(&mut s, "workspace-tab-1") && shown(&mut s, "workspace-tab-2"));
    click_chip(&mut s, "workspace-tab-1");
    assert_eq!(tabs(&mut s), (2, 0, PaneId(1)));
    assert!(has_keyboard_focus(&mut s, PaneId(1)));
    click(&mut s, "new-tab");
    assert_eq!(tabs(&mut s), (3, 1, PaneId(3)));
    click(&mut s, "workspace-tab-close-2");
    assert_eq!(tabs(&mut s), (2, 1, PaneId(2)));
    assert_eq!(*session.closed.lock().unwrap(), [PaneId(3)]);
}

#[gpui::test]
fn tabs_are_saved_per_target_and_restored(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = setup(cx, Arc::default(), dir.path());
    keys(&mut s, "cmd-d");
    keys(&mut s, "cmd-alt-t");
    keys(&mut s, "cmd-shift-d");
    keys(&mut s, "cmd-alt-t");
    keys(&mut s, "cmd-alt-2");
    let saved = s.app.read_with(s.cx, |a, _| a.tabs().to_saved());
    s.app.update(s.cx, |a, _| a.save_now());
    let state = GuiState::load(dir.path());
    assert_eq!(state.tabs["local"], saved);
    assert_eq!(saved.tabs.len(), 3);
    assert_eq!(saved.active, 1);
    // Older builds read the shown tab's layout.
    let shown_layout = s.app.read_with(s.cx, |a, _| a.layout().to_saved());
    assert_eq!(state.layouts["local"], shown_layout);

    let again = app_state(s.cx, Arc::default(), dir.path());
    let restored = again.read_with(s.cx, |a, _| {
        let every_pane_open = a.tabs().panes().iter().all(|p| a.pane_view(*p).is_some());
        (a.tabs().to_saved(), every_pane_open)
    });
    assert_eq!(restored, (saved, true));
}
