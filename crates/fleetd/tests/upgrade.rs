//! `fleetd` run while another fleetd holds the lock: same build, another
//! build (restarted onto this one), and a daemon too old to restart itself.
//! No test has a terminal: prompts are answered by the debug-build hook
//! `FLEETD_TEST_UPGRADE_ANSWER`.
//!
//! The "too old" daemon is a separate process (this test binary run as a
//! fake, see `fake_old_daemon_process`) because the tests send it SIGTERM.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use fleet_client::Connection;
use fleet_proto::{
    AgentInfo, AgentList, AgentState, Frame, FrameCodec, HelloOk, ListAgents, PROTOCOL_VERSION,
    Response, SpawnAgent, request, response,
};
use tempfile::TempDir;
use tokio_util::codec::{Decoder, Encoder};

const FLEETD: &str = env!("CARGO_BIN_EXE_fleetd");

fn wait_for(what: &str, secs: u64, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done() {
        assert!(Instant::now() < deadline, "{what} not within {secs}s");
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct Daemon {
    child: Child,
    dir: TempDir,
}

impl Daemon {
    fn start() -> Self {
        Self::start_bin(Path::new(FLEETD))
    }

    fn start_bin(bin: &Path) -> Self {
        let dir = TempDir::new().unwrap();
        let child = Command::new(bin)
            .env("FLEETD_LOGIN_ENV", "off")
            .arg("--state-dir")
            .arg(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fleetd");
        let sock = dir.path().join("fleetd.sock");
        wait_for("daemon ready", 5, || UnixStream::connect(&sock).is_ok());
        Daemon { child, dir }
    }

    fn conn(&self) -> Connection {
        Connection::unix(&self.dir.path().join("fleetd.sock"), "test").unwrap()
    }

    fn agents(&self) -> Vec<AgentInfo> {
        match self
            .conn()
            .call(request::Kind::List(ListAgents::default()))
            .unwrap()
        {
            response::Kind::Agents(AgentList { agents, .. }) => agents,
            other => panic!("{other:?}"),
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // SAFETY: plain signal to our child (a restart keeps the pid).
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        wait_for("daemon exit", 5, || {
            self.child.try_wait().ok().flatten().is_some()
        });
    }
}

/// Runs `fleetd` against `dir` with `args`, stdin closed (so no terminal),
/// and `answer` as the prompt hook's answer if given.
fn fleetd(dir: &Path, args: &[&str], answer: Option<&str>) -> Output {
    let mut cmd = Command::new(FLEETD);
    cmd.env("FLEETD_LOGIN_ENV", "off")
        .env_remove("FLEETD_TEST_UPGRADE_ANSWER")
        .arg("--state-dir")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null());
    if let Some(a) = answer {
        cmd.env("FLEETD_TEST_UPGRADE_ANSWER", a);
    }
    cmd.output().expect("run fleetd")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A copy of the fleetd binary elsewhere: another path and inode, so another
/// identity.
fn other_build() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let bin = dir.path().join("fleetd-other");
    fs::copy(FLEETD, &bin).unwrap();
    (dir, bin)
}

fn spawn_sh(d: &Daemon) -> u64 {
    match d
        .conn()
        .call(request::Kind::Spawn(Box::new(SpawnAgent {
            program: "sh".into(),
            cols: 80,
            rows: 24,
            ..Default::default()
        })))
        .unwrap()
    {
        response::Kind::Spawned(info) => info.id,
        other => panic!("{other:?}"),
    }
}

#[test]
fn same_build_prints_already_running() {
    let d = Daemon::start();
    let out = fleetd(d.dir.path(), &[], None);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(text(&out.stderr), "fleetd already running\n");
    // Even when a prompt could be answered, or a flag given.
    let out = fleetd(d.dir.path(), &["--upgrade"], Some("y"));
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(text(&out.stderr), "fleetd already running\n");
}

#[test]
fn upgrade_flag_restarts_onto_this_binary() {
    let (_keep, other) = other_build();
    let d = Daemon::start_bin(&other);
    let id = spawn_sh(&d);
    let out = fleetd(d.dir.path(), &["--upgrade"], None);
    assert_eq!(text(&out.stdout), "fleetd restarted: 1 agents kept\n");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let agents = d.agents();
    assert_eq!(agents.len(), 1);
    assert_eq!((agents[0].id, agents[0].state()), (id, AgentState::Running));
    let binary = d.conn().hello().binary.clone().unwrap();
    assert_eq!(
        fs::canonicalize(&binary.path).unwrap(),
        fs::canonicalize(FLEETD).unwrap()
    );
}

#[test]
fn different_build_without_tty_or_flag_explains() {
    let (_keep, other) = other_build();
    let d = Daemon::start_bin(&other);
    let out = fleetd(d.dir.path(), &[], None);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        text(&out.stderr),
        "fleetd already running (a different build; run fleetd --upgrade to restart it onto this one)\n"
    );
    assert_eq!(
        d.conn().hello().binary.as_ref().unwrap().path,
        other.to_str().unwrap()
    );
}

#[test]
fn prompt_yes_restarts_and_no_leaves_it() {
    let (_keep, other) = other_build();
    let d = Daemon::start_bin(&other);
    spawn_sh(&d);
    let out = fleetd(d.dir.path(), &[], Some("n"));
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        text(&out.stderr),
        format!(
            "fleetd is already running a different build ({}, 1 agents).\n\
             Restart it onto this binary? Agents keep running. [y/N]\n\
             left the running fleetd as it is\n",
            other.display()
        )
    );
    let out = fleetd(d.dir.path(), &[], Some("YES"));
    assert_eq!(text(&out.stdout), "fleetd restarted: 1 agents kept\n");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}

/// What the fake old daemon does with SIGTERM.
const FAKE_DIR: &str = "FLEETD_TEST_FAKE_OLD_DIR";
const FAKE_IGNORE_TERM: &str = "FLEETD_TEST_FAKE_IGNORE_TERM";

/// Not a test: with `FLEETD_TEST_FAKE_OLD_DIR` set, this test binary becomes
/// a daemon from before `RestartDaemon` on that state dir: it takes the
/// lock, binds the socket, handshakes (naming no binary) and answers
/// `ListAgents` with no agents and everything else with "empty request". On
/// SIGTERM it removes the socket and exits, or ignores the signal.
#[test]
fn fake_old_daemon_process() {
    let Some(dir) = std::env::var_os(FAKE_DIR) else {
        return;
    };
    let dir = Path::new(&dir);
    let lock: File = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("fleetd.lock"))
        .unwrap();
    lock.lock().unwrap();
    let sock = dir.join("fleetd.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            if std::env::var_os(FAKE_IGNORE_TERM).is_some() {
                libc::SIG_IGN
            } else {
                on_term as extern "C" fn(libc::c_int) as libc::sighandler_t
            },
        );
    }
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            std::thread::spawn(move || serve_old(s));
        }
    });
    while !TERMINATED.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = fs::remove_file(&sock);
    std::process::exit(0);
}

