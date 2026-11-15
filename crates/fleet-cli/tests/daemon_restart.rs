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
        let dir = TempDir::new().unwrap();
        let child = Command::new(fleetd_bin())
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
    Command::new(env!("CARGO_BIN_EXE_fleet"))
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
        "this fleetd is too old to restart itself; stop it once (pkill fleetd) and the next client starts the new one\n"
    );
    assert!(out.stdout.is_empty());
}
