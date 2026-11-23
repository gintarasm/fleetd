//! Benchmark hooks for fleet-perf's GUI wall metrics
//! (`crates/fleet-perf/src/wall/gui.rs`), on when `FLEET_GUI_BENCH` is set:
//!
//! - `cold-start`: once a frame that drew the first `ListAgents` reply in
//!   the sidebar is presented, print `ready` and exit 0.
//! - `input` (with `FLEET_GUI_BENCH_AGENT=<id>`, a `cat` agent): show that
//!   agent in pane 1 (plus `FLEET_GUI_BENCH_EXTRA_PANES=<id,id,..>` in
//!   splits), then [`SAMPLES`] times, [`SAMPLE_GAP`] apart, dispatch the key
//!   `x` through the window's key path and time it until the frame whose
//!   grid holds the echoed `x` is presented; print `input_ns <median>` and
//!   exit 0.
//!
//! - `graph-open`: once the agents are listed, [`OPEN_SAMPLES`] times
//!   toggle the graph view on (`ToggleGraph`) and time it until its first
//!   frame is presented, toggling it off in between; print
//!   `graph_open_ns <median>` and exit 0.
//! - `graph-flood` (with `FLEET_GUI_BENCH_AGENTS=<n>`, the agents to wait
//!   for): once `n` agents are listed, show the graph and time
//!   [`FLOOD_FRAMES`] window redraws, each from the workspace's render to
//!   the end of the graph's paint (the CPU cost of one frame of the graph);
//!   print `graph_frame_ns <median>` and exit 0.
//!
//! In both graph modes the GUI draws a synthetic spawned-by tree (fan-out
//! 3, by id) over agents that have no `fleet.parent`: fleetd refuses
//! `fleet.*` labels from a client, and fleet-perf spawns as the user.
//!
//! In bench mode the connect sheet never shows: a failed connect exits
//! [`EXIT_NO_FLEETD`]; a bench that does not finish within [`WATCHDOG`]
//! exits [`EXIT_TIMEOUT`]. The window floats above other apps' windows
//! ([`window_opened`]): an occluded window presents no frames. The main
//! thread runs at user-interactive QoS ([`interactive_qos`]) however the
//! GUI was launched.

use std::cell::RefCell;
use std::io::Write;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use fleet_term::Cell;
use gpui::{App, AppContext, AsyncApp, Keystroke, Window};

use super::app_state::AppState;
use super::keymap::ToggleGraph;
use crate::core::pane::PaneState;
use crate::core::session::AgentRow;
use crate::core::session::PaneId;

pub const MODE_ENV: &str = "FLEET_GUI_BENCH";
pub const AGENT_ENV: &str = "FLEET_GUI_BENCH_AGENT";
pub const EXTRA_PANES_ENV: &str = "FLEET_GUI_BENCH_EXTRA_PANES";
pub const AGENTS_ENV: &str = "FLEET_GUI_BENCH_AGENTS";
/// Graph openings timed by `graph-open` (median taken).
pub const OPEN_SAMPLES: usize = 20;
/// Frames timed by `graph-flood` (median taken), after [`FLOOD_WARMUP`].
pub const FLOOD_FRAMES: usize = 100;
const FLOOD_WARMUP: usize = 20;
/// The graph's children per synthetic parent.
const SYNTHETIC_FAN_OUT: usize = 3;
/// Keys timed by the `input` bench (median taken), and the pause between them.
pub const SAMPLES: usize = 50;
pub const SAMPLE_GAP: Duration = Duration::from_millis(20);
pub const EXIT_NO_FLEETD: i32 = 3;
pub const EXIT_TIMEOUT: i32 = 4;
/// Bad `FLEET_GUI_BENCH*` settings.
pub const EXIT_USAGE: i32 = 2;
/// A bench still running this long after launch gives up.
pub const WATCHDOG: Duration = Duration::from_secs(30);
/// Settling time between the pane going live and the first key.
const SETTLE: Duration = Duration::from_millis(100);
const POLL: Duration = Duration::from_millis(10);
/// The key typed by the `input` bench (as the PTY echoes it).
const KEY: char = 'x';

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    ColdStart,
    Input { agent: u64, extra: Vec<u64> },
    GraphOpen,
    GraphFlood { agents: usize },
    DiffOpen,
    DiffScroll { agents: usize },
}

