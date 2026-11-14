//! Finding and starting a local fleetd (only ever after the user agreed),
//! and installing the agent skill with `fleet skill install`.

use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fleet_client::Connection;

use super::target::{ConnectError, Target, ssh_remote};

const START_DEADLINE: Duration = Duration::from_secs(5);
/// Per-probe handshake limit, so one stuck probe cannot overshoot the deadline.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(20);
/// The exit code of a fleetd that lost the instance lock to another daemon.
const LOST_LOCK: i32 = 2;

/// `fleetd` next to the running executable, else on `PATH`.
pub fn find_fleetd() -> Option<PathBuf> {
    find_tool("fleetd")
}

/// `name` next to the running executable, else on `PATH`.
fn find_tool(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    find_tool_from(name, &exe, &std::env::var_os("PATH").unwrap_or_default())
}

fn find_tool_from(name: &str, exe: &Path, path_var: &OsStr) -> Option<PathBuf> {
    let sibling = exe.parent()?.join(name);
    if sibling.is_file() {
        return Some(sibling);
    }
    std::env::split_paths(path_var)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// `fleet skill install` for `target`: the sibling (or `PATH`) `fleet`
/// locally, `ssh <dest> sh -lc 'fleet skill install'` remotely.
pub fn skill_install_command(target: &Target, state_dir: &Path) -> std::io::Result<Command> {
    match target {
        Target::Local => {
            let mut cmd = Command::new(find_tool("fleet").unwrap_or_else(|| "fleet".into()));
            cmd.args(["skill", "install"]);
            Ok(cmd)
        }
        Target::Ssh { dest } => ssh_remote(dest, "fleet skill install", state_dir),
    }
}

/// Runs `cmd` (a [`skill_install_command`]) to completion: the printed
/// path, or why it failed. Blocks, so call it off the UI thread.
pub fn install_skill(mut cmd: Command) -> Result<String, String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run fleet: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    Err(match (stderr.is_empty(), out.status.code()) {
        (false, _) => stderr,
        (true, Some(c)) => format!("fleet skill install exited with code {c}"),
        (true, None) => "fleet skill install was killed".to_owned(),
    })
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
        if Connection::unix_with_timeout(&sock, "gui-launch", PROBE_TIMEOUT).is_ok() {
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
        let find = |p: &OsStr| find_tool_from("fleetd", &exe, p);
        assert_eq!(find(&path), Some(a.path().join("fleetd")));
        std::fs::remove_file(a.path().join("fleetd")).unwrap();
        assert_eq!(find(&path), Some(b.path().join("fleetd")));
        std::fs::remove_file(b.path().join("fleetd")).unwrap();
        assert_eq!(find(&path), None);
    }

    #[test]
    fn remote_skill_install_runs_fleet_in_a_login_shell() {
        let dir = tempfile::tempdir().unwrap();
        let target = Target::Ssh {
            dest: "me@box".into(),
        };
        let cmd = skill_install_command(&target, dir.path()).unwrap();
        assert_eq!(cmd.get_program(), "ssh");
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args[args.len() - 2..],
            ["me@box", "sh -lc 'fleet skill install'"]
        );
        assert!(args.contains(&"ControlMaster=auto".to_owned()));
    }

    #[test]
    fn install_skill_reports_the_path_or_the_error() {
        let mut ok = Command::new("sh");
        ok.args(["-c", "echo /home/me/.claude/skills/fleet/SKILL.md"]);
        assert_eq!(
            install_skill(ok),
            Ok("/home/me/.claude/skills/fleet/SKILL.md".to_owned())
        );
        let mut bad = Command::new("sh");
        bad.args(["-c", "echo 'fleet: not found' >&2; exit 127"]);
        assert_eq!(install_skill(bad), Err("fleet: not found".to_owned()));
        let mut quiet = Command::new("sh");
        quiet.args(["-c", "exit 3"]);
        assert_eq!(
            install_skill(quiet),
            Err("fleet skill install exited with code 3".to_owned())
        );
    }
}
