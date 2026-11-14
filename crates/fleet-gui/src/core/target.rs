use std::collections::VecDeque;
use std::fmt;
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::process::{Child, Command};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fleet_client::Connection;

use serde::{Deserialize, Serialize};

/// Which fleetd the GUI talks to.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Target {
    Local,
    /// `dest` is what ssh takes: `user@host` or `host`.
    Ssh {
        dest: String,
    },
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid target {0:?}: expected `local` or `ssh://[user@]host`")]
pub struct ParseTargetError(String);

impl FromStr for Target {
    type Err = ParseTargetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "local" {
            return Ok(Target::Local);
        }
        match s.strip_prefix("ssh://") {
            Some(dest) if !dest.is_empty() => Ok(Target::Ssh {
                dest: dest.to_owned(),
            }),
            _ => Err(ParseTargetError(s.to_owned())),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Local => f.write_str("local"),
            Target::Ssh { dest } => write!(f, "ssh://{dest}"),
        }
    }
}

impl From<Target> for String {
    fn from(t: Target) -> String {
        t.to_string()
    }
}

impl TryFrom<String> for Target {
    type Error = ParseTargetError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

/// Handshake timeout over ssh: connection setup and login shells are slow.
pub const SSH_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

const STDERR_TAIL: usize = 4096;

/// `ssh ... <dest> sh -lc 'exec fleetd --stdio'`, with a shared control
/// master under `<state_dir>/ssh` (created with mode 0700).
pub fn ssh_command(dest: &str, state_dir: &Path) -> std::io::Result<Command> {
    ssh_remote(dest, "exec fleetd --stdio", state_dir)
}

/// `ssh ... <dest> sh -lc '<script>'` over the shared control master
/// (`script` must not contain a single quote).
pub fn ssh_remote(dest: &str, script: &str, state_dir: &Path) -> std::io::Result<Command> {
    use std::os::unix::fs::PermissionsExt;
    let dir = state_dir.join("ssh");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    // An existing directory keeps its old mode otherwise.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let mut cmd = Command::new("ssh");
    cmd.args(["-o", "ControlMaster=auto", "-o"])
        .arg(format!("ControlPath={}/%C", dir.display()))
        .args(["-o", "ControlPersist=60"])
        .arg(dest)
        // One argument: ssh joins them with spaces for the remote shell.
        .arg(format!("sh -lc '{script}'"));
    Ok(cmd)
}

/// Why a connection could not be made; the `Display` copy is shown in the
/// connect sheet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectError {
    NotRunning,
    FleetdNotFound { host: String },
    Ssh { message: String },
    Timeout,
    Other(String),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::NotRunning => f.write_str("fleetd is not running on this Mac"),
            ConnectError::FleetdNotFound { host } => {
                write!(f, "fleetd is not installed on {host}")
            }
            ConnectError::Ssh { message } => write!(f, "ssh: {message}"),
            ConnectError::Timeout => f.write_str("fleetd did not answer in time"),
            ConnectError::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ConnectError {}

pub struct Connected {
    pub conn: Connection,
    /// The ssh child, for an ssh target; the caller reaps or kills it.
    pub child: Option<Child>,
}

/// Connects to `target` and handshakes as `client_name`.
pub fn connect(
    target: &Target,
    state_dir: &Path,
    client_name: &str,
) -> Result<Connected, ConnectError> {
    match target {
        Target::Local => match Connection::unix(&state_dir.join("fleetd.sock"), client_name) {
            Ok(conn) => Ok(Connected { conn, child: None }),
            Err(fleet_client::Error::NotRunning) => Err(ConnectError::NotRunning),
            Err(fleet_client::Error::Timeout) => Err(ConnectError::Timeout),
            Err(e) => Err(ConnectError::Other(e.to_string())),
        },
        Target::Ssh { dest } => {
            let host = dest.rsplit_once('@').map_or(dest.as_str(), |(_, h)| h);
            let cmd = ssh_command(dest, state_dir).map_err(|e| {
                ConnectError::Other(format!("cannot create {}/ssh: {e}", state_dir.display()))
            })?;
            connect_with(cmd, host, client_name, SSH_HANDSHAKE_TIMEOUT)
        }
    }
}

/// Keeps the last [`STDERR_TAIL`] bytes of a child's stderr.
struct StderrTail {
    buf: Arc<Mutex<VecDeque<u8>>>,
    reader: std::thread::JoinHandle<()>,
}

impl StderrTail {
    fn start(mut pipe: std::io::PipeReader) -> Self {
        let buf = Arc::new(Mutex::new(VecDeque::new()));
        let sink = Arc::clone(&buf);
        let reader = std::thread::spawn(move || {
            let mut chunk = [0u8; 1024];
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut b = sink.lock().unwrap_or_else(|e| e.into_inner());
                b.extend(&chunk[..n]);
                let excess = b.len().saturating_sub(STDERR_TAIL);
                b.drain(..excess);
            }
        });
        StderrTail { buf, reader }
    }

