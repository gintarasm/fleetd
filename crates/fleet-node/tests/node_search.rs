//! Searching every agent's output (ADR 0098): screens and history,
//! running and exited agents, newest agent first.

use fleet_node::node::Node;
use fleet_node::term::Needle;
use fleet_proto::SpawnAgent;
use std::time::{Duration, Instant};

fn spawn_sh(node: &Node, script: &str) -> u64 {
    node.spawn(SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cols: 80,
        rows: 10,
        ..Default::default()
    })
    .unwrap()
    .id
}

fn needle(q: &str) -> Needle {
    Needle::new(q, false).unwrap()
}

/// Polls the search until it finds `n` hits for `q`.
fn search_until(node: &Node, q: &str, n: usize) -> fleet_node::search::Hits {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let hits = node.search(needle(q), 0, 0).unwrap();
        if hits.hits.len() >= n {
            return hits;
        }
        assert!(Instant::now() < deadline, "{q:?}: {hits:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn finds_history_and_screen_lines_of_running_and_exited_agents() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    // 40 lines scroll the first ones into history; the last stay on screen.
    let older = spawn_sh(
        &node,
        r#"printf '\033[31mNeedle\033[0m in history\n'; i=0; while [ $i -lt 40 ]; do echo filler $i; i=$((i+1)); done; echo DONE"#,
    );
    let newer = spawn_sh(&node, "echo needle on screen; exec sleep 100");
    let hits = search_until(&node, "NEEDLE", 2);
    let found: Vec<(u64, &str)> = hits
        .hits
        .iter()
        .map(|h| (h.agent_id, h.text.as_str()))
        .collect();
    assert_eq!(
        found,
        vec![(newer, "needle on screen"), (older, "Needle in history")]
    );
    assert!(!hits.truncated);
    // The history hit is history line 0; the screen hit is numbered past
    // the history's end (row 0 of an agent with no history yet).
    assert_eq!(hits.hits[1].line, 0);
    assert_eq!(hits.hits[0].line, 0);

    let case = node
        .search(Needle::new("NEEDLE", true).unwrap(), 0, 0)
        .unwrap();
    assert!(case.hits.is_empty());
}

#[test]
fn a_hit_on_screen_is_numbered_after_the_history() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let id = spawn_sh(
        &node,
        "i=0; while [ $i -lt 30 ]; do echo filler $i; i=$((i+1)); done; echo tail marker; exec sleep 100",
    );
    let hits = search_until(&node, "tail marker", 1);
    let hit = &hits.hits[0];
    assert_eq!(hit.agent_id, id);
    // 31 lines on a 10-row screen: rows 0.. of the screen follow the
    // history's lines, so the marker's number is at least 30 - 9.
    assert!(hit.line >= 21, "{hit:?}");
    let filler = search_until(&node, "filler 0", 1);
    assert_eq!(filler.hits[0].line, 0, "the first line went to history");
}
