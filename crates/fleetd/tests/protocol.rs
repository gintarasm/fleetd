//! Protocol server tests: a real `fleetd` binary on a temp state dir, spoken to
//! over its unix socket with `FrameCodec`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use fleet_proto::{
    AgentExited, AgentInfo, AgentState, Attach, AttachOk, Frame, FrameCodec, Hello, HistoryPage,
    KillAgent, ListAgents, PROTOCOL_VERSION, ReadHistory, Request, Response, SendInput, SetLabels,
    SpawnAgent, event, request, response,
};
use tempfile::TempDir;
use tokio_util::codec::{Decoder, Encoder};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Daemon {
    child: Child,
}

impl Daemon {
    fn start(dir: &Path) -> Self {
        Self::start_with(dir, Command::new(env!("CARGO_BIN_EXE_fleetd")))
    }

    /// Starts fleetd through `sh` after `ulimit <limit>` (e.g. `-n 256`).
    fn start_with_ulimit(dir: &Path, limit: &str) -> Self {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!("ulimit {limit} && exec \"$0\" \"$@\""))
            .arg(env!("CARGO_BIN_EXE_fleetd"));
        Self::start_with(dir, cmd)
    }

    fn start_with(dir: &Path, mut cmd: Command) -> Self {
        let child = cmd
            // Never the developer's real login shell.
            .env("FLEETD_LOGIN_ENV", "off")
            .arg("--state-dir")
            .arg(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fleetd");
        let sock = dir.join("fleetd.sock");
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&sock).is_err() {
            assert!(Instant::now() < deadline, "daemon not ready within 5s");
            std::thread::sleep(Duration::from_millis(5));
        }
        Daemon { child }
    }

    /// Simulates a crash: SIGKILL, then reap.
    fn crash(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    stream: UnixStream,
    buf: BytesMut,
    /// Frames read while waiting for something else.
    pending: VecDeque<Frame>,
    next_id: u64,
}

impl Client {
    fn connect(dir: &Path) -> Self {
        let stream = UnixStream::connect(dir.join("fleetd.sock")).expect("connect");
        // Set once: macOS refuses the option after the peer has closed.
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        Client {
            stream,
            buf: BytesMut::new(),
            pending: VecDeque::new(),
            next_id: 1,
        }
    }

    /// Connects and completes the handshake.
    fn open(dir: &Path) -> Self {
        let mut c = Self::connect(dir);
        c.send(hello(PROTOCOL_VERSION));
        match c.next_frame() {
            Some(Frame::HelloOk(ok)) => {
                assert_eq!(ok.protocol_version, PROTOCOL_VERSION);
                assert_eq!(ok.server, "fleetd");
            }
            other => panic!("expected HelloOk, got {other:?}"),
        }
        c
    }

    fn send(&mut self, frame: Frame) {
        let mut out = BytesMut::new();
        FrameCodec::default().encode(frame, &mut out).unwrap();
        self.stream.write_all(&out).unwrap();
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).unwrap();
    }

    /// Next frame from the wire; `None` on EOF (or reset). Panics on timeout.
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

    /// Next frame, buffered ones first.
    fn next_frame(&mut self) -> Option<Frame> {
        self.pending.pop_front().or_else(|| self.read_frame())
    }

    /// Sends a request and returns its response; other frames are kept.
    fn request(&mut self, kind: request::Kind) -> response::Kind {
        let id = self.next_id;
        self.next_id += 1;
        self.send(Frame::Request(Request {
            id,
            kind: Some(kind),
        }));
        loop {
            match self.read_frame() {
                Some(Frame::Response(Response { id: rid, kind })) if rid == id => {
                    return kind.expect("response kind");
                }
                Some(other) => self.pending.push_back(other),
                None => panic!("connection closed awaiting response {id}"),
            }
        }
    }

    fn spawn(&mut self, program: &str, args: &[&str]) -> AgentInfo {
        match self.request(request::Kind::Spawn(Box::new(SpawnAgent {
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cols: 80,
            rows: 24,
            ..Default::default()
        }))) {
            response::Kind::Spawned(info) => *info,
            other => panic!("spawn failed: {other:?}"),
        }
    }

    fn sh(&mut self, script: &str) -> AgentInfo {
        self.spawn("/bin/sh", &["-c", script])
    }

    fn list(&mut self) -> Vec<AgentInfo> {
        self.list_selected(&[])
    }

    /// Lists the agents whose labels contain every `selector` pair.
    fn list_selected(&mut self, selector: &[(&str, &str)]) -> Vec<AgentInfo> {
        match self.request(request::Kind::List(ListAgents {
            selector: labels(selector),
        })) {
            response::Kind::Agents(list) => list.agents,
            other => panic!("list failed: {other:?}"),
        }
    }

    fn attach(&mut self, agent_id: u64) -> AttachOk {
        self.attach_with(agent_id, false)
    }

    /// A read-only attach (`Attach.observe`).
    fn observe(&mut self, agent_id: u64) -> AttachOk {
        self.attach_with(agent_id, true)
    }

    fn attach_with(&mut self, agent_id: u64, observe: bool) -> AttachOk {
        match self.request(request::Kind::Attach(Attach { agent_id, observe })) {
            response::Kind::Attached(ok) => *ok,
            other => panic!("attach failed: {other:?}"),
        }
    }

    fn input(&mut self, agent_id: u64, data: &[u8]) {
        let r = self.request(request::Kind::Input(SendInput {
            agent_id,
            data: data.to_vec(),
        }));
        assert!(matches!(r, response::Kind::Ack(_)), "input failed: {r:?}");
    }

    fn kill(&mut self, agent_id: u64) {
        let r = self.request(request::Kind::Kill(KillAgent { agent_id }));
        assert!(matches!(r, response::Kind::Ack(_)), "kill failed: {r:?}");
    }

    /// Collects `agent_id`'s output until it contains `needle`.
    fn output_until(&mut self, agent_id: u64, needle: &str) -> String {
        let mut out = Vec::new();
        loop {
            match self.next_frame() {
                Some(Frame::Output { agent_id: a, data }) if a == agent_id => {
                    out.extend_from_slice(&data);
                    if String::from_utf8_lossy(&out).contains(needle) {
                        return String::from_utf8_lossy(&out).into_owned();
                    }
                }
                Some(_) => {}
                None => panic!(
                    "closed before {needle:?}; got {:?}",
                    String::from_utf8_lossy(&out)
                ),
            }
        }
    }

    /// Waits for the agent's exit event, skipping other frames.
    fn exit_event(&mut self, agent_id: u64) -> AgentExited {
        loop {
            match self.next_frame() {
                Some(Frame::Event(fleet_proto::Event {
                    kind: Some(event::Kind::Exited(e)),
                })) if e.agent_id == agent_id => return e,
                Some(_) => {}
                None => panic!("closed before exit event"),
            }
        }
    }

    /// Waits for the agent's next `AttachRole` event and returns `primary`.
    fn role_event(&mut self, agent_id: u64) -> bool {
        loop {
            match self.next_frame() {
                Some(Frame::Event(fleet_proto::Event {
                    kind: Some(event::Kind::Role(r)),
                })) if r.agent_id == agent_id => return r.primary,
                Some(_) => {}
                None => panic!("closed before role event"),
            }
        }
    }

    /// True if an `AttachRole` event for the agent is buffered.
    fn has_role_event(&self, agent_id: u64) -> bool {
        self.pending.iter().any(|f| {
            matches!(
                f,
                Frame::Event(fleet_proto::Event {
                    kind: Some(event::Kind::Role(r)),
                }) if r.agent_id == agent_id
            )
        })
    }

    fn resize(&mut self, agent_id: u64, cols: u32, rows: u32) {
        let r = self.request(request::Kind::Resize(fleet_proto::Resize {
            agent_id,
            cols,
            rows,
        }));
        assert!(matches!(r, response::Kind::Ack(_)), "resize failed: {r:?}");
    }

    /// Next frame (buffered ones first) arriving before `deadline`.
    fn frame_before(&mut self, deadline: Instant) -> Option<Frame> {
        if let Some(f) = self.pending.pop_front() {
            return Some(f);
        }
        loop {
            if let Some(frame) = FrameCodec::default().decode(&mut self.buf).unwrap() {
                return Some(frame);
            }
            if Instant::now() >= deadline {
                return None;
            }
            let mut chunk = [0u8; 64 * 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) => panic!("connection closed"),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) => panic!("read: {e}"),
            }
        }
    }

    /// True once the server has closed the connection.
    fn is_closed(&mut self) -> bool {
        loop {
            match self.read_frame() {
                None => return true,
                Some(_) => continue,
            }
        }
    }
}

