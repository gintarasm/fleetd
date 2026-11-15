//! `RestartDaemon` end to end: a real `fleetd` binary on a temp state dir
//! restarts in its own process (same pid) onto a binary path, with its
//! agents carried over; refusals and failures leave it serving.
//!
//! The tests' daemon is a debug build, so its test hooks
//! (`FLEETD_TEST_RESTART_DELAY_MS`, `FLEETD_TEST_CORRUPT_HANDOFF`,
//! `FLEETD_TEST_SHORT_HANDOFF`, `FLEETD_TEST_PANIC_AFTER_FREEZE`,
//! `FLEETD_TEST_PANIC_DESCRIBING`, `FLEETD_TEST_PANIC_OPENING_HANDOFF`)
//! work.

use std::collections::VecDeque;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use fleet_node::Terminal;
use fleet_node::scrollback::Scrollback;
use fleet_proto::{
    AgentInfo, AgentState, Attach, AttachOk, Detach, Frame, FrameCodec, Hello, HelloOk, ListAgents,
    PROTOCOL_VERSION, ReadHistory, Request, RestartDaemon, SendInput, SpawnAgent, request,
    response,
};
use tempfile::TempDir;
use tokio_util::codec::{Decoder, Encoder};

const TIMEOUT: Duration = Duration::from_secs(10);
/// How long a client waits for the restarted daemon (the CLI's bound).
const RECONNECT: Duration = Duration::from_secs(10);

struct Daemon {
    child: Child,
    dir: TempDir,
}

impl Daemon {
    fn start() -> Self {
        Self::start_with(&[])
    }

    fn start_with(env: &[(&str, &str)]) -> Self {
        Self::start_from(fleetd(), env)
    }

    /// Runs the daemon from `binary`.
    fn start_from(binary: &Path, env: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let child = Command::new(binary)
            // Never the developer's real login shell.
            .env("FLEETD_LOGIN_ENV", "off")
            .envs(env.iter().copied())
            .arg("--state-dir")
            .arg(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fleetd");
        let sock = dir.path().join("fleetd.sock");
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&sock).is_err() {
            assert!(Instant::now() < deadline, "daemon not ready within 5s");
            std::thread::sleep(Duration::from_millis(5));
        }
        Daemon { child, dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn connect(&self) -> (Client, HelloOk) {
        Client::open(self.path()).expect("connect")
    }

    /// Connects to whichever daemon serves the socket once one other than
    /// `old` (an instance token) answers, retrying for [`RECONNECT`].
    fn reconnect_new(&self, old: u64) -> (Client, HelloOk) {
        self.reconnect_until(|h| h.instance != old)
    }

    fn reconnect_until(&self, done: impl Fn(&HelloOk) -> bool) -> (Client, HelloOk) {
        let deadline = Instant::now() + RECONNECT;
        loop {
            if let Some((c, hello)) = Client::open(self.path())
                && done(&hello)
            {
                return (c, hello);
            }
            assert!(Instant::now() < deadline, "no daemon came back");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The process the test started is still running (a restart keeps it).
    fn assert_same_process(&mut self) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "the daemon's process exited"
        );
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // A reaped child's pid may be someone else's by now.
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        // SIGTERM, so the daemon ends its agents.
        // SAFETY: plain signal to our child, not yet reaped.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    stream: UnixStream,
    buf: BytesMut,
    /// Frames other than responses, output and activity read while waiting.
    pending: VecDeque<Frame>,
    next_id: u64,
}

impl Client {
    /// Connects and completes the handshake; `None` if the daemon is not
    /// serving (or closes during the handshake).
    fn open(dir: &Path) -> Option<(Self, HelloOk)> {
        let stream = UnixStream::connect(dir.join("fleetd.sock")).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut c = Client {
            stream,
            buf: BytesMut::new(),
            pending: VecDeque::new(),
            next_id: 1,
        };
        c.try_send(Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client: "restart-test".into(),
        }))
        .ok()?;
        match c.read_frame() {
            Some(Frame::HelloOk(ok)) => Some((c, ok)),
            _ => None,
        }
    }

    fn try_send(&mut self, frame: Frame) -> std::io::Result<()> {
        let mut out = BytesMut::new();
        FrameCodec::default().encode(frame, &mut out).unwrap();
        self.stream.write_all(&out)
    }

    /// Sends a request without waiting; its id.
    fn send(&mut self, kind: request::Kind) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.try_send(Frame::Request(Request {
            id,
            kind: Some(kind),
        }))
        .expect("send");
        id
    }

