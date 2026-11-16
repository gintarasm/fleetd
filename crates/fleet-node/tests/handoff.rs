//! Node handoff: one node freezes its agents and hands them to a second node
//! opened on the same state dir, as a restarted daemon would after `exec`.
//!
//! In-process the first node's PTY fds stay open (its frozen readers park
//! forever and keep them), so each handed-over fd is a `dup` the second node
//! may own. Dropping the first node signals nothing: its agents keep running.

use fleet_node::Terminal;
use fleet_node::history::History;
use fleet_node::node::{AgentPipeline, ConnId, Node, StreamItem, Subscription};
use fleet_node::scrollback::Scrollback;
use fleet_node::store::{Labels, NewAgent, Store};
use fleet_node::{CommandSpec, PtyHost};
use fleet_proto::handoff::HandoffAgent;
use fleet_proto::handoff::handoff_agent::Terminal as HandoffTerminal;
use fleet_proto::{AgentState, AttachOk, SpawnAgent};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(3);

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

fn screen_text(ok: &AttachOk) -> Vec<String> {
    let mut term = Terminal::new(ok.cols as u16, ok.rows as u16);
    term.feed(&ok.snapshot);
    term.visible_text()
}

fn screen(node: &Node, id: u64) -> Vec<String> {
    screen_text(&node.attach(id, ConnId(1)).unwrap().0)
}

/// Polls the agent's screen until a row satisfies `shown`.
fn wait_screen(node: &Node, id: u64, what: &str, shown: impl Fn(&str) -> bool) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let text = screen(node, id);
        if text.iter().any(|l| shown(l)) {
            return text;
        }
        assert!(Instant::now() < deadline, "{what} never shown: {text:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_line(node: &Node, id: u64, line: &str) -> Vec<String> {
    wait_screen(node, id, line, |l| l == line)
}

fn state_of(node: &Node, id: u64) -> (i32, Option<i32>) {
    let info = node
        .list()
        .unwrap()
        .into_iter()
        .find(|a| a.id == id)
        .unwrap();
    (info.state, info.exit_code)
}

fn wait_exited(node: &Node, id: u64) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(code) = node.exit_code(id) {
            return code;
        }
        assert!(Instant::now() < deadline, "agent {id} never exited");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The record as the new node gets it in-process: each fd a duplicate it
/// may own (the first node's frozen reader keeps the original).
fn dup_fds(mut agents: Vec<HandoffAgent>) -> Vec<HandoffAgent> {
    for a in &mut agents {
        if let Some(HandoffTerminal::Fd(fd)) = a.terminal {
            // SAFETY: dup of an fd the first node keeps open.
            let copy = unsafe { libc::dup(fd) };
            assert!(copy >= 0, "dup failed");
            a.terminal = Some(HandoffTerminal::Fd(copy));
        }
    }
    agents
}

/// Freezes `old` and opens a second node on `dir` from its record.
fn hand_over(old: Node, dir: &Path) -> (Node, Vec<u64>, Vec<u64>) {
    let agents = old.prepare_handoff(Instant::now() + DEADLINE).unwrap();
    drop(old);
    Node::open_with_handoff(dir, dup_fds(agents)).unwrap()
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 only checks the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Prints `1..=n`, one per line, in batches of 100 with a pause between,
/// then exits.
fn numbered_writer(n: u32) -> String {
    format!(
        "i=0; while [ $i -lt {n} ]; do i=$((i+1)); echo $i; \
         if [ $((i % 100)) -eq 0 ]; then sleep 0.02; fi; done"
    )
}

/// The numbers in `text`, one per line (blank lines skipped).
fn numbers(text: &str) -> Vec<u32> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.parse().unwrap_or_else(|_| panic!("not a number: {l:?}")))
        .collect()
}

/// A row showing a number of at least 200: a few batches are out.
fn past_200(line: &str) -> bool {
    line.parse::<u32>().is_ok_and(|n| n >= 200)
}

fn ring_numbers(dir: &Path, id: u64) -> Vec<u32> {
    let ring = Scrollback::open(&dir.join("scrollback").join(format!("{id}.ring"))).unwrap();
    let tail = ring.read_tail(usize::MAX).unwrap();
    numbers(&String::from_utf8(tail).unwrap())
}

