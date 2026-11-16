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
use fleet_gui::core::spawn::{KIND_LABEL, SHELL_KIND, SpawnRequest, new_terminal};
use fleet_gui::core::status::{AgentStatus, QUIET_MS, agent_status};
use fleet_gui::core::target::{Target, connect};
use fleet_proto::{
    AgentList, AgentState, Attach, Frame, HelloOk, ListAgents, Response, RestartingDaemon,
    RestoreSummary, SpawnAgent, request, response,
};
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

/// An agent's `fleet report` reaches the sidebar's rows at once (the
/// `AgentReported` event), without waiting for the next list.
#[test]
fn sidebar_follows_an_agents_report() {
    let d = Daemon::start();
    let (s, events) = start_session(&d);
    wait_until("the control connection", || {
        events.has(|e| *e == UiEvent::Connection(ConnState::Connected))
    });
    let tok = d.path().join("a.tok");
    let id = d.spawn(
        &format!(
            "printf %s \"$FLEET_AGENT_TOKEN\" > {0}.tmp && mv {0}.tmp {0}; exec sleep 30",
            tok.display()
        ),
        80,
        24,
    );
    wait_until("the agent's token", || tok.exists());
    let token = std::fs::read_to_string(&tok).unwrap();
    let sock = d.path().join("fleetd.sock");
    let mut agent = Connection::unix_as(&sock, "agent", Some(&token)).expect("connect as agent");
    wait_until("the agent listed", || {
        s.agents().iter().any(|r| r.info.id == id)
    });
    agent
        .call(request::Kind::Report(fleet_proto::ReportStatus {
            state: fleet_proto::ReportedState::ReportedWaiting.into(),
            message: "need a review".into(),
        }))
        .expect("report");
    wait_until("the report in the row", || {
        s.agents()
            .iter()
            .find(|r| r.info.id == id)
            .and_then(|r| r.info.reported.clone())
            .is_some_and(|r| r.message == "need a review")
    });
    // Its status is the reported one, drawn solid.
    let row = s.agents().into_iter().find(|r| r.info.id == id).unwrap();
    assert_eq!(
        fleet_gui::core::status::agent_status(&row, false, i64::MAX / 2),
        Some(fleet_gui::core::status::AgentStatus::NeedsYou)
    );
    s.shutdown();
}

