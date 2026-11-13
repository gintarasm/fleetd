use fleet_node::{CommandSpec, PtyHost};
use std::path::PathBuf;
use std::time::Duration;

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