/// Every history line, oldest first.
fn history_numbers(node: &Node, id: u64) -> Vec<u32> {
    let (ok, _) = node.attach(id, ConnId(1)).unwrap();
    let mut pages = Vec::new();
    let mut before = u64::MAX;
    loop {
        let page = node
            .read_history(id, ok.history_generation, before, 4096)
            .unwrap();
        if page.lines.is_empty() {
            break;
        }
        before = page.first_line;
        pages.push(page.lines);
    }
    let lines: Vec<String> = pages
        .into_iter()
        .rev()
        .flatten()
        .map(|l| String::from_utf8(l.to_vec()).unwrap())
        .collect();
    numbers(&lines.join("\n"))
}

fn assert_consecutive(what: &str, got: &[u32], first: u32, last: u32) {
    let want: Vec<u32> = (first..=last).collect();
    if got != want {
        let gap = got.windows(2).find(|w| w[1] != w[0] + 1);
        panic!(
            "{what}: {} numbers, first {:?}, last {:?}, first break {gap:?}",
            got.len(),
            got.first(),
            got.last()
        );
    }
}

#[test]
fn handed_over_agent_keeps_running_with_its_screen() {
    let dir = tempfile::tempdir().unwrap();
    let old = Node::open(dir.path()).unwrap();
    let id = spawn_sh(&old, "echo ready; while read l; do eval \"$l\"; done");
    let before = wait_for_line(&old, id, "ready");
    let (new, kept, failed) = hand_over(old, dir.path());
    assert_eq!(kept, vec![id]);
    assert!(failed.is_empty());
    assert_eq!(state_of(&new, id).0, AgentState::Running as i32);
    assert_eq!(screen(&new, id), before);
    new.input(id, ConnId(1), b"echo again\n").unwrap();
    wait_for_line(&new, id, "again");
    new.shutdown();
}

#[test]
fn numbered_output_is_exact_across_handoff() {
    const N: u32 = 20_000;
    let dir = tempfile::tempdir().unwrap();
    let old = Node::open(dir.path()).unwrap();
    let id = spawn_sh(&old, &numbered_writer(N));
    wait_screen(&old, id, "output", past_200);
    let (new, kept, _) = hand_over(old, dir.path());
    assert_eq!(kept, vec![id], "handed over mid-stream");
    assert_eq!(wait_exited(&new, id), 0);
    // The ring holds every byte; history every row that scrolled off, the
    // last screen's rows (the final 23 numbers and the prompt row) aside.
    assert_consecutive("scrollback ring", &ring_numbers(dir.path(), id), 1, N);
    let history = history_numbers(&new, id);
    assert_consecutive("history", &history, 1, *history.last().unwrap());
    assert!(*history.last().unwrap() >= N - 24, "history ends early");
}

#[test]
fn alt_screen_agent_keeps_both_screens() {
    let dir = tempfile::tempdir().unwrap();
    let old = Node::open(dir.path()).unwrap();
    let id = spawn_sh(
        &old,
        "printf 'primary\\n'; printf '\\033[?1049h'; printf 'alt\\n'; \
         read x; printf '\\033[?1049l'; read y",
    );
    wait_for_line(&old, id, "alt");
    let (new, kept, _) = hand_over(old, dir.path());
    assert_eq!(kept, vec![id]);
    let text = screen(&new, id);
    assert!(text.iter().any(|l| l == "alt"), "{text:?}");
    assert!(!text.iter().any(|l| l == "primary"), "{text:?}");
    new.input(id, ConnId(1), b"\n").unwrap();
    let text = wait_for_line(&new, id, "primary");
    assert!(!text.iter().any(|l| l == "alt"), "{text:?}");
    new.shutdown();
}

