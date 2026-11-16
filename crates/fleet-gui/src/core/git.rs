//! How an agent's git checkout is shown (ADR 0085). The node reads it on
//! its own host and sends it in `AgentInfo.git`; nothing here touches the
//! file system, so drawing never reads git.

use fleet_proto::{AgentInfo, GitInfo};

/// The branch, or `detached <hash>`.
fn branch_text(git: &GitInfo) -> String {
    if git.detached {
        format!("detached {}", git.branch)
    } else {
        git.branch.clone()
    }
}

/// A linked worktree's own name, if it differs from the repository's.
fn worktree(git: &GitInfo) -> Option<&str> {
    Some(git.worktree.as_str()).filter(|w| !w.is_empty() && *w != git.repo)
}

/// The sidebar row's subtitle: the checkout's folder (the worktree's, else
/// the repository's) and the branch, as `issue-97 · feat/x`. `None` when
/// the agent's cwd is in no checkout (or the node did not say).
pub fn row_subtitle(info: &AgentInfo) -> Option<String> {
    let git = info.git.as_ref()?;
    let name = worktree(git).unwrap_or(&git.repo);
    Some(format!("{name} · {}", branch_text(git)))
}

/// The pane header's text: the repository, its worktree in brackets when
/// linked, and the branch, as `fleetd (issue-97) · feat/x`.
pub fn header_text(info: &AgentInfo) -> Option<String> {
    let git = info.git.as_ref()?;
    let repo = match worktree(git) {
        Some(w) => format!("{} ({w})", git.repo),
        None => git.repo.clone(),
    };
    Some(format!("{repo} · {}", branch_text(git)))
}

/// The repository's name: the sidebar's group when grouping by repo.
pub fn repo_name(info: &AgentInfo) -> Option<&str> {
    info.git
        .as_ref()
        .map(|g| g.repo.as_str())
        .filter(|r| !r.is_empty())
}

/// What the filter, the palette and the switcher match: repository,
/// worktree and branch.
pub fn search_terms(info: &AgentInfo) -> Vec<String> {
    match &info.git {
        Some(g) => [&g.repo, &g.worktree, &g.branch]
            .into_iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_git(repo: &str, worktree: &str, branch: &str, detached: bool) -> AgentInfo {
        AgentInfo {
            git: Some(GitInfo {
                repo: repo.into(),
                worktree: worktree.into(),
                branch: branch.into(),
                detached,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_main_checkout_shows_the_repo_and_branch() {
        let info = with_git("fleetd", "", "main", false);
        assert_eq!(row_subtitle(&info).as_deref(), Some("fleetd · main"));
        assert_eq!(header_text(&info).as_deref(), Some("fleetd · main"));
        assert_eq!(repo_name(&info), Some("fleetd"));
    }

    #[test]
    fn a_worktree_shows_its_own_name_on_the_row_and_both_in_the_header() {
        let info = with_git("fleetd", "issue-97", "feat/gui-git-branch", false);
        assert_eq!(
            row_subtitle(&info).as_deref(),
            Some("issue-97 · feat/gui-git-branch")
        );
        assert_eq!(
            header_text(&info).as_deref(),
            Some("fleetd (issue-97) · feat/gui-git-branch")
        );
        // A worktree named like its repository is not repeated.
        let same = with_git("fleetd", "fleetd", "x", false);
        assert_eq!(header_text(&same).as_deref(), Some("fleetd · x"));
    }

    #[test]
    fn a_detached_head_says_so() {
        let info = with_git("fleetd", "", "1a4a202", true);
        assert_eq!(
            row_subtitle(&info).as_deref(),
            Some("fleetd · detached 1a4a202")
        );
    }

    #[test]
    fn no_checkout_shows_nothing() {
        let info = AgentInfo::default();
        assert_eq!(row_subtitle(&info), None);
        assert_eq!(header_text(&info), None);
        assert_eq!(repo_name(&info), None);
        assert!(search_terms(&info).is_empty());
        assert_eq!(
            search_terms(&with_git("fleetd", "", "main", false)),
            ["fleetd", "main"]
        );
    }
}
