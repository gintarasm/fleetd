use std::sync::{Arc, Mutex};

use gpui::{
    Bounds, ClipboardItem, Entity, EntityInputHandler, Modifiers, MouseButton, MouseDownEvent,
    Pixels, Point, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase, VisualTestContext,
    bounds, point, px, size,
};

use gpui::{ExternalPaths, FileDropEvent, Image, ImageFormat};

use super::ime::ime_cell;
use super::mouse::point_to_cell;
use super::{InputSink, PaneChrome, PaneEvent, PaneView};
use crate::app::keymap::{Copy, Paste, bind_keys};
use crate::app::theme::Theme;
use crate::core::config::GuiConfig;
use crate::core::cursor_blink::{self, Blink, BlinkChoice};
use crate::core::drop_paste::{DROP_NEEDS_LOCAL, IMAGE_NEEDS_LOCAL, shell_quote};
use crate::core::pane::{PaneModel, ViewRow};
use crate::core::selection::{LineRef, SelPoint};
use crate::core::session::PaneId;
use fleet_proto::AttachOk;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Recorder {
    inputs: Mutex<Vec<Vec<u8>>>,
    resizes: Mutex<Vec<(u16, u16)>>,
    paging_checks: Mutex<u32>,
}

impl InputSink for Recorder {
    fn input(&self, _pane: PaneId, bytes: Vec<u8>) {
        self.inputs.lock().unwrap().push(bytes);
    }

    fn resize(&self, _pane: PaneId, cols: u16, rows: u16) {
        self.resizes.lock().unwrap().push((cols, rows));
    }

    fn check_paging(&self, _pane: PaneId) {
        *self.paging_checks.lock().unwrap() += 1;
    }
}

impl Recorder {
    fn take(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.inputs.lock().unwrap())
    }
}

fn pane(
    cx: &mut TestAppContext,
    model: PaneModel,
) -> (Entity<PaneView>, Arc<Recorder>, &mut VisualTestContext) {
    cx.update(bind_keys);
    let sink = Arc::new(Recorder::default());
    let model = Arc::new(Mutex::new(model));
    let sink_dyn: Arc<dyn InputSink> = sink.clone();
    let (view, cx) = cx.add_window_view(|_window, cx| {
        PaneView::new(PaneId(1), model, sink_dyn, &GuiConfig::default(), cx)
    });
    cx.update(|window, cx| window.focus(&view.read(cx).focus_handle().clone()));
    cx.run_until_parked();
    (view, sink, cx)
}

fn blank_model() -> PaneModel {
    PaneModel::new(crate::app::theme::Theme::dark().palette)
}

/// A live pane with `n` history lines `h0..`, an 80×24 grid and the
/// cursor on the last row.
fn model_with_history(n: usize) -> PaneModel {
    let mut model = blank_model();
    model.on_attached(&AttachOk {
        agent_id: 1,
        cols: 80,
        rows: 24,
        history_tail: (0..n)
            .map(|i| format!("h{i}").into_bytes().into())
            .collect(),
        ..Default::default()
    });
    model.on_output(b"\x1b[24;1H");
    model
}

/// The middle of cell `(col, row)` in window coordinates.
fn cell(view: &Entity<PaneView>, cx: &mut VisualTestContext, col: u16, row: u16) -> Point<Pixels> {
    let l = view.read_with(cx, |v, _| v.layout).expect("laid out");
    point(
        l.origin.x + l.cell_w * (f32::from(col) + 0.5),
        l.origin.y + l.line_h * (f32::from(row) + 0.5),
    )
}

fn wheel(cx: &mut VisualTestContext, position: Point<Pixels>, delta: ScrollDelta) {
    cx.simulate_event(ScrollWheelEvent {
        position,
        delta,
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
    });
}

fn offset(view: &Entity<PaneView>, cx: &mut VisualTestContext) -> u64 {
    view.read_with(cx, |v, _| v.model().scroll_offset())
}

fn selected(view: &Entity<PaneView>, cx: &mut VisualTestContext) -> Option<String> {
    view.read_with(cx, |v, _| v.model().selection_text())
}

fn drag(cx: &mut VisualTestContext, from: Point<Pixels>, to: Point<Pixels>, modifiers: Modifiers) {
    cx.simulate_mouse_down(from, MouseButton::Left, modifiers);
    cx.simulate_mouse_move(to, MouseButton::Left, modifiers);
    cx.simulate_mouse_up(to, MouseButton::Left, modifiers);
}

/// The pane's blink timer is running.
fn blinking(view: &Entity<PaneView>, cx: &mut VisualTestContext) -> bool {
    view.read_with(cx, |v, _| v.blink_timer.is_some())
}

/// A focused pane with a live agent (one that draws a cursor).
fn blinking_pane(
    cx: &mut TestAppContext,
    model: PaneModel,
) -> (Entity<PaneView>, &mut VisualTestContext) {
    let (view, _sink, cx) = pane(cx, model);
    agent_pane(&view, cx, true);
    cx.update(|window, _| window.activate_window());
    redraw(cx);
    (view, cx)
}

fn redraw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

#[gpui::test]
fn the_cursor_blinks_when_the_app_asks_and_only_while_focused(cx: &mut TestAppContext) {
    let mut model = model_with_history(0);
    model.on_output(b"\x1b[5 q");
    let (view, cx) = blinking_pane(cx, model);
    assert!(blinking(&view, cx), "a focused pane blinks as asked");

    // Half a period on it hides, then a keypress shows it again.
    let shown = |view: &Entity<PaneView>, cx: &mut VisualTestContext| {
        view.read_with(cx, |v, _| v.blink.shown(Instant::now()))
    };
    assert!(shown(&view, cx));
    view.update(cx, |v, _| {
        v.blink = Blink::new(Instant::now() - cursor_blink::PERIOD);
    });
    assert!(!shown(&view, cx));
    cx.simulate_keystrokes("a");
    assert!(shown(&view, cx), "typing restarts the blink shown");

    cx.update(|window, _| window.blur());
    redraw(cx);
    assert!(!blinking(&view, cx), "no timer in an unfocused pane");
}