#[test]
fn restore_failure_ends_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let old = Node::open(dir.path()).unwrap();
    let id = spawn_sh(&old, "echo ready; exec sleep 100");
    wait_for_line(&old, id, "ready");
    let mut agents = old.prepare_handoff(Instant::now() + DEADLINE).unwrap();
    drop(old);
    let pid = agents[0].pid;
    assert!(pid_alive(pid));
    agents[0].terminal = Some(HandoffTerminal::Fd(-1));
    let (new, kept, failed) = Node::open_with_handoff(dir.path(), agents).unwrap();
    assert!(kept.is_empty());
    assert_eq!(failed, vec![id]);
    assert_eq!(state_of(&new, id), (AgentState::Exited as i32, Some(-1)));
    assert!(!pid_alive(pid), "the agent's process is gone");
}

#[test]
fn thaw_resumes_after_prepare() {
    const N: u32 = 5_000;
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let id = spawn_sh(&node, &numbered_writer(N));
    wait_screen(&node, id, "output", past_200);
    let agents = node.prepare_handoff(Instant::now() + DEADLINE).unwrap();
    assert_eq!(agents.len(), 1);
    std::thread::sleep(Duration::from_millis(100));
    node.thaw();
    node.thaw(); // idempotent
    assert_eq!(wait_exited(&node, id), 0);
    assert_consecutive("scrollback ring", &ring_numbers(dir.path(), id), 1, N);
    let again = spawn_sh(&node, "exit 0");
    assert_eq!(wait_exited(&node, again), 0);
}

#[test]
fn spawn_during_handoff_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let running = spawn_sh(&node, "exec sleep 100");
    let exited = spawn_sh(&node, "exit 0");
    wait_exited(&node, exited);
    node.prepare_handoff(Instant::now() + DEADLINE).unwrap();
    let e = node
        .spawn(SpawnAgent {
            program: "/bin/sh".into(),
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(e.to_string(), "fleetd is restarting");
    let e = node.restart(exited, 0, 0, None).unwrap_err();
    assert_eq!(e.to_string(), "fleetd is restarting");
    let e = node
        .forget(&[exited], false, &Labels::new(), None)
        .unwrap_err();
    assert_eq!(e.to_string(), "fleetd is restarting");
    let e = node.prepare_handoff(Instant::now() + DEADLINE).unwrap_err();
    assert_eq!(e.to_string(), "a restart is already in progress");
    node.thaw();
    assert_eq!(state_of(&node, running).0, AgentState::Running as i32);
    node.forget(&[exited], false, &Labels::new(), None).unwrap();
    node.shutdown();
}

#[test]
fn failed_freeze_leaves_the_node_serving() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    let id = spawn_sh(&node, "echo ready; while read l; do eval \"$l\"; done");
    wait_for_line(&node, id, "ready");
    // A deadline already past: the reader cannot have parked in time.
    let e = node.prepare_handoff(Instant::now()).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
    node.input(id, ConnId(1), b"echo again\n").unwrap();
    wait_for_line(&node, id, "again");
    let other = spawn_sh(&node, "exit 0");
    assert_eq!(wait_exited(&node, other), 0);
    node.shutdown();
}

#[test]
fn other_running_rows_are_still_marked_lost() {
    let dir = tempfile::tempdir().unwrap();
    let old = Node::open(dir.path()).unwrap();
    let id = spawn_sh(&old, "exec sleep 100");
    // A row left running by an earlier daemon that died.
    let stale = Store::open(&dir.path().join("state.db"))
        .unwrap()
        .insert_agent(&NewAgent {
            program: "/bin/sh".into(),
            args: vec![],
            cwd: "/".into(),
            created_unix_ms: 1,
            scrollback_path: String::new(),
            labels: Labels::new(),
            priority: 0,
        })
        .unwrap();
    let (new, kept, _) = hand_over(old, dir.path());
    assert_eq!(kept, vec![id]);
    assert_eq!(state_of(&new, id).0, AgentState::Running as i32);
    assert_eq!(state_of(&new, stale).0, AgentState::Lost as i32);
    new.shutdown();
}

/// Receives one item, giving up after `timeout` (`None`).
fn recv(sub: &mut Subscription, timeout: Duration) -> Option<Option<StreamItem>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async { tokio::time::timeout(timeout, sub.recv()).await })
        .ok()
}

