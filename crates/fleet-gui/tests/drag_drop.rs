//! Drag and drop (issue #34, ADR 0059): sidebar rows onto panes, pane
//! headers onto other panes, Esc to cancel.
#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use fleet_gui::app::{AppState, InputSink, SessionApi, Theme, Workspace};
use fleet_gui::core::config::GuiConfig;
use fleet_gui::core::drag_drop::{DropTarget, Zone};
use fleet_gui::core::layout::{Axis, SplitId};
use fleet_gui::core::pane::PaneModel;
use fleet_gui::core::session::{AgentRow, PaneId};
use fleet_gui::core::spawn::SpawnRequest;
use fleet_gui::core::style::chrome;
use fleet_gui::core::target::Target;
use fleet_proto::{AgentInfo, AgentState};
use fleet_term::Palette;
use gpui::{
    AppContext, Bounds, Entity, Modifiers, MouseButton, Pixels, Point, TestAppContext,
    VisualTestContext, point, px,
};

const AGENT: u64 = 7;

/// A session listing one running agent, that records nothing.
struct OneAgent;

impl InputSink for OneAgent {
    fn input(&self, _: PaneId, _: Vec<u8>) {}
    fn resize(&self, _: PaneId, _: u16, _: u16) {}
}

impl SessionApi for OneAgent {
    fn agents(&self) -> Vec<AgentRow> {
        let mut info = AgentInfo {
            id: AGENT,
            program: "sh".into(),
            cwd: "/tmp".into(),
            ..Default::default()
        };
        info.set_state(AgentState::Running);
        vec![AgentRow {
            info,
            last_line: String::new(),
            last_output_unix_ms: None,
        }]
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
    ws: Entity<Workspace>,
    app: Entity<AppState>,
    cx: &'a mut VisualTestContext,
}

/// The window with two panes side by side, pane 1 focused.
fn two_panes<'a>(cx: &'a mut TestAppContext, dir: &Path) -> Setup<'a> {
    let (_ui, rx) = async_channel::unbounded();
    let session: Arc<dyn SessionApi> = Arc::new(OneAgent);
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
    let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(state, window, cx));
    app.update(cx, |a, cx| {
        a.split(Axis::Horizontal, cx);
        a.set_focused(PaneId(1), cx);
    });
    cx.run_until_parked();
    Setup { ws, app, cx }
}

fn bounds(s: &mut Setup<'_>, selector: &'static str) -> Bounds<Pixels> {
    s.cx.run_until_parked();
    s.cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} is drawn"))
}

/// Where a drop would land now. (Not `debug_bounds`: a test window
/// keeps the selectors of earlier frames.)
fn target(s: &mut Setup<'_>) -> Option<DropTarget> {
    s.cx.run_until_parked();
    s.ws.read_with(s.cx, |w, _| w.drop_target())
}

/// Presses at `from` and moves to `to` with the button held.
fn drag_to(s: &mut Setup<'_>, from: Point<Pixels>, to: Point<Pixels>) {
    s.cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    s.cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    s.cx.run_until_parked();
}

fn release(s: &mut Setup<'_>, at: Point<Pixels>) {
    s.cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    s.cx.run_until_parked();
}

fn drag(s: &mut Setup<'_>, from: Point<Pixels>, to: Point<Pixels>) {
    drag_to(s, from, to);
    release(s, to);
}

/// A point on pane `n`'s header, clear of its buttons.
fn header(s: &mut Setup<'_>, card: &'static str) -> Point<Pixels> {
    let b = bounds(s, card);
    point(
        b.left() + px(40.0),
        b.top() + px(chrome::PANE_GAP / 2.0 + chrome::PANE_HEADER_HEIGHT / 2.0),
    )
}

/// Near the middle of `b`'s `side` edge, inside it.
fn near_right(b: Bounds<Pixels>) -> Point<Pixels> {
    point(b.right() - px(10.0), b.center().y)
}

