//! `fleet skill`: installs the agent skill that teaches an agent (Claude
//! Code or omp) to use `fleet` (`skill/SKILL.md`, embedded at build time).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use fleet_client::{Error, Result};

const SKILL: &str = include_str!("../skill/SKILL.md");

#[derive(Args)]
pub struct SkillArgs {
    /// Agent to install for: `detected` means every agent whose home
    /// folder (~/.claude, ~/.omp) exists
    #[arg(long, alias = "target", global = true, value_enum, default_value_t = AgentKind::Claude)]
    agent: AgentKind,
    #[command(subcommand)]
    action: Action,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AgentKind {
    /// Claude Code: ~/.claude/skills/fleet/
    Claude,
    /// omp: ~/.omp/agent/skills/fleet/
    Omp,
    /// Each of the above whose home folder exists
    Detected,
}

/// The agents `detected` looks for, in output order.
const AGENTS: [Agent; 2] = [Agent::Claude, Agent::Omp];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Agent {
    Claude,
    Omp,
}

impl Agent {
    /// The agent's home folder under `$HOME`.
    fn home_dir(self) -> &'static str {
        match self {
            Agent::Claude => ".claude",
            Agent::Omp => ".omp",
        }
    }

    /// The skill directory under `base` (`$HOME`, or a project for claude).
    fn skill_dir(self, base: &Path) -> PathBuf {
        let home = base.join(self.home_dir());
        match self {
            Agent::Claude => home.join("skills").join("fleet"),
            Agent::Omp => home.join("agent").join("skills").join("fleet"),
        }
    }
}

#[derive(Subcommand)]
enum Action {
    /// Write the skill to ~/.claude/skills/fleet/ (--agent omp:
    /// ~/.omp/agent/skills/fleet/; --project: DIR/.claude/…, claude only)
    Install {
        /// Install into project DIR (default: the current directory)
        #[arg(long, value_name = "DIR", num_args = 0..=1)]
        project: Option<Option<PathBuf>>,
    },
    /// Remove the installed skill directory
    Uninstall {
        /// Remove from project DIR (default: the current directory)
        #[arg(long, value_name = "DIR", num_args = 0..=1)]
        project: Option<Option<PathBuf>>,
    },
    /// Print the skill to stdout
    Print,
}

fn err(msg: String) -> Error {
    Error::Server(msg)
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| err("HOME is not set".into()))
}

/// The skill directories `kind` and `project` name.
fn skill_dirs(kind: AgentKind, project: Option<Option<PathBuf>>) -> Result<Vec<PathBuf>> {
    if let Some(project) = project {
        if kind != AgentKind::Claude {
            return Err(err(
                "--project is only for --agent claude: omp has no known project skill folder"
                    .into(),
            ));
        }
        let base = match project {
            Some(dir) if dir.is_absolute() => dir,
            Some(dir) => std::env::current_dir()?.join(dir),
            None => std::env::current_dir()?,
        };
        return Ok(vec![Agent::Claude.skill_dir(&base)]);
    }
    let home = home()?;
    let agents: Vec<Agent> = match kind {
        AgentKind::Claude => vec![Agent::Claude],
        AgentKind::Omp => vec![Agent::Omp],
        AgentKind::Detected => AGENTS
            .into_iter()
            .filter(|a| home.join(a.home_dir()).is_dir())
            .collect(),
    };
    if agents.is_empty() {
        return Err(err(
            "no agent home folder found (looked for ~/.claude and ~/.omp)".into(),
        ));
    }
    Ok(agents.into_iter().map(|a| a.skill_dir(&home)).collect())
}

fn with_path(e: io::Error, path: &Path) -> Error {
    err(format!("{}: {e}", path.display()))
}

pub fn run(args: SkillArgs) -> Result<()> {
    match args.action {
        Action::Install { project } => {
            for dir in skill_dirs(args.agent, project)? {
                std::fs::create_dir_all(&dir).map_err(|e| with_path(e, &dir))?;
                let path = dir.join("SKILL.md");
                std::fs::write(&path, SKILL).map_err(|e| with_path(e, &path))?;
                println!("{}", path.display());
            }
        }
        Action::Uninstall { project } => {
            for dir in skill_dirs(args.agent, project)? {
                match std::fs::remove_dir_all(&dir) {
                    Ok(()) => println!("{}", dir.display()),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(with_path(e, &dir)),
                }
            }
        }
        Action::Print => io::stdout().write_all(SKILL.as_bytes())?,
    }
    Ok(())
}