/// The bench mode from the `FLEET_GUI_BENCH*` values; `None` when off.
pub fn parse(
    mode: Option<&str>,
    agent: Option<&str>,
    extra: Option<&str>,
    agents: Option<&str>,
) -> Result<Option<Mode>, String> {
    let id = |s: &str, what: &str| {
        s.trim()
            .parse::<u64>()
            .map_err(|_| format!("{what}: not an agent id: {s:?}"))
    };
    match mode {
        None | Some("") => Ok(None),
        Some("cold-start") => Ok(Some(Mode::ColdStart)),
        Some("input") => {
            let agent = id(
                agent.ok_or_else(|| format!("{MODE_ENV}=input needs {AGENT_ENV}"))?,
                AGENT_ENV,
            )?;
            let extra = match extra {
                None | Some("") => Vec::new(),
                Some(list) => list
                    .split(',')
                    .map(|s| id(s, EXTRA_PANES_ENV))
                    .collect::<Result<_, _>>()?,
            };
            Ok(Some(Mode::Input { agent, extra }))
        }
        Some("graph-open") => Ok(Some(Mode::GraphOpen)),
        Some("graph-flood") => {
            let agents = match agents {
                None | Some("") => 1,
                Some(n) => n
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| format!("{AGENTS_ENV}: not a count: {n:?}"))?,
            };
            Ok(Some(Mode::GraphFlood { agents }))
        }
        Some(other) => Err(format!(
            "{MODE_ENV}: unknown mode {other:?} (cold-start, input, graph-open or graph-flood)"
        )),
    }
}

static MODE: OnceLock<Option<Mode>> = OnceLock::new();

/// Reads the bench mode from the environment (exits [`EXIT_USAGE`] on a
/// bad setting) and, in bench mode, arms the watchdog. Call once at launch.
pub fn init() {
    let var = |k: &str| std::env::var(k).ok();
    let mode = match parse(
        var(MODE_ENV).as_deref(),
        var(AGENT_ENV).as_deref(),
        var(EXTRA_PANES_ENV).as_deref(),
        var(AGENTS_ENV).as_deref(),
    ) {
        Ok(mode) => mode,
        Err(e) => {
            eprintln!("fleet-gui: {e}");
            std::process::exit(EXIT_USAGE);
        }
    };
    let on = mode.is_some();
    let _ = MODE.set(mode);
    if on {
        interactive_qos();
        std::thread::spawn(|| {
            std::thread::sleep(WATCHDOG);
            eprintln!(
                "fleet-gui: bench did not finish within {WATCHDOG:?} (no frame presented? a locked screen or hidden window draws none)"
            );
            std::process::exit(EXIT_TIMEOUT);
        });
    }
}

/// Runs the calling (main) thread at user-interactive QoS, as AppKit does
/// for an app LaunchServices knows (launched from Finder, `open` or a
/// terminal app). Under a launchd job (the self-hosted CI runner) the app
/// gets no task role and its main thread stays at default QoS (priority
/// 31-37 instead of 46), so 50 flooding agents and fleetd take the CPU from
/// it: `input_to_render_flood50_ns` read 28-40 ms there against ~11 ms
/// from a terminal. GCD work the main thread submits inherits its QoS.
fn interactive_qos() {
    // SAFETY: plain libpthread call on the current thread.
    let r = unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
    if r != 0 {
        eprintln!("fleet-gui: bench: cannot raise the main thread's QoS (error {r})");
    }
}

fn mode() -> Option<&'static Mode> {
    MODE.get().and_then(Option::as_ref)
}

/// Bench mode is on: the connect sheet never shows.
pub fn active() -> bool {
    mode().is_some()
}

/// A startup connect failed in bench mode.
pub fn connect_failed(reason: Option<&str>) -> ! {
    eprintln!(
        "fleet-gui: bench: cannot connect to fleetd: {}",
        reason.unwrap_or("no saved connection, no local fleetd")
    );
    std::process::exit(EXIT_NO_FLEETD);
}

/// The workspace window opened. In bench mode it floats above other apps'
/// windows on every Space: gpui stops a window's display link while it is
/// occluded, so a window opened behind another app's (fleet-gui is
/// launched from the background and not activated) presents no frame and
/// the bench hits [`WATCHDOG`]. The app is not activated: that costs the
/// cold start ~15-20 ms, and the bench dispatches its keys itself.
pub fn window_opened(window: &Window) {
    if active() {
        raise(window);
    }
}

fn raise(window: &Window) {
    use objc2_app_kit::{NSFloatingWindowLevel, NSView, NSWindowCollectionBehavior};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        return;
    };
    // SAFETY: gpui's AppKit handle points at the window's live content
    // view, and this runs on the main thread (gpui's foreground).
    let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
    let Some(ns_window) = view.window() else {
        return;
    };
    ns_window.setLevel(NSFloatingWindowLevel);
    ns_window.setCollectionBehavior(
        ns_window.collectionBehavior()
            | NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    ns_window.orderFrontRegardless();
}