fn near_bottom(b: Bounds<Pixels>) -> Point<Pixels> {
    point(b.center().x, b.bottom() - px(10.0))
}

fn agent_in(s: &mut Setup<'_>, pane: u32) -> Option<u64> {
    s.app.read_with(s.cx, |a, _| a.layout().agent(PaneId(pane)))
}

fn focused(s: &mut Setup<'_>) -> PaneId {
    s.app.read_with(s.cx, |a, _| a.focused())
}

fn panes(s: &mut Setup<'_>) -> Vec<PaneId> {
    s.app.read_with(s.cx, |a, _| a.layout().panes())
}

#[gpui::test]
fn a_row_dropped_on_a_pane_shows_its_agent_there(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    let row = bounds(&mut s, "tab-7").center();
    let card = bounds(&mut s, "pane-card-2");
    drag_to(&mut s, row, card.center());
    // The whole pane is highlighted while the row is over its middle.
    assert_eq!(
        target(&mut s),
        Some(DropTarget {
            pane: PaneId(2),
            zone: Zone::Center
        })
    );
    assert_eq!(bounds(&mut s, "drop-highlight"), card);
    release(&mut s, card.center());
    assert_eq!(target(&mut s), None);
    assert_eq!(agent_in(&mut s, 2), Some(AGENT));
    assert_eq!(agent_in(&mut s, 1), None);
    assert_eq!(focused(&mut s), PaneId(2));
}

#[gpui::test]
fn a_row_dropped_on_an_edge_splits_there_and_keeps_the_ratios(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    s.app.update(s.cx, |a, cx| {
        a.move_divider(SplitId(0), 300.0, 800.0, 600.0, cx)
    });
    let ratio = s.app.read_with(s.cx, |a, _| a.layout().ratio(SplitId(0)));
    let row = bounds(&mut s, "tab-7").center();
    let card = bounds(&mut s, "pane-card-2");
    drag_to(&mut s, row, near_bottom(card));
    // The half the new pane would take.
    let lit = bounds(&mut s, "drop-highlight");
    assert_eq!(lit.size.width, card.size.width);
    assert_eq!(lit.size.height, card.size.height / 2.0);
    assert_eq!(lit.bottom(), card.bottom());
    release(&mut s, near_bottom(card));
    let new = PaneId(3);
    assert_eq!(panes(&mut s), vec![PaneId(1), PaneId(2), new]);
    assert_eq!(agent_in(&mut s, 3), Some(AGENT));
    assert_eq!(focused(&mut s), new);
    let below = bounds(&mut s, "pane-card-3");
    assert!(below.top() > card.top());
    assert_eq!(
        s.app.read_with(s.cx, |a, _| a.layout().ratio(SplitId(0))),
        ratio
    );
}

#[gpui::test]
fn a_header_dropped_on_a_pane_swaps_them(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    s.app.update(s.cx, |a, cx| a.show_agent(AGENT, cx));
    let (left, right) = (bounds(&mut s, "pane-card-1"), bounds(&mut s, "pane-card-2"));
    let from = header(&mut s, "pane-card-1");
    drag(&mut s, from, right.center());
    assert_eq!(bounds(&mut s, "pane-card-1"), right);
    assert_eq!(bounds(&mut s, "pane-card-2"), left);
    // The pane took its agent with it, and the focus.
    assert_eq!(agent_in(&mut s, 1), Some(AGENT));
    assert_eq!(panes(&mut s), vec![PaneId(2), PaneId(1)]);
    assert_eq!(focused(&mut s), PaneId(1));
}

