//! Node primitives over the wire (ADR 0091): `Exec`, `ReadFile` and
//! `ListDir` for the user; refused for agents; an Exec does not hold up its
//! connection and dies with it.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fleet_client::Connection;
use fleet_client::node::{exec, list_dir, read_file};
use fleet_proto::{EntryKind, Exec, Frame, ListDir, ReadFile, SpawnAgent, request, response};

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

fn sh(script: &str) -> Exec {
    Exec {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        ..Default::default()
    }
}

fn wait_for(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = std::fs::read_to_string(path)
            && s.ends_with('\n')
        {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn dead(pid: i32) -> bool {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        // SAFETY: signal 0 only checks existence.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn the_user_runs_reads_and_lists() {
    let dir = tempfile::tempdir().unwrap();
    let _d = Daemon::start(dir.path());
    let mut user = Connection::unix(&dir.path().join("fleetd.sock"), "user").unwrap();

    let mut req = sh("echo out; echo err >&2; echo \"$K\"; exit 3");
    req.env.insert("K".into(), "v".into());
    let r = exec(&mut user, req).unwrap();
    assert_eq!(r.stdout, b"out\nv\n");
    assert_eq!(r.stderr, b"err\n");
    assert_eq!(r.exit_code, 3);

    let mut slow = sh("sleep 30");
    slow.timeout_ms = 200;
    let r = exec(&mut user, slow).unwrap();
    assert!(r.timed_out);
    assert_eq!(r.signal, libc::SIGTERM);

    let mut capped = sh("head -c 5000 /dev/zero");
    capped.max_output = 100;
    let r = exec(&mut user, capped).unwrap();
    assert_eq!(r.stdout.len(), 100);
    assert!(r.stdout_truncated);

    let err = exec(&mut user, Exec::default()).unwrap_err();
    assert!(err.to_string().contains("argv"), "{err}");

    let file = dir.path().join("f.txt");
    std::fs::write(&file, "hello").unwrap();
    let data = read_file(
        &mut user,
        ReadFile {
            path: file.to_string_lossy().into_owned(),
            offset: 1,
            max_bytes: 3,
        },
    )
    .unwrap();
    assert_eq!(
        (data.data.as_slice(), data.size, data.eof),
        (&b"ell"[..], 5, false)
    );

    let listing = list_dir(
        &mut user,
        ListDir {
            path: dir.path().to_string_lossy().into_owned(),
            max_entries: 0,
        },
    )
    .unwrap();
    let f = listing.entries.iter().find(|e| e.name == "f.txt").unwrap();
    assert_eq!((f.kind(), f.size), (EntryKind::File, 5));
    assert!(
        read_file(
            &mut user,
            ReadFile {
                path: "rel".into(),
                ..Default::default()
            }
        )
        .is_err()
    );
}

#[test]
fn agents_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let _d = Daemon::start(dir.path());
    let sock = dir.path().join("fleetd.sock");
    let mut user = Connection::unix(&sock, "user").unwrap();
    let tok = dir.path().join("a.tok");
    let script = format!(
        "printf %s \"$FLEET_AGENT_TOKEN\" > {0}.tmp && mv {0}.tmp {0}; exec sleep 600",
        tok.display()
    );
    let req = SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script],
        ..Default::default()
    };
    user.call(request::Kind::Spawn(Box::new(req))).unwrap();
    let token = wait_for_token(&tok);
    let mut agent = Connection::unix_as(&sock, "agent", Some(&token)).unwrap();
    let e = exec(&mut agent, sh("true")).unwrap_err();
    assert!(e.is_forbidden(), "{e}");
    let e = read_file(
        &mut agent,
        ReadFile {
            path: "/etc/hosts".into(),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(e.is_forbidden(), "{e}");
    let e = list_dir(
        &mut agent,
        ListDir {
            path: "/tmp".into(),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(e.is_forbidden(), "{e}");
}

fn wait_for_token(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(s) = std::fs::read_to_string(path) {
            return s;
        }
        assert!(Instant::now() < deadline, "token never appeared");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_running_exec_does_not_hold_up_its_connection() {
    let dir = tempfile::tempdir().unwrap();
    let _d = Daemon::start(dir.path());
    let conn = Connection::unix(&dir.path().join("fleetd.sock"), "user").unwrap();
    let (mut reader, mut sender) = conn.split();
    let slow = sender
        .send(request::Kind::Exec(Box::new(sh("sleep 1; echo late"))))
        .unwrap();
    let ls = sender
        .send(request::Kind::ListDir(ListDir {
            path: dir.path().to_string_lossy().into_owned(),
            max_entries: 0,
        }))
        .unwrap();
    let mut order = Vec::new();
    while order.len() < 2 {
        if let Some(Frame::Response(r)) = reader.read_frame().unwrap() {
            order.push(r.id);
        }
    }
    assert_eq!(order, [ls, slow]);
}

#[test]
fn closing_the_connection_kills_the_exec() {
    let dir = tempfile::tempdir().unwrap();
    let _d = Daemon::start(dir.path());
    let conn = Connection::unix(&dir.path().join("fleetd.sock"), "user").unwrap();
    let pidfile = dir.path().join("pid");
    let (reader, mut sender) = conn.split();
    sender
        .send(request::Kind::Exec(Box::new(sh(&format!(
            "echo $$ > {}; sleep 600",
            pidfile.display()
        )))))
        .unwrap();
    let pid: i32 = wait_for(&pidfile).trim().parse().unwrap();
    drop(sender);
    drop(reader);
    assert!(dead(pid), "the exec must die with its connection");
}

#[test]
fn a_ninth_exec_waits_for_a_slot() {
    let dir = tempfile::tempdir().unwrap();
    let _d = Daemon::start(dir.path());
    let sock = dir.path().join("fleetd.sock");
    let started = Instant::now();
    let threads: Vec<_> = (0..9)
        .map(|_| {
            let sock = sock.clone();
            std::thread::spawn(move || {
                let mut c = Connection::unix(&sock, "user").unwrap();
                exec(&mut c, sh("sleep 0.5")).unwrap();
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert!(
        started.elapsed() >= Duration::from_millis(950),
        "{:?}",
        started.elapsed()
    );
}

// Keeps the unused import honest if the helpers change.
#[allow(dead_code)]
fn _kinds(_: response::Kind) {}

#[test]
fn a_daemon_restart_kills_running_execs() {
    use fleet_client::{Restart, RestartOutcome, restart_daemon};
    let dir = tempfile::tempdir().unwrap();
    let _d = Daemon::start(dir.path());
    let sock = dir.path().join("fleetd.sock");
    let pidfile = dir.path().join("pid");
    let conn = Connection::unix(&sock, "user").unwrap();
    let (_reader, mut sender) = conn.split();
    sender
        .send(request::Kind::Exec(Box::new(sh(&format!(
            "echo $$ > {}; sleep 600",
            pidfile.display()
        )))))
        .unwrap();
    let pid: i32 = wait_for(&pidfile).trim().parse().unwrap();
    let admin = Connection::unix(&sock, "admin").unwrap();
    // Onto its own binary: the new daemon must come up, and the Exec's
    // process group (its own session) must be gone.
    match restart_daemon(admin, &sock, "admin", env!("CARGO_BIN_EXE_fleetd")).unwrap() {
        Restart::Outcome(RestartOutcome::Restarted { .. }) => {}
        _ => panic!("restart did not complete"),
    }
    assert!(dead(pid), "a restart must kill the running exec");
}
