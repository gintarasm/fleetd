//! Restart and forget over the wire: a real `fleetd` binary on a temp state
//! dir, two client connections, `AgentRemoved` fan-out and ordering.

use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use fleet_proto::{
    AgentInfo, AgentList, AgentRemoved, ForgetAgents, Forgotten, Frame, FrameCodec, Hello,
    ListAgents, PROTOCOL_VERSION, Request, RestartAgent, Restarted, SpawnAgent, event, request,
    response,
};
use tempfile::TempDir;
use tokio_util::codec::{Decoder, Encoder};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Daemon {
    child: Child,
}

impl Daemon {
    fn start(dir: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_fleetd"))
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
    /// Connects and completes the handshake.
    fn open(dir: &Path) -> Self {
        let stream = UnixStream::connect(dir.join("fleetd.sock")).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut c = Client {
            stream,
            buf: BytesMut::new(),
            pending: VecDeque::new(),
            next_id: 1,
        };
        c.send(Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client: "lifecycle-test".into(),
        }));
        match c.next_frame() {
            Some(Frame::HelloOk(_)) => {}
            other => panic!("expected HelloOk, got {other:?}"),
        }
        c
    }

    fn send(&mut self, frame: Frame) {
        let mut out = BytesMut::new();
        FrameCodec::default().encode(frame, &mut out).unwrap();
        self.stream.write_all(&out).unwrap();
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

    /// Next frame, buffered ones first.
    fn next_frame(&mut self) -> Option<Frame> {
        self.pending.pop_front().or_else(|| self.read_frame())
    }

    /// Sends a request and returns its response; frames before it are kept
    /// in `pending`, so anything read after this follows the response.
    fn request(&mut self, kind: request::Kind) -> response::Kind {
        let id = self.next_id;
        self.next_id += 1;
        self.send(Frame::Request(Request {
            id,
            kind: Some(kind),
        }));
        loop {
            match self.read_frame() {
                Some(Frame::Response(fleet_proto::Response { id: rid, kind })) if rid == id => {
                    return kind.expect("response kind");
                }
                Some(other) => self.pending.push_back(other),
                None => panic!("connection closed awaiting response {id}"),
            }
        }
    }

    fn sh(&mut self, script: &str) -> AgentInfo {
        match self.request(request::Kind::Spawn(Box::new(SpawnAgent {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cols: 80,
            rows: 24,
            ..Default::default()
        }))) {
            response::Kind::Spawned(info) => *info,
            other => panic!("spawn failed: {other:?}"),
        }
    }

    fn list_full(&mut self) -> AgentList {
        match self.request(request::Kind::List(ListAgents::default())) {
            response::Kind::Agents(list) => list,
            other => panic!("list failed: {other:?}"),
        }
    }

    fn list(&mut self) -> Vec<AgentInfo> {
        self.list_full().agents
    }

    fn restart(&mut self, agent_id: u64, attach: bool) -> response::Kind {
        self.request(request::Kind::Restart(RestartAgent {
            agent_id,
            cols: 80,
            rows: 24,
            attach,
        }))
    }

    fn forget(&mut self, ids: &[u64], exited_only: bool) -> Forgotten {
        match self.request(request::Kind::Forget(ForgetAgents {
            ids: ids.to_vec(),
            exited_only,
            selector: HashMap::new(),
        })) {
            response::Kind::Forgotten(f) => f,
            other => panic!("forget failed: {other:?}"),
        }
    }

    /// The next `AgentRemoved` frame, skipping activity/exit/output frames.
    fn removed_event(&mut self) -> AgentRemoved {
        loop {
            match self.next_frame() {
                Some(Frame::Event(fleet_proto::Event {
                    kind: Some(event::Kind::AgentRemoved(r)),
                })) => return r,
                Some(_) => {}
                None => panic!("closed before AgentRemoved"),
            }
        }
    }

    /// The `AgentRemoved` events buffered before the last response.
    fn pending_removed(&self) -> Vec<AgentRemoved> {
        self.pending
            .iter()
            .filter_map(|f| match f {
                Frame::Event(fleet_proto::Event {
                    kind: Some(event::Kind::AgentRemoved(r)),
                }) => Some(*r),
                _ => None,
            })
            .collect()
    }
}

/// Waits for the agent's exit event (read now or earlier). The node sends
/// it once the agent is retired, unlike the list, whose row says exited a
/// moment before restart and forget stop refusing the agent as running.
fn wait_not_running(c: &mut Client, id: u64) {
    let is_exit = |f: &Frame| {
        matches!(f, Frame::Event(fleet_proto::Event {
            kind: Some(event::Kind::Exited(e)),
        }) if e.agent_id == id)
    };
    if let Some(i) = c.pending.iter().position(is_exit) {
        c.pending.remove(i);
        return;
    }
    loop {
        match c.read_frame() {
            Some(f) if is_exit(&f) => return,
            Some(f) => c.pending.push_back(f),
            None => panic!("closed before agent {id} exited"),
        }
    }
}

