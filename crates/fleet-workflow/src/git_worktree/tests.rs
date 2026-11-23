//! Integration tests of the actions against real temp repositories (no
//! network: `fetch_base` fetches from a local bare `origin`), through the
//! registry, so params are validated and the host is capability-checked as
//! in the daemon.

use std::sync::Arc;

use fleet_proto::param_value::Value;
use fleet_proto::view::Kind as ViewKind;
use fleet_proto::{
    AgentInfo, AgentState, DiffView, FileStatus, ParamValue, RunAction, StringList, StringMap,
    TableView,
};

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
    assert_eq!(
        git(&wt, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "fleet/issue-1"
    );
    assert!(
        message.contains(&wt) && message.contains("fleet/issue-1"),
        "{message}"
    );
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
    let (agent, _) = spawned(
        run_in(&f, &host, "new-agent", params, &r.main)
            .await
            .unwrap(),
    );
    assert_eq!(
        git(&agent.cwd, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "feat/x"
    );
    assert_eq!(agent.labels[LABEL_BASE], "main");
}

#[tokio::test]
async fn a_relative_root_is_relative_to_the_main_checkout() {
    let r = Repos::new();
    let config = "[workflows.git-worktree]\nroot = \"../{repo}-wt\"\nbranch_prefix = \"\"\nfetch_base = false\n";
    let f = flow_with(Some(config.into()), "/nonexistent-home");
    let host = LocalHost::new(Caller::User);
    let (agent, _) = spawned(
        run(&f, &host, "new-agent", new_params(&r, "s1"))
            .await
            .unwrap(),
    );
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
        let e = run(&f, &host, "new-agent", new_params(&r, slug))
            .await
            .unwrap_err();
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
    let e = run(&f, &host, "new-agent", new_params(&r, "q"))
        .await
        .unwrap_err();
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
    let e = run(&f, &host, "new-agent", new_params(&r, "denied"))
        .await
        .unwrap_err();
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
    let (first, _) = spawned(
        run(&f, &host, "new-agent", new_params(&r, "again"))
            .await
            .unwrap(),
    );
    let (reason, retry) = refusal(
        run(&f, &host, "new-agent", new_params(&r, "again"))
            .await
            .unwrap(),
    );
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
    let e = run(&f, &host, "new-agent", new_params(&r, "taken"))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("fleet/taken"), "{e}");
    assert!(!exists(&format!("{}/wt/repo/taken", r.root)));
}

#[tokio::test]
async fn fetch_base_branches_from_the_fetched_remote_branch() {
    let r = Repos::new();
    // Someone else pushes to origin.
    let other = format!("{}/other", r.root);
    git(
        &r.root,
        &["clone", "-q", &format!("{}/origin.git", r.root), "other"],
    );
    std::fs::write(format!("{other}/b.txt"), "b\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-qm", "upstream"]);
    git(&other, &["push", "-q", "origin", "main"]);
    let upstream = git(&other, &["rev-parse", "HEAD"]);

    let f = flow(&r, true);
    let host = LocalHost::new(Caller::User);
    let (agent, message) = spawned(
        run(&f, &host, "new-agent", new_params(&r, "fresh"))
            .await
            .unwrap(),
    );
    assert_eq!(git(&agent.cwd, &["rev-parse", "HEAD"]), upstream);
    assert!(!message.contains("fetch"), "{message}");
}

