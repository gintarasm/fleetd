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