#[gpui::test]
fn the_setting_overrides_the_app_and_reduce_motion_stops_it(cx: &mut TestAppContext) {
    let (view, cx) = blinking_pane(cx, model_with_history(0));
    assert!(!blinking(&view, cx), "a steady cursor by default");

    let with = |choice| GuiConfig {
        cursor_blink: choice,
        ..GuiConfig::default()
    };
    view.update(cx, |v, cx| v.apply_config(&with(BlinkChoice::On), cx));
    redraw(cx);
    assert!(blinking(&view, cx), "Always blinks a steady cursor");

    cx.update(|_, cx| cx.set_global(crate::app::controls::ReducedMotion(true)));
    redraw(cx);
    assert!(!blinking(&view, cx), "reduced motion: no blink");
    cx.update(|_, cx| cx.set_global(crate::app::controls::ReducedMotion(false)));

    view.update(cx, |v, _| v.model().on_output(b"\x1b[1 q"));
    view.update(cx, |v, cx| v.apply_config(&with(BlinkChoice::Off), cx));
    redraw(cx);
    assert!(!blinking(&view, cx), "Never wins over the app");
}

#[gpui::test]
fn wheel_scrolls_history_when_no_mouse_mode(cx: &mut TestAppContext) {
    let (view, sink, cx) = pane(cx, model_with_history(50));
    let at = cell(&view, cx, 3, 3);
    wheel(cx, at, ScrollDelta::Lines(point(0.0, 3.0)));
    assert_eq!(offset(&view, cx), 3);
    assert!(*sink.paging_checks.lock().unwrap() >= 1);
    assert!(sink.take().is_empty());

    // Pixel deltas scroll a line only once a whole line height adds up.
    let line_h = view.read_with(cx, |v, _| v.layout.unwrap().line_h);
    wheel(cx, at, ScrollDelta::Pixels(point(px(0.0), line_h * 0.6)));
    assert_eq!(offset(&view, cx), 3);
    wheel(cx, at, ScrollDelta::Pixels(point(px(0.0), line_h * 0.6)));
    assert_eq!(offset(&view, cx), 4);
    wheel(cx, at, ScrollDelta::Lines(point(0.0, -2.0)));
    assert_eq!(offset(&view, cx), 2);

    // A huge delta is clamped, and scrolling stops at the oldest line.
    wheel(cx, at, ScrollDelta::Lines(point(0.0, 1e9)));
    assert_eq!(offset(&view, cx), 50);
    // Typing returns to the live grid.
    cx.simulate_keystrokes("a");
    assert_eq!(offset(&view, cx), 0);
}

#[gpui::test]
fn wheel_reports_when_mouse_mode_on(cx: &mut TestAppContext) {
    let mut model = model_with_history(50);
    model.on_output(b"\x1b[?1000h\x1b[?1006h");
    let (view, sink, cx) = pane(cx, model);
    sink.take();
    let at = cell(&view, cx, 3, 2);
    wheel(cx, at, ScrollDelta::Lines(point(0.0, 1.0)));
    assert_eq!(sink.take(), vec![b"\x1b[<64;4;3M".to_vec()]);
    wheel(cx, at, ScrollDelta::Lines(point(0.0, -2.0)));
    assert_eq!(sink.take(), vec![b"\x1b[<65;4;3M\x1b[<65;4;3M".to_vec()]);
    assert_eq!(offset(&view, cx), 0);
    // A huge delta is clamped to 100 reports.
    wheel(cx, at, ScrollDelta::Lines(point(0.0, 1e9)));
    assert_eq!(sink.take(), vec![b"\x1b[<64;4;3M".repeat(100)]);
}

#[gpui::test]
fn wheel_on_alt_screen_sends_arrows(cx: &mut TestAppContext) {
    let mut model = model_with_history(50);
    model.on_output(b"\x1b[?1049h");
    let (view, sink, cx) = pane(cx, model);
    sink.take();
    let at = cell(&view, cx, 0, 0);
    wheel(cx, at, ScrollDelta::Lines(point(0.0, 2.0)));
    assert_eq!(sink.take(), vec![b"\x1b[A\x1b[A".to_vec()]);
    assert_eq!(offset(&view, cx), 0);
}

#[gpui::test]
fn drag_selects_and_cmd_c_copies(cx: &mut TestAppContext) {
    let mut model = blank_model();
    model.on_output(b"hello world\r\nsecond line");
    let (view, sink, cx) = pane(cx, model);
    let (from, to) = (cell(&view, cx, 2, 0), cell(&view, cx, 5, 1));
    drag(cx, from, to, Modifiers::none());
    assert_eq!(selected(&view, cx).as_deref(), Some("llo world\nsecond"));

    cx.dispatch_action(Copy);
    let clip = cx.read_from_clipboard().and_then(|c| c.text());
    assert_eq!(clip.as_deref(), Some("llo world\nsecond"));

    // The keystrokes, not just the actions.
    cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
    cx.simulate_keystrokes("cmd-c");
    let clip = cx.read_from_clipboard().and_then(|c| c.text());
    assert_eq!(clip.as_deref(), Some("llo world\nsecond"));
    sink.take();
    cx.write_to_clipboard(ClipboardItem::new_string("pasted".to_owned()));
    cx.simulate_keystrokes("cmd-v");
    assert_eq!(sink.take(), vec![b"pasted".to_vec()]);

    // A plain click clears the selection.
    cx.simulate_click(from, Modifiers::none());
    assert_eq!(selected(&view, cx), None);
}

#[gpui::test]
fn copying_flashes_copied_for_a_moment(cx: &mut TestAppContext) {
    let mut model = blank_model();
    model.on_output(b"hello world");
    let (view, _sink, cx) = pane(cx, model);
    // Nothing selected: nothing copied, nothing shown.
    cx.dispatch_action(Copy);
    assert!(!view.read_with(cx, |v, _| v.copied_shown()));
    let (from, to) = (cell(&view, cx, 0, 0), cell(&view, cx, 4, 0));
    drag(cx, from, to, Modifiers::none());
    cx.dispatch_action(Copy);
    cx.run_until_parked();
    assert!(view.read_with(cx, |v, _| v.copied_shown()));
    assert!(
        cx.debug_bounds("pane-0-copied").is_some(),
        "Copied is drawn"
    );
    cx.executor()
        .advance_clock(Duration::from_millis(crate::core::feedback::COPIED_MS));
    cx.run_until_parked();
    assert!(
        !view.read_with(cx, |v, _| v.copied_shown()),
        "and goes by itself"
    );
    assert!(cx.debug_bounds("pane-0-copied").is_none());
}

#[gpui::test]
fn drag_selects_across_history_and_grid(cx: &mut TestAppContext) {
    let mut model = model_with_history(50);
    model.on_output(b"\x1b[1;1Hgrid top");
    let (view, _sink, cx) = pane(cx, model);
    let at = cell(&view, cx, 0, 0);
    wheel(cx, at, ScrollDelta::Lines(point(0.0, 2.0)));
    // Rows 0 and 1 show h48 and h49, row 2 the grid's first row.
    let (from, to) = (cell(&view, cx, 1, 1), cell(&view, cx, 3, 2));
    drag(cx, from, to, Modifiers::none());
    assert_eq!(selected(&view, cx).as_deref(), Some("49\ngrid"));
}

