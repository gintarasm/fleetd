//! Node history: attach tail, paged reads, restart, clear and retention.

use fleet_node::Terminal;
use fleet_node::node::{Node, RETAINED_EXITED};
use fleet_proto::{AttachOk, SpawnAgent};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

fn spawn_sh(node: &Node, script: &str) -> u64 {
    node.spawn(SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cols: 80,
        rows: 24,
        ..Default::default()
    })
    .unwrap()
    .id
}

/// The text of one ANSI-rendered line: CSI and other escape sequences dropped.
fn strip_ansi(line: &[u8]) -> String {
    let mut out = Vec::new();
    let mut i = 0;
    while i < line.len() {
        if line[i] == 0x1b {
            i += 1;
            if i < line.len() && line[i] == b'[' {
                i += 1;
                while i < line.len() && !(0x40..=0x7e).contains(&line[i]) {
                    i += 1;
                }
            }
            i += 1;
        } else {
            out.push(line[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn screen_text(ok: &AttachOk) -> Vec<String> {
    let mut term = Terminal::new(ok.cols as u16, ok.rows as u16);
    term.feed(&ok.snapshot);
    term.visible_text()
}

/// Polls attach until the screen shows `needle`.
fn attach_when_shown(node: &Node, id: u64, needle: &str) -> AttachOk {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (ok, _sub) = node.attach(id).unwrap();
        if screen_text(&ok).iter().any(|l| l.contains(needle)) {
            return ok;
        }
        assert!(Instant::now() < deadline, "{needle:?} never shown");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Every history line before `before`, oldest first, read page by page back
/// to `first_line`.
fn read_all_before(
    node: &Node,
    id: u64,
    generation: u64,
    before: u64,
    first_line: u64,
) -> Vec<String> {
    let mut pages = Vec::new();
    let mut before = before;
    while before > first_line {
        let page = node.read_history(id, generation, before, 4096).unwrap();
        assert!(!page.lines.is_empty(), "empty page before {before}");
        assert_eq!(page.first_line + page.lines.len() as u64, before);
        before = page.first_line;
        pages.push(page.lines);
    }
    pages
        .into_iter()
        .rev()
        .flatten()
        .map(|l| strip_ansi(&l))
        .collect()
}

/// The numbers of `#NNNNNN#` tokens, in order.
fn numbers(lines: &[String]) -> Vec<u64> {
    let mut out = Vec::new();
    for l in lines {
        let mut rest = l.as_str();
        while let Some(p) = rest.find('#') {
            let tok = &rest[p + 1..];
            if tok.len() >= 7
                && tok.as_bytes()[6] == b'#'
                && tok[..6].bytes().all(|b| b.is_ascii_digit())
            {
                out.push(tok[..6].parse().unwrap());
                rest = &tok[7..];
            } else {
                rest = tok;
            }
        }
    }
    out
}

/// Prints `n` numbered lines in colours; every 97th is 300+ chars (wraps).
fn numbered_lines_script(n: u32) -> String {
    format!(
        r#"awk 'BEGIN {{ pad = sprintf("%300s", ""); gsub(/ /, "-", pad);
            for (i = 0; i < {n}; i++) {{
                if (i % 97 == 0) printf "\033[3%dm#%06d#\033[0m %s\n", i % 8, i, pad;
                else printf "\033[3%dm#%06d#\033[1m bold\033[0m\n", i % 8, i;
            }} }}'"#
    )
}

#[test]
fn attach_tail_pages_and_screen_reconstruct_all_lines() {
    const N: u32 = 20_000;
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let id = spawn_sh(
        &node,
        &format!("{}; echo DONE; exec sleep 100", numbered_lines_script(N)),
    );
    let ok = attach_when_shown(&node, id, "DONE");
    assert!(
        ok.tail_first_line > ok.history_first_line,
        "tail should not cover all {N} lines: {} vs {}",
        ok.tail_first_line,
        ok.history_first_line
    );
    let mut text = read_all_before(
        &node,
        id,
        ok.history_generation,
        ok.tail_first_line,
        ok.history_first_line,
    );
    text.extend(ok.history_tail.iter().map(|l| strip_ansi(l)));
    text.extend(screen_text(&ok));
    assert_eq!(numbers(&text), (0..u64::from(N)).collect::<Vec<_>>());
    node.shutdown();
}

#[test]
fn history_survives_daemon_restart() {
    const N: u32 = 1_000;
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let node = Node::open(dir.path()).unwrap();
        let mut exits = node.exit_events();
        let id = spawn_sh(&node, &numbered_lines_script(N));
        assert_eq!(exits.blocking_recv().unwrap().agent_id, id);
        node.shutdown();
        id
    };
    let node = Node::open(dir.path()).unwrap();
    let (ok, _sub) = node.attach(id).unwrap();
    let next_line = ok.tail_first_line + ok.history_tail.len() as u64;
    assert!(next_line > 0, "no history after restart");
    let text = read_all_before(
        &node,
        id,
        ok.history_generation,
        next_line,
        ok.history_first_line,
    );
    let got = numbers(&text);
    // The final screen (at most 24 rows) is not history.
    assert!(got.len() as u32 >= N - 24, "{} numbers", got.len());
    assert_eq!(got, (0..got.len() as u64).collect::<Vec<_>>());
    // A direct read with a huge `before_line` is clamped to the end.
    let page = node
        .read_history(id, ok.history_generation, u64::MAX, 10)
        .unwrap();
    assert_eq!(page.first_line + page.lines.len() as u64, next_line);
}

#[test]
fn stale_generation_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let id = spawn_sh(
        &node,
        &format!(
            "{}; echo READY; read x; printf '\\033[3J'; echo CLEARED; exec sleep 100",
            numbered_lines_script(200)
        ),
    );
    let before = attach_when_shown(&node, id, "READY");
    node.read_history(
        id,
        before.history_generation,
        before.tail_first_line + 1,
        10,
    )
    .unwrap();
    node.input(id, b"\n").unwrap();
    let after = attach_when_shown(&node, id, "CLEARED");
    assert_ne!(after.history_generation, before.history_generation);
    let e = node
        .read_history(
            id,
            before.history_generation,
            before.tail_first_line + 1,
            10,
        )
        .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
    node.read_history(id, after.history_generation, u64::MAX, 10)
        .unwrap();
    node.shutdown();
}

fn history_dir(state: &Path, id: u64) -> std::path::PathBuf {
    state.join("history").join(id.to_string())
}

#[test]
fn evicted_exited_agent_history_is_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let mut exits = node.exit_events();
    let mut ids = Vec::new();
    for _ in 0..=RETAINED_EXITED {
        let id = spawn_sh(&node, &numbered_lines_script(100));
        assert_eq!(exits.blocking_recv().unwrap().agent_id, id);
        ids.push(id);
    }
    let (first, last) = (ids[0], *ids.last().unwrap());
    assert!(
        !history_dir(dir.path(), first).exists(),
        "evicted history kept"
    );
    assert!(node.read_history(first, 0, u64::MAX, 10).is_err());
    assert!(history_dir(dir.path(), last).exists());
    let page = node.read_history(last, 0, u64::MAX, 10).unwrap();
    assert_eq!(page.lines.len(), 10);
}

#[test]
fn open_deletes_unretained_history_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let node = Node::open(dir.path()).unwrap();
        let mut exits = node.exit_events();
        let id = spawn_sh(&node, &numbered_lines_script(100));
        assert_eq!(exits.blocking_recv().unwrap().agent_id, id);
        id
    };
    // A dir with no agent behind it (e.g. a crash mid-spawn).
    let stray = history_dir(dir.path(), 9_999);
    std::fs::create_dir_all(&stray).unwrap();
    let node = Node::open(dir.path()).unwrap();
    assert!(!stray.exists(), "stray history dir kept");
    assert!(history_dir(dir.path(), id).exists());
    node.read_history(id, 0, u64::MAX, 10).unwrap();
}

