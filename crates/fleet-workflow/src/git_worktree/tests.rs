//! Integration tests of the actions against real temp repositories (no
//! network: `fetch_base` fetches from a local bare `origin`), through the
//! registry, so params are validated and the host is capability-checked as
//! in the daemon.

use std::sync::Arc;

use fleet_proto::param_value::Value;
use fleet_proto::view::Kind as ViewKind;
use fleet_proto::{AgentInfo, AgentState, DiffView, FileStatus, ParamValue, RunAction, StringList, StringMap, TableView};

use super::testhost::{LocalHost, Repos, git, labels};
use super::*;
use crate::checked::CancelFlag;
use crate::registry::Registry;

fn flow_with(config: Option<String>, home: &str) -> Arc<GitWorktree> {
    let home = home.to_owned();
    Arc::new(GitWorktree::new(Arc::new(move || Settings {
        config: config.clone(),
        home: home.clone(),
    })))
}

fn flow(r: &Repos, fetch_base: bool) -> Arc<GitWorktree> {
    flow_with(Some(r.config(fetch_base)), &r.root)
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

fn argv(items: &[&str]) -> Value {
    Value::List(StringList {
        items: items.iter().map(|s| (*s).to_string()).collect(),
    })
}

async fn run_in(
    flow: &Arc<GitWorktree>,
    host: &LocalHost,
    name: &str,
    params: Vec<(&str, Value)>,
    cwd: &str,
) -> Result<ActionOutcome, ActionError> {
    let registry = Registry::new(vec![flow.clone() as Arc<dyn Workflow>]).unwrap();
    let req = RunAction {
        name: format!("{ID}.{name}"),
        params: params
            .into_iter()
            .map(|(k, v)| (k.to_string(), ParamValue { value: Some(v) }))
            .collect(),
        cwd: cwd.into(),
    };
    registry.run(req, CancelFlag::default(), |_| host).await
}

async fn run(
    flow: &Arc<GitWorktree>,
    host: &LocalHost,
    name: &str,
    params: Vec<(&str, Value)>,
) -> Result<ActionOutcome, ActionError> {
    run_in(flow, host, name, params, "/").await
}

fn spawned(o: ActionOutcome) -> (AgentInfo, String) {
    match o {
        ActionOutcome::Spawned(s) => (s.agent.unwrap(), s.message),
        other => panic!("not spawned: {other:?}"),
    }
}

fn refusal(o: ActionOutcome) -> (String, Vec<(String, Value)>) {
    match o {
        ActionOutcome::Refused(r) => {
            let mut retry: Vec<_> = r
                .retry
                .into_iter()
                .map(|(k, v)| (k, v.value.unwrap()))
                .collect();
            retry.sort_by(|a, b| a.0.cmp(&b.0));
            (r.reason, retry)
        }
        other => panic!("not refused: {other:?}"),
    }
}

fn exists(p: &str) -> bool {
    std::path::Path::new(p).exists()
}

fn has_branch(main: &str, b: &str) -> bool {
    !git(main, &["branch", "--list", b]).trim().is_empty()
}

fn new_params<'a>(r: &Repos, slug: &'a str) -> Vec<(&'a str, Value)> {
    vec![
        ("slug", s(slug)),
        ("command", argv(&["claude", "--model", "x"])),
        ("repo", s(&r.main)),
    ]
}

// ---- new-agent ----

#[tokio::test]
async fn new_agent_makes_a_worktree_on_a_new_branch_and_starts_the_agent_in_it() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let mut params = new_params(&r, "issue-1");
    params.push(("name", s("kid")));
    params.push((
        "labels",
        Value::Map(StringMap {
            entries: labels(&[("role", "rev")]),
        }),
    ));
    let (agent, message) = spawned(run(&f, &host, "new-agent", params).await.unwrap());
    let wt = format!("{}/wt/repo/issue-1", r.root);
    assert_eq!(agent.cwd, wt);
    assert_eq!(agent.program, "claude");
    assert_eq!(agent.args, ["--model", "x"]);
    assert_eq!(agent.labels.get("name").map(String::as_str), Some("kid"));
    assert_eq!(agent.labels.get("role").map(String::as_str), Some("rev"));
    assert_eq!(agent.labels[LABEL_REPO], r.main);
    assert_eq!(agent.labels[LABEL_WORKTREE], wt);
    assert_eq!(agent.labels[LABEL_BASE], "origin/main");
    assert_eq!(git(&wt, &["symbolic-ref", "--short", "HEAD"]).trim(), "fleet/issue-1");
    assert!(message.contains(&wt) && message.contains("fleet/issue-1"), "{message}");
    // Not tracking the base: `git push` never goes to main by accident.
    assert!(!git(&wt, &["config", "--list"]).contains("branch.fleet/issue-1.merge"));
    let rec = get_record(&host, &wt).await.unwrap().unwrap();
    assert_eq!(rec.repo, r.main);
    assert_eq!(rec.branch, "fleet/issue-1");
    assert_eq!(rec.base, "origin/main");
    assert_eq!(rec.created_by, "user");
    assert_eq!(rec.agent, Some(agent.id));
    // Every git call was safe for a daemon to run.
    for argv in host.execs.lock().unwrap().iter() {
        assert_eq!(argv[..3], ["git", "-c", "core.quotepath=off"], "{argv:?}");
    }
}