#[gpui::test]
fn shift_bypasses_mouse_reporting(cx: &mut TestAppContext) {
    let mut model = blank_model();
    model.on_output(b"\x1b[?1002h\x1b[?1006hhello world");
    let (view, sink, cx) = pane(cx, model);
    sink.take();
    let (from, to) = (cell(&view, cx, 0, 0), cell(&view, cx, 4, 0));
    drag(cx, from, to, Modifiers::shift());
    assert!(sink.take().is_empty());
    assert_eq!(selected(&view, cx).as_deref(), Some("hello"));

    // Without Shift the button events are reported, not selected.
    drag(cx, from, to, Modifiers::none());
    assert_eq!(
        sink.take(),
        vec![
            b"\x1b[<0;1;1M".to_vec(),
            b"\x1b[<32;5;1M".to_vec(),
            b"\x1b[<0;5;1m".to_vec()
        ]
    );
}

#[gpui::test]
fn shift_click_extends_the_selection_even_with_mouse_reporting(cx: &mut TestAppContext) {
    let mut model = blank_model();
    model.on_output(b"\x1b[?1002h\x1b[?1006hhello world\r\nsecond line");
    let (view, sink, cx) = pane(cx, model);
    sink.take();
    let (from, to) = (cell(&view, cx, 0, 0), cell(&view, cx, 4, 0));
    drag(cx, from, to, Modifiers::shift());
    assert_eq!(selected(&view, cx).as_deref(), Some("hello"));
    let at = cell(&view, cx, 5, 1);
    cx.simulate_click(at, Modifiers::shift());
    assert_eq!(selected(&view, cx).as_deref(), Some("hello world\nsecond"));
    // And back before the anchor.
    let at = cell(&view, cx, 1, 0);
    cx.simulate_click(at, Modifiers::shift());
    assert_eq!(selected(&view, cx).as_deref(), Some("he"));
    assert!(sink.take().is_empty(), "nothing reported");
}

#[gpui::test]
fn dragging_past_the_top_scrolls_the_history_and_keeps_selecting(cx: &mut TestAppContext) {
    let mut model = model_with_history(50);
    model.on_output(b"\x1b[1;1Hgrid top");
    let (view, sink, cx) = pane(cx, model);
    let l = view.read_with(cx, |v, _| v.layout.unwrap());
    let at = cell(&view, cx, 3, 0);
    cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
    // Half a line above the grid: one line a tick.
    let above = point(l.origin.x + l.cell_w * 0.5, l.origin.y - l.line_h * 0.5);
    cx.simulate_mouse_move(above, MouseButton::Left, Modifiers::none());
    assert_eq!(offset(&view, cx), 0, "the first move only arms the timer");
    let tick = Duration::from_millis(crate::core::mouse::DRAG_SCROLL_MS);
    cx.executor().advance_clock(tick * 3);
    cx.run_until_parked();
    assert_eq!(offset(&view, cx), 3);
    assert_eq!(selected(&view, cx).as_deref(), Some("h47\nh48\nh49\ngrid"));
    assert!(*sink.paging_checks.lock().unwrap() >= 1, "older pages load");

    // Further away: faster.
    let far = point(above.x, l.origin.y - l.line_h * 2.5);
    cx.simulate_mouse_move(far, MouseButton::Left, Modifiers::none());
    cx.executor().advance_clock(tick);
    cx.run_until_parked();
    assert_eq!(offset(&view, cx), 6);

    // Back over the grid it stops; released, it stays stopped.
    let at = cell(&view, cx, 0, 5);
    cx.simulate_mouse_move(at, MouseButton::Left, Modifiers::none());
    cx.executor().advance_clock(tick * 3);
    cx.run_until_parked();
    assert_eq!(offset(&view, cx), 6);
    assert!(view.read_with(cx, |v, _| v.auto_scroll.is_none()));
    cx.simulate_mouse_move(far, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(far, MouseButton::Left, Modifiers::none());
    cx.executor().advance_clock(tick * 3);
    cx.run_until_parked();
    let after = offset(&view, cx);
    cx.executor().advance_clock(tick * 3);
    cx.run_until_parked();
    assert_eq!(offset(&view, cx), after, "a released drag scrolls no more");
    assert!(view.read_with(cx, |v, _| v.auto_scroll.is_none()));
}

#[gpui::test]
fn focus_changes_are_reported_while_the_program_asks(cx: &mut TestAppContext) {
    let mut model = blank_model();
    model.on_output(b"\x1b[?1004h");
    let (view, sink, cx) = pane(cx, model);
    // A test window starts inactive: the focused pane is not reported.
    assert!(sink.take().is_empty());
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    assert_eq!(sink.take(), vec![b"\x1b[I".to_vec()], "window activated");

    cx.deactivate_window();
    assert_eq!(sink.take(), vec![b"\x1b[O".to_vec()], "window deactivated");
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    assert_eq!(sink.take(), vec![b"\x1b[I".to_vec()], "and again");

    cx.update(|window, _| window.blur());
    cx.run_until_parked();
    assert_eq!(sink.take(), vec![b"\x1b[O".to_vec()], "focus left the pane");
    cx.update(|window, cx| window.focus(&view.read(cx).focus_handle().clone()));
    cx.run_until_parked();
    assert_eq!(sink.take(), vec![b"\x1b[I".to_vec()], "and came back");

    view.update(cx, |v, _| v.model().on_output(b"\x1b[?1004l"));
    cx.update(|window, _| window.blur());
    cx.run_until_parked();
    assert!(sink.take().is_empty(), "mode off: nothing sent");
    view.update(cx, |v, _| v.model().on_output(b"\x1b[?1004h"));
    cx.update(|window, cx| window.focus(&view.read(cx).focus_handle().clone()));
    cx.run_until_parked();
    assert_eq!(sink.take(), vec![b"\x1b[I".to_vec()]);
}

fn hovered(view: &Entity<PaneView>, cx: &mut VisualTestContext) -> Option<(LineRef, u16, u16)> {
    view.read_with(cx, |v, _| v.hovered_link())
}

/// Records the commands the view would run to open files.
fn record_opens(
    view: &Entity<PaneView>,
    cx: &mut VisualTestContext,
) -> Arc<Mutex<Vec<Vec<String>>>> {
    let opened = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&opened);
    view.update(cx, |v, _| {
        v.open_file = Arc::new(move |argv, _| sink.lock().unwrap().push(argv));
    });
    opened
}

