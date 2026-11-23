//! What the GUI knows of the built-in `git-worktree` workflow (ADR 0093,
//! 0094): its action names and params, the slug rule, and when an agent
//! offers Show Changes and Archive Worktree…. The workflow itself does the
//! git work; nothing here runs git.

use std::collections::HashMap;

use fleet_proto::param_value::Value;
use fleet_proto::{ActionSpec, AgentInfo, AgentState, ParamValue, StringList, StringMap};

use super::status::AgentStatus;

pub const NEW_AGENT: &str = "git-worktree.new-agent";
pub const DIFF: &str = "git-worktree.diff";
pub const ARCHIVE: &str = "git-worktree.archive";

/// The worktree an agent was started for (set by the workflow).
pub const WORKTREE_LABEL: &str = "git.worktree";

/// What `against` a diff is taken, as the `diff` action names them.
pub const AGAINST: [&str; 3] = ["base", "uncommitted", "committed"];

/// The longest slug the workflow takes.
pub const MAX_SLUG: usize = 64;

/// The actions the daemon offers, as `ListActions` answered.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Actions {
    /// Not asked yet, or the answer is on its way.
    #[default]
    Unknown,
    /// The daemon predates workflows (it answered the unknown-request
    /// error): every workflow item is hidden.
    Unsupported,
    Listed(Vec<ActionSpec>),
}

impl Actions {
    /// The action `name`, when the daemon has it.
    pub fn get(&self, name: &str) -> Option<&ActionSpec> {
        match self {
            Actions::Listed(list) => list.iter().find(|a| a.name == name),
            _ => None,
        }
    }

    pub fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Every action, for Run Action…
    pub fn all(&self) -> &[ActionSpec] {
        match self {
            Actions::Listed(list) => list,
            _ => &[],
        }
    }
}

/// Show Changes is offered for an agent in a git checkout, while the
/// daemon has the diff action.
pub fn can_show_changes(info: &AgentInfo, actions: &Actions) -> bool {
    info.git.is_some() && actions.has(DIFF)
}

/// Archive Worktree… is offered for an ended agent the workflow started
/// in a worktree.
pub fn can_archive(info: &AgentInfo, actions: &Actions) -> bool {
    matches!(info.state(), AgentState::Exited | AgentState::Lost)
        && info.labels.contains_key(WORKTREE_LABEL)
        && actions.has(ARCHIVE)
}

/// A shown diff is refreshed once when its agent's status turns to
/// waiting, done or ended (never on output or a timer).
pub fn refresh_on(before: Option<AgentStatus>, after: Option<AgentStatus>) -> bool {
    let settled = |s: Option<AgentStatus>| {
        matches!(
            s,
            Some(AgentStatus::NeedsYou | AgentStatus::Done | AgentStatus::Failed)
        )
    };
    before != after && settled(after)
}

/// `dir` (on this machine) is inside a git checkout: it or a parent has a
/// `.git` (a folder, or a linked worktree's file). Read when the New
/// agent sheet's folder changes, never on a timer.
pub fn in_git_checkout(dir: &std::path::Path) -> bool {
    dir.is_absolute() && dir.ancestors().any(|d| d.join(".git").exists())
}

/// Why `slug` is not one the workflow takes, else `None`.
pub fn slug_error(slug: &str) -> Option<&'static str> {
    if slug.is_empty() {
        Some("A slug is needed")
    } else if slug.len() > MAX_SLUG {
        Some("At most 64 characters")
    } else if slug == "." || slug == ".." {
        Some("Not . or ..")
    } else if !slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        Some("Letters, digits, . _ - only")
    } else {
        None
    }
}

/// A slug from an agent name: lower case, other characters as `-`.
pub fn slug_from_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_') {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    out.chars().take(MAX_SLUG).collect()
}

/// The New agent sheet's "New worktree" fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewWorktree {
    pub slug: String,
    /// Empty: the workflow's prefix + slug.
    pub branch: String,
    /// Empty: the repository's default branch.
    pub base: String,
}

/// The params of `git-worktree.new-agent` for a spawn from the New agent
/// sheet: in `repo`, running `argv`, with the sheet's name and labels.
pub fn new_agent_params(
    wt: &NewWorktree,
    repo: &str,
    argv: Vec<String>,
    name: Option<&str>,
    labels: HashMap<String, String>,
) -> HashMap<String, ParamValue> {
    let mut p = HashMap::new();
    p.insert("slug".to_owned(), string(wt.slug.trim()));
    p.insert(
        "command".to_owned(),
        ParamValue {
            value: Some(Value::List(StringList { items: argv })),
        },
    );
    p.insert("repo".to_owned(), string(repo));
    for (k, v) in [("branch", &wt.branch), ("base", &wt.base)] {
        if !v.trim().is_empty() {
            p.insert(k.to_owned(), string(v.trim()));
        }
    }
    if let Some(name) = name.filter(|n| !n.trim().is_empty()) {
        p.insert("name".to_owned(), string(name.trim()));
    }
    if !labels.is_empty() {
        p.insert(
            "labels".to_owned(),
            ParamValue {
                value: Some(Value::Map(StringMap { entries: labels })),
            },
        );
    }
    p
}

