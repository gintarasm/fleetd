//! `git-worktree.ls` (M4 spec §3): every worktree of a repository, made by
//! this workflow or by hand, with its branch, base, ahead/behind, dirty,
//! merged, and the agents in it.

use std::collections::HashMap;

use fleet_proto::{AgentInfo, TableRow, TableView, View, view};

use crate::error::ActionError;
use crate::host::Labels;
use crate::registry::{ActionCall, ActionOutcome};

use super::git::{Git, Mode};
use super::parse::{WorktreeEntry, parse_worktree_list};
use super::{
    LABEL_BASE, LABEL_WORKTREE, agent_name, check_ref, default_base, inside, records,
    repo_param_or_cwd, running,
};

/// The worktree each agent belongs to: the one it was started for (its
/// `git.worktree`), else the deepest one holding its cwd.
fn owners<'a>(
    entries: &[WorktreeEntry],
    agents: &'a [AgentInfo],
) -> HashMap<String, Vec<&'a AgentInfo>> {
    let mut by_path: HashMap<String, Vec<&AgentInfo>> = HashMap::new();
    for a in agents {
        let labelled = a
            .labels
            .get(LABEL_WORKTREE)
            .filter(|wt| entries.iter().any(|e| &&e.path == wt));
        let owner = labelled.cloned().or_else(|| {
            entries
                .iter()
                .filter(|e| inside(&a.cwd, &e.path))
                .max_by_key(|e| e.path.len())
                .map(|e| e.path.clone())
        });
        if let Some(p) = owner {
            by_path.entry(p).or_default().push(a);
        }
    }
    by_path
}

pub async fn run(git: &Git<'_>, call: &ActionCall) -> Result<ActionOutcome, ActionError> {
    let repo = repo_param_or_cwd(git, call).await?;
    let entries: Vec<_> = parse_worktree_list(
        git.ok(
            &repo.main,
            &["worktree", "list", "--porcelain", "-z"],
            Mode::Read,
        )
        .await?
        .as_bytes(),
    )
    .into_iter()
    .filter(|e| !e.bare)
    .collect();
    let made: HashMap<String, String> = records(git.host)
        .await?
        .into_iter()
        .filter(|(_, r)| r.repo == repo.main)
        .map(|(p, r)| (p, r.base))
        .collect();
    let agents = git.host.list_agents(Labels::new()).await?;
    let owners = owners(&entries, &agents);
    let none = Vec::new();
    let default = default_base(git, &repo.main).await.unwrap_or_default();

    let mut rows = Vec::new();
    for e in &entries {
        let theirs = owners.get(&e.path).unwrap_or(&none);
        let branch = match (&e.branch, e.detached) {
            (Some(b), _) => b.clone(),
            (None, true) => format!("detached {}", e.head.get(..7).unwrap_or(&e.head)),
            (None, false) => String::new(),
        };
        let is_main = e.path == repo.main;
        let base = if is_main {
            String::new()
        } else {
            made.get(&e.path)
                .filter(|b| !b.is_empty())
                .cloned()
                .or_else(|| {
                    theirs
                        .iter()
                        .find_map(|a| a.labels.get(LABEL_BASE).cloned())
                })
                .unwrap_or_else(|| default.clone())
        };
        // A bad base (a hand-written label) is not compared with.
        let base = if check_ref("base", &base).is_ok() {
            base
        } else {
            String::new()
        };
        let (mut ahead_behind, mut merged) = (String::new(), String::new());
        let dirty: String;
        if e.prunable {
            dirty = "missing".into();
        } else {
            let status = git
                .ok(&e.path, &["status", "--porcelain", "-z"], Mode::Read)
                .await;
            dirty = match status {
                Ok(s) if s.is_empty() => "no".into(),
                Ok(_) => "yes".into(),
                Err(_) => "?".into(),
            };
            if !base.is_empty() {
                let range = format!("{base}...HEAD");
                if let Ok(counts) = git
                    .line(
                        &e.path,
                        &[
                            "rev-list",
                            "--left-right",
                            "--count",
                            "--end-of-options",
                            &range,
                        ],
                    )
                    .await
                    && let Some((behind, ahead)) = counts.split_once('\t')
                {
                    ahead_behind = format!("+{ahead} -{behind}");
                }
                merged = match git
                    .test(
                        &e.path,
                        &[
                            "merge-base",
                            "--is-ancestor",
                            "--end-of-options",
                            "HEAD",
                            &base,
                        ],
                    )
                    .await
                {
                    Ok(true) => "yes".into(),
                    Ok(false) => "no".into(),
                    Err(_) => "?".into(),
                };
            }
        }
        let names: Vec<_> = theirs.iter().map(|a| agent_name(a)).collect();
        let linked = theirs
            .iter()
            .find(|a| running(a))
            .or(theirs.first())
            .map(|a| a.id);
        rows.push(TableRow {
            cells: vec![
                e.path.clone(),
                branch,
                base,
                ahead_behind,
                dirty,
                merged,
                names.join(", "),
            ],
            agent_id: linked,
            path: e.path.clone(),
        });
    }
    Ok(ActionOutcome::View(View {
        kind: Some(view::Kind::Table(TableView {
            title: format!("worktrees of {}", repo.name),
            columns: [
                "path",
                "branch",
                "base",
                "ahead/behind",
                "dirty",
                "merged",
                "agents",
            ]
            .map(String::from)
            .to_vec(),
            rows,
        })),
    }))
}
