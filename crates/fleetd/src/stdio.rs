//! `fleetd --stdio`: a byte bridge between stdin/stdout and the daemon's
//! socket, so a remote client can run `ssh host fleetd --stdio` (ADR 0019,
//! 0026). It never takes the instance lock itself: if no daemon answers it
//! starts one, detached, and the lock (ADR 0013) decides between racing
//! starters.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::Config;

/// How long to wait for a daemon this bridge started.
pub const START_TIMEOUT: Duration = Duration::from_secs(5);

const POLL: Duration = Duration::from_millis(5);

/// Exit code of a daemon that lost the instance lock.
const ALREADY_RUNNING: i32 = 2;

/// Bridges stdin/stdout to the daemon on `cfg`, starting it if needed, until
/// either side closes. Returns an error if no daemon could be reached.
pub fn run(cfg: &Config) -> Result<(), String> {
    let stream = connect_or_start(cfg)?;
    bridge(stream).map_err(|e| format!("bridge: {e}"))
}

fn connect_or_start(cfg: &Config) -> Result<UnixStream, String> {
    let sock = cfg.socket_path();
    if let Ok(s) = UnixStream::connect(&sock) {
        return Ok(s);
    }
    let mut child = start_daemon(cfg).map_err(|e| format!("starting fleetd: {e}"))?;
    let deadline = Instant::now() + START_TIMEOUT;
    let mut reaped = false;
    loop {
        if let Ok(s) = UnixStream::connect(&sock) {
            return Ok(s);
        }
        if !reaped && let Ok(Some(status)) = child.try_wait() {
            reaped = true;
            // Losing the lock means another daemon is starting: wait for it.
            if status.code() != Some(ALREADY_RUNNING) {
                return Err(format!("fleetd failed to start ({status})"));
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "fleetd did not come up within {}s at {}",
                START_TIMEOUT.as_secs(),
                sock.display()
            ));
        }
        std::thread::sleep(POLL);
    }
}

/// Starts this binary as a daemon on `cfg`'s state dir, detached: its own
/// session (so its own process group, and no controlling terminal), stdio on
/// `/dev/null`. It outlives this bridge and the SSH session that ran it.
fn start_daemon(cfg: &Config) -> io::Result<Child> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--state-dir")
        .arg(&cfg.state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe; nothing else runs in the child.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// Copies stdin to `stream` (half-closing it at EOF) and `stream` to stdout,
/// returning when the daemon side closes or stdout fails.
fn bridge(stream: UnixStream) -> io::Result<()> {
    let mut up = stream.try_clone()?;
    std::thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            let _ = io::copy(&mut io::stdin().lock(), &mut up);
            // The daemon sees EOF and closes, which ends the other direction.
            let _ = up.shutdown(Shutdown::Write);
        })?;
    let mut down = stream;
    let mut out = io::stdout().lock();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match down.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        // `stdout` is line-buffered; frames are binary, so flush each read.
        out.write_all(&buf[..n])?;
        out.flush()?;
    }
}
