//! `fleet pending` / `approve` / `deny` end to end (ADR 0079): an agent's
//! `fleet spawn` waits until the user answers with the real CLI.

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

/// Polls `fleet pending --json` until it lists `n` spawns.
fn pending(d: &Daemon, n: usize) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let v: Vec<serde_json::Value> =
            serde_json::from_str(&d.ok(&["pending", "--json"])).unwrap();
        if v.len() == n {
            return v;
        }
        assert!(Instant::now() < deadline, "{v:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_user_approves_or_denies_an_agents_spawn() {
    let d = Daemon::start("[node]\nspawn_approval = true\n");
    assert_eq!(d.ok(&["pending"]), "no spawns are waiting for approval");
    let a = d.script(
        "a.sh",
        &[
            capture("$F pending", "agent.pending"),
            capture("$F spawn --name kid -- sleep 600", "first"),
            capture("$F spawn -- sleep 600", "second"),
            "exec sleep 600".into(),
        ]
        .join("\n"),
    );
    let lead = d.ok(&["spawn", "--name", "lead", "--", "sh", &a]);
    // Only the user approves.
    assert_eq!(d.read("agent.pending.code"), "3");

    let p = pending(&d, 1).remove(0);
    assert_eq!(p["parent"].to_string(), lead);
    assert_eq!(p["program"], "sleep");
    assert_eq!(p["labels"]["name"], "kid");
    let text = d.ok(&["pending"]);
    assert!(
        text.contains(&format!("agent {lead} (lead) asks to run: sleep 600")),
        "{text}"
    );
    let id = p["id"].to_string();
    d.ok(&["approve", &id]);
    assert_eq!(d.read("first.code"), "0");
    let kid = d.read("first");
    let tree = d.ok(&["tree"]);
    assert!(tree.contains(&kid) && tree.contains("kid"), "{tree}");

    let p = pending(&d, 1).remove(0);
    d.ok(&["deny", &p["id"].to_string()]);
    assert_eq!(d.read("second.code"), "3");
    assert!(
        d.read("second.err").contains("spawn not approved"),
        "{}",
        d.read("second.err")
    );
    // Not pending any more.
    assert_eq!(d.fleet(&["approve", &id]).status.code(), Some(1));
}