/// Exit events only: activity and role events may arrive at any time.
fn is_exit_event(f: &Frame) -> bool {
    matches!(
        f,
        Frame::Event(fleet_proto::Event {
            kind: Some(event::Kind::Exited(_)),
        })
    )
}

fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn spawn_labeled(c: &mut Client, pairs: &[(&str, &str)]) -> response::Kind {
    c.request(request::Kind::Spawn(Box::new(SpawnAgent {
        program: "/bin/sleep".into(),
        args: vec!["100".into()],
        labels: labels(pairs),
        ..Default::default()
    })))
}

fn set_labels(
    c: &mut Client,
    agent_id: u64,
    set: &[(&str, &str)],
    remove: &[&str],
) -> response::Kind {
    c.request(request::Kind::SetLabels(SetLabels {
        agent_id,
        set: labels(set),
        remove: remove.iter().map(|k| k.to_string()).collect(),
    }))
}

fn ids(list: &[AgentInfo]) -> Vec<u64> {
    let mut ids: Vec<u64> = list.iter().map(|a| a.id).collect();
    ids.sort_unstable();
    ids
}

fn hello(version: u32) -> Frame {
    Frame::Hello(Hello {
        protocol_version: version,
        client: "protocol-test".into(),
        ..Default::default()
    })
}

fn find(list: &[AgentInfo], id: u64) -> &AgentInfo {
    list.iter()
        .find(|a| a.id == id)
        .unwrap_or_else(|| panic!("agent {id} not listed in {list:?}"))
}

/// Polls `list` until the agent leaves the running state.
fn wait_not_running(c: &mut Client, id: u64) -> AgentInfo {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let info = find(&c.list(), id).clone();
        if info.state() != AgentState::Running {
            return info;
        }
        assert!(Instant::now() < deadline, "agent {id} still running");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn handshake_ok() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    assert!(c.list().is_empty());
}

#[test]
fn v1_client_is_rejected() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::connect(dir.path());
    c.send(hello(1));
    match c.next_frame() {
        Some(Frame::Reject(r)) => assert!(r.reason.contains("version 1"), "{}", r.reason),
        other => panic!("expected Reject, got {other:?}"),
    }
    assert!(c.is_closed());
}

#[test]
fn version_mismatch_rejected() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::connect(dir.path());
    c.send(hello(PROTOCOL_VERSION + 1));
    match c.next_frame() {
        Some(Frame::Reject(r)) => assert!(!r.reason.is_empty()),
        other => panic!("expected Reject, got {other:?}"),
    }
    assert!(c.is_closed());
}

#[test]
fn spawn_list_kill() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let info = c.spawn("/bin/sleep", &["100"]);
    assert_eq!(info.program, "/bin/sleep");
    assert_eq!(info.args, vec!["100".to_string()]);
    assert_eq!(info.state(), AgentState::Running);
    assert!(info.created_unix_ms > 0);

    let listed = c.list();
    assert_eq!(find(&listed, info.id).state(), AgentState::Running);

    c.kill(info.id);
    let after = wait_not_running(&mut c, info.id);
    assert_eq!(after.state(), AgentState::Exited);
    assert!(after.exit_code.is_some_and(|code| code != 0));
}

#[test]
fn unknown_agent_is_an_error_response() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    match c.request(request::Kind::Attach(Attach {
        agent_id: 999,
        observe: false,
    })) {
        response::Kind::Error(e) => assert!(!e.message.is_empty()),
        other => panic!("expected error, got {other:?}"),
    }
    // The connection stays usable.
    assert!(c.list().is_empty());
}

