use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use fleetd::{Config, StartupError, startup};
use tempfile::TempDir;

/// Tests never run the developer's real login shell.
const NO_LOGIN_ENV: (&str, &str) = ("FLEETD_LOGIN_ENV", "off");

fn spawn(dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_fleetd"))
        .env(NO_LOGIN_ENV.0, NO_LOGIN_ENV.1)
        .arg("--state-dir")
        .arg(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fleetd")
}

fn wait_ready(dir: &Path) {
    let sock = dir.join("fleetd.sock");
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if UnixStream::connect(&sock).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("daemon not ready within 2s");
}

fn sigterm(child: &Child) {
    send_signal(child, "-TERM");
}

fn send_signal(child: &Child, sig: &str) {
    let status = Command::new("kill")
        .arg(sig)
        .arg(child.id().to_string())
        .status()
        .expect("run kill");
    assert!(status.success());
}

struct Reaper(Child);
impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Wait for `child` to exit; kill it and fail the test after 10 s.
fn wait_exit(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("process {} did not exit within 10s", child.id());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn becomes_ready() {
    let dir = TempDir::new().unwrap();
    let _d = Reaper(spawn(dir.path()));
    wait_ready(dir.path());
}

#[test]
fn second_instance_exits_2_and_keeps_socket() {
    let dir = TempDir::new().unwrap();
    let _first = Reaper(spawn(dir.path()));
    wait_ready(dir.path());
    let mut second = Reaper(spawn(dir.path()));
    let status = wait_exit(&mut second.0);
    assert_eq!(status.code(), Some(2));
    assert!(UnixStream::connect(dir.path().join("fleetd.sock")).is_ok());
}

#[test]
fn stale_socket_is_replaced() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("fleetd.sock"), b"stale").unwrap();
    let _d = Reaper(spawn(dir.path()));
    wait_ready(dir.path());
}

#[test]
fn overlong_socket_path_errors() {
    let tmp = TempDir::new().unwrap();
    let cfg = Config {
        state_dir: tmp.path().join("a".repeat(100)),
    };
    assert!(cfg.socket_path().as_os_str().len() > 103);
    match startup(&cfg) {
        Err(StartupError::SocketPathTooLong(_)) => {}
        other => panic!("expected SocketPathTooLong, got {:?}", other.err()),
    }
}

#[test]
fn lock_released_on_exit() {
    let dir = TempDir::new().unwrap();
    let mut first = Reaper(spawn(dir.path()));
    wait_ready(dir.path());
    sigterm(&first.0);
    let status = wait_exit(&mut first.0);
    assert!(
        status.success(),
        "fleetd exited with {status} after SIGTERM"
    );
    assert!(!dir.path().join("fleetd.sock").exists());
    let _second = Reaper(spawn(dir.path()));
    wait_ready(dir.path());
}

#[test]
fn sighup_shuts_down_like_sigterm() {
    let dir = TempDir::new().unwrap();
    let mut d = Reaper(spawn(dir.path()));
    wait_ready(dir.path());
    send_signal(&d.0, "-HUP");
    let status = wait_exit(&mut d.0);
    assert!(status.success(), "fleetd exited with {status} after SIGHUP");
    assert!(!dir.path().join("fleetd.sock").exists());
}

// ---- `fleetd --stdio` -------------------------------------------------------

/// `fleetd --stdio --state-dir dir`, handshaken over its stdin/stdout.
fn bridge(dir: &Path) -> (fleet_client::Connection, Reaper) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fleetd"));
    // The daemon a bridge starts inherits its environment.
    cmd.env(NO_LOGIN_ENV.0, NO_LOGIN_ENV.1)
        .arg("--stdio")
        .arg("--state-dir")
        .arg(dir);
    let (conn, child) = fleet_client::Connection::command(cmd).expect("handshake via --stdio");
    (conn, Reaper(child))
}

fn list(conn: &mut fleet_client::Connection) -> Vec<fleet_proto::AgentInfo> {
    match conn
        .call(fleet_proto::request::Kind::List(Default::default()))
        .expect("list")
    {
        fleet_proto::response::Kind::Agents(a) => a.agents,
        other => panic!("unexpected {other:?}"),
    }
}

