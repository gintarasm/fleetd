//! Switching the target while the graph is shown (issue #133): the new
//! session is asked for message lines, and the old target's lines and
//! notice are gone.
#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use fleet_gui::app::{AppState, InputSink, SessionApi, Theme, Workspace};
use fleet_gui::core::config::GuiConfig;
use fleet_gui::core::message_lines::Edge;
use fleet_gui::core::pane::PaneModel;
use fleet_gui::core::session::{PaneId, UiEvent};
use fleet_gui::core::spawn::SpawnRequest;
use fleet_gui::core::target::Target;
use fleet_term::Palette;
use gpui::{AppContext, TestAppContext, VisualTestContext};

#[derive(Default)]
struct Fake {
    watching: AtomicBool,
    edges: Mutex<Vec<Edge>>,
}

impl InputSink for Fake {
    fn input(&self, _: PaneId, _: Vec<u8>) {}
    fn resize(&self, _: PaneId, _: u16, _: u16) {}
}

impl SessionApi for Fake {
    fn agents(&self) -> Vec<fleet_gui::core::session::AgentRow> {
        Vec::new()
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
    fn open_pane(&self, _: PaneId, _: Option<u64>) -> Arc<Mutex<PaneModel>> {
        Arc::new(Mutex::new(PaneModel::new(Theme::dark().palette)))
    }
    fn attach(&self, _: PaneId, _: u64) {}
    fn close_pane(&self, _: PaneId) {}
    fn set_palette(&self, _: Palette) {}
    fn watch_message_edges(&self, on: bool) {
        self.watching.store(on, Ordering::SeqCst);
    }
    fn message_edges(&self) -> Vec<Edge> {
        self.edges.lock().unwrap().clone()
    }
}

fn run(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let old = Arc::new(Fake::default());
    let (ui, rx) = async_channel::unbounded();
    let session: Arc<dyn SessionApi> = old.clone();
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
    let (ws, cx): (_, &mut VisualTestContext) =
        cx.add_window_view(|window, cx| Workspace::new(state, window, cx));
    cx.run_until_parked();

    cx.update(|window, cx| ws.update(cx, |ws, cx| ws.toggle_graph(window, cx)));
    assert!(old.watching.load(Ordering::SeqCst));
    old.edges.lock().unwrap().push(Edge {
        id: 1,
        from: 1,
        to: 2,
        at_ms: 1,
    });
    ui.send_blocking(UiEvent::MessageEdgesChanged).unwrap();
    cx.run_until_parked();
    assert_eq!(app.read_with(cx, |a, _| a.message_edges().len()), 1);

    let new = Arc::new(Fake::default());
    let (_ui2, rx2) = async_channel::unbounded();
    let session: Arc<dyn SessionApi> = new.clone();
    app.update(cx, |a, cx| {
        a.show_notice("old target".into(), cx);
        a.replace_session(session, rx2, Target::Ssh { dest: "box".into() }, cx);
    });
    cx.run_until_parked();

    assert!(ws.read_with(cx, |ws, _| ws.graph_shown()));
    assert!(new.watching.load(Ordering::SeqCst), "new session watches");
    assert!(app.read_with(cx, |a, _| a.message_edges().is_empty()));
    assert_eq!(
        app.read_with(cx, |a, _| a.notice().map(str::to_owned)),
        None
    );
}

#[gpui::test]
fn switching_target_rearms_message_lines(cx: &mut TestAppContext) {
    run(cx);
}
