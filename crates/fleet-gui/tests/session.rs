#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use fleet_client::Connection;
use fleet_gui::core::backoff::Backoff;
use fleet_gui::core::launch::start_local;
use fleet_gui::core::pane::{PaneModel, PaneState};
use fleet_gui::core::session::{ConnState, PaneId, Session, UiEvent};
use fleet_gui::core::target::{Target, connect};
use fleet_proto::{AgentState, Attach, ListAgents, SpawnAgent, request, response};
use fleet_term::{Palette, Terminal};

const DEADLINE: Duration = Duration::from_secs(10);

fn fleetd_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let mut cmd = Command::new(env!("CARGO"));
        cmd.args(["build", "-p", "fleetd", "--message-format=short"]);
        if !cfg!(debug_assertions) {
            cmd.arg("--release");
        }
        assert!(cmd.status().expect("cargo build").success());
        // Never the developer's real login shell. Set here, once, before
        // any test starts a daemon (they all call `fleetd_bin()` first).
        // SAFETY: no other thread of this test binary reads the environment
        // concurrently: every test is blocked on this OnceLock.
        unsafe { std::env::set_var("FLEETD_LOGIN_ENV", "off") };
        let exe = std::env::current_exe().unwrap();
        exe.parent().unwrap().parent().unwrap().join("fleetd")
    })
}

/// Stops the daemons left running for this state dir, found by their
/// command line (there is no pid file).
struct DaemonCleanup(PathBuf);

impl DaemonCleanup {
    fn pids(&self) -> Vec<String> {
        let out = Command::new("pgrep")
            .args(["-f", "--"])
            .arg(format!("--state-dir {}", self.0.display()))
            .output()
            .expect("pgrep");
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }

    /// SIGTERMs the daemon and waits until it is gone.
    fn stop(&self) {
        for pid in self.pids() {
            let _ = Command::new("kill").args(["-TERM", &pid]).status();
        }
        wait_until("the daemon to exit", || self.pids().is_empty());
    }
}

impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        for pid in self.pids() {
            let _ = Command::new("kill").args(["-TERM", &pid]).status();
        }
    }
}

/// A running daemon on a fresh state dir.
struct Daemon {
    dir: tempfile::TempDir,
    cleanup: DaemonCleanup,
}

impl Daemon {
    fn start() -> Daemon {
        let bin = fleetd_bin();
        let dir = tempfile::tempdir().unwrap();
        let cleanup = DaemonCleanup(dir.path().to_owned());
        start_local(bin, dir.path()).expect("start fleetd");
        Daemon { dir, cleanup }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn conn(&self) -> Connection {
        connect(&Target::Local, self.path(), "gui-test")
            .expect("connect")
            .conn
    }

    /// Spawns `/bin/sh -c script` (not attached) and returns its id.
    fn spawn(&self, script: &str, cols: u32, rows: u32) -> u64 {
        let spawn = SpawnAgent {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: "/".into(),
            cols,
            rows,
            ..Default::default()
        };
        match self
            .conn()
            .call(request::Kind::Spawn(Box::new(spawn)))
            .expect("spawn")
        {
            response::Kind::Spawned(info) => info.id,
            other => panic!("unexpected reply {other:?}"),
        }
    }

    fn agent_state(&self, id: u64) -> AgentState {
        match self
            .conn()
            .call(request::Kind::List(ListAgents::default()))
            .expect("list")
        {
            response::Kind::Agents(l) => l
                .agents
                .iter()
                .find(|a| a.id == id)
                .map(|a| a.state())
                .expect("agent listed"),
            other => panic!("unexpected reply {other:?}"),
        }
    }

    /// The node's screen, as `fleet screen` reads it: an observing attach.
    fn screen(&self, id: u64) -> Vec<String> {
        let ok = match self
            .conn()
            .call(request::Kind::Attach(Attach {
                agent_id: id,
                observe: true,
            }))
            .expect("observe")
        {
            response::Kind::Attached(ok) => ok,
            other => panic!("unexpected reply {other:?}"),
        };
        let mut term = Terminal::new_client(ok.cols as u16, ok.rows as u16, palette());
        term.feed(&ok.snapshot);
        term.visible_text()
    }
}

fn palette() -> Palette {
    Palette {
        fg: (0xc0, 0xc0, 0xc0),
        bg: (0x10, 0x20, 0x30),
        cursor: (0xff, 0xff, 0xff),
        ansi: [(0, 0, 0); 16],
    }
}

/// Every `UiEvent` the session sent, collected on a thread.
struct Events(Arc<Mutex<Vec<UiEvent>>>);

impl Events {
    fn has(&self, f: impl Fn(&UiEvent) -> bool) -> bool {
        self.0.lock().unwrap().iter().any(f)
    }