/// The params of `git-worktree.diff` for an agent; `file` asks for one
/// omitted file.
pub fn diff_params(
    agent_id: u64,
    against: &str,
    file: Option<&str>,
) -> HashMap<String, ParamValue> {
    let mut p = HashMap::from([
        ("agent".to_owned(), int(agent_id)),
        ("against".to_owned(), string(against)),
    ]);
    if let Some(f) = file {
        p.insert("file".to_owned(), string(f));
    }
    p
}

/// The params of `git-worktree.archive` for an agent.
pub fn archive_params(agent_id: u64) -> HashMap<String, ParamValue> {
    HashMap::from([("agent".to_owned(), int(agent_id))])
}

fn string(s: &str) -> ParamValue {
    ParamValue {
        value: Some(Value::String(s.to_owned())),
    }
}

fn int(n: u64) -> ParamValue {
    ParamValue {
        value: Some(Value::Int(i64::try_from(n).unwrap_or(i64::MAX))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::GitInfo;

    fn listed(names: &[&str]) -> Actions {
        Actions::Listed(
            names
                .iter()
                .map(|n| ActionSpec {
                    name: (*n).into(),
                    ..Default::default()
                })
                .collect(),
        )
    }

    fn info(state: AgentState) -> AgentInfo {
        let mut i = AgentInfo {
            id: 1,
            ..Default::default()
        };
        i.set_state(state);
        i
    }

    #[test]
    fn show_changes_needs_a_checkout_and_the_action() {
        let mut i = info(AgentState::Running);
        let all = listed(&[DIFF, ARCHIVE]);
        assert!(!can_show_changes(&i, &all));
        i.git = Some(GitInfo::default());
        assert!(can_show_changes(&i, &all));
        assert!(!can_show_changes(&i, &Actions::Unsupported));
        assert!(!can_show_changes(&i, &listed(&[ARCHIVE])));
    }

    #[test]
    fn archive_needs_an_ended_agent_in_a_workflow_worktree() {
        let all = listed(&[DIFF, ARCHIVE]);
        let mut running = info(AgentState::Running);
        running.labels.insert(WORKTREE_LABEL.into(), "/wt/x".into());
        assert!(!can_archive(&running, &all));
        let mut exited = running.clone();
        exited.set_state(AgentState::Exited);
        assert!(can_archive(&exited, &all));
        assert!(!can_archive(&exited, &listed(&[DIFF])));
        exited.labels.clear();
        assert!(!can_archive(&exited, &all));
    }

    #[test]
    fn refresh_once_on_turning_waiting_done_or_ended() {
        use AgentStatus::*;
        assert!(refresh_on(Some(Working), Some(NeedsYou)));
        assert!(refresh_on(Some(Working), Some(Done)));
        assert!(refresh_on(Some(NeedsYou), Some(Failed)));
        assert!(!refresh_on(Some(Done), Some(Done)));
        assert!(!refresh_on(Some(Idle), Some(Working)));
        assert!(!refresh_on(Some(NeedsYou), Some(Idle)));
    }

    #[test]
    fn checkouts_are_found_from_any_folder_inside() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../repo/.git/worktrees/wt").unwrap();
        assert!(in_git_checkout(&repo));
        assert!(in_git_checkout(&repo.join("src/deep")));
        assert!(in_git_checkout(&wt));
        assert!(!in_git_checkout(dir.path()));
        assert!(!in_git_checkout(std::path::Path::new("relative")));
    }

    #[test]
    fn slugs_follow_the_workflows_rule() {
        assert_eq!(slug_error("issue-140"), None);
        assert_eq!(slug_error("a.b_c"), None);
        assert!(slug_error("").is_some());
        assert!(slug_error("..").is_some());
        assert!(slug_error("a/b").is_some());
        assert!(slug_error(&"x".repeat(65)).is_some());
    }

    #[test]
    fn slugs_come_from_names() {
        assert_eq!(slug_from_name("M4 D: GUI diff"), "m4-d-gui-diff");
        assert_eq!(slug_from_name("  --x--  "), "x");
        assert_eq!(slug_from_name(&"a".repeat(80)).len(), MAX_SLUG);
        assert_eq!(slug_error(&slug_from_name("Ünïcode name")), None);
    }

    #[test]
    fn new_agent_params_leave_out_blank_fields() {
        let wt = NewWorktree {
            slug: " fix-1 ".into(),
            branch: String::new(),
            base: "origin/dev".into(),
        };
        let p = new_agent_params(
            &wt,
            "/src/repo",
            vec!["claude".into()],
            Some("fixer"),
            HashMap::new(),
        );
        let mut keys: Vec<_> = p.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["base", "command", "name", "repo", "slug"]);
        assert_eq!(p["slug"], string("fix-1"));
        assert_eq!(p["repo"], string("/src/repo"));
    }

    #[test]
    fn diff_params_name_the_agent_and_file() {
        let p = diff_params(7, "base", Some("src/a.rs"));
        assert_eq!(p["agent"], int(7));
        assert_eq!(p["against"], string("base"));
        assert_eq!(p["file"], string("src/a.rs"));
        assert!(!diff_params(7, "committed", None).contains_key("file"));
        assert_eq!(archive_params(3)["agent"], int(3));
    }
}