/// `Session::start_with` uses the connection it is given (here one to a
/// daemon in another dir than the session's target, so a fresh connect
/// could not have listed this agent) and, once it drops, reconnects to
/// the target as before.
#[test]
fn start_with_uses_the_given_connection_first() {
    let d = Daemon::start();
    let id = d.spawn("sleep 30", 80, 24);
    let other = tempfile::tempdir().unwrap();
    let first = connect(&Target::Local, d.path(), "gui").expect("connect");
    let (tx, rx) = async_channel::unbounded();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    std::thread::spawn(move || {
        while let Ok(e) = rx.recv_blocking() {
            sink.lock().unwrap().push(e);
        }
    });
    let events = Events(events);
    let s = Session::start_with(Target::Local, other.path().to_owned(), tx, Some(first));
    wait_until("the agent listed over the given connection", || {
        s.agents().iter().any(|r| r.info.id == id)
    });
    assert_eq!(
        events.conn_states().first(),
        Some(&ConnState::Connecting),
        "{:?}",
        events.conn_states()
    );
    assert!(events.has(|e| *e == UiEvent::Connection(ConnState::Connected)));
    // The daemon goes away: the session retries its own target (which has
    // no daemon) with backoff, unchanged.
    d.cleanup.stop();
    wait_until("a reconnect attempt to the target", || {
        events
            .conn_states()
            .iter()
            .any(|c| matches!(c, ConnState::Disconnected { .. }))
    });
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
        SpawnRequest {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: "/".into(),
            labels: Default::default(),
        },
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

/// New terminal: a login shell in the chosen folder, labelled
/// `kind=shell`, shown in the pane it was spawned for.
#[test]
fn new_terminal_spawns_a_labelled_login_shell() {
    let d = Daemon::start();
    let (s, _events) = start_session(&d);
    let pane = PaneId(1);
    let model = s.open_pane(pane, None);
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let cwd = cwd.to_string_lossy().into_owned();
    s.spawn(
        new_terminal(&Target::Local, Some("/bin/sh"), cwd.clone(), &[]),
        80,
        24,
        pane,
    )
    .expect("spawn");
    let mut row = None;
    wait_until("the shell in the sidebar", || {
        row = s.agents().into_iter().next();
        row.is_some()
    });
    let info = row.unwrap().info;
    assert_eq!(info.program, "/bin/sh");
    assert_eq!(info.args, ["-l"]);
    assert_eq!(info.cwd, cwd);
    assert_eq!(
        info.labels.get(KIND_LABEL).map(String::as_str),
        Some(SHELL_KIND)
    );
    assert!(info.labels["name"].starts_with("shell-"), "{info:?}");
    wait_until("the pane to show the shell", || {
        model.lock().unwrap().agent_id() == Some(info.id)
    });
    // A login shell may still be reading its profile when the first `pwd`
    // arrives, and a profile script can swallow typed input (the likely
    // cause of a timeout on CI's Ubuntu runner), so type it again until
    // its output shows.
    let deadline = Instant::now() + DEADLINE;
    let mut typed: Option<Instant> = None;
    loop {
        if typed.is_none_or(|t| t.elapsed() >= Duration::from_millis(500)) {
            s.input(pane, b"pwd\n".to_vec());
            typed = Some(Instant::now());
        }
        let screen = grid(&model);
        if screen.iter().any(|l| l.trim() == cwd) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the shell's pwd ({cwd}); screen:\n{}",
            screen.join("\n")
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    s.shutdown();
}

/// Rename sets the `name` label, keeps the others, and the sidebar
/// shows it without waiting for the periodic list.
#[test]
fn rename_sets_the_name_label_and_relists() {
    let d = Daemon::start();
    let (s, events) = start_session(&d);
    wait_until("the control connection", || {
        events.has(|e| *e == UiEvent::Connection(ConnState::Connected))
    });
    let id = d.spawn("sleep 30", 80, 24);
    wait_until("the agent listed", || {
        s.agents().iter().any(|r| r.info.id == id)
    });
    let started = Instant::now();
    s.rename(id, "lto-falcon".into());
    wait_until("the new name", || {
        s.agents().iter().any(|r| {
            r.info.id == id && r.info.labels.get("name").map(String::as_str) == Some("lto-falcon")
        })
    });
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "relisted at once"
    );
    s.shutdown();
}

#[test]
fn read_output_gives_an_exited_agents_whole_history_as_plain_text() {
    let d = Daemon::start();
    let (s, events) = start_session(&d);
    wait_until("the control connection", || {
        events.has(|e| *e == UiEvent::Connection(ConnState::Connected))
    });
    // More lines than the 10-row screen: most of them are history.
    let id = d.spawn(
        "i=1; while [ $i -le 40 ]; do printf '\\033[31mline %d\\033[0m\\n' $i; i=$((i+1)); done",
        80,
        10,
    );
    wait_until("the agent to exit", || {
        d.agent_state(id) == AgentState::Exited
    });
    let text = s.read_output(id)().expect("read output");
    assert!(!text.contains('\x1b'), "no escapes: {text:?}");
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    let want: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
    assert_eq!(lines, want);
    assert!(s.read_output(id + 1000)().is_err());
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

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Issue #32: a real agent's status, from the list and its activity.
#[test]
fn status_follows_real_agents() {
    let d = Daemon::start();
    let (s, _events) = start_session(&d);
    let busy = d.spawn("while :; do echo tick; sleep 0.2; done", 80, 24);
    let asks = d.spawn("printf 'Proceed? '; sleep 30", 80, 24);
    let quiet = d.spawn("echo compiling; sleep 30", 80, 24);
    let ok = d.spawn("sleep 0.3; exit 0", 80, 24);
    let bad = d.spawn("sleep 0.3; exit 3", 80, 24);
    let status = |id: u64| {
        s.agents()
            .into_iter()
            .find(|r| r.info.id == id)
            .and_then(|r| agent_status(&r, false, unix_ms()))
    };
    wait_until("done and failed", || {
        status(ok) == Some(AgentStatus::Done) && status(bad) == Some(AgentStatus::Failed)
    });
    wait_until("busy to be working", || {
        status(busy) == Some(AgentStatus::Working)
    });
    // Quiet for QUIET_MS after their one line.
    std::thread::sleep(Duration::from_millis(QUIET_MS as u64));
    wait_until("the prompt to need you", || {
        status(asks) == Some(AgentStatus::NeedsYou)
    });
    assert_eq!(status(quiet), Some(AgentStatus::Idle));
    assert_eq!(status(busy), Some(AgentStatus::Working));
    s.shutdown();
}

/// Issue #32: a BEL in an attached pane's output reaches its model once.
#[test]
fn a_bell_reaches_the_pane_showing_the_agent() {
    let d = Daemon::start();
    let (s, _events) = start_session(&d);
    let id = d.spawn("sleep 0.5; printf 'ding\\a'; sleep 30", 80, 24);
    let model = s.open_pane(PaneId(1), Some(id));
    wait_until("the bell", || model.lock().unwrap().take_bell());
    assert!(!model.lock().unwrap().take_bell());
    s.shutdown();
}

/// A scripted fleetd on a unix socket: one handshake per connection, from
/// `hellos` in order (the last repeats), `List` answered with no agents,
/// and `RestartDaemon` answered by `on_restart` (`None`: an old daemon's
/// "empty request"), after which the connection is dropped, as an exec
/// would.
struct FakeFleetd {
    dir: tempfile::TempDir,
    /// `RestartDaemon` requests received, on any connection.
    restarts: Arc<std::sync::atomic::AtomicUsize>,
    /// Their `binary`, in order.
    binaries: Arc<Mutex<Vec<String>>>,
}

impl FakeFleetd {
    fn start(
        hellos: Vec<HelloOk>,
        on_restart: Option<response::Kind>,
        wait_before_serving_again: Duration,
    ) -> FakeFleetd {
        Self::start_answering_after(
            hellos,
            on_restart,
            wait_before_serving_again,
            Duration::ZERO,
        )
    }

    /// [`FakeFleetd::start`], answering `RestartDaemon` only after
    /// `answer_delay` (as a daemon checking and freezing does).
    fn start_answering_after(
        hellos: Vec<HelloOk>,
        on_restart: Option<response::Kind>,
        wait_before_serving_again: Duration,
        answer_delay: Duration,
    ) -> FakeFleetd {
        use bytes::BytesMut;
        use fleet_proto::FrameCodec;
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        use tokio_util::codec::{Decoder, Encoder};

        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("fleetd.sock")).unwrap();
        let restarts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&restarts);
        let binaries = Arc::new(Mutex::new(Vec::new()));
        let asked_for = Arc::clone(&binaries);
        std::thread::spawn(move || {
            for (served, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { return };
                if served > 0 && wait_before_serving_again > Duration::ZERO {
                    // Only the first reconnect after the restart waits.
                    if served == 1 {
                        std::thread::sleep(wait_before_serving_again);
                    }
                }
                let hello = hellos[served.min(hellos.len() - 1)].clone();
                let on_restart = on_restart.clone();
                let counter = Arc::clone(&counter);
                let asked_for = Arc::clone(&asked_for);
                std::thread::spawn(move || {
                    let mut buf = BytesMut::new();
                    let mut chunk = [0u8; 4096];
                    let mut greeted = false;
                    let send = |stream: &mut std::os::unix::net::UnixStream, f: Frame| {
                        let mut out = BytesMut::new();
                        FrameCodec::new().encode(f, &mut out).unwrap();
                        stream.write_all(&out).is_ok()
                    };
                    loop {
                        while let Some(frame) = FrameCodec::new().decode(&mut buf).unwrap() {
                            match frame {
                                Frame::Hello(_) if !greeted => {
                                    greeted = true;
                                    if !send(&mut stream, Frame::HelloOk(hello.clone())) {
                                        return;
                                    }
                                }
                                Frame::Request(r) => {
                                    let kind = match r.kind {
                                        Some(request::Kind::RestartDaemon(rd)) => {
                                            asked_for.lock().unwrap().push(rd.binary);
                                            counter
                                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                            let kind = on_restart.clone().unwrap_or_else(|| {
                                                response::Kind::Error(fleet_proto::Error {
                                                    message: "empty request".into(),
                                                })
                                            });
                                            let restarting =
                                                matches!(kind, response::Kind::RestartingDaemon(_));
                                            std::thread::sleep(answer_delay);
                                            send(
                                                &mut stream,
                                                Frame::Response(Response {
                                                    id: r.id,
                                                    kind: Some(kind),
                                                }),
                                            );
                                            if restarting {
                                                // exec: the connection ends, after
                                                // a moment in which a pipelined
                                                // second request is still counted.
                                                stream
                                                    .set_read_timeout(Some(Duration::from_millis(
                                                        150,
                                                    )))
                                                    .unwrap();
                                            }
                                            continue;
                                        }
                                        Some(request::Kind::List(_)) => {
                                            response::Kind::Agents(AgentList::default())
                                        }
                                        _ => continue,
                                    };
                                    send(
                                        &mut stream,
                                        Frame::Response(Response {
                                            id: r.id,
                                            kind: Some(kind),
                                        }),
                                    );
                                }
                                _ => {}
                            }
                        }
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                });
            }
        });
        FakeFleetd {
            dir,
            restarts,
            binaries,
        }
    }
}

fn hello(instance: u64, restore: Option<RestoreSummary>) -> HelloOk {
    HelloOk {
        instance,
        restore: restore.map(Box::new),
        ..Default::default()
    }
}

fn start_session_in(dir: &Path) -> (Session, Events) {
    let (tx, rx) = async_channel::unbounded();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    std::thread::spawn(move || {
        while let Ok(e) = rx.recv_blocking() {
            if !matches!(e, UiEvent::PaneChanged(_)) {
                sink.lock().unwrap().push(e);
            }
        }
    });
    (
        Session::start(Target::Local, dir.to_owned(), tx),
        Events(events),
    )
}

fn connected(events: &Events) {
    wait_until("the control connection", || {
        events.has(|e| *e == UiEvent::Connection(ConnState::Connected))
    });
}

#[test]
fn restart_reports_kept_agents_after_reconnect() {
    let kept = RestoreSummary {
        previous_instance: 5,
        kept: vec![1, 2],
        ..Default::default()
    };
    let fake = FakeFleetd::start(
        vec![hello(5, None), hello(6, Some(kept))],
        Some(response::Kind::RestartingDaemon(RestartingDaemon {
            agents: 2,
        })),
        // The daemon is away for a moment, as during an exec.
        Duration::from_millis(500),
    );
    let (s, events) = start_session_in(fake.dir.path());
    connected(&events);
    s.restart_daemon();
    wait_until("the outcome", || {
        events.has(|e| matches!(e, UiEvent::DaemonRestarted(_)))
    });
    assert!(events.has(|e| {
        *e == UiEvent::DaemonRestarted(fleet_client::RestartOutcome::Restarted { kept: 2 })
    }));
    assert!(!events.has(|e| matches!(e, UiEvent::DaemonRestartFailed(_))));
    s.shutdown();
}

#[test]
fn restart_with_same_instance_reports_failure() {
    let thawed = RestoreSummary {
        restart_error: "exec failed".into(),
        ..Default::default()
    };
    let fake = FakeFleetd::start(
        vec![hello(5, None), hello(5, Some(thawed))],
        Some(response::Kind::RestartingDaemon(RestartingDaemon {
            agents: 2,
        })),
        Duration::ZERO,
    );
    let (s, events) = start_session_in(fake.dir.path());
    connected(&events);
    s.restart_daemon();
    wait_until("the outcome", || {
        events.has(|e| matches!(e, UiEvent::DaemonRestartFailed(_)))
    });
    assert!(events.has(|e| *e == UiEvent::DaemonRestartFailed("exec failed".into())));
    assert!(!events.has(|e| matches!(e, UiEvent::DaemonRestarted(_))));
    s.shutdown();
}

#[test]
fn restart_of_an_old_daemon_reports_it_is_too_old() {
    let fake = FakeFleetd::start(vec![hello(5, None)], None, Duration::ZERO);
    let (s, events) = start_session_in(fake.dir.path());
    connected(&events);
    s.restart_daemon();
    wait_until("the outcome", || {
        events.has(|e| matches!(e, UiEvent::DaemonRestartFailed(_)))
    });
    assert!(events.has(|e| *e == UiEvent::DaemonRestartFailed(fleet_client::TOO_OLD.into())));
    s.shutdown();
}

#[test]
fn restart_wait_runs_from_the_answer_not_the_click() {
    let kept = RestoreSummary {
        previous_instance: 5,
        kept: vec![1, 2],
        ..Default::default()
    };
    // The answer takes longer than the wait for the new daemon: that wait
    // has not started yet.
    let fake = FakeFleetd::start_answering_after(
        vec![hello(5, None), hello(6, Some(kept))],
        Some(response::Kind::RestartingDaemon(RestartingDaemon {
            agents: 2,
        })),
        Duration::ZERO,
        Duration::from_millis(900),
    );
    let (s, events) = start_session_in(fake.dir.path());
    s.set_restart_wait_for_test(Duration::from_millis(400));
    connected(&events);
    s.restart_daemon();
    wait_until("the outcome", || {
        events.has(|e| matches!(e, UiEvent::DaemonRestarted(_)))
    });
    assert!(events.has(|e| {
        *e == UiEvent::DaemonRestarted(fleet_client::RestartOutcome::Restarted { kept: 2 })
    }));
    assert!(!events.has(|e| matches!(e, UiEvent::DaemonRestartFailed(_))));
    s.shutdown();
}

#[test]
fn restart_never_answered_times_out_after_the_answer_wait() {
    let fake = FakeFleetd::start_answering_after(
        vec![hello(5, None)],
        Some(response::Kind::RestartingDaemon(RestartingDaemon {
            agents: 2,
        })),
        Duration::ZERO,
        Duration::from_secs(5),
    );
    let (s, events) = start_session_in(fake.dir.path());
    s.set_restart_answer_wait_for_test(Duration::from_millis(300));
    connected(&events);
    let asked = Instant::now();
    s.restart_daemon();
    wait_until("the timeout", || {
        events.has(|e| matches!(e, UiEvent::DaemonRestartFailed(_)))
    });
    // Well before the answer (5 s) or the wait for the new daemon (10 s).
    assert!(asked.elapsed() < Duration::from_secs(3));
    assert!(events.has(|e| {
        *e == UiEvent::DaemonRestartFailed("fleetd did not come back within 10 s".into())
    }));
    s.shutdown();
}

#[test]
fn restart_that_never_settles_times_out_once_and_a_second_ask_is_ignored() {
    // The daemon acknowledges the restart but the "new" one is still the
    // old instance with no `restart_error`: never decided. The wait runs
    // from that acknowledgement.
    let fake = FakeFleetd::start(
        vec![hello(5, None)],
        Some(response::Kind::RestartingDaemon(RestartingDaemon {
            agents: 2,
        })),
        Duration::ZERO,
    );
    let (s, events) = start_session_in(fake.dir.path());
    s.set_restart_wait_for_test(Duration::from_millis(600));
    connected(&events);
    s.restart_daemon();
    // Asked again while pending: ignored (no second timer, no second event).
    s.restart_daemon();
    wait_until("the timeout", || {
        events.has(|e| matches!(e, UiEvent::DaemonRestartFailed(_)))
    });
    std::thread::sleep(Duration::from_millis(900));
    let failures: Vec<_> = events
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, UiEvent::DaemonRestartFailed(_)))
        .cloned()
        .collect();
    assert_eq!(
        failures,
        vec![UiEvent::DaemonRestartFailed(
            "fleetd did not come back within 10 s".into()
        )]
    );
    assert!(!events.has(|e| matches!(e, UiEvent::DaemonRestarted(_))));
    // The second ask never reached the daemon.
    assert_eq!(fake.restarts.load(std::sync::atomic::Ordering::SeqCst), 1);
    s.shutdown();
}

