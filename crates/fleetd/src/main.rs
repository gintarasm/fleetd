use std::path::PathBuf;
use std::process::ExitCode;

use fleetd::{Config, StartupError, startup};

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
    match rt.block_on(daemon.serve()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fleetd: {e}");
            ExitCode::FAILURE
        }
    }
}