fn finish(line: &str) -> ! {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
    std::process::exit(0);
}

/// One timed key: dispatched at `t0`; `seen` once a painted grid held its
/// echo, `t1` once that frame was presented.
struct Sample {
    t0: Instant,
    seen: bool,
    t1: Option<Instant>,
}

#[derive(Default)]
struct State {
    /// The first `ListAgents` reply reached the app.
    listed: bool,
    ready_scheduled: bool,
    /// The pane the `input` bench types into, once armed.
    pane: Option<PaneId>,
    /// `KEY`s on that pane's grid before the first sample.
    base: usize,
    samples: Vec<Sample>,
    /// `graph-open`: when the toggle being timed was dispatched.
    open_t0: Option<Instant>,
    open_ns: Vec<u128>,
    /// `graph-flood`: sampling is on, and the frame being timed started.
    flood_armed: bool,
    frame_t0: Option<Instant>,
    frame_ns: Vec<u128>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

/// The session's first agent list arrived (`UiEvent::AgentsChanged` while
/// connected).
pub fn agents_listed() {
    if mode() == Some(&Mode::ColdStart) {
        STATE.with_borrow_mut(|s| s.listed = true);
    }
}

/// The workspace drew its sidebar from the current agent list: once that
/// is the first listed one, `ready` follows the frame's presentation.
pub fn sidebar_drawn(window: &Window) {
    if mode() != Some(&Mode::ColdStart) {
        return;
    }
    let schedule = STATE.with_borrow_mut(|s| {
        let go = s.listed && !s.ready_scheduled;
        s.ready_scheduled |= go;
        go
    });
    if schedule {
        window.on_next_frame(|_, _| finish("ready"));
    }
}

/// `KEY`s among `cells`.
pub fn count_key<'a>(cells: impl IntoIterator<Item = &'a Cell>) -> usize {
    cells.into_iter().filter(|c| c.ch == KEY).count()
}

/// Whether the `input` bench watches `pane`'s paint (cheap when off).
pub fn watches(pane: PaneId) -> bool {
    matches!(mode(), Some(Mode::Input { .. })) && STATE.with_borrow(|s| s.pane == Some(pane))
}

/// `pane` painted a grid holding `count` `KEY`s: each sample echoed by
/// now is timed when this frame is presented.
pub fn painted(pane: PaneId, count: usize, window: &Window) {
    if !watches(pane) {
        return;
    }
    let newly: Vec<usize> = STATE.with_borrow_mut(|s| {
        let echoed = count.saturating_sub(s.base);
        s.samples
            .iter_mut()
            .enumerate()
            .take(echoed)
            .filter(|(_, x)| !x.seen)
            .map(|(i, x)| {
                x.seen = true;
                i
            })
            .collect()
    });
    for i in newly {
        window.on_next_frame(move |_, _| presented(i));
    }
}

fn presented(i: usize) {
    let done = STATE.with_borrow_mut(|s| {
        s.samples[i].t1 = Some(Instant::now());
        (s.samples.len() == SAMPLES && s.samples.iter().all(|x| x.t1.is_some())).then(|| {
            s.samples
                .iter()
                .map(|x| x.t1.unwrap_or(x.t0).duration_since(x.t0).as_nanos())
                .collect::<Vec<_>>()
        })
    });
    if let Some(ns) = done {
        finish(&format!("input_ns {}", median(ns)));
    }
}

pub fn median(mut v: Vec<u128>) -> u128 {
    v.sort_unstable();
    let mid = v.len() / 2;
    match v.len() {
        0 => 0,
        n if n % 2 == 0 => (v[mid - 1] + v[mid]) / 2,
        _ => v[mid],
    }
}

/// `KEY`s on `pane`'s grid, once it shows `agent` live at its laid-out size.
fn live_count(pane: PaneId, agent: u64, cx: &App) -> Option<usize> {
    let app = AppState::global(cx)?;
    let view = app.read(cx).pane_view(pane)?.read(cx);
    view.grid()?;
    let model = view.model_handle().lock().ok()?;
    if model.agent_id() != Some(agent) || model.state() != PaneState::Live {
        return None;
    }
    let term = model.terminal();
    let (_, rows) = term.size();
    let mut cells = Vec::new();
    let mut n = 0;
    for row in 0..rows {
        term.row_cells(row, &mut cells);
        n += count_key(&cells);
    }
    Some(n)
}