#[gpui::test]
fn cmd_hover_underlines_a_url_and_cmd_click_opens_it(cx: &mut TestAppContext) {
    let mut model = blank_model();
    model.on_output(b"PR https://a.io/p/1 ok");
    let (view, _sink, cx) = pane(cx, model);
    let at = cell(&view, cx, 6, 0);

    // Without Command: no link, and a click opens nothing.
    cx.simulate_mouse_move(at, None, Modifiers::none());
    assert_eq!(hovered(&view, cx), None);
    cx.simulate_click(at, Modifiers::none());
    assert_eq!(cx.opened_url(), None);

    cx.simulate_mouse_move(at, None, Modifiers::command());
    assert_eq!(hovered(&view, cx), Some((LineRef::Grid(0), 3, 19)));
    // Off the link.
    let p = cell(&view, cx, 20, 0);
    cx.simulate_mouse_move(p, None, Modifiers::command());
    assert_eq!(hovered(&view, cx), None);

    cx.simulate_click(at, Modifiers::command());
    assert_eq!(cx.opened_url().as_deref(), Some("https://a.io/p/1"));
    // The click does not start a selection.
    assert_eq!(selected(&view, cx), None);
}

#[gpui::test]
fn pressing_and_releasing_cmd_over_a_link_toggles_the_underline(cx: &mut TestAppContext) {
    let mut model = blank_model();
    model.on_output(b"https://a.io");
    let (view, _sink, cx) = pane(cx, model);
    let p = cell(&view, cx, 2, 0);
    cx.simulate_mouse_move(p, None, Modifiers::none());
    cx.simulate_modifiers_change(Modifiers::command());
    assert_eq!(hovered(&view, cx), Some((LineRef::Grid(0), 0, 12)));
    cx.simulate_modifiers_change(Modifiers::none());
    assert_eq!(hovered(&view, cx), None);
}

#[gpui::test]
fn cmd_click_wins_over_mouse_reporting(cx: &mut TestAppContext) {
    let mut model = blank_model();
    model.on_output(b"\x1b[?1000h\x1b[?1006hhttps://a.io");
    let (view, sink, cx) = pane(cx, model);
    sink.take();
    let p = cell(&view, cx, 2, 0);
    cx.simulate_click(p, Modifiers::command());
    assert_eq!(cx.opened_url().as_deref(), Some("https://a.io"));
    assert!(sink.take().is_empty());
}

#[gpui::test]
fn an_osc8_link_opens_its_uri(cx: &mut TestAppContext) {
    let mut model = blank_model();
    model.on_output(b"see \x1b]8;;https://a.io/pr/9\x1b\\the PR\x1b]8;;\x1b\\.");
    let (view, _sink, cx) = pane(cx, model);
    let at = cell(&view, cx, 5, 0);
    cx.simulate_mouse_move(at, None, Modifiers::command());
    assert_eq!(hovered(&view, cx), Some((LineRef::Grid(0), 4, 10)));
    cx.simulate_click(at, Modifiers::command());
    assert_eq!(cx.opened_url().as_deref(), Some("https://a.io/pr/9"));
}

#[gpui::test]
fn cmd_click_opens_an_existing_file_of_a_local_agent_in_the_editor(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
    let mut model = blank_model();
    model.on_output(b"--> src/main.rs:10:5 and src/gone.rs:1");
    let (view, _sink, cx) = pane(cx, model);
    let opened = record_opens(&view, cx);
    let at = cell(&view, cx, 6, 0);

    // A remote agent (no local directory): no file links.
    cx.simulate_mouse_move(at, None, Modifiers::command());
    assert_eq!(hovered(&view, cx), None);

    let cwd = dir.path().to_string_lossy().into_owned();
    view.update(cx, |v, _| {
        v.chrome.link_dir = Some(cwd.clone());
        v.editor = "zed".to_owned();
    });
    cx.simulate_mouse_move(at, None, Modifiers::command());
    assert_eq!(hovered(&view, cx), Some((LineRef::Grid(0), 4, 20)));
    cx.simulate_click(at, Modifiers::command());
    let file = format!("{cwd}/src/main.rs:10:5");
    assert_eq!(*opened.lock().unwrap(), vec![vec!["zed".to_owned(), file]]);

    // A path that does not exist is no link.
    let p = cell(&view, cx, 28, 0);
    cx.simulate_mouse_move(p, None, Modifiers::command());
    assert_eq!(hovered(&view, cx), None);
}

#[gpui::test]
fn double_click_selects_word_triple_click_line(cx: &mut TestAppContext) {
    let mut model = blank_model();
    model.on_output(b"cargo test -p fleet-gui");
    let (view, _sink, cx) = pane(cx, model);
    let at = cell(&view, cx, 16, 0);
    let down = |click_count| MouseDownEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::none(),
        click_count,
        first_mouse: false,
    };
    cx.simulate_event(down(1));
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    cx.simulate_event(down(2));
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    assert_eq!(selected(&view, cx).as_deref(), Some("fleet-gui"));
    cx.simulate_event(down(3));
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    assert_eq!(
        selected(&view, cx).as_deref(),
        Some("cargo test -p fleet-gui")
    );
}

#[gpui::test]
fn new_output_chip_shows_while_scrolled_up_and_returns_to_bottom(cx: &mut TestAppContext) {
    let (view, _sink, cx) = pane(cx, model_with_history(50));
    let at = cell(&view, cx, 0, 0);
    wheel(cx, at, ScrollDelta::Lines(point(0.0, 5.0)));
    assert_eq!(view.read_with(cx, |v, _| v.chip), None);
    // A redraw in place is no new output.
    view.update(cx, |v, cx| {
        v.model().on_output(b"\rspinning");
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |v, _| v.chip), None);
    view.update(cx, |v, cx| {
        v.model().on_output(b"\r\nx\r\ny");
        cx.notify();
    });
    cx.run_until_parked();
    let chip = view.read_with(cx, |v, _| v.chip).expect("chip shown");
    assert_eq!(offset(&view, cx), 7);
    cx.simulate_click(chip.center(), Modifiers::none());
    assert_eq!(offset(&view, cx), 0);
    assert_eq!(view.read_with(cx, |v, _| v.chip), None);
    assert_eq!(selected(&view, cx), None);
}