#[gpui::test]
fn a_header_dropped_on_an_edge_moves_the_pane_beside_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    let right = bounds(&mut s, "pane-card-2");
    let from = header(&mut s, "pane-card-2");
    let left = bounds(&mut s, "pane-card-1");
    drag(&mut s, from, near_bottom(left));
    // 1 above 2, full width.
    assert_eq!(panes(&mut s), vec![PaneId(1), PaneId(2)]);
    let (top, bottom) = (bounds(&mut s, "pane-card-1"), bounds(&mut s, "pane-card-2"));
    assert_eq!(top.size.width, left.size.width + right.size.width);
    assert_eq!(bottom.top(), top.bottom());
    assert_eq!(focused(&mut s), PaneId(2));
    // And back to the right of 1.
    let from = header(&mut s, "pane-card-2");
    let top = bounds(&mut s, "pane-card-1");
    drag(&mut s, from, near_right(top));
    assert_eq!(bounds(&mut s, "pane-card-2"), right);
}

#[gpui::test]
fn a_pane_over_itself_has_no_target(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    let before = s.app.read_with(s.cx, |a, _| a.layout().clone());
    let card = bounds(&mut s, "pane-card-1");
    let from = header(&mut s, "pane-card-1");
    drag_to(&mut s, from, card.center());
    assert_eq!(target(&mut s), None);
    release(&mut s, card.center());
    assert_eq!(s.app.read_with(s.cx, |a, _| a.layout().clone()), before);
}

#[gpui::test]
fn esc_cancels_a_drag(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    let row = bounds(&mut s, "tab-7").center();
    let card = bounds(&mut s, "pane-card-2");
    drag_to(&mut s, row, card.center());
    assert!(target(&mut s).is_some());
    s.cx.simulate_keystrokes("escape");
    assert_eq!(target(&mut s), None);
    // Moving on and releasing does nothing.
    s.cx.simulate_mouse_move(near_right(card), MouseButton::Left, Modifiers::none());
    release(&mut s, near_right(card));
    assert_eq!(target(&mut s), None);
    assert_eq!(agent_in(&mut s, 2), None);
    assert_eq!(panes(&mut s).len(), 2);
}

#[gpui::test]
fn a_short_press_on_a_row_is_still_a_click(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    let row = bounds(&mut s, "tab-7").center();
    // Under the threshold: a click, shown in the focused pane.
    drag(&mut s, row, row + point(px(2.0), px(1.0)));
    assert_eq!(target(&mut s), None);
    assert_eq!(agent_in(&mut s, 1), Some(AGENT));
}

#[gpui::test]
fn a_row_dragged_back_onto_itself_is_not_a_click(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    let row = bounds(&mut s, "tab-7").center();
    let card = bounds(&mut s, "pane-card-2");
    drag_to(&mut s, row, card.center());
    s.cx.simulate_mouse_move(row, MouseButton::Left, Modifiers::none());
    release(&mut s, row);
    assert_eq!(agent_in(&mut s, 1), None);
    assert_eq!(agent_in(&mut s, 2), None);
}

#[gpui::test]
fn header_buttons_still_click(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    let close = bounds(&mut s, "pane-2-close").center();
    s.cx.simulate_click(close, Modifiers::none());
    s.cx.run_until_parked();
    assert_eq!(panes(&mut s), vec![PaneId(1)]);
}

#[gpui::test]
fn with_four_panes_one_maximized_a_row_on_its_edge_shows_it_there(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = two_panes(cx, dir.path());
    s.app.update(s.cx, |a, cx| {
        a.split(Axis::Vertical, cx);
        a.split(Axis::Vertical, cx);
        a.set_focused(PaneId(1), cx);
        a.toggle_maximize(cx);
    });
    let row = bounds(&mut s, "tab-7").center();
    let card = bounds(&mut s, "pane-card-1");
    drag_to(&mut s, row, near_right(card));
    assert_eq!(
        target(&mut s),
        Some(DropTarget {
            pane: PaneId(1),
            zone: Zone::Center
        })
    );
    release(&mut s, near_right(card));
    assert_eq!(panes(&mut s).len(), 4);
    assert_eq!(agent_in(&mut s, 1), Some(AGENT));
}
