//! Mailboxes in the node (ADR 0076, spec §3): who may write to whom,
//! broadcast, inbox cursors, automatic reports and wait checks.

use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant};

use fleet_node::mailbox::{Notice, Status, WaitState};
use fleet_node::node::{Node, WaitSpec};
use fleet_node::scope::{Caller, FORBIDDEN_PREFIX};
use fleet_node::store::Labels;
use fleet_proto::{Broadcast, MessageKind, ReportedState, SendMessage, SpawnAgent};

fn sh(script: &str, l: &[(&str, &str)]) -> SpawnAgent {
    SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cols: 80,
        rows: 24,
        labels: l
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect::<Labels>(),
        ..Default::default()
    }
}

fn sleeper() -> SpawnAgent {
    sh("exec sleep 600", &[])
}

fn wait_exit(node: &Node, id: u64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while node.exit_code(id).is_none() {
        assert!(Instant::now() < deadline, "agent {id} did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn is_forbidden(e: &io::Error) -> bool {
    e.to_string().starts_with(FORBIDDEN_PREFIX)
}

fn to(ids: &[u64], body: &str) -> SendMessage {
    SendMessage {
        body: body.into(),
        agent_ids: ids.to_vec(),
        ..Default::default()
    }
}

/// A node with the user's root `r`, its children `c1`, `c2` (with
/// `team=a` on `c2`), and `g`, a child of `c1`.
struct Family {
    _dir: tempfile::TempDir,
    node: Node,
    r: u64,
    c1: u64,
    c2: u64,
    g: u64,
}

impl Family {
    fn new() -> Family {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::open(dir.path()).unwrap();
        let r = node.spawn(sleeper()).unwrap().id;
        let c1 = node.spawn_by(Caller::Agent(r), sleeper()).unwrap().id;
        let c2 = node
            .spawn_by(Caller::Agent(r), sh("exec sleep 600", &[("team", "a")]))
            .unwrap()
            .id;
        let g = node.spawn_by(Caller::Agent(c1), sleeper()).unwrap().id;
        Family {
            _dir: dir,
            node,
            r,
            c1,
            c2,
            g,
        }
    }

    fn inbox(&self, caller: Caller) -> Vec<fleet_proto::Message> {
        self.node.inbox(caller, Some(0), 0).unwrap().0
    }
}

impl Drop for Family {
    fn drop(&mut self) {
        self.node.shutdown();
    }
}

fn bodies(ms: &[fleet_proto::Message]) -> Vec<&str> {
    ms.iter().map(|m| m.body.as_str()).collect()
}

#[test]
fn an_agent_writes_to_its_descendants_its_parent_and_the_user_only() {
    let f = Family::new();
    let c1 = Caller::Agent(f.c1);
    // Its child, its parent, the user.
    f.node.send_message(c1, &to(&[f.g], "to g")).unwrap();
    f.node.send_message(c1, &to(&[f.r], "to r")).unwrap();
    let sent = f
        .node
        .send_message(
            c1,
            &SendMessage {
                body: "to the user".into(),
                to_user: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(sent.user && sent.agent_ids.is_empty());
    // Not its sibling, nor itself, nor an unknown id.
    for id in [f.c2, f.c1, 999] {
        let e = f.node.send_message(c1, &to(&[id], "no")).unwrap_err();
        assert!(is_forbidden(&e), "{id}: {e}");
    }
    let g = f.inbox(Caller::Agent(f.g));
    assert_eq!(bodies(&g), ["to g"]);
    assert_eq!(g[0].sender, Some(f.c1));
    assert_eq!(g[0].kind, MessageKind::Message as i32);
    assert_eq!(bodies(&f.inbox(Caller::Agent(f.r))), ["to r"]);
    assert_eq!(bodies(&f.inbox(Caller::User)), ["to the user"]);
    assert!(f.inbox(Caller::Agent(f.c2)).is_empty());
}

#[test]
fn to_parent_reaches_the_parent_or_for_a_root_the_user() {
    let f = Family::new();
    let up = SendMessage {
        body: "up".into(),
        to_parent: true,
        ..Default::default()
    };
    f.node.send_message(Caller::Agent(f.g), &up).unwrap();
    f.node.send_message(Caller::Agent(f.r), &up).unwrap();
    assert_eq!(bodies(&f.inbox(Caller::Agent(f.c1))), ["up"]);
    assert_eq!(bodies(&f.inbox(Caller::User)), ["up"]);
    assert!(f.node.send_message(Caller::User, &up).is_err());
}

#[test]
fn bodies_are_checked() {
    let f = Family::new();
    assert!(f.node.send_message(Caller::User, &to(&[f.r], "")).is_err());
    let big = "x".repeat(4097);
    assert!(
        f.node
            .send_message(Caller::User, &to(&[f.r], &big))
            .is_err()
    );
    let just = "x".repeat(4096);
    f.node
        .send_message(Caller::User, &to(&[f.r], &just))
        .unwrap();
    let e = f
        .node
        .send_message(Caller::User, &to(&[999], "hi"))
        .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound);
}

#[test]
fn broadcasts_reach_running_children_descendants_or_labelled_ones() {
    let f = Family::new();
    let send = |caller, broadcast: Broadcast, sel: &[(&str, &str)]| {
        let req = SendMessage {
            body: "all".into(),
            broadcast: broadcast as i32,
            selector: sel
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
            ..Default::default()
        };
        let mut ids = f.node.send_message(caller, &req).map(|s| s.agent_ids)?;
        ids.sort_unstable();
        io::Result::Ok(ids)
    };
    let r = Caller::Agent(f.r);
    assert_eq!(send(r, Broadcast::Children, &[]).unwrap(), [f.c1, f.c2]);
    assert_eq!(
        send(r, Broadcast::Descendants, &[]).unwrap(),
        [f.c1, f.c2, f.g]
    );
    assert_eq!(
        send(r, Broadcast::Descendants, &[("team", "a")]).unwrap(),
        [f.c2]
    );
    // The user's children are the roots; descendants are everyone.
    assert_eq!(send(Caller::User, Broadcast::Children, &[]).unwrap(), [f.r]);
    assert_eq!(
        send(Caller::User, Broadcast::Descendants, &[]).unwrap(),
        [f.r, f.c1, f.c2, f.g]
    );
    // Nobody: an error saying so (exit 1 in the CLI).
    let e = send(Caller::Agent(f.g), Broadcast::Children, &[]).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound);
    // Ended agents get no broadcast.
    f.node.kill(f.c2).unwrap();
    wait_exit(&f.node, f.c2);
    assert_eq!(send(r, Broadcast::Children, &[]).unwrap(), [f.c1]);
}

#[test]
fn the_inbox_reads_from_the_read_mark_and_moves_it() {
    let f = Family::new();
    for body in ["one", "two"] {
        f.node
            .send_message(Caller::User, &to(&[f.r], body))
            .unwrap();
    }
    let r = Caller::Agent(f.r);
    let (first, cursor) = f.node.inbox(r, None, 0).unwrap();
    assert_eq!(bodies(&first), ["one", "two"]);
    assert_eq!(cursor, first[1].id);
    let (none, same) = f.node.inbox(r, None, 0).unwrap();
    assert!(none.is_empty());
    assert_eq!(same, cursor);
    f.node
        .send_message(Caller::User, &to(&[f.r], "three"))
        .unwrap();
    assert_eq!(bodies(&f.node.inbox(r, None, 0).unwrap().0), ["three"]);
    // An explicit cursor rereads.
    assert_eq!(
        bodies(&f.node.inbox(r, Some(first[0].id), 0).unwrap().0),
        ["two", "three"]
    );
    // A page is at most `max`.
    assert_eq!(f.node.inbox(r, Some(0), 1).unwrap().0.len(), 1);
}

#[test]
fn an_exit_is_reported_to_the_parent_with_its_code() {
    let f = Family::new();
    let child = f
        .node
        .spawn_by(Caller::Agent(f.c1), sh("exit 3", &[("name", "quick")]))
        .unwrap()
        .id;
    wait_exit(&f.node, child);
    let deadline = Instant::now() + Duration::from_secs(10);
    let m = loop {
        let got = f.inbox(Caller::Agent(f.c1));
        if let Some(m) = got.into_iter().find(|m| m.sender == Some(child)) {
            break m;
        }
        assert!(Instant::now() < deadline, "no exit report");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(m.kind, MessageKind::Exit as i32);
    assert_eq!(m.exit_code, Some(3));
    assert_eq!(m.sender_name, "quick");
    assert_eq!(m.recipient, Some(f.c1));
}

#[test]
fn a_status_change_is_reported_once_per_change() {
    let f = Family::new();
    let mut notices = f.node.notices();
    assert!(
        f.node
            .status_changed(f.g, Status::Working, "", false)
            .unwrap()
    );
    assert!(
        !f.node
            .status_changed(f.g, Status::Working, "", false)
            .unwrap()
    );
    assert!(
        f.node
            .status_changed(f.g, Status::Waiting, "need a review", false)
            .unwrap()
    );
    let got = f.inbox(Caller::Agent(f.c1));
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|m| m.kind == MessageKind::Status as i32));
    assert_eq!(got[1].status, "waiting");
    assert_eq!(got[1].body, "need a review");
    let mut seen = Vec::new();
    while let Ok(n) = notices.try_recv() {
        seen.push(n);
    }
    assert!(seen.contains(&Notice::State {
        agent: f.g,
        state: WaitState::Waiting
    }));
    assert!(seen.contains(&Notice::Message {
        recipient: Some(f.c1)
    }));
}

fn spec(targets: Option<Vec<u64>>, states: &[WaitState]) -> WaitSpec {
    WaitSpec {
        targets,
        states: if states.is_empty() {
            WaitState::ALL.into_iter().collect()
        } else {
            states.iter().copied().collect::<HashSet<_>>()
        },
    }
}

#[test]
fn a_named_target_matches_on_its_current_state() {
    let f = Family::new();
    let c1 = Caller::Agent(f.c1);
    let s = spec(Some(vec![f.g]), &[WaitState::Exited, WaitState::Done]);
    assert!(f.node.wait_check(c1, &s, None).unwrap().is_empty());
    // `fleet report` stores the status and posts it to the parent (ADR 0075/0076).
    f.node
        .report(f.g, ReportedState::ReportedDone, "all green")
        .unwrap();
    let m = f.node.wait_check(c1, &s, None).unwrap();
    assert_eq!(m.len(), 1, "{m:?}");
    assert_eq!((m[0].agent, m[0].state), (Some(f.g), WaitState::Done));
    assert_eq!(m[0].message.as_ref().unwrap().body, "all green");
    // Reading the inbox does not hide a named target's state.
    f.inbox(c1);
    f.node.inbox(c1, None, 0).unwrap();
    let m = f.node.wait_check(c1, &s, None).unwrap();
    assert_eq!((m[0].agent, m[0].state), (Some(f.g), WaitState::Done));
    // Already exited: matches at once.
    f.node.kill(f.g).unwrap();
    wait_exit(&f.node, f.g);
    let only_exit = spec(Some(vec![f.g]), &[WaitState::Exited]);
    let m = f.node.wait_check(c1, &only_exit, None).unwrap();
    assert!(m.iter().any(|m| m.state == WaitState::Exited), "{m:?}");
    // An unknown target is an error.
    assert!(
        f.node
            .wait_check(c1, &spec(Some(vec![999]), &[]), None)
            .is_err()
    );
}

#[test]
fn without_names_only_news_matches() {
    let f = Family::new();
    let r = Caller::Agent(f.r);
    let all = spec(None, &[]);
    assert!(f.node.wait_check(r, &all, None).unwrap().is_empty());
    // An unread report in the mailbox matches until it is read.
    f.node
        .status_changed(f.c2, Status::Waiting, "", false)
        .unwrap();
    let m = f.node.wait_check(r, &all, None).unwrap();
    assert_eq!((m[0].agent, m[0].state), (Some(f.c2), WaitState::Waiting));
    // ...but not when the state is not asked for.
    let exits = spec(None, &[WaitState::Exited]);
    assert!(f.node.wait_check(r, &exits, None).unwrap().is_empty());
    f.node.inbox(r, None, 0).unwrap();
    assert!(f.node.wait_check(r, &all, None).unwrap().is_empty());
    // A grandchild's change reaches its parent's mailbox, not r's: r
    // matches it as it happens.
    let notice = Notice::State {
        agent: f.g,
        state: WaitState::Done,
    };
    let m = f.node.wait_check(r, &all, Some(&notice)).unwrap();
    assert_eq!((m[0].agent, m[0].state), (Some(f.g), WaitState::Done));
    // Not someone else's agent's.
    let other = Notice::State {
        agent: f.r,
        state: WaitState::Done,
    };
    assert!(
        f.node
            .wait_check(Caller::Agent(f.c2), &all, Some(&other))
            .unwrap()
            .is_empty()
    );
    // A plain message matches `message`.
    f.node
        .send_message(Caller::User, &to(&[f.r], "hello"))
        .unwrap();
    let m = f
        .node
        .wait_check(r, &spec(None, &[WaitState::Message]), None)
        .unwrap();
    assert_eq!(m[0].agent, None, "from the user");
    assert_eq!(m[0].state, WaitState::Message);
}

#[test]
fn a_restarted_agent_keeps_its_mailbox() {
    let f = Family::new();
    let quick = f
        .node
        .spawn_by(Caller::Agent(f.r), sh("exit 0", &[]))
        .unwrap()
        .id;
    wait_exit(&f.node, quick);
    f.node
        .send_message(Caller::User, &to(&[quick], "keep me"))
        .unwrap();
    let new = f.node.restart(quick, 80, 24, None).unwrap().info.id;
    assert_eq!(bodies(&f.inbox(Caller::Agent(new))), ["keep me"]);
}

/// `fleet report` (ADR 0075) feeds the mailbox (ADR 0076): the parent gets
/// one `status` message per change, and a wait on the reporter wakes.
#[test]
fn a_report_reaches_the_parent_and_wakes_a_named_wait() {
    let f = Family::new();
    let c1 = Caller::Agent(f.c1);
    let mut notices = f.node.notices();
    let waiting = spec(Some(vec![f.g]), &[WaitState::Waiting]);
    assert!(f.node.wait_check(c1, &waiting, None).unwrap().is_empty());
    f.node
        .report(f.g, ReportedState::ReportedWaiting, "need a review")
        .unwrap();
    // The same report again posts nothing new.
    f.node
        .report(f.g, ReportedState::ReportedWaiting, "need a review")
        .unwrap();
    let got = f.inbox(c1);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].kind, MessageKind::Status as i32);
    assert_eq!(
        (got[0].status.as_str(), got[0].body.as_str()),
        ("waiting", "need a review")
    );
    assert_eq!(got[0].sender, Some(f.g));
    let m = f.node.wait_check(c1, &waiting, None).unwrap();
    assert_eq!((m[0].agent, m[0].state), (Some(f.g), WaitState::Waiting));
    let mut seen = Vec::new();
    while let Ok(n) = notices.try_recv() {
        seen.push(n);
    }
    assert!(
        seen.contains(&Notice::State {
            agent: f.g,
            state: WaitState::Waiting
        }),
        "{seen:?}"
    );
}
