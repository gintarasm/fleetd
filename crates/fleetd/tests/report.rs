//! `fleet report` over the wire (ADR 0075): identity decides who may report,
//! and the event is scoped like the other agent events.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fleet_client::Connection;
use fleet_proto::{
    Frame, ListAgents, ReportStatus, ReportedState, SpawnAgent, event, request, response,
};

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

const TOKEN_SCRIPT: &str =
    "printf %s \"$FLEET_AGENT_TOKEN\" > {0}.tmp && mv {0}.tmp {0}; exec sleep 600";

fn spawn_with_token(c: &mut Connection, dir: &Path, name: &str) -> (u64, String) {
    let tok = dir.join(format!("{name}.tok"));
    let id = spawn(c, &TOKEN_SCRIPT.replace("{0}", &tok.display().to_string()));
    (id, read_when_there(&tok))
}

fn report(state: ReportedState, message: &str) -> request::Kind {
    request::Kind::Report(ReportStatus {
        state: state.into(),
        message: message.into(),
    })
}

/// The `AgentReported` events in `rx` within `wait`.
fn reports(
    rx: &std::sync::mpsc::Receiver<Frame>,
    wait: Duration,
) -> Vec<fleet_proto::AgentReported> {
    let deadline = Instant::now() + wait;
    let mut out = Vec::new();
    while let Ok(f) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if let Frame::Event(e) = f
            && let Some(event::Kind::Reported(r)) = e.kind
        {
            out.push(r);
        }
    }
    out
}

#[test]
fn only_an_agent_reports_and_its_ancestors_and_the_user_hear_it() {
    let dir = tempfile::tempdir().unwrap();
    let _d = Daemon::start(dir.path());
    let sock = dir.path().join("fleetd.sock");
    let mut user = Connection::unix(&sock, "user").unwrap();

    // The user has no agent to report for.
    let err = user
        .call(report(ReportedState::ReportedWorking, "hi"))
        .unwrap_err();
    assert!(err.is_forbidden(), "{err}");

    let (p, p_token) = spawn_with_token(&mut user, dir.path(), "p");
    let (stranger, s_token) = spawn_with_token(&mut user, dir.path(), "s");
    let mut parent = Connection::unix_as(&sock, "p", Some(&p_token)).unwrap();
    // P spawns C, which writes its token.
    let tok = dir.path().join("c.tok");
    let c = spawn(
        &mut parent,
        &TOKEN_SCRIPT.replace("{0}", &tok.display().to_string()),
    );
    let c_token = read_when_there(&tok);

    let user_events = frames(Connection::unix(&sock, "watch").unwrap());
    let parent_events = frames(Connection::unix_as(&sock, "p2", Some(&p_token)).unwrap());
    let stranger_events = frames(Connection::unix_as(&sock, "s2", Some(&s_token)).unwrap());

    let mut child = Connection::unix_as(&sock, "c", Some(&c_token)).unwrap();
    child
        .call(report(ReportedState::ReportedWaiting, "need a review"))
        .unwrap();
    // A message over 200 characters is an error, not forbidden.
    let err = child
        .call(report(ReportedState::ReportedWorking, &"x".repeat(201)))
        .unwrap_err();
    assert!(!err.is_forbidden(), "{err}");

    for (who, rx) in [("user", &user_events), ("parent", &parent_events)] {
        let got = reports(rx, Duration::from_millis(1500));
        assert_eq!(got.len(), 1, "{who}: {got:?}");
        assert_eq!(got[0].agent_id, c);
        let r = got[0].reported.as_ref().unwrap();
        assert_eq!(
            (r.state(), r.message.as_str()),
            (ReportedState::ReportedWaiting, "need a review")
        );
    }
    assert!(reports(&stranger_events, Duration::from_millis(500)).is_empty());

    // The list carries it, for the user and for the parent's scope.
    for conn in [&mut user, &mut parent] {
        match conn
            .call(request::Kind::List(ListAgents::default()))
            .unwrap()
        {
            response::Kind::Agents(l) => {
                let a = l.agents.iter().find(|a| a.id == c).expect("c listed");
                assert_eq!(a.reported.as_ref().unwrap().message, "need a review");
            }
            other => panic!("list: {other:?}"),
        }
    }
    let _ = (p, stranger);
}
