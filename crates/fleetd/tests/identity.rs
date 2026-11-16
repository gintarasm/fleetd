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