#[test]
fn attach_receives_snapshot_then_output() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let info = c.sh("echo before; read l; echo \"after-$l\"; sleep 100");

    // Re-attach until the first line is on the grid.
    let deadline = Instant::now() + TIMEOUT;
    let ok = loop {
        let ok = c.attach(info.id);
        if String::from_utf8_lossy(&ok.snapshot).contains("before") {
            break ok;
        }
        assert!(Instant::now() < deadline, "snapshot never showed output");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(ok.agent_id, info.id);
    assert_eq!((ok.cols, ok.rows), (80, 24));

    c.input(info.id, b"x\n");
    c.output_until(info.id, "after-x");
    c.kill(info.id);
}

#[test]
fn input_round_trip() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let info = c.spawn("/bin/cat", &[]);
    c.attach(info.id);
    c.input(info.id, b"ping-123\n");
    // Echoed by the tty, then printed by cat.
    let out = c.output_until(info.id, "ping-123\r\nping-123");
    assert!(out.matches("ping-123").count() >= 2);
    c.kill(info.id);
}

#[test]
fn resize_reaches_pty_and_grid() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let info = c.sh("read l; stty size; sleep 100");
    c.attach(info.id);
    let r = c.request(request::Kind::Resize(fleet_proto::Resize {
        agent_id: info.id,
        cols: 100,
        rows: 30,
    }));
    assert!(matches!(r, response::Kind::Ack(_)), "resize failed: {r:?}");
    c.input(info.id, b"\n");
    c.output_until(info.id, "30 100");
    let ok = c.attach(info.id);
    assert_eq!((ok.cols, ok.rows), (100, 30));
    c.kill(info.id);
}

#[test]
fn exit_event_delivered() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut attached = Client::open(dir.path());
    let mut observer = Client::open(dir.path());
    let info = attached.sh("read l; exit 7");
    attached.attach(info.id);
    attached.input(info.id, b"\n");

    let e = attached.exit_event(info.id);
    assert_eq!(e.exit_code, 7);
    // An unattached client hears about it too.
    let e = observer.exit_event(info.id);
    assert_eq!(e.exit_code, 7);

    // Exactly once for the attached client: nothing more arrives before the
    // response to a later request.
    let listed = attached.list();
    let dup = attached.pending.iter().any(is_exit_event);
    assert!(!dup, "duplicate exit event: {:?}", attached.pending);

    let rec = find(&listed, info.id);
    assert_eq!(rec.state(), AgentState::Exited);
    assert_eq!(rec.exit_code, Some(7));
}

#[test]
fn reattach_replaces_subscription() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let info = c.sh("read l; echo \"got-$l\"; read l; echo \"end-$l\"; sleep 100");
    c.attach(info.id);
    c.attach(info.id);
    c.input(info.id, b"zz\n");
    c.input(info.id, b"yy\n");
    let out = c.output_until(info.id, "end-yy");
    // A list round trip flushes anything a second subscription would send.
    c.list();
    let mut all = out.into_bytes();
    for f in &c.pending {
        if let Frame::Output { agent_id, data } = f
            && *agent_id == info.id
        {
            all.extend_from_slice(data);
        }
    }
    let all = String::from_utf8_lossy(&all);
    assert_eq!(all.matches("got-zz").count(), 1, "output: {all:?}");
    c.kill(info.id);
}

#[test]
fn client_disconnect_unsubscribes() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut a = Client::open(dir.path());
    let info = a.sh("while :; do echo tick; sleep 0.02; done");
    a.attach(info.id);
    a.output_until(info.id, "tick");
    drop(a);

    let mut b = Client::open(dir.path());
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(find(&b.list(), info.id).state(), AgentState::Running);
    b.attach(info.id);
    b.output_until(info.id, "tick\r\ntick");
    b.kill(info.id);
}

#[test]
fn detach_stops_output() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let info = c.sh("while :; do echo tick; sleep 0.01; done");
    c.attach(info.id);
    c.output_until(info.id, "tick");
    let r = c.request(request::Kind::Detach(fleet_proto::Detach {
        agent_id: info.id,
    }));
    assert!(matches!(r, response::Kind::Ack(_)), "detach failed: {r:?}");
    c.pending.clear();
    std::thread::sleep(Duration::from_millis(200));
    c.list();
    let leaked = c.pending.iter().any(|f| matches!(f, Frame::Output { .. }));
    assert!(!leaked, "output after detach");
    c.kill(info.id);
}

#[test]
fn malformed_frame_closes_only_that_connection() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut good = Client::open(dir.path());
    let mut bad = Client::open(dir.path());
    // Valid length, unknown frame kind.
    bad.send_raw(&[0, 0, 0, 1, 99]);
    assert!(bad.is_closed());

    assert!(good.list().is_empty());
    let mut fresh = Client::open(dir.path());
    assert!(fresh.list().is_empty());
}

#[test]
fn oversized_hello_closes_connection() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::connect(dir.path());
    // A length prefix above the 4 KiB handshake limit.
    c.send_raw(&5000u32.to_be_bytes());
    assert!(c.is_closed());
    Client::open(dir.path());
}

#[test]
fn request_before_hello_closes_connection() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::connect(dir.path());
    c.send(Frame::Request(Request {
        id: 1,
        kind: Some(request::Kind::List(ListAgents::default())),
    }));
    assert!(c.is_closed());
}

#[test]
fn too_many_connections_rejected() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut open: Vec<Client> = (0..64).map(|_| Client::open(dir.path())).collect();

    let mut extra = Client::connect(dir.path());
    match extra.next_frame() {
        Some(Frame::Reject(r)) => assert_eq!(r.reason, "too many connections"),
        other => panic!("expected Reject, got {other:?}"),
    }
    assert!(extra.is_closed());

    // Closing one frees a slot.
    open.pop();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let mut c = Client::connect(dir.path());
        c.send(hello(PROTOCOL_VERSION));
        match c.next_frame() {
            Some(Frame::HelloOk(_)) => break,
            Some(Frame::Reject(_)) | None => {
                assert!(Instant::now() < deadline, "slot never freed");
                std::thread::sleep(Duration::from_millis(20));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn lag_event_names_agent() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    // 32 MiB is several times what fleetd buffers for one subscription (256
    // merged chunks of at most 16 KiB, plus the 256-frame outbox and the
    // socket). Each line homes the cursor, so the screen never scrolls and
    // even a debug build parses it quickly. The agent can only finish its
    // writes once fleetd has read them, so the marker means the output went
    // through the node while this client was not reading. `cat` then keeps
    // the agent alive to echo input.
    let marker = dir.path().join("flooded");
    let line = format!("\x1b[H{}", "x".repeat(900));
    let script = format!(
        "yes \"$(printf '{line}')\" | head -c 33554432; touch '{}'; exec cat",
        marker.display()
    );
    let info = c.sh(&script);
    c.attach(info.id);
    // Stop reading until the agent has flooded.
    let deadline = Instant::now() + Duration::from_secs(120);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "agent never finished flooding");
        std::thread::sleep(Duration::from_millis(20));
    }
    loop {
        match c.next_frame() {
            Some(Frame::Event(fleet_proto::Event {
                kind: Some(event::Kind::Lagged(l)),
            })) => {
                assert_eq!(l.agent_id, info.id);
                break;
            }
            Some(Frame::Response(r)) => panic!("unexpected response {r:?}"),
            Some(_) => {}
            None => panic!("closed before lagged event"),
        }
    }
    // A new subscription streams again.
    c.attach(info.id);
    c.input(info.id, b"ping");
    c.output_until(info.id, "ping");
    c.kill(info.id);
}

