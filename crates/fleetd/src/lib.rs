//! fleetd: the single-instance node daemon, serving the fleet protocol on a
//! unix socket in the state dir.

pub mod handoff;
pub mod logging;
mod server;
pub mod stdio;

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fleet_node::env::{LOGIN_ENV_SWITCH, LOGIN_ENV_TIMEOUT, LoginEnv, login_shell};
use fleet_node::node::Node;
use fleet_proto::handoff::{HandoffRecord, handoff_agent};
use fleet_proto::{HANDOFF_VERSION, PROTOCOL_VERSION, RestoreSummary};

use fs4::{FileExt, TryLockError};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::server::{RESTART_FLUSH, RestartReply, RestartRequest, Server};

/// Bound on the new binary's `--handoff-check`.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(2);
/// Bound on stopping requests and freezing the node for a restart.
pub const FREEZE_DEADLINE: Duration = Duration::from_secs(3);

/// Longest socket path macOS accepts: `sun_path` is 104 bytes including the NUL.
const MAX_SOCKET_PATH_BYTES: usize = 103;

/// Pause after EMFILE/ENFILE so a full fd table does not spin the accept loop.
const ACCEPT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(10);

// errno values are identical on macOS and Linux.
const ENFILE: i32 = 23;
const EMFILE: i32 = 24;

fn is_fd_exhaustion(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(ENFILE | EMFILE))
}

/// `OPEN_MAX` from macOS `<sys/syslimits.h>` (not exported by `libc`).
const MACOS_OPEN_MAX: libc::rlim_t = 10240;

/// Raises the soft limit on open files to the hard limit (on macOS at most
/// `OPEN_MAX`, which `setrlimit` requires), so ≥ 50 agents fit under a low
/// default soft limit. Returns the soft limit now in effect. Called by
/// `main`, not [`startup`], which is benchmarked.
pub fn raise_fd_limit() -> io::Result<libc::rlim_t> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit/setrlimit with a valid, initialised struct.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut target = lim.rlim_max;
    if cfg!(target_os = "macos") {
        target = target.min(MACOS_OPEN_MAX);
    }
    if target > lim.rlim_cur {
        let raised = libc::rlimit {
            rlim_cur: target,
            rlim_max: lim.rlim_max,
        };
        // SAFETY: as above.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } != 0 {
            return Err(io::Error::last_os_error());
        }
        lim.rlim_cur = target;
    }
    Ok(lim.rlim_cur)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub state_dir: PathBuf,
}

impl Config {
    pub fn socket_path(&self) -> PathBuf {
        self.state_dir.join("fleetd.sock")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.state_dir.join("fleetd.lock")
    }
}

impl Default for Config {
    /// `~/.fleet`, resolved through `$HOME`.
    fn default() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        Config {
            state_dir: home.join(".fleet"),
        }
    }
}

#[derive(Debug)]
pub enum StartupError {
    AlreadyRunning,
    SocketPathTooLong(PathBuf),
    Io(io::Error),
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning => write!(f, "fleetd already running"),
            Self::SocketPathTooLong(p) => write!(f, "socket path too long: {}", p.display()),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for StartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for StartupError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// A started daemon: holds the instance lock and the bound (non-blocking) listener.
pub struct Daemon {
    lock: File,
    listener: StdUnixListener,
    socket_path: PathBuf,
    origin: Origin,
}

/// How this daemon process came to be, which decides how the node opens.
enum Origin {
    /// A normal start.
    Fresh,
    /// Started by a predecessor's `exec` whose handoff it could not use: the
    /// agents were ended (see [`handoff::abandon`]), and it starts normally
    /// (see [`Daemon::fell_back`]).
    FellBack,
    /// Started by a predecessor's `exec` from this record (see
    /// [`Daemon::adopted`]).
    Handoff(HandoffRecord),
}

/// Makes `dir` (and missing parents) exist with mode 0700. The common case,
/// an existing 0700 dir, costs one `stat` and no `chmod`.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    let private = || fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    match fs::metadata(dir) {
        Ok(m) if m.is_dir() => {
            if m.permissions().mode() & 0o777 != 0o700 {
                private()?;
            }
            Ok(())
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", dir.display()),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
            // The umask may have narrowed the mode.
            private()
        }
        Err(e) => Err(e),
    }
}