#[gpui::test]
fn end_returns_a_scrolled_up_view_to_the_bottom(cx: &mut TestAppContext) {
    let (view, sink, cx) = pane(cx, model_with_history(50));
    let at = cell(&view, cx, 0, 0);
    wheel(cx, at, ScrollDelta::Lines(point(0.0, 5.0)));
    view.update(cx, |v, cx| {
        v.model().on_output(b"\r\nx");
        cx.notify();
    });
    cx.run_until_parked();
    assert!(view.read_with(cx, |v, _| v.chip.is_some()));
    cx.simulate_keystrokes("end");
    assert_eq!(offset(&view, cx), 0);
    assert!(sink.take().is_empty(), "End stays in the GUI");
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |v, _| v.chip), None);
    // At the bottom, End goes to the agent.
    cx.simulate_keystrokes("end");
    assert_eq!(sink.take(), vec![b"\x1b[F".to_vec()]);
}

#[gpui::test]
fn scroll_keys_page_and_jump_through_history(cx: &mut TestAppContext) {
    let (view, sink, cx) = pane(cx, model_with_history(100));
    sink.take();
    cx.simulate_keystrokes("shift-pageup");
    assert_eq!(offset(&view, cx), 23);
    cx.simulate_keystrokes("shift-pageup shift-pagedown");
    assert_eq!(offset(&view, cx), 23);
    cx.simulate_keystrokes("cmd-up");
    assert_eq!(offset(&view, cx), 100);
    cx.simulate_keystrokes("cmd-down");
    assert_eq!(offset(&view, cx), 0);
    cx.simulate_keystrokes("cmd-home");
    assert_eq!(offset(&view, cx), 100);
    cx.simulate_keystrokes("cmd-end");
    assert_eq!(offset(&view, cx), 0);
    assert!(sink.take().is_empty(), "the keys stay in the GUI");
}

#[gpui::test]
fn scroll_keys_go_to_the_program_on_the_alternate_screen(cx: &mut TestAppContext) {
    let mut model = model_with_history(100);
    model.on_output(b"\x1b[?1049h");
    let (view, sink, cx) = pane(cx, model);
    sink.take();
    cx.simulate_keystrokes("shift-pageup");
    cx.simulate_keystrokes("shift-pagedown");
    cx.simulate_keystrokes("cmd-up");
    assert_eq!(offset(&view, cx), 0);
    assert_eq!(
        sink.take(),
        vec![b"\x1b[5;2~".to_vec(), b"\x1b[6;2~".to_vec()]
    );
}

#[gpui::test]
fn cmd_up_in_the_find_field_moves_its_caret_not_the_view(cx: &mut TestAppContext) {
    let (view, _sink, cx) = pane(cx, model_with_history(100));
    cx.simulate_keystrokes("cmd-f");
    cx.simulate_keystrokes("cmd-up");
    assert_eq!(offset(&view, cx), 0);
    // Paging works from the find field too.
    cx.simulate_keystrokes("shift-pageup");
    assert_eq!(offset(&view, cx), 23);
}

#[gpui::test]
fn cmd_a_selects_history_and_grid_for_cmd_c(cx: &mut TestAppContext) {
    let mut model = model_with_history(3);
    model.on_output(b"$ ls");
    let (view, sink, cx) = pane(cx, model);
    sink.take();
    cx.simulate_keystrokes("cmd-a");
    assert_eq!(
        selected(&view, cx).as_deref(),
        Some(format!("h0\nh1\nh2{}\n$ ls", "\n".repeat(23)).as_str())
    );
    cx.simulate_keystrokes("cmd-c");
    let clip = cx.read_from_clipboard().and_then(|c| c.text());
    assert!(clip.is_some_and(|t| t.starts_with("h0\nh1") && t.ends_with("$ ls")));
    assert!(sink.take().is_empty());
}

#[gpui::test]
fn cmd_k_clears_the_view_history_only(cx: &mut TestAppContext) {
    let (view, sink, cx) = pane(cx, model_with_history(100));
    sink.take();
    cx.simulate_keystrokes("shift-pageup");
    cx.simulate_keystrokes("cmd-k");
    assert_eq!(offset(&view, cx), 0);
    assert!(view.read_with(cx, |v, _| v.model().scrollback().is_empty()));
    assert!(sink.take().is_empty(), "nothing reaches the agent");
    // Nothing left to scroll.
    cx.simulate_keystrokes("shift-pageup");
    assert_eq!(offset(&view, cx), 0);
    assert_eq!(
        view.read_with(cx, |v, _| v.scroll_state()),
        (false, false, false)
    );
}

/// A pane whose scrollbar shows at once (Reduce Motion: no fade-in).
fn pane_without_motion(
    cx: &mut TestAppContext,
    model: PaneModel,
) -> (Entity<PaneView>, Arc<Recorder>, &mut VisualTestContext) {
    let (view, sink, cx) = pane(cx, model);
    view.update(cx, |v, _| v.reduce_motion = || true);
    (view, sink, cx)
}

#[gpui::test]
fn the_scrollbar_shows_only_once_the_view_scrolls(cx: &mut TestAppContext) {
    let (view, _sink, cx) = pane_without_motion(cx, model_with_history(50));
    // Idle: nothing shown, nothing scheduled.
    view.update(cx, |v, cx| {
        v.model().on_output(b"\r\nx");
        cx.notify();
    });
    cx.run_until_parked();
    let idle = view.read_with(cx, |v, _| {
        (
            v.scrollbar_shown,
            v.fade.is_active(),
            v.fade_timer.is_some(),
        )
    });
    assert_eq!(idle, (false, false, false));
    let at = cell(&view, cx, 0, 0);
    wheel(cx, at, ScrollDelta::Lines(point(0.0, 5.0)));
    cx.run_until_parked();
    let shown = view.read_with(cx, |v, _| (v.scrollbar_shown, v.fade_timer.is_some()));
    assert_eq!(shown, (true, true), "shown, one timer to fade it");
}

#[gpui::test]
fn dragging_the_thumb_scrolls_the_view(cx: &mut TestAppContext) {
    let (view, sink, cx) = pane_without_motion(cx, model_with_history(50));
    let at = cell(&view, cx, 0, 0);
    wheel(cx, at, ScrollDelta::Lines(point(0.0, 1.0)));
    cx.run_until_parked();
    let track = view
        .read_with(cx, |v, _| v.scrollbar_track())
        .expect("laid out");
    let thumb = view.read_with(cx, |v, _| v.thumb_on(track)).expect("thumb");
    let grab = point(
        track.center().x,
        track.origin.y + px(thumb.top + thumb.height / 2.0),
    );
    let top = point(track.center().x, track.origin.y);
    cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
    assert!(view.read_with(cx, |v, _| v.fade.is_held()));
    cx.simulate_mouse_move(top, MouseButton::Left, Modifiers::none());
    assert_eq!(offset(&view, cx), 50, "dragged to the oldest line");
    let bottom = point(track.center().x, track.bottom() + px(100.0));
    cx.simulate_mouse_move(bottom, MouseButton::Left, Modifiers::none());
    assert_eq!(offset(&view, cx), 0, "dragged back to the live grid");
    cx.simulate_mouse_up(bottom, MouseButton::Left, Modifiers::none());
    assert!(!view.read_with(cx, |v, _| v.fade.is_held()));
    // Nothing was selected or sent, and paging was checked.
    assert_eq!(selected(&view, cx), None);
    assert!(sink.take().is_empty());
    assert!(*sink.paging_checks.lock().unwrap() >= 2);
}

