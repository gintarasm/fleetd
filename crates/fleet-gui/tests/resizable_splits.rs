//! Resizable splits (issue #35, ADR 0045): the shell's divider strips.
#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fleet_gui::app::{AppState, InputSink, SessionApi, Theme, Workspace};
use fleet_gui::core::config::GuiConfig;
use fleet_gui::core::layout::{Axis, SavedNode, SplitId};
use fleet_gui::core::pane::PaneModel;
use fleet_gui::core::session::{AgentRow, PaneId};
use fleet_gui::core::spawn::SpawnRequest;
use fleet_gui::core::state::GuiState;
use fleet_gui::core::style::chrome;
use fleet_gui::core::target::Target;
use fleet_term::Palette;
use gpui::{
    AppContext, Bounds, Entity, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels,
    Point, TestAppContext, VisualTestContext, point, px,
};

/// A session with no agents that records nothing.
struct Quiet;

impl InputSink for Quiet {
    fn input(&self, _: PaneId, _: Vec<u8>) {}
    fn resize(&self, _: PaneId, _: u16, _: u16) {}
}

impl SessionApi for Quiet {
    fn agents(&self) -> Vec<AgentRow> {
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
    fn replaced(&self) -> HashMap<u64, u64> {
        HashMap::new()
    }
    fn open_pane(&self, _: PaneId, _: Option<u64>) -> Arc<Mutex<PaneModel>> {
        Arc::new(Mutex::new(PaneModel::new(Theme::dark().palette)))
    }
    fn attach(&self, _: PaneId, _: u64) {}
    fn close_pane(&self, _: PaneId) {}
    fn set_palette(&self, _: Palette) {}
}

struct Setup<'a> {
    app: Entity<AppState>,
    cx: &'a mut VisualTestContext,
}

/// The window with two panes side by side.
fn two_panes<'a>(cx: &'a mut TestAppContext, dir: &Path) -> Setup<'a> {
    let (_ui, rx) = async_channel::unbounded();
    let session: Arc<dyn SessionApi> = Arc::new(Quiet);
    let dir = dir.to_owned();
    let app = cx.new(|cx| {
        AppState::new(
            session,
            rx,
            Target::Local,
            dir,
            GuiConfig::default(),
            Theme::dark(),
            cx,
        )
    });
    cx.update(|cx| AppState::install(app.clone(), cx));
    let state = app.clone();
    let (_ws, cx) = cx.add_window_view(|window, cx| Workspace::new(state, window, cx));
    app.update(cx, |a, cx| a.split(Axis::Horizontal, cx))
        .unwrap();
    cx.run_until_parked();
    Setup { app, cx }
}

fn bounds(s: &mut Setup<'_>, selector: &'static str) -> Bounds<Pixels> {
    s.cx.run_until_parked();
    s.cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} is drawn"))
}

fn ratio(s: &mut Setup<'_>) -> f32 {
    s.app
        .read_with(s.cx, |a, _| a.layout().ratio(SplitId(0)))
        .unwrap()
}

fn drag(s: &mut Setup<'_>, from: Point<Pixels>, to: Point<Pixels>) {
    s.cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    s.cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    s.cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    s.cx.run_until_parked();
}

fn double_click(s: &mut Setup<'_>, at: Point<Pixels>) {
    for click_count in [1, 2] {
        s.cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::none(),
            click_count,
            first_mouse: false,
        });
        s.cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    }
    s.cx.run_until_parked();
}

#[gpui::test]
fn the_divider_strip_sits_on_the_gap_a_little_wider_than_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    let left = bounds(&mut s, "pane-card-1");
    let strip = bounds(&mut s, "split-divider-0");
    let width = f32::from(strip.size.width);
    assert_eq!(width, chrome::DIVIDER_HIT);
    assert!(width > chrome::PANE_GAP);
    // Centred on the line between the cards.
    assert_eq!(strip.center().x, left.right());
    assert_eq!(strip.size.height, left.size.height);
}

#[gpui::test]
fn dragging_the_divider_follows_the_pointer_and_moves_the_panes(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    let left = bounds(&mut s, "pane-card-1");
    let strip = bounds(&mut s, "split-divider-0");
    // Grabbed off centre: the line moves by the pointer's travel.
    let from = strip.center() + point(px(3.0), px(0.0));
    drag(&mut s, from, from - point(px(100.0), px(0.0)));
    assert!(ratio(&mut s) < 0.5);
    let after = bounds(&mut s, "pane-card-1");
    assert_eq!(after.size.width, left.size.width - px(100.0));
    let right = bounds(&mut s, "pane-card-2");
    assert_eq!(right.left(), after.right());
    // The strip followed.
    let moved = bounds(&mut s, "split-divider-0");
    assert_eq!(moved.center().x, strip.center().x - px(100.0));
}

#[gpui::test]
fn the_divider_keeps_each_pane_at_its_minimum(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    let strip = bounds(&mut s, "split-divider-0");
    drag(&mut s, strip.center(), point(px(0.0), strip.center().y));
    assert_eq!(
        bounds(&mut s, "pane-card-1").size.width,
        px(chrome::PANE_MIN)
    );
    let strip = bounds(&mut s, "split-divider-0");
    drag(
        &mut s,
        strip.center(),
        point(px(10_000.0), strip.center().y),
    );
    assert_eq!(
        bounds(&mut s, "pane-card-2").size.width,
        px(chrome::PANE_MIN)
    );
}

#[gpui::test]
fn a_double_click_evens_the_divider(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    let strip = bounds(&mut s, "split-divider-0");
    drag(
        &mut s,
        strip.center(),
        strip.center() + point(px(150.0), px(0.0)),
    );
    assert!(ratio(&mut s) > 0.5);
    let strip = bounds(&mut s, "split-divider-0");
    double_click(&mut s, strip.center());
    assert_eq!(ratio(&mut s), 0.5);
}

#[gpui::test]
fn the_ratio_is_saved_with_the_targets_layout(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    let strip = bounds(&mut s, "split-divider-0");
    drag(
        &mut s,
        strip.center(),
        strip.center() - point(px(80.0), px(0.0)),
    );
    let want = ratio(&mut s);
    // Past the save debounce.
    s.cx.executor().advance_clock(Duration::from_secs(2));
    s.cx.run_until_parked();
    let saved = &GuiState::load(dir.path()).layouts["local"];
    let SavedNode::Split { ratio, .. } = &saved.root else {
        panic!("a split is saved: {saved:?}");
    };
    assert_eq!(*ratio, want);
    assert!(*ratio < 0.5);
}

#[gpui::test]
fn a_mouse_up_ends_the_drag(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    let strip = bounds(&mut s, "split-divider-0");
    drag(
        &mut s,
        strip.center(),
        strip.center() - point(px(60.0), px(0.0)),
    );
    let after = ratio(&mut s);
    // Moving with the button held after the release moves nothing.
    let anywhere = strip.center() + point(px(200.0), px(0.0));
    s.cx.simulate_mouse_move(anywhere, MouseButton::Left, Modifiers::none());
    s.cx.simulate_event(MouseUpEvent {
        button: MouseButton::Left,
        position: anywhere,
        modifiers: Modifiers::none(),
        click_count: 1,
    });
    s.cx.run_until_parked();
    assert_eq!(ratio(&mut s), after);
}