/// The app connected to its fleetd: the `input` bench starts, once the
/// window is open (the session may start before it) and the caller,
/// possibly the workspace itself, is done.
pub fn connected(cx: &mut App) {
    match mode() {
        Some(Mode::GraphOpen) => return graph_open(cx),
        Some(Mode::GraphFlood { agents }) => return graph_flood(*agents, cx),
        _ => {}
    }
    let Some(Mode::Input { agent, extra }) = mode() else {
        return;
    };
    let agent = *agent;
    cx.spawn(async move |cx| {
        let window = |cx: &mut AsyncApp| {
            cx.update(|cx| AppState::global(cx).and_then(|a| a.read(cx).window))
                .ok()
                .flatten()
        };
        let handle = loop {
            if let Some(h) = window(cx) {
                break h;
            }
            cx.background_executor().timer(POLL).await;
        };
        let Ok(pane) = handle.update(cx, |ws, window, cx| ws.bench_open(agent, extra, window, cx))
        else {
            return;
        };
        // Until pane 1 shows the agent's snapshot at its laid-out size.
        loop {
            cx.background_executor().timer(POLL).await;
            match cx.update(|cx| live_count(pane, agent, cx)) {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(_) => return,
            }
        }
        cx.background_executor().timer(SETTLE).await;
        let Ok(Some(base)) = cx.update(|cx| live_count(pane, agent, cx)) else {
            return;
        };
        STATE.with_borrow_mut(|s| {
            s.pane = Some(pane);
            s.base = base;
        });
        let key = Keystroke::parse(&KEY.to_string()).expect("a plain key parses");
        let window = handle.into();
        for _ in 0..SAMPLES {
            let typed = cx.update_window(window, |_, window, cx| {
                STATE.with_borrow_mut(|s| {
                    s.samples.push(Sample {
                        t0: Instant::now(),
                        seen: false,
                        t1: None,
                    });
                });
                window.dispatch_keystroke(key.clone(), cx)
            });
            if typed.is_err() {
                return;
            }
            cx.background_executor().timer(SAMPLE_GAP).await;
        }
    })
    .detach();
}

/// Over agents with no `fleet.parent`, in graph bench modes: each agent
/// (by id) after the first is a child of the `(i - 1) / 3`-th, a tree of
/// fan-out [`SYNTHETIC_FAN_OUT`]. `None` otherwise (the real tree).
pub fn synthetic_parents(rows: &[AgentRow]) -> Option<Vec<(u64, Option<u64>)>> {
    if !matches!(mode(), Some(Mode::GraphOpen | Mode::GraphFlood { .. })) {
        return None;
    }
    Some(synthetic_tree(rows.iter().map(|r| r.info.id)))
}

fn synthetic_tree(ids: impl Iterator<Item = u64>) -> Vec<(u64, Option<u64>)> {
    let mut ids: Vec<u64> = ids.collect();
    ids.sort_unstable();
    (0..ids.len())
        .map(|i| {
            let parent = i.checked_sub(1).map(|j| ids[j / SYNTHETIC_FAN_OUT]);
            (ids[i], parent)
        })
        .collect()
}

/// The workspace starts drawing a frame (cheap when off).
pub fn frame_started() {
    if matches!(mode(), Some(Mode::GraphFlood { .. })) {
        STATE.with_borrow_mut(|s| {
            if s.flood_armed {
                s.frame_t0 = Some(Instant::now());
            }
        });
    }
}

/// The graph view rendered: a timed opening ends when this frame is
/// presented.
pub fn graph_rendered(window: &Window) {
    if mode() != Some(&Mode::GraphOpen) {
        return;
    }
    if let Some(t0) = STATE.with_borrow_mut(|s| s.open_t0.take()) {
        window.on_next_frame(move |_, _| {
            STATE.with_borrow_mut(|s| s.open_ns.push(t0.elapsed().as_nanos()));
        });
    }
}

/// The graph finished painting a frame: one `graph-flood` sample, and the
/// next frame is asked for.
pub fn graph_painted(window: &mut Window) {
    if !matches!(mode(), Some(Mode::GraphFlood { .. })) {
        return;
    }
    let done = STATE.with_borrow_mut(|s| {
        if let Some(t0) = s.frame_t0.take() {
            s.frame_ns.push(t0.elapsed().as_nanos());
        }
        (s.frame_ns.len() >= FLOOD_WARMUP + FLOOD_FRAMES)
            .then(|| s.frame_ns[FLOOD_WARMUP..].to_vec())
    });
    match done {
        Some(ns) => finish(&format!("graph_frame_ns {}", median(ns))),
        None if STATE.with_borrow(|s| s.flood_armed) => {
            window.on_next_frame(|window, _| window.refresh());
        }
        None => {}
    }
}

