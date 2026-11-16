use fleet_node::node::{AgentPipeline, ConnId, StreamItem, Subscription};
use fleet_node::scrollback::Scrollback;
use fleet_node::{AgentId, CommandSpec, OutputSink, PtyHost, Terminal};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const COLS: u16 = 80;
const ROWS: u16 = 24;

fn sh(script: &str) -> CommandSpec {
    CommandSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
        cwd: None,
        env: vec![],
        cols: COLS,
        rows: ROWS,
        nice: 0,
    }
}

struct Fixture {
    host: PtyHost,
    pipeline: AgentPipeline,
    scrollback_path: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let scrollback_path = dir.path().join("scrollback");
        let scrollback = Scrollback::create(&scrollback_path, 1024 * 1024).unwrap();
        Self {
            host: PtyHost::new(),
            pipeline: AgentPipeline::new(COLS, ROWS, scrollback),
            scrollback_path,
            _dir: dir,
        }
    }

    fn spawn(&self, script: &str) -> AgentId {
        self.host
            .spawn_with_sink(sh(script), Box::new(self.pipeline.clone()))
            .unwrap()
    }

    fn wait(&self) {
        self.host.wait_all(Duration::from_secs(30)).unwrap();
    }

    /// Polls the pipeline's grid until some row contains `needle`.
    fn wait_for_text(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self
                .pipeline
                .visible_text()
                .iter()
                .any(|l| l.contains(needle))
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

/// Receives one item, giving up after `timeout` (`Err(())`).
fn recv(sub: &mut Subscription, timeout: Duration) -> Result<Option<StreamItem>, ()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async { tokio::time::timeout(timeout, sub.recv()).await })
        .map_err(|_| ())
}