#[tokio::test]
async fn a_failed_fetch_falls_back_to_the_local_ref_with_a_note() {
    let r = Repos::new();
    git(
        &r.main,
        &[
            "remote",
            "set-url",
            "origin",
            &format!("{}/gone.git", r.root),
        ],
    );
    let f = flow(&r, true);
    let host = LocalHost::new(Caller::User);
    let (agent, message) = spawned(
        run(&f, &host, "new-agent", new_params(&r, "offline"))
            .await
            .unwrap(),
    );
    assert!(
        message.contains("fetch") && message.contains("local"),
        "{message}"
    );
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
    let (agent, _) = spawned(
        run(&f, &host, "new-agent", new_params(&r, "child"))
            .await
            .unwrap(),
    );
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
        .map(|f| {
            if f.new_path.is_empty() {
                f.old_path.clone()
            } else {
                f.new_path.clone()
            }
        })
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
    let (agent, _) = spawned(
        run(f, host, "new-agent", new_params(r, "work"))
            .await
            .unwrap(),
    );
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
    let d = diff_view(
        run(
            &f,
            &host,
            "diff",
            vec![("agent", Value::Int(agent.id as i64))],
        )
        .await
        .unwrap(),
    );
    let mut got = paths(&d);
    got.sort();
    assert_eq!(got, ["a.txt", "b.bin", "c.txt", "u.txt"]);
    assert_eq!(d.title, "work · fleet/work");
    assert!(
        d.from_label.starts_with("origin/main (merge base "),
        "{}",
        d.from_label
    );
    assert_eq!(d.to_label, "working tree");
    assert_eq!(find(&d, "c.txt").status(), FileStatus::FileAdded);
    assert_eq!(find(&d, "a.txt").status(), FileStatus::FileModified);
    let u = find(&d, "u.txt");
    assert_eq!(u.status(), FileStatus::FileAdded);
    assert_eq!(
        u.hunks[0].body,
        b"+untracked\n+file\n\\ No newline at end of file".to_vec()
    );
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
        run(
            &f,
            &host,
            "diff",
            vec![("agent", id.clone()), ("against", s("uncommitted"))],
        )
        .await
        .unwrap(),
    );
    let mut got = paths(&d);
    got.sort();
    assert_eq!(got, ["a.txt", "b.bin", "u.txt"]);
    assert!(d.from_label.starts_with("HEAD ("), "{}", d.from_label);
    let d = diff_view(
        run(
            &f,
            &host,
            "diff",
            vec![("agent", id), ("against", s("committed"))],
        )
        .await
        .unwrap(),
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
    let d = diff_view(
        run(
            &f,
            &host,
            "diff",
            vec![("path", path.clone()), ("file", s("a.txt"))],
        )
        .await
        .unwrap(),
    );
    assert_eq!(paths(&d), ["a.txt"]);
    assert_eq!(
        String::from_utf8_lossy(&d.files[0].hunks[0].body),
        " one\n-two\n+TWO\n three"
    );
    let d = diff_view(
        run(
            &f,
            &host,
            "diff",
            vec![
                ("path", path.clone()),
                ("file", s("a.txt")),
                ("context", Value::Int(0)),
            ],
        )
        .await
        .unwrap(),
    );
    assert_eq!(
        String::from_utf8_lossy(&d.files[0].hunks[0].body),
        "-two\n+TWO"
    );
    let d = diff_view(
        run(
            &f,
            &host,
            "diff",
            vec![("path", path), ("file", s("u.txt"))],
        )
        .await
        .unwrap(),
    );
    assert_eq!(paths(&d), ["u.txt"]);
}

#[tokio::test]
async fn a_big_diff_is_capped_and_one_file_can_be_asked_for() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let (agent, _) = spawned(
        run(&f, &host, "new-agent", new_params(&r, "big"))
            .await
            .unwrap(),
    );
    let wt = agent.cwd.clone();
    let big: String = (0..60_000).map(|i| format!("line {i}\n")).collect();
    r.write(&format!("{wt}/big.txt"), &big);
    r.write(&format!("{wt}/small.txt"), "s\n");
    let id = Value::Int(agent.id as i64);
    let d = diff_view(
        run(&f, &host, "diff", vec![("agent", id.clone())])
            .await
            .unwrap(),
    );
    assert!(d.truncated);
    let b = find(&d, "big.txt");
    assert!(b.hunks_omitted && b.hunks.is_empty());
    assert_eq!(b.additions, 60_000);
    let d = diff_view(
        run(
            &f,
            &host,
            "diff",
            vec![("agent", id), ("file", s("big.txt"))],
        )
        .await
        .unwrap(),
    );
    assert!(!d.truncated);
    assert_eq!(d.files[0].hunks[0].new_lines, 60_000);
}

#[tokio::test]
async fn diff_of_an_agent_without_labels_uses_its_cwd_and_the_default_branch() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    git(
        &r.main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "hand",
            &format!("{}/hand", r.root),
            "main",
        ],
    );
    let wt = format!("{}/hand", r.root);
    r.write(&format!("{wt}/h.txt"), "h\n");
    let id = host.add_agent(&wt, &[], true);
    let d = diff_view(
        run(&f, &host, "diff", vec![("agent", Value::Int(id as i64))])
            .await
            .unwrap(),
    );
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

// ---- archive ----

fn done_message(o: ActionOutcome) -> String {
    match o {
        ActionOutcome::Done(d) => d.message,
        other => panic!("not done: {other:?}"),
    }
}