/// A diff pane showed its first diff, `at` being when Show Changes asked
/// for it (`diff-open`).
pub fn diff_opened(at: Instant) {
    if mode() != Some(&Mode::DiffOpen) {
        return;
    }
    STATE.with_borrow_mut(|s| s.open_t0 = Some(at));
}

/// Waits for the workspace window and at least `agents` listed agents.
async fn window_with_agents(
    agents: usize,
    cx: &mut AsyncApp,
) -> Option<gpui::WindowHandle<super::workspace::Workspace>> {
    loop {
        let ready = cx
            .update(|cx| {
                let app = AppState::global(cx)?;
                let app = app.read(cx);
                (app.agents.len() >= agents).then_some(app.window).flatten()
            })
            .ok()?;
        if let Some(h) = ready {
            return Some(h);
        }
        cx.background_executor().timer(POLL).await;
    }
}

fn toggle_graph(
    handle: gpui::WindowHandle<super::workspace::Workspace>,
    cx: &mut AsyncApp,
) -> bool {
    cx.update_window(handle.into(), |_, window, cx| {
        window.dispatch_action(Box::new(ToggleGraph), cx);
    })
    .is_ok()
}

fn graph_open(cx: &mut App) {
    cx.spawn(async move |cx| {
        let Some(handle) = window_with_agents(1, cx).await else {
            return;
        };
        cx.background_executor().timer(SETTLE).await;
        for i in 0..OPEN_SAMPLES {
            STATE.with_borrow_mut(|s| s.open_t0 = Some(Instant::now()));
            if !toggle_graph(handle, cx) {
                return;
            }
            while STATE.with_borrow(|s| s.open_ns.len()) <= i {
                cx.background_executor().timer(POLL).await;
            }
            // Back to the panes, and let that frame go out.
            if !toggle_graph(handle, cx) {
                return;
            }
            cx.background_executor().timer(SETTLE).await;
        }
        let ns = STATE.with_borrow(|s| s.open_ns.clone());
        finish(&format!("graph_open_ns {}", median(ns)));
    })
    .detach();
}

fn graph_flood(agents: usize, cx: &mut App) {
    cx.spawn(async move |cx| {
        let Some(handle) = window_with_agents(agents, cx).await else {
            return;
        };
        if !toggle_graph(handle, cx) {
            return;
        }
        cx.background_executor().timer(SETTLE * 5).await;
        STATE.with_borrow_mut(|s| s.flood_armed = true);
        let _ = cx.update_window(handle.into(), |_, window, _| window.refresh());
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::{Mode, median, parse, synthetic_tree};

    #[test]
    fn mode_parses_from_the_environment_values() {
        assert_eq!(parse(None, None, None, None), Ok(None));
        assert_eq!(
            parse(Some("cold-start"), None, None, None),
            Ok(Some(Mode::ColdStart))
        );
        assert_eq!(
            parse(Some("input"), Some("7"), Some("1,2,3"), None),
            Ok(Some(Mode::Input {
                agent: 7,
                extra: vec![1, 2, 3]
            }))
        );
        assert_eq!(
            parse(Some("input"), Some("7"), None, None),
            Ok(Some(Mode::Input {
                agent: 7,
                extra: vec![]
            }))
        );
        assert!(parse(Some("input"), None, None, None).is_err());
        assert!(parse(Some("input"), Some("x"), None, None).is_err());
        assert!(parse(Some("input"), Some("7"), Some("1,,2"), None).is_err());
        assert!(parse(Some("warm"), None, None, None).is_err());
        assert_eq!(
            parse(Some("graph-open"), None, None, None),
            Ok(Some(Mode::GraphOpen))
        );
        assert_eq!(
            parse(Some("graph-flood"), None, None, Some("50")),
            Ok(Some(Mode::GraphFlood { agents: 50 }))
        );
        assert_eq!(
            parse(Some("graph-flood"), None, None, None),
            Ok(Some(Mode::GraphFlood { agents: 1 }))
        );
        assert!(parse(Some("graph-flood"), None, None, Some("x")).is_err());
    }

    #[test]
    fn median_of_samples() {
        assert_eq!(median(vec![3, 1, 2]), 2);
        assert_eq!(median(vec![4, 1, 3, 2]), 2);
    }

    #[test]
    fn synthetic_tree_fans_out_by_id() {
        let t = synthetic_tree([9, 3, 5, 7, 4].into_iter());
        assert_eq!(
            t,
            [
                (3, None),
                (4, Some(3)),
                (5, Some(3)),
                (7, Some(3)),
                (9, Some(4))
            ]
        );
    }
}