/// The identity `fleetd` at `path` reports in its handshake.
fn identity_of(path: &Path) -> fleet_proto::BinaryIdentity {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).unwrap();
    fleet_proto::BinaryIdentity {
        path: path.to_string_lossy().into_owned(),
        dev: m.dev(),
        ino: m.ino(),
        size: m.size(),
        mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
    }
}

fn hello_running(instance: u64, restore: Option<RestoreSummary>, binary: &Path) -> HelloOk {
    HelloOk {
        binary: Some(Box::new(identity_of(binary))),
        ..hello(instance, restore)
    }
}

/// A `fleetd` on disk (a stand-in file) and a copy of it as it was: the
/// running daemon reports the copy, so the one on disk is a newer build.
fn builds() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("fleetd-old");
    std::fs::write(&old, "old build").unwrap();
    let new = dir.path().join("fleetd");
    std::fs::write(&new, "a newer build").unwrap();
    (dir, old, new)
}

#[test]
fn update_is_offered_only_when_the_binary_on_disk_is_newer() {
    let (_bins, old, new) = builds();
    // The daemon runs the build on disk: no update.
    let same = FakeFleetd::start(vec![hello_running(5, None, &new)], None, Duration::ZERO);
    let (s, events) = start_session_in(same.dir.path());
    s.set_fleetd_for_test(new.clone());
    connected(&events);
    assert!(!s.update_available());
    s.shutdown();

    // It runs an older one: an update is offered.
    let older = FakeFleetd::start(vec![hello_running(5, None, &old)], None, Duration::ZERO);
    let (s, events) = start_session_in(older.dir.path());
    s.set_fleetd_for_test(new.clone());
    connected(&events);
    assert!(s.update_available());
    s.shutdown();

    // An older fleetd that does not report its binary: never.
    let unknown = FakeFleetd::start(vec![hello(5, None)], None, Duration::ZERO);
    let (s, events) = start_session_in(unknown.dir.path());
    s.set_fleetd_for_test(new);
    connected(&events);
    assert!(!s.update_available());
    s.shutdown();
}

