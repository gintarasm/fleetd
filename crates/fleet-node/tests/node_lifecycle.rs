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

fn wait_exited(exits: &mut tokio::sync::broadcast::Receiver<fleet_proto::AgentExited>, id: u64) {
    loop {
        if exits.blocking_recv().unwrap().agent_id == id {
            return;
        }
    }
}

/// Polls the agent's screen (live or retained) until it contains `needle`.
fn wait_screen(node: &Node, id: u64, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (ok, _sub) = node.observe(id).unwrap();
        let screen = String::from_utf8_lossy(&ok.snapshot).into_owned();
        if screen.contains(needle) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "{needle:?} never seen: {screen:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn spawn_sh_in(node: &Node, cwd: &std::path::Path, script: &str, labels: &[(&str, &str)]) -> u64 {
    node.spawn(SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: cwd.to_string_lossy().into_owned(),
        labels: labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    })
    .unwrap()
    .id
}

#[test]
fn restart_replaces_exited_agent() {
    let dir = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let mut exits = node.exit_events();
    let mut removed = node.removed_events();
    let script = "echo run-$FLEET_AGENT_ID; exit 0";
    let old = spawn_sh_in(&node, work.path(), script, &[("project", "x")]);
    wait_exited(&mut exits, old);
    let before = node
        .list()
        .unwrap()
        .into_iter()
        .find(|a| a.id == old)
        .unwrap();
    let new = node.restart(old, 0, 0, None).unwrap();
    assert_ne!(new.id, old);
    assert_eq!(new.program, before.program);
    assert_eq!(new.args, before.args);
    assert_eq!(new.cwd, before.cwd);
    assert_eq!(new.labels, before.labels);
    assert_eq!(new.state, fleet_proto::AgentState::Running as i32);
    let listed = ids(&node);
    assert!(listed.contains(&new.id));
    assert!(!listed.contains(&old));
    assert_eq!(node.replacements().unwrap(), [(old, new.id)].into());
    wait_screen(&node, new.id, &format!("run-{}", new.id));
    let Removed { event, origin } = removed.blocking_recv().unwrap();
    assert_eq!(
        event,
        AgentRemoved {
            agent_id: old,
            replaced_by: Some(new.id)
        }
    );
    assert_eq!(origin, None);
}

#[test]
fn restart_refuses_running_and_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let id = spawn_sh(&node, "while read l; do echo got:$l; done");
    let e = node.restart(id, 0, 0, None).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    let e = node.restart(9999, 0, 0, None).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound);
    assert_eq!(ids(&node), vec![id]);
    assert!(node.replacements().unwrap().is_empty());
    node.input(id, ConnId(1), b"ping\n").unwrap();
    wait_screen(&node, id, "got:ping");
}

#[test]
fn restart_failure_keeps_old_agent() {
    let dir = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let mut exits = node.exit_events();
    let mut removed = node.removed_events();
    let old = spawn_sh_in(&node, work.path(), "echo hi; exit 0", &[]);
    wait_exited(&mut exits, old);
    let work_path = work.path().to_path_buf();
    drop(work);
    assert!(!work_path.exists());
    assert!(node.restart(old, 0, 0, None).is_err());
    assert_eq!(ids(&node), vec![old]);
    assert!(node.replacements().unwrap().is_empty());
    assert!(node.attach(old, ConnId(1)).is_ok(), "old screen kept");
    assert!(removed.try_recv().is_err(), "nothing announced");
}

#[test]
fn concurrent_restart_creates_one_agent() {
    let dir = tempfile::tempdir().unwrap();
    let node = std::sync::Arc::new(Node::open(dir.path()).unwrap());
    let mut exits = node.exit_events();
    let old = spawn_sh(&node, "echo hi; exit 0");
    wait_exited(&mut exits, old);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let node = std::sync::Arc::clone(&node);
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                node.restart(old, 0, 0, None)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let oks: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(oks.len(), 1, "{results:?}");
    let listed = node.list().unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].id, oks[0].id);
    assert_ne!(listed[0].id, old);
}

#[test]
fn restart_attached_gets_first_output() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let mut exits = node.exit_events();
    let old = spawn_sh(&node, "echo first-$FLEET_AGENT_ID; exec sleep 100");
    // Kill so it exits, then restart.
    let first = format!("first-{old}");
    wait_screen(&node, old, &first);
    node.kill(old).unwrap();
    wait_exited(&mut exits, old);
    let mut started = None;
    let (info, ok, mut sub) = node
        .restart_attached(old, 80, 24, ConnId(7), |id| started = Some(id))
        .unwrap();
    assert_eq!(started, Some(info.id));
    assert_eq!(ok.agent_id, info.id);
    assert!(ok.primary);
    let want = format!("first-{}", info.id);
    let mut seen = String::from_utf8_lossy(&ok.snapshot).into_owned();
    while !seen.contains(&want) {
        match sub.blocking_recv() {
            Some(fleet_node::node::StreamItem::Output(b)) => {
                seen.push_str(&String::from_utf8_lossy(&b))
            }
            Some(_) => {}
            None => panic!("stream ended before {want:?}: {seen:?}"),
        }
    }
    node.shutdown();
}

#[test]
fn restart_attached_announces_origin() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let mut removed = node.removed_events();
    let old = spawn_exited(&node, &[]);
    let (info, _ok, _sub) = node.restart_attached(old, 0, 0, ConnId(7), |_| {}).unwrap();
    let Removed { event, origin } = removed.blocking_recv().unwrap();
    assert_eq!(event.agent_id, old);
    assert_eq!(event.replaced_by, Some(info.id));
    assert_eq!(origin, Some(ConnId(7)));
}

#[test]
fn restart_does_not_restore_env() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let mut exits = node.exit_events();
    let old = node
        .spawn(SpawnAgent {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "echo secret=[$SECRET] done; exit 0".into()],
            env: [("SECRET".to_string(), "s3cr3tv4lue".to_string())].into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    wait_exited(&mut exits, old);
    wait_screen(&node, old, "secret=[s3cr3tv4lue]");
    let new = node.restart(old, 0, 0, None).unwrap();
    let screen = wait_screen(&node, new.id, "done");
    assert!(screen.contains("secret=[]"), "{screen:?}");
    assert!(!screen.contains("s3cr3tv4lue"), "{screen:?}");
}