/// A new agent's worktree, the agent exited.
async fn ended(r: &Repos, f: &Arc<GitWorktree>, host: &LocalHost, slug: &str) -> AgentInfo {
    let (agent, _) = spawned(
        run(f, host, "new-agent", new_params(r, slug))
            .await
            .unwrap(),
    );
    host.set_state(agent.id, AgentState::Exited);
    agent
}

fn by_agent(a: &AgentInfo) -> Vec<(&'static str, Value)> {
    vec![("agent", Value::Int(a.id as i64))]
}

#[tokio::test]
async fn archive_removes_a_clean_merged_worktree_its_branch_and_its_ended_agents() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let a = ended(&r, &f, &host, "done").await;
    // A build's ignored output goes with the worktree.
    r.write(&format!("{}/target/debug/big", a.cwd), "build\n");
    let msg = done_message(run(&f, &host, "archive", by_agent(&a)).await.unwrap());
    assert!(!exists(&a.cwd));
    assert!(!has_branch(&r.main, "fleet/done"), "{msg}");
    assert_eq!(get_record(&host, &a.cwd).await.unwrap(), None);
    assert!(
        host.agents.lock().unwrap().is_empty(),
        "the ended agent is forgotten"
    );
    assert!(msg.contains("fleet/done"), "{msg}");
}

#[tokio::test]
async fn archive_is_always_refused_with_a_running_agent_inside() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let (a, _) = spawned(
        run(&f, &host, "new-agent", new_params(&r, "busy"))
            .await
            .unwrap(),
    );
    for force in [false, true] {
        let mut p = by_agent(&a);
        p.push(("force", Value::Bool(force)));
        let (reason, retry) = refusal(run(&f, &host, "archive", p).await.unwrap());
        assert!(reason.contains("running"), "{reason}");
        assert!(retry.is_empty(), "no override: {retry:?}");
    }
    assert!(exists(&a.cwd));
}

#[tokio::test]
async fn archive_refuses_uncommitted_and_untracked_work_unless_forced() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let a = ended(&r, &f, &host, "dirty").await;
    r.write(&format!("{}/a.txt", a.cwd), "changed\n");
    let (reason, retry) = refusal(run(&f, &host, "archive", by_agent(&a)).await.unwrap());
    assert!(reason.contains("uncommitted"), "{reason}");
    assert_eq!(retry, vec![("force".to_string(), Value::Bool(true))]);
    git(&a.cwd, &["checkout", "--", "a.txt"]);
    r.write(&format!("{}/new.txt", a.cwd), "n\n");
    let (reason, _) = refusal(run(&f, &host, "archive", by_agent(&a)).await.unwrap());
    assert!(
        reason.contains("untracked") || reason.contains("uncommitted"),
        "{reason}"
    );
    // Ignored files do not count.
    std::fs::remove_file(format!("{}/new.txt", a.cwd)).unwrap();
    r.write(&format!("{}/target/x", a.cwd), "build\n");
    let mut forced = by_agent(&a);
    forced.push(("force", Value::Bool(true)));
    r.write(&format!("{}/a.txt", a.cwd), "changed again\n");
    done_message(run(&f, &host, "archive", forced).await.unwrap());
    assert!(!exists(&a.cwd));
    assert!(!has_branch(&r.main, "fleet/dirty"));
}

#[tokio::test]
async fn archive_refuses_commits_on_no_remote_and_not_merged() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let a = ended(&r, &f, &host, "local").await;
    r.write(&format!("{}/l.txt", a.cwd), "l\n");
    git(&a.cwd, &["add", "-A"]);
    git(&a.cwd, &["commit", "-qm", "local work"]);
    let (reason, retry) = refusal(run(&f, &host, "archive", by_agent(&a)).await.unwrap());
    assert!(reason.contains("commit"), "{reason}");
    assert_eq!(retry, vec![("force".to_string(), Value::Bool(true))]);
    let mut p = by_agent(&a);
    p.push(("force", Value::Bool(true)));
    p.push(("keep_branch", Value::Bool(true)));
    done_message(run(&f, &host, "archive", p).await.unwrap());
    assert!(!exists(&a.cwd));
    assert!(has_branch(&r.main, "fleet/local"), "keep_branch keeps it");
}