#[gpui::test]
fn a_press_on_the_track_jumps_the_thumb_there(cx: &mut TestAppContext) {
    let (view, _sink, cx) = pane_without_motion(cx, model_with_history(50));
    let at = cell(&view, cx, 0, 0);
    wheel(cx, at, ScrollDelta::Lines(point(0.0, 1.0)));
    cx.run_until_parked();
    let track = view
        .read_with(cx, |v, _| v.scrollbar_track())
        .expect("laid out");
    let top = point(track.center().x, track.origin.y + px(1.0));
    cx.simulate_mouse_down(top, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(top, MouseButton::Left, Modifiers::none());
    assert_eq!(offset(&view, cx), 50);
}

#[gpui::test]
fn a_hidden_scrollbar_leaves_the_right_edge_to_the_selection(cx: &mut TestAppContext) {
    let (view, _sink, cx) = pane_without_motion(cx, model_with_history(50));
    let track = view
        .read_with(cx, |v, _| v.scrollbar_track())
        .expect("laid out");
    let edge = point(track.center().x, track.origin.y + px(1.0));
    cx.simulate_mouse_down(edge, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(edge, MouseButton::Left, Modifiers::none());
    assert_eq!(offset(&view, cx), 0);
    assert!(!view.read_with(cx, |v, _| v.fade.is_held()));
}

#[gpui::test]
fn ime_position_follows_the_cursor_else_the_top_left(cx: &mut TestAppContext) {
    let (view, _sink, cx) = pane(cx, model_with_history(50));
    let ime_bounds = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.replace_and_mark_text_in_range(None, "に", None, window, cx);
                let at = v.bounds_for_range(0..1, Bounds::default(), window, cx);
                (at.map(|b| b.origin), ime_cell(&v.model()))
            })
        })
    };
    let l = view.read_with(cx, |v, _| v.layout).expect("laid out");
    // The cursor is on the last row.
    let (at, ime) = ime_bounds(cx);
    assert_eq!(ime, (0, 23));
    assert_eq!(at, Some(point(l.origin.x, l.origin.y + l.line_h * 23.0)));
    let top = cell(&view, cx, 0, 0);
    wheel(cx, top, ScrollDelta::Lines(point(0.0, 3.0)));
    let (at, ime) = ime_bounds(cx);
    assert_eq!(ime, (0, 0));
    assert_eq!(at, Some(l.origin));
    // The overlay draws there too (paint smoke test while scrolled).
    cx.run_until_parked();
}

#[gpui::test]
fn unmark_text_commits_the_marked_text(cx: &mut TestAppContext) {
    let (view, sink, cx) = pane(cx, blank_model());
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.replace_and_mark_text_in_range(None, "é", None, window, cx);
            v.unmark_text(window, cx);
            assert_eq!(v.marked_text_range(window, cx), None);
        });
    });
    assert_eq!(sink.take(), vec!["é".as_bytes().to_vec()]);
}

#[test]
fn point_to_cell_clamps() {
    let b = bounds(point(px(10.0), px(20.0)), size(px(800.0), px(480.0)));
    let cell = |x: f32, y: f32| point_to_cell(point(px(x), px(y)), b, px(8.0), px(16.0), 80, 24);
    assert_eq!(cell(10.0, 20.0), (0, 0));
    assert_eq!(cell(10.0 + 8.0 * 2.5, 20.0 + 16.0 * 1.9), (2, 1));
    assert_eq!(cell(0.0, 0.0), (0, 0));
    assert_eq!(cell(5000.0, 5000.0), (79, 23));
    assert_eq!(cell(f32::NAN, -1.0), (0, 0));
    assert_eq!(
        point_to_cell(point(px(50.0), px(50.0)), b, px(8.0), px(16.0), 0, 0),
        (0, 0)
    );
}

#[gpui::test]
fn typing_sends_bytes_through_session(cx: &mut TestAppContext) {
    let (_view, sink, cx) = pane(cx, blank_model());
    sink.take(); // nothing typed yet
    cx.simulate_keystrokes("a enter alt-b ctrl-c");
    assert_eq!(
        sink.take(),
        vec![
            b"a".to_vec(),
            b"\r".to_vec(),
            b"\x1bb".to_vec(),
            b"\x03".to_vec()
        ]
    );
}

#[gpui::test]
fn ime_marked_text_is_not_sent_until_committed(cx: &mut TestAppContext) {
    let (view, sink, cx) = pane(cx, blank_model());
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.replace_and_mark_text_in_range(None, "に", None, window, cx);
        });
    });
    assert!(sink.take().is_empty());
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            assert_eq!(v.marked_text_range(window, cx), Some(0..1));
            v.replace_text_in_range(None, "日本", window, cx);
            assert_eq!(v.marked_text_range(window, cx), None);
        });
    });
    assert_eq!(sink.take(), vec!["日本".as_bytes().to_vec()]);
}

#[gpui::test]
fn paste_uses_bracketed_mode(cx: &mut TestAppContext) {
    let mut model = blank_model();
    model.on_output(b"\x1b[?2004h");
    assert!(model.terminal().modes().bracketed_paste);
    let (_view, sink, cx) = pane(cx, model);
    cx.write_to_clipboard(ClipboardItem::new_string("x\ny".to_owned()));
    cx.dispatch_action(Paste);
    assert_eq!(sink.take(), vec![b"\x1b[200~x\ry\x1b[201~".to_vec()]);
}

/// The pane's notices, as they are emitted.
fn notices(view: &Entity<PaneView>, cx: &mut VisualTestContext) -> Arc<Mutex<Vec<String>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    cx.update(|_, cx| {
        cx.subscribe(view, move |_, e: &PaneEvent, _| {
            if let PaneEvent::Notice(text) = e {
                log.lock().unwrap().push((*text).to_owned());
            }
        })
        .detach();
    });
    seen
}

/// A pane showing a running agent, on this machine (`local`) or not.
fn agent_pane(view: &Entity<PaneView>, cx: &mut VisualTestContext, local: bool) {
    view.update(cx, |v, _| {
        v.chrome.has_agent = true;
        v.chrome.local = local;
    });
}

