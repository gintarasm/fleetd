//! The built-in `git-worktree` workflow (ADR 0093, M4 spec §3): a new agent
//! in a new worktree, the read-only diff of what an agent changed, archive,
//! list and prune.
//!
//! Everything git is `git` through the host's `Exec` ([`git`]); output is
//! parsed by pure functions ([`parse`]). The core never learns about git
//! (ADR 0025): the workflow finds an agent's worktree by the `git.*` labels
//! it put on the agent when it spawned it.

mod archive;
pub mod config;
pub mod diff;
pub mod git;
mod ls;
mod new_agent;
pub mod parse;
#[cfg(test)]
mod testhost;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;

use fleet_proto::param_value::Value;
use fleet_proto::{
    ActionCallers, ActionRefused, ActionSpec, AgentInfo, AgentState, ParamKind, ParamSpec,
    ParamValue,
};
use serde::{Deserialize, Serialize};

use crate::error::ActionError;
use crate::host::{BoxFuture, Caller, Host, HostResult, Labels};
use crate::manifest::Manifest;
use crate::registry::{ActionCall, ActionOutcome, Workflow};
use config::Config;
use git::Git;

pub const ID: &str = "git-worktree";
/// The main checkout's absolute path.
pub const LABEL_REPO: &str = "git.repo";
/// The worktree's absolute path.
pub const LABEL_WORKTREE: &str = "git.worktree";
/// The base ref the worktree was made from (`origin/main`).
pub const LABEL_BASE: &str = "git.base";
/// Storage keys of the worktrees this workflow made: `wt/<path>`.
const RECORD_PREFIX: &str = "wt/";

/// What the workflow reads each time an action runs: the config file's
/// text (`None`: no file) and the user's home (for `~` in `root`).
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub config: Option<String>,
    pub home: String,
}

pub type SettingsSource = Arc<dyn Fn() -> Settings + Send + Sync>;

pub struct GitWorktree {
    settings: SettingsSource,
}

impl GitWorktree {
    pub fn new(settings: SettingsSource) -> Self {
        GitWorktree { settings }
    }

    fn config(&self, repo: &str) -> HostResult<(Config, String)> {
        let s = (self.settings)();
        let config = Config::for_repo(s.config.as_deref(), repo).map_err(ActionError::failed)?;
        Ok((config, s.home))
    }
}

/// A record of a worktree this workflow made (`wt/<path>` in storage).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The main checkout.
    pub repo: String,
    pub branch: String,
    pub base: String,
    pub created_ms: i64,
    /// `user` or `agent:<id>`.
    pub created_by: String,
    /// The agent started in it, once spawned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<u64>,
}

fn record_key(path: &str) -> String {
    format!("{RECORD_PREFIX}{path}")
}

pub async fn get_record(host: &dyn Host, path: &str) -> HostResult<Option<Record>> {
    let Some(bytes) = host.storage().get(&record_key(path)).await? else {
        return Ok(None);
    };
    // A record this build cannot read is treated as none.
    Ok(serde_json::from_slice(&bytes).ok())
}

pub async fn put_record(host: &dyn Host, path: &str, r: &Record) -> HostResult<()> {
    let bytes = serde_json::to_vec(r).map_err(|e| ActionError::failed(e.to_string()))?;
    host.storage().put(&record_key(path), bytes).await
}

pub async fn delete_record(host: &dyn Host, path: &str) -> HostResult<bool> {
    host.storage().delete(&record_key(path)).await
}

/// Every record, with its worktree's path.
pub async fn records(host: &dyn Host) -> HostResult<Vec<(String, Record)>> {
    let mut out = Vec::new();
    for key in host.storage().keys(RECORD_PREFIX).await? {
        let path = key[RECORD_PREFIX.len()..].to_owned();
        if let Some(r) = get_record(host, &path).await? {
            out.push((path, r));
        }
    }
    Ok(out)
}

fn caller_tag(c: Caller) -> String {
    match c {
        Caller::User => "user".into(),
        Caller::Agent(a) => format!("agent:{a}"),
    }
}

/// A repository: its main checkout and name (the folder's, as ADR 0085).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    pub main: String,
    pub name: String,
}

