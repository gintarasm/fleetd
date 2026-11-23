//! `ListActions` / `RunAction` over the wire (ADR 0092). The workflows
//! themselves are tested in `fleet-workflow` and the server's unit tests
//! with a test workflow; here the real daemon answers both requests.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fleet_client::Connection;
use fleet_proto::{ListActions, RunAction, request, response};

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

#[test]
fn the_daemon_lists_and_runs_actions() {
    let dir = tempfile::tempdir().unwrap();
    let _d = Daemon::start(dir.path());
    let mut c = Connection::unix_as(&dir.path().join("fleetd.sock"), "test", None).unwrap();
    match c.call(request::Kind::ListActions(ListActions {})) {
        Ok(response::Kind::ActionList(_)) => {}
        other => panic!("list actions: {other:?}"),
    }
    let run = RunAction {
        name: "nope.nothing".into(),
        ..Default::default()
    };
    let e = c
        .call(request::Kind::RunAction(Box::new(run)))
        .unwrap_err()
        .to_string();
    assert!(e.contains("no action"), "{e}");
    // The connection serves on after an action.
    assert!(c.call(request::Kind::ListActions(ListActions {})).is_ok());
}
