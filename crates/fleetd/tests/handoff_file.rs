//! `--handoff-check`, the stale handoff file, the instance identity in the
//! handshake, and `run_check`'s refusals.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fleet_client::Connection;
use fleetd::handoff::run_check;

mod support;
use support::script;

struct Reaper(Child);
impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(dir: &Path) -> Reaper {
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
        assert!(Instant::now() < deadline, "daemon not ready");
        std::thread::sleep(Duration::from_millis(5));
    }
    Reaper(child)
}

#[test]
fn handoff_check_prints_version() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let out = Command::new(env!("CARGO_BIN_EXE_fleetd"))
        .arg("--handoff-check")
        .arg("--state-dir")
        .arg(&state)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, b"handoff 1\n");
    assert!(!state.exists());
}

#[test]
fn stale_record_is_removed_on_normal_start() {
    let tmp = tempfile::tempdir().unwrap();
    let stale = tmp.path().join("handoff.bin");
    fs::write(&stale, b"leftover").unwrap();
    let _daemon = start(tmp.path());
    let conn = Connection::unix(&tmp.path().join("fleetd.sock"), "test").unwrap();
    assert!(!stale.exists());
    let restore = conn.hello().restore.as_deref();
    assert!(restore.is_none_or(|r| r.kept.is_empty() && r.previous_instance == 0));
}

#[test]
fn hello_carries_instance_and_binary() {
    let exe = Path::new(env!("CARGO_BIN_EXE_fleetd"));
    let meta = fs::metadata(exe).unwrap();
    let mut instances = Vec::new();
    for _ in 0..2 {
        let tmp = tempfile::tempdir().unwrap();
        let _daemon = start(tmp.path());
        let conn = Connection::unix(&tmp.path().join("fleetd.sock"), "test").unwrap();
        let hello = conn.hello();
        let bin = hello.binary.as_deref().expect("binary identity");
        assert_eq!(
            fs::canonicalize(&bin.path).unwrap(),
            fs::canonicalize(exe).unwrap()
        );
        assert_eq!(
            (bin.ino, bin.size, bin.dev),
            (meta.ino(), meta.size(), meta.dev())
        );
        instances.push(hello.instance);
    }
    assert_ne!(instances[0], instances[1]);
}

/// Limit for a check that ends on its own: generous, since starting a
/// process on a busy machine can take seconds; only a hang reaches it.
const ENDS: Duration = Duration::from_secs(30);
/// Limit for a check whose child never ends in time.
const TIMES_OUT: Duration = Duration::from_secs(2);
/// How long a timed-out check may take beyond its limit (killing and
/// reaping), still far less than its child's own run time.
const KILL_SLACK: Duration = Duration::from_secs(15);

#[test]
fn check_refusals() {
    let tmp = tempfile::tempdir().unwrap();
    let err = |p: &Path| run_check(p, ENDS).unwrap_err();

    assert_eq!(err(&tmp.path().join("missing")), "not an executable file");
    let plain = tmp.path().join("plain");
    fs::write(&plain, "#!/bin/sh\n").unwrap();
    assert_eq!(err(&plain), "not an executable file");

    // The limit, not the child, ends the check.
    let slow = script(tmp.path(), "slow", "sleep 60");
    let started = Instant::now();
    assert_eq!(run_check(&slow, TIMES_OUT).unwrap_err(), "check timed out");
    let took = started.elapsed();
    assert!(
        took >= TIMES_OUT && took < TIMES_OUT + KILL_SLACK,
        "timed-out check took {took:?}"
    );

    assert_eq!(
        err(&script(tmp.path(), "v9", "echo 'handoff 9'")),
        "no common handoff version"
    );
    assert_eq!(
        err(&script(tmp.path(), "bad", "exit 3")),
        "check failed (exit 3)"
    );
    run_check(&script(tmp.path(), "ok", "echo 'handoff 1'"), ENDS).unwrap();
    run_check(Path::new(env!("CARGO_BIN_EXE_fleetd")), ENDS).unwrap();
}

/// Waits up to 10 s for no process's command line to match `needle` (a
/// killed process lingers until it is reaped); panics with `what` if one
/// still does.
fn wait_gone(needle: &str, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let pids = pgrep(needle);
        if pids.is_empty() {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: pids {pids:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Pids of processes whose command line matches `needle`.
fn pgrep(needle: &str) -> Vec<String> {
    let out = Command::new("pgrep")
        .arg("-f")
        .arg(needle)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

#[test]
fn check_leaves_no_descendants_and_is_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    // Sleeps unique to this test process, so `pgrep` never sees another
    // run's (parallel test runs share the machine).
    let pid = std::process::id();
    let (grandchild, child) = (format!("101.{pid}"), format!("102.{pid}"));

    // A grandchild holds stdout open: the check still returns promptly with
    // the child's answer, long before the grandchild would exit, and the
    // grandchild is gone.
    let leaky = script(
        tmp.path(),
        "leaky",
        &format!("sleep {grandchild} &\necho 'handoff 1'"),
    );
    let started = Instant::now();
    run_check(&leaky, ENDS).unwrap();
    let took = started.elapsed();
    assert!(
        took < KILL_SLACK,
        "check waited on the grandchild: {took:?}"
    );

    // A timeout takes the child's descendants with it.
    let slow = script(tmp.path(), "slow2", &format!("sleep {child}"));
    assert_eq!(run_check(&slow, TIMES_OUT).unwrap_err(), "check timed out");

    wait_gone(&format!("sleep {grandchild}"), "grandchild left running");
    wait_gone(&format!("sleep {child}"), "timed-out child left running");
}