/// Collects output until `Exited`; panics on `Lagged` or a stall.
fn drain_until_exit(sub: &mut Subscription) -> (Vec<u8>, i32) {
    let mut out = Vec::new();
    loop {
        match recv(sub, Duration::from_secs(10)).expect("stream stalled") {
            Some(StreamItem::Output(bytes)) => out.extend_from_slice(&bytes),
            Some(StreamItem::Exited(code)) => return (out, code),
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn render(bytes: &[u8]) -> Terminal {
    let mut t = Terminal::new(COLS, ROWS);
    t.feed(bytes);
    t
}

#[test]
fn subscribe_gets_snapshot_then_stream_without_gap() {
    let f = Fixture::new();
    let id = f.spawn("printf 'A-line\\n'; read x; printf 'B-line\\n'");
    assert!(f.wait_for_text("A-line", Duration::from_secs(5)));

    let (snapshot, mut sub) = f.pipeline.subscribe();
    let snap_text = render(&snapshot).visible_text().join("\n");
    assert!(snap_text.contains("A-line"), "{snap_text}");
    assert!(!snap_text.contains("B-line"), "{snap_text}");

    // `read` gets the line; the tty echoes it, then the agent prints B.
    std::thread::sleep(Duration::from_millis(100));
    f.host.write_input(id, b"go\n").unwrap();
    f.wait();
    let (stream, code) = drain_until_exit(&mut sub);
    assert_eq!(code, 0);
    let stream_text = String::from_utf8_lossy(&stream);
    assert!(stream_text.contains("B-line"), "{stream_text:?}");
    assert!(!stream_text.contains("A-line"), "{stream_text:?}");

    let mut replay = snapshot.clone();
    replay.extend_from_slice(&stream);
    let client = render(&replay);
    assert_eq!(client.visible_text(), f.pipeline.visible_text());
    assert_eq!(client.cursor(), f.pipeline.cursor());
}

#[test]
fn every_subscriber_gets_the_full_stream_and_exit() {
    let f = Fixture::new();
    let (_, mut first) = f.pipeline.subscribe();
    let (_, mut second) = f.pipeline.subscribe();
    f.spawn("printf 'hello fleet\\n'; exit 4");
    f.wait();
    for sub in [&mut first, &mut second] {
        let (out, code) = drain_until_exit(sub);
        assert_eq!(code, 4);
        assert!(String::from_utf8_lossy(&out).contains("hello fleet"));
        assert_eq!(recv(sub, Duration::from_secs(1)), Ok(None));
    }

    // on_exit flushed the history to disk.
    let tail = Scrollback::open(&f.scrollback_path)
        .unwrap()
        .read_tail(1024)
        .unwrap();
    assert!(String::from_utf8_lossy(&tail).contains("hello fleet"));

    // A late subscriber gets the final screen and the exit at once.
    let (snapshot, mut late) = f.pipeline.subscribe();
    assert!(render(&snapshot).visible_text()[0].contains("hello fleet"));
    assert_eq!(
        recv(&mut late, Duration::from_secs(1)),
        Ok(Some(StreamItem::Exited(4)))
    );
    assert_eq!(recv(&mut late, Duration::from_secs(1)), Ok(None));
}

#[test]
fn slow_subscriber_is_dropped_not_blocking() {
    let f = Fixture::new();
    let (_, mut stalled) = f.pipeline.subscribe();
    // 16 MiB: four times what a subscription can queue (256 chunks merged
    // up to 16 KiB). Each line homes the cursor, so nothing scrolls.
    let id = f.spawn(&format!(
        "yes \"$(printf '\x1b[H{}')\" | head -c 16777216",
        "x".repeat(900)
    ));
    // The agent completes although the subscriber never reads.
    f.wait();
    let stats = f.host.stats(id).unwrap();
    assert_eq!(stats.exit_code, Some(0));
    assert!(stats.bytes_read >= 16 * 1024 * 1024);
    assert_eq!(
        recv(&mut stalled, Duration::from_secs(1)),
        Ok(Some(StreamItem::Lagged))
    );
    assert_eq!(recv(&mut stalled, Duration::from_secs(1)), Ok(None));
}

#[test]
fn stalled_synchronized_update_shows_in_snapshot() {
    let f = Fixture::new();
    // BSU without ESU: the frame must appear once the sync timeout expires.
    let id = f.spawn("printf '\\033[?2026hstalled frame'; sleep 2");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (snapshot, _sub) = f.pipeline.subscribe();
        if render(&snapshot).visible_text()[0] == "stalled frame" {
            break;
        }
        assert!(Instant::now() < deadline, "frame never shown");
        std::thread::sleep(Duration::from_millis(10));
    }
    f.host.kill(id).unwrap();
    f.wait();
}

/// Asks for the cursor position and prints the 6-byte answer it reads, with
/// ESC shown as `E`.
const CPR_AGENT: &str =
    "stty raw -echo; printf '\\033[6n'; dd bs=1 count=6 2>/dev/null | tr '\\033' E; stty sane";

#[test]
fn query_is_answered_while_no_subscriber_is_attached() {
    let f = Fixture::new();
    // A subscriber that went away does not count as attached.
    drop(f.pipeline.subscribe());
    let id = f.spawn(CPR_AGENT);
    let answered = f.wait_for_text("E[1;1R", Duration::from_secs(5));
    if !answered {
        f.host.kill(id).unwrap();
    }
    f.wait();
    assert!(answered, "{:?}", f.pipeline.visible_text());
}

#[test]
fn query_is_left_to_the_client_while_subscribed() {
    let f = Fixture::new();
    let (_, _sub) = f.pipeline.subscribe();
    let id = f.spawn(CPR_AGENT);
    // Wait until the query (4 bytes, the agent's only output) went through.
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.host.stats(id).unwrap().bytes_read < 4 {
        assert!(Instant::now() < deadline, "query never arrived");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Stand in for the attached client's terminal answering.
    f.host.write_input(id, b"Zz;zzR").unwrap();
    let got_client_answer = f.wait_for_text("Zz;zzR", Duration::from_secs(5));
    if !got_client_answer {
        f.host.kill(id).unwrap();
    }
    f.wait();
    assert!(got_client_answer, "{:?}", f.pipeline.visible_text());
    let text = f.pipeline.visible_text().join("\n");
    assert!(!text.contains("E[1;1R"), "pipeline answered: {text:?}");
}

#[test]
fn agent_flooding_queries_does_not_block_the_reader() {
    let f = Fixture::new();
    // Never reads its input, so unanswered replies pile up in the tty.
    let id = f.spawn("stty raw -echo; while :; do printf '\\033[6n\\033[c'; done");
    let read_after = |delay: u64| {
        std::thread::sleep(Duration::from_millis(delay));
        f.host.stats(id).unwrap().bytes_read
    };
    let early = read_after(1000);
    let late = read_after(500);
    f.host.kill(id).unwrap();
    f.wait();
    assert!(late > early, "reader stalled at {early} bytes");
}

fn pipeline() -> (AgentPipeline, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let scrollback = Scrollback::create(&dir.path().join("scrollback"), 1024 * 1024).unwrap();
    (AgentPipeline::new(COLS, ROWS, scrollback), dir)
}

/// Feeds `before`, attaches, feeds `after` and exits; the attached client's
/// replay must equal the pipeline's own screen (text, pen, modes, cursor).
fn assert_attach_has_no_gap(before: &[&[u8]], after: &[u8]) {
    let (p, _dir) = pipeline();
    let mut sink = p.clone();
    for chunk in before {
        sink.on_output(chunk);
    }
    let (snapshot, mut sub) = p.subscribe();
    sink.on_output(after);
    sink.on_exit(0);
    let (stream, _) = drain_until_exit(&mut sub);
    let mut replay = snapshot;
    replay.extend_from_slice(&stream);
    let client = render(&replay);
    let (expected, _) = p.subscribe();
    assert_eq!(
        String::from_utf8_lossy(&client.snapshot()),
        String::from_utf8_lossy(&expected),
        "before {before:?}, after {after:?}"
    );
}

#[test]
fn attach_inside_synchronized_update_has_no_gap() {
    assert_attach_has_no_gap(&[b"before\r\n\x1b[?2026hpart1 "], b"part2\x1b[?2026l");
    assert_attach_has_no_gap(&[b"\x1b[?2026h\x1b[?25l\x1b[1mbold"], b" more\x1b[?2026l");
}

#[test]
fn attach_inside_split_sequence_has_no_gap() {
    // CSI split before its final byte.
    assert_attach_has_no_gap(&[b"red:\x1b[3"], b"1mX\x1b[0m");
    assert_attach_has_no_gap(&[b"esc:\x1b"], b"[1mB");
    // 3-byte UTF-8 character split after two bytes.
    assert_attach_has_no_gap(&[b"euro:\xe2\x82"], b"\xacZ");
    // OSC title split mid-payload, across chunks, BEL- and ST-terminated.
    assert_attach_has_no_gap(&[b"x\x1b]0;ti", b"t"], b"le\x07after");
    assert_attach_has_no_gap(&[b"x\x1b]0;title\x1b"], b"\\after");
    // DCS split mid-payload.
    assert_attach_has_no_gap(&[b"x\x1bP+q54"], b"4e\x1b\\after");
}

/// The next item, which must arrive within a second.
fn next_item(sub: &mut Subscription) -> Option<StreamItem> {
    recv(sub, Duration::from_secs(1)).expect("no item")
}

fn nothing_queued(sub: &mut Subscription) -> bool {
    recv(sub, Duration::from_millis(50)).is_err()
}

#[test]
fn roles_follow_attach_input_and_departure() {
    let f = Fixture::new();
    let agent = f.spawn("sleep 100");
    let p = &f.pipeline;
    let (a, b, c) = (ConnId(1), ConnId(2), ConnId(3));
    // No connection attached: anyone may resize.
    assert!(p.may_resize(a));

    let mut sa = p.attach(a);
    assert!(sa.primary);
    let mut sb = p.attach(b);
    assert!(sb.primary);
    assert_eq!(
        next_item(&mut sa.subscription),
        Some(StreamItem::Role(false))
    );
    assert!(p.may_resize(b) && !p.may_resize(a) && !p.may_resize(c));

    // Anonymous subscribers take no part.
    let (_, mut anon) = p.subscribe();
    assert!(nothing_queued(&mut sb.subscription));

    // Input from an unattached connection changes nothing; from A, it moves.
    p.input_from(c);
    assert!(nothing_queued(&mut sb.subscription));
    p.input_from(a);
    assert_eq!(
        next_item(&mut sa.subscription),
        Some(StreamItem::Role(true))
    );
    assert_eq!(
        next_item(&mut sb.subscription),
        Some(StreamItem::Role(false))
    );
    p.input_from(a);
    assert!(nothing_queued(&mut sa.subscription));

    // A primary re-attaching stays primary without role events.
    let mut sa2 = p.attach(a);
    assert!(sa2.primary);
    assert_eq!(next_item(&mut sa.subscription), None, "old stream ended");
    assert!(nothing_queued(&mut sb.subscription));

    // The primary leaving promotes the most recent remaining connection.
    let mut sc = p.attach(c);
    assert_eq!(
        next_item(&mut sa2.subscription),
        Some(StreamItem::Role(false))
    );
    p.input_from(b);
    assert_eq!(
        next_item(&mut sb.subscription),
        Some(StreamItem::Role(true))
    );
    assert_eq!(
        next_item(&mut sc.subscription),
        Some(StreamItem::Role(false))
    );
    drop(sb);
    assert_eq!(
        next_item(&mut sc.subscription),
        Some(StreamItem::Role(true))
    );
    drop(sc);
    assert_eq!(
        next_item(&mut sa2.subscription),
        Some(StreamItem::Role(true))
    );
    drop(sa2);
    assert!(nothing_queued(&mut anon));
    assert!(p.may_resize(c), "no connection attached any more");
    f.host.force_kill(agent).unwrap();
    f.wait();
}
