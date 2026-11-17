//! `fleet report --question` and `fleet answer` end to end (ADR 0078): the
//! real CLI inside an agent and as the user.

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

#[test]
fn a_question_is_answered_by_the_user_and_read_by_the_agent() {
    let d = Daemon::start("");
    let a = d.script(
        "a.sh",
        &[
            capture(
                "$F report waiting --question 'Which branch?' --choice main --choice dev",
                "ask",
            ),
            capture("$F report working --question hm", "bad.state"),
            capture("$F report waiting --choice x", "bad.choice"),
            capture("$F report waiting --question q --choice '' ", "bad.empty"),
            capture("$F answer asker nope", "agent.answer"),
            capture("$F answer 1 nope", "agent.answer.id"),
            "echo ready > $S/ready.tmp; mv $S/ready.tmp $S/ready".into(),
            capture("$F wait --state message --timeout 30s", "wait"),
            capture("$F inbox --json", "inbox"),
            "exec sleep 600".into(),
        ]
        .join("\n"),
    );
    let id = d.ok(&["spawn", "--name", "asker", "--", "sh", &a]);
    d.read("ready");
    assert_eq!(d.read("ask.code"), "0");
    assert_eq!(d.read("bad.state.code"), "2");
    assert_eq!(d.read("bad.choice.code"), "2");
    assert_eq!(d.read("bad.empty.code"), "2");
    // An agent may not answer, by name or by id.
    assert_eq!(d.read("agent.answer.code"), "3");
    assert_eq!(d.read("agent.answer.id.code"), "3");

    let json: serde_json::Value = serde_json::from_str(&d.ok(&["ls", "--json"])).unwrap();
    let q = &json[0]["reported"]["question"];
    assert_eq!(q["text"], "Which branch?");
    assert_eq!(q["choices"], serde_json::json!(["main", "dev"]));

    // Unknown agent: 1. Usage: 2.
    assert_eq!(d.fleet(&["answer", "nobody", "x"]).status.code(), Some(1));
    assert_eq!(d.fleet(&["answer", "asker"]).status.code(), Some(2));
    let out = d.fleet(&["answer", "asker", "dev"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Closed: no open question.
    assert_eq!(d.fleet(&["answer", &id, "again"]).status.code(), Some(1));

    assert_eq!(d.read("wait.code"), "0");
    let inbox: serde_json::Value = serde_json::from_str(&d.read("inbox")).unwrap();
    let answer = inbox
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["kind"] == "answer")
        .expect("an answer in the inbox");
    assert_eq!(answer["body"], "dev");
    assert_eq!(answer["trusted"], true);
    let json: serde_json::Value = serde_json::from_str(&d.ok(&["ls", "--json"])).unwrap();
    assert!(json[0]["reported"]["question"].is_null());
}
