use std::path::PathBuf;
use std::process::ExitCode;

use fleetd::handoff::{self, Handed};
use fleetd::logging::{self, DAEMON_LOG, STDIO_LOG};
use fleetd::{Config, Daemon, StartupError, raise_fd_limit, startup};
use tracing::{error, info, warn};

/// Logs a fatal error and prints it to stderr: the log may be unavailable,
/// and whoever started the daemon may capture stderr.
fn fatal(msg: &str) {
    error!("{msg}");
    eprintln!("fleetd: {msg}");
}

/// What a predecessor handed over, ready to serve from; or why it could not
/// be used.
fn take_handoff(cfg: &Config, handed: &Handed) -> std::io::Result<Daemon> {
    let fd = handed.record_fd.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("bad {}", handoff::HANDOFF_FD_ENV),
        )
    })?;
    let rec = handoff::take_record(fd, &cfg.state_dir)?;
    let (listener, lock) = handoff::adopt_listener_and_lock(&rec, &cfg.socket_path())?;
    Ok(Daemon::adopted(cfg, listener, lock, rec))
}

fn main() -> ExitCode {
    // Before anything else (logging, the login-env capture, any thread):
    // the handoff variables leave the environment, so no agent, capture or
    // later restart sees them.
    // SAFETY: the first statement of `main`; no other thread exists.
    let handed = unsafe { handoff::take_handed() };

    // Before logging or any state dir: the old daemon runs this on the new
    // binary and it must touch nothing (no lock, no log, no directory).
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == "--handoff-check")
    {
        print!("{}", fleetd::handoff::check_output());
        return ExitCode::SUCCESS;
    }

    let mut cfg = Config::default();
    let mut stdio = false;
    let mut args = std::env::args_os().skip(1);
    // Usage errors go to stderr only: there is no state dir to log to yet.
    while let Some(arg) = args.next() {
        if arg == "--stdio" {
            stdio = true;
        } else if arg == "--state-dir" {
            let Some(dir) = args.next() else {
                eprintln!("fleetd: --state-dir requires a path");
                return ExitCode::from(64);
            };
            cfg.state_dir = PathBuf::from(dir);
        } else {
            eprintln!("usage: fleetd [--stdio] [--state-dir <path>]");
            return ExitCode::from(64);
        }
    }

    if stdio {
        // Never stdout: it carries the protocol. Without a log file the
        // bridge still works.
        let _log = logging::init(&cfg.state_dir, STDIO_LOG).ok();
        return match fleetd::stdio::run(&cfg) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                error!("fleetd --stdio: {e}");
                // The bridge's stderr is the remote client's.
                eprintln!("fleetd --stdio: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // Before this process opens files of its own: the record is taken (its
    // listener and lock adopted), or the handoff abandoned, which closes
    // every inherited fd.
    let (adopted, fell_back) = match &handed {
        None => (None, None),
        Some(handed) => match take_handoff(&cfg, handed) {
            Ok(daemon) => (Some(daemon), None),
            Err(e) => {
                handoff::abandon(&handed.pids);
                (None, Some(e))
            }
        },
    };

    let log = logging::init(&cfg.state_dir, DAEMON_LOG);
    let log_error = log.as_ref().err().map(ToString::to_string);
    let _log = log.ok();
    if let Some(e) = &fell_back {
        warn!(error = %e, "the handoff could not be used; its agents were ended, starting normally");
    }

    match raise_fd_limit() {
        Ok(limit) => info!(limit, "open-file limit"),
        Err(e) => warn!(error = %e, "raising the open-file limit failed"),
    }

    let started = match adopted {
        Some(daemon) => Ok(daemon),
        None if fell_back.is_some() => startup(&cfg).map(Daemon::fell_back),
        None => startup(&cfg),
    };
    let daemon = match started {
        Ok(d) => d,
        Err(StartupError::AlreadyRunning) => {
            info!(state_dir = ?cfg.state_dir, "another fleetd holds the instance lock; exiting");
            eprintln!("fleetd already running");
            return ExitCode::from(2);
        }
        Err(e) => {
            fatal(&e.to_string());
            return ExitCode::FAILURE;
        }
    };
    if let Some(e) = log_error {
        // The daemon runs anyway, unlogged.
        eprintln!("fleetd: no log file: {e}");
    }

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            fatal(&format!("runtime: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let served = rt.block_on(daemon.serve());
    // Do not wait on blocking calls stuck on an agent (e.g. a full input queue).
    rt.shutdown_timeout(std::time::Duration::from_secs(1));
    match served {
        Ok(()) => {
            info!("fleetd stopped");
            ExitCode::SUCCESS
        }
        Err(e) => {
            fatal(&e.to_string());
            ExitCode::FAILURE
        }
    }
}