    /// Next frame from the wire; `None` on EOF. Panics on timeout.
    fn read_frame(&mut self) -> Option<Frame> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(frame) = FrameCodec::default().decode(&mut self.buf).unwrap() {
                return Some(frame);
            }
            assert!(Instant::now() < deadline, "no frame within {TIMEOUT:?}");
            let mut chunk = [0u8; 64 * 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::ConnectionReset => return None,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) => panic!("read: {e}"),
            }
        }
    }

    /// The response to request `id`; `None` if the connection closes first.
    fn response(&mut self, id: u64) -> Option<response::Kind> {
        loop {
            match self.read_frame()? {
                Frame::Response(fleet_proto::Response { id: rid, kind }) if rid == id => {
                    return Some(kind.expect("response kind"));
                }
                Frame::Output { .. } => {}
                Frame::Event(fleet_proto::Event {
                    kind: Some(fleet_proto::event::Kind::Activity(_)),
                }) => {}
                other => self.pending.push_back(other),
            }
        }
    }

    fn request(&mut self, kind: request::Kind) -> response::Kind {
        let id = self.send(kind);
        self.response(id)
            .unwrap_or_else(|| panic!("connection closed awaiting response {id}"))
    }

    fn spawn(&mut self, program: &str, args: &[&str]) -> AgentInfo {
        match self.request(request::Kind::Spawn(Box::new(SpawnAgent {
            program: program.into(),
            args: args.iter().map(|&a| a.into()).collect(),
            cols: 80,
            rows: 24,
            ..Default::default()
        }))) {
            response::Kind::Spawned(info) => *info,
            other => panic!("spawn failed: {other:?}"),
        }
    }

    fn sh(&mut self, script: &str) -> u64 {
        self.spawn("/bin/sh", &["-c", script]).id
    }

    fn list(&mut self) -> Vec<AgentInfo> {
        match self.request(request::Kind::List(ListAgents::default())) {
            response::Kind::Agents(list) => list.agents,
            other => panic!("list failed: {other:?}"),
        }
    }

    fn agent(&mut self, id: u64) -> AgentInfo {
        self.list().into_iter().find(|a| a.id == id).unwrap()
    }

    fn input(&mut self, agent_id: u64, data: &[u8]) {
        match self.request(request::Kind::Input(SendInput {
            agent_id,
            data: data.to_vec(),
        })) {
            response::Kind::Ack(_) => {}
            other => panic!("input failed: {other:?}"),
        }
    }

    /// Observes the agent for its attach reply, then detaches.
    fn attach_ok(&mut self, agent_id: u64) -> AttachOk {
        let ok = match self.request(request::Kind::Attach(Attach {
            agent_id,
            observe: true,
        })) {
            response::Kind::Attached(ok) => *ok,
            other => panic!("attach failed: {other:?}"),
        };
        self.request(request::Kind::Detach(Detach { agent_id }));
        ok
    }

    fn screen(&mut self, agent_id: u64) -> Vec<String> {
        let ok = self.attach_ok(agent_id);
        let mut term = Terminal::new(ok.cols as u16, ok.rows as u16);
        term.feed(&ok.snapshot);
        term.visible_text()
    }

    /// Polls the agent's screen until a row satisfies `shown`.
    fn wait_screen(&mut self, id: u64, what: &str, shown: impl Fn(&str) -> bool) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let text = self.screen(id);
            if text.iter().any(|l| shown(l)) {
                return text;
            }
            assert!(Instant::now() < deadline, "{what} never shown: {text:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_exit_code(&mut self, id: u64) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let info = self.agent(id);
            if info.state == AgentState::Exited as i32 {
                return info.exit_code.expect("exit code");
            }
            assert!(Instant::now() < deadline, "agent {id} never exited");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Every history line of the agent, oldest first.
    fn history(&mut self, id: u64) -> Vec<String> {
        let ok = self.attach_ok(id);
        let mut pages = Vec::new();
        let mut before = u64::MAX;
        loop {
            let page = match self.request(request::Kind::ReadHistory(ReadHistory {
                agent_id: id,
                generation: ok.history_generation,
                before_line: before,
                max_lines: 4096,
            })) {
                response::Kind::History(page) => page,
                other => panic!("read history failed: {other:?}"),
            };
            if page.lines.is_empty() {
                break;
            }
            before = page.first_line;
            pages.push(page.lines);
        }
        pages
            .into_iter()
            .rev()
            .flatten()
            .map(|l| String::from_utf8(l.to_vec()).unwrap())
            .collect()
    }

    fn restart_daemon(&mut self, binary: &Path) -> u64 {
        self.send(request::Kind::RestartDaemon(RestartDaemon {
            binary: binary.to_string_lossy().into_owned(),
        }))
    }

    /// Asks for a restart naming no binary: onto the daemon's own path.
    fn restart_daemon_onto_itself(&mut self) -> u64 {
        self.send(request::Kind::RestartDaemon(RestartDaemon::default()))
    }
}