#[tokio::test]
async fn archive_keeps_a_pushed_unmerged_branch_and_deletes_a_merged_one() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let pushed = ended(&r, &f, &host, "pushed").await;
    r.write(&format!("{}/p.txt", pushed.cwd), "p\n");
    git(&pushed.cwd, &["add", "-A"]);
    git(&pushed.cwd, &["commit", "-qm", "pushed work"]);
    git(&pushed.cwd, &["push", "-q", "origin", "fleet/pushed"]);
    let msg = done_message(run(&f, &host, "archive", by_agent(&pushed)).await.unwrap());
    assert!(has_branch(&r.main, "fleet/pushed"), "{msg}");
    assert!(msg.contains("kept"), "{msg}");

    let merged = ended(&r, &f, &host, "merged").await;
    r.write(&format!("{}/m.txt", merged.cwd), "m\n");
    git(&merged.cwd, &["add", "-A"]);
    git(&merged.cwd, &["commit", "-qm", "merged work"]);
    git(&merged.cwd, &["push", "-q", "origin", "HEAD:main"]);
    git(&r.main, &["fetch", "-q", "origin"]);
    done_message(run(&f, &host, "archive", by_agent(&merged)).await.unwrap());
    assert!(!has_branch(&r.main, "fleet/merged"));
}

#[tokio::test]
async fn archive_by_path_and_never_the_main_checkout() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let a = ended(&r, &f, &host, "bypath").await;
    done_message(
        run(&f, &host, "archive", vec![("path", s(&a.cwd))])
            .await
            .unwrap(),
    );
    assert!(!exists(&a.cwd));
    let e = run(&f, &host, "archive", vec![("path", s(&r.main))])
        .await
        .unwrap_err();
    assert!(e.to_string().contains("main checkout"), "{e}");
}

#[tokio::test]
async fn an_agent_archives_only_its_subtrees_worktrees_and_never_forces() {
    let r = Repos::new();
    let f = flow(&r, false);
    let user = LocalHost::new(Caller::User);
    let other = ended(&r, &f, &user, "others").await;
    // Agent 50's view: it sees nothing of the user's, and made `mine`.
    let me = LocalHost::new(Caller::Agent(50));
    let e = run(&f, &me, "archive", vec![("path", s(&other.cwd))])
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("forbidden:"), "{e}");
    let mine = ended(&r, &f, &me, "mine").await;
    let e = run(
        &f,
        &me,
        "archive",
        vec![("path", s(&mine.cwd)), ("force", Value::Bool(true))],
    )
    .await
    .unwrap_err();
    assert!(e.to_string().starts_with("forbidden:"), "{e}");
    done_message(
        run(&f, &me, "archive", vec![("path", s(&mine.cwd))])
            .await
            .unwrap(),
    );
    assert!(!exists(&mine.cwd));
}

#[tokio::test]
async fn an_agent_cannot_archive_the_worktree_it_works_in() {
    let r = Repos::new();
    let f = flow(&r, false);
    let me = LocalHost::new(Caller::Agent(50));
    let mine = ended(&r, &f, &me, "here").await;
    let (reason, retry) = refusal(
        run_in(&f, &me, "archive", vec![("path", s(&mine.cwd))], &mine.cwd)
            .await
            .unwrap(),
    );
    assert!(reason.contains("inside"), "{reason}");
    assert!(retry.is_empty());
}

// ---- prune ----

fn table(o: ActionOutcome) -> TableView {
    match o {
        ActionOutcome::View(v) => match v.kind {
            Some(ViewKind::Table(t)) => t,
            other => panic!("not a table: {other:?}"),
        },
        other => panic!("not a view: {other:?}"),
    }
}

fn row<'a>(t: &'a TableView, path: &str) -> &'a [String] {
    &t.rows
        .iter()
        .find(|r| r.path == path)
        .unwrap_or_else(|| panic!("no row {path} in {t:#?}"))
        .cells
}