#[test]
fn restored_screen_reaches_only_the_grid() {
    let dir = tempfile::tempdir().unwrap();
    let ring_path = dir.path().join("ring");
    let ring = Scrollback::create(&ring_path, 1024 * 1024).unwrap();
    let history = History::create(&dir.path().join("history")).unwrap();
    let pipeline = AgentPipeline::with_history(80, 24, ring, Some(history));
    let (_, mut sub) = pipeline.subscribe();
    // 30 lines scroll 7 rows off a 24-row grid; then a cursor position query.
    let lines: String = (1..=30).map(|i| format!("line{i}\r\n")).collect();
    pipeline.restore_screen(format!("{lines}\x1b[6n").as_bytes());
    assert_eq!(pipeline.visible_text()[0], "line8");
    assert_eq!(
        recv(&mut sub, Duration::from_millis(100)),
        None,
        "subscriber got it"
    );
    let page = pipeline.read_history(0, u64::MAX, 100).unwrap().unwrap();
    assert!(page.lines.is_empty(), "history got {:?}", page.lines);
    drop(sub);
    // The query's reply is not owed to the agent: the agent reads the
    // first six bytes it is sent, which must be the stand-in's.
    let host = PtyHost::new();
    let agent = host
        .spawn_with_sink(
            CommandSpec {
                program: PathBuf::from("/bin/sh"),
                args: vec![
                    "-c".into(),
                    "stty raw -echo; printf go; dd bs=1 count=6 2>/dev/null | tr '\\033' E; \
                     stty sane"
                        .into(),
                ],
                cwd: None,
                env: vec![],
                cols: 80,
                rows: 24,
                nice: 0,
            },
            Box::new(pipeline.clone()),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pipeline.visible_text().iter().any(|l| l.contains("go")) {
        assert!(Instant::now() < deadline, "agent never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    host.write_input(agent, b"Zz;zzR").unwrap();
    host.wait_all(Duration::from_secs(10)).unwrap();
    let text = pipeline.visible_text().join("\n");
    assert!(text.contains("goZz;zzR"), "{text:?}");
    let ring = Scrollback::open(&ring_path).unwrap();
    let ring = String::from_utf8(ring.read_tail(usize::MAX).unwrap()).unwrap();
    assert!(!ring.contains("line"), "the ring got the restored screen");
}

#[test]
fn agent_past_its_output_but_running_blocks_the_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    // Closes its terminal and keeps running: its reader sees the output end
    // and then waits in `waitpid` for as long as it runs. Perl, not sh:
    // macOS's /bin/sh (bash 3.2) keeps the terminal referenced after
    // redirecting every fd, so the output would never end.
    let id = node
        .spawn(SpawnAgent {
            program: "perl".into(),
            args: vec![
                "-e".into(),
                "print \"ready\\n\"; open STDIN, '</dev/null'; open STDOUT, '>/dev/null'; \
                 open STDERR, '>/dev/null'; sleep 1000"
                    .into(),
            ],
            cols: 80,
            rows: 24,
            ..Default::default()
        })
        .unwrap()
        .id;
    wait_for_line(&node, id, "ready");
    // Until the reader has seen the output end, the agent is frozen and
    // handed over like any other: thaw and try again.
    let deadline = Instant::now() + Duration::from_secs(30);
    let e = loop {
        match node.prepare_handoff(Instant::now() + Duration::from_millis(200)) {
            Ok(_) => node.thaw(),
            Err(e) => break e,
        }
        assert!(Instant::now() < deadline, "the agent's output never ended");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(e.to_string(), "an agent is exiting; try again");
    // Still serving, the agent still tracked.
    assert_eq!(state_of(&node, id).0, AgentState::Running as i32);
    let other = spawn_sh(&node, "exit 0");
    assert_eq!(wait_exited(&node, other), 0);
    node.kill(id).unwrap();
    wait_exited(&node, id);
}

#[test]
fn agents_exiting_around_the_freeze_record_their_codes() {
    let dir = tempfile::tempdir().unwrap();
    let old = Node::open(dir.path()).unwrap();
    // Staggered exits: some end before the freeze (retired by the old node
    // while it waits), some as it freezes (handed over and reaped by the new
    // node), whichever the timing.
    let ids: Vec<u64> = (0..6)
        .map(|i| spawn_sh(&old, &format!("echo ready; sleep 0.0{i}; exit 7")))
        .collect();
    let (new, _, failed) = hand_over(old, dir.path());
    assert!(failed.is_empty());
    for id in ids {
        assert_eq!(wait_exited(&new, id), 7, "agent {id}");
        assert_eq!(state_of(&new, id), (AgentState::Exited as i32, Some(7)));
    }
}

/// The file `fd` is open on, as (device, inode); `None` if it is closed.
fn fd_file(fd: i32) -> Option<(u64, u64)> {
    // SAFETY: fstat with a valid out-pointer; it fails on a closed fd.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(fd, &mut st) } == 0).then_some((st.st_dev as u64, st.st_ino as u64))
}

#[test]
fn record_naming_a_non_terminal_fd_leaves_it_open() {
    let dir = tempfile::tempdir().unwrap();
    let old = Node::open(dir.path()).unwrap();
    let id = spawn_sh(&old, "echo ready; exec sleep 100");
    wait_for_line(&old, id, "ready");
    let mut agents = old.prepare_handoff(Instant::now() + DEADLINE).unwrap();
    drop(old);
    let pid = agents[0].pid;
    // One of the daemon's own files, say its log.
    let file = std::fs::File::create(dir.path().join("own.log")).unwrap();
    let own = std::os::fd::AsRawFd::as_raw_fd(&file);
    let before = fd_file(own);
    agents[0].terminal = Some(HandoffTerminal::Fd(own));
    let (new, kept, failed) = Node::open_with_handoff(dir.path(), agents).unwrap();
    assert!(kept.is_empty());
    assert_eq!(failed, vec![id]);
    assert_eq!(fd_file(own), before, "the daemon's file was closed");
    assert_eq!(state_of(&new, id), (AgentState::Exited as i32, Some(-1)));
    assert!(!pid_alive(pid));
    drop(file);
}

#[test]
fn record_naming_one_fd_twice_claims_it_once() {
    let dir = tempfile::tempdir().unwrap();
    let old = Node::open(dir.path()).unwrap();
    let a = spawn_sh(&old, "echo ready; while read l; do eval \"$l\"; done");
    let b = spawn_sh(&old, "echo ready; exec sleep 100");
    wait_for_line(&old, a, "ready");
    wait_for_line(&old, b, "ready");
    let mut agents = dup_fds(old.prepare_handoff(Instant::now() + DEADLINE).unwrap());
    drop(old);
    agents.sort_by_key(|x| x.id);
    let b_pid = agents[1].pid;
    agents[1].terminal = agents[0].terminal.clone();
    let (new, kept, failed) = Node::open_with_handoff(dir.path(), agents).unwrap();
    assert_eq!(kept, vec![a]);
    assert_eq!(failed, vec![b]);
    assert!(!pid_alive(b_pid));
    new.input(a, ConnId(1), b"echo still\n").unwrap();
    wait_for_line(&new, a, "still");
    new.shutdown();
}

#[test]
fn record_repeating_an_id_registers_it_once() {
    let dir = tempfile::tempdir().unwrap();
    let old = Node::open(dir.path()).unwrap();
    let id = spawn_sh(&old, "echo ready; while read l; do eval \"$l\"; done");
    wait_for_line(&old, id, "ready");
    let first = old.prepare_handoff(Instant::now() + DEADLINE).unwrap();
    drop(old);
    let copy = dup_fds(first.clone());
    let pid = first[0].pid;
    let mut agents = dup_fds(first);
    agents.extend(copy);
    let (new, kept, failed) = Node::open_with_handoff(dir.path(), agents).unwrap();
    assert_eq!(kept, vec![id]);
    assert!(failed.is_empty());
    assert!(pid_alive(pid), "the kept agent was ended");
    assert_eq!(state_of(&new, id).0, AgentState::Running as i32);
    let listed = new.list().unwrap();
    assert_eq!(listed.iter().filter(|x| x.id == id).count(), 1);
    new.input(id, ConnId(1), b"echo once\n").unwrap();
    wait_for_line(&new, id, "once");
    new.shutdown();
    assert_eq!(state_of(&new, id).0, AgentState::Exited as i32);
}
