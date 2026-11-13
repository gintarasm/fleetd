//! `fleet` against a real `fleetd` on a temp state dir.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tempfile::TempDir;

/// Builds `fleetd` once and returns its path (cargo only builds this crate's
/// own binaries for integration tests).
fn fleetd_bin() -> &'static Path {
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
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

struct Daemon {
    child: Child,
    dir: TempDir,
}

impl Daemon {
    fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let child = Command::new(fleetd_bin())
            // Never the developer's real login shell.
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

    fn fleet(&self, args: &[&str]) -> Output {
        fleet(self.dir.path(), args)
    }

    /// Runs `fleet`, expecting success, and returns stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.fleet(args);
        assert!(
            out.status.success(),
            "fleet {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fleet(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fleet"))
        .arg("--state-dir")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run fleet")
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn spawn_then_ls_shows_running() {
    let d = Daemon::start();
    let id = d.ok(&["spawn", "--", "cat"]).trim().to_string();
    assert!(id.parse::<u64>().is_ok(), "id: {id:?}");
    let ls = d.ok(&["ls"]);
    let mut lines = ls.lines();
    assert_eq!(
        lines.next().unwrap().split_whitespace().collect::<Vec<_>>(),
        ["ID", "STATE", "EXIT", "PROGRAM"]
    );
    let row: Vec<_> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(row, [id.as_str(), "running", "-", "cat"]);
    assert!(lines.next().is_none());
}

#[test]
fn kill_then_ls_shows_exited() {
    let d = Daemon::start();
    let id = d.ok(&["spawn", "--", "cat"]).trim().to_string();
    d.ok(&["kill", &id]);
    wait_for("exited", || d.ok(&["ls"]).contains("exited"));
    let ls = d.ok(&["ls"]);
    let row: Vec<_> = ls.lines().nth(1).unwrap().split_whitespace().collect();
    assert_eq!(row[0], id);
    assert_eq!(row[1], "exited");
    assert_ne!(row[2], "-", "exit code shown: {ls}");
}

#[test]
fn send_then_attach_snapshot_contains_text() {
    let d = Daemon::start();
    let id = d.ok(&["spawn", "--", "cat"]).trim().to_string();
    d.ok(&["send", "--enter", &id, "hello"]);
    wait_for("snapshot with text", || {
        let out = d.fleet(&["attach", "--snapshot-only", &id]);
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("hello")
    });
}

#[test]
fn unknown_agent_errors_exit_1() {
    let d = Daemon::start();
    for args in [&["kill", "999"][..], &["attach", "--snapshot-only", "999"]] {
        let out = d.fleet(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(!out.stderr.is_empty());
    }
}

#[test]
fn fleet_logs_prints_tail() {
    let d = Daemon::start();
    let id = d.ok(&["spawn", "--", "cat"]).trim().to_string();
    wait_for("spawn logged", || {
        d.ok(&["logs"])
            .contains(&format!("agent spawned agent={id}"))
    });
    let all = d.ok(&["logs"]);
    assert!(all.contains("fleetd started"), "{all}");
    assert!(all.lines().count() <= 100, "default -n 100");
    let one = d.ok(&["logs", "-n", "1"]);
    assert_eq!(one.lines().count(), 1, "{one}");
}

#[test]
fn fleet_logs_follow_prints_new_lines() {
    let d = Daemon::start();
    let mut child = Command::new(env!("CARGO_BIN_EXE_fleet"))
        .arg("--state-dir")
        .arg(d.dir.path())
        .args(["logs", "-n", "0", "-f"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
            if String::from_utf8_lossy(&seen).contains("agent spawned") {
                break;
            }
        }
        String::from_utf8_lossy(&seen).into_owned()
    });
    // `-f` is following once its own connection's ReadLogs is answered;
    // keep spawning until a spawn shows up in the stream.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !reader.is_finished() {
        assert!(Instant::now() < deadline, "follow printed no spawn");
        d.ok(&["spawn", "--", "cat"]);
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(reader.join().unwrap().contains("agent spawned"));
}