#[tokio::test]
async fn prune_archives_only_clean_merged_idle_worktrees_it_made() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let clean = ended(&r, &f, &host, "clean").await;
    let dirty = ended(&r, &f, &host, "dirty").await;
    r.write(&format!("{}/a.txt", dirty.cwd), "x\n");
    let (busy, _) = spawned(
        run(&f, &host, "new-agent", new_params(&r, "busy"))
            .await
            .unwrap(),
    );
    let unmerged = ended(&r, &f, &host, "unmerged").await;
    r.write(&format!("{}/u.txt", unmerged.cwd), "u\n");
    git(&unmerged.cwd, &["add", "-A"]);
    git(&unmerged.cwd, &["commit", "-qm", "u"]);
    let hand = format!("{}/hand", r.root);
    git(
        &r.main,
        &["worktree", "add", "-q", "-b", "hand", &hand, "main"],
    );
    let repo = ("repo", s(&r.main));

    let mut dry = vec![repo.clone(), ("dry_run", Value::Bool(true))];
    let t = table(run(&f, &host, "prune", dry.clone()).await.unwrap());
    assert!(row(&t, &clean.cwd).join(" ").contains("remove"), "{t:#?}");
    assert!(
        row(&t, &dirty.cwd).join(" ").contains("uncommitted"),
        "{t:#?}"
    );
    assert!(row(&t, &busy.cwd).join(" ").contains("running"), "{t:#?}");
    assert!(
        row(&t, &unmerged.cwd).join(" ").contains("not merged"),
        "{t:#?}"
    );
    assert!(
        !t.rows.iter().any(|r| r.path == hand),
        "hand-made worktrees are never pruned"
    );
    assert!(exists(&clean.cwd), "a dry run removes nothing");

    dry.pop();
    let t = table(run(&f, &host, "prune", dry).await.unwrap());
    assert!(row(&t, &clean.cwd).join(" ").contains("removed"), "{t:#?}");
    assert!(!exists(&clean.cwd));
    assert!(!has_branch(&r.main, "fleet/clean"));
    for kept in [&dirty.cwd, &busy.cwd, &unmerged.cwd, &hand] {
        assert!(exists(kept), "{kept}");
    }
}

#[tokio::test]
async fn prune_is_the_users_only() {
    let r = Repos::new();
    let f = flow(&r, false);
    let me = LocalHost::new(Caller::Agent(5));
    let e = run(&f, &me, "prune", vec![("repo", s(&r.main))])
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("forbidden:"), "{e}");
}

#[tokio::test]
async fn prune_fetches_the_base_first_and_drops_records_of_worktrees_gone() {
    let r = Repos::new();
    let f = flow(&r, true);
    let host = LocalHost::new(Caller::User);
    let a = ended(&r, &f, &host, "upstreamed").await;
    r.write(&format!("{}/w.txt", a.cwd), "w\n");
    git(&a.cwd, &["add", "-A"]);
    git(&a.cwd, &["commit", "-qm", "w"]);
    // Merged upstream by someone else: the local origin/main is stale.
    let other = format!("{}/other", r.root);
    git(
        &r.root,
        &["clone", "-q", &format!("{}/origin.git", r.root), "other"],
    );
    git(&other, &["fetch", "-q", &a.cwd, "fleet/upstreamed"]);
    git(&other, &["merge", "-q", "--ff-only", "FETCH_HEAD"]);
    git(&other, &["push", "-q", "origin", "main"]);
    let gone = ended(&r, &f, &host, "gone").await;
    git(&r.main, &["worktree", "remove", &gone.cwd]);

    let t = table(
        run(&f, &host, "prune", vec![("repo", s(&r.main))])
            .await
            .unwrap(),
    );
    assert!(row(&t, &a.cwd).join(" ").contains("removed"), "{t:#?}");
    assert!(!exists(&a.cwd));
    assert!(row(&t, &gone.cwd).join(" ").contains("gone"), "{t:#?}");
    assert_eq!(get_record(&host, &gone.cwd).await.unwrap(), None);
}

// ---- ls ----

#[tokio::test]
async fn ls_lists_every_worktree_with_its_state_and_agents() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let (a, _) = spawned(
        run(&f, &host, "new-agent", {
            let mut p = new_params(&r, "one");
            p.push(("name", s("kid")));
            p
        })
        .await
        .unwrap(),
    );
    r.write(&format!("{}/n.txt", a.cwd), "n\n");
    git(&a.cwd, &["add", "-A"]);
    git(&a.cwd, &["commit", "-qm", "n"]);
    r.write(&format!("{}/a.txt", a.cwd), "dirty\n");
    let hand = format!("{}/hand", r.root);
    git(
        &r.main,
        &["worktree", "add", "-q", "-b", "hand", &hand, "main"],
    );
    let t = table(run_in(&f, &host, "ls", vec![], &a.cwd).await.unwrap());
    assert_eq!(
        t.columns,
        [
            "path",
            "branch",
            "base",
            "ahead/behind",
            "dirty",
            "merged",
            "agents"
        ]
    );
    let one = row(&t, &a.cwd);
    assert_eq!(one[1], "fleet/one");
    assert_eq!(one[2], "origin/main");
    assert_eq!(one[3], "+1 -0");
    assert_eq!(one[4], "yes");
    assert_eq!(one[5], "no");
    assert_eq!(one[6], format!("kid#{}", a.id));
    let linked = t.rows.iter().find(|r| r.path == a.cwd).unwrap();
    assert_eq!(linked.agent_id, Some(a.id));
    let h = row(&t, &hand);
    assert_eq!(
        (h[1].as_str(), h[4].as_str(), h[5].as_str()),
        ("hand", "no", "yes")
    );
    assert!(
        t.rows
            .iter()
            .any(|r| r.path == r.path && r.cells[0] == r.path)
    );
    row(&t, &r.main);
}