#[gpui::test]
fn pasting_an_image_saves_it_and_pastes_its_path(cx: &mut TestAppContext) {
    let (view, sink, cx) = pane(cx, blank_model());
    let dir = tempfile::tempdir().unwrap();
    agent_pane(&view, cx, true);
    view.update(cx, |v, _| v.paste_dir = dir.path().to_path_buf());
    let image = Image::from_bytes(ImageFormat::Png, b"png bytes".to_vec());
    cx.write_to_clipboard(ClipboardItem::new_image(&image));
    cx.dispatch_action(Paste);
    cx.run_until_parked();

    let files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1);
    let file = &files[0];
    let name = file.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("fleet-paste-") && name.ends_with(".png"),
        "{name}"
    );
    assert_eq!(std::fs::read(file).unwrap(), b"png bytes");
    let typed = shell_quote(&file.to_string_lossy());
    assert_eq!(sink.take(), vec![typed.into_bytes()]);
}

#[gpui::test]
fn text_on_the_clipboard_wins_over_nothing_and_an_image_needs_a_local_agent(
    cx: &mut TestAppContext,
) {
    let (view, sink, cx) = pane(cx, blank_model());
    let dir = tempfile::tempdir().unwrap();
    agent_pane(&view, cx, false);
    view.update(cx, |v, _| v.paste_dir = dir.path().to_path_buf());
    let seen = notices(&view, cx);
    let image = Image::from_bytes(ImageFormat::Png, b"png".to_vec());
    cx.write_to_clipboard(ClipboardItem::new_image(&image));
    cx.dispatch_action(Paste);
    cx.run_until_parked();

    assert_eq!(sink.take(), Vec::<Vec<u8>>::new());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    assert_eq!(*seen.lock().unwrap(), vec![IMAGE_NEEDS_LOCAL.to_owned()]);

    // Text still pastes into a remote agent.
    cx.write_to_clipboard(ClipboardItem::new_string("hi".to_owned()));
    cx.dispatch_action(Paste);
    assert_eq!(sink.take(), vec![b"hi".to_vec()]);
}

#[gpui::test]
fn dropped_paths_are_typed_quoted_and_bracketed(cx: &mut TestAppContext) {
    let mut model = blank_model();
    model.on_output(b"\x1b[?2004h");
    let (view, sink, cx) = pane(cx, model);
    let seen = notices(&view, cx);
    let paths = vec!["/a/b.txt".to_owned(), "/c d/it's.png".to_owned()];

    // No agent: nothing.
    view.update(cx, |v, cx| v.type_paths(&paths, cx));
    assert_eq!(sink.take(), Vec::<Vec<u8>>::new());

    agent_pane(&view, cx, true);
    view.update(cx, |v, cx| v.type_paths(&paths, cx));
    let typed = b"\x1b[200~/a/b.txt '/c d/it'\\''s.png'\x1b[201~".to_vec();
    assert_eq!(sink.take(), vec![typed]);

    // An ended agent: nothing.
    view.update(cx, |v, cx| {
        v.chrome.agent_ended = true;
        v.type_paths(&paths, cx);
    });
    assert_eq!(sink.take(), Vec::<Vec<u8>>::new());

    // On another machine the paths would not exist: a notice.
    agent_pane(&view, cx, false);
    view.update(cx, |v, cx| {
        v.chrome.agent_ended = false;
        v.type_paths(&paths, cx);
    });
    assert_eq!(sink.take(), Vec::<Vec<u8>>::new());
    assert_eq!(*seen.lock().unwrap(), vec![DROP_NEEDS_LOCAL.to_owned()]);
}

#[gpui::test]
fn a_file_drag_highlights_the_pane_and_the_drop_focuses_it(cx: &mut TestAppContext) {
    let (view, _sink, cx) = pane(cx, model_with_history(0));
    agent_pane(&view, cx, true);
    cx.update(|window, _| window.blur());
    let at = cell(&view, cx, 10, 5);
    assert!(cx.debug_bounds("pane-0-file-drop").is_none());

    cx.simulate_event(FileDropEvent::Entered {
        position: at,
        paths: ExternalPaths::default(),
    });
    cx.simulate_event(FileDropEvent::Pending { position: at });
    cx.run_until_parked();
    assert!(cx.debug_bounds("pane-0-file-drop").is_some());

    cx.simulate_event(FileDropEvent::Submit { position: at });
    cx.run_until_parked();
    let focused = cx.update(|window, cx| view.read(cx).focus_handle().is_focused(window));
    assert!(focused, "the drop gives the pane the keyboard");
}

#[gpui::test]
fn layout_resizes_the_session(cx: &mut TestAppContext) {
    let (view, sink, cx) = pane(cx, blank_model());
    let layout = view.read_with(cx, |v, _| v.layout).expect("laid out");
    let resizes = sink.resizes.lock().unwrap().clone();
    assert_eq!(resizes, vec![(layout.cols, layout.rows)]);
    // Not primary: the local grid keeps the agent's size.
    let grid = view.read_with(cx, |v, _| v.model().terminal().size());
    assert_eq!(grid, (80, 24));
}

#[gpui::test]
fn primary_resizes_its_local_grid(cx: &mut TestAppContext) {
    let mut model = blank_model();
    model.on_attached(&AttachOk {
        agent_id: 1,
        cols: 80,
        rows: 24,
        primary: true,
        ..Default::default()
    });
    let (view, _sink, cx) = pane(cx, model);
    let (layout, grid) = view.read_with(cx, |v, _| (v.layout, v.model().terminal().size()));
    let layout = layout.expect("laid out");
    assert_eq!(grid, (layout.cols, layout.rows));
}

#[gpui::test]
fn paints_styled_wide_selected_and_marked_cells(cx: &mut TestAppContext) {
    let mut model = blank_model();
    model
        .on_output("\x1b[1;31mred\x1b[0m \x1b[7m日本\x1b[0m e\u{301} \x1b[4;9mx\x1b[0m".as_bytes());
    model.select_start(SelPoint {
        line: LineRef::Grid(0),
        col: 1,
    });
    model.select_extend(SelPoint {
        line: LineRef::Grid(0),
        col: 6,
    });
    let (view, _sink, cx) = pane(cx, model);
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.replace_and_mark_text_in_range(None, "に", None, window, cx);
        });
    });
    cx.run_until_parked();
    let marked = view.read_with(cx, |v, _| v.marked.clone());
    assert_eq!(marked.as_deref(), Some("に"));
}