#[test]
fn daemon_not_running_message() {
    let dir = TempDir::new().unwrap();
    let out = fleet(dir.path(), &["ls"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("fleetd is not running"));
}

/// The whole restore sequence: soft reset, pop the kitty keyboard mode (on
/// the alt screen, where the agent pushed it), leave the alt screen, then
/// reset what DECSTR does not (cursor, paste, mouse, focus, keypad, wrap).
const RESTORE: &str = concat!(
    "\x1b[!p\x1b[<u\x1b[?1049l\x1b[?25h\x1b[?2004l",
    "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l",
    "\x1b[?1l\x1b[?7h",
);

/// `fleet attach` running on a pty.
struct PtyAttach {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    reader: Option<Box<dyn Read + Send>>,
    rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    seen: Vec<u8>,
}

impl PtyAttach {
    /// Starts `fleet attach <id>`; output is only collected after `drain`
    /// (called here unless `paused`).
    fn start(d: &Daemon, id: &str, paused: bool) -> Self {
        Self::start_sized(d, id, paused, 80, 24)
    }

    /// [`PtyAttach::start`] on a `cols`x`rows` terminal.
    fn start_sized(d: &Daemon, id: &str, paused: bool, cols: u16, rows: u16) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_fleet"));
        cmd.args(["--state-dir", d.dir.path().to_str().unwrap(), "attach", id]);
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut p = PtyAttach {
            child,
            reader: Some(pair.master.try_clone_reader().unwrap()),
            writer: pair.master.take_writer().unwrap(),
            master: pair.master,
            rx: None,
            seen: Vec::new(),
        };
        if !paused {
            p.drain();
        }
        p
    }

    /// Starts collecting the pty's output.
    fn drain(&mut self) {
        let mut reader = self.reader.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        self.rx = Some(rx);
    }

    fn until(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let rx = self.rx.as_ref().unwrap();
        while !String::from_utf8_lossy(&self.seen).contains(needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(chunk) => self.seen.extend(chunk),
                Err(_) => panic!(
                    "no {needle:?} in {:?}",
                    String::from_utf8_lossy(&self.seen).into_owned()
                ),
            }
        }
    }

    /// Exit code, polled against a deadline.
    fn exit_code(&mut self) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.exit_code();
            }
            assert!(Instant::now() < deadline, "fleet attach did not exit");
            // Keep the pty drained so the child is never stuck writing.
            if let Some(rx) = &self.rx {
                while let Ok(chunk) = rx.try_recv() {
                    self.seen.extend(chunk);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Drives `fleet attach` on a pty: it enters the alt screen, shows the agent,
/// forwards typing, detaches on Ctrl-\ and writes the terminal restore
/// sequence before exiting 0.
#[test]
fn attach_on_pty_detaches_and_restores_terminal() {
    let d = Daemon::start();
    let id = d.ok(&["spawn", "--", "cat"]).trim().to_string();
    let mut p = PtyAttach::start(&d, &id, false);
    p.until("\x1b[?1049h\x1b[!p");
    p.writer.write_all(b"typed-text\r").unwrap();
    p.until("typed-text");
    p.writer.write_all(&[0x1c]).unwrap();
    p.until(RESTORE);
    assert_eq!(p.exit_code(), 0);
    // The agent keeps running after a detach.
    assert!(d.ok(&["ls"]).contains("running"));
}

#[test]
fn attach_detaches_on_encoded_ctrl_backslash() {
    // Kitty keyboard protocol and modifyOtherKeys forms of Ctrl-\, split across
    // writes; a key release does not detach.
    let d = Daemon::start();
    let id = d.ok(&["spawn", "--", "cat"]).trim().to_string();
    let detach_forms: [&[u8]; 2] = [b"\x1b[92;5:1u", b"\x1b[27;5;92~"];
    for form in detach_forms {
        let mut p = PtyAttach::start(&d, &id, false);
        p.until("\x1b[?1049h\x1b[!p");
        p.writer.write_all(b"\x1b[92;5:3u").unwrap();
        p.writer.write_all(b"still-attached\r").unwrap();
        p.until("still-attached");
        let (head, tail) = form.split_at(5);
        p.writer.write_all(head).unwrap();
        p.writer.flush().unwrap();
        std::thread::sleep(Duration::from_millis(5));
        p.writer.write_all(tail).unwrap();
        p.until(RESTORE);
        assert_eq!(p.exit_code(), 0);
    }
    assert!(d.ok(&["ls"]).contains("running"));
}

#[test]
fn attach_forwards_a_lone_escape_after_a_short_wait() {
    // A lone Esc could start an encoded Ctrl-\; it is held back briefly, then
    // forwarded although nothing follows it.
    let d = Daemon::start();
    let id = d
        .ok(&[
            "spawn",
            "--",
            "sh",
            "-c",
            "stty raw -echo; head -c 1 | od -An -tx1; sleep 5",
        ])
        .trim()
        .to_string();
    let mut p = PtyAttach::start(&d, &id, false);
    p.until("\x1b[?1049h\x1b[!p");
    std::thread::sleep(Duration::from_millis(300));
    p.writer.write_all(b"\x1b").unwrap();
    p.writer.flush().unwrap();
    p.until("1b");
    p.writer.write_all(&[0x1c]).unwrap();
    p.until(RESTORE);
    assert_eq!(p.exit_code(), 0);
}

#[test]
fn attach_exits_0_and_restores_when_agent_exits() {
    let d = Daemon::start();
    let id = d
        .ok(&["spawn", "--", "sh", "-c", "echo hi; sleep 0.3"])
        .trim()
        .to_string();
    let mut p = PtyAttach::start(&d, &id, false);
    p.until(RESTORE);
    assert_eq!(p.exit_code(), 0);
}

#[test]
fn sigterm_restores_terminal_and_exits_143() {
    let d = Daemon::start();
    let id = d.ok(&["spawn", "--", "cat"]).trim().to_string();
    let mut p = PtyAttach::start(&d, &id, false);
    p.until("\x1b[?1049h");
    let pid = p.child.process_id().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    p.until(RESTORE);
    assert_eq!(p.exit_code(), 128 + libc::SIGTERM as u32);
    assert!(d.ok(&["ls"]).contains("running"));
}

#[test]
fn window_resize_reaches_the_agent() {
    let d = Daemon::start();
    let id = d
        .ok(&["spawn", "--", "sh", "-c", "read x; stty size; sleep 5"])
        .trim()
        .to_string();
    let mut p = PtyAttach::start(&d, &id, false);
    p.until("\x1b[?1049h");
    p.master
        .resize(PtySize {
            rows: 31,
            cols: 101,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    p.writer.write_all(b"\r").unwrap();
    p.until("31 101");
    p.writer.write_all(&[0x1c]).unwrap();
    assert_eq!(p.exit_code(), 0);
}

/// A second `fleet attach` becomes primary, and the agent follows its
/// terminal's size (not the first client's).
#[test]
fn second_attach_resizes_the_agent_to_its_terminal() {
    let d = Daemon::start();
    let id = d
        .ok(&[
            "spawn",
            "--",
            "sh",
            "-c",
            "while read l; do echo \"size=$(stty size).\"; done",
        ])
        .trim()
        .to_string();
    let mut first = PtyAttach::start_sized(&d, &id, false, 80, 24);
    first.until("\x1b[?1049h");
    let mut second = PtyAttach::start_sized(&d, &id, false, 100, 30);
    second.until("\x1b[?1049h");
    second.writer.write_all(b"\r").unwrap();
    second.until("size=30 100.");
    for p in [&mut first, &mut second] {
        p.writer.write_all(&[0x1c]).unwrap();
        assert_eq!(p.exit_code(), 0);
    }
    d.ok(&["kill", &id]);
}

/// Every `ESC [ row ; col H` in `bytes`.
fn cursor_positions(bytes: &[u8]) -> Vec<(u32, u32)> {
    let text = String::from_utf8_lossy(bytes);
    text.split("\x1b[")
        .skip(1)
        .filter_map(|seq| {
            let end = seq.find(|c: char| !(c.is_ascii_digit() || c == ';'))?;
            if !seq[end..].starts_with('H') {
                return None;
            }
            let (row, col) = seq[..end].split_once(';')?;
            Some((row.parse().ok()?, col.parse().ok()?))
        })
        .collect()
}

/// Attaching from a smaller terminal than the agent's resizes the agent
/// first, so the snapshot is drawn at the client's size.
#[test]
fn attach_from_smaller_terminal_gets_snapshot_at_its_size() {
    let d = Daemon::start();
    let script = "for i in $(seq 1 20); do echo line$i; done; printf '%0150dEND' 0; sleep 30";
    let id = d
        .ok(&[
            "spawn", "--cols", "80", "--rows", "24", "--", "sh", "-c", script,
        ])
        .trim()
        .to_string();
    wait_for("agent output", || {
        let out = d.fleet(&["attach", "--snapshot-only", &id]);
        String::from_utf8_lossy(&out.stdout).contains("END")
    });
    let mut p = PtyAttach::start_sized(&d, &id, false, 40, 10);
    p.until("END");
    p.writer.write_all(&[0x1c]).unwrap();
    p.until(RESTORE);
    assert_eq!(p.exit_code(), 0);
    let seen = String::from_utf8_lossy(&p.seen).into_owned();
    let snapshot = &seen[..seen.find(RESTORE).unwrap()];
    let cups = cursor_positions(snapshot.as_bytes());
    assert!(!cups.is_empty(), "no cursor position in {snapshot:?}");
    assert!(
        cups.iter().all(|&(row, col)| row <= 10 && col <= 40),
        "{cups:?} outside 40x10 in {snapshot:?}"
    );
    d.ok(&["kill", &id]);
}

/// A terminal that stops reading must not make `fleet` buffer without bound:
/// the daemon sees the backlog, reports a lag, and attach exits 2 (once the
/// terminal drains) with the terminal restored.
#[test]
fn stalled_terminal_gets_lagged_exit_2() {
    let d = Daemon::start();
    let id = d.ok(&["spawn", "--", "yes"]).trim().to_string();
    let mut p = PtyAttach::start(&d, &id, true);
    std::thread::sleep(Duration::from_secs(3));
    p.drain();
    p.until(RESTORE);
    assert_eq!(p.exit_code(), 2);
    d.ok(&["kill", &id]);
}

#[test]
fn spawn_cwd_is_used() {
    let d = Daemon::start();
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().canonicalize().unwrap();
    let id = d
        .ok(&["spawn", "--cwd", dir_path.to_str().unwrap(), "--", "pwd"])
        .trim()
        .to_string();
    wait_for("pwd output", || {
        let out = d.ok(&["screen", &id]);
        out.contains(dir_path.file_name().unwrap().to_str().unwrap())
    });
}

#[test]
fn ls_json_parses() {
    let d = Daemon::start();
    let id = d
        .ok(&["spawn", "--label", "role=test", "--", "cat"])
        .trim()
        .to_string();
    let v: serde_json::Value = serde_json::from_str(&d.ok(&["ls", "--json"])).unwrap();
    let a = &v.as_array().unwrap()[0];
    assert_eq!(a["id"].to_string(), id);
    assert_eq!(a["state"], "running");
    assert!(a["exit_code"].is_null());
    assert_eq!(a["program"], "cat");
    assert_eq!(a["labels"]["role"], "test");
    assert!(a["created_unix_ms"].is_number());
    let none: serde_json::Value =
        serde_json::from_str(&d.ok(&["ls", "--json", "--label", "role=other"])).unwrap();
    assert!(none.as_array().unwrap().is_empty());
    d.ok(&["label", &id, "a=b", "--rm", "role"]);
    let v: serde_json::Value = serde_json::from_str(&d.ok(&["ls", "--json"])).unwrap();
    assert_eq!(v[0]["labels"]["a"], "b");
    assert!(v[0]["labels"]["role"].is_null());
    let out = d.fleet(&["label", &id, "=x"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn history_plain_prints_all_lines() {
    let d = Daemon::start();
    let id = d
        .ok(&[
            "spawn",
            "--",
            "sh",
            "-c",
            "i=1; while [ $i -le 5000 ]; do echo \"line-$i-end\"; i=$((i+1)); done",
        ])
        .trim()
        .to_string();
    wait_for("exited", || d.ok(&["ls"]).contains("exited"));
    let out = d.ok(&["history", "--plain", &id]);
    for i in 1..=5000 {
        let needle = format!("line-{i}-end");
        assert_eq!(
            out.lines().filter(|l| l.trim() == needle).count(),
            1,
            "{needle}"
        );
    }
    let last = d.ok(&["history", "--plain", "--lines", "3", &id]);
    let lines: Vec<_> = last.lines().collect();
    assert_eq!(lines.len(), 3, "{last:?}");
    assert!(lines.iter().any(|l| l.contains("line-5000-end")));
}

#[test]
fn screen_prints_visible_text() {
    let d = Daemon::start();
    let id = d.ok(&["spawn", "--", "cat"]).trim().to_string();
    d.ok(&["send", "--enter", &id, "hello"]);
    wait_for("screen text", || d.ok(&["screen", &id]).contains("hello"));
    let s = d.ok(&["screen", &id]);
    assert!(!s.contains('\x1b'));
    assert!(!s.ends_with("\n\n"), "trailing blank rows dropped: {s:?}");
}

#[test]
fn spawn_relative_cwd_is_resolved_against_cli_cwd() {
    let d = Daemon::start();
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    std::fs::create_dir(base.join("sub")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_fleet"))
        .arg("--state-dir")
        .arg(d.dir.path())
        .args(["spawn", "--cwd", "sub", "--", "pwd"])
        .current_dir(&base)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let want = base.join("sub");
    wait_for("pwd output", || {
        d.ok(&["screen", &id])
            .lines()
            .any(|l| l.trim() == want.to_str().unwrap())
    });
}

// ---- `fleet skill` -------------------------------------------------------------

/// Runs `fleet args…` with `HOME=home` in `cwd`, without a daemon.
fn fleet_home(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fleet"))
        .args(args)
        .env("HOME", home)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("run fleet")
}

fn assert_ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "status {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

const SKILL: &str = include_str!("../skill/SKILL.md");

#[test]
fn skill_install_writes_and_overwrites() {
    let home = TempDir::new().unwrap();
    let path = home.path().join(".claude/skills/fleet/SKILL.md");
    let out = assert_ok(&fleet_home(home.path(), home.path(), &["skill", "install"]));
    assert_eq!(out.trim(), path.to_str().unwrap());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("---\nname: fleet\n"), "{text:?}");
    assert_eq!(text, SKILL);
    std::fs::write(&path, "stale").unwrap();
    assert_ok(&fleet_home(home.path(), home.path(), &["skill", "install"]));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), SKILL);
}

#[test]
fn skill_install_project() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let dir = project.path().canonicalize().unwrap();
    // `--project DIR`
    let other = TempDir::new().unwrap();
    let out = assert_ok(&fleet_home(
        home.path(),
        other.path(),
        &["skill", "install", "--project", dir.to_str().unwrap()],
    ));
    let path = dir.join(".claude/skills/fleet/SKILL.md");
    assert_eq!(out.trim(), path.to_str().unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), SKILL);
    // `--project` alone: the cwd.
    let cwd = TempDir::new().unwrap();
    let cwd_path = cwd.path().canonicalize().unwrap();
    assert_ok(&fleet_home(
        home.path(),
        &cwd_path,
        &["skill", "install", "--project"],
    ));
    assert!(cwd_path.join(".claude/skills/fleet/SKILL.md").is_file());
    assert!(!home.path().join(".claude").exists());
}

#[test]
fn skill_uninstall_removes() {
    let home = TempDir::new().unwrap();
    assert_ok(&fleet_home(home.path(), home.path(), &["skill", "install"]));
    let dir = home.path().join(".claude/skills/fleet");
    assert!(dir.is_dir());
    assert_ok(&fleet_home(
        home.path(),
        home.path(),
        &["skill", "uninstall"],
    ));
    assert!(!dir.exists());
    assert!(
        home.path().join(".claude/skills").is_dir(),
        "only fleet/ goes"
    );

    let project = TempDir::new().unwrap();
    let p = project.path().to_str().unwrap();
    assert_ok(&fleet_home(
        home.path(),
        home.path(),
        &["skill", "install", "--project", p],
    ));
    assert_ok(&fleet_home(
        home.path(),
        home.path(),
        &["skill", "uninstall", "--project", p],
    ));
    assert!(!project.path().join(".claude/skills/fleet").exists());
}

#[test]
fn skill_unknown_target_errors() {
    let home = TempDir::new().unwrap();
    let out = fleet_home(
        home.path(),
        home.path(),
        &["skill", "install", "--target", "codex"],
    );
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("M4"), "{err}");
    assert!(!home.path().join(".claude").exists());
    assert_ok(&fleet_home(
        home.path(),
        home.path(),
        &["skill", "install", "--target", "claude"],
    ));
}

#[test]
fn skill_mentions_untrusted_output() {
    let lower = SKILL.to_lowercase();
    assert!(
        lower.contains("untrusted"),
        "skill lacks an untrusted-output rule"
    );
    assert!(lower.contains("not instructions"));
}

#[test]
fn skill_print_has_frontmatter() {
    let home = TempDir::new().unwrap();
    let out = assert_ok(&fleet_home(home.path(), home.path(), &["skill", "print"]));
    assert_eq!(out, SKILL);
    let rest = out.strip_prefix("---\n").expect("opening ---");
    let (front, body) = rest.split_once("\n---\n").expect("closing ---");
    let lines: Vec<_> = front.lines().collect();
    assert_eq!(lines[0], "name: fleet");
    let desc = lines[1].strip_prefix("description: ").expect("description");
    for word in ["agent", "fleet", "spawn", "output"] {
        assert!(desc.contains(word), "description lacks {word:?}: {desc}");
    }
    assert_eq!(lines.len(), 2, "{front}");
    assert!(!body.trim().is_empty());
    assert!(SKILL.lines().count() < 120);
    for needle in ["$FLEET_AGENT_ID", "--lines"] {
        assert!(SKILL.contains(needle), "skill lacks {needle}");
    }
}

/// Splits a shell command line: whitespace, '…' and "…" quoting.
fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur: Option<String> = None;
    let mut quote = None;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.get_or_insert_default().push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                cur.get_or_insert_default();
            }
            (None, c) if c.is_whitespace() => words.extend(cur.take()),
            (None, c) => cur.get_or_insert_default().push(c),
        }
    }
    assert!(quote.is_none(), "unterminated quote in {line:?}");
    words.extend(cur);
    words
}