#[tokio::test]
async fn new_agent_uses_the_given_branch_and_base_and_the_caller_cwd_as_repo() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let params = vec![
        ("slug", s("x")),
        ("command", argv(&["sh"])),
        ("branch", s("feat/x")),
        ("base", s("main")),
    ];
    let (agent, _) = spawned(run_in(&f, &host, "new-agent", params, &r.main).await.unwrap());
    assert_eq!(git(&agent.cwd, &["symbolic-ref", "--short", "HEAD"]).trim(), "feat/x");
    assert_eq!(agent.labels[LABEL_BASE], "main");
}

#[tokio::test]
async fn a_relative_root_is_relative_to_the_main_checkout() {
    let r = Repos::new();
    let config = "[workflows.git-worktree]\nroot = \"../{repo}-wt\"\nbranch_prefix = \"\"\nfetch_base = false\n";
    let f = flow_with(Some(config.into()), "/nonexistent-home");
    let host = LocalHost::new(Caller::User);
    let (agent, _) = spawned(run(&f, &host, "new-agent", new_params(&r, "s1")).await.unwrap());
    assert_eq!(agent.cwd, format!("{}/repo-wt/s1", r.root));
    assert!(has_branch(&r.main, "s1"));
}

#[tokio::test]
async fn bad_slugs_are_refused_before_anything_runs() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let long = "x".repeat(65);
    for slug in ["", ".", "..", "a/b", "a b", "../up", long.as_str()] {
        let e = run(&f, &host, "new-agent", new_params(&r, slug)).await.unwrap_err();
        assert!(matches!(e, ActionError::Invalid(_)), "{slug:?}: {e}");
    }
    assert!(host.execs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_quota_refusal_leaves_nothing_behind() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    *host.refuse_check.lock().unwrap() = Some(ActionError::forbidden("max_children"));
    let e = run(&f, &host, "new-agent", new_params(&r, "q")).await.unwrap_err();
    assert!(e.to_string().contains("max_children"), "{e}");
    assert!(!exists(&format!("{}/wt/repo/q", r.root)));
    assert!(!has_branch(&r.main, "fleet/q"));
}

#[tokio::test]
async fn a_refused_spawn_removes_the_worktree_and_branch_it_made() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    *host.refuse_spawn.lock().unwrap() = Some(ActionError::forbidden("the spawn was not approved"));
    let e = run(&f, &host, "new-agent", new_params(&r, "denied")).await.unwrap_err();
    assert!(e.to_string().starts_with("forbidden:"), "{e}");
    let wt = format!("{}/wt/repo/denied", r.root);
    assert!(!exists(&wt));
    assert!(!has_branch(&r.main, "fleet/denied"));
    assert_eq!(get_record(&host, &wt).await.unwrap(), None);
    assert!(!git(&r.main, &["worktree", "list"]).contains("denied"));
}

#[tokio::test]
async fn an_existing_worktree_is_refused_then_reused_then_shared() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let (first, _) = spawned(run(&f, &host, "new-agent", new_params(&r, "again")).await.unwrap());
    let (reason, retry) = refusal(run(&f, &host, "new-agent", new_params(&r, "again")).await.unwrap());
    assert!(reason.contains("already"), "{reason}");
    assert_eq!(retry, vec![("reuse".to_string(), Value::Bool(true))]);

    // The first agent still runs there: reuse needs `shared`.
    let mut reuse = new_params(&r, "again");
    reuse.push(("reuse", Value::Bool(true)));
    let (reason, retry) = refusal(run(&f, &host, "new-agent", reuse.clone()).await.unwrap());
    assert!(reason.contains("another agent"), "{reason}");
    assert_eq!(
        retry,
        vec![
            ("reuse".to_string(), Value::Bool(true)),
            ("shared".to_string(), Value::Bool(true))
        ]
    );
    let mut shared = reuse.clone();
    shared.push(("shared", Value::Bool(true)));
    let (second, _) = spawned(run(&f, &host, "new-agent", shared).await.unwrap());
    assert_eq!(second.cwd, first.cwd);
    assert_eq!(second.labels[LABEL_BASE], "origin/main");

    // Once the agents ended, reuse alone is enough (resuming an agent).
    host.set_state(first.id, AgentState::Exited);
    host.set_state(second.id, AgentState::Exited);
    let (third, _) = spawned(run(&f, &host, "new-agent", reuse).await.unwrap());
    assert_eq!(third.cwd, first.cwd);
}

