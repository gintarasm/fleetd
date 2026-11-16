//! The git checkout an agent's cwd is in (ADR 0085): repository and
//! worktree names and the checked-out branch, read from the files under
//! `.git` (never by running `git`), on the node's host, and cached per cwd
//! for [`GIT_TTL`].
//!
//! A lookup walks up from the cwd to the first `.git`. A directory is a
//! main checkout; a file (`gitdir: <path>`) is a linked worktree or a
//! submodule, whose git directory names the shared one in `commondir`.
//! `HEAD` gives the branch (`ref: refs/heads/<branch>`) or, detached, the
//! commit. Only `stat`s and two or three small reads: no index, so no
//! dirty marker.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use fleet_proto::GitInfo;

/// How long a cwd's answer is reused: a branch switch shows within this
/// (plus the client's own list interval).
pub const GIT_TTL: Duration = Duration::from_secs(3);
/// Most directories walked up from a cwd.
const MAX_DEPTH: usize = 64;
/// Most bytes read from `HEAD`, `.git` or `commondir`.
const MAX_FILE: u64 = 4096;
/// Cached cwds kept before stale ones are dropped.
const MAX_ENTRIES: usize = 256;
/// Hex digits shown for a detached commit.
const SHORT_HASH: usize = 7;

/// The checkout holding `cwd`, or `None` when it is in none (or `cwd` is
/// not absolute, or its `.git` cannot be read).
pub fn read_git(cwd: &Path) -> Option<GitInfo> {
    if !cwd.is_absolute() {
        return None;
    }
    let (root, dot_git) = cwd
        .ancestors()
        .take(MAX_DEPTH)
        .map(|d| (d, d.join(".git")))
        .find(|(_, g)| fs::symlink_metadata(g).is_ok())?;
    let meta = fs::metadata(&dot_git).ok()?;
    let git_dir = if meta.is_dir() {
        dot_git
    } else {
        let text = read_small(&dot_git)?;
        let target = text.trim().strip_prefix("gitdir:")?.trim();
        normalize(&root.join(target))
    };
    let common = read_small(&git_dir.join("commondir")).map(|c| normalize(&git_dir.join(c.trim())));
    let root_name = file_name(root)?;
    let (repo, worktree) = match &common {
        // A linked worktree: the repository is where the shared git
        // directory lives.
        Some(common) => (
            repo_name(common).unwrap_or_else(|| root_name.clone()),
            root_name,
        ),
        None => (root_name, String::new()),
    };
    let (branch, detached) = parse_head(&read_small(&git_dir.join("HEAD"))?)?;
    Some(GitInfo {
        repo,
        worktree,
        branch,
        detached,
    })
}

/// `HEAD`'s branch (`false`), or its commit's short hash (`true`).
fn parse_head(text: &str) -> Option<(String, bool)> {
    let text = text.trim();
    if let Some(r) = text.strip_prefix("ref:") {
        let r = r.trim();
        let branch = r
            .strip_prefix("refs/heads/")
            .or_else(|| r.strip_prefix("refs/"))
            .unwrap_or(r);
        return (!branch.is_empty()).then(|| (branch.to_owned(), false));
    }
    let hex = text.len() >= SHORT_HASH && text.bytes().all(|b| b.is_ascii_hexdigit());
    hex.then(|| (text[..SHORT_HASH].to_owned(), true))
}

/// The repository's name from its git directory: the folder holding
/// `.git`, or a bare `name.git` without its suffix.
fn repo_name(common: &Path) -> Option<String> {
    let name = file_name(common)?;
    if name == ".git" {
        return common.parent().and_then(file_name);
    }
    Some(name.strip_suffix(".git").unwrap_or(&name).to_owned())
}

