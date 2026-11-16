//! Identity over the wire (ADR 0072): a token in `Hello` makes the
//! connection that agent, and a refused agent spawn reaches every
//! connection as `SpawnRefused`.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fleet_client::Connection;
use fleet_proto::{Frame, ListAgents, SpawnAgent, event, request, response};

struct Daemon(Child);

impl Daemon {
    fn start(dir: &Path) -> Self {
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

fn spawn(c: &mut Connection, script: &str) -> u64 {
    let req = SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        ..Default::default()
    };
    match c.call(request::Kind::Spawn(Box::new(req))) {
        Ok(response::Kind::Spawned(info)) => info.id,
        other => panic!("spawn: {other:?}"),
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

#[test]
fn a_token_identifies_the_connection_and_refusals_are_broadcast() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "[node]\nmax_children = 0\n").unwrap();
    let _d = Daemon::start(dir.path());
    let sock = dir.path().join("fleetd.sock");
    let mut user = Connection::unix(&sock, "user").unwrap();
    let tok = dir.path().join("a.tok");
    let a = spawn(
        &mut user,
        &format!(
            "printf %s \"$FLEET_AGENT_TOKEN\" > {0}.tmp && mv {0}.tmp {0}; exec sleep 600",
            tok.display()
        ),
    );
    let token = read_when_there(&tok);

    // The test's own session leads no agent: the token alone makes it A.
    let mut agent = Connection::unix_as(&sock, "agent", Some(&token)).unwrap();
    match agent.call(request::Kind::List(ListAgents::default())) {
        Ok(response::Kind::Agents(list)) => assert!(list.agents.is_empty(), "{list:?}"),
        other => panic!("list: {other:?}"),
    }
    let req = SpawnAgent {
        program: "/bin/sh".into(),
        ..Default::default()
    };
    let err = agent.call(request::Kind::Spawn(Box::new(req))).unwrap_err();
    assert!(err.is_forbidden(), "{err}");

    // The user's connection hears of it (other events may come first).
    let (mut reader, _sender) = user.split();
    let refused = loop {
        match reader.read_frame().unwrap() {
            Some(Frame::Event(e)) => {
                if let Some(event::Kind::SpawnRefused(r)) = e.kind {
                    break r;
                }
            }
            other => panic!("unexpected {other:?}"),
        }
    };
    assert_eq!(refused.parent, a);
    assert!(
        refused.reason.contains("max_children = 0"),
        "{}",
        refused.reason
    );
}

/// Reads `conn`'s frames on a thread of their own.
fn frames(conn: Connection) -> std::sync::mpsc::Receiver<Frame> {
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

/// The agent an event is about, and what kind it is.
fn about(f: &Frame) -> Option<(u64, &'static str)> {
    let Frame::Event(e) = f else { return None };
    Some(match e.kind.as_ref()? {
        event::Kind::Activity(a) => (a.agent_id, "activity"),
        event::Kind::Exited(x) => (x.agent_id, "exited"),
        event::Kind::AgentRemoved(r) => (r.agent_id, "removed"),
        event::Kind::SpawnRefused(r) => (r.parent, "refused"),
        _ => return None,
    })
}

#[test]
fn an_agents_events_are_its_descendants_only() {
    let dir = tempfile::tempdir().unwrap();
    let _d = Daemon::start(dir.path());
    let sock = dir.path().join("fleetd.sock");
    let mut user = Connection::unix(&sock, "user").unwrap();
    let tok = dir.path().join("p.tok");
    let p = spawn(
        &mut user,
        &format!(
            "printf %s \"$FLEET_AGENT_TOKEN\" > {0}.tmp && mv {0}.tmp {0}; exec sleep 600",
            tok.display()
        ),
    );
    let token = read_when_there(&tok);
    let mut agent = Connection::unix_as(&sock, "agent", Some(&token)).unwrap();
    // P's child, and the user's own agent, each print and exit.
    let child = spawn(&mut agent, "echo child-out; sleep 1; exit 4");
    let other = spawn(&mut user, "echo user-secret; sleep 1; exit 5");
    let events = frames(agent);

    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let wait_for = |seen: &mut Vec<(u64, &'static str)>, want: (u64, &'static str)| {
        while !seen.contains(&want) {
            let left = deadline.saturating_duration_since(Instant::now());
            match events.recv_timeout(left) {
                Ok(f) => seen.extend(about(&f)),
                Err(_) => panic!("no {want:?}; saw {seen:?}"),
            }
        }
    };
    wait_for(&mut seen, (child, "activity"));
    wait_for(&mut seen, (child, "exited"));
    // Both have exited (the user's started after the child): forget them.
    let gone = Instant::now() + Duration::from_secs(10);
    for id in [other, child] {
        loop {
            let req = fleet_proto::ForgetAgents {
                ids: vec![id],
                ..Default::default()
            };
            match user.call(request::Kind::Forget(Box::new(req))).unwrap() {
                response::Kind::Forgotten(f) if f.refused.is_empty() => break,
                _ => assert!(Instant::now() < gone, "agent {id} did not exit"),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    wait_for(&mut seen, (child, "removed"));
    std::thread::sleep(Duration::from_millis(300));
    seen.extend(events.try_iter().filter_map(|f| about(&f)));
    assert!(
        seen.iter().all(|&(id, _)| id == child),
        "only the child's events (not {other}'s or {p}'s): {seen:?}"
    );
}
