use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use fleetd::logging::{self, DAEMON_LOG, STDIO_LOG};
use fleetd::{Config, StartupError, raise_fd_limit, startup};
use tracing::{error, info, warn};

/// Logs a fatal error, and also prints it when a person is watching stderr
/// (a daemon started by `--stdio` or launchd has it on /dev/null or a file).
fn fatal(msg: &str) {
    error!("{msg}");
    if std::io::stderr().is_terminal() {
        eprintln!("fleetd: {msg}");
    }
}

fn main() -> ExitCode {
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

    let log = logging::init(&cfg.state_dir, DAEMON_LOG);
    let log_error = log.as_ref().err().map(ToString::to_string);
    let _log = log.ok();

    match raise_fd_limit() {
        Ok(limit) => info!(limit, "open-file limit"),
        Err(e) => warn!(error = %e, "raising the open-file limit failed"),
    }

    let daemon = match startup(&cfg) {
        Ok(d) => d,
        Err(StartupError::AlreadyRunning) => {
            info!(state_dir = ?cfg.state_dir, "another fleetd holds the instance lock; exiting");
            if std::io::stderr().is_terminal() {
                eprintln!("fleetd already running");
            }
            return ExitCode::from(2);
        }
        Err(e) => {
            fatal(&e.to_string());
            return ExitCode::FAILURE;
        }
    };
    if let Some(e) = log_error {
        // The daemon runs anyway, unlogged.
        if std::io::stderr().is_terminal() {
            eprintln!("fleetd: no log file: {e}");
        }
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
