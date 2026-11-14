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
//! In bench mode the connect sheet never shows: a failed connect exits
//! [`EXIT_NO_FLEETD`]; a bench that does not finish within [`WATCHDOG`]
//! exits [`EXIT_TIMEOUT`].

use std::cell::RefCell;
use std::io::Write;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use fleet_term::Cell;
use gpui::{App, AppContext, Keystroke, Window};

use super::app_state::AppState;
use crate::core::pane::PaneState;
use crate::core::session::PaneId;

pub const MODE_ENV: &str = "FLEET_GUI_BENCH";
pub const AGENT_ENV: &str = "FLEET_GUI_BENCH_AGENT";
pub const EXTRA_PANES_ENV: &str = "FLEET_GUI_BENCH_EXTRA_PANES";
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
}

/// The bench mode from the `FLEET_GUI_BENCH*` values; `None` when off.
pub fn parse(
    mode: Option<&str>,
    agent: Option<&str>,
    extra: Option<&str>,
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
        Some(other) => Err(format!(
            "{MODE_ENV}: unknown mode {other:?} (cold-start or input)"
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
        std::thread::spawn(|| {
            std::thread::sleep(WATCHDOG);
            eprintln!(
                "fleet-gui: bench did not finish within {WATCHDOG:?} (no frame presented? a locked screen or hidden window draws none)"
            );
            std::process::exit(EXIT_TIMEOUT);
        });
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

/// The app connected to its fleetd: the `input` bench starts (once the
/// caller, possibly the workspace itself, is done).
pub fn connected(cx: &mut App) {
    if matches!(mode(), Some(Mode::Input { .. })) {
        cx.defer(start_input);
    }
}

fn start_input(cx: &mut App) {
    let Some(Mode::Input { agent, extra }) = mode() else {
        return;
    };
    let Some(app) = AppState::global(cx) else {
        return;
    };
    let Some(handle) = app.read(cx).window else {
        eprintln!("fleet-gui: bench: no window");
        std::process::exit(EXIT_TIMEOUT);
    };
    let Ok(pane) = handle.update(cx, |ws, window, cx| {
        ws.bench_open(*agent, extra, window, cx)
    }) else {
        return;
    };
    let agent = *agent;
    cx.spawn(async move |cx| {
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

#[cfg(test)]
mod tests {
    use super::{Mode, median, parse};

    #[test]
    fn mode_parses_from_the_environment_values() {
        assert_eq!(parse(None, None, None), Ok(None));
        assert_eq!(
            parse(Some("cold-start"), None, None),
            Ok(Some(Mode::ColdStart))
        );
        assert_eq!(
            parse(Some("input"), Some("7"), Some("1,2,3")),
            Ok(Some(Mode::Input {
                agent: 7,
                extra: vec![1, 2, 3]
            }))
        );
        assert_eq!(
            parse(Some("input"), Some("7"), None),
            Ok(Some(Mode::Input {
                agent: 7,
                extra: vec![]
            }))
        );
        assert!(parse(Some("input"), None, None).is_err());
        assert!(parse(Some("input"), Some("x"), None).is_err());
        assert!(parse(Some("input"), Some("7"), Some("1,,2")).is_err());
        assert!(parse(Some("warm"), None, None).is_err());
    }

    #[test]
    fn median_of_samples() {
        assert_eq!(median(vec![3, 1, 2]), 2);
        assert_eq!(median(vec![4, 1, 3, 2]), 2);
    }
}