#[tokio::test]
async fn a_branch_that_exists_is_an_error_naming_it() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    git(&r.main, &["branch", "fleet/taken"]);
    let e = run(&f, &host, "new-agent", new_params(&r, "taken")).await.unwrap_err();
    assert!(e.to_string().contains("fleet/taken"), "{e}");
    assert!(!exists(&format!("{}/wt/repo/taken", r.root)));
}

#[tokio::test]
async fn fetch_base_branches_from_the_fetched_remote_branch() {
    let r = Repos::new();
    // Someone else pushes to origin.
    let other = format!("{}/other", r.root);
    git(&r.root, &["clone", "-q", &format!("{}/origin.git", r.root), "other"]);
    std::fs::write(format!("{other}/b.txt"), "b\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-qm", "upstream"]);
    git(&other, &["push", "-q", "origin", "main"]);
    let upstream = git(&other, &["rev-parse", "HEAD"]);

    let f = flow(&r, true);
    let host = LocalHost::new(Caller::User);
    let (agent, message) = spawned(run(&f, &host, "new-agent", new_params(&r, "fresh")).await.unwrap());
    assert_eq!(git(&agent.cwd, &["rev-parse", "HEAD"]), upstream);
    assert!(!message.contains("fetch"), "{message}");
}

#[tokio::test]
async fn a_failed_fetch_falls_back_to_the_local_ref_with_a_note() {
    let r = Repos::new();
    git(&r.main, &["remote", "set-url", "origin", &format!("{}/gone.git", r.root)]);
    let f = flow(&r, true);
    let host = LocalHost::new(Caller::User);
    let (agent, message) = spawned(run(&f, &host, "new-agent", new_params(&r, "offline")).await.unwrap());
    assert!(message.contains("fetch") && message.contains("local"), "{message}");
    assert_eq!(
        git(&agent.cwd, &["rev-parse", "HEAD"]),
        git(&r.main, &["rev-parse", "origin/main"])
    );
}

#[tokio::test]
async fn an_agents_worktree_records_it_as_the_maker() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::Agent(7));
    let (agent, _) = spawned(run(&f, &host, "new-agent", new_params(&r, "child")).await.unwrap());
    let rec = get_record(&host, &agent.cwd).await.unwrap().unwrap();
    assert_eq!(rec.created_by, "agent:7");
}

// ---- diff ----

fn diff_view(o: ActionOutcome) -> DiffView {
    match o {
        ActionOutcome::View(v) => match v.kind {
            Some(ViewKind::Diff(d)) => d,
            other => panic!("not a diff: {other:?}"),
        },
        other => panic!("not a view: {other:?}"),
    }
}

fn paths(d: &DiffView) -> Vec<String> {
    d.files
        .iter()
        .map(|f| if f.new_path.is_empty() { f.old_path.clone() } else { f.new_path.clone() })
        .collect()
}

fn find<'a>(d: &'a DiffView, path: &str) -> &'a fleet_proto::FileDiff {
    d.files
        .iter()
        .find(|f| f.new_path == path || f.old_path == path)
        .unwrap_or_else(|| panic!("no {path} in {:?}", paths(d)))
}

/// A worktree made by new-agent with a commit (`c.txt`), an uncommitted
/// change (`a.txt`), untracked text and binary files, and an ignored one.
async fn worked(r: &Repos, f: &Arc<GitWorktree>, host: &LocalHost) -> AgentInfo {
    let (agent, _) = spawned(run(f, host, "new-agent", new_params(r, "work")).await.unwrap());
    let wt = agent.cwd.clone();
    r.write(&format!("{wt}/c.txt"), "committed\n");
    git(&wt, &["add", "c.txt"]);
    git(&wt, &["commit", "-qm", "c"]);
    r.write(&format!("{wt}/a.txt"), "one\nTWO\nthree\n");
    r.write(&format!("{wt}/u.txt"), "untracked\nfile");
    std::fs::write(format!("{wt}/b.bin"), b"\0\x01\x02").unwrap();
    r.write(&format!("{wt}/target/debug/x"), "ignored\n");
    agent
}