/// A live 80×24 pane whose history has `needle` on lines 10 and 40.
fn model_with_needles() -> PaneModel {
    let mut model = blank_model();
    model.on_attached(&AttachOk {
        agent_id: 1,
        cols: 80,
        rows: 24,
        history_tail: (0..50)
            .map(|i| match i {
                10 | 40 => format!("a Needle {i}"),
                _ => format!("h{i}"),
            })
            .map(|s| s.into_bytes().into())
            .collect(),
        ..Default::default()
    });
    model.on_output(b"\x1b[24;1H");
    model
}

fn find_status(
    view: &Entity<PaneView>,
    cx: &mut VisualTestContext,
) -> Option<crate::core::find::FindStatus> {
    view.read_with(cx, |v, _| v.model().find_status())
}

fn shows_line(view: &Entity<PaneView>, cx: &mut VisualTestContext, line: u64) -> bool {
    view.read_with(cx, |v, _| {
        let mut rows = Vec::new();
        v.model().view_rows(24, &mut rows);
        rows.contains(&ViewRow::History(line))
    })
}

#[gpui::test]
fn cmd_f_finds_in_history_and_steps_through_matches(cx: &mut TestAppContext) {
    let (view, sink, cx) = pane(cx, model_with_needles());
    sink.take();
    cx.simulate_keystrokes("cmd-f");
    assert!(cx.debug_bounds("pane-0-find").is_some());
    cx.simulate_input("needle");
    cx.run_until_parked();
    // The query went to the find field, not to the agent.
    assert!(sink.take().is_empty());
    let status = find_status(&view, cx).unwrap();
    assert_eq!((status.index, status.count), (Some(2), 2));
    assert!(shows_line(&view, cx, 40));
    assert_eq!(
        view.read_with(cx, |v, _| v.find_label()).as_deref(),
        Some("2 of 2")
    );

    // Enter goes up to the older match, Shift-Enter back down.
    cx.simulate_keystrokes("enter");
    assert_eq!(find_status(&view, cx).unwrap().index, Some(1));
    assert!(shows_line(&view, cx, 10));
    cx.simulate_keystrokes("shift-enter");
    assert_eq!(find_status(&view, cx).unwrap().index, Some(2));
    cx.simulate_keystrokes("cmd-g");
    assert_eq!(find_status(&view, cx).unwrap().index, Some(1));
    cx.simulate_keystrokes("cmd-shift-g");
    assert_eq!(find_status(&view, cx).unwrap().index, Some(2));
    assert!(sink.take().is_empty());

    // Case sensitive: "needle" is not "Needle".
    let case = cx.debug_bounds("pane-0-find-case").unwrap().center();
    cx.simulate_click(case, Modifiers::none());
    cx.run_until_parked();
    assert_eq!(find_status(&view, cx).unwrap().count, 0);
    assert_eq!(
        view.read_with(cx, |v, _| v.find_label()).as_deref(),
        Some("No matches")
    );
    cx.simulate_click(case, Modifiers::none());
    cx.run_until_parked();
    assert_eq!(find_status(&view, cx).unwrap().count, 2);

    // Esc closes it and gives the keys back to the agent.
    cx.simulate_keystrokes("escape");
    assert_eq!(find_status(&view, cx), None);
    assert_eq!(view.read_with(cx, |v, _| v.find_label()), None);
    assert!(sink.take().is_empty());
    cx.simulate_keystrokes("enter");
    assert_eq!(sink.take(), vec![b"\r".to_vec()]);

    // ⌘F again brings the last query back, selected.
    cx.simulate_keystrokes("cmd-f");
    cx.run_until_parked();
    assert_eq!(find_status(&view, cx).unwrap().count, 2);
    cx.simulate_input("h4");
    cx.run_until_parked();
    // h4, h40..h49 minus the needle on 40.
    assert_eq!(find_status(&view, cx).unwrap().count, 10);
}

#[gpui::test]
fn find_bar_buttons_step_and_close(cx: &mut TestAppContext) {
    let (view, _sink, cx) = pane(cx, model_with_needles());
    cx.simulate_keystrokes("cmd-f");
    cx.simulate_input("needle");
    cx.run_until_parked();
    let click = |cx: &mut VisualTestContext, id: &'static str| {
        let at = cx.debug_bounds(id).unwrap().center();
        cx.simulate_click(at, Modifiers::none());
        cx.run_until_parked();
    };
    click(cx, "pane-0-find-up");
    assert_eq!(find_status(&view, cx).unwrap().index, Some(1));
    click(cx, "pane-0-find-down");
    assert_eq!(find_status(&view, cx).unwrap().index, Some(2));
    click(cx, "pane-0-find-close");
    assert_eq!(find_status(&view, cx), None);
    // The keys go back to the terminal.
    let focused = cx.update(|window, cx| view.read(cx).focus_handle().is_focused(window));
    assert!(focused);
}

#[gpui::test]
fn find_follows_output_while_open(cx: &mut TestAppContext) {
    let (view, _sink, cx) = pane(cx, model_with_needles());
    cx.simulate_keystrokes("cmd-f");
    cx.simulate_input("needle");
    cx.run_until_parked();
    let model = view.read_with(cx, |v, _| Arc::clone(v.model_handle()));
    model.lock().unwrap().on_output(b"\r\nneedle again");
    // The session notifies the view of output.
    view.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    assert_eq!(find_status(&view, cx).unwrap().count, 3);
}

/// Issue #33: the focused pane's ring is its agent's colour; with no
/// agent it is the accent, and an unfocused pane keeps the plain border.
#[test]
fn the_focus_ring_takes_the_agent_colour() {
    use super::render::{focus_ring, header_icon_color, ring_color};
    use crate::app::agent_mark::agent_color;
    let theme = Theme::light();
    let focused = PaneChrome {
        focused: true,
        agent: Some(3),
        ..PaneChrome::default()
    };
    assert_eq!(focus_ring(&focused, &theme), agent_color(3, &theme));
    assert_eq!(header_icon_color(&focused, &theme), agent_color(3, &theme));
    let unfocused = PaneChrome {
        focused: false,
        ..focused.clone()
    };
    assert_eq!(
        ring_color(&theme, focus_ring(&unfocused, &theme), 0.0),
        theme.border
    );
    // Unfocused, the icon still names the agent.
    assert_eq!(
        header_icon_color(&unfocused, &theme),
        agent_color(3, &theme)
    );
    let empty = PaneChrome {
        focused: true,
        ..PaneChrome::default()
    };
    assert_eq!(focus_ring(&empty, &theme), theme.accent.opacity(0.55));
    assert_eq!(header_icon_color(&empty, &theme), theme.accent);
    let empty_unfocused = PaneChrome::default();
    assert_eq!(header_icon_color(&empty_unfocused, &theme), theme.tertiary);
}