fn restarted(r: response::Kind) -> Restarted {
    match r {
        response::Kind::Restarted(r) => *r,
        other => panic!("restart failed: {other:?}"),
    }
}

fn sorted(mut v: Vec<u64>) -> Vec<u64> {
    v.sort_unstable();
    v
}

#[test]
fn restart_over_the_wire() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut a = Client::open(dir.path());
    let mut b = Client::open(dir.path());
    let old = a.sh("exit 0").id;
    wait_not_running(&mut a, old);

    let r = restarted(a.restart(old, false));
    assert_eq!(r.old_id, old);
    let new = r.agent.expect("restarted agent").id;
    assert_ne!(new, old);
    assert!(r.attached.is_none());
    assert!(
        a.pending_removed().is_empty(),
        "AgentRemoved must follow the response"
    );
    let removed = AgentRemoved {
        agent_id: old,
        replaced_by: Some(new),
    };
    assert_eq!(a.removed_event(), removed);
    assert_eq!(b.removed_event(), removed);

    let list = a.list_full();
    assert_eq!(list.replaced, HashMap::from([(old, new)]));
    assert!(list.agents.iter().all(|x| x.id != old));
    assert!(list.agents.iter().any(|x| x.id == new));
    // The requester got exactly one AgentRemoved.
    assert!(a.pending_removed().is_empty());
}

#[test]
fn restart_attached_over_the_wire() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut a = Client::open(dir.path());
    let old = a.sh("echo lifecycle-marker; sleep 0.3").id;
    wait_not_running(&mut a, old);

    let r = restarted(a.restart(old, true));
    assert_eq!(r.old_id, old);
    let new = r.agent.expect("restarted agent").id;
    assert!(r.attached.is_some(), "attach: true sets Restarted.attached");
    assert!(a.pending_removed().is_empty());
    let is_new_output = |f: &Frame| matches!(f, Frame::Output { agent_id, .. } if *agent_id == new);
    let is_new_exit = |f: &Frame| {
        matches!(f, Frame::Event(fleet_proto::Event {
            kind: Some(event::Kind::Exited(e)),
        }) if e.agent_id == new)
    };
    assert!(
        !a.pending.iter().any(|f| is_new_output(f) || is_new_exit(f)),
        "nothing of the new agent precedes the reply: {:?}",
        a.pending
    );
    a.pending.clear();

    // In order: our AgentRemoved, then the new agent's output, then its exit.
    let mut removed_seen = false;
    let mut out = Vec::new();
    loop {
        match a.next_frame() {
            Some(Frame::Event(fleet_proto::Event {
                kind: Some(event::Kind::AgentRemoved(r)),
            })) => {
                assert_eq!(
                    r,
                    AgentRemoved {
                        agent_id: old,
                        replaced_by: Some(new)
                    }
                );
                assert!(!removed_seen, "one AgentRemoved");
                assert!(out.is_empty(), "AgentRemoved precedes the new output");
                removed_seen = true;
            }
            Some(Frame::Output { agent_id, data }) if agent_id == new => {
                assert!(removed_seen, "AgentRemoved precedes the new output");
                out.extend_from_slice(&data);
            }
            Some(f) if is_new_exit(&f) => break,
            Some(_) => {}
            None => panic!("closed before the new agent exited"),
        }
    }
    assert!(String::from_utf8_lossy(&out).contains("lifecycle-marker"));
    // No second exit: the exit-event forwarder skipped the attached agent.
    std::thread::sleep(Duration::from_millis(300));
    a.list();
    assert!(!a.pending.iter().any(is_new_exit), "{:?}", a.pending);
}

#[test]
fn restart_unknown_is_an_error() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut a = Client::open(dir.path());
    match a.restart(9999, false) {
        response::Kind::Error(e) => assert!(e.message.contains("unknown agent"), "{}", e.message),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn restart_whose_spawn_fails_keeps_the_old_agent() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let bin = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut a = Client::open(dir.path());
    let program = bin.path().join("agent.sh");
    std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let old = match a.request(request::Kind::Spawn(Box::new(SpawnAgent {
        program: program.to_string_lossy().into_owned(),
        cwd: bin.path().to_string_lossy().into_owned(),
        ..Default::default()
    }))) {
        response::Kind::Spawned(info) => info.id,
        other => panic!("spawn failed: {other:?}"),
    };
    wait_not_running(&mut a, old);
    std::fs::remove_file(&program).unwrap();
    for attach in [false, true] {
        match a.restart(old, attach) {
            response::Kind::Error(_) => {}
            other => panic!("expected Error, got {other:?}"),
        }
        let list = a.list_full();
        assert!(a.pending_removed().is_empty());
        assert!(list.replaced.is_empty());
        let ids: Vec<u64> = list.agents.iter().map(|x| x.id).collect();
        assert_eq!(ids, vec![old]);
    }
}