// ---- option injection (no caller value reaches git as an option) ----

/// Whether any git call carried `needle` anywhere in its argv.
fn ran_with(host: &LocalHost, needle: &str) -> bool {
    host.execs
        .lock()
        .unwrap()
        .iter()
        .any(|argv| argv.iter().any(|a| a.contains(needle)))
}

#[tokio::test]
async fn an_option_as_base_or_branch_is_invalid_and_never_reaches_git() {
    let r = Repos::new();
    let f = flow(&r, true);
    let host = LocalHost::new(Caller::Agent(9));
    let evil = "--upload-pack=touch /tmp/pwned";
    for (key, value) in [
        ("base", format!("origin/{evil}")),
        ("base", evil.to_string()),
        ("base", "-x".to_string()),
        ("branch", "-x".to_string()),
        ("branch", evil.to_string()),
        ("branch", "a b".to_string()),
    ] {
        let mut p = new_params(&r, "inj");
        p.push((key, s(&value)));
        let e = run(&f, &host, "new-agent", p).await.unwrap_err();
        assert!(matches!(e, ActionError::Invalid(_)), "{key}={value}: {e}");
    }
    assert!(!ran_with(&host, "upload-pack"));
    assert!(!ran_with(&host, "-x"));
    assert!(!exists(&format!("{}/wt/repo/inj", r.root)));
}

#[tokio::test]
async fn an_empty_prefix_and_a_dash_slug_do_not_make_an_option_branch() {
    let r = Repos::new();
    let config = format!(
        "[workflows.git-worktree]\nroot = \"{}/wt\"\nbranch_prefix = \"\"\nfetch_base = false\n",
        r.root
    );
    let f = flow_with(Some(config), &r.root);
    let host = LocalHost::new(Caller::User);
    let e = run(&f, &host, "new-agent", new_params(&r, "-D"))
        .await
        .unwrap_err();
    assert!(matches!(e, ActionError::Invalid(_)), "{e}");
    assert!(!ran_with(&host, "-D"));
}

#[tokio::test]
async fn a_base_label_that_is_an_option_is_invalid_for_diff_archive_and_ls() {
    let r = Repos::new();
    let f = flow(&r, false);
    let host = LocalHost::new(Caller::User);
    let (agent, _) = spawned(
        run(&f, &host, "new-agent", new_params(&r, "lbl"))
            .await
            .unwrap(),
    );
    host.set_state(agent.id, AgentState::Exited);
    // The user may write any label; a bad one must not become a git option.
    host.agents.lock().unwrap()[0]
        .labels
        .insert(LABEL_BASE.into(), "--output=/tmp/x".into());
    let id = Value::Int(agent.id as i64);
    for action in ["diff", "archive"] {
        let e = run(&f, &host, action, vec![("agent", id.clone())])
            .await
            .unwrap_err();
        assert!(matches!(e, ActionError::Invalid(_)), "{action}: {e}");
    }
    assert!(!ran_with(&host, "--output"));
}

#[tokio::test]
async fn positional_refs_follow_end_of_options() {
    let r = Repos::new();
    let f = flow(&r, true);
    let host = LocalHost::new(Caller::User);
    let a = ended(&r, &f, &host, "eoo").await;
    run(&f, &host, "diff", by_agent(&a)).await.unwrap();
    run(&f, &host, "ls", vec![("repo", s(&r.main))])
        .await
        .unwrap();
    run(&f, &host, "archive", by_agent(&a)).await.unwrap();
    for argv in host.execs.lock().unwrap().iter() {
        let sub = argv.get(3).map(String::as_str).unwrap_or_default();
        let sub = if sub == "--no-optional-locks" {
            argv[4].as_str()
        } else {
            sub
        };
        let refs_given = argv
            .iter()
            .any(|x| x.contains("origin/main") || x.starts_with("fleet/"));
        if refs_given
            && [
                "fetch",
                "merge-base",
                "worktree",
                "branch",
                "rev-list",
                "rev-parse",
            ]
            .contains(&sub)
        {
            assert!(argv.iter().any(|x| x == "--end-of-options"), "{argv:?}");
        }
    }
}
