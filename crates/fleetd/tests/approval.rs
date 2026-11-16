//! Spawn approval over the wire (ADR 0079): with `[node] spawn_approval =
//! true` an agent's spawn waits for the user, who sees it, and approves or
//! denies it; it expires otherwise. The user's own spawns never wait.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use fleet_client::Connection;
use fleet_proto::{
    DecideSpawn, Frame, ListPendingSpawns, PendingSpawn, SpawnAgent, SpawnDecided, SpawnDecision,
    event, request, response,
};

struct Daemon(Child);

impl Daemon {
    fn start(dir: &Path, config: &str) -> Self {
        std::fs::write(dir.join("config.toml"), config).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_fleetd"))
            .env("FLEETD_LOGIN_ENV", "off")
            .arg("--state-dir")
            .arg(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fleetd");
        let sock = dir.join("fleetd.sock");
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(&sock).is_err() {
            assert!(Instant::now() < deadline, "daemon not ready within 5s");
            std::thread::sleep(Duration::from_millis(5));
        }
        Daemon(child)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn read_when_there(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(s) = std::fs::read_to_string(path) {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn spawn_req(labels: &[(&str, &str)]) -> SpawnAgent {
    SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exec sleep 600".into()],
        cwd: "/tmp".into(),
        labels: labels
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect(),
        ..Default::default()
    }
}

/// The test's setup: a daemon, the user's connections, and agent P with
/// its token.
struct Fleet {
    _dir: tempfile::TempDir,
    _daemon: Daemon,
    sock: std::path::PathBuf,
    /// The user's connection for calls.
    user: Connection,
    /// The events of another of the user's connections.
    user_events: Receiver<Frame>,
    p: u64,
    token: String,
}

impl Fleet {
    fn start(config: &str) -> Fleet {
        let dir = tempfile::tempdir().unwrap();
        let daemon = Daemon::start(dir.path(), config);
        let sock = dir.path().join("fleetd.sock");
        let mut user = Connection::unix(&sock, "user").unwrap();
        let user_events = frames(Connection::unix(&sock, "user-events").unwrap());
        let tok = dir.path().join("p.tok");
        let mut req = spawn_req(&[("team", "x")]);
        req.args[1] = format!(
            "printf %s \"$FLEET_AGENT_TOKEN\" > {0}.tmp && mv {0}.tmp {0}; exec sleep 600",
            tok.display()
        );
        // The user's spawn never waits, approval on or not.
        let p = match user.call(request::Kind::Spawn(Box::new(req))) {
            Ok(response::Kind::Spawned(info)) => info.id,
            other => panic!("spawn: {other:?}"),
        };
        let token = read_when_there(&tok);
        Fleet {
            _dir: dir,
            _daemon: daemon,
            sock,
            user,
            user_events,
            p,
            token,
        }
    }

    fn agent(&self) -> Connection {
        Connection::unix_as(&self.sock, "agent", Some(&self.token)).unwrap()
    }

    /// Spawns as P on a thread of its own: the call blocks until decided.
    fn agent_spawn(
        &self,
        labels: &[(&str, &str)],
    ) -> std::thread::JoinHandle<fleet_client::Result<response::Kind>> {
        let mut agent = self.agent();
        let req = spawn_req(labels);
        std::thread::spawn(move || agent.call(request::Kind::Spawn(Box::new(req))))
    }

    fn pending(&mut self) -> Vec<PendingSpawn> {
        match self
            .user
            .call(request::Kind::ListPendingSpawns(ListPendingSpawns {}))
        {
            Ok(response::Kind::PendingSpawns(p)) => p.pending,
            other => panic!("list pending: {other:?}"),
        }
    }

    fn decide(&mut self, id: u64, approve: bool) -> fleet_client::Result<response::Kind> {
        self.user
            .call(request::Kind::DecideSpawn(DecideSpawn { id, approve }))
    }

    /// The next pending spawn the user's event connection hears of.
    fn next_pending(&self) -> PendingSpawn {
        next_event(&self.user_events, |k| match k {
            event::Kind::SpawnPending(p) => Some(p),
            _ => None,
        })
    }

    fn next_decided(&self) -> SpawnDecided {
        next_event(&self.user_events, |k| match k {
            event::Kind::SpawnDecided(d) => Some(d),
            _ => None,
        })
    }
}

/// Reads `conn`'s frames on a thread of their own.
fn frames(conn: Connection) -> Receiver<Frame> {
    let (tx, rx) = std::sync::mpsc::channel();
    let (mut reader, sender) = conn.split();
    std::thread::spawn(move || {
        let _keep = sender;
        while let Ok(Some(f)) = reader.read_frame() {
            if tx.send(f).is_err() {
                return;
            }
        }
    });
    rx
}

fn next_event<T>(rx: &Receiver<Frame>, pick: impl Fn(event::Kind) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Frame::Event(e)) => {
                if let Some(t) = e.kind.and_then(&pick) {
                    return t;
                }
            }
            Ok(_) => {}
            Err(e) => panic!("no such event: {e:?}"),
        }
    }
}

#[test]
fn an_approved_spawn_starts_as_the_agents_child() {
    let mut f = Fleet::start("[node]\nspawn_approval = true\n");
    let asked = f.agent_spawn(&[("role", "child")]);
    let pending = f.next_pending();
    assert_eq!(pending.parent, f.p);
    assert_eq!(pending.program, "/bin/sh");
    assert_eq!(pending.args, vec!["-c", "exec sleep 600"]);
    assert_eq!(pending.cwd, "/tmp");
    assert_eq!(pending.labels.get("team").map(String::as_str), Some("x"));
    assert_eq!(
        pending.labels.get("role").map(String::as_str),
        Some("child")
    );
    let wait = pending.expires_unix_ms - pending.asked_unix_ms;
    assert_eq!(wait, 300_000, "five minutes by default");
    assert_eq!(f.pending(), vec![pending.clone()]);
    assert!(!asked.is_finished(), "the agent's spawn waits");

    assert!(matches!(
        f.decide(pending.id, true),
        Ok(response::Kind::Ack(_))
    ));
    let child = match asked.join().unwrap() {
        Ok(response::Kind::Spawned(info)) => info,
        other => panic!("approved spawn: {other:?}"),
    };
    let p = f.p.to_string();
    assert_eq!(
        child.labels.get("fleet.parent").map(String::as_str),
        Some(p.as_str())
    );
    assert_eq!(
        f.next_decided(),
        SpawnDecided {
            id: pending.id,
            decision: SpawnDecision::SpawnApproved.into()
        }
    );
    assert!(f.pending().is_empty());
    let again = f.decide(pending.id, true).unwrap_err();
    assert!(again.to_string().contains("no pending spawn"), "{again}");
}

#[test]
fn a_denied_spawn_is_forbidden_and_agents_cannot_decide() {
    let mut f = Fleet::start("[node]\nspawn_approval = true\n");
    // Another connection of P's hears nothing of approvals.
    let mut watcher = f.agent();
    let agent_events = {
        let w = f.agent();
        frames(w)
    };
    let asked = f.agent_spawn(&[]);
    let pending = f.next_pending();

    let e = watcher
        .call(request::Kind::ListPendingSpawns(ListPendingSpawns {}))
        .unwrap_err();
    assert!(e.is_forbidden(), "{e}");
    let e = watcher
        .call(request::Kind::DecideSpawn(DecideSpawn {
            id: pending.id,
            approve: true,
        }))
        .unwrap_err();
    assert!(e.is_forbidden(), "{e}");

    assert!(matches!(
        f.decide(pending.id, false),
        Ok(response::Kind::Ack(_))
    ));
    let e = asked.join().unwrap().unwrap_err();
    assert!(e.is_forbidden(), "{e}");
    assert_eq!(e.to_string(), "forbidden: spawn not approved");
    assert_eq!(
        f.next_decided().decision,
        i32::from(SpawnDecision::SpawnDenied)
    );
    // The user's side has seen both events by now; the agent's had time to.
    loop {
        match agent_events.recv_timeout(Duration::from_millis(200)) {
            Ok(Frame::Event(e)) => assert!(
                !matches!(
                    e.kind,
                    Some(event::Kind::SpawnPending(_) | event::Kind::SpawnDecided(_))
                ),
                "an agent saw {e:?}"
            ),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => break,
            Err(e) => panic!("{e:?}"),
        }
    }
}

#[test]
fn an_unanswered_spawn_expires_and_a_closed_one_is_withdrawn() {
    let mut f = Fleet::start("[node]\nspawn_approval = true\nspawn_approval_timeout = 1\n");
    let started = Instant::now();
    let e = f.agent_spawn(&[]).join().unwrap().unwrap_err();
    assert!(e.is_forbidden(), "{e}");
    assert_eq!(
        e.to_string(),
        "forbidden: spawn not approved (no answer within 1s)"
    );
    assert!(started.elapsed() >= Duration::from_millis(900));
    f.next_pending();
    assert_eq!(
        f.next_decided().decision,
        i32::from(SpawnDecision::SpawnExpired)
    );
    assert!(f.pending().is_empty());

    // An agent's connection that closes withdraws its pending spawn.
    let (_, mut sender) = f.agent().split();
    sender
        .send(request::Kind::Spawn(Box::new(spawn_req(&[]))))
        .unwrap();
    let pending = f.next_pending();
    drop(sender);
    let decided = f.next_decided();
    assert_eq!(decided.id, pending.id);
    assert_eq!(decided.decision, i32::from(SpawnDecision::SpawnWithdrawn));
}

#[test]
fn quotas_come_first_and_approval_is_off_by_default() {
    let mut f = Fleet::start("[node]\nspawn_approval = true\nmax_children = 0\n");
    let e = f.agent_spawn(&[]).join().unwrap().unwrap_err();
    assert!(e.to_string().contains("quota max_children"), "{e}");
    assert!(f.pending().is_empty(), "a refused spawn never waits");

    let f = Fleet::start("");
    match f.agent_spawn(&[]).join().unwrap() {
        Ok(response::Kind::Spawned(_)) => {}
        other => panic!("spawn without approval: {other:?}"),
    }
}

#[test]
fn a_daemon_restart_refuses_pending_spawns() {
    let mut f = Fleet::start("[node]\nspawn_approval = true\n");
    let asked = f.agent_spawn(&[]);
    f.next_pending();
    match f.user.call(request::Kind::RestartDaemon(
        fleet_proto::RestartDaemon::default(),
    )) {
        Ok(response::Kind::RestartingDaemon(_)) => {}
        other => panic!("restart: {other:?}"),
    }
    // Refused before the exec; if the answer did not make it out first,
    // the connection ends instead. Either way nothing was spawned.
    match asked.join().unwrap() {
        Err(e) if e.is_forbidden() => assert_eq!(
            e.to_string(),
            "forbidden: spawn not approved (fleetd is restarting)"
        ),
        Err(_) => {}
        Ok(r) => panic!("spawned across a restart: {r:?}"),
    }
    // The new daemon (the old one may still answer "restarting" briefly).
    let before = f.user.hello().instance;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut user = loop {
        if let Ok(c) = Connection::unix(&f.sock, "user")
            && c.hello().instance != before
        {
            break c;
        }
        assert!(Instant::now() < deadline, "no new daemon after the restart");
        std::thread::sleep(Duration::from_millis(20));
    };
    match user.call(request::Kind::ListPendingSpawns(ListPendingSpawns {})) {
        Ok(response::Kind::PendingSpawns(p)) => assert!(p.pending.is_empty(), "{p:?}"),
        other => panic!("list pending: {other:?}"),
    }
    match user.call(request::Kind::List(fleet_proto::ListAgents::default())) {
        Ok(response::Kind::Agents(list)) => assert_eq!(list.agents.len(), 1, "P only"),
        other => panic!("list: {other:?}"),
    }
}
