//! Finding and starting a local fleetd (only ever after the user agreed).

use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fleet_client::Connection;

use super::target::ConnectError;

const START_DEADLINE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);
/// The exit code of a fleetd that lost the instance lock to another daemon.
const LOST_LOCK: i32 = 2;

/// `fleetd` next to the running executable, else on `PATH`.
pub fn find_fleetd() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    find_fleetd_from(&exe, &std::env::var_os("PATH").unwrap_or_default())
}

fn find_fleetd_from(exe: &Path, path_var: &OsStr) -> Option<PathBuf> {
    let sibling = exe.parent()?.join("fleetd");
    if sibling.is_file() {
        return Some(sibling);
    }
    std::env::split_paths(path_var)
        .map(|d| d.join("fleetd"))
        .find(|p| p.is_file())
}

/// Starts `fleetd --state-dir <state_dir>` detached and waits (up to 5 s)
/// until it answers a handshake. Harmless if a daemon already runs there.
pub fn start_local(fleetd: &Path, state_dir: &Path) -> Result<(), ConnectError> {
    let mut cmd = Command::new(fleetd);
    cmd.arg("--state-dir")
        .arg(state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and nothing else runs in the child.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| ConnectError::Other(format!("cannot start {}: {e}", fleetd.display())))?;
    let sock = state_dir.join("fleetd.sock");
    let deadline = Instant::now() + START_DEADLINE;
    let mut exited = false;
    let result = loop {
        if Connection::unix(&sock, "gui-launch").is_ok() {
            break Ok(());
        }
        if !exited {
            match child.try_wait() {
                // Lost the lock to another daemon: that one will answer.
                Ok(Some(st)) if st.code() == Some(LOST_LOCK) => exited = true,
                Ok(Some(st)) => {
                    exited = true;
                    break Err(ConnectError::Other(match st.code() {
                        Some(c) => format!("fleetd exited with code {c}"),
                        None => "fleetd was killed".to_owned(),
                    }));
                }
                _ => {}
            }
        }
        if Instant::now() >= deadline {
            break Err(ConnectError::Timeout);
        }
        std::thread::sleep(POLL);
    };
    if !exited {
        // Detached daemon outlives us; just reap it whenever it ends.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_fleetd_prefers_sibling() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("fleetd"), "").unwrap();
        std::fs::write(b.path().join("fleetd"), "").unwrap();
        let exe = a.path().join("fleet-gui");
        let path = std::env::join_paths([b.path()]).unwrap();
        assert_eq!(find_fleetd_from(&exe, &path), Some(a.path().join("fleetd")));
        std::fs::remove_file(a.path().join("fleetd")).unwrap();
        assert_eq!(find_fleetd_from(&exe, &path), Some(b.path().join("fleetd")));
        std::fs::remove_file(b.path().join("fleetd")).unwrap();
        assert_eq!(find_fleetd_from(&exe, &path), None);
    }
}
