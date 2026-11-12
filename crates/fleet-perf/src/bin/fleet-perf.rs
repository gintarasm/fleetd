use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use fleet_perf::baseline::{Baselines, baseline_add, compare, tighten, write_into};
use fleet_perf::results::Results;

#[derive(Parser)]
#[command(name = "fleet-perf", about = "fleetd perf gate")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Compare results against baselines; exit 1 on failure.
    Compare {
        #[arg(long, default_value = "bench/baselines.toml")]
        baselines: PathBuf,
        #[arg(long)]
        results: PathBuf,
    },
    /// Lower baselines (or record unrecorded ones) from results. Never raises.
    Tighten {
        #[arg(long, default_value = "bench/baselines.toml")]
        baselines: PathBuf,
        #[arg(long)]
        results: PathBuf,
    },
    /// Add metrics unknown to the baselines, with baseline = value.
    BaselineAdd {
        #[arg(long, default_value = "bench/baselines.toml")]
        baselines: PathBuf,
        #[arg(long)]
        results: PathBuf,
    },
}

fn load(baselines: &PathBuf, results: &PathBuf) -> Result<(String, Baselines, Results), String> {
    let text = std::fs::read_to_string(baselines)
        .map_err(|e| format!("reading {}: {e}", baselines.display()))?;
    let b = Baselines::parse(&text).map_err(|e| format!("parsing {}: {e}", baselines.display()))?;
    let rtext = std::fs::read_to_string(results)
        .map_err(|e| format!("reading {}: {e}", results.display()))?;
    let r =
        Results::from_json(&rtext).map_err(|e| format!("parsing {}: {e}", results.display()))?;
    Ok((text, b, r))
}

fn run(cli: Cli) -> Result<ExitCode, String> {
    match cli.cmd {
        Cmd::Compare { baselines, results } => {
            let (_, b, r) = load(&baselines, &results)?;
            let report = compare(&b, &r);
            print!("{report}");
            Ok(if report.failed {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
        Cmd::Tighten { baselines, results } => {
            let (text, mut b, r) = load(&baselines, &results)?;
            tighten(&mut b, &r);
            write(&baselines, &text, &b)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::BaselineAdd { baselines, results } => {
            let (text, mut b, r) = load(&baselines, &results)?;
            let added = baseline_add(&mut b, &r);
            write(&baselines, &text, &b)?;
            for name in added {
                println!("added {name}");
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn write(path: &PathBuf, text: &str, b: &Baselines) -> Result<(), String> {
    let out = write_into(text, b).map_err(|e| format!("editing {}: {e}", path.display()))?;
    std::fs::write(path, out).map_err(|e| format!("writing {}: {e}", path.display()))
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fleet-perf: {e}");
            ExitCode::from(2)
        }
    }
}