#[tokio::test]
async fn diff_against_base_is_everything_the_agent_did() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let agent = worked(&r, &f, &host).await;
    let d = diff_view(run(&f, &host, "diff", vec![("agent", Value::Int(agent.id as i64))]).await.unwrap());
    let mut got = paths(&d);
    got.sort();
    assert_eq!(got, ["a.txt", "b.bin", "c.txt", "u.txt"]);
    assert_eq!(d.title, "work · fleet/work");
    assert!(d.from_label.starts_with("origin/main (merge base "), "{}", d.from_label);
    assert_eq!(d.to_label, "working tree");
    assert_eq!(find(&d, "c.txt").status(), FileStatus::FileAdded);
    assert_eq!(find(&d, "a.txt").status(), FileStatus::FileModified);
    let u = find(&d, "u.txt");
    assert_eq!(u.status(), FileStatus::FileAdded);
    assert_eq!(u.hunks[0].body, b"+untracked\n+file\n\\ No newline at end of file".to_vec());
    assert!(find(&d, "b.bin").binary);
    assert_eq!(d.files_changed, 4);
    assert_eq!((d.additions, d.deletions), (1 + 1 + 2, 1));
    assert!(!d.truncated);
}

#[tokio::test]
async fn diff_uncommitted_and_committed() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let agent = worked(&r, &f, &host).await;
    let id = Value::Int(agent.id as i64);
    let d = diff_view(
        run(&f, &host, "diff", vec![("agent", id.clone()), ("against", s("uncommitted"))]).await.unwrap(),
    );
    let mut got = paths(&d);
    got.sort();
    assert_eq!(got, ["a.txt", "b.bin", "u.txt"]);
    assert!(d.from_label.starts_with("HEAD ("), "{}", d.from_label);
    let d = diff_view(
        run(&f, &host, "diff", vec![("agent", id), ("against", s("committed"))]).await.unwrap(),
    );
    assert_eq!(paths(&d), ["c.txt"]);
    assert_eq!(d.to_label, "HEAD");
}

#[tokio::test]
async fn diff_by_path_one_file_and_context() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let agent = worked(&r, &f, &host).await;
    let sub = format!("{}/target", agent.cwd);
    let path = Value::String(sub);
    let d = diff_view(run(&f, &host, "diff", vec![("path", path.clone()), ("file", s("a.txt"))]).await.unwrap());
    assert_eq!(paths(&d), ["a.txt"]);
    assert_eq!(
        String::from_utf8_lossy(&d.files[0].hunks[0].body),
        " one\n-two\n+TWO\n three"
    );
    let d = diff_view(
        run(&f, &host, "diff", vec![("path", path.clone()), ("file", s("a.txt")), ("context", Value::Int(0))])
            .await
            .unwrap(),
    );
    assert_eq!(String::from_utf8_lossy(&d.files[0].hunks[0].body), "-two\n+TWO");
    let d = diff_view(run(&f, &host, "diff", vec![("path", path), ("file", s("u.txt"))]).await.unwrap());
    assert_eq!(paths(&d), ["u.txt"]);
}

#[tokio::test]
async fn a_big_diff_is_capped_and_one_file_can_be_asked_for() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let (agent, _) = spawned(run(&f, &host, "new-agent", new_params(&r, "big")).await.unwrap());
    let wt = agent.cwd.clone();
    let big: String = (0..60_000).map(|i| format!("line {i}\n")).collect();
    r.write(&format!("{wt}/big.txt"), &big);
    r.write(&format!("{wt}/small.txt"), "s\n");
    let id = Value::Int(agent.id as i64);
    let d = diff_view(run(&f, &host, "diff", vec![("agent", id.clone())]).await.unwrap());
    assert!(d.truncated);
    let b = find(&d, "big.txt");
    assert!(b.hunks_omitted && b.hunks.is_empty());
    assert_eq!(b.additions, 60_000);
    let d = diff_view(run(&f, &host, "diff", vec![("agent", id), ("file", s("big.txt"))]).await.unwrap());
    assert!(!d.truncated);
    assert_eq!(d.files[0].hunks[0].new_lines, 60_000);
}

#[tokio::test]
async fn diff_of_an_agent_without_labels_uses_its_cwd_and_the_default_branch() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    git(&r.main, &["worktree", "add", "-q", "-b", "hand", &format!("{}/hand", r.root), "main"]);
    let wt = format!("{}/hand", r.root);
    r.write(&format!("{wt}/h.txt"), "h\n");
    let id = host.add_agent(&format!("{wt}"), &[], true);
    let d = diff_view(run(&f, &host, "diff", vec![("agent", Value::Int(id as i64))]).await.unwrap());
    assert_eq!(paths(&d), ["h.txt"]);
    assert!(d.from_label.starts_with("origin/main"), "{}", d.from_label);
}

#[tokio::test]
async fn diff_needs_an_agent_or_a_path() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let e = run(&f, &host, "diff", vec![]).await.unwrap_err();
    assert!(matches!(e, ActionError::Invalid(_)), "{e}");
}
