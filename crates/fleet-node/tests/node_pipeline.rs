use fleet_node::node::{AgentPipeline, StreamItem, Subscription};
use fleet_node::scrollback::Scrollback;
use fleet_node::{AgentId, CommandSpec, PtyHost, Terminal};
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
    let id = f.spawn("head -c 4194304 /dev/zero | tr '\\0' a");
    // The agent completes although the subscriber never reads.
    f.wait();
    let stats = f.host.stats(id).unwrap();
    assert_eq!(stats.exit_code, Some(0));
    assert!(stats.bytes_read >= 4 * 1024 * 1024);
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
    std::thread::sleep(Duration::from_millis(500));
    let (snapshot, _sub) = f.pipeline.subscribe();
    assert_eq!(render(&snapshot).visible_text()[0], "stalled frame");
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
    std::thread::sleep(Duration::from_millis(500));
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