#[test]
fn state_dir_is_0700() {
    let tmp = TempDir::new().unwrap();

    let fresh = tmp.path().join("fresh");
    let _d1 = Daemon::start(&fresh);
    let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "new state dir mode {mode:o}");

    let existing = tmp.path().join("existing");
    std::fs::create_dir(&existing).unwrap();
    std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _d2 = Daemon::start(&existing);
    let mode = std::fs::metadata(&existing).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "existing state dir mode {mode:o}");
}

#[test]
fn restart_marks_agents_lost() {
    let dir = TempDir::new().unwrap();
    let d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let running = c.spawn("/bin/sleep", &["100"]);
    let done = c.sh("exit 3");
    let done = wait_not_running(&mut c, done.id);
    assert_eq!(done.exit_code, Some(3));
    // The exit time is listed (ADR 0058), and survives the restart.
    let exited_at = done
        .exited_unix_ms
        .expect("an exited agent lists its exit time");
    assert!(exited_at >= done.created_unix_ms);
    assert_eq!(find(&c.list(), running.id).exited_unix_ms, None);
    drop(c);
    d.crash();

    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let listed = c.list();
    assert_eq!(find(&listed, running.id).state(), AgentState::Lost);
    assert_eq!(find(&listed, running.id).exited_unix_ms, None);
    let done = find(&listed, done.id);
    assert_eq!(done.state(), AgentState::Exited);
    assert_eq!(done.exit_code, Some(3));
    assert_eq!(done.exited_unix_ms, Some(exited_at));
    // A new agent gets a fresh id.
    let next = c.spawn("/bin/sleep", &["100"]);
    assert!(next.id > running.id && next.id > done.id);
    c.kill(next.id);
}

#[test]
fn attach_to_exited_agent_gets_snapshot_and_exit() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let info = c.sh("echo bye-now; exit 4");
    assert_eq!(wait_not_running(&mut c, info.id).exit_code, Some(4));
    c.pending.clear();
    let ok = c.attach(info.id);
    assert!(
        String::from_utf8_lossy(&ok.snapshot).contains("bye-now"),
        "snapshot: {:?}",
        String::from_utf8_lossy(&ok.snapshot)
    );
    assert_eq!(c.exit_event(info.id).exit_code, 4);
}

/// True while any process of group `pgid` exists.
fn group_alive(pgid: i32) -> bool {
    // SAFETY: kill with signal 0 only checks for existence.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

#[test]
fn sigterm_stops_agents_and_records_their_exit() {
    let dir = TempDir::new().unwrap();
    let mut d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    // Ignores the PTY hangup and SIGTERM: only SIGKILL stops it.
    let info = c.sh("trap '' HUP TERM; echo \"pid=$$ ready\"; sleep 600");
    // The echo may land before the attach (then it is in the snapshot, drawn
    // with cursor moves and no "\n") or after it (then in Output frames).
    let snapshot = String::from_utf8_lossy(&c.attach(info.id).snapshot).into_owned();
    let out = if snapshot.contains(" ready") {
        snapshot
    } else {
        c.output_until(info.id, " ready")
    };
    let pid: i32 = out
        .split("pid=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no pid in {out:?}"));
    assert!(group_alive(pid));

    let started = Instant::now();
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(d.child.id().to_string())
        .status()
        .unwrap();
    assert!(status.success());
    let deadline = started + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = d.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "fleetd did not exit within 5s");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "fleetd exited with {status}");
    assert!(!group_alive(pid), "agent group {pid} survived fleetd");
    assert!(!dir.path().join("fleetd.sock").exists());
    drop(c);
    drop(d);

    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let rec = find(&c.list(), info.id).clone();
    assert_eq!(rec.state(), AgentState::Exited, "{rec:?}");
    assert!(rec.exit_code.is_some());
}

/// Spawns 60 agents that stay alive, under fleetd started with `limit`.
fn sixty_agents_under(limit: &str) {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start_with_ulimit(dir.path(), limit);
    let mut c = Client::open(dir.path());
    let mut ids = Vec::new();
    for i in 0..60 {
        match c.request(request::Kind::Spawn(Box::new(SpawnAgent {
            program: "/bin/sleep".into(),
            args: vec!["100".into()],
            ..Default::default()
        }))) {
            response::Kind::Spawned(info) => ids.push(info.id),
            other => panic!("spawn {i} under `ulimit {limit}` failed: {other:?}"),
        }
    }
    for id in ids {
        c.kill(id);
    }
}

#[test]
fn sixty_agents_with_low_soft_fd_limit() {
    // Too low even for the agents' own fds: fleetd raises its soft limit
    // to the hard limit.
    sixty_agents_under("-S -n 128");
}

#[test]
fn sixty_agents_within_256_fds() {
    // Soft and hard limit: the agents' own fds must fit.
    sixty_agents_under("-n 256");
}

