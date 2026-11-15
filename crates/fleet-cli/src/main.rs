//! `fleet`: command-line client for `fleetd`.

mod attach;
mod history;
mod skill;
mod upgrade;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use fleet_proto::{
    AgentInfo, AgentState, ForgetAgents, KillAgent, ListAgents, RestartAgent, SendInput, SetLabels,
    SpawnAgent,
};
use fleet_proto::{request, response};

use fleet_client::{Connection, Error, Restart, Result, TOO_OLD, restart_daemon};

/// Pause between `fleet send`'s text and its `--enter` Return, long enough
/// for the receiving TUI to finish reading the text as one paste.
const SEND_ENTER_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

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
    /// Restart an exited agent (same command, cwd and labels; new id). Its environment is not restored.
    Restart {
        id: u64,
        /// Attach to the new agent
        #[arg(long)]
        attach: bool,
    },
    /// Remove exited or lost agents and their history
    Rm {
        /// Agents to remove (must be exited or lost)
        ids: Vec<u64>,
        /// Remove every exited or lost agent instead
        #[arg(long, conflicts_with = "ids", required_unless_present = "ids")]
        exited: bool,
        /// With --exited: only agents with this label, as k=v
        #[arg(long = "selector", value_name = "K=V", requires = "exited")]
        selector: Vec<String>,
    },
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
    /// Print fleetd's log (on the daemon's host: <state-dir>/logs/fleetd.log)
    Logs {
        /// The last N lines (at most 10000)
        #[arg(short = 'n', long = "lines", default_value_t = 100)]
        lines: u32,
        /// Then print new lines as they are logged, until Ctrl-C
        #[arg(short, long)]
        follow: bool,
    },
    /// Manage the fleetd daemon itself
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Install the agent skill that teaches agents to use `fleet`
    Skill(skill::SkillArgs),
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// What `fleetd` runs when another fleetd already holds the lock
    #[command(hide = true)]
    Upgrade {
        /// The fleetd binary to restart the running daemon onto
        #[arg(long)]
        binary: PathBuf,
        /// Restart without asking
        #[arg(long)]
        yes: bool,
        /// Stop a daemon too old to restart itself, without asking
        #[arg(long)]
        force_stop: bool,
    },
    /// Restart fleetd into a new binary; running agents keep running
    Restart {
        /// The fleetd binary to restart into (default: the daemon's own binary, as now on disk)
        #[arg(long)]
        binary: Option<PathBuf>,
    },
}

/// Prints `lines`; `false` once stdout is closed (e.g. piped into `head`).
fn print_lines(out: &mut impl std::io::Write, lines: &[String]) -> bool {
    lines.iter().all(|l| writeln!(out, "{l}").is_ok()) && out.flush().is_ok()
}