    /// What was written so far, after giving the reader a moment to reach
    /// the end of the pipe.
    fn text(self) -> String {
        let deadline = Instant::now() + Duration::from_millis(500);
        while !self.reader.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let b = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        let bytes: Vec<u8> = b.iter().copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

fn connect_with(
    mut cmd: Command,
    host: &str,
    client_name: &str,
    timeout: Duration,
) -> Result<Connected, ConnectError> {
    let (pipe, writer) = std::io::pipe().map_err(|e| ConnectError::Other(e.to_string()))?;
    cmd.stderr(writer);
    let tail = StderrTail::start(pipe);
    match Connection::command_with_timeout(cmd, client_name, timeout) {
        // On success the tail thread keeps draining ssh's stderr so a
        // chatty ssh never blocks; it ends when the child does.
        Ok((conn, child)) => Ok(Connected {
            conn,
            child: Some(child),
        }),
        Err(e) => {
            let stderr = tail.text();
            let stderr = stderr.trim();
            if stderr.contains("fleetd: not found") || stderr.contains("command not found") {
                return Err(ConnectError::FleetdNotFound {
                    host: host.to_owned(),
                });
            }
            if matches!(e, fleet_client::Error::ChildExited { status: Some(127) }) {
                return Err(ConnectError::FleetdNotFound {
                    host: host.to_owned(),
                });
            }
            if matches!(e, fleet_client::Error::Timeout) {
                return Err(ConnectError::Timeout);
            }
            match stderr.lines().last() {
                Some(line) => Err(ConnectError::Ssh {
                    message: line.trim().to_owned(),
                }),
                None => Err(ConnectError::Other(e.to_string())),
            }
        }
    }
}

/// What the GUI does at launch.
#[derive(Debug, PartialEq, Eq)]
pub enum Startup {
    /// Connect to the target; show the connect sheet if that fails.
    Connect(Target),
    AskUser {
        reason: Option<String>,
    },
}

pub fn startup_action(saved: Option<&Target>, local_answers: bool) -> Startup {
    match saved {
        Some(t) => Startup::Connect(t.clone()),
        None if local_answers => Startup::Connect(Target::Local),
        None => Startup::AskUser { reason: None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parses_and_formats() {
        for s in ["local", "ssh://me@box", "ssh://box"] {
            assert_eq!(s.parse::<Target>().unwrap().to_string(), s);
        }
        assert!("ftp://x".parse::<Target>().is_err());
        assert!("ssh://".parse::<Target>().is_err());
    }

    #[test]
    fn ssh_command_argv_is_exact() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = ssh_command("me@box", dir.path()).unwrap();
        assert_eq!(cmd.get_program(), "ssh");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-o".to_owned(),
                "ControlMaster=auto".into(),
                "-o".into(),
                format!("ControlPath={}/ssh/%C", dir.path().display()),
                "-o".into(),
                "ControlPersist=60".into(),
                "me@box".into(),
                "sh -lc 'exec fleetd --stdio'".into(),
            ]
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("ssh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn ssh_exit_127_maps_to_fleetd_not_found() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo \"sh: fleetd: not found\" >&2; exit 127"]);
        let err = connect_with(cmd, "box", "t", Duration::from_secs(5))
            .err()
            .expect("must fail");
        assert_eq!(err, ConnectError::FleetdNotFound { host: "box".into() });
        assert_eq!(err.to_string(), "fleetd is not installed on box");
    }

    #[test]
    fn ssh_exit_127_without_stderr_maps_to_fleetd_not_found() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 127"]);
        let err = connect_with(cmd, "box", "t", Duration::from_secs(5))
            .err()
            .expect("must fail");
        assert_eq!(err, ConnectError::FleetdNotFound { host: "box".into() });
    }

    #[test]
    fn ssh_command_fixes_mode_of_existing_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("ssh")).unwrap();
        std::fs::set_permissions(
            dir.path().join("ssh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        ssh_command("box", dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join("ssh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn ssh_other_stderr_maps_to_ssh_error() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo 'Permission denied (publickey).' >&2; exit 255"]);
        let err = connect_with(cmd, "box", "t", Duration::from_secs(5))
            .err()
            .expect("must fail");
        assert_eq!(err.to_string(), "ssh: Permission denied (publickey).");
    }

    #[test]
    fn startup_action_table() {
        let ssh = Target::Ssh { dest: "box".into() };
        for local in [true, false] {
            assert_eq!(
                startup_action(Some(&ssh), local),
                Startup::Connect(ssh.clone())
            );
        }
        assert_eq!(startup_action(None, true), Startup::Connect(Target::Local));
        assert_eq!(
            startup_action(None, false),
            Startup::AskUser { reason: None }
        );
    }
}