#[test]
fn out_of_range_sizes_are_error_responses() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    for (cols, rows) in [(1001, 24), (80, 1001), (501, 500)] {
        match c.request(request::Kind::Spawn(Box::new(SpawnAgent {
            program: "/bin/sleep".into(),
            args: vec!["100".into()],
            cols,
            rows,
            ..Default::default()
        }))) {
            response::Kind::Error(e) => assert!(e.message.contains("out of range"), "{e:?}"),
            other => panic!("spawn {cols}x{rows}: expected error, got {other:?}"),
        }
    }
    let info = c.spawn("/bin/sleep", &["100"]);
    for (cols, rows) in [(0, 24), (80, 0), (1001, 24), (501, 500)] {
        match c.request(request::Kind::Resize(fleet_proto::Resize {
            agent_id: info.id,
            cols,
            rows,
        })) {
            response::Kind::Error(e) => assert!(e.message.contains("out of range"), "{e:?}"),
            other => panic!("resize {cols}x{rows}: expected error, got {other:?}"),
        }
    }
    c.kill(info.id);
}

#[test]
fn agents_get_xterm_256color_unless_term_is_given() {
    let dir = TempDir::new().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fleetd"));
    cmd.env("TERM", "daemon-term");
    let _d = Daemon::start_with(dir.path(), cmd);
    let mut c = Client::open(dir.path());
    let script = "echo \"term=$TERM.\"; sleep 100";
    for (env, want) in [
        (vec![], "term=xterm-256color."),
        (
            vec![("TERM".to_string(), "vt100".to_string())],
            "term=vt100.",
        ),
    ] {
        let info = match c.request(request::Kind::Spawn(Box::new(SpawnAgent {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: env.into_iter().collect(),
            ..Default::default()
        }))) {
            response::Kind::Spawned(info) => *info,
            other => panic!("spawn failed: {other:?}"),
        };
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let snap = String::from_utf8_lossy(&c.attach(info.id).snapshot).into_owned();
            if snap.contains("term=") {
                assert!(snap.contains(want), "want {want:?} in {snap:?}");
                break;
            }
            assert!(Instant::now() < deadline, "no output in {snap:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
        c.kill(info.id);
    }
}

/// The printable text of an ANSI history line (escape sequences dropped).
fn plain(line: &[u8]) -> String {
    let mut out = String::new();
    let mut esc = false;
    for c in String::from_utf8_lossy(line).chars() {
        match (esc, c) {
            (false, '\x1b') => esc = true,
            (false, c) => out.push(c),
            (true, c) if c.is_ascii_alphabetic() => esc = false,
            (true, _) => {}
        }
    }
    out.trim_end().to_string()
}

fn read_history(c: &mut Client, req: ReadHistory) -> response::Kind {
    c.request(request::Kind::ReadHistory(req))
}

#[test]
fn read_history_round_trip() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let info = c.sh("seq 1 5000; echo seq-done; sleep 100");
    let deadline = Instant::now() + TIMEOUT;
    let ok = loop {
        let ok = c.attach(info.id);
        if String::from_utf8_lossy(&ok.snapshot).contains("seq-done") {
            break ok;
        }
        assert!(Instant::now() < deadline, "seq never finished");
        std::thread::sleep(Duration::from_millis(20));
    };
    let end = ok.tail_first_line + ok.history_tail.len() as u64;
    assert!(end >= 4900, "only {end} history lines");

    // A page ending where the tail ends repeats the tail's last lines.
    let page = match read_history(
        &mut c,
        ReadHistory {
            agent_id: info.id,
            generation: ok.history_generation,
            before_line: end,
            max_lines: 10,
        },
    ) {
        response::Kind::History(page) => page,
        other => panic!("read_history failed: {other:?}"),
    };
    assert_eq!(page.agent_id, info.id);
    assert_eq!(page.first_line, end - 10);
    assert_eq!(page.lines, ok.history_tail[ok.history_tail.len() - 10..]);

    // `max_lines` is capped server-side; every line is the next number.
    let HistoryPage {
        first_line, lines, ..
    } = match read_history(
        &mut c,
        ReadHistory {
            agent_id: info.id,
            generation: ok.history_generation,
            before_line: end,
            max_lines: u32::MAX,
        },
    ) {
        response::Kind::History(page) => page,
        other => panic!("read_history failed: {other:?}"),
    };
    assert_eq!(lines.len(), 4096);
    assert_eq!(first_line, end - 4096);
    for (i, line) in lines.iter().enumerate() {
        // `seq 1 N` prints `n` on line `n - 1`.
        assert_eq!(plain(line), (first_line + i as u64 + 1).to_string());
    }

    // A stale generation and an unknown agent are errors.
    match read_history(
        &mut c,
        ReadHistory {
            agent_id: info.id,
            generation: ok.history_generation + 1,
            before_line: end,
            max_lines: 10,
        },
    ) {
        response::Kind::Error(e) => assert_eq!(e.message, "history cleared"),
        other => panic!("expected error, got {other:?}"),
    }
    match read_history(
        &mut c,
        ReadHistory {
            agent_id: 999,
            generation: 0,
            before_line: 10,
            max_lines: 10,
        },
    ) {
        response::Kind::Error(e) => assert!(!e.message.is_empty()),
        other => panic!("expected error, got {other:?}"),
    }
    c.kill(info.id);
}

#[test]
fn spawn_attach_replies_with_snapshot_and_streams_first_output() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    // The very first output, a terminal query included, must be streamed: an
    // unattached agent's queries are answered by the node instead.
    let r = c.request(request::Kind::Spawn(Box::new(SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "printf 'first-out\\033[6n'; sleep 100".into()],
        cols: 100,
        rows: 30,
        attach: true,
        labels: labels(&[("project", "fleetd")]),
        ..Default::default()
    })));
    let (agent, attached) = match r {
        response::Kind::SpawnedAttached(sa) => (sa.agent.unwrap(), sa.attached.unwrap()),
        other => panic!("expected SpawnedAttached, got {other:?}"),
    };
    assert_eq!(agent.state(), AgentState::Running);
    assert_eq!(agent.labels, labels(&[("project", "fleetd")]));
    assert_eq!(attached.agent_id, agent.id);
    assert_eq!((attached.cols, attached.rows), (100, 30));
    assert!(
        !String::from_utf8_lossy(&attached.snapshot).contains("first-out"),
        "the first output landed in the snapshot"
    );
    let out = c.output_until(agent.id, "\x1b[6n");
    assert!(out.contains("first-out"), "{out:?}");

    // The attachment is a normal one: its exit arrives once, after output.
    c.kill(agent.id);
    c.exit_event(agent.id);
    c.list();
    let dup = c.pending.iter().any(is_exit_event);
    assert!(!dup, "duplicate exit event: {:?}", c.pending);
}