/// Live (non-zombie) daemons on `dir`: `fleetd` processes naming it, other
/// than bridges.
fn daemon_pids(dir: &Path) -> Vec<i32> {
    let out = Command::new("ps")
        .args(["-axww", "-o", "pid=,stat=,args="])
        .output()
        .expect("ps");
    let dir = dir.to_string_lossy();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains(&*dir) && l.contains("fleetd") && !l.contains("--stdio"))
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let pid = f.next()?.parse().ok()?;
            let stat = f.next()?;
            (!stat.starts_with('Z')).then_some(pid)
        })
        .collect()
}

/// Kills the daemons a bridge started on `dir` when dropped.
struct DetachedDaemons<'a>(&'a Path);
impl Drop for DetachedDaemons<'_> {
    fn drop(&mut self) {
        for pid in daemon_pids(self.0) {
            // SAFETY: plain signal to a test daemon.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

#[test]
fn stdio_bridges_to_running_daemon() {
    let dir = TempDir::new().unwrap();
    let _d = Reaper(spawn(dir.path()));
    wait_ready(dir.path());
    let (mut conn, _bridge) = bridge(dir.path());
    assert!(list(&mut conn).is_empty());
    assert_eq!(daemon_pids(dir.path()).len(), 1);
}

#[test]
fn stdio_starts_daemon_when_absent() {
    let dir = TempDir::new().unwrap();
    let _cleanup = DetachedDaemons(dir.path());
    let (mut conn, mut b) = bridge(dir.path());
    assert!(list(&mut conn).is_empty());
    drop(conn);
    // Closing the bridge's stdin ends it.
    let status = wait_exit(&mut b.0);
    assert!(status.success(), "bridge exited with {status}");
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        UnixStream::connect(dir.path().join("fleetd.sock")).is_ok(),
        "the started daemon outlives the bridge"
    );
    assert_eq!(daemon_pids(dir.path()).len(), 1);
}

#[test]
fn concurrent_stdio_bridges_start_one_daemon() {
    let dir = TempDir::new().unwrap();
    let _cleanup = DetachedDaemons(dir.path());
    let bridges: Vec<_> = (0..2)
        .map(|_| {
            let dir = dir.path().to_path_buf();
            std::thread::spawn(move || {
                let (mut conn, b) = bridge(&dir);
                assert!(list(&mut conn).is_empty());
                (conn, b)
            })
        })
        .collect();
    let _bridges: Vec<_> = bridges.into_iter().map(|t| t.join().unwrap()).collect();
    // The loser exits on the lock; give it a moment.
    let deadline = Instant::now() + Duration::from_secs(2);
    while daemon_pids(dir.path()).len() > 1 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(daemon_pids(dir.path()).len(), 1);
}

#[test]
fn stdio_fails_when_daemon_cannot_start() {
    let tmp = TempDir::new().unwrap();
    // A state dir under a regular file can never be created.
    let file = tmp.path().join("f");
    std::fs::write(&file, b"").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_fleetd"))
        .env(NO_LOGIN_ENV.0, NO_LOGIN_ENV.1)
        .arg("--stdio")
        .arg("--state-dir")
        .arg(file.join("state"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(!out.stderr.is_empty());
}

// ---- login-shell environment -------------------------------------------------

/// Writes an executable script into `dir`.
fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// fleetd with `SHELL=shell` (and `extra` env) for the daemon process only.
fn spawn_with_shell(dir: &Path, shell: &Path, extra: &[(&str, &str)]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_fleetd"))
        .arg("--state-dir")
        .arg(dir)
        .env("SHELL", shell)
        .env_remove(NO_LOGIN_ENV.0)
        .envs(extra.iter().copied())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fleetd")
}

/// Spawns `sh -c script` in `cwd` and waits up to 10 s for `cwd/out`.
fn agent_output(dir: &Path, cwd: &Path, script: &str) -> String {
    let mut conn = fleet_client::Connection::unix(&dir.join("fleetd.sock")).unwrap();
    let spawn = fleet_proto::SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: cwd.to_string_lossy().into_owned(),
        ..Default::default()
    };
    conn.call(fleet_proto::request::Kind::Spawn(spawn))
        .expect("spawn");
    let out = cwd.join("out");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = std::fs::read_to_string(&out)
            && s.ends_with('\n')
        {
            return s;
        }
        assert!(Instant::now() < deadline, "agent wrote no output");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn login_env_reaches_agent() {
    let dir = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    // Called as `shell -lic CMD`: a login shell that sets one variable.
    let shell = script(
        work.path(),
        "login.sh",
        "#!/bin/sh\nexport FLEET_TEST_VAR=1\nexec /bin/sh -c \"$2\"\n",
    );
    let _d = Reaper(spawn_with_shell(dir.path(), &shell, &[]));
    wait_ready(dir.path());
    let got = agent_output(dir.path(), work.path(), "echo \"$FLEET_TEST_VAR\" > out");
    assert_eq!(got, "1\n");
}

#[test]
fn login_env_is_the_base_of_the_agent_env() {
    let dir = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    // A login env built from scratch: the daemon's own variables are gone,
    // `PWD`/`SHLVL` and a stray `FLEET_AGENT_ID` are dropped.
    let shell = script(
        work.path(),
        "login.sh",
        "#!/bin/sh\nexec env -i PATH=/usr/bin:/bin FLEET_TEST_VAR=login \
         FLEET_AGENT_ID=999 SHLVL=7 PWD=/nonexistent /bin/sh -c \"$2\"\n",
    );
    let _d = Reaper(spawn_with_shell(
        dir.path(),
        &shell,
        &[("FLEET_DAEMON_ONLY", "daemon")],
    ));
    wait_ready(dir.path());
    let got = agent_output(
        dir.path(),
        work.path(),
        "echo \"$FLEET_TEST_VAR:${FLEET_DAEMON_ONLY-unset}:$FLEET_AGENT_ID:$TERM\" > out",
    );
    let fields: Vec<_> = got.trim_end().split(':').collect();
    assert_eq!(fields[0], "login");
    assert_eq!(fields[1], "unset");
    assert_ne!(fields[2], "999", "the node sets FLEET_AGENT_ID");
    assert!(fields[2].parse::<u64>().is_ok());
    assert_eq!(fields[3], "xterm-256color");
}

#[test]
fn noisy_or_hanging_login_shell_falls_back() {
    let dir = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let pidfile = work.path().join("shell.pid");
    let shell = script(
        work.path(),
        "hang.sh",
        &format!(
            "#!/bin/sh\necho $$ > '{}'\necho junk\nprintf 'more junk\\000__FLEET_ENV__\\000A=B\\000'\nsleep 30\n",
            pidfile.display()
        ),
    );
    let start = Instant::now();
    let _d = Reaper(spawn_with_shell(
        dir.path(),
        &shell,
        &[("FLEET_DAEMON_ONLY", "daemon")],
    ));
    let sock = dir.path().join("fleetd.sock");
    while UnixStream::connect(&sock).is_err() {
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "socket not ready within 1s"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let got = agent_output(
        dir.path(),
        work.path(),
        "echo \"${FLEET_DAEMON_ONLY-unset}:${A-unset}\" > out",
    );
    assert!(
        start.elapsed() < Duration::from_secs(6),
        "spawn took {:?}",
        start.elapsed()
    );
    assert_eq!(got, "daemon:unset\n");
    // The hung shell (and its `sleep`) were killed.
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    // SAFETY: signal 0 only checks for existence.
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(Instant::now() < deadline, "login shell {pid} still running");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn no_shell_uses_daemon_env() {
    let dir = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let _d = Reaper(
        Command::new(env!("CARGO_BIN_EXE_fleetd"))
            .arg("--state-dir")
            .arg(dir.path())
            .env_remove("SHELL")
            .env_remove(NO_LOGIN_ENV.0)
            .env("FLEET_DAEMON_ONLY", "daemon")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_ready(dir.path());
    let got = agent_output(dir.path(), work.path(), "echo \"$FLEET_DAEMON_ONLY\" > out");
    assert_eq!(got, "daemon\n");
}

#[test]
fn login_env_switch_off_skips_capture() {
    let dir = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let shell = script(
        work.path(),
        "login.sh",
        "#!/bin/sh\nexport FLEET_TEST_VAR=1\nexec /bin/sh -c \"$2\"\n",
    );
    let _d = Reaper(spawn_with_shell(
        dir.path(),
        &shell,
        &[NO_LOGIN_ENV, ("FLEET_DAEMON_ONLY", "daemon")],
    ));
    wait_ready(dir.path());
    let got = agent_output(
        dir.path(),
        work.path(),
        "echo \"${FLEET_TEST_VAR-unset}:$FLEET_DAEMON_ONLY\" > out",
    );
    assert_eq!(got, "unset:daemon\n");
}
