use std::path::PathBuf;
use std::process::ExitCode;

use fleetd::handoff::{self, Handed};
use fleetd::logging::{self, DAEMON_LOG, STDIO_LOG};
use fleetd::{Config, Daemon, StartupError, raise_fd_limit, startup, stop_signal};
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
    Ok(Daemon::adopted(
        cfg,
        listener,
        lock,
        rec,
        handed.pids.clone(),
    ))
}

/// The `fleet` that sits next to this binary, else the first on `PATH`.
fn find_fleet() -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("fleet")));
    beside.filter(|p| p.is_file()).or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join("fleet"))
            .find(|p| p.is_file())
    })
}

/// Another fleetd holds the lock. Whether it is this build, or a different
/// one to offer a restart onto, is `fleet daemon upgrade`'s to decide (the
/// flow is not in this binary; ADR R13), so this execs it. Without a `fleet`
/// it says so and exits 2.
fn offer_upgrade(cfg: &Config, upgrade: bool, force_stop: bool) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let (Some(fleet), Ok(me)) = (find_fleet(), std::env::current_exe()) else {
        eprintln!("fleetd already running (install fleet next to fleetd to upgrade from here)");
        return ExitCode::from(2);
    };
    let mut cmd = std::process::Command::new(&fleet);
    cmd.arg("--state-dir")
        .arg(&cfg.state_dir)
        .args(["daemon", "upgrade", "--binary"])
        .arg(me);
    if upgrade {
        cmd.arg("--yes");
    }
    if force_stop {
        cmd.arg("--force-stop");
    }
    // Only returns if the exec failed.
    let err = cmd.exec();
    eprintln!(
        "fleetd already running (cannot run {}: {err})",
        fleet.display()
    );
    ExitCode::from(2)
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
    // Used only when another fleetd holds the lock; ignored with --stdio.
    let (mut upgrade, mut force_stop) = (false, false);
    let mut args = std::env::args_os().skip(1);
    // Usage errors go to stderr only: there is no state dir to log to yet.
    while let Some(arg) = args.next() {
        if arg == "--stdio" {
            stdio = true;
        } else if arg == "--upgrade" {
            upgrade = true;
        } else if arg == "--force-stop" {
            force_stop = true;
        } else if arg == "--state-dir" {
            let Some(dir) = args.next() else {
                eprintln!("fleetd: --state-dir requires a path");
                return ExitCode::from(64);
            };
            cfg.state_dir = PathBuf::from(dir);
        } else {
            eprintln!("usage: fleetd [--stdio] [--upgrade] [--force-stop] [--state-dir <path>]");
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

    // Before the socket is bound (issue #23): a stop signal sent once it
    // exists is kept until `serve` reads it, never lost, and never the
    // default action that skips the shutdown. After the handoff above,
    // whose abandon closes every inherited fd.
    let stop = match stop_signal::arm() {
        Ok(stop) => stop,
        Err(e) => {
            fatal(&format!("arming the stop signals: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let started = match adopted {
        Some(daemon) => Ok(daemon),
        None if fell_back.is_some() => startup(&cfg).map(Daemon::fell_back),
        None => startup(&cfg),
    };
    let daemon = match started {
        Ok(d) => d,
        Err(StartupError::AlreadyRunning) => {
            info!(state_dir = ?cfg.state_dir, "another fleetd holds the instance lock; exiting");
            // The prompt and the upgrade it may exec stop on Ctrl-C as before.
            stop_signal::disarm(stop);
            return offer_upgrade(&cfg, upgrade, force_stop);
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
    let served = rt.block_on(daemon.serve(stop));
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
