//! `git-worktree.new-agent` (M4 spec §3): a worktree on a new branch, then
//! an agent in it. The quota is checked before anything is made, and a
//! refused spawn takes the new worktree and branch back.

use fleet_proto::SpawnAgent;
use fleet_proto::param_value::Value;

use crate::error::ActionError;
use crate::host::{HostResult, Labels};
use crate::registry::{ActionCall, ActionOutcome};

use super::config::resolve_root;
use super::git::{FETCH_TIMEOUT_MS, Git, Mode, failure, request};
use super::parse::parse_worktree_list;
use super::{
    GitWorktree, LABEL_BASE, LABEL_REPO, LABEL_WORKTREE, Record, Repo, agents_in, caller_tag,
    check_ref, default_base, delete_record, get_record, now_ms, put_record, refused,
    repo_param_or_cwd, running,
};

/// `[A-Za-z0-9._-]`, at most 64, not `.` or `..`.
pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug != "."
        && slug != ".."
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub async fn run(
    flow: &GitWorktree,
    git: &Git<'_>,
    call: &ActionCall,
) -> Result<ActionOutcome, ActionError> {
    let slug = call.params.string("slug").unwrap_or_default();
    if !valid_slug(slug) {
        return Err(ActionError::invalid(format!(
            "slug {slug:?}: letters, digits, `.`, `_` and `-`, at most 64, not `.` or `..`"
        )));
    }
    let command = call.params.list("command").unwrap_or_default();
    let Some((program, args)) = command.split_first() else {
        return Err(ActionError::invalid("the command is empty"));
    };
    // Caller-named refs are checked before any git runs (they must never
    // reach git as options).
    let given_base = call.params.string("base").filter(|b| !b.is_empty());
    let given_branch = call.params.string("branch").filter(|b| !b.is_empty());
    if let Some(b) = given_base {
        check_ref("base", b)?;
    }
    if let Some(b) = given_branch {
        check_ref("branch", b)?;
    }
    let repo = repo_param_or_cwd(git, call).await?;
    let (config, home) = flow.config(&repo.name)?;
    let root = resolve_root(&config.root, &repo.name, &repo.main, &home);
    let path = format!("{root}/{slug}");

    // Before any side effect: a quota refusal leaves nothing behind.
    git.host.check_spawn().await?;

    let worktrees = parse_worktree_list(
        git.ok(
            &repo.main,
            &["worktree", "list", "--porcelain", "-z"],
            Mode::Read,
        )
        .await?
        .as_bytes(),
    );
    let reuse = call.params.bool("reuse").unwrap_or(false);
    if worktrees.iter().any(|w| w.path == path) {
        if !reuse {
            return Ok(refused(
                format!("{path} is already a worktree of {}", repo.name),
                &[("reuse", Value::Bool(true))],
            ));
        }
        return reuse_worktree(git, call, &repo, &path, program, args).await;
    }

    let mut notes = Vec::new();
    let base = match given_base {
        Some(b) => b.to_owned(),
        None => default_base(git, &repo.main).await?,
    };
    check_ref("base", &base)?;
    let branch = match given_branch {
        Some(b) => b.to_owned(),
        None => format!("{}{slug}", config.branch_prefix),
    };
    check_ref("branch", &branch)?;
    if !git
        .test(&repo.main, &["check-ref-format", "--branch", &branch])
        .await?
    {
        return Err(ActionError::invalid(format!(
            "branch {branch:?} is not a valid branch name"
        )));
    }
    if config.fetch_base
        && let Some(note) = fetch_base(git, &repo.main, &base).await?
    {
        notes.push(note);
    }
    let commit = format!("{base}^{{commit}}");
    if !git
        .test(
            &repo.main,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &commit,
            ],
        )
        .await?
    {
        return Err(ActionError::invalid(format!(
            "no base {base:?} in {}",
            repo.main
        )));
    }
    let head = format!("refs/heads/{branch}");
    if git
        .test(&repo.main, &["show-ref", "--verify", "--quiet", &head])
        .await?
    {
        let at = worktrees
            .iter()
            .find(|w| w.branch.as_deref() == Some(branch.as_str()))
            .map_or_else(
                || " (checked out nowhere)".to_string(),
                |w| format!(" (checked out in {})", w.path),
            );
        return Err(ActionError::failed(format!(
            "branch {branch} already exists{at}; give another `branch` or slug"
        )));
    }

    let add = [
        "worktree",
        "add",
        "--no-track",
        "-b",
        &branch,
        "--end-of-options",
        &path,
        &base,
    ];
    let out = git.run(&repo.main, &add, Mode::Write).await?;
    if out.exit_code != Some(0) {
        let said = failure(&add, &out);
        if said.contains("already exists") || said.contains("lock") {
            return Err(ActionError::failed(format!("slug {slug} in use: {said}")));
        }
        return Err(ActionError::failed(said));
    }

    let mut record = Record {
        repo: repo.main.clone(),
        branch: branch.clone(),
        base: base.clone(),
        created_ms: now_ms(),
        created_by: caller_tag(git.host.caller()),
        agent: None,
    };
    let spawned = async {
        put_record(git.host, &path, &record).await?;
        let agent = git
            .host
            .spawn(spawn_request(call, &repo, &path, &base, program, args))
            .await?;
        record.agent = Some(agent.id);
        // Best effort: the record is only a pointer for the GUI.
        let _ = put_record(git.host, &path, &record).await;
        Ok::<_, ActionError>(agent)
    }
    .await;
    let agent = match spawned {
        Ok(a) => a,
        Err(e) => {
            rollback(git, &repo.main, &path, &branch).await;
            return Err(e);
        }
    };
    let mut message = format!("worktree {path} on {branch} from {base}");
    for n in notes {
        message.push_str("; ");
        message.push_str(&n);
    }
    Ok(ActionOutcome::Spawned(Box::new(
        fleet_proto::ActionSpawned {
            agent: Some(agent),
            message,
        },
    )))
}

