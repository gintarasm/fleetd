//! The login-shell environment agents start from.
//!
//! A daemon started by launchd, systemd or `ssh host fleetd --stdio` has a
//! bare environment (a minimal `PATH`, none of the user's exports). Agents
//! should see what the user sees in a terminal, so the daemon asks the user's
//! login shell for its environment once, in the background, and the node uses
//! it as the base of every agent's environment.
//!
//! Setting [`LOGIN_ENV_SWITCH`]`=off` in the daemon's environment skips the
//! capture: agents then start from the daemon's own environment (tests use
//! this so they never run the developer's real login shell).
//!
//! Shells: zsh, bash and sh are tested; whether fish's or dash's
//! `printf "\0"` prints a NUL is untested. If the capture fails for any
//! reason, the fallback is the daemon's own environment.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Environment variables, in order.
pub type EnvVars = Vec<(OsString, OsString)>;

/// Daemon environment variable: `off` skips the login-shell capture.
pub const LOGIN_ENV_SWITCH: &str = "FLEETD_LOGIN_ENV";

/// The shell whose login env the daemon captures, given the daemon's
/// [`LOGIN_ENV_SWITCH`] and `SHELL` values: none when the switch is `off`
/// or `SHELL` is unset or empty.
pub fn login_shell(switch: Option<&OsStr>, shell: Option<&OsStr>) -> Option<PathBuf> {
    if switch.is_some_and(|v| v == "off") {
        return None;
    }
    shell.filter(|s| !s.is_empty()).map(PathBuf::from)
}

/// How long the login shell may take before the daemon's own env is used.
pub const LOGIN_ENV_TIMEOUT: Duration = Duration::from_secs(5);

/// What the login shell runs: the env between two markers, NUL-separated, so
/// output from the user's rc files around it is ignored.
const SCRIPT: &str = r#"printf "\0__FLEET_ENV__\0"; env -0; printf "\0__FLEET_END__\0""#;
const START: &[u8] = b"\0__FLEET_ENV__\0";
const END: &[u8] = b"\0__FLEET_END__\0";

/// Variables that describe the capturing shell, not the user's setup, and
/// `FLEET_AGENT_ID`, which the node sets for each agent.
const DROPPED: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_", crate::node::AGENT_ID_ENV];

/// Stack of the background capture thread; it only spawns and reads a pipe.
const CAPTURE_STACK: usize = 128 * 1024;

/// Runs `shell -lic '…'` (stdin `/dev/null`, stderr discarded, in its own
/// session and process group) and returns the environment it prints,
/// without `PWD`, `OLDPWD`, `SHLVL`, `_` and `FLEET_AGENT_ID`. `None` if the
/// shell cannot be run, prints no complete environment, or takes longer
/// than `timeout`; the shell's whole process group is then killed.
pub fn capture_login_env(shell: &Path, timeout: Duration) -> Option<EnvVars> {
    capture(shell, timeout, &Mutex::new(None)).ok()
}

/// Why a capture produced no environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureFailure {
    Timeout,
    Failed,
}

impl CaptureFailure {
    fn as_str(self) -> &'static str {
        match self {
            CaptureFailure::Timeout => "timeout",
            CaptureFailure::Failed => "failed",
        }
    }
}

/// [`capture_login_env`], keeping the shell's pid in `running` while it may
/// be signalled (it is cleared before the shell is reaped).
fn capture(
    shell: &Path,
    timeout: Duration,
    running: &Mutex<Option<u32>>,
) -> Result<EnvVars, CaptureFailure> {
    let deadline = Instant::now() + timeout;
    let mut cmd = Command::new(shell);
    cmd.arg("-lic")
        .arg(SCRIPT)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Its own session, so its own process group (killed as one) and no
    // controlling terminal: an rc file that touches /dev/tty fails at once
    // instead of being stopped by SIGTTIN/SIGTTOU until the timeout.
    // SAFETY: `setsid` is async-signal-safe; nothing else runs in the child.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|_| CaptureFailure::Failed)?;
    *lock(running) = Some(child.id());
    let stdout = child.stdout.take().expect("piped stdout");
    let out = read_until_end(stdout, deadline);
    // A shell that printed its env exits by itself; wait for that until the
    // deadline (so its logout files run), then kill its group.
    let exited = out.is_ok() && wait_until(&mut child, deadline);
    {
        let mut running = lock(running);
        if !exited {
            kill_group(child.id());
        }
        *running = None;
    }
    let _ = child.wait();
    let out = out.map_err(|e| match e.kind() {
        io::ErrorKind::TimedOut => CaptureFailure::Timeout,
        _ => CaptureFailure::Failed,
    })?;
    parse_env_dump(&out).ok_or(CaptureFailure::Failed)
}