/// Synchronous (no tokio runtime needed): create the state dir (mode 0700,
/// also enforced on an existing one), take the instance lock, replace any
/// stale socket, bind. The node's state is opened by [`Daemon::serve`].
pub fn startup(cfg: &Config) -> Result<Daemon, StartupError> {
    ensure_private_dir(&cfg.state_dir)?;

    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cfg.lock_path())?;
    match FileExt::try_lock(&lock) {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Err(StartupError::AlreadyRunning),
        Err(TryLockError::Error(e)) => return Err(StartupError::Io(e)),
    }

    let socket_path = cfg.socket_path();
    if socket_path.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        return Err(StartupError::SocketPathTooLong(socket_path));
    }

    // The lock is ours, so no other fleetd is mid-restart on this dir.
    handoff::remove_stale(&cfg.state_dir);

    match fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = StdUnixListener::bind(&socket_path)?;
    listener.set_nonblocking(true)?;

    Ok(Daemon {
        lock,
        listener,
        socket_path,
        origin: Origin::Fresh,
    })
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl Daemon {
    /// A daemon started by a predecessor's `exec` from its handoff record
    /// `rec`: the instance lock and the listener are the predecessor's,
    /// inherited (see [`handoff::adopt_listener_and_lock`]), so nothing is
    /// bound or locked; [`Daemon::serve`] adopts the record's agents.
    pub fn adopted(
        cfg: &Config,
        listener: StdUnixListener,
        lock: File,
        rec: HandoffRecord,
    ) -> Daemon {
        Daemon {
            lock,
            listener,
            socket_path: cfg.socket_path(),
            origin: Origin::Handoff(rec),
        }
    }

    /// Marks a normally started daemon as one whose handoff could not be
    /// used (its handshakes report `fell_back`).
    pub fn fell_back(mut self) -> Daemon {
        self.origin = Origin::FellBack;
        self
    }

    /// Start capturing the login shell's environment in the background
    /// unless `FLEETD_LOGIN_ENV=off` (see [`fleet_node::env`]), open the
    /// node's state (adopting the handed-over agents, see
    /// [`Daemon::adopted`]), then serve protocol connections until
    /// SIGTERM/SIGINT/SIGHUP, or until a `RestartDaemon` `exec`s a new
    /// binary in this process (see `Restart::run`; one that fails is
    /// undone and serving goes on). Then stop every agent (decision 0018): SIGTERM
    /// to each agent's process group, up to 2 s grace, then SIGKILL; their
    /// exits are recorded. Finally remove the socket file and return.
    pub async fn serve(self) -> io::Result<()> {
        let listener = UnixListener::from_std(self.listener)?;
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        let mut hup = signal(SignalKind::hangup())?;
        // `socket_path` is `<state_dir>/fleetd.sock` (see `Config::socket_path`).
        let state_dir = self
            .socket_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        info!(
            version = env!("CARGO_PKG_VERSION"),
            pid = std::process::id(),
            state_dir = ?state_dir,
            protocol = PROTOCOL_VERSION,
            "fleetd started"
        );
        let shell = login_shell(
            std::env::var_os(LOGIN_ENV_SWITCH).as_deref(),
            std::env::var_os("SHELL").as_deref(),
        );
        if shell.is_none() {
            info!(
                "login env capture skipped ({LOGIN_ENV_SWITCH}=off or no SHELL); agents start from the daemon's env"
            );
        }
        let (node, restore) = match self.origin {
            Origin::Handoff(rec) => {
                // The handed-over terminals are claimed before the capture
                // opens a terminal of its own.
                let dir = state_dir.clone();
                let agents = rec.agents;
                let (node, kept, failed) =
                    tokio::task::spawn_blocking(move || Node::open_with_handoff(&dir, agents))
                        .await
                        .map_err(io::Error::other)??;
                let login_env = LoginEnv::capture_in_background(shell, LOGIN_ENV_TIMEOUT);
                info!(
                    kept = kept.len(),
                    failed = failed.len(),
                    frozen_ms = unix_ms().saturating_sub(rec.frozen_unix_ms),
                    "restarted from a handoff; serving"
                );
                let restore = RestoreSummary {
                    previous_instance: rec.previous_instance,
                    kept,
                    failed,
                    ..Default::default()
                };
                (node.with_base_env(login_env), restore)
            }
            origin => {
                // In the background, overlapping opening the store; the
                // first spawn waits for it (bounded), nothing else does.
                let login_env = LoginEnv::capture_in_background(shell, LOGIN_ENV_TIMEOUT);
                let dir = state_dir.clone();
                let node = tokio::task::spawn_blocking(move || {
                    Node::open(&dir).map(|n| n.with_base_env(login_env))
                })
                .await
                .map_err(io::Error::other)??;
                let restore = RestoreSummary {
                    fell_back: matches!(origin, Origin::FellBack),
                    ..Default::default()
                };
                (node, restore)
            }
        };
        let node = Arc::new(node);
        let identity = handoff::Identity::current();
        let instance = identity.instance;
        // One restart at a time: the server refuses a second while one is
        // pending, so the channel never holds more than one.
        let (restarts_tx, mut restarts) = mpsc::channel(1);
        let server = Server::new(Arc::clone(&node), identity, restore, restarts_tx);
        // The restart whose binary is being checked, and the check.
        let mut checking: Option<(RestartRequest, PathBuf)> = None;
        let mut check: Option<JoinHandle<Result<(), String>>> = None;
        let signal = loop {
            tokio::select! {
                _ = term.recv() => break "SIGTERM",
                _ = int.recv() => break "SIGINT",
                _ = hup.recv() => break "SIGHUP",
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => server.accept(stream),
                    // Accept errors never end `serve`; only a signal does.
                    Err(e) => {
                        warn!(error = %e, "accept failed");
                        if is_fd_exhaustion(&e) {
                            tokio::time::sleep(ACCEPT_BACKOFF).await;
                        }
                    }
                },
                Some(req) = restarts.recv(), if check.is_none() => {
                    match handoff::resolve_binary(&req.binary) {
                        Ok(binary) => {
                            info!(binary = ?binary, "restart: checking the new binary");
                            let b = binary.clone();
                            check = Some(tokio::task::spawn_blocking(move || {
                                let checked = handoff::run_check(&b, CHECK_TIMEOUT);
                                if checked.is_ok() {
                                    handoff::test_delay_after_check();
                                }
                                checked
                            }));
                            checking = Some((req, binary));
                        }
                        Err(reason) => refuse(&server, req.reply, &reason),
                    }
                }
                checked = async { check.as_mut().expect("guarded").await }, if check.is_some() => {
                    check = None;
                    let (req, binary) = checking.take().expect("set with the check");
                    let checked = checked.map_err(|e| e.to_string()).and_then(|c| c);
                    let mut reply = Some(req.reply);
                    let reason = match checked {
                        Ok(()) => {
                            info!("restart: check passed");
                            let restart = Restart {
                                server: &server,
                                node: &node,
                                state_dir: &state_dir,
                                instance,
                                listener_fd: listener.as_raw_fd(),
                                lock_fd: self.lock.as_raw_fd(),
                            };
                            restart.run(&binary, &mut reply).await
                        }
                        Err(reason) => reason,
                    };
                    match reply {
                        Some(reply) => refuse(&server, reply, &reason),
                        None => {
                            warn!(reason, "restart failed after its answer; serving on");
                            server.restart_failed(&reason);
                        }
                    }
                }
            }
        };
        info!(signal, "fleetd stopping: stopping agents");
        tokio::task::spawn_blocking(move || node.shutdown())
            .await
            .map_err(io::Error::other)?;
        match fs::remove_file(&self.socket_path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Answers a restart with an `Error`: it failed, and the daemon serves on.
fn refuse(server: &Server, reply: oneshot::Sender<RestartReply>, reason: &str) {
    warn!(reason, "restart refused; serving on");
    let _ = reply.send(RestartReply::Refused(reason.to_owned()));
    server.restart_failed(reason);
}

/// A restart whose binary passed the check: what [`Restart::run`] needs.
/// Only the serve loop runs one (it is the single restart coordinator, the
/// only caller of [`Node::prepare_handoff`] and [`Node::thaw`]).
struct Restart<'a> {
    server: &'a Server,
    node: &'a Arc<Node>,
    state_dir: &'a Path,
    instance: u64,
    listener_fd: RawFd,
    lock_fd: RawFd,
}

impl Restart<'_> {
    /// Stops requests, freezes the node, writes the handoff record, answers
    /// the requester `RestartingDaemon` (taking `reply`), waits for that to
    /// be written (at most [`RESTART_FLUSH`]) and `exec`s `binary`. Accepting
    /// stops meanwhile: the serve loop is here, and connections wait in the
    /// listener's backlog for whichever daemon serves next.
    ///
    /// Returns only if something failed before or at the `exec`, with the
    /// reason, everything undone: the node thawed, requests admitted again,
    /// no record left. `reply` is still there if the requester was not
    /// answered yet.
    async fn run(
        &self,
        binary: &Path,
        reply: &mut Option<oneshot::Sender<RestartReply>>,
    ) -> String {
        let started = Instant::now();
        let frozen_unix_ms = unix_ms();
        let deadline = started + FREEZE_DEADLINE;
        let Some(stopped) = self.server.stop_requests(deadline.into()).await else {
            return "agents are starting; try again".into();
        };
        let node = Arc::clone(self.node);
        let prepared = tokio::task::spawn_blocking(move || node.prepare_handoff(deadline))
            .await
            .map_err(io::Error::other)
            .and_then(|r| r);
        let agents = match prepared {
            Ok(agents) => agents,
            // Nothing stays frozen (see `prepare_handoff`).
            Err(e) => return e.to_string(),
        };
        let mut keep = vec![self.listener_fd, self.lock_fd];
        keep.extend(agents.iter().filter_map(|a| match a.terminal {
            Some(handoff_agent::Terminal::Fd(fd)) => Some(fd),
            _ => None,
        }));
        let pids: Vec<i32> = agents.iter().map(|a| a.pid).collect();
        let count = agents.len() as u32;
        let rec = HandoffRecord {
            version: HANDOFF_VERSION,
            listener_fd: self.listener_fd,
            lock_fd: self.lock_fd,
            previous_instance: self.instance,
            agents,
            frozen_unix_ms,
        };
        let reason = match handoff::write_record(self.state_dir, &rec) {
            Ok(record_fd) => {
                drop(rec);
                handoff::test_corrupt_record(self.state_dir);
                info!(
                    agents = count,
                    froze_in_ms = started.elapsed().as_millis() as u64,
                    "restart: agents frozen and recorded; exec"
                );
                let (flushed, written) = oneshot::channel();
                if let Some(r) = reply.take() {
                    let restarting = RestartReply::Restarting {
                        agents: count,
                        flushed,
                    };
                    // A requester that has gone does not stop the restart.
                    if r.send(restarting).is_ok() {
                        let _ = tokio::time::timeout(RESTART_FLUSH, written).await;
                    }
                }
                let e = handoff::exec(binary, self.state_dir, record_fd, &keep, &pids);
                format!("exec {}: {e}", binary.display())
            }
            Err(e) => format!("writing the handoff record: {e}"),
        };
        handoff::remove_stale(self.state_dir);
        self.node.thaw();
        drop(stopped);
        reason
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_dir_is_created_nested_and_rejects_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a/b");
        ensure_private_dir(&nested).unwrap();
        let mode = fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        ensure_private_dir(&nested).unwrap();

        let file = tmp.path().join("f");
        fs::write(&file, b"").unwrap();
        let err = ensure_private_dir(&file).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotADirectory);
    }

    #[test]
    fn fd_exhaustion_is_detected_and_other_errors_are_not() {
        assert!(is_fd_exhaustion(&io::Error::from_raw_os_error(EMFILE)));
        assert!(is_fd_exhaustion(&io::Error::from_raw_os_error(ENFILE)));
        assert!(!is_fd_exhaustion(&io::Error::from(
            io::ErrorKind::ConnectionAborted
        )));
        assert!(!is_fd_exhaustion(&io::Error::from_raw_os_error(4)));
    }
}