/// Starts the agent in an existing worktree of the repository (`reuse`):
/// refused while another agent runs there, unless `shared`.
async fn reuse_worktree(
    git: &Git<'_>,
    call: &ActionCall,
    repo: &Repo,
    path: &str,
    program: &str,
    args: &[String],
) -> Result<ActionOutcome, ActionError> {
    let agents = git.host.list_agents(Labels::new()).await?;
    let busy: Vec<_> = agents_in(&agents, path)
        .into_iter()
        .filter(|a| running(a))
        .map(super::agent_name)
        .collect();
    if !busy.is_empty() && !call.params.bool("shared").unwrap_or(false) {
        return Ok(refused(
            format!(
                "another agent is working here ({}): {path}",
                busy.join(", ")
            ),
            &[("reuse", Value::Bool(true)), ("shared", Value::Bool(true))],
        ));
    }
    let base = match get_record(git.host, path).await? {
        Some(r) if !r.base.is_empty() => r.base,
        _ => match call.params.string("base").filter(|b| !b.is_empty()) {
            Some(b) => b.to_owned(),
            None => default_base(git, &repo.main).await?,
        },
    };
    check_ref("base", &base)?;
    let agent = git
        .host
        .spawn(spawn_request(call, repo, path, &base, program, args))
        .await?;
    Ok(ActionOutcome::Spawned(Box::new(
        fleet_proto::ActionSpawned {
            agent: Some(agent),
            message: format!("reused worktree {path}"),
        },
    )))
}

fn spawn_request(
    call: &ActionCall,
    repo: &Repo,
    path: &str,
    base: &str,
    program: &str,
    args: &[String],
) -> SpawnAgent {
    let mut labels = call.params.map("labels").cloned().unwrap_or_default();
    if let Some(name) = call.params.string("name").filter(|n| !n.is_empty()) {
        labels.insert("name".into(), name.to_owned());
    }
    labels.insert(LABEL_REPO.into(), repo.main.clone());
    labels.insert(LABEL_WORKTREE.into(), path.to_owned());
    labels.insert(LABEL_BASE.into(), base.to_owned());
    SpawnAgent {
        program: program.to_owned(),
        args: args.to_vec(),
        cwd: path.to_owned(),
        labels,
        ..Default::default()
    }
}

/// With `fetch_base`, fetches a remote-tracking base (`origin/main`) from
/// its remote; a note when that fails (the local ref is used).
pub(super) async fn fetch_base(
    git: &Git<'_>,
    main: &str,
    base: &str,
) -> HostResult<Option<String>> {
    let remotes = git.ok(main, &["remote"], Mode::Read).await?;
    let Some((remote, branch)) = remotes.lines().find_map(|r| {
        base.strip_prefix(r)
            .and_then(|b| b.strip_prefix('/'))
            .filter(|b| !b.is_empty())
            .map(|b| (r, b))
    }) else {
        return Ok(None);
    };
    check_ref("base", base)?;
    let args = [
        "fetch",
        "--quiet",
        "--no-tags",
        "--end-of-options",
        remote,
        branch,
    ];
    let out = git
        .host
        .exec(request(main, &args, Mode::Write, FETCH_TIMEOUT_MS))
        .await?;
    if out.exit_code == Some(0) {
        return Ok(None);
    }
    Ok(Some(format!(
        "fetching {base} failed, so the local {base} was used ({})",
        failure(&args, &out)
    )))
}

/// Takes back a worktree and branch just made (they hold no work yet).
/// Best effort: what fails is left for `git worktree prune`.
async fn rollback(git: &Git<'_>, main: &str, path: &str, branch: &str) {
    let _ = git
        .run(
            main,
            &["worktree", "remove", "--force", "--end-of-options", path],
            Mode::Write,
        )
        .await;
    let _ = git
        .run(
            main,
            &["branch", "-D", "--end-of-options", branch],
            Mode::Write,
        )
        .await;
    let _ = delete_record(git.host, path).await;
}