#[test]
fn rows_a_shrink_pushes_off_become_history() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let id = spawn_sh(
        &node,
        &format!("{}; echo DONE; exec sleep 100", numbered_lines_script(10)),
    );
    let before = attach_when_shown(&node, id, "DONE");
    assert!(before.history_tail.is_empty());
    node.resize(id, 80, 5).unwrap();
    let (after, _sub) = node.attach(id).unwrap();
    assert!(!after.history_tail.is_empty(), "shrink left no history");
    let mut text: Vec<String> = after.history_tail.iter().map(|l| strip_ansi(l)).collect();
    text.extend(screen_text(&after));
    assert_eq!(numbers(&text), (0..10).collect::<Vec<_>>());
    node.shutdown();
}

#[test]
fn blank_lines_keep_attach_bounded() {
    use prost::Message;
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let id = spawn_sh(&node, "yes '' | head -n 100000; echo DONE; exec sleep 100");
    let ok = attach_when_shown(&node, id, "DONE");
    assert!(
        ok.tail_first_line + ok.history_tail.len() as u64 >= 99_000,
        "blank lines did not reach history"
    );
    assert!(
        ok.history_tail.len() <= 4096,
        "{} tail lines",
        ok.history_tail.len()
    );
    let encoded = ok.encoded_len();
    assert!(
        encoded <= 256 * 1024 + 64 * 1024,
        "AttachOk is {encoded} bytes"
    );
    node.shutdown();
}
