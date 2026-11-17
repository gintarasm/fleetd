//! The inferred `waiting` sweep (ADR 0088, issue #126).

use std::time::{Duration, Instant};

use fleet_node::mailbox::{Notice, WaitState};
use fleet_node::node::{ConnId, Node, WaitSpec};
use fleet_node::scope::Caller;
use fleet_node::store::Labels;
use fleet_proto::status::WAITING_QUIET_MS;
use fleet_proto::{MessageKind, ReportedState, SpawnAgent};

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

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A parent `p` and its child `c`, which prints a prompt-like line.
struct Pair {
    _dir: tempfile::TempDir,
    node: Node,
    p: u64,
    c: u64,
}

impl Pair {
    fn new(script: &str, labels: &[(&str, &str)]) -> Pair {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::open(dir.path()).unwrap();
        let p = node.spawn(sh("exec sleep 600", &[])).unwrap().id;
        let c = node
            .spawn_by(Caller::Agent(p), sh(script, labels))
            .unwrap()
            .id;
        let pair = Pair {
            _dir: dir,
            node,
            p,
            c,
        };
        pair.wait_for_screen(|t| !t.trim().is_empty());
        pair
    }

    fn wait_for_screen(&self, ok: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self
            .node
            .last_activity(self.c)
            .is_some_and(|(line, _)| ok(&line))
        {
            assert!(Instant::now() < deadline, "no output");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The statuses the parent was told of, as `(status, body)`.
    fn told(&self) -> Vec<(String, String)> {
        self.node
            .inbox(Caller::Agent(self.p), Some(0), 0)
            .unwrap()
            .0
            .into_iter()
            .filter(|m| m.kind == MessageKind::Status as i32)
            .map(|m| (m.status, m.body))
            .collect()
    }

    fn wait_spec(&self) -> WaitSpec {
        WaitSpec {
            targets: Some(vec![self.c]),
            states: [WaitState::Waiting].into_iter().collect(),
        }
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        self.node.shutdown();
    }
}

const PROMPT: &str = "printf 'Continue? (y/n) '; exec sleep 600";
const QUIET: i64 = WAITING_QUIET_MS + 1000;

#[test]
fn a_prompt_then_quiet_posts_one_inferred_status_and_wakes_a_named_wait() {
    let f = Pair::new(PROMPT, &[]);
    let mut notices = f.node.notices();
    let parent = Caller::Agent(f.p);
    // Not quiet long enough yet: nothing.
    assert!(f.node.sweep_waiting_at(now()));
    assert!(f.told().is_empty());
    assert!(
        f.node
            .wait_check_at(parent, &f.wait_spec(), None, now())
            .unwrap()
            .is_empty()
    );

    let later = now() + QUIET;
    f.node.sweep_waiting_at(later);
    f.node.sweep_waiting_at(later + 5000);
    assert_eq!(
        f.told(),
        vec![("waiting".to_string(), "waiting (inferred)".to_string())]
    );
    let mut seen = Vec::new();
    while let Ok(n) = notices.try_recv() {
        seen.push(n);
    }
    assert!(seen.contains(&Notice::State {
        agent: f.c,
        state: WaitState::Waiting
    }));
    // `fleet wait --state waiting` answers from the same data.
    let m = f
        .node
        .wait_check_at(parent, &f.wait_spec(), None, later)
        .unwrap();
    assert_eq!((m[0].agent, m[0].state), (Some(f.c), WaitState::Waiting));
}

#[test]
fn output_resuming_ends_it_once() {
    let f = Pair::new(PROMPT, &[]);
    f.node.sweep_waiting_at(now() + QUIET);
    assert_eq!(f.told().len(), 1);

    let before = f.node.last_activity(f.c).unwrap().1;
    f.node.input(f.c, ConnId(1), b"y\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while f.node.last_activity(f.c).unwrap().1 == before {
        assert!(Instant::now() < deadline, "no echo");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The agent printed just now: it is working again (as of "now").
    f.node.sweep_waiting_at(now());
    f.node.sweep_waiting_at(now());
    let told = f.told();
    assert_eq!(told.len(), 2, "{told:?}");
    assert_eq!(told[1].0, "working");
    assert!(told[1].1.ends_with("(inferred)"));
}

#[test]
fn a_live_reported_status_is_not_overridden() {
    let f = Pair::new(PROMPT, &[]);
    f.node
        .report(f.c, ReportedState::ReportedWorking, "thinking")
        .unwrap();
    let before = f.told();
    let later = now() + QUIET;
    f.node.sweep_waiting_at(later);
    assert_eq!(f.told(), before);
    assert!(
        f.node
            .wait_check_at(Caller::Agent(f.p), &f.wait_spec(), None, later)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_shell_is_never_inferred_waiting() {
    let f = Pair::new(PROMPT, &[("kind", "shell")]);
    f.node.sweep_waiting_at(now() + QUIET);
    assert!(f.told().is_empty());
}

#[test]
fn a_quiet_agent_without_a_prompt_is_not_waiting() {
    let f = Pair::new("echo building; exec sleep 600", &[]);
    f.node.sweep_waiting_at(now() + QUIET);
    assert!(f.told().is_empty());
}

#[test]
fn nothing_runs_nothing_is_swept() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    assert!(!node.sweep_waiting_at(now()));
    node.shutdown();
}
