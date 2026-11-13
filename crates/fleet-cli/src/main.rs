//! `fleet`: command-line client for `fleetd`.

mod attach;
mod history;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use fleet_proto::{AgentInfo, AgentState, KillAgent, ListAgents, SendInput, SetLabels, SpawnAgent};
use fleet_proto::{request, response};

use fleet_client::{Connection, Error, Result};

#[derive(Parser)]
#[command(name = "fleet", about = "Manage agents run by fleetd")]
struct Cli {
    /// State directory of fleetd (default: ~/.fleet)
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start an agent and print its id
    Spawn {
        #[arg(long)]
        cols: Option<u32>,
        #[arg(long)]
        rows: Option<u32>,
        /// Working directory (default: the current directory)
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Label as k=v (repeatable)
        #[arg(long = "label", value_name = "K=V")]
        labels: Vec<String>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// List agents
    Ls {
        /// Print a JSON array instead of a table
        #[arg(long)]
        json: bool,
        /// Only agents with this label k=v (repeatable)
        #[arg(long = "label", value_name = "K=V")]
        labels: Vec<String>,
    },
    /// Set or remove labels on an agent
    Label {
        id: u64,
        /// Labels to set, as k=v
        set: Vec<String>,
        /// Label keys to remove
        #[arg(long = "rm", num_args = 1.., value_name = "K")]
        remove: Vec<String>,
    },
    /// Print an agent's history, oldest first, then its current screen
    History {
        id: u64,
        /// Only the last N lines
        #[arg(long)]
        lines: Option<usize>,
        /// Strip colours and other styling
        #[arg(long)]
        plain: bool,
    },
    /// Print the visible text of an agent's current screen
    Screen { id: u64 },
    /// Kill an agent
    Kill { id: u64 },
    /// Send text to an agent's terminal
    Send {
        id: u64,
        text: String,
        /// Append a carriage return
        #[arg(long)]
        enter: bool,
    },
    /// Attach to an agent interactively (Ctrl-\ detaches)
    Attach {
        id: u64,
        /// Print the snapshot and exit, without touching the terminal
        #[arg(long, hide = true)]
        snapshot_only: bool,
    },
}

fn state_dir(cli: &Cli) -> PathBuf {
    cli.state_dir.clone().unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".fleet")
    })
}

fn state_name(info: &AgentInfo) -> &'static str {
    match info.state() {
        AgentState::Running => "running",
        AgentState::Exited => "exited",
        AgentState::Lost => "lost",
        AgentState::Unspecified => "unknown",
    }
}

/// Parses `k=v`: the first `=` splits; the key must not be empty.
fn parse_label(s: &str) -> Result<(String, String)> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(Error::Server(format!("invalid label {s:?}: expected k=v"))),
    }
}

fn parse_labels(items: &[String]) -> Result<HashMap<String, String>> {
    items.iter().map(|s| parse_label(s)).collect()
}

fn print_json(agents: &[AgentInfo]) {
    let items: Vec<_> = agents
        .iter()
        .map(|a| {
            serde_json::json!({
                "id": a.id,
                "state": state_name(a),
                "exit_code": a.exit_code,
                "program": a.program,
                "args": a.args,
                "cwd": a.cwd,
                "created_unix_ms": a.created_unix_ms,
                "labels": a.labels.iter().collect::<std::collections::BTreeMap<_, _>>(),
            })
        })
        .collect();
    println!("{}", serde_json::Value::Array(items));
}

fn print_table(agents: &[AgentInfo]) {
    let mut rows = vec![[
        "ID".to_string(),
        "STATE".into(),
        "EXIT".into(),
        "PROGRAM".into(),
    ]];
    for a in agents {
        let program = std::iter::once(a.program.as_str())
            .chain(a.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        rows.push([
            a.id.to_string(),
            state_name(a).into(),
            a.exit_code.map_or("-".into(), |c| c.to_string()),
            program,
        ]);
    }
    let width = |col: usize| rows.iter().map(|r| r[col].len()).max().unwrap_or(0);
    let (w0, w1, w2) = (width(0), width(1), width(2));
    for r in &rows {
        println!("{:<w0$}  {:<w1$}  {:<w2$}  {}", r[0], r[1], r[2], r[3]);
    }
}

/// Exit code of a finished command.
fn run(cli: Cli) -> Result<u8> {
    let mut client = Connection::unix(&state_dir(&cli).join("fleetd.sock"))?;
    match cli.command {
        Command::Spawn {
            cols,
            rows,
            cwd,
            labels,
            mut command,
        } => {
            let (tc, tr) = crossterm::terminal::size().unwrap_or((80, 24));
            let program = command.remove(0);
            let labels = parse_labels(&labels)?;
            let cwd = match cwd {
                Some(p) if p.is_absolute() => p,
                Some(p) => std::env::current_dir()?.join(p),
                None => std::env::current_dir().unwrap_or_default(),
            }
            .to_string_lossy()
            .into_owned();
            let spawn = SpawnAgent {
                program,
                args: command,
                cwd,
                cols: cols.unwrap_or(u32::from(tc)),
                rows: rows.unwrap_or(u32::from(tr)),
                labels,
                ..Default::default()
            };
            match client.call(request::Kind::Spawn(spawn))? {
                response::Kind::Spawned(info) => println!("{}", info.id),
                _ => return Err(Error::Protocol("unexpected reply to spawn".into())),
            }
        }
        Command::Ls { json, labels } => {
            let selector = parse_labels(&labels)?;
            match client.call(request::Kind::List(ListAgents { selector }))? {
                response::Kind::Agents(list) if json => print_json(&list.agents),
                response::Kind::Agents(list) => print_table(&list.agents),
                _ => return Err(Error::Protocol("unexpected reply to list".into())),
            }
        }
        Command::Label { id, set, remove } => {
            let set = parse_labels(&set)?;
            client.call(request::Kind::SetLabels(SetLabels {
                agent_id: id,
                set,
                remove,
            }))?;
        }
        Command::History { id, lines, plain } => history::history(client, id, lines, plain)?,
        Command::Screen { id } => history::screen(client, id)?,
        Command::Kill { id } => {
            client.call(request::Kind::Kill(KillAgent { agent_id: id }))?;
        }
        Command::Send { id, text, enter } => {
            let mut data = text.into_bytes();
            if enter {
                data.push(b'\r');
            }
            client.call(request::Kind::Input(SendInput { agent_id: id, data }))?;
        }
        Command::Attach {
            id,
            snapshot_only: true,
        } => attach::snapshot_only(client, id)?,
        Command::Attach { id, .. } => match attach::run(client, id)? {
            attach::Outcome::Lagged => {
                eprintln!("fleet: output lagged behind; re-attach");
                return Ok(2);
            }
            attach::Outcome::Signaled(sig) => return Ok(128 + sig as u8),
            attach::Outcome::Exited | attach::Outcome::Detached => {}
        },
    }
    Ok(0)
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("fleet: {e}");
            ExitCode::from(1)
        }
    }
}