fn file_name(p: &Path) -> Option<String> {
    p.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// `p` with `.` and `..` resolved by name (as git writes `commondir`).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

fn read_small(path: &Path) -> Option<String> {
    let mut text = String::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_FILE)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// [`read_git`] answers per cwd, each reused for [`GIT_TTL`].
#[derive(Default)]
pub struct GitCache {
    entries: Mutex<HashMap<String, (Instant, Option<GitInfo>)>>,
}

impl GitCache {
    /// The checkout holding `cwd`, read again once the cached answer is
    /// older than [`GIT_TTL`]. The files are read without the cache's lock.
    pub fn get(&self, cwd: &str) -> Option<GitInfo> {
        self.get_at(cwd, Instant::now())
    }

    fn get_at(&self, cwd: &str, now: Instant) -> Option<GitInfo> {
        if cwd.is_empty() {
            return None;
        }
        if let Some((at, info)) = self.lock().get(cwd)
            && now.saturating_duration_since(*at) < GIT_TTL
        {
            return info.clone();
        }
        let info = read_git(Path::new(cwd));
        let mut entries = self.lock();
        if entries.len() >= MAX_ENTRIES {
            entries.retain(|_, (at, _)| now.saturating_duration_since(*at) < GIT_TTL);
        }
        entries.insert(cwd.to_owned(), (now, info.clone()));
        info
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Instant, Option<GitInfo>)>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn info(repo: &str, worktree: &str, branch: &str, detached: bool) -> GitInfo {
        GitInfo {
            repo: repo.into(),
            worktree: worktree.into(),
            branch: branch.into(),
            detached,
        }
    }

    #[test]
    fn head_names_the_branch_or_the_detached_commit() {
        assert_eq!(
            parse_head("ref: refs/heads/fix/freeze-timeout\n"),
            Some(("fix/freeze-timeout".into(), false))
        );
        assert_eq!(
            parse_head("ref: refs/remotes/origin/main"),
            Some(("remotes/origin/main".into(), false))
        );
        assert_eq!(
            parse_head("1a4a202c0ffee00000000000000000000000000\n"),
            Some(("1a4a202".into(), true))
        );
        assert_eq!(parse_head(""), None);
        assert_eq!(parse_head("ref: "), None);
        assert_eq!(parse_head("not a hash"), None);
    }

    #[test]
    fn a_main_checkout_is_found_from_a_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("fleetd");
        write(&repo.join(".git/HEAD"), "ref: refs/heads/main\n");
        let sub = repo.join("crates/fleet-node");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(read_git(&sub), Some(info("fleetd", "", "main", false)));
        assert_eq!(read_git(&repo), Some(info("fleetd", "", "main", false)));
    }

    #[test]
    fn a_linked_worktree_names_its_repository() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("fleetd");
        write(&repo.join(".git/HEAD"), "ref: refs/heads/main\n");
        let admin = repo.join(".git/worktrees/issue-97");
        write(&admin.join("HEAD"), "ref: refs/heads/feat/gui-git-branch\n");
        write(&admin.join("commondir"), "../..\n");
        let wt = dir.path().join("fleetd-wt/issue-97");
        write(&wt.join(".git"), &format!("gitdir: {}\n", admin.display()));
        assert_eq!(
            read_git(&wt),
            Some(info("fleetd", "issue-97", "feat/gui-git-branch", false))
        );
    }

    #[test]
    fn a_relative_gitdir_and_a_bare_common_dir() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("app.git");
        let admin = bare.join("worktrees/topic");
        write(
            &admin.join("HEAD"),
            "0123456789abcdef0123456789abcdef01234567",
        );
        write(&admin.join("commondir"), "../..");
        let wt = dir.path().join("topic");
        write(&wt.join(".git"), "gitdir: ../app.git/worktrees/topic");
        assert_eq!(read_git(&wt), Some(info("app", "topic", "0123456", true)));
    }

    #[test]
    fn a_submodule_is_its_own_repository() {
        let dir = tempfile::tempdir().unwrap();
        let modules = dir.path().join("outer/.git/modules/lib");
        write(&modules.join("HEAD"), "ref: refs/heads/dev");
        let sub = dir.path().join("outer/lib");
        write(&sub.join(".git"), "gitdir: ../.git/modules/lib");
        assert_eq!(read_git(&sub), Some(info("lib", "", "dev", false)));
    }

    #[test]
    fn outside_a_checkout_or_unreadable_gives_none() {
        let dir = tempfile::tempdir().unwrap();
        // Unless the temp dir itself sits in a checkout.
        if read_git(dir.path()).is_none() {
            let plain = dir.path().join("plain");
            fs::create_dir_all(&plain).unwrap();
            assert_eq!(read_git(&plain), None);
        }
        assert_eq!(read_git(Path::new("relative/dir")), None);
        let broken = dir.path().join("broken");
        write(&broken.join(".git"), "not a gitdir line");
        assert_eq!(read_git(&broken), None);
    }

    #[test]
    fn the_cache_reuses_an_answer_until_it_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("r");
        write(&repo.join(".git/HEAD"), "ref: refs/heads/one");
        let cwd = repo.to_str().unwrap();
        let cache = GitCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.get_at(cwd, t0).unwrap().branch, "one");
        write(&repo.join(".git/HEAD"), "ref: refs/heads/two");
        assert_eq!(cache.get_at(cwd, t0 + GIT_TTL / 2).unwrap().branch, "one");
        assert_eq!(cache.get_at(cwd, t0 + GIT_TTL).unwrap().branch, "two");
        assert_eq!(cache.get_at("", t0), None);
    }
}
