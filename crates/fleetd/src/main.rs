use std::path::PathBuf;
use std::process::ExitCode;

use fleetd::{Config, StartupError, raise_fd_limit, startup};

fn main() -> ExitCode {
    let mut cfg = Config::default();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--state-dir" {
            let Some(dir) = args.next() else {
                eprintln!("fleetd: --state-dir requires a path");
                return ExitCode::from(64);
            };
            cfg.state_dir = PathBuf::from(dir);
        } else {
            eprintln!("usage: fleetd [--state-dir <path>]");
            return ExitCode::from(64);
        }
    }

    if let Err(e) = raise_fd_limit() {
        eprintln!("fleetd: raising the open-file limit: {e}");
    }

    let daemon = match startup(&cfg) {
        Ok(d) => d,
        Err(StartupError::AlreadyRunning) => {
            eprintln!("fleetd already running");
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("fleetd: {e}");
            return ExitCode::FAILURE;
        }
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fleetd: runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let served = rt.block_on(daemon.serve());
    // Do not wait on blocking calls stuck on an agent (e.g. a full input queue).
    rt.shutdown_timeout(std::time::Duration::from_secs(1));
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fleetd: {e}");
            ExitCode::FAILURE
        }
    }
}
