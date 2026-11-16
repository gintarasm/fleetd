//! `fleet report` end to end (ADR 0075): the real CLI inside an agent, and
//! the reported status in `fleet ls` and `fleet tree`.

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
fn report_needs_an_agent_and_shows_in_ls_and_tree() {
    let d = Daemon::start("");
    // The user has no agent to report for.
    let out = d.fleet(&["report", "working", "hi"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("forbidden"));

    let long = "x".repeat(201);
    let a = d.script(
        "a.sh",
        &[
            capture("$F report busy", "bad.state"),
            capture(&format!("$F report working {long}"), "bad.long"),
            capture("$F report working", "r1"),
            capture("$F report waiting 'need a review'", "r2"),
            "echo ready > $S/ready.tmp; mv $S/ready.tmp $S/ready".into(),
            "exec sleep 600".into(),
        ]
        .join("\n"),
    );
    let id = d.ok(&["spawn", "--name", "reporter", "--", "sh", &a]);
    d.read("ready");
    assert_eq!(d.read("bad.state.code"), "2");
    assert_eq!(d.read("bad.long.code"), "2");
    assert!(
        d.read("bad.long.err").contains("200"),
        "{}",
        d.read("bad.long.err")
    );
    assert_eq!(d.read("r1.code"), "0");
    assert_eq!(d.read("r2.code"), "0");

    let json: serde_json::Value = serde_json::from_str(&d.ok(&["ls", "--json"])).unwrap();
    let r = &json[0]["reported"];
    assert_eq!(r["state"], "waiting");
    assert_eq!(r["message"], "need a review");
    assert!(r["at_unix_ms"].as_i64().unwrap() > 0);

    let ls = d.ok(&["ls"]);
    let mut lines = ls.lines();
    let header: Vec<_> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(
        header,
        ["ID", "NAME", "STATE", "EXIT", "REPORTED", "PROGRAM"]
    );
    let row: Vec<_> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(
        &row[..5],
        [id.as_str(), "reporter", "running", "-", "waiting"]
    );

    let tree = d.ok(&["tree"]);
    assert!(tree.contains("waiting"), "{tree}");
    assert!(tree.contains("need a review"), "{tree}");
    let tj: serde_json::Value = serde_json::from_str(&d.ok(&["tree", "--json"])).unwrap();
    assert_eq!(tj[0]["status"], "waiting");
    assert_eq!(tj[0]["reported"]["message"], "need a review");

    // Once it ends, the exit speaks, not the last report.
    d.ok(&["kill", &id]);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let all = d.ok(&["tree", "--all"]);
        if all.contains("failed") {
            assert!(!all.contains("waiting"), "{all}");
            break;
        }
        assert!(Instant::now() < deadline, "{all}");
        std::thread::sleep(Duration::from_millis(50));
    }
}
