//! `fleet node exec|cat|ls` end to end (ADR 0091) against a real `fleetd`:
//! the child's output and exit code, 124 on a timeout, `cat` and `ls`, and
//! an agent's refusal (exit 3).

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
    fn start() -> Self {
        let dir = TempDir::new().unwrap();
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

#[test]
fn exec_prints_streams_and_exits_with_the_childs_code() {
    let d = Daemon::start();
    let out = d.user(&[
        "node",
        "exec",
        "--env",
        "K=v",
        "--",
        "/bin/sh",
        "-c",
        "echo out; echo err >&2; echo $K; exit 7",
    ]);
    assert_eq!(text(&out.stdout), "out\nv\n");
    assert_eq!(text(&out.stderr), "err\n");
    assert_eq!(out.status.code(), Some(7));
}

#[test]
fn exec_uses_cwd_and_the_callers_directory_by_default() {
    let d = Daemon::start();
    let dir = TempDir::new().unwrap();
    let want = dir.path().canonicalize().unwrap();
    let cwd = want.to_str().unwrap();
    let out = d.user(&["node", "exec", "--cwd", cwd, "--", "pwd", "-P"]);
    assert_eq!(text(&out.stdout), format!("{cwd}\n"));
    let here = std::env::current_dir().unwrap().canonicalize().unwrap();
    let out = d.user(&["node", "exec", "--", "pwd", "-P"]);
    assert_eq!(text(&out.stdout), format!("{}\n", here.display()));
}

#[test]
fn exec_exits_124_on_a_timeout_like_timeout_1() {
    let d = Daemon::start();
    let start = Instant::now();
    let out = d.user(&["node", "exec", "--timeout", "300ms", "--", "sleep", "30"]);
    assert_eq!(out.status.code(), Some(124), "{}", text(&out.stderr));
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[test]
fn exec_of_a_missing_program_fails() {
    let d = Daemon::start();
    let out = d.user(&["node", "exec", "--", "no-such-program-0091"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("not found"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn cat_prints_a_file_and_ls_lists_a_directory() {
    let d = Daemon::start();
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let file = dir.path().join("a.txt");
    let out = d.user(&["node", "cat", file.to_str().unwrap()]);
    assert_eq!(text(&out.stdout), "hello\n");

    let out = d.user(&["node", "ls", dir.path().to_str().unwrap()]);
    let lines: Vec<String> = text(&out.stdout).lines().map(str::to_owned).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].ends_with("a.txt") && lines[0].starts_with("file"),
        "{lines:?}"
    );
    assert!(
        lines[1].ends_with("sub/") && lines[1].starts_with("dir"),
        "{lines:?}"
    );

    let out = d.user(&["node", "ls", "--json", dir.path().to_str().unwrap()]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let a = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "a.txt")
        .unwrap();
    assert_eq!(
        (a["kind"].as_str(), a["size"].as_u64()),
        (Some("file"), Some(6))
    );
    assert_eq!(v["truncated"], false);
}

#[test]
fn cat_of_a_missing_file_fails() {
    let d = Daemon::start();
    let out = d.user(&["node", "cat", "/no/such/file/0091"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn an_agent_is_refused_with_exit_3() {
    let d = Daemon::start();
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
    for args in [
        &["node", "exec", "--", "true"][..],
        &["node", "cat", "/etc/hosts"],
        &["node", "ls", "/tmp"],
    ] {
        let out = d.fleet(Some(&token), args);
        assert_eq!(
            out.status.code(),
            Some(3),
            "{args:?}: {}",
            text(&out.stderr)
        );
    }
}
