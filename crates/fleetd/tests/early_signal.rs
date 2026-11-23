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

/// Issue #152: a launcher's blocked signals are not fleetd's. The GUI starts
/// fleetd from a macOS GCD worker thread, whose mask blocks every signal,
/// and a child inherits its parent's mask: fleetd must still stop on SIGTERM
/// (and its threads take the freeze signal, see `fleet_node::pty`).
#[test]
fn sigterm_stops_fleetd_launched_with_every_signal_blocked() {
    use std::os::unix::process::CommandExt;
    let dir = TempDir::new().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fleetd"));
    cmd.env("FLEETD_LOGIN_ENV", "off")
        .arg("--state-dir")
        .arg(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: sigfillset/sigprocmask are async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            let mut all: libc::sigset_t = std::mem::zeroed();
            libc::sigfillset(&mut all);
            libc::sigprocmask(libc::SIG_SETMASK, &all, std::ptr::null_mut());
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn fleetd");
    wait_socket(dir.path(), &mut child);
    // SAFETY: plain kill(2) on our own child.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let status = wait_exit(&mut child, EXIT_WITHIN);
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    assert!(
        status.is_some_and(|s| s.success()),
        "fleetd did not stop on SIGTERM: {status:?}"
    );
}
