//! Node resource bounds. A single test in its own binary, so no concurrent
//! test opens or closes fds while they are counted.

use fleet_node::node::Node;
use fleet_proto::SpawnAgent;
use std::time::{Duration, Instant};

fn open_fds() -> usize {
    std::fs::read_dir("/dev/fd").unwrap().count()
}

#[test]
fn exited_agents_release_their_resources() {
    const AGENTS: usize = 200;
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let mut exits = node.exit_events();
    let baseline = open_fds();

    for _ in 0..AGENTS {
        node.spawn(SpawnAgent {
            program: "/usr/bin/true".into(),
            ..Default::default()
        })
        .unwrap();
    }
    for _ in 0..AGENTS {
        exits.blocking_recv().unwrap();
    }

    // Reader threads close their last fds just after announcing the exit.
    let deadline = Instant::now() + Duration::from_secs(5);
    while open_fds() > baseline + 10 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let now = open_fds();
    assert!(now <= baseline + 10, "{now} fds open, baseline {baseline}");
    let retained = node.retained_exited_agents();
    assert!(retained <= 64, "{retained} exited agents retained");

    // Every agent still lists, and attaching to any of them yields its exit:
    // the oldest exit was evicted (empty snapshot), the newest is retained.
    let listed = node.list().unwrap();
    assert_eq!(listed.len(), AGENTS);
    let evicted = node.attach(listed[0].id).unwrap().0;
    assert!(evicted.snapshot.is_empty() && (evicted.cols, evicted.rows) == (0, 0));
    for info in [&listed[0], &listed[AGENTS - 1]] {
        let (ok, mut sub) = node.attach(info.id).unwrap();
        assert_eq!(ok.agent_id, info.id);
        assert_eq!(
            sub.blocking_recv(),
            Some(fleet_node::node::StreamItem::Exited(0))
        );
    }
}
