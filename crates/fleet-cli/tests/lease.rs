//! Leases end to end (ADR 0077, spec §4): the real `fleet lease` against a
//! real `fleetd`, as the user and as agents, across a daemon restart.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn fleetd_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let mut cmd = Command::new(env!("CARGO"));
        cmd.args(["build", "-p", "fleetd", "--message-format=short"]);
        if !cfg!(debug_assertions) {
            cmd.arg("--release");
        }
        assert!(cmd.status().expect("cargo build").success());
        let fleet = Path::new(env!("CARGO_BIN_EXE_fleet"));
        fleet.parent().unwrap().join("fleetd")
    })
}

const FLEET: &str = env!("CARGO_BIN_EXE_fleet");

struct Daemon {
    child: Child,
    dir: TempDir,
}

impl Daemon {
    /// A daemon on a fresh state dir, with `config` as its `config.toml`.
    fn start(config: &str) -> Self {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        let child = Command::new(fleetd_bin())
            .env("FLEETD_LOGIN_ENV", "off")
            .arg("--state-dir")
            .arg(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fleetd");
        let sock = dir.path().join("fleetd.sock");
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(&sock).is_err() {
            assert!(Instant::now() < deadline, "daemon not ready within 5s");
            std::thread::sleep(Duration::from_millis(5));
        }
        Daemon { child, dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// `fleet` run by the test: the user (its session leads no agent, and
    /// it sends no token).
    fn fleet(&self, args: &[&str]) -> Output {
        Command::new(FLEET)
            .env_remove("FLEET_AGENT_TOKEN")
            .arg("--state-dir")
            .arg(self.dir.path())
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("run fleet")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.fleet(args);
        assert!(
            out.status.success(),
            "fleet {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Writes `body` as script `name` (with `$F` the fleet command and `$S`
    /// the state dir) and returns its path.
    fn script(&self, name: &str, body: &str) -> String {
        let path = self.path(name);
        let text = format!(
            "F='{FLEET} --state-dir {dir}'\nS='{dir}'\n{body}\n",
            dir = self.dir.path().display()
        );
        std::fs::write(&path, text).unwrap();
        path.display().to_string()
    }

    /// Waits for file `name` and returns its trimmed contents.
    fn read(&self, name: &str) -> String {
        let path = self.path(name);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(s) = std::fs::read_to_string(&path) {
                return s.trim().to_owned();
            }
            assert!(Instant::now() < deadline, "{name} never appeared");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs `cmd` and writes its stdout to `$S/<out>`, its exit code to
/// `$S/<out>.code` (each renamed into place, so readers see them whole).
fn capture(cmd: &str, out: &str) -> String {
    format!(
        "{cmd} > $S/{out}.tmp 2> $S/{out}.err; echo $? > $S/{out}.code.tmp; \
         mv $S/{out}.tmp $S/{out}; mv $S/{out}.code.tmp $S/{out}.code"
    )
}

/// `fleet lease ls --json`, parsed.
fn leases(d: &Daemon) -> Vec<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(&d.ok(&["lease", "ls", "--json"])).unwrap();
    v.as_array().unwrap().clone()
}

fn lease<'a>(all: &'a [serde_json::Value], name: &str) -> Option<&'a serde_json::Value> {
    all.iter().find(|l| l["name"] == name)
}

/// Waits until `f` holds (30 s at most).
fn until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !f() {
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// `fleet lease <args>` as a background process of the user's.
fn background(d: &Daemon, args: &[&str]) -> Child {
    Command::new(FLEET)
        .env_remove("FLEET_AGENT_TOKEN")
        .arg("--state-dir")
        .arg(d.dir.path())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fleet")
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn the_command_form_holds_while_it_runs_and_exits_with_its_code() {
    let d = Daemon::start("");
    assert_eq!(
        code(&d.fleet(&["lease", "q", "--", "sh", "-c", "exit 7"])),
        7
    );
    assert_eq!(
        code(&d.fleet(&["lease", "q", "--", "no-such-command-xyz"])),
        127
    );
    assert!(leases(&d).is_empty(), "released when the command ended");

    let mut holder = background(&d, &["lease", "wall", "--", "sleep", "600"]);
    until("the lease is held", || lease(&leases(&d), "wall").is_some());
    let l = leases(&d);
    let wall = lease(&l, "wall").unwrap();
    assert_eq!(wall["holder_kind"], "user");
    assert_eq!(wall["held_by_connection"], true);

    // Busy without --wait: exit 1, and the table names the holder.
    let busy = d.fleet(&["lease", "wall", "--", "true"]);
    assert_eq!(code(&busy), 1, "{}", stderr(&busy));
    assert!(stderr(&busy).contains("busy:"), "{}", stderr(&busy));
    assert!(d.ok(&["lease", "ls"]).contains("user"));

    // SIGTERM goes to the command, and the lease goes with it.
    // SAFETY: plain signal to our child.
    unsafe { libc::kill(holder.id() as i32, libc::SIGTERM) };
    assert_eq!(holder.wait().unwrap().code(), Some(128 + libc::SIGTERM));
    until("the lease is released", || leases(&d).is_empty());
}

#[test]
fn a_waiter_gets_the_lease_when_the_holder_ends_and_runs_its_command() {
    let d = Daemon::start("");
    let mut holder = background(&d, &["lease", "wall", "--", "sleep", "600"]);
    until("held", || lease(&leases(&d), "wall").is_some());
    let marker = d.path("ran");
    let mut waiter = background(
        &d,
        &[
            "lease",
            "wall",
            "--wait",
            "--",
            "sh",
            "-c",
            &format!("touch {}", marker.display()),
        ],
    );
    until("queued", || {
        lease(&leases(&d), "wall").is_some_and(|l| l["waiters"] == 1)
    });
    assert!(!marker.exists());
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert_eq!(waiter.wait().unwrap().code(), Some(0));
    assert!(marker.exists());
    until("released", || leases(&d).is_empty());
}

#[test]
fn acquire_needs_a_ttl_from_the_user_and_the_ttl_releases() {
    let d = Daemon::start("");
    let out = d.fleet(&["lease", "acquire", "x"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    assert!(stderr(&out).contains("--ttl"));
    d.ok(&["lease", "acquire", "x", "--ttl", "2s"]);
    let l = leases(&d);
    assert_eq!(lease(&l, "x").unwrap()["held_by_connection"], false);
    assert!(lease(&l, "x").unwrap()["expires_unix_ms"].is_i64());
    assert_eq!(code(&d.fleet(&["lease", "acquire", "x", "--ttl", "1m"])), 1);
    until("the ttl ran out", || leases(&d).is_empty());

    d.ok(&["lease", "acquire", "y", "--ttl", "1h"]);
    d.ok(&["lease", "release", "y"]);
    let gone = d.fleet(&["lease", "release", "y"]);
    assert_eq!(code(&gone), 1, "not held: {}", stderr(&gone));
    assert_eq!(
        code(&d.fleet(&["lease", "acquire", "z", "--ttl", "bogus"])),
        2
    );
}

#[test]
fn max_load_waits_for_a_quiet_machine() {
    let d = Daemon::start("");
    // Below any real load average: never granted.
    let out = d.fleet(&["lease", "bench", "--max-load", "0.0000001", "--", "true"]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    assert!(stderr(&out).contains("load average"), "{}", stderr(&out));
    let mut waiter = background(
        &d,
        &[
            "lease",
            "bench",
            "--max-load",
            "0.0000001",
            "--wait",
            "--",
            "true",
        ],
    );
    until("queued", || {
        lease(&leases(&d), "bench").is_some_and(|l| l["waiters"] == 1)
    });
    assert_eq!(leases(&d)[0]["held"], false);
    waiter.kill().unwrap();
    waiter.wait().unwrap();
    until("the dead waiter left", || leases(&d).is_empty());
    // A high limit is granted at once.
    assert_eq!(
        code(&d.fleet(&["lease", "bench", "--max-load", "1000000", "--", "true"])),
        0
    );
}

#[test]
fn an_agents_lease_ends_with_the_agent_and_others_cannot_release_it() {
    let d = Daemon::start("");
    let a = d.script(
        "a.sh",
        "$F lease acquire shared > $S/a.out 2>&1 && touch $S/a.held; exec sleep 600",
    );
    let a = d.ok(&["spawn", "--", "sh", &a]);
    d.read("a.held");
    let l = leases(&d);
    let shared = lease(&l, "shared").unwrap();
    assert_eq!(shared["holder_kind"], "agent");
    assert_eq!(shared["holder"].as_u64().unwrap().to_string(), a);

    // Another agent (a sibling root) neither sees the holder nor releases.
    let b = d.script(
        "b.sh",
        &[
            capture("$F lease ls --json", "b.ls"),
            capture("$F lease release shared", "b.release"),
            capture("$F lease acquire shared", "b.acquire"),
            "exec sleep 600".into(),
        ]
        .join("\n"),
    );
    d.ok(&["spawn", "--", "sh", &b]);
    d.read("b.acquire.code");
    let seen: serde_json::Value = serde_json::from_str(&d.read("b.ls")).unwrap();
    assert_eq!(seen[0]["holder_kind"], "other");
    assert_eq!(seen[0]["holder"], serde_json::Value::Null);
    assert_eq!(d.read("b.release.code"), "3", "{}", d.read("b.release.err"));
    assert_eq!(d.read("b.acquire.code"), "1");

    // Killing the holder releases it.
    d.ok(&["kill", &a]);
    until("released with its agent", || leases(&d).is_empty());
}

#[test]
fn leases_survive_a_daemon_restart_and_the_command_form_is_reclaimed() {
    let d = Daemon::start("");
    d.ok(&["lease", "acquire", "kept", "--ttl", "1h"]);
    let mut holder = background(&d, &["lease", "wall", "--", "sleep", "40"]);
    until("held", || lease(&leases(&d), "wall").is_some());

    d.ok(&["daemon", "restart"]);
    until("the new daemon answers", || {
        d.fleet(&["lease", "ls"]).status.success()
    });
    let l = leases(&d);
    assert!(lease(&l, "kept").is_some(), "the ttl lease survived");
    // The command form is reclaimed by its client, which reconnected.
    std::thread::sleep(Duration::from_millis(1500));
    let l = leases(&d);
    let wall = lease(&l, "wall").expect("still held");
    assert_eq!(wall["held_by_connection"], true);
    assert!(holder.try_wait().unwrap().is_none(), "the command runs on");

    // Reclaimed means tied to the new connection again: killing the client
    // releases at once, not after the 30 s an unclaimed lease waits.
    holder.kill().unwrap();
    holder.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while lease(&leases(&d), "wall").is_some() {
        assert!(Instant::now() < deadline, "not reclaimed: still held");
        std::thread::sleep(Duration::from_millis(25));
    }
}