#[test]
fn spawn_attach_of_quickly_exiting_agent_streams_output_then_one_exit() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let r = c.request(request::Kind::Spawn(Box::new(SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "echo quick-bye; exit 5".into()],
        attach: true,
        ..Default::default()
    })));
    let agent = match r {
        response::Kind::SpawnedAttached(sa) => sa.agent.unwrap(),
        other => panic!("expected SpawnedAttached, got {other:?}"),
    };
    // Nothing of the agent's stream (output, exit) precedes the reply.
    // Activity events go to every connection apart from it, so one for
    // the agent's output may arrive first.
    let early = c.pending.iter().any(|f| activity_of(f).is_none());
    assert!(!early, "frames before the reply: {:?}", c.pending);
    c.output_until(agent.id, "quick-bye");
    assert_eq!(c.exit_event(agent.id).exit_code, 5);
    c.list();
    let dup = c.pending.iter().any(is_exit_event);
    assert!(!dup, "duplicate exit event: {:?}", c.pending);
}

#[test]
fn labels_set_at_spawn_listed_and_filtered() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let coder = [("project", "fleetd"), ("role", "coder")];
    let reviewer = [("project", "fleetd"), ("role", "reviewer")];
    let spawn = |c: &mut Client, pairs: &[(&str, &str)]| match spawn_labeled(c, pairs) {
        response::Kind::Spawned(info) => {
            assert_eq!(info.labels, labels(pairs));
            info.id
        }
        other => panic!("spawn failed: {other:?}"),
    };
    let a = spawn(&mut c, &coder);
    let b = spawn(&mut c, &reviewer);
    let n = spawn(&mut c, &[]);

    let all = c.list();
    assert_eq!(ids(&all), vec![a, b, n]);
    assert_eq!(find(&all, a).labels, labels(&coder));
    assert_eq!(find(&all, b).labels, labels(&reviewer));
    assert!(find(&all, n).labels.is_empty());

    assert_eq!(ids(&c.list_selected(&[("project", "fleetd")])), vec![a, b]);
    assert_eq!(ids(&c.list_selected(&reviewer)), vec![b]);
    assert_eq!(ids(&c.list_selected(&[("role", "coder")])), vec![a]);
    assert!(c.list_selected(&[("project", "other")]).is_empty());
    assert!(c.list_selected(&[("missing", "")]).is_empty());
    for id in [a, b, n] {
        c.kill(id);
    }
}

#[test]
fn set_labels_updates_and_removes() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let id = match spawn_labeled(&mut c, &[("a", "1"), ("b", "2")]) {
        response::Kind::Spawned(info) => info.id,
        other => panic!("spawn failed: {other:?}"),
    };
    let r = set_labels(&mut c, id, &[("b", "3"), ("c", "4")], &["a", "absent"]);
    assert!(
        matches!(r, response::Kind::Ack(_)),
        "set_labels failed: {r:?}"
    );
    assert_eq!(
        find(&c.list(), id).labels,
        labels(&[("b", "3"), ("c", "4")])
    );
    assert_eq!(ids(&c.list_selected(&[("b", "3")])), vec![id]);
    assert!(c.list_selected(&[("a", "1")]).is_empty());

    // A key both set and removed ends up removed.
    let r = set_labels(&mut c, id, &[("d", "5")], &["d", "c"]);
    assert!(
        matches!(r, response::Kind::Ack(_)),
        "set_labels failed: {r:?}"
    );
    assert_eq!(find(&c.list(), id).labels, labels(&[("b", "3")]));

    // Unknown agents are an error; exited ones keep their labels editable.
    match set_labels(&mut c, 999, &[("x", "y")], &[]) {
        response::Kind::Error(e) => assert!(!e.message.is_empty()),
        other => panic!("expected error, got {other:?}"),
    }
    c.kill(id);
    wait_not_running(&mut c, id);
    let r = set_labels(&mut c, id, &[("done", "yes")], &[]);
    assert!(
        matches!(r, response::Kind::Ack(_)),
        "set_labels failed: {r:?}"
    );
    assert_eq!(
        find(&c.list(), id).labels,
        labels(&[("b", "3"), ("done", "yes")])
    );
}

#[test]
fn label_limits_and_reserved_prefix_rejected() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let key64 = "k".repeat(64);
    let key65 = "k".repeat(65);
    let val256 = "v".repeat(256);
    let val257 = "v".repeat(257);
    let many: Vec<(String, String)> = (0..65).map(|i| (format!("k{i}"), "v".into())).collect();
    let pairs = |n: usize| -> Vec<(&str, &str)> {
        many[..n]
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    };

    let bad_spawns: Vec<Vec<(&str, &str)>> = vec![
        vec![("fleet.parent", "1")],
        vec![("", "v")],
        vec![(key65.as_str(), "v")],
        vec![("k", val257.as_str())],
        pairs(65),
    ];
    for bad in &bad_spawns {
        match spawn_labeled(&mut c, bad) {
            response::Kind::Error(e) => assert!(!e.message.is_empty()),
            other => panic!(
                "spawn with {} labels: expected error, got {other:?}",
                bad.len()
            ),
        }
    }
    assert!(c.list().is_empty(), "rejected spawns left rows");

    let mut at_limit = pairs(63);
    at_limit.push((key64.as_str(), val256.as_str()));
    let id = match spawn_labeled(&mut c, &at_limit) {
        response::Kind::Spawned(info) => info.id,
        other => panic!("spawn at the limits failed: {other:?}"),
    };
    assert_eq!(find(&c.list(), id).labels.len(), 64);

    for (set, remove) in [
        (vec![("fleet.parent", "1")], vec![]),
        (vec![], vec!["fleet.parent"]),
        (vec![("", "v")], vec![]),
        (vec![(key65.as_str(), "v")], vec![]),
        (vec![("k", val257.as_str())], vec![]),
        // A 65th label.
        (vec![("one-more", "v")], vec![]),
    ] {
        match set_labels(&mut c, id, &set, &remove) {
            response::Kind::Error(e) => assert!(!e.message.is_empty()),
            other => panic!("set {set:?} remove {remove:?}: expected error, got {other:?}"),
        }
    }
    // Rejected changes change nothing.
    assert_eq!(find(&c.list(), id).labels, labels(&at_limit));

    // The limit applies after the change: replacing one label is fine.
    let r = set_labels(&mut c, id, &[("one-more", "v")], &["k0"]);
    assert!(
        matches!(r, response::Kind::Ack(_)),
        "set_labels failed: {r:?}"
    );
    let listed = find(&c.list(), id).labels.clone();
    assert_eq!(listed.len(), 64);
    assert!(listed.contains_key("one-more") && !listed.contains_key("k0"));
    c.kill(id);
}

