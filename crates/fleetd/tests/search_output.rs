//! `SearchOutput` over the wire (ADR 0098): the user searches every
//! agent's output, an agent is refused, an empty text is an error.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fleet_client::Connection;
use fleet_client::search::search;
use fleet_proto::{SearchOutput, SpawnAgent, request, response};

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

fn ask(text: &str) -> SearchOutput {
    SearchOutput {
        text: text.into(),
        ..Default::default()
    }
}

#[test]
fn the_user_searches_every_agent_and_an_agent_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let _d = Daemon::start(dir.path());
    let sock = dir.path().join("fleetd.sock");
    let mut user = Connection::unix(&sock, "user").unwrap();
    let tok = dir.path().join("a.tok");
    let a = spawn(
        &mut user,
        &format!(
            "echo first ERROR seen; printf %s \"$FLEET_AGENT_TOKEN\" > {0}.tmp && mv {0}.tmp {0}; exec sleep 600",
            tok.display()
        ),
    );
    let b = spawn(&mut user, "echo second error seen; exec sleep 600");
    let deadline = Instant::now() + Duration::from_secs(30);
    let hits = loop {
        let hits = search(&mut user, ask("error seen")).unwrap();
        if hits.hits.len() == 2 {
            break hits;
        }
        assert!(Instant::now() < deadline, "{hits:?}");
        std::thread::sleep(Duration::from_millis(20));
    };
    let found: Vec<(u64, &str)> = hits
        .hits
        .iter()
        .map(|h| (h.agent_id, h.text.as_str()))
        .collect();
    assert_eq!(found, vec![(b, "second error seen"), (a, "first ERROR seen")]);
    assert!(!hits.truncated);

    let one = search(
        &mut user,
        SearchOutput {
            limit: 1,
            ..ask("error seen")
        },
    )
    .unwrap();
    assert_eq!((one.hits.len(), one.truncated), (1, true));

    let err = search(&mut user, ask("")).unwrap_err();
    assert!(err.to_string().contains("empty"), "{err}");

    let token = read_when_there(&tok);
    let mut agent = Connection::unix_as(&sock, "agent", Some(&token)).unwrap();
    let err = search(&mut agent, ask("error")).unwrap_err();
    assert!(err.is_forbidden(), "{err}");
}
