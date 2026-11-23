//! Spawns by a workflow for its caller (ADR 0092): the owned label prefix,
//! the quota dry run, and that everything else is an ordinary spawn by
//! the caller.

use fleet_node::node::Node;
use fleet_node::scope::{Caller, FORBIDDEN_PREFIX, PARENT_LABEL, Quotas};
use fleet_node::store::Labels;
use fleet_proto::SpawnAgent;

fn labels(pairs: &[(&str, &str)]) -> Labels {
    pairs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect()
}

fn sleeper(l: &[(&str, &str)]) -> SpawnAgent {
    SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exec sleep 600".into()],
        cols: 80,
        rows: 24,
        labels: labels(l),
        ..Default::default()
    }
}

fn is_forbidden(e: &std::io::Error) -> bool {
    e.to_string().starts_with(FORBIDDEN_PREFIX)
}

#[test]
fn a_workflow_spawn_for_an_agent_replaces_the_owned_prefix_only() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(&dir.path().join("state")).unwrap();
    let p = node
        .spawn(sleeper(&[
            ("project", "fleetd"),
            ("git.worktree", "/wt/parent"),
            ("git.base", "main"),
        ]))
        .unwrap()
        .id;
    let c = node
        .spawn_for_workflow(
            Caller::Agent(p),
            sleeper(&[("role", "coder"), ("git.worktree", "/wt/child")]),
            "git.",
        )
        .unwrap();
    assert_eq!(
        c.labels,
        labels(&[
            ("project", "fleetd"),
            ("role", "coder"),
            ("git.worktree", "/wt/child"),
            (PARENT_LABEL, &p.to_string()),
        ])
    );

    // Outside the prefix, narrowing holds as for any spawn of p's.
    let e = node
        .spawn_for_workflow(
            Caller::Agent(p),
            sleeper(&[("project", "other"), ("git.worktree", "/wt/x")]),
            "git.",
        )
        .unwrap_err();
    assert!(is_forbidden(&e), "{e}");
    // A plain spawn of p's still inherits git.* and cannot change it.
    let e = node
        .spawn_by(Caller::Agent(p), sleeper(&[("git.worktree", "/wt/y")]))
        .unwrap_err();
    assert!(is_forbidden(&e), "{e}");
    let plain = node.spawn_by(Caller::Agent(p), sleeper(&[])).unwrap();
    assert_eq!(
        plain.labels.get("git.worktree").map(String::as_str),
        Some("/wt/parent")
    );
    // A bad prefix is refused before anything starts.
    let before = node.list().unwrap().len();
    let e = node
        .spawn_for_workflow(Caller::Agent(p), sleeper(&[]), "fleet.")
        .unwrap_err();
    assert!(is_forbidden(&e), "{e}");
    // Reserved keys stay refused.
    assert!(
        node.spawn_for_workflow(Caller::Agent(p), sleeper(&[(PARENT_LABEL, "1")]), "git.")
            .is_err()
    );
    assert_eq!(node.list().unwrap().len(), before);
    node.shutdown();
}

#[test]
fn a_workflow_spawn_for_the_user_takes_the_labels_as_given() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(&dir.path().join("state")).unwrap();
    let c = node
        .spawn_for_workflow(
            Caller::User,
            sleeper(&[("project", "x"), ("git.repo", "/r")]),
            "git.",
        )
        .unwrap();
    assert_eq!(c.labels, labels(&[("project", "x"), ("git.repo", "/r")]));
    node.shutdown();
}

#[test]
fn a_workflow_spawn_is_under_the_callers_quotas() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(&dir.path().join("state"))
        .unwrap()
        .with_quotas(Quotas {
            max_children: 1,
            ..Quotas::default()
        });
    let p = node.spawn(sleeper(&[])).unwrap().id;
    assert!(node.check_spawn(Caller::Agent(p)).is_ok());
    assert_eq!(node.list().unwrap().len(), 1, "a dry run spawns nothing");
    node.spawn_for_workflow(Caller::Agent(p), sleeper(&[]), "git.")
        .unwrap();
    let e = node.check_spawn(Caller::Agent(p)).unwrap_err();
    assert!(
        is_forbidden(&e) && e.to_string().contains("max_children"),
        "{e}"
    );
    let e = node
        .spawn_for_workflow(Caller::Agent(p), sleeper(&[]), "git.")
        .unwrap_err();
    assert!(is_forbidden(&e), "{e}");
    assert!(
        node.check_spawn(Caller::User).is_ok(),
        "the user has no quota"
    );
    node.shutdown();
}

#[test]
fn a_workflow_precheck_shows_the_owned_labels() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(&dir.path().join("state")).unwrap();
    let p = node
        .spawn(sleeper(&[("git.worktree", "/wt/parent")]))
        .unwrap()
        .id;
    let pending = node
        .precheck_spawn_for_workflow(
            Caller::Agent(p),
            &sleeper(&[("git.worktree", "/wt/child")]),
            "git.",
        )
        .unwrap();
    assert_eq!(pending.parent, p);
    assert_eq!(
        pending.labels.get("git.worktree").map(String::as_str),
        Some("/wt/child")
    );
    assert_eq!(node.list().unwrap().len(), 1, "nothing was spawned");
    node.shutdown();
}

#[test]
fn workflow_storage_goes_through_the_node() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(&dir.path().join("state")).unwrap();
    node.kv_put("git-worktree", "wt/a", b"{}").unwrap();
    assert_eq!(
        node.kv_get("git-worktree", "wt/a").unwrap().as_deref(),
        Some(&b"{}"[..])
    );
    assert_eq!(node.kv_keys("git-worktree", "wt/").unwrap(), vec!["wt/a"]);
    let e = node
        .kv_put("git-worktree", "big", &vec![0; 64 * 1024 + 1])
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{e}");
    assert!(node.kv_delete("git-worktree", "wt/a").unwrap());
    assert_eq!(node.kv_get("git-worktree", "wt/a").unwrap(), None);
    node.shutdown();
}
