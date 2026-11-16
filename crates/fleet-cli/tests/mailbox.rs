//! `fleet message`, `fleet inbox` and `fleet wait` end to end (ADR 0076,
//! spec §3): agents run the real `fleet` against a real `fleetd`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;
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
    fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let child = Command::new(fleetd_bin())
            .current_dir("/")
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

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(FLEET);
        cmd.env_remove("FLEET_AGENT_TOKEN")
            .arg("--state-dir")
            .arg(self.dir.path())
            .args(args)
            .stdin(Stdio::null());
        cmd
    }

    /// `fleet` run by the test: the user.
    fn fleet(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run fleet")
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

    /// Spawns an agent running `body` as a script (with `$F` the fleet
    /// command and `$S` the state dir); its id.
    fn agent(&self, name: &str, body: &str) -> String {
        let path = self.path(name);
        let text = format!(
            "F='{FLEET} --state-dir {dir}'\nS='{dir}'\n{body}\n",
            dir = self.dir.path().display()
        );
        std::fs::write(&path, text).unwrap();
        self.ok(&[
            "spawn",
            "--name",
            name,
            "--",
            "/bin/sh",
            path.to_str().unwrap(),
        ])
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

    fn json(&self, name: &str) -> Value {
        let s = self.read(name);
        serde_json::from_str(&s).unwrap_or_else(|e| panic!("{name}: {e}: {s}"))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // SAFETY: plain signal to our child.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs `cmd`, writing its stdout to `$S/<out>` and its exit code to
/// `$S/<out>.code` (each renamed into place, so readers see them whole).
fn capture(cmd: &str, out: &str) -> String {
    format!(
        "{cmd} > $S/{out}.tmp 2> $S/{out}.err; echo $? > $S/{out}.code.tmp; \
         mv $S/{out}.tmp $S/{out}; mv $S/{out}.code.tmp $S/{out}.code"
    )
}

#[test]
fn a_parent_waits_for_a_child_that_exits_first_and_reads_its_report() {
    let d = Daemon::start();
    // The child exits at once, before the parent waits: the exit report in
    // the parent's mailbox still wakes the wait.
    d.agent(
        "parent",
        &format!(
            "C=$($F spawn --name kid -- /bin/sh -c 'exit 3')\necho $C > $S/kid.id\nsleep 1\n{}\n{}\nexec sleep 600",
            capture("$F wait --timeout 30s --json", "waited"),
            capture("$F inbox --json", "inbox"),
        ),
    );
    let kid: u64 = d.read("kid.id").parse().unwrap();
    assert_eq!(d.read("waited.code"), "0", "{}", d.read("waited.err"));
    let waited = d.json("waited");
    assert_eq!(waited["matched"], true);
    let m = &waited["matches"][0];
    assert_eq!(m["agent"], kid);
    assert_eq!(m["state"], "exited");
    assert_eq!(m["message"]["exit_code"], 3);
    assert_eq!(m["message"]["sender_name"], "kid");

    assert_eq!(d.read("inbox.code"), "0");
    let inbox = d.json("inbox");
    let first = &inbox[0];
    assert_eq!(first["kind"], "exit");
    assert_eq!(first["sender"], kid);
    assert_eq!(first["trusted"], false);
}

#[test]
fn agents_message_their_children_and_parent_but_not_siblings() {
    let d = Daemon::start();
    let parent = d.agent(
        "boss",
        &format!(
            "A=$($F spawn --name a -- /bin/sh $S/child_a)\nB=$($F spawn --name b -- sleep 600)\n\
             echo $B > $S/b.id\n$F message a 'hello a'\n{}\nexec sleep 600",
            capture("$F wait a --state message --timeout 30s", "boss_waited"),
        ),
    );
    std::fs::write(
        d.path("child_a"),
        format!(
            "F='{FLEET} --state-dir {dir}'\nS='{dir}'\n{}\n{}\n{}\n{}\nexec sleep 600\n",
            capture("$F wait --state message --timeout 30s", "a_waited"),
            capture("$F inbox", "a_inbox"),
            capture("$F message $(cat $S/b.id) 'psst'", "a_sibling"),
            capture("$F message parent 'done here'", "a_up"),
            dir = d.dir.path().display(),
        ),
    )
    .unwrap();
    assert_eq!(d.read("a_waited.code"), "0", "{}", d.read("a_waited.err"));
    assert!(
        d.read("a_waited").contains("hello a"),
        "{}",
        d.read("a_waited")
    );
    assert!(
        d.read("a_inbox")
            .starts_with(&format!("[from agent {parent} (boss)] message: hello a")),
        "{}",
        d.read("a_inbox")
    );
    assert_eq!(d.read("a_sibling.code"), "3", "{}", d.read("a_sibling.err"));
    assert!(d.read("a_sibling.err").contains("forbidden:"));
    assert_eq!(d.read("a_up.code"), "0");
    assert_eq!(
        d.read("boss_waited.code"),
        "0",
        "{}",
        d.read("boss_waited.err")
    );
    assert!(d.read("boss_waited").contains("done here"));
}

#[test]
fn the_user_broadcasts_and_reads_their_own_mailbox() {
    let d = Daemon::start();
    let a = d.ok(&[
        "spawn", "--name", "one", "--label", "team=x", "--", "sleep", "600",
    ]);
    let b = d.ok(&["spawn", "--name", "two", "--", "sleep", "600"]);
    let all = d.ok(&["message", "--descendants", "stand by"]);
    assert_eq!(all.lines().collect::<Vec<_>>(), [a.as_str(), b.as_str()]);
    assert_eq!(d.ok(&["message", "--label", "team=x", "only x"]), a);
    // Nobody matches: exit 1. No such agent: exit 1. Empty text: exit 2.
    assert_eq!(
        d.fleet(&["message", "--label", "team=y", "x"])
            .status
            .code(),
        Some(1)
    );
    assert_eq!(d.fleet(&["message", "nobody", "x"]).status.code(), Some(1));
    assert_eq!(d.fleet(&["message", "999", "x"]).status.code(), Some(1));
    assert_eq!(d.fleet(&["message", &a, ""]).status.code(), Some(2));
    let big = "x".repeat(4097);
    assert_eq!(d.fleet(&["message", &a, &big]).status.code(), Some(2));
    // A note to self, read once.
    d.ok(&["message", "user", "remember the milk"]);
    let inbox = d.ok(&["inbox"]);
    assert_eq!(inbox, "[from the user] message: remember the milk");
    assert_eq!(d.ok(&["inbox"]), "");
    let again = d.ok(&["inbox", "--since", "0", "--json"]);
    let v: Value = serde_json::from_str(&again).unwrap();
    assert_eq!(v[0]["body"], "remember the milk");
}

#[test]
fn a_wait_times_out_with_exit_1_and_the_current_statuses() {
    let d = Daemon::start();
    let a = d.ok(&["spawn", "--name", "slow", "--", "sleep", "600"]);
    let out = d.fleet(&["wait", &a, "--state", "exited,done", "--timeout", "300ms"]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim(), format!("agent {a} (slow): running"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("timed out"));
    // Named and already exited: at once.
    d.ok(&["kill", &a]);
    let start = Instant::now();
    let out = d.fleet(&[
        "wait",
        &a,
        "--state",
        "exited",
        "--timeout",
        "30s",
        "--json",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(start.elapsed() < Duration::from_secs(20));
    // Usage errors exit 2; an unknown name 1.
    assert_eq!(
        d.fleet(&["wait", "--state", "bogus"]).status.code(),
        Some(2)
    );
    assert_eq!(
        d.fleet(&["wait", "nobody", "--timeout", "1s"])
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn mailboxes_and_waits_survive_a_daemon_restart() {
    let d = Daemon::start();
    // The worker reads its inbox once told to, after the restart.
    let a = d.agent(
        "worker",
        &format!(
            "while [ ! -f $S/go ]; do sleep 0.05; done\n{}\nexec sleep 600",
            capture("$F inbox", "w_inbox")
        ),
    );
    d.ok(&["message", &a, "before the restart"]);
    let waiter = d
        .command(&["wait", &a, "--state", "exited", "--timeout", "60s"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let out = d.fleet(&["daemon", "restart"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::write(d.path("go"), "").unwrap();
    assert_eq!(d.read("w_inbox.code"), "0", "{}", d.read("w_inbox.err"));
    assert!(d.read("w_inbox").contains("before the restart"));
    // The wait reconnected and still sees the exit.
    d.ok(&["kill", &a]);
    let out = waiter.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("exited"));
}
