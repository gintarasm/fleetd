use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fleetd::{Config, StartupError, startup};
use tempfile::TempDir;

fn spawn(dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_fleetd"))
        .arg("--state-dir")
        .arg(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fleetd")
}

fn wait_ready(dir: &Path) {
    let sock = dir.join("fleetd.sock");
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if UnixStream::connect(&sock).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("daemon not ready within 2s");
}

fn sigterm(child: &Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("run kill");
    assert!(status.success());
}

struct Reaper(Child);
impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn becomes_ready() {
    let dir = TempDir::new().unwrap();
    let _d = Reaper(spawn(dir.path()));
    wait_ready(dir.path());
}

#[test]
fn second_instance_exits_2_and_keeps_socket() {
    let dir = TempDir::new().unwrap();
    let _first = Reaper(spawn(dir.path()));
    wait_ready(dir.path());
    let mut second = spawn(dir.path());
    let status = second.wait().unwrap();
    assert_eq!(status.code(), Some(2));
    assert!(UnixStream::connect(dir.path().join("fleetd.sock")).is_ok());
}

#[test]
fn stale_socket_is_replaced() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("fleetd.sock"), b"stale").unwrap();
    let _d = Reaper(spawn(dir.path()));
    wait_ready(dir.path());
}

#[test]
fn overlong_socket_path_errors() {
    let tmp = TempDir::new().unwrap();
    let cfg = Config {
        state_dir: tmp.path().join("a".repeat(100)),
    };
    assert!(cfg.socket_path().as_os_str().len() > 103);
    match startup(&cfg) {
        Err(StartupError::SocketPathTooLong(_)) => {}
        other => panic!("expected SocketPathTooLong, got {:?}", other.err()),
    }
}

#[test]
fn lock_released_on_exit() {
    let dir = TempDir::new().unwrap();
    let mut first = spawn(dir.path());
    wait_ready(dir.path());
    sigterm(&first);
    first.wait().unwrap();
    assert!(!dir.path().join("fleetd.sock").exists());
    let _second = Reaper(spawn(dir.path()));
    wait_ready(dir.path());
}