/// The repository holding `dir` (any checkout of it).
pub async fn resolve_repo(git: &Git<'_>, dir: &str) -> HostResult<Repo> {
    let common = git
        .line(
            dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await
        .map_err(|e| ActionError::invalid(format!("{dir} is not in a git repository ({e})")))?;
    let Some(main) = common.strip_suffix("/.git") else {
        return Err(ActionError::invalid(format!(
            "{dir}: a bare repository has no main checkout ({common})"
        )));
    };
    let name = main.rsplit('/').next().unwrap_or(main).to_owned();
    Ok(Repo {
        main: main.to_owned(),
        name,
    })
}

/// A ref named by a caller or a label (`base`, `branch`), checked before it
/// reaches git's argv: not empty, no component starting with `-` (so it
/// can never be read as an option, e.g. `origin/--upload-pack=…` split
/// into a fetch's refspec), no whitespace or control characters. Git calls
/// also put `--end-of-options` before positional refs; this is the first
/// line.
pub fn check_ref(what: &str, v: &str) -> Result<(), ActionError> {
    let bad = v.is_empty()
        || v.split('/').any(|c| c.starts_with('-'))
        || v.chars().any(|c| c.is_whitespace() || c.is_control());
    if bad {
        return Err(ActionError::invalid(format!(
            "{what} {v:?} is not a ref (no part may start with `-`, no spaces)"
        )));
    }
    Ok(())
}

/// The top folder of the checkout holding `dir`.
pub async fn toplevel(git: &Git<'_>, dir: &str) -> HostResult<String> {
    git.line(dir, &["rev-parse", "--show-toplevel"])
        .await
        .map_err(|e| ActionError::invalid(format!("{dir} is not in a git checkout ({e})")))
}

/// The repository's default branch: the remote's (`origin/HEAD`), else the
/// main checkout's branch, else its commit.
pub async fn default_base(git: &Git<'_>, main: &str) -> HostResult<String> {
    let remote = [
        "symbolic-ref",
        "--quiet",
        "--short",
        "refs/remotes/origin/HEAD",
    ];
    if let Ok(b) = git.line(main, &remote).await
        && !b.is_empty()
    {
        return Ok(b);
    }
    if let Ok(b) = git
        .line(main, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        && !b.is_empty()
    {
        return Ok(b);
    }
    git.line(main, &["rev-parse", "--verify", "HEAD"]).await
}

/// The checked-out branch of `dir`, else `detached <short commit>`.
pub async fn branch_of(git: &Git<'_>, dir: &str) -> String {
    match git
        .line(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
    {
        Ok(b) if !b.is_empty() => b,
        _ => match git.line(dir, &["rev-parse", "--short", "HEAD"]).await {
            Ok(c) => format!("detached {c}"),
            Err(_) => "no commits".into(),
        },
    }
}

/// Whether `path` is `dir` or inside it.
pub fn inside(path: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    path == dir || path.strip_prefix(dir).is_some_and(|r| r.starts_with('/'))
}

/// The agents (of `agents`) of worktree `wt`: started for it (its
/// `git.worktree` label) or working inside it.
pub fn agents_in<'a>(agents: &'a [AgentInfo], wt: &str) -> Vec<&'a AgentInfo> {
    agents
        .iter()
        .filter(|a| {
            a.labels.get(LABEL_WORKTREE).map(String::as_str) == Some(wt) || inside(&a.cwd, wt)
        })
        .collect()
}

pub fn running(a: &AgentInfo) -> bool {
    a.state() == AgentState::Running
}

/// `name#id`, or `#id`.
pub fn agent_name(a: &AgentInfo) -> String {
    match a.labels.get("name") {
        Some(n) => format!("{n}#{}", a.id),
        None => format!("#{}", a.id),
    }
}

/// The worktree an action is about: from an `agent` param (its
/// `git.worktree` label, else its cwd's checkout) or a `path` param.
pub struct Target {
    pub path: String,
    /// The agent named, if any.
    pub agent: Option<AgentInfo>,
}

pub async fn target(git: &Git<'_>, call: &ActionCall) -> HostResult<Target> {
    match (call.params.agent("agent"), call.params.string("path")) {
        (Some(_), Some(_)) => Err(ActionError::invalid("give `agent` or `path`, not both")),
        (None, None) => Err(ActionError::invalid("give `agent` or `path`")),
        (None, Some(p)) => Ok(Target {
            path: toplevel(git, p).await?,
            agent: None,
        }),
        (Some(id), None) => {
            let agents = git.host.list_agents(Labels::new()).await?;
            let Some(a) = agents.into_iter().find(|a| a.id == id) else {
                return Err(ActionError::invalid(format!("no agent {id}")));
            };
            let path = match a.labels.get(LABEL_WORKTREE) {
                Some(wt) => wt.clone(),
                None => toplevel(git, &a.cwd).await?,
            };
            Ok(Target {
                path,
                agent: Some(a),
            })
        }
    }
}

/// The base a worktree is compared with: the agent's `git.base`, else the
/// workflow's record, else the repository's default branch.
pub async fn base_of(git: &Git<'_>, t: &Target, main: &str) -> HostResult<String> {
    let base = if let Some(b) = t.agent.as_ref().and_then(|a| a.labels.get(LABEL_BASE)) {
        b.clone()
    } else if let Some(r) = get_record(git.host, &t.path).await?
        && !r.base.is_empty()
    {
        r.base
    } else {
        default_base(git, main).await?
    };
    check_ref("base", &base)?;
    Ok(base)
}

pub fn refused(reason: impl Into<String>, retry: &[(&str, Value)]) -> ActionOutcome {
    ActionOutcome::Refused(ActionRefused {
        reason: reason.into(),
        retry: retry
            .iter()
            .map(|(k, v)| {
                (
                    (*k).to_string(),
                    ParamValue {
                        value: Some(v.clone()),
                    },
                )
            })
            .collect::<HashMap<_, _>>(),
    })
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn param(key: &str, title: &str, kind: ParamKind, help: &str) -> ParamSpec {
    ParamSpec {
        key: key.into(),
        title: title.into(),
        kind: kind as i32,
        help: help.into(),
        ..Default::default()
    }
}

fn required(mut p: ParamSpec) -> ParamSpec {
    p.required = true;
    p
}

fn default(mut p: ParamSpec, v: Value) -> ParamSpec {
    p.default = Some(ParamValue { value: Some(v) });
    p
}

fn placeholder(mut p: ParamSpec, text: &str) -> ParamSpec {
    p.placeholder = text.into();
    p
}

fn action(
    name: &str,
    title: &str,
    description: &str,
    callers: ActionCallers,
    destructive: bool,
    target_param: &str,
    params: Vec<ParamSpec>,
) -> ActionSpec {
    ActionSpec {
        name: format!("{ID}.{name}"),
        title: title.into(),
        description: description.into(),
        params,
        callers: callers as i32,
        destructive,
        target_param: target_param.into(),
    }
}

fn agent_or_path() -> [ParamSpec; 2] {
    [
        param(
            "agent",
            "Agent",
            ParamKind::ParamAgent,
            "The agent whose worktree it is (or give a path)",
        ),
        param(
            "path",
            "Worktree",
            ParamKind::ParamPath,
            "A folder in the worktree (or give an agent)",
        ),
    ]
}

fn repo_param() -> ParamSpec {
    param(
        "repo",
        "Repository",
        ParamKind::ParamPath,
        "Any checkout of the repository; the caller's folder when not given",
    )
}

/// The workflow's actions (M4 spec §3): names and params are what clients
/// call (the GUI and `fleet wt`), so they do not change.
pub fn specs() -> Vec<ActionSpec> {
    use ActionCallers::{User, UserAndAgents as Both};
    use ParamKind::*;
    let [agent, path] = agent_or_path();
    vec![
        action(
            "new-agent",
            "New Agent in a Worktree",
            "Makes a git worktree on a new branch and starts an agent in it",
            Both,
            false,
            "",
            vec![
                placeholder(
                    required(param(
                        "slug",
                        "Slug",
                        ParamString,
                        "The worktree's folder name: letters, digits, . _ -",
                    )),
                    "issue-140",
                ),
                required(param(
                    "command",
                    "Command",
                    ParamArgv,
                    "The agent's program and arguments",
                )),
                repo_param(),
                param(
                    "base",
                    "Base",
                    ParamString,
                    "The ref to branch from; the default branch when not given",
                ),
                param(
                    "branch",
                    "Branch",
                    ParamString,
                    "The new branch; the configured prefix + slug when not given",
                ),
                param("name", "Name", ParamString, "The agent's name"),
                param("labels", "Labels", ParamLabels, "The agent's labels"),
                default(
                    param(
                        "reuse",
                        "Reuse",
                        ParamBool,
                        "Start the agent in the slug's existing worktree",
                    ),
                    Value::Bool(false),
                ),
                default(
                    param(
                        "shared",
                        "Shared",
                        ParamBool,
                        "With reuse: even if another agent is working there",
                    ),
                    Value::Bool(false),
                ),
            ],
        ),
        action(
            "diff",
            "Show Changes",
            "What changed in a worktree: against its base, uncommitted, or committed",
            Both,
            false,
            "agent",
            vec![
                agent.clone(),
                path.clone(),
                ParamSpec {
                    choices: vec!["base".into(), "uncommitted".into(), "committed".into()],
                    ..default(
                        param(
                            "against",
                            "Against",
                            ParamChoice,
                            "base: everything since the merge base; uncommitted: against HEAD; committed: commits only",
                        ),
                        Value::String("base".into()),
                    )
                },
                param(
                    "file",
                    "File",
                    ParamString,
                    "Only this file (a path in the worktree)",
                ),
                default(
                    param(
                        "context",
                        "Context",
                        ParamInt,
                        "Lines of context around changes",
                    ),
                    Value::Int(3),
                ),
            ],
        ),
        action(
            "archive",
            "Archive Worktree",
            "Removes a worktree and its merged branch",
            Both,
            true,
            "agent",
            vec![
                agent,
                path,
                ParamSpec {
                    user_only: true,
                    ..param(
                        "force",
                        "Force",
                        ParamBool,
                        "Even with uncommitted changes or unmerged commits (deletes the branch)",
                    )
                },
                default(
                    param("keep_branch", "Keep Branch", ParamBool, "Keep the branch"),
                    Value::Bool(false),
                ),
            ],
        ),
        action(
            "ls",
            "List Worktrees",
            "The repository's worktrees: branch, base, ahead/behind, dirty, merged, agents",
            Both,
            false,
            "",
            vec![repo_param()],
        ),
        action(
            "prune",
            "Prune Worktrees",
            "Archives every worktree this workflow made that is clean, merged and has no running agent",
            User,
            true,
            "",
            vec![
                repo_param(),
                default(
                    param(
                        "dry_run",
                        "Dry Run",
                        ParamBool,
                        "Only list what would be removed",
                    ),
                    Value::Bool(false),
                ),
            ],
        ),
    ]
}

/// The repository a `repo` param (else the caller's cwd) names.
pub async fn repo_param_or_cwd(git: &Git<'_>, call: &ActionCall) -> HostResult<Repo> {
    let dir = call.params.string("repo").unwrap_or(&call.cwd);
    if !dir.starts_with('/') {
        return Err(ActionError::invalid(
            "give `repo`: the caller has no working directory",
        ));
    }
    resolve_repo(git, dir).await
}

impl Workflow for GitWorktree {
    fn id(&self) -> &str {
        ID
    }

    fn manifest(&self) -> Result<Manifest, String> {
        Manifest::parse(&[
            "exec:git",
            "fs:read:*",
            "spawn",
            "labels:git.",
            "storage:own",
        ])
    }

    fn actions(&self) -> Vec<ActionSpec> {
        specs()
    }

    fn run<'a>(
        &'a self,
        host: &'a dyn Host,
        call: ActionCall,
    ) -> BoxFuture<'a, Result<ActionOutcome, ActionError>> {
        Box::pin(async move {
            let git = Git { host };
            match call.action.as_str() {
                "new-agent" => new_agent::run(self, &git, &call).await,
                "diff" => diff::run(&git, &call).await,
                "archive" => archive::run(&git, &call).await,
                "ls" => ls::run(&git, &call).await,
                "prune" => archive::prune(self, &git, &call).await,
                other => Err(ActionError::invalid(format!("no action {ID}.{other}"))),
            }
        })
    }
}
