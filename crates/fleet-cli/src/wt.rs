//! `fleet wt` / `fleet worktree` (ADR 0093, M4 spec §3): a thin front over
//! the `git-worktree.*` actions. Exit codes as M3: 0 ok, 1 refused or not
//! found, 2 usage or other error, 3 forbidden.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::IsTerminal;
use std::path::Path;

use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use fleet_client::{Connection, Error, Result};
use fleet_proto::action_result::Outcome;
use fleet_proto::param_value::Value;
use fleet_proto::{
    ActionRefused, DiffView, ParamValue, RunAction, StringList, StringMap, request, response, view,
};

use crate::action::{absolute, outcome_json, render_diff, render_table};

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Against {
    /// Everything since the merge base with the base, untracked included
    Base,
    /// The working tree against HEAD, untracked included
    Uncommitted,
    /// Commits since the merge base only
    Committed,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Make a worktree on a new branch and start an agent in it
    New {
        /// The worktree's folder name (letters, digits, . _ -)
        slug: String,
        /// Any checkout of the repository (default: this folder's)
        #[arg(long, value_name = "D")]
        repo: Option<String>,
        /// The ref to branch from (default: the remote's default branch)
        #[arg(long, value_name = "REF")]
        base: Option<String>,
        /// The new branch (default: the configured prefix + slug)
        #[arg(long, value_name = "B")]
        branch: Option<String>,
        /// The agent's name
        #[arg(long, value_name = "N")]
        name: Option<String>,
        /// A label for the agent, k=v (repeatable)
        #[arg(long = "label", value_name = "K=V")]
        labels: Vec<String>,
        /// Start the agent in the slug's existing worktree
        #[arg(long)]
        reuse: bool,
        /// With --reuse: even if another agent is working there
        #[arg(long, requires = "reuse")]
        shared: bool,
        /// Print the agent as JSON
        #[arg(long)]
        json: bool,
        /// The agent's program and arguments
        #[arg(last = true, required = true, value_name = "PROGRAM")]
        command: Vec<String>,
    },
    /// List the repository's worktrees and the agents in them
    Ls {
        #[arg(long, value_name = "D")]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show what changed in an agent's worktree (default: this folder's)
    Diff {
        /// Agent id or name
        agent: Option<String>,
        /// A folder in the worktree instead of an agent
        #[arg(long, value_name = "D", conflicts_with = "agent")]
        path: Option<String>,
        #[arg(long, value_enum, default_value = "base")]
        against: Against,
        /// Only file names and +/- counts
        #[arg(long)]
        stat: bool,
        /// Only this file (a path in the worktree)
        #[arg(long, value_name = "F")]
        file: Option<String>,
        /// Lines of context
        #[arg(long, short = 'U', value_name = "N")]
        context: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    /// Remove a worktree (and its merged branch)
    Archive {
        /// Agent id or name
        #[arg(required_unless_present = "path")]
        agent: Option<String>,
        /// The worktree's folder instead of an agent
        #[arg(long, value_name = "D", conflicts_with = "agent")]
        path: Option<String>,
        /// Even with uncommitted changes or unmerged commits (deletes the branch)
        #[arg(long)]
        force: bool,
        #[arg(long)]
        keep_branch: bool,
    },
    /// Archive every worktree fleet made that is clean, merged and idle
    Prune {
        #[arg(long, value_name = "D")]
        repo: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
}

fn string(s: impl Into<String>) -> ParamValue {
    ParamValue {
        value: Some(Value::String(s.into())),
    }
}

fn boolean(b: bool) -> ParamValue {
    ParamValue {
        value: Some(Value::Bool(b)),
    }
}

/// The action a command runs and its params (paths made absolute against
/// `cwd`). `Err` is a usage error.
fn call(
    cmd: &Cmd,
    cwd: &Path,
) -> std::result::Result<(&'static str, HashMap<String, ParamValue>), String> {
    let mut p = HashMap::new();
    let path = |d: &str| string(absolute(cwd, d));
    let name = match cmd {
        Cmd::New {
            slug,
            repo,
            base,
            branch,
            name,
            labels,
            reuse,
            shared,
            json: _,
            command,
        } => {
            p.insert("slug".into(), string(slug.as_str()));
            p.insert(
                "command".into(),
                ParamValue {
                    value: Some(Value::List(StringList {
                        items: command.clone(),
                    })),
                },
            );
            if let Some(r) = repo {
                p.insert("repo".into(), path(r));
            }
            for (k, v) in [("base", base), ("branch", branch), ("name", name)] {
                if let Some(v) = v {
                    p.insert(k.into(), string(v.as_str()));
                }
            }
            if !labels.is_empty() {
                let mut entries = HashMap::new();
                for l in labels {
                    let (k, v) = l
                        .split_once('=')
                        .filter(|(k, _)| !k.is_empty())
                        .ok_or_else(|| format!("--label takes k=v, not {l:?}"))?;
                    entries.insert(k.to_owned(), v.to_owned());
                }
                p.insert(
                    "labels".into(),
                    ParamValue {
                        value: Some(Value::Map(StringMap { entries })),
                    },
                );
            }
            if *reuse {
                p.insert("reuse".into(), boolean(true));
            }
            if *shared {
                p.insert("shared".into(), boolean(true));
            }
            "git-worktree.new-agent"
        }
        Cmd::Ls { repo, json: _ } => {
            if let Some(r) = repo {
                p.insert("repo".into(), path(r));
            }
            "git-worktree.ls"
        }
        Cmd::Diff {
            agent,
            path: dir,
            against,
            stat: _,
            file,
            context,
            json: _,
        } => {
            match (agent, dir) {
                (Some(a), _) => p.insert("agent".into(), string(a.as_str())),
                (None, Some(d)) => p.insert("path".into(), path(d)),
                (None, None) => p.insert("path".into(), path(".")),
            };
            let against = match against {
                Against::Base => "base",
                Against::Uncommitted => "uncommitted",
                Against::Committed => "committed",
            };
            p.insert("against".into(), string(against));
            if let Some(f) = file {
                p.insert("file".into(), string(f.as_str()));
            }
            if let Some(n) = context {
                p.insert(
                    "context".into(),
                    ParamValue {
                        value: Some(Value::Int(*n)),
                    },
                );
            }
            "git-worktree.diff"
        }
        Cmd::Archive {
            agent,
            path: dir,
            force,
            keep_branch,
        } => {
            match (agent, dir) {
                (Some(a), _) => p.insert("agent".into(), string(a.as_str())),
                (None, Some(d)) => p.insert("path".into(), path(d)),
                (None, None) => return Err("give an agent or --path".into()),
            };
            // Only when given: an agent may not set `force` at all.
            if *force {
                p.insert("force".into(), boolean(true));
            }
            if *keep_branch {
                p.insert("keep_branch".into(), boolean(true));
            }
            "git-worktree.archive"
        }
        Cmd::Prune { repo, dry_run } => {
            if let Some(r) = repo {
                p.insert("repo".into(), path(r));
            }
            if *dry_run {
                p.insert("dry_run".into(), boolean(true));
            }
            "git-worktree.prune"
        }
    };
    Ok((name, p))
}

/// A refusal's retry as `fleet wt` flags (`--force`).
fn retry_flags(r: &ActionRefused) -> String {
    let mut flags: Vec<String> = r
        .retry
        .iter()
        .filter(|(_, v)| matches!(v.value, Some(Value::Bool(true))))
        .map(|(k, _)| format!("--{}", k.replace('_', "-")))
        .collect();
    flags.sort();
    flags.join(" ")
}

/// `git diff --stat`-like lines and a summary.
fn render_stat(d: &DiffView) -> String {
    const BAR: u32 = 50;
    let names: Vec<String> = d
        .files
        .iter()
        .map(|f| match (f.old_path.as_str(), f.new_path.as_str()) {
            ("", n) => n.to_owned(),
            (o, "") => o.to_owned(),
            (o, n) if o != n => format!("{o} => {n}"),
            (_, n) => n.to_owned(),
        })
        .collect();
    let width = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
    let most = d
        .files
        .iter()
        .map(|f| f.additions + f.deletions)
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (f, name) in d.files.iter().zip(&names) {
        if f.binary {
            let _ = writeln!(out, " {name:<width$} | Bin");
            continue;
        }
        let n = f.additions + f.deletions;
        let (mut plus, mut minus) = (f.additions, f.deletions);
        if most > BAR {
            plus = (u64::from(plus) * u64::from(BAR) / u64::from(most)) as u32;
            minus = (u64::from(minus) * u64::from(BAR) / u64::from(most)) as u32;
        }
        let bar = format!(
            "{}{}",
            "+".repeat(plus as usize),
            "-".repeat(minus as usize)
        );
        let _ = writeln!(out, " {name:<width$} | {n} {bar}");
    }
    let plural = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut summary = vec![plural(
        u64::from(d.files_changed),
        "file changed",
        "files changed",
    )];
    if d.additions > 0 {
        summary.push(plural(d.additions, "insertion(+)", "insertions(+)"));
    }
    if d.deletions > 0 {
        summary.push(plural(d.deletions, "deletion(-)", "deletions(-)"));
    }
    let _ = writeln!(out, " {}", summary.join(", "));
    out
}

/// A unified diff coloured for a terminal.
fn colour(diff: &str) -> String {
    const BOLD: &str = "\x1b[1m";
    const CYAN: &str = "\x1b[36m";
    const RED: &str = "\x1b[31m";
    const GREEN: &str = "\x1b[32m";
    const RESET: &str = "\x1b[0m";
    let mut out = String::with_capacity(diff.len() + diff.len() / 4);
    let mut in_header = false;
    for line in diff.split_inclusive('\n') {
        let text = line.strip_suffix('\n').unwrap_or(line);
        let nl = if text.len() < line.len() { "\n" } else { "" };
        if text.starts_with("diff --git ") {
            in_header = true;
        } else if text.starts_with("@@") {
            in_header = false;
        }
        let paint = if in_header {
            Some(BOLD)
        } else if text.starts_with("@@") {
            Some(CYAN)
        } else if text.starts_with('+') {
            Some(GREEN)
        } else if text.starts_with('-') {
            Some(RED)
        } else {
            None
        };
        match paint {
            Some(c) => {
                let _ = write!(out, "{c}{text}{RESET}{nl}");
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Whether the error means "no such agent or worktree" (exit 1).
fn not_found(message: &str) -> bool {
    message.contains("no agent") || message.contains("not in a git")
}

pub fn run(client: &mut Connection, args: Args) -> Result<u8> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let (name, params) = match call(&args.cmd, &cwd) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fleet: {e}");
            return Ok(2);
        }
    };
    let req = RunAction {
        name: name.to_owned(),
        params,
        cwd: cwd.to_string_lossy().into_owned(),
    };
    let outcome = match client.call(request::Kind::RunAction(Box::new(req))) {
        Ok(response::Kind::ActionResult(r)) => match r.outcome {
            Some(o) => o,
            None => {
                return Err(Error::Protocol(
                    "an action result without an outcome".into(),
                ));
            }
        },
        Ok(_) => return Err(Error::Protocol("unexpected reply to run action".into())),
        Err(e) if e.is_forbidden() => return Err(e),
        Err(Error::Server(m)) if m == "empty request" => {
            eprintln!("fleet: the daemon is too old for workflow actions");
            return Ok(2);
        }
        Err(Error::Server(m)) => {
            eprintln!("fleet: {m}");
            return Ok(if not_found(&m) { 1 } else { 2 });
        }
        Err(e) => return Err(e),
    };
    let json = match &args.cmd {
        Cmd::New { json, .. } | Cmd::Ls { json, .. } | Cmd::Diff { json, .. } => *json,
        Cmd::Archive { .. } | Cmd::Prune { .. } => false,
    };
    if json {
        println!("{}", outcome_json(&outcome));
        return Ok(u8::from(matches!(outcome, Outcome::Refused(_))));
    }
    let stat = matches!(args.cmd, Cmd::Diff { stat: true, .. });
    Ok(match outcome {
        Outcome::Done(d) => {
            if !d.message.is_empty() {
                println!("{}", d.message);
            }
            0
        }
        Outcome::Spawned(s) => {
            if let Some(agent) = &s.agent {
                println!("{}", agent.id);
            }
            if !s.message.is_empty() {
                eprintln!("{}", s.message);
            }
            0
        }
        Outcome::View(v) => {
            match v.kind {
                Some(view::Kind::Diff(d)) if stat => print!("{}", render_stat(&d)),
                Some(view::Kind::Diff(d)) => {
                    let text = render_diff(&d);
                    let tty =
                        std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
                    if tty {
                        print!("{}", colour(&text));
                    } else {
                        print!("{text}");
                    }
                    if d.truncated {
                        eprintln!(
                            "fleet: some files are too big to show here: ask for one with --file"
                        );
                    }
                }
                Some(view::Kind::Table(t)) => {
                    if !t.title.is_empty() {
                        eprintln!("{}", t.title);
                    }
                    print!("{}", render_table(&t));
                }
                Some(view::Kind::Text(t)) => println!("{}", t.text.trim_end()),
                None => {}
            }
            0
        }
        Outcome::Refused(r) => {
            eprintln!("fleet: refused: {}", r.reason);
            let flags = retry_flags(&r);
            if !flags.is_empty() {
                eprintln!("fleet: to do it anyway, add: {flags}");
            }
            1
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use fleet_proto::{FileDiff, FileStatus, Hunk};

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        cmd: Cmd,
    }

    fn parse(argv: &[&str]) -> Cmd {
        Cli::try_parse_from(std::iter::once("wt").chain(argv.iter().copied()))
            .unwrap()
            .cmd
    }

    fn get<'a>(m: &'a HashMap<String, ParamValue>, k: &str) -> &'a Value {
        m[k].value.as_ref().unwrap()
    }

    #[test]
    fn new_maps_to_new_agent() {
        let cmd = parse(&[
            "new",
            "issue-1",
            "--repo",
            "../r",
            "--base",
            "origin/main",
            "--branch",
            "feat/x",
            "--name",
            "kid",
            "--label",
            "role=rev",
            "--label",
            "m4=c",
            "--reuse",
            "--shared",
            "--",
            "claude",
            "--model",
            "x",
        ]);
        let (name, p) = call(&cmd, Path::new("/home/u/src")).unwrap();
        assert_eq!(name, "git-worktree.new-agent");
        assert_eq!(get(&p, "slug"), &Value::String("issue-1".into()));
        assert_eq!(get(&p, "repo"), &Value::String("/home/u/r".into()));
        assert_eq!(get(&p, "base"), &Value::String("origin/main".into()));
        assert_eq!(get(&p, "branch"), &Value::String("feat/x".into()));
        assert_eq!(get(&p, "name"), &Value::String("kid".into()));
        assert_eq!(
            get(&p, "command"),
            &Value::List(StringList {
                items: vec!["claude".into(), "--model".into(), "x".into()]
            })
        );
        let Value::Map(m) = get(&p, "labels") else {
            panic!()
        };
        assert_eq!(m.entries.len(), 2);
        assert_eq!(get(&p, "reuse"), &Value::Bool(true));
        assert_eq!(get(&p, "shared"), &Value::Bool(true));
        // The repo defaults to the cwd the daemon gets; flags not given are not sent.
        let (_, p) = call(&parse(&["new", "s", "--", "sh"]), Path::new("/x")).unwrap();
        let mut keys: Vec<_> = p.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["command", "slug"]);
        assert!(
            call(
                &parse(&["new", "s", "--label", "nokv", "--", "sh"]),
                Path::new("/x")
            )
            .is_err()
        );
    }

    #[test]
    fn diff_archive_ls_prune_map_to_their_actions() {
        let cwd = Path::new("/w/wt");
        let (name, p) = call(
            &parse(&[
                "diff",
                "kid",
                "--against",
                "committed",
                "--file",
                "a.rs",
                "-U",
                "5",
            ]),
            cwd,
        )
        .unwrap();
        assert_eq!(name, "git-worktree.diff");
        assert_eq!(get(&p, "agent"), &Value::String("kid".into()));
        assert_eq!(get(&p, "against"), &Value::String("committed".into()));
        assert_eq!(get(&p, "file"), &Value::String("a.rs".into()));
        assert_eq!(get(&p, "context"), &Value::Int(5));
        // No agent and no path: this folder's worktree.
        let (_, p) = call(&parse(&["diff"]), cwd).unwrap();
        assert_eq!(get(&p, "path"), &Value::String("/w/wt".into()));
        let (name, p) = call(&parse(&["archive", "--path", "../o", "--keep-branch"]), cwd).unwrap();
        assert_eq!(name, "git-worktree.archive");
        assert_eq!(get(&p, "path"), &Value::String("/w/o".into()));
        assert_eq!(get(&p, "keep_branch"), &Value::Bool(true));
        assert!(
            !p.contains_key("force"),
            "force is sent only when given (agents may not set it)"
        );
        let (_, p) = call(&parse(&["archive", "7", "--force"]), cwd).unwrap();
        assert_eq!(get(&p, "force"), &Value::Bool(true));
        let (name, p) = call(&parse(&["ls"]), cwd).unwrap();
        assert_eq!(name, "git-worktree.ls");
        assert!(p.is_empty());
        let (name, p) = call(&parse(&["prune", "--repo", "/r", "--dry-run"]), cwd).unwrap();
        assert_eq!(name, "git-worktree.prune");
        assert_eq!(get(&p, "repo"), &Value::String("/r".into()));
        assert_eq!(get(&p, "dry_run"), &Value::Bool(true));
    }

    #[test]
    fn the_worktree_alias_and_usage_errors() {
        assert!(Cli::try_parse_from(["wt", "archive"]).is_err());
        assert!(
            Cli::try_parse_from(["wt", "new", "s"]).is_err(),
            "a command is required"
        );
        assert!(Cli::try_parse_from(["wt", "new", "s", "--shared", "--", "sh"]).is_err());
    }

    #[test]
    fn refusals_name_the_flags_to_add() {
        let r = ActionRefused {
            reason: "x".into(),
            retry: [("reuse", true), ("shared", true), ("keep_branch", true)]
                .into_iter()
                .map(|(k, v)| (k.to_string(), boolean(v)))
                .collect(),
        };
        assert_eq!(retry_flags(&r), "--keep-branch --reuse --shared");
    }

    fn diff() -> DiffView {
        DiffView {
            files: vec![
                FileDiff {
                    old_path: "src/a.rs".into(),
                    new_path: "src/a.rs".into(),
                    status: FileStatus::FileModified as i32,
                    additions: 3,
                    deletions: 1,
                    hunks: vec![Hunk {
                        old_start: 1,
                        old_lines: 2,
                        new_start: 1,
                        new_lines: 4,
                        body: b" a\n-b\n+c\n+d\n+e".to_vec(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                FileDiff {
                    new_path: "img.png".into(),
                    status: FileStatus::FileAdded as i32,
                    binary: true,
                    ..Default::default()
                },
            ],
            files_changed: 2,
            additions: 3,
            deletions: 1,
            ..Default::default()
        }
    }

    #[test]
    fn stat_lines_and_summary() {
        let s = render_stat(&diff());
        assert_eq!(
            s,
            " src/a.rs | 4 +++-\n img.png  | Bin\n 2 files changed, 3 insertions(+), 1 deletion(-)\n"
        );
    }

    #[test]
    fn colours_for_a_terminal() {
        let c = colour("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n c\n");
        assert!(c.contains("\x1b[1mdiff --git a/x b/x\x1b[0m"), "{c:?}");
        assert!(c.contains("\x1b[36m@@ -1 +1 @@\x1b[0m"));
        assert!(c.contains("\x1b[31m-a\x1b[0m"));
        assert!(c.contains("\x1b[32m+b\x1b[0m"));
        assert!(c.contains("\x1b[1m--- a/x\x1b[0m") && c.contains("\x1b[1m+++ b/x\x1b[0m"));
        assert!(c.contains("\n c\n"));
    }
}
