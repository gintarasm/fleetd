//! Agent identity and scope end to end (ADR 0072, spec §1): agents run the
//! real `fleet` against a real `fleetd`, inside their sessions, with and
//! without their tokens.

use std::collections::BTreeSet;
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

/// The ids in `fleet ls --json` output.
fn ids(json: &str) -> BTreeSet<u64> {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}: {json}"));
    v.as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_u64().unwrap())
        .collect()
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
fn an_agents_fleet_is_scoped_by_its_session_and_its_token() {
    let d = Daemon::start("");
    // The user's own agent, and agent B, which publishes its token.
    let u = d.ok(&["spawn", "--", "sleep", "600"]);
    std::fs::write(d.path("u.id"), &u).unwrap();
    let b = d.script(
        "b.sh",
        "printf %s \"$FLEET_AGENT_TOKEN\" > $S/b.tok.tmp; mv $S/b.tok.tmp $S/b.tok; exec sleep 600",
    );
    let b = d.ok(&["spawn", "--", "sh", &b]);
    d.read("b.tok");

    // C, a child of P, spawns G and then runs `fleet` every way.
    let c = d.script(
        "c.sh",
        &[
            capture("$F spawn --name grand -- sleep 600", "g.id"),
            capture("$F ls --json", "c.ls"),
            capture("env -u FLEET_AGENT_TOKEN $F ls --json", "c.ls.notoken"),
            // A helper that left C's session: its token still says C.
            capture(
                "perl -MPOSIX -e 'POSIX::setsid() or die; exec @ARGV' -- $F ls --json",
                "c.ls.setsid",
            ),
            capture("FLEET_AGENT_TOKEN=$(cat $S/b.tok) $F ls --json", "c.cross"),
            capture("$F kill $(cat $S/u.id)", "c.kill.user"),
            capture("$F screen $(cat $S/g.id)", "c.screen.child"),
            capture("$F label $(cat $S/g.id) x=y", "c.label"),
            capture("$F logs -n 1", "c.logs"),
            capture("$F daemon restart", "c.restart"),
            "exec sleep 600".into(),
        ]
        .join("\n"),
    );
    let p = d.script(
        "p.sh",
        &[
            capture(&format!("$F spawn --name kid -- sh {c}"), "c.id"),
            // Waits for C to finish its checks.
            "while [ ! -f $S/c.restart.code ]; do sleep 0.05; done".into(),
            capture("$F ls --json", "p.ls"),
            "exec sleep 600".into(),
        ]
        .join("\n"),
    );
    let p = d.ok(&["spawn", "--label", "project=x", "--", "sh", &p]);
    let id = |s: &str| s.parse::<u64>().unwrap();
    let (u, b, p) = (id(&u), id(&b), id(&p));
    let c = id(&d.read("c.id"));
    let g = id(&d.read("g.id"));

    // C sees only G: with its token, without it, and from a helper
    // outside its session that kept the token.
    for out in ["c.ls", "c.ls.notoken", "c.ls.setsid"] {
        assert_eq!(
            d.read(&format!("{out}.code")),
            "0",
            "{out}: {}",
            d.read(&format!("{out}.err"))
        );
        assert_eq!(ids(&d.read(out)), BTreeSet::from([g]), "{out}");
    }
    // B's token inside C's session is refused at the handshake.
    assert_eq!(d.read("c.cross.code"), "1");
    assert!(
        d.read("c.cross.err").contains("rejected"),
        "{}",
        d.read("c.cross.err")
    );
    // Outside its subtree, and label/log/daemon commands: exit 3.
    for out in ["c.kill.user", "c.label", "c.logs", "c.restart"] {
        assert_eq!(
            d.read(&format!("{out}.code")),
            "3",
            "{out}: {}",
            d.read(&format!("{out}.err"))
        );
        assert!(
            d.read(&format!("{out}.err")).contains("forbidden:"),
            "{out}"
        );
    }
    assert_eq!(
        d.read("c.screen.child.code"),
        "0",
        "{}",
        d.read("c.screen.child.err")
    );
    // P sees its subtree.
    assert_eq!(ids(&d.read("p.ls")), BTreeSet::from([c, g]));
    // The user sees everything; the user's agent was not killed.
    let all = d.ok(&["ls", "--json"]);
    assert_eq!(ids(&all), BTreeSet::from([u, b, p, c, g]));
    let v: serde_json::Value = serde_json::from_str(&all).unwrap();
    let agent = |id: u64| {
        v.as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(agent(u)["state"], "running");
    // Parents and inherited labels, set by fleetd; the token never shows.
    assert_eq!(agent(c)["labels"]["fleet.parent"], p.to_string());
    assert_eq!(agent(g)["labels"]["fleet.parent"], c.to_string());
    assert_eq!(agent(g)["labels"]["project"], "x");
    assert_eq!(agent(g)["labels"]["name"], "grand");
    assert!(agent(p)["labels"].get("fleet.parent").is_none());
    assert!(!all.contains(&d.read("b.tok")));
}

#[test]
fn an_agents_spawn_past_a_quota_exits_3_but_the_users_does_not() {
    let d = Daemon::start("[node]\nmax_children = 1\n");
    let p = d.script(
        "p.sh",
        &[
            capture("$F spawn -- sleep 600", "first"),
            capture("$F spawn -- sleep 600", "second"),
            "exec sleep 600".into(),
        ]
        .join("\n"),
    );
    let p = d.ok(&["spawn", "--", "sh", &p]);
    assert_eq!(d.read("first.code"), "0", "{}", d.read("first.err"));
    assert_eq!(d.read("second.code"), "3");
    let err = d.read("second.err");
    assert!(err.contains("forbidden: quota max_children = 1"), "{err}");
    // The user is never refused.
    for _ in 0..3 {
        d.ok(&["spawn", "--", "sleep", "600"]);
    }
    assert_eq!(ids(&d.ok(&["ls", "--json"])).len(), 5, "p {p}");
}

#[test]
fn identities_survive_a_daemon_restart() {
    let d = Daemon::start("");
    let other = d.ok(&["spawn", "--", "sleep", "600"]);
    // C waits for the restart, then runs `fleet` with and without its
    // token, and from outside its session with it.
    let c = d.script(
        "c.sh",
        &[
            capture("$F spawn -- sleep 600", "g.id"),
            "while [ ! -f $S/go ]; do sleep 0.05; done".into(),
            capture("$F ls --json", "c.ls"),
            capture("env -u FLEET_AGENT_TOKEN $F ls --json", "c.ls.notoken"),
            capture(
                "perl -MPOSIX -e 'POSIX::setsid() or die; exec @ARGV' -- $F ls --json",
                "c.ls.setsid",
            ),
            capture(&format!("$F kill {other}"), "c.kill"),
            "exec sleep 600".into(),
        ]
        .join("\n"),
    );
    let p = d.script(
        "p.sh",
        &[
            capture(&format!("$F spawn -- sh {c}"), "c.id"),
            "exec sleep 600".into(),
        ]
        .join("\n"),
    );
    d.ok(&["spawn", "--", "sh", &p]);
    let g: u64 = d.read("g.id").parse().unwrap();
    let out = d.ok(&[
        "daemon",
        "restart",
        "--binary",
        fleetd_bin().to_str().unwrap(),
    ]);
    assert!(out.starts_with("fleetd restarted: 4 agents kept"), "{out}");
    std::fs::write(d.path("go"), "").unwrap();
    for out in ["c.ls", "c.ls.notoken", "c.ls.setsid"] {
        assert_eq!(
            d.read(&format!("{out}.code")),
            "0",
            "{out}: {}",
            d.read(&format!("{out}.err"))
        );
        assert_eq!(ids(&d.read(out)), BTreeSet::from([g]), "{out}");
    }
    assert_eq!(d.read("c.kill.code"), "3");
}
