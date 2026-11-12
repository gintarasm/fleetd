use fleet_node::{AgentId, CommandSpec, OutputSink, PtyHost};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn sh(script: &str) -> CommandSpec {
    CommandSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
        cwd: None,
        env: vec![],
        cols: 80,
        rows: 24,
    }
}

#[test]
fn counts_all_output() {
    let host = PtyHost::new();
    let id = host
        .spawn(sh("head -c 2000000 /dev/zero | tr \"\\0\" a"))
        .unwrap();
    host.wait_all(Duration::from_secs(10)).unwrap();
    let s = host.stats(id).unwrap();
    assert!(s.bytes_read >= 2_000_000, "got {}", s.bytes_read);
    assert_eq!(s.exit_code, Some(0));
}

#[test]
fn fifty_concurrent_agents() {
    let host = PtyHost::new();
    let ids: Vec<_> = (0..50)
        .map(|_| {
            host.spawn(sh("head -c 200000 /dev/zero | tr \"\\0\" a"))
                .unwrap()
        })
        .collect();
    host.wait_all(Duration::from_secs(30)).unwrap();
    for id in ids {
        let s = host.stats(id).unwrap();
        assert_eq!(s.exit_code, Some(0));
        assert_eq!(s.bytes_read, 200_000);
    }
}

#[test]
fn killed_agent_reports_exit() {
    let host = PtyHost::new();
    let id = host.spawn(sh("sleep 60")).unwrap();
    host.kill(id).unwrap();
    host.wait_all(Duration::from_secs(5)).unwrap();
    assert!(host.stats(id).unwrap().exit_code.is_some());
}

#[test]
fn nonzero_exit_is_reported() {
    let host = PtyHost::new();
    let id = host.spawn(sh("exit 3")).unwrap();
    host.wait_all(Duration::from_secs(5)).unwrap();
    assert_eq!(host.stats(id).unwrap().exit_code, Some(3));
}

#[test]
fn spawns_from_many_threads_at_once() {
    // macOS `ptsname` is not reentrant: concurrent opens must be serialised
    // inside PtyHost, so callers can spawn from any thread.
    let host = PtyHost::new();
    let ids: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..16)
            .map(|_| {
                s.spawn(|| {
                    (0..8)
                        .map(|_| host.spawn(sh("printf 0123456789")).unwrap())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    host.wait_all(Duration::from_secs(60)).unwrap();
    for id in ids {
        let s = host.stats(id).unwrap();
        assert_eq!(s.exit_code, Some(0));
        assert_eq!(s.bytes_read, 10);
    }
}

/// Collects everything an agent prints.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    /// Polls until the captured output contains `needle`.
    fn wait_for(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.text().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

impl OutputSink for Capture {
    fn on_output(&mut self, bytes: &[u8]) {
        self.0.lock().unwrap().extend_from_slice(bytes);
    }

    fn on_exit(&mut self, _code: i32) {}
}

fn spawn_captured(host: &PtyHost, spec: CommandSpec) -> (AgentId, Capture) {
    let capture = Capture::default();
    let id = host
        .spawn_with_sink(spec, Box::new(capture.clone()))
        .unwrap();
    (id, capture)
}

#[test]
fn sink_receives_output_and_exit() {
    #[derive(Clone, Default)]
    struct Exit(Arc<Mutex<Option<i32>>>);
    impl OutputSink for Exit {
        fn on_output(&mut self, _bytes: &[u8]) {}
        fn on_exit(&mut self, code: i32) {
            *self.0.lock().unwrap() = Some(code);
        }
    }
    let host = PtyHost::new();
    let exit = Exit::default();
    host.spawn_with_sink(sh("exit 7"), Box::new(exit.clone()))
        .unwrap();
    host.wait_all(Duration::from_secs(5)).unwrap();
    assert_eq!(*exit.0.lock().unwrap(), Some(7));

    let (id, capture) = spawn_captured(&host, sh("printf hello"));
    host.wait_all(Duration::from_secs(5)).unwrap();
    assert!(capture.text().contains("hello"), "{:?}", capture.text());
    assert_eq!(host.stats(id).unwrap().bytes_read, 5);
}

#[test]
fn input_reaches_agent() {
    let host = PtyHost::new();
    let (id, capture) = spawn_captured(&host, sh("stty -echo; cat"));
    // Wait until `stty -echo` has run so the echo below comes from `cat`.
    std::thread::sleep(Duration::from_millis(300));
    host.write_input(id, b"ping-from-input\n").unwrap();
    assert!(
        capture.wait_for("ping-from-input", Duration::from_secs(5)),
        "{:?}",
        capture.text()
    );
    host.kill(id).unwrap();
    host.wait_all(Duration::from_secs(5)).unwrap();
}

#[test]
fn resize_reaches_agent() {
    let host = PtyHost::new();
    let (id, capture) = spawn_captured(&host, sh("sleep 0.3; stty size"));
    host.resize(id, 100, 50).unwrap();
    host.wait_all(Duration::from_secs(5)).unwrap();
    assert!(capture.text().contains("50 100"), "{:?}", capture.text());
}

#[test]
fn kill_terminates_process_group() {
    let host = PtyHost::new();
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    // The background `sleep` ignores SIGHUP, so it survives the shell's death
    // (and the PTY hangup) unless the whole process group is signalled.
    let script = format!(
        "trap '' HUP; sleep 30 & echo $! > {}; wait",
        pidfile.display()
    );
    let id = host.spawn(sh(&script)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&pidfile)
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "agent never wrote its pid");
        std::thread::sleep(Duration::from_millis(10));
    };
    host.kill(id).unwrap();
    host.wait_all(Duration::from_secs(5)).unwrap();
    assert!(host.stats(id).unwrap().exit_code.is_some());
    let deadline = Instant::now() + Duration::from_secs(2);
    // SAFETY: signal 0 only checks that the process exists.
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "background child {pid} survived kill"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn input_and_resize_on_unknown_or_finished_agent_fail() {
    let host = PtyHost::new();
    let id = host.spawn(sh("exit 0")).unwrap();
    host.wait_all(Duration::from_secs(5)).unwrap();
    assert!(host.write_input(id, b"x").is_err());
    assert!(host.resize(id, 10, 10).is_err());
    let unknown = AgentId(u64::MAX);
    assert!(host.write_input(unknown, b"x").is_err());
    assert!(host.kill(unknown).is_err());
}

#[test]
fn remove_drops_finished_agent_only() {
    let host = PtyHost::new();
    let running = host.spawn(sh("sleep 60")).unwrap();
    assert!(
        host.remove(running).is_err(),
        "running agent must not be removed"
    );
    host.kill(running).unwrap();
    host.wait_all(Duration::from_secs(5)).unwrap();
    host.remove(running).unwrap();
    assert!(host.stats(running).is_none());
    assert!(host.remove(running).is_err());
}
