//! `fleet skill`: installs the agent skill that teaches Claude Code to use
//! `fleet` (`skill/SKILL.md`, embedded at build time).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use fleet_client::{Error, Result};

const SKILL: &str = include_str!("../skill/SKILL.md");

#[derive(Args)]
pub struct SkillArgs {
    /// Agent to install for (only `claude` until M4)
    #[arg(long, global = true, default_value = "claude")]
    target: String,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Write the skill to ~/.claude/skills/fleet/ (or DIR/.claude/… with --project)
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

/// `.claude/skills/fleet` under the project dir or `$HOME`.
fn skill_dir(project: Option<Option<PathBuf>>) -> Result<PathBuf> {
    let base = match project {
        Some(Some(dir)) if dir.is_absolute() => dir,
        Some(Some(dir)) => std::env::current_dir()?.join(dir),
        Some(None) => std::env::current_dir()?,
        None => std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| err("HOME is not set".into()))?,
    };
    Ok(base.join(".claude").join("skills").join("fleet"))
}

fn with_path(e: io::Error, path: &Path) -> Error {
    err(format!("{}: {e}", path.display()))
}

pub fn run(args: SkillArgs) -> Result<()> {
    if args.target != "claude" {
        return Err(err(format!(
            "skill target {:?} is not supported: only \"claude\" for now (other agents come in M4)",
            args.target
        )));
    }
    match args.action {
        Action::Install { project } => {
            let dir = skill_dir(project)?;
            std::fs::create_dir_all(&dir).map_err(|e| with_path(e, &dir))?;
            let path = dir.join("SKILL.md");
            std::fs::write(&path, SKILL).map_err(|e| with_path(e, &path))?;
            println!("{}", path.display());
        }
        Action::Uninstall { project } => {
            let dir = skill_dir(project)?;
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => println!("{}", dir.display()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(with_path(e, &dir)),
            }
        }
        Action::Print => io::stdout().write_all(SKILL.as_bytes())?,
    }
    Ok(())
}