/// The `fleet …` command lines in the skill's fenced code blocks.
fn skill_examples() -> Vec<Vec<String>> {
    let mut fenced = false;
    let mut out = Vec::new();
    for line in SKILL.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if fenced && line.trim_start().starts_with("fleet ") {
            let cmd = line.split(" #").next().unwrap();
            out.push(shell_words(cmd));
        }
    }
    out
}

/// Every documented command line runs against a real daemon: `<id>` becomes
/// a live agent, `DIR` a temp dir, `N` a number and the agent command `cat`.
#[test]
fn skill_examples_run() {
    let examples = skill_examples();
    let subs: Vec<&str> = examples.iter().map(|e| e[1].as_str()).collect();
    for (sub, flag) in [
        ("ls", "--json"),
        ("screen", ""),
        ("history", "--lines"),
        ("history", "--plain"),
        ("send", "--enter"),
        ("spawn", "--cwd"),
        ("kill", ""),
    ] {
        assert!(
            examples
                .iter()
                .any(|e| e[1] == sub && (flag.is_empty() || e.iter().any(|w| w == flag))),
            "no `fleet {sub} {flag}` example"
        );
    }
    assert!(!subs.contains(&"label"), "agents must not write labels");

    let d = Daemon::start();
    let work = TempDir::new().unwrap();
    for example in &examples {
        let id = d.ok(&["spawn", "--", "cat"]).trim().to_string();
        let mut args: Vec<String> = Vec::new();
        for w in &example[1..] {
            if args.last().is_some_and(|a| a == "--") {
                args.push("cat".into());
                break;
            }
            args.push(match w.as_str() {
                "<id>" => id.clone(),
                "DIR" => work.path().to_string_lossy().into_owned(),
                "N" => "20".into(),
                _ => w.clone(),
            });
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = d.ok(&args);
        if example[1] == "ls" {
            let v: serde_json::Value = serde_json::from_str(&out).expect("ls --json");
            assert!(v.is_array());
        }
    }
}