#[test]
fn labels_survive_daemon_restart() {
    let dir = TempDir::new().unwrap();
    let d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let id = match spawn_labeled(&mut c, &[("project", "fleetd"), ("role", "coder")]) {
        response::Kind::Spawned(info) => info.id,
        other => panic!("spawn failed: {other:?}"),
    };
    let r = set_labels(&mut c, id, &[("role", "reviewer")], &[]);
    assert!(
        matches!(r, response::Kind::Ack(_)),
        "set_labels failed: {r:?}"
    );
    drop(c);
    d.crash();

    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let want = labels(&[("project", "fleetd"), ("role", "reviewer")]);
    let rec = find(&c.list(), id).clone();
    assert_eq!(rec.state(), AgentState::Lost);
    assert_eq!(rec.labels, want);
    assert_eq!(ids(&c.list_selected(&[("role", "reviewer")])), vec![id]);
}

#[test]
fn primary_moves_on_input_and_only_primary_resizes() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut a = Client::open(dir.path());
    let mut b = Client::open(dir.path());
    // Prints the PTY size for every line of input.
    let info = a.sh("while read l; do echo \"size=$(stty size).\"; done");
    let id = info.id;

    assert!(a.attach(id).primary, "the only subscriber is primary");
    assert!(b.attach(id).primary, "the latest attach is primary");
    assert!(!a.role_event(id), "A was demoted when B attached");

    // A is not primary: its resize is acknowledged and ignored.
    a.resize(id, 100, 30);
    b.input(id, b"\n");
    b.output_until(id, "size=24 80.");
    assert!(!a.has_role_event(id) && !b.has_role_event(id));

    // Input moves the role to A.
    a.input(id, b"\n");
    assert!(a.role_event(id), "A became primary by sending input");
    assert!(!b.role_event(id), "B was demoted");
    a.output_until(id, "size=24 80.");

    // Now A's resize applies.
    a.resize(id, 100, 30);
    a.input(id, b"\n");
    a.output_until(id, "size=30 100.");
    a.kill(id);
}

#[test]
fn observing_attach_never_takes_the_primary_role() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut a = Client::open(dir.path());
    let mut b = Client::open(dir.path());
    let info = a.sh("while read l; do echo \"size=$(stty size).\"; done");
    let id = info.id;

    assert!(a.attach(id).primary);
    assert!(!b.observe(id).primary, "an observer is never primary");
    // The observer's resize is ignored; the primary's still applies.
    b.resize(id, 120, 40);
    a.resize(id, 100, 30);
    a.input(id, b"\n");
    a.output_until(id, "size=30 100.");
    b.output_until(id, "size=30 100.");
    // Input from the observer does not promote it either.
    b.input(id, b"\n");
    b.output_until(id, "size=30 100.");
    a.output_until(id, "size=30 100.");
    a.list();
    b.list();
    assert!(!a.has_role_event(id), "the primary got a role event");
    assert!(!b.has_role_event(id), "the observer got a role event");
    a.kill(id);
}

/// Resizes reach every subscriber in-stream, ordered with the output.
#[test]
fn observer_sees_agent_resized_in_stream() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut a = Client::open(dir.path());
    let mut b = Client::open(dir.path());
    let id = a.sh("while read l; do echo \"line=$l.\"; done").id;
    a.attach(id);
    let ok = b.observe(id);
    assert_eq!((ok.cols, ok.rows), (80, 24));

    a.input(id, b"one\n");
    b.output_until(id, "line=one.");
    a.resize(id, 100, 30);
    a.input(id, b"two\n");
    // In order: the resize, then the output after it.
    let mut out = Vec::new();
    let mut resized = None;
    while !String::from_utf8_lossy(&out).contains("line=two.") {
        match b.next_frame() {
            Some(Frame::Output { agent_id, data }) if agent_id == id => {
                out.extend_from_slice(&data);
            }
            Some(Frame::Event(fleet_proto::Event {
                kind: Some(event::Kind::AgentResized(r)),
            })) if r.agent_id == id => {
                assert!(resized.is_none(), "one resize, one event");
                assert!(
                    !String::from_utf8_lossy(&out).contains("line=two."),
                    "resize event after later output"
                );
                resized = Some((r.cols, r.rows));
            }
            Some(_) => {}
            None => panic!("closed"),
        }
    }
    assert_eq!(resized, Some((100, 30)));
    // The primary gets it too.
    loop {
        match a.next_frame() {
            Some(Frame::Event(fleet_proto::Event {
                kind: Some(event::Kind::AgentResized(r)),
            })) if r.agent_id == id => {
                assert_eq!((r.cols, r.rows), (100, 30));
                break;
            }
            Some(_) => {}
            None => panic!("closed"),
        }
    }
    a.kill(id);
}

#[test]
fn primary_leaving_promotes_the_latest_remaining_subscriber() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut a = Client::open(dir.path());
    let mut b = Client::open(dir.path());
    let mut c = Client::open(dir.path());
    let id = a.spawn("/bin/sleep", &["100"]).id;

    a.attach(id);
    b.attach(id);
    assert!(!a.role_event(id));
    c.attach(id);
    assert!(!b.role_event(id));
    // B sends input: B is primary, then the most recent.
    b.input(id, b"x");
    assert!(b.role_event(id));
    assert!(!c.role_event(id));

    // B detaches: C (more recent than A) takes over.
    let r = b.request(request::Kind::Detach(fleet_proto::Detach { agent_id: id }));
    assert!(matches!(r, response::Kind::Ack(_)), "{r:?}");
    assert!(c.role_event(id), "C promoted when the primary detached");

    // C's connection closes: A takes over.
    drop(c);
    assert!(a.role_event(id), "A promoted when the primary disconnected");
    a.kill(id);
}

