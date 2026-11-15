//! `--handoff-check`, the stale handoff file, the instance identity in the
//! handshake, and `run_check`'s refusals.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fleet_client::Connection;
use fleetd::handoff::run_check;

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

fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn check_refusals() {
    let tmp = tempfile::tempdir().unwrap();
    let t = Duration::from_secs(2);
    let err = |p: &Path| run_check(p, t).unwrap_err();

    assert_eq!(err(&tmp.path().join("missing")), "not an executable file");
    let plain = tmp.path().join("plain");
    fs::write(&plain, "#!/bin/sh\n").unwrap();
    assert_eq!(err(&plain), "not an executable file");

    let slow = script(tmp.path(), "slow", "sleep 5");
    let started = Instant::now();
    assert_eq!(err(&slow), "check timed out");
    assert!(started.elapsed() < Duration::from_secs(4));

    assert_eq!(
        err(&script(tmp.path(), "v9", "echo 'handoff 9'")),
        "no common handoff version"
    );
    assert_eq!(
        err(&script(tmp.path(), "bad", "exit 3")),
        "check failed (exit 3)"
    );
    run_check(&script(tmp.path(), "ok", "echo 'handoff 1'"), t).unwrap();
    run_check(Path::new(env!("CARGO_BIN_EXE_fleetd")), t).unwrap();
}
