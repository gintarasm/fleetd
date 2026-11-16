//! Daemon logging (ADR 0027): the log file under `<state-dir>/logs`, what is
//! (and is never) logged, and `ReadLogs` over the protocol.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use fleet_proto::{
    Frame, FrameCodec, Hello, KillAgent, PROTOCOL_VERSION, ReadLogs, Request, SendInput,
    SpawnAgent, StopLogs, event, request, response,
};
use tempfile::TempDir;
use tokio_util::codec::{Decoder, Encoder};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fleetd() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fleetd"));
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    cmd
}

/// Starts fleetd on `dir` with `env`, without a login-env capture unless
/// `env` sets `SHELL`.
fn start(dir: &Path, env: &[(&str, &str)]) -> Daemon {
    let mut cmd = fleetd();
    if !env.iter().any(|(k, _)| *k == "SHELL") {
        cmd.env("FLEETD_LOGIN_ENV", "off");
    } else {
        cmd.env_remove("FLEETD_LOGIN_ENV");
    }
    let child = cmd
        .envs(env.iter().copied())
        .arg("--state-dir")
        .arg(dir)
        .spawn()
        .expect("spawn fleetd");
    let sock = dir.join("fleetd.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&sock).is_err() {
        assert!(Instant::now() < deadline, "daemon not ready within 5s");
        std::thread::sleep(Duration::from_millis(5));
    }
    Daemon(child)
}

fn log_path(dir: &Path) -> PathBuf {
    dir.join("logs").join("fleetd.log")
}

fn read_log(dir: &Path) -> String {
    std::fs::read_to_string(log_path(dir)).unwrap_or_default()
}

