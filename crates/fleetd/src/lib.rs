//! fleetd: the single-instance node daemon, serving the fleet protocol on a
//! unix socket in the state dir.

mod approval;
pub mod handoff;
pub mod logging;
mod primitives;
mod restart;
mod scope;
mod server;
pub mod stdio;
pub mod stop_signal;
mod wait;

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fleet_node::env::{LOGIN_ENV_SWITCH, LOGIN_ENV_TIMEOUT, LoginEnv, login_shell};
use fleet_node::node::Node;
use fleet_proto::handoff::HandoffRecord;
use fleet_proto::{PROTOCOL_VERSION, RestoreSummary};

use fs4::{FileExt, TryLockError};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::server::Server;
use crate::stop_signal::StopSignal;

pub use restart::{CHECK_TIMEOUT, FREEZE_DEADLINE};

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

/// The node's agent priority setting from `FLEETD_AGENT_NICE` (ADR 0066),
/// logged; an invalid value is logged and treated as off.
fn agent_nice_setting() -> u8 {
    use fleet_node::priority::{AGENT_NICE_VAR, parse_setting};
    match parse_setting(std::env::var_os(AGENT_NICE_VAR).as_deref()) {
        Ok(0) => 0,
        Ok(nice) => {
            info!(nice, "agents run lowered ({AGENT_NICE_VAR})");
            nice
        }
        Err(e) => {
            warn!("{e}; agents keep fleetd's priority");
            0
        }
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
    /// [`Daemon::adopted`]), with the pids it passed: ended if this start
    /// fails before the node has adopted them (see `restart::HandedAgents`).
    Handoff(HandoffRecord, restart::HandedAgents),
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

impl Daemon {
    /// A daemon started by a predecessor's `exec` from its handoff record
    /// `rec`: the instance lock and the listener are the predecessor's,
    /// inherited (see [`handoff::adopt_listener_and_lock`]), so nothing is
    /// bound or locked; [`Daemon::serve`] adopts the record's agents.
    ///
    /// `pids` are the agents' pids the predecessor passed (see
    /// [`handoff::Handed`]). Every one the node has not adopted is ended:
    /// once it opens, those the record left out; if the daemon is dropped or
    /// `serve` fails before then, all of them.
    pub fn adopted(
        cfg: &Config,
        listener: StdUnixListener,
        lock: File,
        rec: HandoffRecord,
        pids: Vec<i32>,
    ) -> Daemon {
        Daemon {
            lock,
            listener,
            socket_path: cfg.socket_path(),
            origin: Origin::Handoff(rec, restart::HandedAgents::new(pids, cfg.state_dir.clone())),
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
    /// SIGTERM/SIGINT/SIGHUP (armed by [`stop_signal::arm`] before the socket
    /// was bound; one received before `serve` counts), or until a `RestartDaemon` `exec`s a new
    /// binary in this process (see `Restart::run`; one that fails is
    /// undone and serving goes on). Then stop every agent (decision 0018): SIGTERM
    /// to each agent's process group, up to 2 s grace, then SIGKILL; their
    /// exits are recorded. Finally remove the socket file and return.
    pub async fn serve(self, stop: StopSignal) -> io::Result<()> {
        let listener = UnixListener::from_std(self.listener)?;
        let stop = stop.recv();
        tokio::pin!(stop);
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
            // Restart-only code runs on blocking threads (see `restart`):
            // kept out of `serve`'s own code, which is paged in on every
            // start (idle RSS is gated).
            Origin::Handoff(rec, handed) => {
                restart::open_from_handoff(state_dir.clone(), rec, handed, shell)
                    .await
                    .map_err(io::Error::other)??
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
        let node = Arc::new(
            node.with_agent_nice(agent_nice_setting())
                .with_quotas(scope::read_quotas(&state_dir)),
        );
        let identity = handoff::Identity::current();
        let instance = identity.instance;
        // What a restart with no binary named restarts onto.
        let own_binary = identity.binary.path.clone();
        let server = Server::new(Arc::clone(&node), identity, restore);
        let leases = tokio::spawn(server::lease::drive(Arc::clone(&node)));
        let restarts = server.restarts();
        // The restart whose binary is being checked.
        let mut check: Option<JoinHandle<restart::Checked>> = None;
        let signal = loop {
            tokio::select! {
                signal = &mut stop => break signal?,
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
                // One restart at a time: the server refuses a second while
                // one is pending.
                req = restarts.next(), if check.is_none() => {
                    check = restart::check(&restarts, req, &own_binary);
                }
                checked = async { check.as_mut().expect("guarded").await }, if check.is_some() => {
                    check = None;
                    let finished = match checked {
                        Ok(checked) => restart::Restart {
                            restarts: restarts.clone(),
                            node: Arc::clone(&node),
                            state_dir: state_dir.clone(),
                            instance,
                            listener_fd: listener.as_raw_fd(),
                            lock_fd: self.lock.as_raw_fd(),
                        }
                        .finish(checked)
                        .await,
                        Err(e) => Err(e),
                    };
                    if let Err(e) = finished {
                        restarts.failed(&format!("restart: {e}"));
                    }
                }
            }
        };
        info!(signal, "fleetd stopping: stopping agents");
        leases.abort();
        tokio::task::spawn_blocking(move || {
            // Execs (ADR 0091) run in their own sessions: nothing else
            // would end them.
            fleet_node::primitives::kill_all_execs();
            node.shutdown()
        })
        .await
        .map_err(io::Error::other)?;
        match fs::remove_file(&self.socket_path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
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
