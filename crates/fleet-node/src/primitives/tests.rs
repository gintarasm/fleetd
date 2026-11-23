use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::*;

const GRACE: Duration = Duration::from_millis(300);

fn req(argv: &[&str]) -> ExecRequest {
    ExecRequest {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        cwd: std::env::temp_dir(),
        ..Default::default()
    }
}

fn sh(script: &str) -> ExecRequest {
    req(&["/bin/sh", "-c", script])
}

fn go(r: &ExecRequest) -> io::Result<ExecResult> {
    run(r, None, &AtomicBool::new(false), GRACE)
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_dead(pid: i32) -> bool {
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn captures_output_streams_and_exit_code() {
    let r = go(&sh("echo out; echo err >&2; exit 3")).unwrap();
    assert_eq!(r.stdout, b"out\n");
    assert_eq!(r.stderr, b"err\n");
    assert_eq!(r.exit_code, 3);
    assert_eq!(r.signal, 0);
    assert!(!r.timed_out && !r.stdout_truncated && !r.stderr_truncated);
}

#[test]
fn reports_the_signal_that_ended_the_child() {
    let r = go(&sh("kill -9 $$")).unwrap();
    assert_eq!((r.exit_code, r.signal), (-1, 9));
    assert!(!r.timed_out);
}

#[test]
fn runs_in_its_own_process_group() {
    let r = go(&sh("echo $$; ps -o pgid= -p $$")).unwrap();
    let text = String::from_utf8(r.stdout).unwrap();
    let mut it = text.split_whitespace();
    assert_eq!(it.next(), it.next(), "pid == pgid: {text}");
}

#[test]
fn timeout_sends_sigterm_to_the_group() {
    let mut r = sh("sleep 30 & echo $!; wait");
    r.timeout = Duration::from_millis(200);
    let start = Instant::now();
    let got = go(&r).unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(got.timed_out);
    assert_eq!(got.signal, libc::SIGTERM);
    let pid: i32 = String::from_utf8(got.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        wait_dead(pid),
        "the background child must die with the group"
    );
}

#[test]
fn timeout_escalates_to_sigkill_after_the_grace() {
    let mut r = sh("trap '' TERM; while true; do sleep 0.05; done");
    r.timeout = Duration::from_millis(200);
    let start = Instant::now();
    let got = go(&r).unwrap();
    let took = start.elapsed();
    assert!(got.timed_out);
    assert_eq!(got.signal, libc::SIGKILL);
    assert!(took >= Duration::from_millis(200) + GRACE, "{took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[test]
fn cancel_kills_the_group() {
    let cancel = Arc::new(AtomicBool::new(false));
    let c = Arc::clone(&cancel);
    let t =
        std::thread::spawn(move || run(&sh("sleep 30 & echo $!; wait"), None, &c, GRACE).unwrap());
    std::thread::sleep(Duration::from_millis(300));
    cancel.store(true, Ordering::Relaxed);
    let got = t.join().unwrap();
    assert!(!got.timed_out);
    assert_ne!(got.signal, 0);
    let pid: i32 = String::from_utf8(got.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(wait_dead(pid));
}

#[test]
fn stragglers_are_killed_when_the_child_exits() {
    let start = Instant::now();
    // The background sleep keeps the output pipes open.
    let got = go(&sh("sleep 30 & echo $!")).unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
    let pid: i32 = String::from_utf8(got.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(wait_dead(pid));
}

#[test]
fn output_cap_cuts_the_stream_that_overflowed() {
    let mut r = sh("head -c 5000 /dev/zero");
    r.max_output = 1000;
    let got = go(&r).unwrap();
    assert_eq!(got.stdout.len(), 1000);
    assert!(got.stdout_truncated && !got.stderr_truncated);

    let mut r = sh("head -c 5000 /dev/zero >&2");
    r.max_output = 1000;
    let got = go(&r).unwrap();
    assert_eq!(got.stderr.len(), 1000);
    assert!(got.stderr_truncated && !got.stdout_truncated);
}

#[test]
fn output_cap_is_shared_by_both_streams() {
    let mut r = sh("head -c 5000 /dev/zero; head -c 5000 /dev/zero >&2");
    r.max_output = 1000;
    let got = go(&r).unwrap();
    assert_eq!(got.stdout.len() + got.stderr.len(), 1000);
    assert!(got.stdout_truncated || got.stderr_truncated);
}

#[test]
fn a_child_that_floods_past_the_cap_still_finishes() {
    let mut r = sh("head -c 20000000 /dev/zero");
    r.max_output = 0;
    let got = go(&r).unwrap();
    assert_eq!(got.stdout.len(), MAX_OUTPUT);
    assert!(got.stdout_truncated);
    assert_eq!(got.exit_code, 0);
}

#[test]
fn stdin_is_empty_unless_given() {
    assert_eq!(go(&req(&["cat"])).unwrap().stdout, b"");
    let mut r = req(&["cat"]);
    r.stdin = b"hello".to_vec();
    assert_eq!(go(&r).unwrap().stdout, b"hello");
}

#[test]
fn a_big_stdin_does_not_deadlock_against_big_output() {
    let mut r = req(&["cat"]);
    r.stdin = vec![b'x'; MAX_STDIN];
    let got = go(&r).unwrap();
    assert_eq!(got.stdout.len(), MAX_STDIN);
}

#[test]
fn a_child_that_ignores_its_stdin_is_not_a_failure() {
    let mut r = sh("true");
    r.stdin = vec![b'x'; MAX_STDIN];
    assert_eq!(go(&r).unwrap().exit_code, 0);
}

#[test]
fn env_is_added_to_the_base_and_overrides_it() {
    let base: EnvVars = vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("A".into(), "base".into()),
        ("B".into(), "base".into()),
    ];
    let mut r = req(&["env"]);
    r.env = vec![("B".into(), "req".into())];
    let got = run(&r, Some(&base), &AtomicBool::new(false), GRACE).unwrap();
    let out = String::from_utf8(got.stdout).unwrap();
    let mut lines: Vec<_> = out.lines().collect();
    lines.sort();
    assert_eq!(lines, ["A=base", "B=req", "PATH=/usr/bin:/bin"]);
}

#[test]
fn clear_env_starts_from_nothing() {
    let base: EnvVars = vec![("A".into(), "base".into())];
    let mut r = req(&["/usr/bin/env"]);
    r.clear_env = true;
    r.env = vec![("ONLY".into(), "1".into())];
    let got = run(&r, Some(&base), &AtomicBool::new(false), GRACE).unwrap();
    assert_eq!(got.stdout, b"ONLY=1\n");
}

#[test]
fn argv0_is_looked_up_on_the_login_path_not_the_daemons() {
    let dir = tempfile::tempdir().unwrap();
    let tool = dir.path().join("mytool-0091");
    // Written by a `sh` child: a fork in another test thread while this
    // process held a write fd to it would make exec'ing it fail with
    // ETXTBSY (Linux).
    let status = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"printf '#!/bin/sh\necho found\n' > "$1" && chmod 755 "$1""#)
        .arg("sh")
        .arg(&tool)
        .status()
        .unwrap();
    assert!(status.success(), "writing {}", tool.display());
    let base: EnvVars = vec![(
        "PATH".into(),
        format!("{}:/usr/bin:/bin", dir.path().display()).into(),
    )];
    let got = run(
        &req(&["mytool-0091"]),
        Some(&base),
        &AtomicBool::new(false),
        GRACE,
    )
    .unwrap();
    assert_eq!(got.stdout, b"found\n");
    // Without that PATH it is not found.
    let e = go(&req(&["mytool-0091"])).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound);
}

#[test]
fn runs_in_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = req(&["pwd", "-P"]);
    r.cwd = dir.path().canonicalize().unwrap();
    let got = go(&r).unwrap();
    let want = format!("{}\n", r.cwd.display());
    assert_eq!(String::from_utf8(got.stdout).unwrap(), want);
}

#[test]
fn bad_requests_are_invalid_input() {
    let kind = |r: &ExecRequest| go(r).unwrap_err().kind();
    assert_eq!(kind(&req(&[])), io::ErrorKind::InvalidInput);
    assert_eq!(kind(&req(&[""])), io::ErrorKind::InvalidInput);
    let mut r = req(&["true"]);
    r.cwd = "relative".into();
    assert_eq!(kind(&r), io::ErrorKind::InvalidInput);
    let mut r = req(&["true"]);
    r.timeout = MAX_TIMEOUT + Duration::from_secs(1);
    assert_eq!(kind(&r), io::ErrorKind::InvalidInput);
    let mut r = req(&["true"]);
    r.stdin = vec![0; MAX_STDIN + 1];
    assert_eq!(kind(&r), io::ErrorKind::InvalidInput);
}

#[test]
fn a_missing_cwd_or_program_is_an_error() {
    let mut r = req(&["true"]);
    r.cwd = "/no/such/dir/0091".into();
    assert!(go(&r).is_err());
    assert_eq!(
        go(&req(&["no-such-program-0091"])).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

#[test]
fn execs_run_at_most_n_at_once() {
    let slots = tokio::sync::Semaphore::new(2);
    let started = Instant::now();
    rt().block_on(async {
        let all = (0..4).map(|_| exec_in(&slots, sh("sleep 0.3"), LoginEnv::daemon(), GRACE));
        for r in futures_join(all.collect()).await {
            r.unwrap();
        }
    });
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(580), "{took:?}");
}

#[test]
fn queued_execs_run_in_arrival_order() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let slots = tokio::sync::Semaphore::new(1);
    rt().block_on(async {
        let mut tasks = Vec::new();
        for i in 0..4 {
            let script = format!("echo {i} >> {}; sleep 0.05", log.display());
            tasks.push(exec_in(&slots, sh(&script), LoginEnv::daemon(), GRACE));
        }
        // Polled in order: each registers in the queue before the next.
        for r in futures_join(tasks).await {
            r.unwrap();
        }
    });
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "0\n1\n2\n3\n");
}

#[test]
fn dropping_the_future_kills_the_child() {
    let slots = tokio::sync::Semaphore::new(1);
    let out = rt().block_on(async {
        let mut r = sh("echo $$; sleep 30");
        r.cwd = std::env::temp_dir();
        // A pid file: the future is dropped before it returns anything.
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        r.argv[2] = format!("echo $$ > {}; sleep 30", pidfile.display());
        let fut = exec_in(&slots, r, LoginEnv::daemon(), GRACE);
        let _ = tokio::time::timeout(Duration::from_millis(500), fut).await;
        std::fs::read_to_string(pidfile).unwrap()
    });
    let pid: i32 = out.trim().parse().unwrap();
    assert!(wait_dead(pid));
}

/// Polls the futures concurrently, in order, without a futures crate.
async fn futures_join<T, F: std::future::Future<Output = T>>(fs: Vec<F>) -> Vec<T> {
    let mut pending: Vec<_> = fs.into_iter().map(|f| (Box::pin(f), None)).collect();
    std::future::poll_fn(move |cx| {
        let mut done = true;
        for (f, out) in pending.iter_mut() {
            if out.is_none() {
                match std::future::Future::poll(f.as_mut(), cx) {
                    std::task::Poll::Ready(v) => *out = Some(v),
                    std::task::Poll::Pending => done = false,
                }
            }
        }
        if done {
            std::task::Poll::Ready(pending.iter_mut().map(|(_, o)| o.take().unwrap()).collect())
        } else {
            std::task::Poll::Pending
        }
    })
    .await
}

// ---- ReadFile / ListDir ----

fn file_with(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f");
    std::fs::write(&p, bytes).unwrap();
    (dir, p)
}

fn read(p: &Path, offset: u64, max: usize) -> io::Result<FileData> {
    read_file(&ReadFileRequest {
        path: p.into(),
        offset,
        max_bytes: max,
    })
}

#[test]
fn read_file_returns_the_range_size_mtime_and_eof() {
    let (_d, p) = file_with(b"0123456789");
    let all = read(&p, 0, 0).unwrap();
    assert_eq!(
        (all.data.as_slice(), all.size, all.eof),
        (&b"0123456789"[..], 10, true)
    );
    assert!(all.mtime_unix_ms > 1_600_000_000_000);
    let mid = read(&p, 2, 3).unwrap();
    assert_eq!((mid.data.as_slice(), mid.eof), (&b"234"[..], false));
    let tail = read(&p, 7, 3).unwrap();
    assert_eq!((tail.data.as_slice(), tail.eof), (&b"789"[..], true));
    let past = read(&p, 99, 3).unwrap();
    assert_eq!((past.data.len(), past.size, past.eof), (0, 10, true));
}

#[test]
fn read_file_caps_a_read_at_15_mib() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("big");
    std::fs::File::create(&p)
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();
    for max in [0, usize::MAX] {
        let got = read(&p, 0, max).unwrap();
        assert_eq!(got.data.len(), MAX_READ);
        assert!(!got.eof);
    }
}

#[test]
fn read_file_follows_symlinks() {
    let (d, p) = file_with(b"abc");
    let l = d.path().join("link");
    symlink(&p, &l).unwrap();
    assert_eq!(read(&l, 0, 0).unwrap().data, b"abc");
}

#[test]
fn read_file_refuses_relative_paths_directories_and_special_files() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        read(Path::new("rel"), 0, 0).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(read(dir.path(), 0, 0).is_err());
    assert_eq!(
        read(&dir.path().join("missing"), 0, 0).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    // A FIFO with no writer would block an open forever.
    let fifo = dir.path().join("fifo");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(read(&fifo, 0, 0).is_err());
}

fn ls(p: &Path, max: usize) -> io::Result<DirListing> {
    list_dir(&ListDirRequest {
        path: p.into(),
        max_entries: max,
    })
}

#[test]
fn list_dir_reports_kinds_sizes_and_symlinks_unfollowed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file"), b"12345").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    symlink("/etc", dir.path().join("link")).unwrap();
    let c = std::ffi::CString::new(dir.path().join("fifo").to_str().unwrap()).unwrap();
    // SAFETY: valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let got = ls(dir.path(), 0).unwrap();
    assert!(!got.truncated);
    let mut by: Vec<_> = got
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.kind))
        .collect();
    by.sort_by_key(|e| e.0);
    assert_eq!(
        by,
        [
            ("fifo", EntryKind::Other),
            ("file", EntryKind::File),
            ("link", EntryKind::Symlink),
            ("sub", EntryKind::Dir)
        ]
    );
    let file = got.entries.iter().find(|e| e.name == "file").unwrap();
    assert_eq!(file.size, 5);
    assert!(file.mtime_unix_ms > 1_600_000_000_000);
}

#[test]
fn list_dir_truncates_at_max_entries() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..5 {
        std::fs::write(dir.path().join(format!("f{i}")), b"").unwrap();
    }
    let got = ls(dir.path(), 3).unwrap();
    assert_eq!((got.entries.len(), got.truncated), (3, true));
    let got = ls(dir.path(), 5).unwrap();
    assert_eq!((got.entries.len(), got.truncated), (5, false));
}

#[test]
fn list_dir_refuses_relative_paths_and_files() {
    let (_d, p) = file_with(b"x");
    assert_eq!(
        ls(Path::new("rel"), 0).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(ls(&p, 0).is_err());
}
