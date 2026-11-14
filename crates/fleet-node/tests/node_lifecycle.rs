//! Node lifecycle: forgetting exited agents and announcing the removal.

use fleet_node::node::{ConnId, Node, Removed};
use fleet_node::store::Labels;
use fleet_proto::{AgentRemoved, SpawnAgent};
use std::io;
use std::time::{Duration, Instant};

fn spawn_sh_labeled(node: &Node, script: &str, labels: &[(&str, &str)]) -> u64 {
    node.spawn(SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cols: 80,
        rows: 24,
        labels: labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    })
    .unwrap()
    .id
}

fn spawn_sh(node: &Node, script: &str) -> u64 {
    spawn_sh_labeled(node, script, &[])
}

fn spawn_exited(node: &Node, labels: &[(&str, &str)]) -> u64 {
    let mut exits = node.exit_events();
    let id = spawn_sh_labeled(node, "echo hi; exit 3", labels);
    loop {
        if exits.blocking_recv().unwrap().agent_id == id {
            return id;
        }
    }
}

fn ids(node: &Node) -> Vec<u64> {
    node.list().unwrap().into_iter().map(|a| a.id).collect()
}

#[test]
fn forget_removes_exited_agent_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let id = spawn_exited(&node, &[]);
    let ring = dir.path().join("scrollback").join(format!("{id}.ring"));
    assert!(ring.exists(), "ring should exist before the forget");
    let before = node.retained_exited_agents();
    let out = node.forget(&[id], false, &Labels::new(), None).unwrap();
    assert_eq!(out.forgotten, vec![id]);
    assert!(out.refused.is_empty());
    assert!(!ids(&node).contains(&id));
    assert!(node.attach(id, ConnId(1)).is_err());
    assert!(!dir.path().join("history").join(id.to_string()).exists());
    assert!(
        !dir.path()
            .join("scrollback")
            .join(format!("{id}.ring"))
            .exists()
    );
    assert_eq!(node.retained_exited_agents(), before - 1);
}

#[test]
fn forget_refuses_running_and_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let id = spawn_sh(&node, "while read l; do echo got:$l; done");
    let out = node
        .forget(&[id, 9999], false, &Labels::new(), None)
        .unwrap();
    assert!(out.forgotten.is_empty());
    assert_eq!(out.refused, vec![id, 9999]);
    node.input(id, ConnId(1), b"ping\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (ok, _sub) = node.attach(id, ConnId(1)).unwrap();
        if String::from_utf8_lossy(&ok.snapshot).contains("got:ping") {
            break;
        }
        assert!(Instant::now() < deadline, "echo never seen");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn forget_exited_only_respects_selector() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let a1 = spawn_exited(&node, &[("project", "a")]);
    let b = spawn_exited(&node, &[("project", "b")]);
    let a2 = spawn_exited(&node, &[("project", "a")]);
    let running = spawn_sh_labeled(&node, "exec sleep 100", &[("project", "a")]);
    let sel: Labels = [("project".to_string(), "a".to_string())].into();
    let mut out = node.forget(&[], true, &sel, None).unwrap();
    out.forgotten.sort();
    assert_eq!(out.forgotten, vec![a1, a2]);
    let mut left = ids(&node);
    left.sort();
    assert_eq!(left, vec![b, running]);
}

#[test]
fn forget_exited_only_with_ids_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let e = node.forget(&[1], true, &Labels::new(), None).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn forget_announces_removed() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let mut removed = node.removed_events();
    let id = spawn_exited(&node, &[]);
    node.forget(&[id], false, &Labels::new(), None).unwrap();
    let Removed { event, origin } = removed.blocking_recv().unwrap();
    assert_eq!(
        event,
        AgentRemoved {
            agent_id: id,
            replaced_by: None
        }
    );
    assert_eq!(origin, None);
}

#[test]
fn forgotten_id_is_not_reused() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let id = spawn_exited(&node, &[]);
    node.forget(&[id], false, &Labels::new(), None).unwrap();
    let next = spawn_exited(&node, &[]);
    assert!(next > id);
}

#[test]
fn forget_removes_lost_agent() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let node = Node::open(dir.path()).unwrap();
        let id = spawn_sh(&node, "exec sleep 100");
        // Dropping the node without shutdown leaves the row `running`.
        std::mem::forget(node);
        id
    };
    let node = Node::open(dir.path()).unwrap();
    let info = node
        .list()
        .unwrap()
        .into_iter()
        .find(|a| a.id == id)
        .unwrap();
    assert_eq!(info.state, fleet_proto::AgentState::Lost as i32);
    let out = node.forget(&[id], false, &Labels::new(), None).unwrap();
    assert_eq!(out.forgotten, vec![id]);
    assert!(!ids(&node).contains(&id));
}