#[test]
fn update_double_clicked_sends_one_restart_and_clears_once_restarted() {
    let (_bins, old, new) = builds();
    let kept = RestoreSummary {
        previous_instance: 5,
        kept: vec![1],
        ..Default::default()
    };
    let fake = FakeFleetd::start(
        vec![
            hello_running(5, None, &old),
            hello_running(6, Some(kept), &new),
        ],
        Some(response::Kind::RestartingDaemon(RestartingDaemon {
            agents: 1,
        })),
        Duration::from_millis(300),
    );
    let (s, events) = start_session_in(fake.dir.path());
    s.set_fleetd_for_test(new.clone());
    connected(&events);
    assert!(s.update_available());
    assert!(!s.restart_pending());

    // A double click: two asks back to back.
    s.restart_daemon();
    assert!(s.restart_pending());
    s.restart_daemon();
    wait_until("the outcome", || {
        events.has(|e| matches!(e, UiEvent::DaemonRestarted(_)))
    });
    assert!(events.has(|e| {
        *e == UiEvent::DaemonRestarted(fleet_client::RestartOutcome::Restarted { kept: 1 })
    }));
    assert!(!s.restart_pending());
    // The new daemon runs the build on disk: the button goes away.
    assert!(!s.update_available());
    // One request, onto the fleetd the update check compared against.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(fake.restarts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        *fake.binaries.lock().unwrap(),
        vec![new.to_string_lossy().into_owned()]
    );
    s.shutdown();
}

#[test]
fn update_of_a_daemon_too_old_to_restart_says_how_to_stop_it() {
    let (_bins, old, new) = builds();
    let fake = FakeFleetd::start(vec![hello_running(5, None, &old)], None, Duration::ZERO);
    let (s, events) = start_session_in(fake.dir.path());
    s.set_fleetd_for_test(new);
    connected(&events);
    s.restart_daemon();
    wait_until("the outcome", || {
        events.has(|e| matches!(e, UiEvent::DaemonRestartFailed(_)))
    });
    assert!(!s.restart_pending());
    let text = fleet_gui::core::session::restart_failure_text(fleet_client::TOO_OLD);
    assert!(text.contains("--force-stop"), "{text}");
    assert!(events.has(|e| *e == UiEvent::DaemonRestartFailed(fleet_client::TOO_OLD.into())));
    // Still out of date: the button stays.
    assert!(s.update_available());
    s.shutdown();
}