fn logs(mut client: Connection, lines: u32, follow: bool) -> Result<()> {
    let mut out = std::io::stdout().lock();
    if !print_lines(&mut out, &client.read_logs(lines, follow)?) || !follow {
        return Ok(());
    }
    while let Some(line) = client.next_log_line()? {
        if !print_lines(&mut out, std::slice::from_ref(&line)) {
            break;
        }
    }
    Ok(())
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

/// An old daemon answers requests it does not know with "empty request".
fn old_daemon(e: Error) -> Error {
    match e {
        Error::Server(m) if m == "empty request" => {
            Error::Server("the daemon is too old to restart/remove agents".into())
        }
        e => e,
    }
}

/// A `--binary` as the daemon should get it. The daemon resolves a relative
/// path against its own working directory (`/` for one the GUI started),
/// which is not where the path was typed, so a path is made absolute here,
/// against this command's. A bare name (no `/`) is sent as it is, for the
/// daemon to look up on its `PATH`, as a shell would.
fn binary_for_daemon(binary: &Path) -> String {
    let is_name = !binary.as_os_str().as_encoded_bytes().contains(&b'/');
    let binary = match std::path::absolute(binary) {
        Ok(abs) if !is_name => abs,
        // Only an empty path fails; the daemon refuses it.
        _ => binary.to_path_buf(),
    };
    binary.to_string_lossy().into_owned()
}

/// `fleet daemon restart`: asks the daemon to exec into `binary` (none:
/// the binary the daemon runs from, as rebuilt), waits for whichever daemon
/// answers next and reports what it says. Success goes to stdout, every
/// other outcome to stderr with exit 1.
fn daemon_restart(client: Connection, sock: &Path, binary: Option<PathBuf>) -> Result<u8> {
    let binary = binary.map(|b| binary_for_daemon(&b)).unwrap_or_default();
    Ok(match restart_daemon(client, sock, "fleet", &binary)? {
        Restart::TooOld => {
            eprintln!("{TOO_OLD}");
            1
        }
        Restart::Outcome(done) if done.is_success() => {
            println!("{done}");
            0
        }
        Restart::Outcome(other) => {
            eprintln!("{other}");
            1
        }
    })
}

/// Attaches interactively; the exit code of `fleet attach`.
fn attach_interactive(client: Connection, id: u64) -> Result<u8> {
    Ok(match attach::run(client, id)? {
        attach::Outcome::Lagged => {
            eprintln!("fleet: output lagged behind; re-attach");
            2
        }
        attach::Outcome::Signaled(sig) => 128 + sig as u8,
        attach::Outcome::Exited | attach::Outcome::Detached => 0,
    })
}

/// Exit code of a finished command.
fn run(cli: Cli) -> Result<u8> {
    if let Command::Skill(args) = cli.command {
        return skill::run(args).map(|()| 0);
    }
    if let Command::Daemon {
        cmd:
            DaemonCmd::Upgrade {
                binary,
                yes,
                force_stop,
            },
    } = &cli.command
    {
        let flags = upgrade::Flags {
            upgrade: *yes,
            force_stop: *force_stop,
        };
        return Ok(upgrade::run(
            &state_dir(&cli),
            &binary_for_daemon(binary),
            flags,
        ));
    }
    let sock = state_dir(&cli).join("fleetd.sock");
    let mut client = Connection::unix(&sock, "fleet")?;
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
            match client.call(request::Kind::Spawn(Box::new(spawn)))? {
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
        Command::Restart { id, attach } => {
            let req = RestartAgent {
                agent_id: id,
                ..Default::default()
            };
            let new_id = match client
                .call(request::Kind::Restart(req))
                .map_err(old_daemon)?
            {
                response::Kind::Restarted(r) => {
                    r.agent
                        .ok_or_else(|| Error::Protocol("restart reply without agent".into()))?
                        .id
                }
                _ => return Err(Error::Protocol("unexpected reply to restart".into())),
            };
            if !attach {
                println!("{new_id}");
            } else {
                return attach_interactive(client, new_id);
            }
        }
        Command::Rm {
            ids,
            exited,
            selector,
        } => {
            let req = ForgetAgents {
                ids,
                exited_only: exited,
                selector: parse_labels(&selector)?,
            };
            match client
                .call(request::Kind::Forget(Box::new(req)))
                .map_err(old_daemon)?
            {
                response::Kind::Forgotten(f) => {
                    if exited {
                        println!("removed {} agents", f.ids.len());
                    }
                    for id in &f.refused {
                        eprintln!("fleet: agent {id} not removed (running or unknown)");
                    }
                    if !f.refused.is_empty() {
                        return Ok(1);
                    }
                }
                _ => return Err(Error::Protocol("unexpected reply to rm".into())),
            }
        }
        Command::Send { id, text, enter } => {
            // The Return goes as its own write, a moment after the text: TUIs
            // such as Claude Code read text and Return arriving together as a
            // paste, and leave it typed but unsubmitted.
            let data = text.into_bytes();
            let typed = !data.is_empty();
            if typed {
                client.call(request::Kind::Input(SendInput { agent_id: id, data }))?;
            }
            if enter {
                if typed {
                    std::thread::sleep(SEND_ENTER_DELAY);
                }
                client.call(request::Kind::Input(SendInput {
                    agent_id: id,
                    data: b"\r".to_vec(),
                }))?;
            }
        }
        Command::Attach {
            id,
            snapshot_only: true,
        } => attach::snapshot_only(client, id)?,
        Command::Attach { id, .. } => return attach_interactive(client, id),
        Command::Logs { lines, follow } => logs(client, lines, follow)?,
        Command::Daemon {
            cmd: DaemonCmd::Restart { binary },
        } => return daemon_restart(client, &sock, binary),
        Command::Daemon {
            cmd: DaemonCmd::Upgrade { .. },
        }
        | Command::Skill(_) => unreachable!("handled before connecting"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binary_path_is_sent_absolute_and_a_name_as_it_is() {
        let cwd = std::env::current_dir().unwrap();
        let sent = binary_for_daemon(Path::new("target/release/fleetd"));
        assert_eq!(Path::new(&sent), cwd.join("target/release/fleetd"));
        let sent = binary_for_daemon(Path::new("./fleetd"));
        assert!(Path::new(&sent).is_absolute(), "{sent}");
        assert_eq!(binary_for_daemon(Path::new("/opt/fleetd")), "/opt/fleetd");
        assert_eq!(binary_for_daemon(Path::new("fleetd")), "fleetd");
    }
}