    fn conn_states(&self) -> Vec<ConnState> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                UiEvent::Connection(s) => Some(*s),
                _ => None,
            })
            .collect()
    }
}

fn start_session(d: &Daemon) -> (Session, Events) {
    let (tx, rx) = async_channel::unbounded();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    std::thread::spawn(move || {
        while let Ok(e) = rx.recv_blocking() {
            // Pane output events are many; keep only the rest.
            if !matches!(e, UiEvent::PaneChanged(_)) {
                sink.lock().unwrap().push(e);
            }
        }
    });
    let s = Session::start(Target::Local, d.path().to_owned(), tx);
    s.set_palette(palette());
    (s, Events(events))
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn state(m: &Arc<Mutex<PaneModel>>) -> PaneState {
    m.lock().unwrap().state()
}

fn grid(m: &Arc<Mutex<PaneModel>>) -> Vec<String> {
    m.lock().unwrap().terminal().visible_text()
}

#[test]
fn backoff_sequence() {
    let mut b = Backoff::new();
    let ms: Vec<u128> = (0..7).map(|_| b.next_delay().as_millis()).collect();
    assert_eq!(ms, [250, 500, 1000, 2000, 4000, 5000, 5000]);
    b.reset();
    assert_eq!(b.next_delay(), Duration::from_millis(250));
}

#[test]
fn sidebar_lists_and_follows_activity() {
    let d = Daemon::start();
    let (s, events) = start_session(&d);
    wait_until("the control connection", || {
        events.has(|e| *e == UiEvent::Connection(ConnState::Connected))
    });
    let id = d.spawn("echo hello; sleep 30", 80, 24);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let row = s.agents().into_iter().find(|r| r.info.id == id);
        if row.as_ref().is_some_and(|r| r.last_line == "hello")
            && events.has(|e| *e == UiEvent::AgentsChanged)
        {
            assert!(row.unwrap().last_output_unix_ms.is_some());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "agent row not seen in 2 s: {row:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    s.shutdown();
}

/// The bytes the agent read back, from the `HEX:` dump on its screen.
fn hex_dump(lines: &[String]) -> Option<Vec<u8>> {
    let start = lines.iter().position(|l| l.starts_with("HEX:"))?;
    let end = lines[start..].iter().position(|l| l.starts_with("END"))? + start;
    let text = lines[start..end].join(" ");
    let text = text.trim_start_matches("HEX:");
    Some(
        text.split_whitespace()
            .map(|h| u8::from_str_radix(h, 16).expect("hex byte"))
            .collect(),
    )
}

fn count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

#[test]
fn spawn_attached_pane_answers_startup_queries() {
    let d = Daemon::start();
    let (s, _events) = start_session(&d);
    let pane = PaneId(1);
    let model = s.open_pane(pane, None);
    // Raw mode first, so the replies are neither echoed nor line-buffered;
    // `dd` stops 2 s after the last byte.
    let script = r"stty raw -echo min 0 time 20; printf '\033[?u\033]11;?\033\\\033[6n'; r=$(dd bs=1 count=200 2>/dev/null | od -An -tx1 -v); stty sane; echo; echo HEX:$r; echo END; sleep 30";
    s.spawn(
        "/bin/sh".into(),
        vec!["-c".into(), script.into()],
        "/".into(),
        120,
        30,
        pane,
    )
    .expect("spawn");
    let mut bytes = None;
    wait_until("the agent's hex dump", || {
        bytes = hex_dump(&grid(&model));
        bytes.is_some()
    });
    let bytes = bytes.unwrap();
    assert!(model.lock().unwrap().is_primary());
    assert_eq!(count(&bytes, b"\x1b[?0u"), 1, "kitty reply in {bytes:?}");
    assert_eq!(
        count(&bytes, b"\x1b]11;rgb:"),
        1,
        "OSC 11 reply in {bytes:?}"
    );
    let cpr = bytes
        .windows(2)
        .filter(|w| w[0] == 0x1b && w[1] == b'[')
        .count();
    // `?0u` and the cursor report are the CSI replies.
    assert_eq!(cpr, 2, "CSI replies in {bytes:?}");
    assert_eq!(count(&bytes, b"R"), 1, "cursor report in {bytes:?}");
    s.shutdown();
}

#[test]
fn input_order_preserved_under_flood() {
    let d = Daemon::start();
    let (s, _events) = start_session(&d);
    let mut models = Vec::new();
    for n in 0..3 {
        let id = d.spawn("exec yes", 80, 24);
        models.push(s.open_pane(PaneId(n), Some(id)));
    }
    let cat = d.spawn("exec cat", 250, 24);
    let cat_pane = PaneId(9);
    let cat_model = s.open_pane(cat_pane, Some(cat));
    wait_until("the cat pane to attach", || {
        state(&cat_model) == PaneState::Live
    });
    let expected: String = (0..200)
        .map(|i| char::from(b'0' + (i % 10) as u8))
        .collect();
    let mut slowest = Duration::ZERO;
    for c in expected.bytes() {
        let t = Instant::now();
        s.input(cat_pane, vec![c]);
        slowest = slowest.max(t.elapsed());
    }
    assert!(
        slowest < Duration::from_millis(5),
        "Session::input took {slowest:?}"
    );
    wait_until("the cat pane to show the input in order", || {
        grid(&cat_model).concat().contains(&expected)
    });
    assert!(grid(&cat_model).concat().matches(&expected).count() == 1);
    s.shutdown();
}

#[test]
fn lagged_pane_reattaches_by_itself() {
    let d = Daemon::start();
    let (s, _events) = start_session(&d);
    // Floods once it reads a line, then idles with a stable screen.
    let id = d.spawn("read x; seq 1 2000000; echo DONE; sleep 30", 80, 24);
    let pane = PaneId(1);
    let model = s.open_pane(pane, Some(id));
    wait_until("the pane to attach", || state(&model) == PaneState::Live);
    s.pause_reader_for_test(pane, Duration::from_secs(2));
    s.input(pane, b"\n".to_vec());
    wait_until("a Lagged re-attach", || s.lag_count_for_test(pane) >= 1);
    wait_until("the pane to match the node's screen", || {
        let g = grid(&model);
        state(&model) == PaneState::Live && g.iter().any(|l| l == "DONE") && g == d.screen(id)
    });
    s.shutdown();
}

#[test]
fn daemon_restart_reconnects_and_reattaches() {
    let d = Daemon::start();
    let (s, events) = start_session(&d);
    let id = d.spawn("sleep 60", 80, 24);
    let model = s.open_pane(PaneId(1), Some(id));
    wait_until("the pane to attach", || state(&model) == PaneState::Live);
    wait_until("the control connection", || {
        events.has(|e| *e == UiEvent::Connection(ConnState::Connected))
    });
    let before = events.conn_states().len();
    d.cleanup.stop();
    wait_until("the pane to see the disconnect", || {
        state(&model) == PaneState::Disconnected
    });
    start_local(fleetd_bin(), d.path()).expect("restart fleetd");
    wait_until("Disconnected then Connected", || {
        let states = events.conn_states();
        let after = &states[before..];
        let down = after
            .iter()
            .position(|s| matches!(s, ConnState::Disconnected { .. }));
        down.is_some_and(|i| after[i..].contains(&ConnState::Connected))
    });
    wait_until("the pane to settle", || {
        matches!(
            state(&model),
            PaneState::Exited(_) | PaneState::Lost | PaneState::Live
        )
    });
    if state(&model) == PaneState::Live {
        assert_eq!(d.agent_state(id), AgentState::Running);
    }
    s.shutdown();
}

#[test]
fn close_pane_detaches_without_killing() {
    let d = Daemon::start();
    let (s, _events) = start_session(&d);
    let id = d.spawn("sleep 30", 80, 24);
    let pane = PaneId(1);
    let model = s.open_pane(pane, Some(id));
    wait_until("the pane to attach", || state(&model) == PaneState::Live);
    s.close_pane(pane);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(d.agent_state(id), AgentState::Running);
    s.shutdown();
}