/// Waits until the daemon's log contains every one of `needles`.
fn wait_log(dir: &Path, needles: &[&str]) -> String {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let log = read_log(dir);
        if needles.iter().all(|n| log.contains(n)) {
            return log;
        }
        assert!(
            Instant::now() < deadline,
            "log lacks one of {needles:?}:\n{log}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct Client {
    stream: UnixStream,
    buf: BytesMut,
    next_id: u64,
}

impl Client {
    fn open(dir: &Path) -> Client {
        let stream = UnixStream::connect(dir.join("fleetd.sock")).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut c = Client {
            stream,
            buf: BytesMut::new(),
            next_id: 1,
        };
        c.send(Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client: "logging-test".into(),
        }));
        match c.next_frame(TIMEOUT) {
            Some(Frame::HelloOk(_)) => c,
            other => panic!("expected HelloOk, got {other:?}"),
        }
    }

    fn send(&mut self, frame: Frame) {
        let mut out = BytesMut::new();
        FrameCodec::default().encode(frame, &mut out).unwrap();
        self.stream.write_all(&out).unwrap();
    }

    /// The next frame within `wait`; `None` on timeout. Panics if the
    /// connection closed.
    fn next_frame(&mut self, wait: Duration) -> Option<Frame> {
        let deadline = Instant::now() + wait;
        let mut codec = FrameCodec::default();
        loop {
            if let Some(f) = codec.decode(&mut self.buf).unwrap() {
                return Some(f);
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

    fn send_request(&mut self, kind: request::Kind) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(Frame::Request(Request {
            id,
            kind: Some(kind),
        }));
        id
    }

    /// Sends `kind` and returns its response; other frames are skipped.
    fn request(&mut self, kind: request::Kind) -> response::Kind {
        let id = self.send_request(kind);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.next_frame(left) {
                Some(Frame::Response(r)) if r.id == id => return r.kind.expect("response kind"),
                Some(_) => {}
                None => panic!("no response to request {id}"),
            }
        }
    }

    fn read_logs(&mut self, tail: u32, follow: bool) -> Vec<String> {
        match self.request(request::Kind::ReadLogs(ReadLogs { tail, follow })) {
            response::Kind::LogLines(l) => l.lines,
            other => panic!("expected LogLines, got {other:?}"),
        }
    }

    /// The next `LogLine` event's line within `wait`, skipping other frames.
    fn next_log_line(&mut self, wait: Duration) -> Option<String> {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if let Frame::Event(fleet_proto::Event {
                kind: Some(event::Kind::LogLine(l)),
            }) = self.next_frame(left)?
            {
                return Some(l.line);
            }
        }
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn log_file_created_private() {
    let dir = TempDir::new().unwrap();
    let _d = start(dir.path(), &[]);
    let log = wait_log(dir.path(), &["fleetd started"]);
    assert_eq!(mode(&dir.path().join("logs")), 0o700);
    assert_eq!(mode(&log_path(dir.path())), 0o600);
    // One line per event: RFC 3339 UTC timestamp, level, target, message and
    // key=value fields.
    let started = log.lines().find(|l| l.contains("fleetd started")).unwrap();
    let ts = started.split_whitespace().next().unwrap();
    assert!(
        ts.len() >= 20
            && ts.as_bytes()[4] == b'-'
            && ts.as_bytes()[10] == b'T'
            && ts.ends_with('Z'),
        "not an RFC 3339 UTC timestamp: {started}"
    );
    assert!(started.contains(" INFO "), "{started}");
    assert!(started.contains("fleetd"), "{started}");
    assert!(started.contains(&format!("pid={}", _d.0.id())), "{started}");
    assert!(
        started.contains(&format!("protocol={PROTOCOL_VERSION}")),
        "{started}"
    );
}

#[test]
fn spawn_and_exit_are_logged_without_env_values() {
    let dir = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    // A login shell exporting a sentinel: the captured env must never be logged.
    let shell = work.path().join("login.sh");
    std::fs::write(
        &shell,
        "#!/bin/sh\nexport FLEET_LOG_SENTINEL=login-sentinel-7c3e\nexec /bin/sh -c \"$2\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _d = start(
        dir.path(),
        &[("SHELL", shell.to_str().unwrap()), ("FLEETD_LOG", "trace")],
    );
    let mut c = Client::open(dir.path());

    let spawn = |program: &str, args: &[&str]| SpawnAgent {
        program: program.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        cwd: work.path().to_string_lossy().into_owned(),
        env: HashMap::from([("SECRET".to_string(), "hunter2".to_string())]),
        labels: HashMap::from([("team".to_string(), "label-value-q9".to_string())]),
        ..Default::default()
    };
    let id = match c.request(request::Kind::Spawn(Box::new(spawn(
        "/bin/sh",
        &["-c", "echo \"out-$SECRET-$FLEET_LOG_SENTINEL\"; exit 3"],
    )))) {
        response::Kind::Spawned(info) => info.id,
        other => panic!("spawn: {other:?}"),
    };
    // An agent fed input, which echoes it (PTY output), then killed.
    let cat = match c.request(request::Kind::Spawn(Box::new(spawn("/bin/cat", &[])))) {
        response::Kind::Spawned(info) => info.id,
        other => panic!("spawn: {other:?}"),
    };
    match c.request(request::Kind::Input(SendInput {
        agent_id: cat,
        data: b"typed-input-secret-5d\n".to_vec(),
    })) {
        response::Kind::Ack(_) => {}
        other => panic!("input: {other:?}"),
    }
    std::thread::sleep(Duration::from_millis(200));
    match c.request(request::Kind::Kill(KillAgent { agent_id: cat })) {
        response::Kind::Ack(_) => {}
        other => panic!("kill: {other:?}"),
    }

    let log = wait_log(
        dir.path(),
        &[
            &format!("agent spawned agent={id}"),
            &format!("agent exited agent={id} code=3"),
            &format!("agent killed agent={cat}"),
            &format!("agent exited agent={cat}"),
            "login env captured",
        ],
    );
    let spawned = log
        .lines()
        .find(|l| l.contains(&format!("agent spawned agent={id}")))
        .unwrap();
    assert!(spawned.contains("/bin/sh"), "{spawned}");
    assert!(spawned.contains("team"), "label keys are logged: {spawned}");
    for secret in [
        "hunter2",
        "login-sentinel-7c3e",
        "label-value-q9",
        "typed-input-secret-5d",
        "out-",
    ] {
        assert!(
            !log.contains(secret),
            "{secret:?} leaked into the log:\n{log}"
        );
    }
}

/// Every line of the log starts with an RFC 3339 timestamp: nothing a client
/// or an env value controls can start a line of its own.
fn assert_lines_are_events(log: &str) {
    for line in log.lines() {
        let b = line.as_bytes();
        assert!(
            b.len() > 20 && b[..4].iter().all(u8::is_ascii_digit) && b[4] == b'-' && b[10] == b'T',
            "a line that is not an event: {line:?}\n{log}"
        );
    }
}

#[test]
fn spawn_failure_logs_no_env_and_cannot_inject_lines() {
    let dir = TempDir::new().unwrap();
    let _d = start(dir.path(), &[("FLEETD_LOG", "trace")]);
    let mut c = Client::open(dir.path());
    // A relative program not on PATH: the spawn error names the PATH searched.
    let spawn = SpawnAgent {
        program: "no-such-prog\n2026-01-01T00:00:00Z ERROR fake: injected\r".into(),
        env: HashMap::from([("PATH".to_string(), "/opt/sentinel-path-77".to_string())]),
        ..Default::default()
    };
    match c.request(request::Kind::Spawn(Box::new(spawn))) {
        response::Kind::Error(_) => {}
        other => panic!("expected a spawn error, got {other:?}"),
    }
    let log = wait_log(dir.path(), &["spawn failed"]);
    assert!(
        !log.contains("sentinel-path-77"),
        "env value leaked:\n{log}"
    );
    assert_lines_are_events(&log);
}

#[test]
fn read_logs_tail_spans_rotated_files() {
    let dir = TempDir::new().unwrap();
    let logs = dir.path().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let lines = |tag: &str| (0..5).map(|i| format!("{tag}-{i}\n")).collect::<String>();
    std::fs::write(logs.join("fleetd.log.2"), lines("rotated2")).unwrap();
    std::fs::write(logs.join("fleetd.log.1"), lines("rotated1")).unwrap();
    let _d = start(dir.path(), &[]);
    wait_log(dir.path(), &["fleetd started"]);
    let mut c = Client::open(dir.path());

    let all = c.read_logs(10_000, false);
    assert_eq!(all[0], "rotated2-0");
    assert_eq!(all[4], "rotated2-4");
    assert_eq!(all[5], "rotated1-0");
    assert_eq!(all[9], "rotated1-4");
    assert!(all[10..].iter().any(|l| l.contains("fleetd started")));
    // The last N lines, oldest first; capped at 10 000.
    let two = c.read_logs(2, false);
    assert_eq!(two.len(), 2);
    assert!(!two[0].starts_with("rotated"));
    assert_eq!(c.read_logs(u32::MAX, false)[0], "rotated2-0");
}

/// The name a `fleet-client` connection hands over appears in the daemon's
/// connection line.
#[test]
fn client_name_is_logged() {
    let dir = TempDir::new().unwrap();
    let _d = start(dir.path(), &[]);
    let _conn = fleet_client::Connection::unix(&dir.path().join("fleetd.sock"), "gui-pane-7")
        .expect("connect");
    wait_log(dir.path(), &["connection opened", "client=\"gui-pane-7\""]);
}

#[test]
fn follow_streams_new_lines() {
    let dir = TempDir::new().unwrap();
    let _d = start(dir.path(), &[]);
    let mut follower = Client::open(dir.path());
    assert!(follower.read_logs(0, true).is_empty());
    // A new connection is logged, and streamed to the follower.
    let _other = Client::open(dir.path());
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let line = follower
            .next_log_line(deadline.saturating_duration_since(Instant::now()))
            .expect("no LogLine for the new connection");
        if line.contains("connection opened") && line.contains("logging-test") {
            break;
        }
    }
    // `StopLogs` ends the stream.
    match follower.request(request::Kind::StopLogs(StopLogs {})) {
        response::Kind::Ack(_) => {}
        other => panic!("stop: {other:?}"),
    }
    let _third = Client::open(dir.path());
    assert_eq!(follower.next_log_line(Duration::from_millis(500)), None);
}