static TERMINATED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    TERMINATED.store(true, Ordering::SeqCst);
}

fn serve_old(mut s: UnixStream) {
    let mut buf = BytesMut::new();
    let send = |s: &mut UnixStream, f: Frame| {
        let mut out = BytesMut::new();
        FrameCodec::default().encode(f, &mut out).unwrap();
        let _ = s.write_all(&out);
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
                Frame::Request(r) => {
                    let kind = match r.kind {
                        Some(request::Kind::List(_)) => {
                            response::Kind::Agents(AgentList::default())
                        }
                        _ => response::Kind::Error(fleet_proto::Error {
                            message: "empty request".into(),
                        }),
                    };
                    send(
                        &mut s,
                        Frame::Response(Response {
                            id: r.id,
                            kind: Some(kind),
                        }),
                    );
                }
                _ => {}
            }
        }
        let mut chunk = [0u8; 4096];
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

struct FakeOld {
    child: Child,
    dir: TempDir,
}

impl FakeOld {
    fn start(ignore_term: bool) -> Self {
        let dir = TempDir::new().unwrap();
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "fake_old_daemon_process", "--nocapture"])
            .env(FAKE_DIR, dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if ignore_term {
            cmd.env(FAKE_IGNORE_TERM, "1");
        }
        let child = cmd.spawn().unwrap();
        let sock = dir.path().join("fleetd.sock");
        wait_for("fake ready", 5, || UnixStream::connect(&sock).is_ok());
        FakeOld { child, dir }
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
}

impl Drop for FakeOld {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn too_old_daemon_without_force_stop_is_left_running() {
    let mut old = FakeOld::start(false);
    let out = fleetd(old.dir.path(), &[], None);
    assert_eq!(out.status.code(), Some(2));
    let err = text(&out.stderr);
    assert!(
        err.starts_with(
            "the running fleetd is too old to restart itself; stopping it ends its 0 agents\n"
        ),
        "{err}"
    );
    // `--upgrade` does not stop a daemon either.
    let out = fleetd(old.dir.path(), &["--upgrade"], None);
    assert_eq!(out.status.code(), Some(2));
    // A "no" at the prompt leaves it too.
    let out = fleetd(old.dir.path(), &[], Some("n"));
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("Stop it? [y/N]\n"));
    assert!(old.alive());
}

#[test]
fn force_stop_that_does_not_stop_says_so() {
    let mut old = FakeOld::start(true);
    let started = Instant::now();
    let out = fleetd(old.dir.path(), &["--force-stop"], None);
    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(out.status.code(), Some(1));
    let pid = old.child.id();
    let err = text(&out.stderr);
    assert!(
        err.ends_with(&format!(
            "the old fleetd (pid {pid}) did not stop; stop it with kill -9 {pid} and run fleetd again\n"
        )),
        "{err}"
    );
    // Never SIGKILL.
    assert!(old.alive());
}

#[test]
fn force_stop_stops_the_old_daemon_and_takes_over() {
    let old = FakeOld::start(false);
    let dir = old.dir.path().to_path_buf();
    // The fake is our child: reap it when it exits, or it stays a zombie and
    // `kill(pid, 0)` keeps finding it (under init a real daemon is reaped).
    let pid = old.child.id() as i32;
    std::thread::spawn(move || {
        // SAFETY: waits for our own child.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
    });
    let mut new = Command::new(FLEETD)
        .env("FLEETD_LOGIN_ENV", "off")
        .arg("--state-dir")
        .arg(&dir)
        .arg("--force-stop")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // It serves once the old one is gone: a handshake naming its binary.
    let sock = dir.join("fleetd.sock");
    wait_for("new daemon", 8, || {
        Connection::unix(&sock, "test").is_ok_and(|c| c.hello().binary.is_some())
    });
    // SAFETY: plain signal to our child.
    unsafe { libc::kill(new.id() as i32, libc::SIGTERM) };
    let mut err = String::new();
    new.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    new.wait().unwrap();
    assert!(err.contains("stopped the old fleetd\n"), "{err}");
}