/// Reads `stdout` until the end marker, EOF or the deadline. Stops at the
/// marker, so a background process holding the pipe open cannot stall it.
fn read_until_end(mut stdout: impl Read + AsRawFd, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let mut pfd = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = libc::c_int::try_from(left.as_millis().max(1)).unwrap_or(libc::c_int::MAX);
        // SAFETY: one valid pollfd.
        match unsafe { libc::poll(&mut pfd, 1, ms) } {
            0 => continue,
            n if n < 0 => {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            _ => {}
        }
        match stdout.read(&mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => {
                // Only the new bytes (and a marker straddling the boundary)
                // need searching.
                let from = out.len().saturating_sub(END.len() - 1);
                out.extend_from_slice(&buf[..n]);
                if find(&out[from..], END).is_some() {
                    return Ok(out);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Whether `child` exited before `deadline`.
fn wait_until(child: &mut Child, deadline: Instant) -> bool {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Err(_) => return false,
            Ok(None) if Instant::now() >= deadline => return false,
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn kill_group(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: plain signal to a process group we created; its leader is
        // not yet reaped, so the id cannot have been reused.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Parses the shell's output: the `K=V\0` entries between the first start
/// marker and the end marker after it, without the [`DROPPED`] variables.
/// `None` without both markers.
pub fn parse_env_dump(out: &[u8]) -> Option<EnvVars> {
    let body = &out[find(out, START)? + START.len()..];
    // `env -0` ends each entry with NUL and the end marker starts with one,
    // so an entry can never contain the marker.
    let body = &body[..find(body, END)?];
    let mut vars: EnvVars = body
        .split(|&b| b == 0)
        .filter_map(|entry| {
            let eq = entry.iter().position(|&b| b == b'=')?;
            let (k, v) = (&entry[..eq], &entry[eq + 1..]);
            if k.is_empty() || DROPPED.iter().any(|d| d.as_bytes() == k) {
                return None;
            }
            Some((
                OsStr::from_bytes(k).to_os_string(),
                OsStr::from_bytes(v).to_os_string(),
            ))
        })
        .collect();
    vars.shrink_to_fit();
    Some(vars)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The environment agents start from: a login-shell capture running in the
/// background, or the daemon's own environment.
#[derive(Clone)]
pub struct LoginEnv {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    done: Condvar,
    /// Waiters give up at this point and use the daemon's env.
    deadline: Instant,
    /// The capturing shell's pid while it may be signalled.
    running: Mutex<Option<u32>>,
}

enum State {
    Pending,
    /// `None`: the daemon's own environment.
    Done(Option<Arc<EnvVars>>),
}

impl LoginEnv {
    /// The daemon's own environment, with no capture.
    pub fn daemon() -> LoginEnv {
        LoginEnv::with_state(State::Done(None), Instant::now())
    }

    fn with_state(state: State, deadline: Instant) -> LoginEnv {
        LoginEnv {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                done: Condvar::new(),
                deadline,
                running: Mutex::new(None),
            }),
        }
    }

    /// Starts capturing `shell`'s login environment on a background thread
    /// ([`capture_login_env`]); the deadline for waiters, `timeout`, starts
    /// now. Without a shell, or if the thread cannot start, this is
    /// [`LoginEnv::daemon`].
    pub fn capture_in_background(shell: Option<PathBuf>, timeout: Duration) -> LoginEnv {
        let Some(shell) = shell.filter(|s| !s.as_os_str().is_empty()) else {
            return LoginEnv::daemon();
        };
        let env = LoginEnv::with_state(State::Pending, Instant::now() + timeout);
        let inner = Arc::clone(&env.inner);
        let started = std::thread::Builder::new()
            .name("login-env".into())
            .stack_size(CAPTURE_STACK)
            .spawn(move || {
                let begun = Instant::now();
                let captured = capture(&shell, timeout, &inner.running);
                let duration_ms = begun.elapsed().as_millis() as u64;
                // Only the count: values are never logged.
                match &captured {
                    Ok(vars) => tracing::info!(
                        result = "ok",
                        duration_ms,
                        vars = vars.len(),
                        "login env captured"
                    ),
                    Err(why) => tracing::warn!(
                        result = why.as_str(),
                        duration_ms,
                        shell = ?shell,
                        "login env capture failed; agents start from the daemon's env"
                    ),
                }
                let vars = captured.ok().map(Arc::new);
                let mut state = lock(&inner.state);
                // A waiter that gave up already settled on the daemon's env.
                if matches!(*state, State::Pending) {
                    *state = State::Done(vars);
                }
                inner.done.notify_all();
            });
        match started {
            Ok(_) => env,
            Err(e) => {
                tracing::warn!(error = %e, "starting the login env capture failed");
                LoginEnv::daemon()
            }
        }
    }

    /// The captured environment, waiting for the capture until its deadline;
    /// `None` means the daemon's own environment. Once settled, the answer
    /// never changes.
    pub fn wait(&self) -> Option<Arc<EnvVars>> {
        let inner = &*self.inner;
        let mut state = lock(&inner.state);
        loop {
            match &*state {
                State::Done(vars) => return vars.clone(),
                State::Pending => {
                    let left = inner.deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        *state = State::Done(None);
                        return None;
                    }
                    state = inner
                        .done
                        .wait_timeout(state, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            }
        }
    }

    /// Kills a capture still running (e.g. at shutdown); waiters then get
    /// the daemon's env.
    pub fn cancel(&self) {
        if let Some(pid) = *lock(&self.inner.running) {
            kill_group(pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn vars(v: &EnvVars) -> Vec<(String, String)> {
        v.iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn parses_between_markers_and_drops_shell_vars() {
        let out = b"motd junk\n\0__FLEET_ENV__\0A=1\0PWD=/x\0B=x=y\0SHLVL=2\0_=/usr/bin/env\0\
OLDPWD=/\0FLEET_AGENT_ID=7\0EMPTY=\0\0__FLEET_END__\0logout junk";
        let got = parse_env_dump(out).unwrap();
        assert_eq!(
            vars(&got),
            [
                ("A".into(), "1".into()),
                ("B".into(), "x=y".into()),
                ("EMPTY".into(), String::new())
            ]
        );
    }

    #[test]
    fn multiline_values_survive() {
        let out = b"\0__FLEET_ENV__\0F=a\nb\0\0__FLEET_END__\0";
        assert_eq!(
            vars(&parse_env_dump(out).unwrap()),
            [("F".into(), "a\nb".into())]
        );
    }

    #[test]
    fn switch_off_or_no_shell_means_no_capture() {
        let zsh = Some(OsStr::new("/bin/zsh"));
        assert_eq!(login_shell(None, zsh), Some(PathBuf::from("/bin/zsh")));
        assert_eq!(
            login_shell(Some(OsStr::new("on")), zsh),
            Some(PathBuf::from("/bin/zsh"))
        );
        assert_eq!(login_shell(Some(OsStr::new("off")), zsh), None);
        assert_eq!(login_shell(None, None), None);
        assert_eq!(login_shell(None, Some(OsStr::new(""))), None);
    }

    #[test]
    fn missing_markers_are_none() {
        assert!(parse_env_dump(b"").is_none());
        assert!(parse_env_dump(b"\0__FLEET_ENV__\0A=1\0").is_none());
        assert!(parse_env_dump(b"A=1\0\0__FLEET_END__\0").is_none());
    }

    fn script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("shell.sh");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn captures_a_login_shell() {
        let tmp = tempfile::tempdir().unwrap();
        let shell = script(
            tmp.path(),
            "#!/bin/sh\n[ \"$1\" = -lic ] || exit 9\nread x && exit 8\n\
             [ \"$(ps -o pgid= -p $$ | tr -d ' ')\" = $$ ] || exit 7\n\
             (exec </dev/tty) 2>/dev/null && exit 6\n\
             echo noise\nexport FLEET_CAPTURE_TEST=yes\nexec /bin/sh -c \"$2\"\n",
        );
        let got = capture_login_env(&shell, Duration::from_secs(5)).unwrap();
        let got = vars(&got);
        assert!(got.contains(&("FLEET_CAPTURE_TEST".into(), "yes".into())));
        assert!(!got.iter().any(|(k, _)| k == "PWD" || k == "SHLVL"));
    }

    #[test]
    fn hanging_shell_times_out_and_is_killed() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("pid");
        let shell = script(
            tmp.path(),
            &format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nwait\n",
                pidfile.display()
            ),
        );
        let start = Instant::now();
        assert!(capture_login_env(&shell, Duration::from_secs(1)).is_none());
        assert!(start.elapsed() < Duration::from_secs(3));
        // The shell's background child went with its group.
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        // SAFETY: signal 0 only checks for existence. The orphaned `sleep`
        // is reaped by init, so it disappears once killed.
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(Instant::now() < deadline, "sleep {pid} survived");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn missing_shell_is_none() {
        assert!(capture_login_env(Path::new("/nonexistent/sh"), Duration::from_secs(1)).is_none());
        let env =
            LoginEnv::capture_in_background(Some("/nonexistent/sh".into()), LOGIN_ENV_TIMEOUT);
        assert!(env.wait().is_none());
        assert!(
            LoginEnv::capture_in_background(None, LOGIN_ENV_TIMEOUT)
                .wait()
                .is_none()
        );
    }

    #[test]
    fn waiters_give_up_at_the_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let shell = script(tmp.path(), "#!/bin/sh\nexec sleep 30\n");
        let env = LoginEnv::capture_in_background(Some(shell), Duration::from_millis(200));
        let start = Instant::now();
        assert!(env.wait().is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(env.wait().is_none());
    }
}
