//! `git-worktree.archive` and `git-worktree.prune` (M4 spec §3, ADR 0010's
//! archive rule): a worktree goes only when nothing in it would be lost,
//! unless the user forces it; never while an agent runs inside it.

use std::collections::BTreeSet;

use fleet_proto::param_value::Value;
use fleet_proto::{AgentInfo, TableRow, TableView, View, view};

use crate::error::ActionError;
use crate::host::{Caller, HostResult, Labels};
use crate::registry::{ActionCall, ActionOutcome};

use super::git::{Git, Mode};
use super::new_agent::fetch_base;
use super::parse::{parse_worktree_list, split_nul};
use super::{
    GitWorktree, LABEL_WORKTREE, agent_name, agents_in, base_of, caller_tag, default_base,
    delete_record, get_record, inside, records, refused, repo_param_or_cwd, resolve_repo, running,
    target,
};

/// What decides whether a worktree may go.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// The checked-out branch; `None` when detached.
    pub branch: Option<String>,
    /// Why it is not clean: uncommitted changes, untracked files.
    pub dirty: Option<String>,
    /// Commits on no remote branch and not in the base.
    pub local_commits: bool,
    /// `HEAD` is in the base.
    pub merged: bool,
}

pub async fn inspect(git: &Git<'_>, wt: &str, base: &str) -> HostResult<State> {
    let status = git
        .ok(
            wt,
            &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
            Mode::Read,
        )
        .await?;
    let fields = split_nul(status.as_bytes());
    let untracked = fields.iter().any(|f| f.starts_with("?? "));
    let changed = fields
        .iter()
        .any(|f| f.len() > 3 && f.as_bytes()[2] == b' ' && !f.starts_with("?? "));
    let dirty = match (changed, untracked) {
        (true, true) => Some("uncommitted changes and untracked files".to_string()),
        (true, false) => Some("uncommitted changes".to_string()),
        (false, true) => Some("untracked files".to_string()),
        (false, false) => None,
    };
    let merged = git
        .test(wt, &["merge-base", "--is-ancestor", "HEAD", base])
        .await?;
    let local_commits = !merged
        && !git
            .line(wt, &["rev-list", "-n1", "HEAD", "--not", "--remotes", base])
            .await?
            .is_empty();
    let branch = git
        .line(wt, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .ok()
        .filter(|b| !b.is_empty());
    Ok(State {
        branch,
        dirty,
        local_commits,
        merged,
    })
}

/// Why `wt` may not go even with `force`: agents running inside it.
fn running_inside(agents: &[AgentInfo], wt: &str) -> Option<String> {
    let names: Vec<_> = agents_in(agents, wt)
        .into_iter()
        .filter(|a| running(a))
        .map(agent_name)
        .collect();
    (!names.is_empty()).then(|| format!("{} running inside {wt}: kill it first", names.join(", ")))
}

/// Why `wt` may not go without `force`.
fn blocker(state: &State, base: &str) -> Option<String> {
    if let Some(d) = &state.dirty {
        return Some(d.clone());
    }
    if state.local_commits {
        return Some(format!(
            "commits that are on no remote branch and not merged into {base}"
        ));
    }
    None
}

/// Removes `wt` and, unless kept, its branch (merged into the base, or
/// any with `force`); forgets the ended agents in it and drops its record.
/// What it did, as a message.
async fn remove(
    git: &Git<'_>,
    main: &str,
    wt: &str,
    state: &State,
    agents: &[AgentInfo],
    force: bool,
    keep_branch: bool,
) -> HostResult<String> {
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(wt);
    git.ok(main, &args, Mode::Write).await?;
    let mut said = vec![format!("archived {wt}")];
    if let Some(b) = &state.branch {
        if keep_branch {
            said.push(format!("kept branch {b}"));
        } else if state.merged || force {
            // Our own merged-into-base check decides, not `branch -d`'s
            // (which looks at the main checkout's HEAD).
            git.ok(main, &["branch", "-D", b], Mode::Write).await?;
            let why = if state.merged { "" } else { " (not merged)" };
            said.push(format!("deleted branch {b}{why}"));
        } else {
            said.push(format!("kept branch {b} (not merged into the base)"));
        }
    }
    let ended: Vec<u64> = agents_in(agents, wt)
        .into_iter()
        .filter(|a| !running(a))
        .map(|a| a.id)
        .collect();
    if !ended.is_empty() {
        let f = git.host.forget(ended).await?;
        if !f.ids.is_empty() {
            said.push(format!(
                "forgot agents {}",
                f.ids
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    delete_record(git.host, wt).await?;
    Ok(said.join("; "))
}

pub async fn run(git: &Git<'_>, call: &ActionCall) -> Result<ActionOutcome, ActionError> {
    let t = target(git, call).await?;
    let wt = t.path.as_str();
    let repo = resolve_repo(git, wt).await?;
    if wt == repo.main {
        return Err(ActionError::invalid(format!(
            "{wt} is the main checkout; only linked worktrees are archived"
        )));
    }
    let caller = git.host.caller();
    let agents = git.host.list_agents(Labels::new()).await?;
    if let Caller::Agent(a) = caller {
        // By path: only a worktree made for one of D(a) (an `agent` param
        // is already held to D(a) by the host).
        if t.agent.is_none() {
            let made_by_it = get_record(git.host, wt)
                .await?
                .is_some_and(|r| r.created_by == caller_tag(caller));
            let for_its_own = agents
                .iter()
                .any(|x| x.labels.get(LABEL_WORKTREE).map(String::as_str) == Some(wt));
            if !made_by_it && !for_its_own {
                return Err(ActionError::forbidden(format!(
                    "agent {a} may archive only worktrees made for its descendants, not {wt}"
                )));
            }
        }
        if inside(&call.cwd, wt) {
            return Ok(refused(format!("agent {a} is working inside {wt}"), &[]));
        }
    }
    if let Some(reason) = running_inside(&agents, wt) {
        return Ok(refused(reason, &[]));
    }
    let force = call.params.bool("force").unwrap_or(false);
    let keep_branch = call.params.bool("keep_branch").unwrap_or(false);
    let base = base_of(git, &t, &repo.main).await?;
    let state = inspect(git, wt, &base).await?;
    if !force && let Some(why) = blocker(&state, &base) {
        let retry: &[(&str, Value)] = match caller {
            Caller::User => &[("force", Value::Bool(true))],
            // `force` is the user's: an agent gets no override.
            Caller::Agent(_) => &[],
        };
        return Ok(refused(format!("{wt} has {why}"), retry));
    }
    let message = remove(git, &repo.main, wt, &state, &agents, force, keep_branch).await?;
    Ok(ActionOutcome::Done(fleet_proto::ActionDone { message }))
}

pub async fn prune(
    flow: &GitWorktree,
    git: &Git<'_>,
    call: &ActionCall,
) -> Result<ActionOutcome, ActionError> {
    let repo = repo_param_or_cwd(git, call).await?;
    let (config, _) = flow.config(&repo.name)?;
    let dry = call.params.bool("dry_run").unwrap_or(false);
    let listed = parse_worktree_list(
        git.ok(
            &repo.main,
            &["worktree", "list", "--porcelain", "-z"],
            Mode::Read,
        )
        .await?
        .as_bytes(),
    );
    let mut made: Vec<_> = records(git.host)
        .await?
        .into_iter()
        .filter(|(_, r)| r.repo == repo.main)
        .collect();
    made.sort_by(|a, b| a.0.cmp(&b.0));
    let agents = git.host.list_agents(Labels::new()).await?;
    let mut notes = Vec::new();
    if config.fetch_base {
        let bases: BTreeSet<&str> = made.iter().map(|(_, r)| r.base.as_str()).collect();
        for base in bases.into_iter().filter(|b| !b.is_empty()) {
            if let Some(n) = fetch_base(git, &repo.main, base).await? {
                notes.push(n);
            }
        }
    }
    let mut fallback: Option<String> = None;
    let mut rows = Vec::new();
    for (wt, record) in &made {
        let base = if record.base.is_empty() {
            if fallback.is_none() {
                fallback = Some(default_base(git, &repo.main).await?);
            }
            fallback.clone().unwrap_or_default()
        } else {
            record.base.clone()
        };
        let result = match listed.iter().find(|w| &w.path == wt) {
            None => {
                if !dry {
                    delete_record(git.host, wt).await?;
                }
                "gone (made by hand since; record dropped)".to_string()
            }
            Some(w) if w.prunable => "keep: its folder is missing (git worktree prune)".into(),
            Some(_) => match running_inside(&agents, wt) {
                Some(_) => "keep: an agent is running inside".into(),
                None => {
                    let state = inspect(git, wt, &base).await?;
                    if let Some(d) = &state.dirty {
                        format!("keep: {d}")
                    } else if !state.merged {
                        format!("keep: not merged into {base}")
                    } else if dry {
                        "remove".into()
                    } else {
                        match remove(git, &repo.main, wt, &state, &agents, false, false).await {
                            Ok(m) => format!("removed ({m})"),
                            Err(ActionError::Cancelled) => return Err(ActionError::Cancelled),
                            Err(e) => format!("keep: {e}"),
                        }
                    }
                }
            },
        };
        rows.push(TableRow {
            cells: vec![wt.clone(), record.branch.clone(), base, result],
            agent_id: None,
            path: wt.clone(),
        });
    }
    let mut title = format!("prune {}", repo.name);
    if dry {
        title.push_str(" (dry run)");
    }
    for n in notes {
        title.push_str("; ");
        title.push_str(&n);
    }
    Ok(ActionOutcome::View(View {
        kind: Some(view::Kind::Table(TableView {
            title,
            columns: vec![
                "path".into(),
                "branch".into(),
                "base".into(),
                "result".into(),
            ],
            rows,
        })),
    }))
}