#[test]
fn spawn_attach_makes_the_spawner_primary() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut a = Client::open(dir.path());
    let mut b = Client::open(dir.path());
    let r = a.request(request::Kind::Spawn(Box::new(SpawnAgent {
        program: "/bin/sleep".into(),
        args: vec!["100".into()],
        attach: true,
        ..Default::default()
    })));
    let (id, ok) = match r {
        response::Kind::SpawnedAttached(sa) => (sa.agent.unwrap().id, sa.attached.unwrap()),
        other => panic!("expected SpawnedAttached, got {other:?}"),
    };
    assert!(ok.primary);
    assert!(b.attach(id).primary);
    assert!(!a.role_event(id), "the spawner was demoted");
    a.kill(id);
}

#[test]
fn agent_activity_is_throttled() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    // A 250-char line, printed as fast as possible on a 300-column grid.
    let script = "l=$(printf '%0250d' 0); while :; do echo \"$l\"; done";
    let id = match c.request(request::Kind::Spawn(Box::new(SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cols: 300,
        rows: 24,
        ..Default::default()
    }))) {
        response::Kind::Spawned(info) => info.id,
        other => panic!("spawn failed: {other:?}"),
    };

    // Count the agent's activity events in the second after the first one.
    let first = Instant::now() + TIMEOUT;
    let mut events = Vec::new();
    let mut window_end = first;
    while let Some(frame) = c.frame_before(window_end) {
        if let Frame::Event(fleet_proto::Event {
            kind: Some(event::Kind::Activity(a)),
        }) = frame
            && a.agent_id == id
        {
            if events.is_empty() {
                window_end = Instant::now() + Duration::from_secs(1);
            }
            events.push(a);
        }
    }
    assert!(!events.is_empty(), "no activity within {TIMEOUT:?}");
    assert!(
        (2..=5).contains(&events.len()),
        "{} activity events in 1s",
        events.len()
    );
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    for a in &events {
        assert!(a.last_line.chars().count() <= 200, "{:?}", a.last_line);
        assert!(!a.last_line.is_empty());
        assert!(a.last_line.chars().all(|ch| ch == '0'), "{:?}", a.last_line);
        assert!(
            (now_ms - a.last_output_unix_ms).abs() < 10_000,
            "last_output_unix_ms {} vs now {now_ms}",
            a.last_output_unix_ms
        );
    }
    c.kill(id);
}

#[test]
fn agent_sees_its_id() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let script = "echo \"id=$FLEET_AGENT_ID.\"; sleep 100";
    // Clients cannot make an agent impersonate another (ADR 0025).
    for env in [
        vec![],
        vec![("FLEET_AGENT_ID".to_string(), "999".to_string())],
    ] {
        let info = match c.request(request::Kind::Spawn(Box::new(SpawnAgent {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: env.into_iter().collect(),
            ..Default::default()
        }))) {
            response::Kind::Spawned(info) => *info,
            other => panic!("spawn failed: {other:?}"),
        };
        let want = format!("id={}.", info.id);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let snap = String::from_utf8_lossy(&c.attach(info.id).snapshot).into_owned();
            if snap.contains("id=") {
                assert!(snap.contains(&want), "want {want:?} in {snap:?}");
                break;
            }
            assert!(Instant::now() < deadline, "no output in {snap:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
        c.kill(info.id);
    }
}

/// The activity event in `frame`, if any.
fn activity_of(frame: &Frame) -> Option<&fleet_proto::AgentActivity> {
    match frame {
        Frame::Event(fleet_proto::Event {
            kind: Some(event::Kind::Activity(a)),
        }) => Some(a),
        _ => None,
    }
}

#[test]
fn backed_up_client_still_gets_every_agents_final_line() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut slow = Client::open(dir.path());
    let mut ctl = Client::open(dir.path());
    // The slow client attaches to a flood and stops reading: its outbox and
    // socket fill, so its activity forwarder is stuck behind them.
    let flood = slow.sh("while :; do echo flood-flood-flood-flood; done");
    slow.attach(flood.id);

    let mut want = HashMap::new();
    for i in 0..100 {
        let script = format!(
            "for s in 1 2 3 4; do echo step-$s; sleep 0.3; done; echo final-{i}; sleep 100"
        );
        let id = ctl.sh(&script).id;
        want.insert(id, format!("final-{i}"));
    }
    // Every agent has printed its final line and gone quiet.
    std::thread::sleep(Duration::from_secs(3));
    ctl.kill(flood.id);

    let mut latest: HashMap<u64, String> = HashMap::new();
    let deadline = Instant::now() + TIMEOUT;
    while latest
        .iter()
        .filter(|(id, l)| want.get(id) == Some(l))
        .count()
        < want.len()
    {
        match slow.frame_before(deadline) {
            Some(f) => {
                if let Some(a) = activity_of(&f) {
                    latest.insert(a.agent_id, a.last_line.clone());
                }
            }
            None => break,
        }
    }
    let stale: Vec<_> = want
        .iter()
        .filter(|(id, l)| latest.get(id) != Some(l))
        .map(|(id, l)| (id, l, latest.get(id)))
        .collect();
    assert!(stale.is_empty(), "{} agents stale: {stale:?}", stale.len());
    for id in want.keys() {
        ctl.kill(*id);
    }
}

#[test]
fn no_activity_after_exit() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Client::open(dir.path());
    let mut ctl = Client::open(dir.path());
    let ids: Vec<u64> = (0..5)
        .map(|_| ctl.sh("while :; do echo busy; done").id)
        .collect();
    std::thread::sleep(Duration::from_millis(400));
    for &id in &ids {
        ctl.kill(id);
    }
    let mut exited = HashSet::new();
    // Read until well past a tick after the last exit.
    let mut deadline = Instant::now() + TIMEOUT;
    while let Some(f) = c.frame_before(deadline) {
        if let Frame::Event(fleet_proto::Event {
            kind: Some(event::Kind::Exited(e)),
        }) = &f
        {
            exited.insert(e.agent_id);
            if exited.len() == ids.len() {
                deadline = Instant::now() + Duration::from_millis(800);
            }
        }
        if let Some(a) = activity_of(&f) {
            assert!(!exited.contains(&a.agent_id), "activity after exit: {a:?}");
        }
    }
    assert_eq!(exited.len(), ids.len());
}
