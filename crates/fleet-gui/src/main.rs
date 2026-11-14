use std::path::PathBuf;

use clap::Parser;

/// Native client for fleetd.
#[derive(Parser, Debug)]
#[command(name = "fleet-gui", version)]
pub struct Cli {
    /// State directory (config.toml, gui-state.toml, sockets). Default: ~/.fleet
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
    /// Connect to `local` or `ssh://[user@]host` for this launch, overriding the saved connection.
    #[arg(long, value_name = "TARGET")]
    pub target: Option<fleet_gui::core::target::Target>,
}

#[cfg(target_os = "macos")]
fn main() {
    let cli = Cli::parse();
    let state_dir = cli
        .state_dir
        .unwrap_or_else(fleet_gui::core::state::default_state_dir);
    fleet_gui::app::run(state_dir, cli.target);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    let _ = Cli::parse();
    eprintln!("fleet-gui is macOS-only");
    std::process::exit(1);
}
