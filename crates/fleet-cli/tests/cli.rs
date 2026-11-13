//! `fleet` against a real `fleetd` on a temp state dir.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tempfile::TempDir;

/// Builds `fleetd` once and returns its path (cargo only builds this crate's
/// own binaries for integration tests).
fn fleetd_bin() -> &'static Path {
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let mut cmd = Command::new(env!("CARGO"));
        cmd.args(["build", "-p", "fleetd", "--message-format=short"]);
        if !cfg!(debug_assertions) {
            cmd.arg("--release");
        }
        assert!(cmd.status().expect("cargo build").success());
        let fleet = Path::new(env!("CARGO_BIN_EXE_fleet"));
        fleet.parent().unwrap().join("fleetd")
    })
}

struct Daemon {
    child: Child,
    dir: TempDir,
}

impl Daemon {
    fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let child = Command::new(fleetd_bin())
            .arg("--state-dir")
            .arg(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fleetd");
        let sock = dir.path().join("fleetd.sock");
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(&sock).is_err() {
            assert!(Instant::now() < deadline, "daemon not ready within 5s");
            std::thread::sleep(Duration::from_millis(5));
        }
        Daemon { child, dir }
    }

    fn fleet(&self, args: &[&str]) -> Output {
        fleet(self.dir.path(), args)
    }

    /// Runs `fleet`, expecting success, and returns stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.fleet(args);
        assert!(
            out.status.success(),
            "fleet {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fleet(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fleet"))
        .arg("--state-dir")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run fleet")
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn spawn_then_ls_shows_running() {
    let d = Daemon::start();
    let id = d.ok(&["spawn", "--", "cat"]).trim().to_string();
    assert!(id.parse::<u64>().is_ok(), "id: {id:?}");
    let ls = d.ok(&["ls"]);
    let mut lines = ls.lines();
    assert_eq!(
        lines.next().unwrap().split_whitespace().collect::<Vec<_>>(),
        ["ID", "STATE", "EXIT", "PROGRAM"]
    );
    let row: Vec<_> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(row, [id.as_str(), "running", "-", "cat"]);
    assert!(lines.next().is_none());
}

#[test]
fn kill_then_ls_shows_exited() {
    let d = Daemon::start();
    let id = d.ok(&["spawn", "--", "cat"]).trim().to_string();
    d.ok(&["kill", &id]);
    wait_for("exited", || d.ok(&["ls"]).contains("exited"));
    let ls = d.ok(&["ls"]);
    let row: Vec<_> = ls.lines().nth(1).unwrap().split_whitespace().collect();
    assert_eq!(row[0], id);
    assert_eq!(row[1], "exited");
    assert_ne!(row[2], "-", "exit code shown: {ls}");
}

#[test]
fn send_then_attach_snapshot_contains_text() {
    let d = Daemon::start();
    let id = d.ok(&["spawn", "--", "cat"]).trim().to_string();
    d.ok(&["send", "--enter", &id, "hello"]);
    wait_for("snapshot with text", || {
        let out = d.fleet(&["attach", "--snapshot-only", &id]);
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("hello")
    });
}

#[test]
fn unknown_agent_errors_exit_1() {
    let d = Daemon::start();
    for args in [&["kill", "999"][..], &["attach", "--snapshot-only", "999"]] {
        let out = d.fleet(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(!out.stderr.is_empty());
    }
}

#[test]
fn daemon_not_running_message() {
    let dir = TempDir::new().unwrap();
    let out = fleet(dir.path(), &["ls"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("fleetd is not running"));
}

/// Drives `fleet attach` on a pty: it enters the alt screen, shows the agent,
/// forwards typing, detaches on Ctrl-\ and writes the terminal restore
/// sequence before exiting 0.
#[test]
fn attach_on_pty_detaches_and_restores_terminal() {
    let d = Daemon::start();
    let id = d.ok(&["spawn", "--", "cat"]).trim().to_string();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_fleet"));
    cmd.args(["--state-dir", d.dir.path().to_str().unwrap(), "attach", &id]);
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut seen = Vec::new();
    let until = |seen: &mut Vec<u8>, needle: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !String::from_utf8_lossy(seen).contains(needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(chunk) => seen.extend(chunk),
                Err(_) => panic!(
                    "no {needle:?} in {:?}",
                    String::from_utf8_lossy(seen).into_owned()
                ),
            }
        }
    };

    until(&mut seen, "\x1b[?1049h\x1b[!p");
    writer.write_all(b"typed-text\r").unwrap();
    until(&mut seen, "typed-text");
    writer.write_all(&[0x1c]).unwrap();
    until(&mut seen, "\x1b[!p\x1b[?1049l\x1b[?25h\x1b[?2004l");

    let status = child.wait().unwrap();
    assert!(status.success(), "exit {status:?}");
    // The agent keeps running after a detach.
    assert!(d.ok(&["ls"]).contains("running"));
}
