//! `fleet report` in the node (ADR 0075, spec §2).

use fleet_node::node::Node;
use fleet_proto::{ReportedState, SpawnAgent};
use std::time::{Duration, Instant};

fn sleeper(labels: &[(&str, &str)]) -> SpawnAgent {
    SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exec sleep 600".into()],
        cols: 80,
        rows: 24,
        labels: labels
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect(),
        ..Default::default()
    }
}

fn reported_of(node: &Node, id: u64) -> Option<fleet_proto::Reported> {
    node.list()
        .unwrap()
        .into_iter()
        .find(|a| a.id == id)
        .unwrap()
        .reported
}

#[test]
fn a_report_is_listed_announced_and_replaced_by_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(&dir.path().join("state")).unwrap();
    let a = node.spawn(sleeper(&[])).unwrap().id;
    assert_eq!(reported_of(&node, a), None);
    let mut events = node.report_events();

    node.report(a, ReportedState::ReportedWaiting, "need a review")
        .unwrap();
    let r = reported_of(&node, a).expect("reported");
    assert_eq!(r.state(), ReportedState::ReportedWaiting);
    assert_eq!(r.message, "need a review");
    assert!(r.at_unix_ms > 0);
    let ev = events.try_recv().expect("an AgentReported event");
    assert_eq!((ev.agent_id, ev.reported.as_ref()), (a, Some(&r)));

    node.report(a, ReportedState::ReportedDone, "").unwrap();
    let r = reported_of(&node, a).unwrap();
    assert_eq!(
        (r.state(), r.message.as_str()),
        (ReportedState::ReportedDone, "")
    );
    node.shutdown();
}

#[test]
fn a_bad_report_is_refused_and_stores_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(&dir.path().join("state")).unwrap();
    let a = node.spawn(sleeper(&[])).unwrap().id;
    let shell = node.spawn(sleeper(&[("kind", "shell")])).unwrap().id;
    let mut events = node.report_events();

    let long = "x".repeat(201);
    for (id, state, msg) in [
        (a, ReportedState::ReportedWorking, long.as_str()),
        (a, ReportedState::Unspecified, "hi"),
        (a, ReportedState::ReportedWorking, "two\nlines"),
        (shell, ReportedState::ReportedWorking, "hi"),
        (9999, ReportedState::ReportedWorking, "hi"),
    ] {
        assert!(
            node.report(id, state, msg).is_err(),
            "{id} {state:?} {msg:?}"
        );
    }
    // 200 characters (not bytes) is the limit.
    node.report(a, ReportedState::ReportedWorking, &"é".repeat(200))
        .unwrap();
    assert_eq!(reported_of(&node, shell), None);
    assert_eq!(events.try_recv().unwrap().agent_id, a);
    assert!(events.try_recv().is_err());

    node.kill(a).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while node.exit_code(a).is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(node.report(a, ReportedState::ReportedDone, "late").is_err());
    node.shutdown();
}

#[test]
fn a_report_survives_a_daemon_restart() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let node = Node::open(&state).unwrap();
    let a = node.spawn(sleeper(&[])).unwrap().id;
    node.report(a, ReportedState::ReportedError, "build broke")
        .unwrap();
    node.shutdown();
    drop(node);
    let node = Node::open(&state).unwrap();
    let r = reported_of(&node, a).expect("still reported");
    assert_eq!(
        (r.state(), r.message.as_str()),
        (ReportedState::ReportedError, "build broke")
    );
    node.shutdown();
}
