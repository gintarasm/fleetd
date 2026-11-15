//! `fleet daemon restart` against a real `fleetd` on a temp state dir, and
//! against a fake daemon that predates `RestartDaemon`.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use fleet_proto::{Frame, FrameCodec, HelloOk, PROTOCOL_VERSION, Response, response};
use tempfile::TempDir;
use tokio_util::codec::{Decoder, Encoder};

/// Builds `fleetd` once and returns its path (cargo only builds this crate's
/// own binaries for integration tests).
fn fleetd_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let mut cmd = Command::new(env!("CARGO"));
        cmd.args(["build", "-p", "fleetd", "--message-format=short"]);
        if !cfg!(debug_assertions) {
            cmd.arg("--release");
        }
        assert!(cmd.status().expect("cargo build").success());
        Path::new(env!("CARGO_BIN_EXE_fleet"))
            .parent()
            .unwrap()
            .join("fleetd")
    })
}

struct Daemon {
    child: Child,
    dir: TempDir,
}

impl Daemon {
    fn start() -> Self {
        Self::start_from(fleetd_bin())
    }

    /// Runs the daemon from `binary`, in `/` as when the GUI starts it.
    fn start_from(binary: &Path) -> Self {
        let dir = TempDir::new().unwrap();
        let child = Command::new(binary)
            .current_dir("/")
            // Never the developer's real login shell.
            .env("FLEETD_LOGIN_ENV", "off")
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
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // SIGTERM, so the daemon ends its agents.
        // SAFETY: plain signal to our child.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fleet(dir: &Path, args: &[&str]) -> Output {
    fleet_in(Path::new("."), dir, args)
}

/// [`fleet`], run in `cwd`.
fn fleet_in(cwd: &Path, dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fleet"))
        .current_dir(cwd)
        .arg("--state-dir")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run fleet")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn restart_keeps_the_running_agent() {
    let d = Daemon::start();
    let id = text(&fleet(d.dir.path(), &["spawn", "--", "cat"]).stdout);
    let id = id.trim();
    let out = fleet(
        d.dir.path(),
        &[
            "daemon",
            "restart",
            "--binary",
            fleetd_bin().to_str().unwrap(),
        ],
    );
    assert_eq!(text(&out.stdout), "fleetd restarted: 1 agents kept\n");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let ls = text(&fleet(d.dir.path(), &["ls"]).stdout);
    let row: Vec<_> = ls.lines().nth(1).unwrap().split_whitespace().collect();
    assert_eq!(&row[..2], [id, "running"], "{ls}");
}

#[test]
fn relative_binary_resolves_where_fleet_runs_not_where_the_daemon_does() {
    let d = Daemon::start();
    // `<profile>/fleetd` from the target dir, as typed in a checkout; the
    // daemon runs in `/`.
    let bin = fleetd_bin();
    let profile = bin.parent().unwrap();
    let relative = Path::new(profile.file_name().unwrap()).join("fleetd");
    let out = fleet_in(
        profile.parent().unwrap(),
        d.dir.path(),
        &["daemon", "restart", "--binary", relative.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "fleetd restarted: 0 agents kept\n");
}

#[test]
fn no_binary_restarts_onto_the_daemons_own_path_as_rebuilt() {
    use std::os::unix::fs::MetadataExt;
    let bin_dir = TempDir::new().unwrap();
    let own = bin_dir.path().join("fleetd");
    fs::copy(fleetd_bin(), &own).unwrap();
    let d = Daemon::start_from(&own);
    let id = text(&fleet(d.dir.path(), &["spawn", "--", "cat"]).stdout);
    // A different build lands at the daemon's path (a new file there).
    let next = bin_dir.path().join("fleetd.new");
    fs::copy(fleetd_bin(), &next).unwrap();
    fs::rename(&next, &own).unwrap();
    let rebuilt = fs::metadata(&own).unwrap().ino();
    // macOS scans a new executable on its first run, which under load can
    // outlast the daemon's 5 s check: run it once first.
    let warm = Command::new(&own).arg("--handoff-check").output().unwrap();
    assert!(warm.status.success());

    let out = fleet(d.dir.path(), &["daemon", "restart"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "fleetd restarted: 1 agents kept\n");
    assert_eq!(running_ino(d.dir.path()), rebuilt);
    let ls = text(&fleet(d.dir.path(), &["ls"]).stdout);
    let row: Vec<_> = ls.lines().nth(1).unwrap().split_whitespace().collect();
    assert_eq!(&row[..2], [id.trim(), "running"], "{ls}");
}

/// The inode of the binary the daemon on `dir` reports it runs.
fn running_ino(dir: &Path) -> u64 {
    let mut s = std::os::unix::net::UnixStream::connect(dir.join("fleetd.sock")).unwrap();
    let mut out = BytesMut::new();
    let hello = Frame::Hello(fleet_proto::Hello {
        protocol_version: PROTOCOL_VERSION,
        client: "restart-test".into(),
    });
    FrameCodec::default().encode(hello, &mut out).unwrap();
    s.write_all(&out).unwrap();
    let mut buf = BytesMut::new();
    loop {
        if let Some(Frame::HelloOk(ok)) = FrameCodec::default().decode(&mut buf).unwrap() {
            return ok.binary.expect("binary identity").ino;
        }
        let mut chunk = [0u8; 4096];
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0, "the daemon closed the handshake");
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[test]
fn failing_binary_is_refused_and_the_old_daemon_serves() {
    let d = Daemon::start();
    let bin = script(d.dir.path(), "bad", "exit 3");
    let out = fleet(
        d.dir.path(),
        &["daemon", "restart", "--binary", bin.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "fleetd restart failed: check failed (exit 3); the old fleetd is still running\n"
    );
    assert!(out.stdout.is_empty());
    assert!(fleet(d.dir.path(), &["ls"]).status.success());
}

/// A daemon from before `RestartDaemon`: handshakes, answers every request
/// with "empty request" (what an unknown oneof arm decodes to).
fn fake_old_daemon(dir: &Path) {
    let listener = UnixListener::bind(dir.join("fleetd.sock")).unwrap();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = BytesMut::new();
        let send = |s: &mut std::os::unix::net::UnixStream, f: Frame| {
            let mut out = BytesMut::new();
            FrameCodec::default().encode(f, &mut out).unwrap();
            s.write_all(&out).unwrap();
        };
        loop {
            while let Some(frame) = FrameCodec::default().decode(&mut buf).unwrap() {
                match frame {
                    Frame::Hello(_) => send(
                        &mut s,
                        Frame::HelloOk(HelloOk {
                            protocol_version: PROTOCOL_VERSION,
                            server: "old".into(),
                            instance: 1,
                            ..Default::default()
                        }),
                    ),
                    Frame::Request(r) => send(
                        &mut s,
                        Frame::Response(Response {
                            id: r.id,
                            kind: Some(response::Kind::Error(fleet_proto::Error {
                                message: "empty request".into(),
                            })),
                        }),
                    ),
                    _ => {}
                }
            }
            let mut chunk = [0u8; 4096];
            match s.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    });
}

#[test]
fn old_daemon_gets_the_too_old_message() {
    let dir = TempDir::new().unwrap();
    fake_old_daemon(dir.path());
    let out = fleet(dir.path(), &["daemon", "restart"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "this fleetd is too old to restart itself; stop it once (fleetd --force-stop, or scripts/install.sh --force-stop) and the next client starts the new one\n"
    );
    assert!(out.stdout.is_empty());
}
