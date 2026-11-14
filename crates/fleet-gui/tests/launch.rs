#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use fleet_gui::core::launch::start_local;
use fleet_gui::core::target::{Target, connect};
use fleet_proto::{ListAgents, request, response};

fn fleetd_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let mut cmd = Command::new(env!("CARGO"));
        cmd.args(["build", "-p", "fleetd", "--message-format=short"]);
        if !cfg!(debug_assertions) {
            cmd.arg("--release");
        }
        assert!(cmd.status().expect("cargo build").success());
        // Never the developer's real login shell. Set here, once, before
        // any test starts a daemon (they all call `fleetd_bin()` first).
        // SAFETY: no other thread of this test binary reads the environment
        // concurrently: every test is blocked on this OnceLock.
        unsafe { std::env::set_var("FLEETD_LOGIN_ENV", "off") };
        // The test binary lives in target/<profile>/deps.
        let exe = std::env::current_exe().unwrap();
        exe.parent().unwrap().parent().unwrap().join("fleetd")
    })
}

/// Stops the daemons `start_local` left running for this state dir; there
/// is no pid file, so they are found by their command line.
struct DaemonCleanup(PathBuf);

impl DaemonCleanup {
    fn pids(&self) -> Vec<String> {
        let out = Command::new("pgrep")
            .args(["-f", "--"])
            .arg(format!("--state-dir {}", self.0.display()))
            .output()
            .expect("pgrep");
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        for pid in self.pids() {
            let _ = Command::new("kill").args(["-TERM", &pid]).status();
        }
    }
}

fn agent_count(state_dir: &Path) -> usize {
    let mut c = connect(&Target::Local, state_dir, "gui-test")
        .expect("connect")
        .conn;
    match c
        .call(request::Kind::List(ListAgents::default()))
        .expect("list")
    {
        response::Kind::Agents(l) => l.agents.len(),
        _ => panic!("unexpected reply"),
    }
}

fn setup() -> (tempfile::TempDir, DaemonCleanup) {
    let dir = tempfile::tempdir().unwrap();
    let cleanup = DaemonCleanup(dir.path().to_owned());
    (dir, cleanup)
}

#[test]
fn start_local_then_connect() {
    let bin = fleetd_bin();
    let (dir, _cleanup) = setup();
    start_local(bin, dir.path()).expect("start");
    assert_eq!(agent_count(dir.path()), 0);
}

#[test]
fn start_local_twice_is_harmless() {
    let bin = fleetd_bin();
    let (dir, cleanup) = setup();
    let (a, b) = std::thread::scope(|s| {
        let a = s.spawn(|| start_local(bin, dir.path()));
        let b = s.spawn(|| start_local(bin, dir.path()));
        (a.join().unwrap(), b.join().unwrap())
    });
    a.expect("first");
    b.expect("second");
    assert_eq!(agent_count(dir.path()), 0);
    // The loser exits (code 2) once it sees the lock is taken.
    for _ in 0..100 {
        if cleanup.pids().len() == 1 {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(cleanup.pids().len(), 1, "exactly one daemon must remain");
}
