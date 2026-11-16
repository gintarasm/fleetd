//! `ListMessageEdges` over the wire (ADR 0089): the user reads who wrote to
//! whom, an agent is refused.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fleet_client::Connection;
use fleet_proto::{ListMessageEdges, SendMessage, SpawnAgent, request, response};

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
fn the_user_lists_message_edges_and_an_agent_is_refused() {
    let dir = tempfile::tempdir().unwrap();
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
    let mut agent = Connection::unix_as(&sock, "agent", Some(&token)).unwrap();
    let b = match agent.call(request::Kind::Spawn(Box::new(SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exec sleep 600".into()],
        ..Default::default()
    }))) {
        Ok(response::Kind::Spawned(info)) => info.id,
        other => panic!("spawn: {other:?}"),
    };
    let msg = SendMessage {
        body: "hello".into(),
        agent_ids: vec![b],
        ..Default::default()
    };
    agent.call(request::Kind::SendMessage(Box::new(msg))).unwrap();

    let ask = || request::Kind::ListMessageEdges(ListMessageEdges::default());
    match user.call(ask()) {
        Ok(response::Kind::MessageEdges(e)) => {
            assert_eq!(e.edges.len(), 1, "{e:?}");
            assert_eq!((e.edges[0].sender, e.edges[0].recipient), (a, b));
        }
        other => panic!("edges: {other:?}"),
    }
    let err = agent.call(ask()).unwrap_err();
    assert!(err.is_forbidden(), "{err}");
}