#[test]
fn forget_over_the_wire() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut a = Client::open(dir.path());
    let mut b = Client::open(dir.path());
    let e1 = a.sh("exit 0").id;
    let e2 = a.sh("exit 3").id;
    let running = a.sh("sleep 100").id;
    wait_not_running(&mut a, e1);
    wait_not_running(&mut a, e2);

    let f = a.forget(&[], true);
    assert_eq!(sorted(f.ids), vec![e1, e2]);
    assert!(f.refused.is_empty());
    assert!(a.pending_removed().is_empty());

    let got = |c: &mut Client| sorted(vec![c.removed_event().agent_id, c.removed_event().agent_id]);
    assert_eq!(got(&mut a), vec![e1, e2]);
    assert_eq!(got(&mut b), vec![e1, e2]);

    let ids: Vec<u64> = a.list().iter().map(|x| x.id).collect();
    assert_eq!(ids, vec![running]);
}

#[test]
fn forget_ids_reports_refused() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut a = Client::open(dir.path());
    let running = a.sh("sleep 100").id;
    let f = a.forget(&[running, 9999], false);
    assert!(f.ids.is_empty());
    assert_eq!(f.refused, vec![running, 9999]);
    assert!(a.list().iter().any(|x| x.id == running));
}

#[test]
fn restart_running_is_an_error() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut a = Client::open(dir.path());
    let running = a.sh("sleep 100").id;
    match a.restart(running, false) {
        response::Kind::Error(e) => assert!(e.message.contains("is running"), "{}", e.message),
        other => panic!("expected Error, got {other:?}"),
    }
}

/// Runs `sql` on a second connection to the daemon's state DB (e.g. a
/// trigger that makes a store write fail).
fn inject(dir: &Path, sql: &str) {
    rusqlite::Connection::open(dir.join("state.db"))
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

#[test]
fn restart_whose_record_fails_announces_no_removal() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut a = Client::open(dir.path());
    let old = a.sh("exit 0").id;
    wait_not_running(&mut a, old);
    inject(
        dir.path(),
        "CREATE TRIGGER no_replace BEFORE INSERT ON agent_replacements \
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    );
    let new = restarted(a.restart(old, false)).agent.unwrap().id;
    // Anything queued after the reply precedes the list's reply.
    let list = a.list_full();
    assert!(a.pending_removed().is_empty(), "{:?}", a.pending_removed());
    assert!(list.replaced.is_empty());
    let ids: Vec<u64> = list.agents.iter().map(|x| x.id).collect();
    assert!(ids.contains(&old) && ids.contains(&new), "{ids:?}");
}

#[test]
fn forget_that_fails_midway_announces_what_it_forgot() {
    let dir = TempDir::new().unwrap();
    let _d = Daemon::start(dir.path());
    let mut a = Client::open(dir.path());
    let mut b = Client::open(dir.path());
    let e1 = a.sh("exit 0").id;
    let e2 = a.sh("exit 0").id;
    wait_not_running(&mut a, e1);
    wait_not_running(&mut a, e2);
    inject(
        dir.path(),
        &format!(
            "CREATE TRIGGER no_forget BEFORE DELETE ON agents WHEN OLD.id = {e2} \
             BEGIN SELECT RAISE(ABORT, 'injected'); END;"
        ),
    );
    match a.request(request::Kind::Forget(ForgetAgents {
        ids: vec![e1, e2],
        exited_only: false,
        selector: HashMap::new(),
    })) {
        response::Kind::Error(e) => assert!(e.message.contains("injected"), "{}", e.message),
        other => panic!("expected Error, got {other:?}"),
    }
    let removed = AgentRemoved {
        agent_id: e1,
        replaced_by: None,
    };
    // The requester is told of what was forgotten, after the reply.
    assert!(a.pending_removed().is_empty());
    let ids: Vec<u64> = a.list().iter().map(|x| x.id).collect();
    assert_eq!(a.pending_removed(), vec![removed]);
    assert_eq!(ids, vec![e2]);
    assert_eq!(b.removed_event(), removed);
}
