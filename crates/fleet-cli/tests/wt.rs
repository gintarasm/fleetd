//! `fleet wt` end to end (ADR 0093) against a real `fleetd` and a temp
//! repository (no network: `fetch_base = false`): new, diff (unified and
//! `--stat`), ls, a refused archive (exit 1) and a forced one, prune, the
//! `worktree` alias, and the usage and scope exit codes.

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
    fn start(config: &str) -> Self {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        let child = Command::new(fleetd_bin())
            .env("FLEETD_LOGIN_ENV", "off")
            // The daemon's git (through Exec) never reads the user's config.
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
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

    fn fleet(&self, token: Option<&str>, args: &[&str]) -> Output {
        let mut cmd = Command::new(FLEET);
        cmd.env_remove("FLEET_AGENT_TOKEN");
        if let Some(t) = token {
            cmd.env("FLEET_AGENT_TOKEN", t);
        }
        cmd.arg("--state-dir")
            .arg(self.dir.path())
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("run fleet")
    }

    fn user(&self, args: &[&str]) -> Output {
        self.fleet(None, args)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
}

/// A repository with one commit, and a daemon putting worktrees under
/// `<tmp>/wt`.
fn setup() -> (TempDir, PathBuf, Daemon) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    git(&root, &["init", "-q", "-b", "main", "repo"]);
    let main = root.join("repo");
    std::fs::write(main.join("a.txt"), "a\n").unwrap();
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "init"]);
    let config = format!(
        "[workflows.git-worktree]\nroot = \"{}/wt\"\nfetch_base = false\n",
        root.display()
    );
    let d = Daemon::start(&config);
    (tmp, main, d)
}

fn wait_exited(d: &Daemon, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = d.user(&["ls", "--json"]);
        let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let exited = list.as_array().unwrap().iter().any(|a| {
            a["id"].as_u64().map(|n| n.to_string()).as_deref() == Some(id)
                && a["state"].as_str() == Some("exited")
        });
        if exited {
            return;
        }
        assert!(Instant::now() < deadline, "agent {id} did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn new_diff_ls_archive_and_prune() {
    let (_tmp, main, d) = setup();
    let repo = main.to_string_lossy().into_owned();
    let out = d.user(&[
        "wt",
        "new",
        "s1",
        "--repo",
        &repo,
        "--name",
        "kid",
        "--",
        "/bin/sh",
        "-c",
        "echo hi > n.txt",
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let id = text(&out.stdout).trim().to_owned();
    assert!(
        text(&out.stderr).contains("fleet/s1"),
        "{}",
        text(&out.stderr)
    );
    wait_exited(&d, &id);

    let out = d.user(&["wt", "diff", "kid", "--stat"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stat = text(&out.stdout);
    assert!(stat.contains(" n.txt | 1 +"), "{stat}");
    assert!(stat.contains("1 file changed, 1 insertion(+)"), "{stat}");
    let out = d.user(&["wt", "diff", "kid"]);
    let diff = text(&out.stdout);
    assert!(
        diff.contains("diff --git a/n.txt b/n.txt") && diff.contains("\n+hi\n"),
        "{diff}"
    );
    assert!(!diff.contains('\x1b'), "no colour when not a terminal");

    let out = d.user(&["wt", "ls", "--repo", &repo]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("fleet/s1"),
        "{}",
        text(&out.stdout)
    );

    let out = d.user(&["wt", "archive", "kid"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let err = text(&out.stderr);
    assert!(err.contains("refused") && err.contains("--force"), "{err}");
    let out = d.user(&["worktree", "archive", "kid", "--force"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!main.parent().unwrap().join("wt/s1").exists());

    let out = d.user(&["wt", "prune", "--repo", &repo, "--dry-run"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
}

#[test]
fn exit_codes_for_not_found_usage_and_scope() {
    let (_tmp, main, d) = setup();
    let repo = main.to_string_lossy().into_owned();
    let out = d.user(&["wt", "diff", "nosuch"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let out = d.user(&["wt", "new", "bad/slug", "--repo", &repo, "--", "sh"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));

    let tok = d.dir.path().join("tok");
    let script = format!(
        "printf %s \"$FLEET_AGENT_TOKEN\" > {0}.tmp && mv {0}.tmp {0}; exec sleep 600",
        tok.display()
    );
    let out = d.user(&["spawn", "/bin/sh", "-c", &script]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let deadline = Instant::now() + Duration::from_secs(30);
    let token = loop {
        if let Ok(t) = std::fs::read_to_string(&tok) {
            break t;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = d.fleet(Some(&token), &["wt", "prune", "--repo", &repo]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
}
