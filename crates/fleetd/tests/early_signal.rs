//! Issue #23: a stop signal sent the moment the socket appears must stop
//! fleetd. Each test starts the daemon many times and signals it as soon as
//! `fleetd.sock` exists, before it has necessarily started serving.

use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const EXIT_WITHIN: Duration = Duration::from_secs(3);

fn spawn(dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_fleetd"))
        // Tests never run the developer's real login shell.
        .env("FLEETD_LOGIN_ENV", "off")
        .arg("--state-dir")
        .arg(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fleetd")
}

/// Spins until the socket file exists (not until it accepts: the point is
/// to signal as early as a client could see the daemon).
fn wait_socket(dir: &Path, child: &mut Child) {
    let sock = dir.join("fleetd.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !sock.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("fleetd exited before its socket appeared: {status}");
        }
        assert!(Instant::now() < deadline, "no socket within 5 s");
        std::hint::spin_loop();
    }
}

fn wait_exit(child: &mut Child, within: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Starts fleetd `runs` times, sending `sig` as soon as its socket exists;
/// each must exit cleanly (its handler, not the default action, having
/// stopped it) within [`EXIT_WITHIN`].
fn signal_right_after_socket(sig: libc::c_int, runs: u32) {
    let mut failures = Vec::new();
    for run in 0..runs {
        let dir = TempDir::new().unwrap();
        let mut child = spawn(dir.path());
        wait_socket(dir.path(), &mut child);
        // SAFETY: plain kill(2) on our own child.
        assert_eq!(unsafe { libc::kill(child.id() as i32, sig) }, 0);
        match wait_exit(&mut child, EXIT_WITHIN) {
            Some(status) if status.success() => {}
            Some(status) => failures.push(format!("run {run}: exited with {status}")),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                failures.push(format!("run {run}: still running after {EXIT_WITHIN:?}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {runs} failed: {failures:#?}",
        failures.len()
    );
}

#[test]
fn sigterm_right_after_socket_appears_stops_fleetd() {
    signal_right_after_socket(libc::SIGTERM, 200);
}

#[test]
fn sigint_right_after_socket_appears_stops_fleetd() {
    signal_right_after_socket(libc::SIGINT, 50);
}

#[test]
fn sighup_right_after_socket_appears_stops_fleetd() {
    signal_right_after_socket(libc::SIGHUP, 50);
}