fn fleetd() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_fleetd"))
}

fn restarting_agents(r: Option<response::Kind>) -> u32 {
    match r {
        Some(response::Kind::RestartingDaemon(r)) => r.agents,
        other => panic!("expected RestartingDaemon, got {other:?}"),
    }
}

fn error_message(r: Option<response::Kind>) -> String {
    match r {
        Some(response::Kind::Error(e)) => e.message,
        other => panic!("expected an Error, got {other:?}"),
    }
}

/// Prints `1..=n`, one per line, in batches of 100 with a pause between,
/// then exits.
fn numbered_writer(n: u32) -> String {
    format!(
        "i=0; while [ $i -lt {n} ]; do i=$((i+1)); echo $i; \
         if [ $((i % 100)) -eq 0 ]; then sleep 0.02; fi; done"
    )
}

/// A row showing a number of at least 200: a few batches are out.
fn past_200(line: &str) -> bool {
    line.trim().parse::<u32>().is_ok_and(|n| n >= 200)
}

/// History lines as numbers (a PTY ends lines with `\r`).
fn numbers(lines: &[String]) -> Vec<u32> {
    lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.parse().unwrap_or_else(|_| panic!("not a number: {l:?}")))
        .collect()
}

/// The writer's history holds every number once, in order, up to the last
/// screen (whose rows have not scrolled off).
fn assert_no_gap(c: &mut Client, id: u64, n: u32) {
    let got = numbers(&c.history(id));
    let last = *got.last().expect("history is empty");
    let want: Vec<u32> = (1..=last).collect();
    if got != want {
        let gap = got.windows(2).find(|w| w[1] != w[0] + 1);
        panic!(
            "history: {} numbers, first {:?}, last {last}, first break {gap:?}",
            got.len(),
            got.first()
        );
    }
    assert!(last >= n - 24, "history ends early at {last}");
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 only checks the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The process `pid` no longer exists (not even as a zombie).
fn assert_gone(pid: i32) {
    assert!(!pid_alive(pid), "agent {pid} still exists");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

/// Runs `binary --handoff-check` once (see its callers).
fn warm_up(binary: &Path) {
    let out = Command::new(binary)
        .arg("--handoff-check")
        .output()
        .unwrap();
    assert!(out.status.success());
}

/// The agent's state as the store on disk has it.
fn stored_state(dir: &Path, id: u64) -> fleet_node::store::AgentState {
    let store = fleet_node::store::Store::open(&dir.join("state.db")).unwrap();
    store.agent(id).unwrap().expect("agent row").state
}

/// The pid an agent printed as `pid=N`.
fn shown_pid(c: &mut Client, id: u64) -> i32 {
    let text = c.wait_screen(id, "pid", |l| l.starts_with("pid="));
    let line = text.iter().find(|l| l.starts_with("pid=")).unwrap();
    line["pid=".len()..].trim().parse().unwrap()
}

const SHELL: &str = "echo ready; while read l; do eval \"$l\"; done";

#[test]
fn restart_keeps_agents_running() {
    const N: u32 = 20_000;
    let mut daemon = Daemon::start();
    let (mut c, hello) = daemon.connect();
    let shell = c.sh(SHELL);
    let writer = c.sh(&numbered_writer(N));
    let before = c.wait_screen(shell, "ready", |l| l == "ready");
    c.wait_screen(writer, "output", past_200);

    let id = c.restart_daemon(fleetd());
    assert_eq!(restarting_agents(c.response(id)), 2);

    let (mut c, after) = daemon.reconnect_new(hello.instance);
    daemon.assert_same_process();
    let restore = after.restore.as_deref().expect("restore summary");
    assert_eq!(restore.previous_instance, hello.instance);
    let mut kept = restore.kept.clone();
    kept.sort_unstable();
    assert_eq!(kept, vec![shell, writer]);
    assert!(restore.failed.is_empty() && !restore.fell_back);
    assert!(restore.restart_error.is_empty());

    assert_eq!(c.screen(shell), before);
    c.input(shell, b"echo after\n");
    c.wait_screen(shell, "after", |l| l == "after");
    assert_eq!(c.wait_exit_code(writer), 0);
    assert_no_gap(&mut c, writer, N);
}

#[test]
fn agent_exiting_during_restart_is_recorded() {
    let daemon = Daemon::start();
    let (mut c, hello) = daemon.connect();
    let id = c.sh("sleep 0.3; exit 5");
    let req = c.restart_daemon(fleetd());
    restarting_agents(c.response(req));
    let (mut c, _) = daemon.reconnect_new(hello.instance);
    assert_eq!(c.wait_exit_code(id), 5);
}

#[test]
fn failed_check_refuses_and_keeps_serving() {
    let mut daemon = Daemon::start();
    let bin = script(daemon.path(), "bad", "exit 3");
    let (mut c, hello) = daemon.connect();
    let shell = c.sh(SHELL);
    c.wait_screen(shell, "ready", |l| l == "ready");

    let id = c.restart_daemon(&bin);
    let e = error_message(c.response(id));
    assert!(e.contains("check failed (exit 3)"), "{e}");
    // The same connection is served on, by the same daemon.
    assert_eq!(c.agent(shell).state, AgentState::Running as i32);
    c.input(shell, b"echo still\n");
    c.wait_screen(shell, "still", |l| l == "still");
    let (_, again) = daemon.connect();
    assert_eq!(again.instance, hello.instance);
    let restore = again.restore.as_deref().expect("restore summary");
    assert!(restore.restart_error.contains("check failed (exit 3)"));
    daemon.assert_same_process();
}

#[test]
fn exec_failure_thaws() {
    // Writes for about 4 s, past the restart (1.5 s delay, then the exec
    // fails): still running when it is looked at, also under load.
    const N: u32 = 20_000;
    let mut daemon = Daemon::start_with(&[("FLEETD_TEST_RESTART_DELAY_MS", "1500")]);
    let checked = daemon.path().join("checked");
    let bin = script(
        daemon.path(),
        "next",
        &format!("echo 'handoff 1'; touch '{}'", checked.display()),
    );
    let (mut c, hello) = daemon.connect();
    let writer = c.sh(&numbered_writer(N));
    c.wait_screen(writer, "output", past_200);

    let id = c.restart_daemon(&bin);
    // Once the check has passed, the daemon waits before freezing: replace
    // the binary with a file that cannot be executed.
    let deadline = Instant::now() + TIMEOUT;
    while !checked.exists() {
        assert!(Instant::now() < deadline, "the check never ran");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(200));
    fs::remove_file(&bin).unwrap();
    fs::write(&bin, b"not a program").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).unwrap();
    // Answered just before the exec, which then fails.
    restarting_agents(c.response(id));

    let (mut c, again) = daemon.reconnect_until(|h| {
        h.restore
            .as_ref()
            .is_some_and(|r| !r.restart_error.is_empty())
    });
    assert_eq!(again.instance, hello.instance);
    daemon.assert_same_process();
    assert_eq!(c.agent(writer).state, AgentState::Running as i32);
    assert_eq!(c.wait_exit_code(writer), 0);
    assert_no_gap(&mut c, writer, N);
    // Serving as usual: a new agent starts.
    let next = c.sh("exit 0");
    assert_eq!(c.wait_exit_code(next), 0);
}

#[test]
fn second_restart_is_refused() {
    let daemon = Daemon::start_with(&[("FLEETD_TEST_RESTART_DELAY_MS", "300")]);
    let (mut a, hello) = daemon.connect();
    let (mut b, _) = daemon.connect();
    let ra = a.restart_daemon(fleetd());
    let rb = b.restart_daemon(fleetd());
    let (ra, rb) = (a.response(ra), b.response(rb));
    let (won, lost) = match (&ra, &rb) {
        (Some(response::Kind::RestartingDaemon(_)), _) => (ra, rb),
        _ => (rb, ra),
    };
    restarting_agents(won);
    assert_eq!(error_message(lost), "a restart is already in progress");
    daemon.reconnect_new(hello.instance);
}

#[test]
fn stuck_input_does_not_block_restart() {
    let daemon = Daemon::start();
    let (mut c, hello) = daemon.connect();
    let sleeper = c.sh("exec sleep 30");
    let (mut typist, _) = daemon.connect();
    // 4 MiB of lines the agent never reads, pipelined: the first fill the
    // terminal's input queue, the rest wait behind them. Sent from a thread
    // of their own, as the socket fills too.
    const LINES: u64 = 4096;
    let mut out = typist.stream.try_clone().unwrap();
    let sending = std::thread::spawn(move || {
        let line = format!("{}\n", "x".repeat(1023));
        for id in 1..=LINES {
            let mut buf = BytesMut::new();
            let frame = Frame::Request(Request {
                id,
                kind: Some(request::Kind::Input(SendInput {
                    agent_id: sleeper,
                    data: line.clone().into(),
                })),
            });
            FrameCodec::default().encode(frame, &mut buf).unwrap();
            // The restart closes the connection.
            if out.write_all(&buf).is_err() {
                return;
            }
        }
    });
    let typing = std::thread::spawn(move || {
        for id in 1..=LINES {
            match typist.response(id) {
                Some(response::Kind::Ack(_)) => {}
                Some(response::Kind::Error(e)) => return Some(e.message),
                Some(other) => panic!("unexpected {other:?}"),
                None => return None,
            }
        }
        panic!("every input was accepted by an agent that never reads");
    });
    std::thread::sleep(Duration::from_millis(300));

    let started = Instant::now();
    let id = c.restart_daemon(fleetd());
    assert_eq!(restarting_agents(c.response(id)), 1);
    assert!(started.elapsed() < Duration::from_secs(4));
    if let Some(e) = typing.join().unwrap() {
        assert_eq!(e, "fleetd is restarting");
    }
    sending.join().unwrap();
    let (_, after) = daemon.reconnect_new(hello.instance);
    assert_eq!(after.restore.as_deref().unwrap().kept, vec![sleeper]);
}

#[test]
fn handoff_fd_does_not_leak_to_agents() {
    let daemon = Daemon::start();
    let (mut c, hello) = daemon.connect();
    let id = c.restart_daemon(fleetd());
    restarting_agents(c.response(id));
    let (mut c, _) = daemon.reconnect_new(hello.instance);
    let env = c.spawn("/bin/sh", &["-c", "env"]).id;
    assert_eq!(c.wait_exit_code(env), 0);
    let ring =
        Scrollback::open(&daemon.path().join("scrollback").join(format!("{env}.ring"))).unwrap();
    let out = String::from_utf8(ring.read_tail(usize::MAX).unwrap()).unwrap();
    assert!(out.contains("PATH="), "{out}");
    assert!(!out.contains("FLEETD_HANDOFF_FD"), "{out}");
    assert!(!out.contains("FLEETD_HANDOFF_PIDS"), "{out}");
}

#[test]
fn corrupt_record_falls_back() {
    let mut daemon = Daemon::start_with(&[("FLEETD_TEST_CORRUPT_HANDOFF", "1")]);
    let (mut c, hello) = daemon.connect();
    let ids = [
        c.sh("echo pid=$$; exec sleep 100"),
        c.sh("echo pid=$$; exec sleep 100"),
    ];
    let pids = ids.map(|id| shown_pid(&mut c, id));
    let req = c.restart_daemon(fleetd());
    assert_eq!(restarting_agents(c.response(req)), 2);

    let (mut c, after) = daemon.reconnect_new(hello.instance);
    daemon.assert_same_process();
    let restore = after.restore.as_deref().expect("restore summary");
    assert!(restore.fell_back);
    assert!(restore.kept.is_empty());
    for id in ids {
        assert_eq!(c.agent(id).state, AgentState::Lost as i32);
    }
    for pid in pids {
        assert_gone(pid);
    }
    // The fallback daemon serves as any fresh one.
    let next = c.sh("exit 0");
    assert_eq!(c.wait_exit_code(next), 0);
}

#[test]
fn panic_after_freeze_thaws_and_keeps_serving() {
    panic_in_restart_thaws_and_keeps_serving("FLEETD_TEST_PANIC_AFTER_FREEZE");
}

#[test]
fn panic_while_freezing_thaws_and_keeps_serving() {
    // Inside the node's freeze, while it saves each agent's screen.
    panic_in_restart_thaws_and_keeps_serving("FLEETD_TEST_PANIC_DESCRIBING");
}

fn panic_in_restart_thaws_and_keeps_serving(hook: &str) {
    const N: u32 = 8_000;
    let mut daemon = Daemon::start_with(&[(hook, "1")]);
    let (mut c, hello) = daemon.connect();
    let shell = c.sh(SHELL);
    let writer = c.sh(&numbered_writer(N));
    c.wait_screen(shell, "ready", |l| l == "ready");
    c.wait_screen(writer, "output", past_200);

    // The restart panics with the agents frozen: answered with an Error,
    // everything undone.
    let id = c.restart_daemon(fleetd());
    let e = error_message(c.response(id));
    assert!(e.contains("internal error"), "{e}");
    daemon.assert_same_process();
    assert_eq!(c.agent(shell).state, AgentState::Running as i32);
    c.input(shell, b"echo thawed\n");
    c.wait_screen(shell, "thawed", |l| l == "thawed");
    assert_eq!(c.wait_exit_code(writer), 0);
    assert_no_gap(&mut c, writer, N);
    // Spawns are admitted again (the handoff gate is open).
    let next = c.sh("exit 0");
    assert_eq!(c.wait_exit_code(next), 0);
    let (_, again) = daemon.connect();
    assert_eq!(again.instance, hello.instance);
    let restore = again.restore.as_deref().expect("restore summary");
    assert!(
        restore.restart_error.contains("internal error"),
        "{restore:?}"
    );
}

#[test]
fn agent_left_out_of_the_record_is_ended() {
    let mut daemon = Daemon::start_with(&[("FLEETD_TEST_SHORT_HANDOFF", "1")]);
    let (mut c, hello) = daemon.connect();
    let ids = [
        c.sh("echo pid=$$; exec sleep 100"),
        c.sh("echo pid=$$; exec sleep 100"),
    ];
    let pids = ids.map(|id| shown_pid(&mut c, id));
    let req = c.restart_daemon(fleetd());
    assert_eq!(restarting_agents(c.response(req)), 2);

    // The record lists one agent; both pids were passed.
    let (mut c, after) = daemon.reconnect_new(hello.instance);
    daemon.assert_same_process();
    let restore = after.restore.as_deref().expect("restore summary");
    assert_eq!(restore.kept.len(), 1, "{restore:?}");
    let kept = ids.iter().position(|&id| id == restore.kept[0]).unwrap();
    let (left_out, left_out_pid) = (ids[1 - kept], pids[1 - kept]);
    assert_gone(left_out_pid);
    assert_eq!(c.agent(left_out).state, AgentState::Lost as i32);
    assert!(pid_alive(pids[kept]));
    assert_eq!(c.agent(ids[kept]).state, AgentState::Running as i32);
}

#[test]
fn panic_opening_the_handoff_ends_every_agent() {
    let mut daemon = Daemon::start_with(&[("FLEETD_TEST_PANIC_OPENING_HANDOFF", "1")]);
    let (mut c, _) = daemon.connect();
    let ids = [
        c.sh("echo pid=$$; exec sleep 100"),
        c.sh("echo pid=$$; exec sleep 100"),
    ];
    let pids = ids.map(|id| shown_pid(&mut c, id));
    let req = c.restart_daemon(fleetd());
    assert_eq!(restarting_agents(c.response(req)), 2);

    // The successor panics with the agents adopted: it ends them all, marks
    // any still running in the store lost and exits.
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = daemon.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the daemon never exited");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success());
    for pid in pids {
        assert_gone(pid);
    }
    // Lost, or exited if its reader recorded the exit first: the agents
    // were adopted before the panic.
    for id in ids {
        let state = stored_state(daemon.path(), id);
        assert_ne!(state, fleet_node::store::AgentState::Running, "agent {id}");
    }
}

#[test]
fn no_binary_named_restarts_onto_the_daemons_own_path_rebuilt() {
    use std::os::unix::fs::MetadataExt;
    let bin_dir = tempfile::tempdir().unwrap();
    let own = bin_dir.path().join("fleetd");
    fs::copy(fleetd(), &own).unwrap();
    let daemon = Daemon::start_from(&own, &[]);
    let (mut c, hello) = daemon.connect();
    let shell = c.sh(SHELL);
    let started_from = hello.binary.as_deref().expect("binary identity").ino;
    assert_eq!(started_from, fs::metadata(&own).unwrap().ino());

    // A new build replaces the file at that path (a new inode).
    let next = bin_dir.path().join("fleetd.new");
    fs::copy(fleetd(), &next).unwrap();
    fs::rename(&next, &own).unwrap();
    let rebuilt = fs::metadata(&own).unwrap().ino();
    assert_ne!(rebuilt, started_from);
    // macOS scans a new executable on its first run, which under load can
    // outlast the 5 s check: run it once first.
    warm_up(&own);

    let id = c.restart_daemon_onto_itself();
    assert_eq!(restarting_agents(c.response(id)), 1);
    let (mut c, after) = daemon.reconnect_new(hello.instance);
    assert_eq!(after.binary.as_deref().unwrap().ino, rebuilt);
    assert_eq!(after.restore.as_deref().unwrap().kept, vec![shell]);

    // Its path gone: refused, no fallback to `fleetd` on PATH.
    fs::remove_file(&own).unwrap();
    let id = c.restart_daemon_onto_itself();
    let e = error_message(c.response(id));
    assert_eq!(e, "not an executable file");
}