#[test]
fn slow_follower_gets_dropped_marker_not_disconnect() {
    let dir = TempDir::new().unwrap();
    let _d = start(dir.path(), &[]);
    let mut slow = Client::open(dir.path());
    slow.read_logs(0, true);
    // Many log lines (two per connection) while the follower does not read:
    // far more than its queue, the outbox and the socket buffers hold.
    for _ in 0..2500 {
        drop(Client::open(dir.path()));
    }
    // Catch up. The marker rides on the first line offered once the
    // follower's queue has room again: a late line of the flood (the last
    // "connection closed") while we read, or else a line we cause. So every
    // line read counts, and while the stream is idle each new connection
    // offers one more line, until the marker comes.
    let deadline = Instant::now() + TIMEOUT;
    let mut extra = Vec::new();
    loop {
        assert!(
            Instant::now() < deadline,
            "no drop marker within {TIMEOUT:?} after the flood"
        );
        match slow.next_log_line(Duration::from_millis(300)) {
            Some(line) if line.contains("lines dropped") => break,
            Some(_) => {}
            None => extra.push(Client::open(dir.path())),
        }
    }
    // Still connected and served.
    match slow.request(request::Kind::StopLogs(StopLogs {})) {
        response::Kind::Ack(_) => {}
        other => panic!("stop: {other:?}"),
    }
}

#[test]
fn stdio_bridge_never_writes_logs_to_stdout() {
    let dir = TempDir::new().unwrap();
    let _d = start(dir.path(), &[]);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fleetd"));
    cmd.env("FLEETD_LOGIN_ENV", "off")
        .env("FLEETD_LOG", "trace")
        .arg("--stdio")
        .arg("--state-dir")
        .arg(dir.path())
        .stderr(Stdio::null());
    let (mut conn, mut child) =
        fleet_client::Connection::command(cmd, "logging-test").expect("handshake via --stdio");
    match conn
        .call(request::Kind::List(Default::default()))
        .expect("list")
    {
        response::Kind::Agents(_) => {}
        other => panic!("unexpected {other:?}"),
    }
    drop(conn);
    let _ = child.wait();
    // The bridge logs to its own file.
    let bridge_log = dir.path().join("logs").join("fleetd-stdio.log");
    let log = std::fs::read_to_string(&bridge_log).unwrap_or_default();
    assert!(log.contains("stdio bridge"), "bridge log:\n{log}");
    assert_eq!(mode(&bridge_log), 0o600);
}
