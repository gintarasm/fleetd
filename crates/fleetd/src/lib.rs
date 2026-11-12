//! fleetd: single-instance daemon stub. Protocol arrives in M1.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::PathBuf;

use fs4::{FileExt, TryLockError};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};

/// Longest socket path macOS accepts: `sun_path` is 104 bytes including the NUL.
const MAX_SOCKET_PATH_BYTES: usize = 103;

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
    _lock: File,
    listener: StdUnixListener,
    socket_path: PathBuf,
}

/// Synchronous (no tokio runtime needed): create the state dir, take the
/// instance lock, replace any stale socket, bind.
pub fn startup(cfg: &Config) -> Result<Daemon, StartupError> {
    fs::create_dir_all(&cfg.state_dir)?;

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

    match fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = StdUnixListener::bind(&socket_path)?;
    listener.set_nonblocking(true)?;

    Ok(Daemon {
        _lock: lock,
        listener,
        socket_path,
    })
}

impl Daemon {
    /// Accept and immediately close connections until SIGTERM/SIGINT, then
    /// remove the socket file and return.
    pub async fn serve(self) -> io::Result<()> {
        let listener = UnixListener::from_std(self.listener)?;
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        loop {
            tokio::select! {
                _ = term.recv() => break,
                _ = int.recv() => break,
                accepted = listener.accept() => {
                    // Protocol arrives in M1: drop the stream, closing it.
                    drop(accepted?);
                }
            }
        }
        match fs::remove_file(&self.socket_path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}
