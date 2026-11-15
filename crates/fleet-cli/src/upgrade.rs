//! `fleet daemon upgrade` (hidden): what `fleetd` does when another fleetd
//! already holds the instance lock. `fleetd` execs this with its own path
//! (ADR R13: the flow lives here so fleetd's binary and idle RSS do not grow).
//! It tells a daemon of the same build from one of another, and offers (or,
//! with `--yes`, does) the restart onto `--binary`; it stops a daemon too old
//! to restart itself (`--force-stop`) and then execs `<binary> --state-dir
//! <dir>` so the new fleetd takes over in the user's terminal.
//!
//! Decisions, in the code's order:
//! - A daemon is "too old" when its handshake names no binary (every
//!   restart-capable daemon reports one) or when it answers `RestartDaemon`
//!   with "empty request". Both lead to the stop flow.
//! - Prompts need stdin and stderr to be terminals; otherwise the flags are
//!   the only way to act, and without them fleetd explains and exits 2.
//! - SIGTERM only, never SIGKILL: a daemon that ignores it (this happened) is
//!   named by pid so the user can decide. "Stopped" means the pid is gone:
//!   the lock is then free, and `startup` replaces a leftover socket.

use std::io::{BufRead, IsTerminal};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use fleet_client::{Connection, Restart, restart_daemon};
use fleet_proto::{AgentState, BinaryIdentity, ListAgents, request, response};

/// How long a stopped daemon gets to exit after SIGTERM.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// Today's message, for a daemon that is the same build (or that we cannot
/// look at).
const ALREADY_RUNNING: &str = "fleetd already running";

/// The command-line flags that apply here (`--yes` is fleetd's `--upgrade`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    /// `--yes`: restart a daemon of another build onto this one, no prompt.
    pub upgrade: bool,
    /// `--force-stop`: stop a daemon too old to restart itself, no prompt.
    pub force_stop: bool,
}

/// What the running daemon is, compared with this binary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Running {
    Same,
    Different,
    TooOld,
}

/// A yes/no question to the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Question {
    Restart,
    Stop,
}

/// What to do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Print [`ALREADY_RUNNING`], exit 2.
    AlreadyRunning,
    /// Another build, nobody to ask and no flag: say how, exit 2.
    ExplainDifferent,
    /// Too old, nobody to ask and no flag: say how, exit 2.
    ExplainTooOld,
    Ask(Question),
    Restart,
    Stop,
    /// The user said no: leave the daemon alone, exit 2.
    LeaveAlone,
}

impl Question {
    /// The decision after the user answered.
    pub fn answered(self, yes: bool) -> Decision {
        match (self, yes) {
            (_, false) => Decision::LeaveAlone,
            (Question::Restart, true) => Decision::Restart,
            (Question::Stop, true) => Decision::Stop,
        }
    }
}

/// The first decision, before any prompt. `interactive`: stdin and stderr
/// are both terminals.
pub fn decide(running: Running, flags: Flags, interactive: bool) -> Decision {
    match running {
        Running::Same => Decision::AlreadyRunning,
        Running::Different if flags.upgrade => Decision::Restart,
        Running::Different if interactive => Decision::Ask(Question::Restart),
        Running::Different => Decision::ExplainDifferent,
        Running::TooOld if flags.force_stop => Decision::Stop,
        Running::TooOld if interactive => Decision::Ask(Question::Stop),
        Running::TooOld => Decision::ExplainTooOld,
    }
}

/// What to do after the old daemon was stopped.
pub enum Next {
    /// Exit with this code.
    Exit(u8),
    /// The old daemon is gone: exec the new fleetd.
    Start,
}

/// The identity of the file at `path`, as `fleetd` reports its own.
fn identity_of(path: &str) -> std::io::Result<BinaryIdentity> {
    let m = std::fs::metadata(path)?;
    Ok(BinaryIdentity {
        path: path.to_string(),
        dev: m.dev(),
        ino: m.ino(),
        size: m.size(),
        mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
    })
}

fn classify(theirs: &BinaryIdentity, mine: &BinaryIdentity) -> Running {
    if theirs.path.is_empty() {
        Running::TooOld
    } else if theirs == mine {
        Running::Same
    } else {
        Running::Different
    }
}

/// Test hook (debug builds): `FLEETD_TEST_UPGRADE_ANSWER` stands for a
/// terminal and the user's answer to every prompt (`y` or `n`), so tests
/// need no TTY.
fn test_answer() -> Option<String> {
    #[cfg(debug_assertions)]
    {
        std::env::var("FLEETD_TEST_UPGRADE_ANSWER").ok()
    }
    #[cfg(not(debug_assertions))]
    None
}

fn is_interactive() -> bool {
    test_answer().is_some() || (std::io::stdin().is_terminal() && std::io::stderr().is_terminal())
}

/// Prints `prompt` to stderr and reads the answer: `y` or `yes` in any case
/// is yes, anything else (including end of input) is no.
fn ask(prompt: &str) -> bool {
    eprintln!("{prompt}");
    let line = match test_answer() {
        Some(a) => a,
        None => {
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            line
        }
    };
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// The number of `running` agents in the daemon's `ListAgents` reply.
fn running_agents(conn: &mut Connection) -> Option<usize> {
    match conn.call(request::Kind::List(ListAgents::default())) {
        Ok(response::Kind::Agents(list)) => Some(
            list.agents
                .iter()
                .filter(|a| a.state() == AgentState::Running)
                .count(),
        ),
        _ => None,
    }
}

/// Handles the lock being held by another fleetd. Prints what it did (stdout
/// for a successful restart, stderr otherwise).
pub fn run(state_dir: &Path, binary: &str, flags: Flags) -> u8 {
    match on_already_running(state_dir, binary, flags) {
        Next::Exit(code) => code,
        Next::Start => {
            // Only returns if the exec failed.
            let err = Command::new(binary)
                .arg("--state-dir")
                .arg(state_dir)
                .exec();
            eprintln!("fleetd: cannot start {binary}: {err}");
            1
        }
    }
}

fn on_already_running(state_dir: &Path, binary: &str, flags: Flags) -> Next {
    let sock = state_dir.join("fleetd.sock");
    let plain = || {
        eprintln!("{ALREADY_RUNNING}");
        Next::Exit(2)
    };
    // The lock is held but the socket does not answer: nothing to compare.
    let Ok(conn) = Connection::unix(&sock, "fleetd") else {
        return plain();
    };
    // Cannot identify the new binary: nothing to compare with.
    let Ok(mine) = identity_of(binary) else {
        return plain();
    };
    let running = classify(
        conn.hello().binary.as_ref().unwrap_or(&Default::default()),
        &mine,
    );
    let mut decision = decide(running, flags, is_interactive());
    let mut conn = Some(conn);
    loop {
        match decision {
            Decision::AlreadyRunning => return plain(),
            Decision::ExplainDifferent => {
                eprintln!(
                    "{ALREADY_RUNNING} (a different build; run fleetd --upgrade to restart it onto this one)"
                );
                return Next::Exit(2);
            }
            Decision::LeaveAlone => {
                eprintln!("left the running fleetd as it is");
                return Next::Exit(2);
            }
            Decision::ExplainTooOld => {
                eprintln!("{}", too_old_line(conn.as_mut()));
                eprintln!("run fleetd --force-stop to stop it and start this one");
                return Next::Exit(2);
            }
            Decision::Ask(Question::Restart) => {
                let theirs = conn
                    .as_ref()
                    .and_then(|c| c.hello().binary.as_ref())
                    .map_or("an unknown binary", |b| b.path.as_str())
                    .to_string();
                let agents = conn.as_mut().and_then(running_agents).unwrap_or(0);
                let yes = ask(&format!(
                    "fleetd is already running a different build ({theirs}, {agents} agents).\n\
                     Restart it onto this binary? Agents keep running. [y/N]"
                ));
                decision = Question::Restart.answered(yes);
            }
            Decision::Ask(Question::Stop) => {
                eprintln!("{}", too_old_line(conn.as_mut()));
                decision = Question::Stop.answered(ask("Stop it? [y/N]"));
            }
            Decision::Restart => match conn.take() {
                Some(c) => match restart_daemon(c, &sock, "fleetd", &mine.path) {
                    Ok(Restart::TooOld) => {
                        // Said "empty request": it cannot restart itself.
                        conn = Connection::unix(&sock, "fleetd").ok();
                        decision = decide(Running::TooOld, flags, is_interactive());
                    }
                    Ok(Restart::Outcome(done)) if done.is_success() => {
                        println!("{done}");
                        return Next::Exit(0);
                    }
                    Ok(Restart::Outcome(other)) => {
                        eprintln!("{other}");
                        return Next::Exit(1);
                    }
                    Err(e) => {
                        eprintln!("fleetd: {e}");
                        return Next::Exit(1);
                    }
                },
                None => {
                    eprintln!("fleetd: the running fleetd went away; run fleetd again");
                    return Next::Exit(1);
                }
            },
            Decision::Stop => return stop_old(state_dir),
        }
    }
}

fn too_old_line(conn: Option<&mut Connection>) -> String {
    let agents = conn.and_then(running_agents).unwrap_or(0);
    format!("the running fleetd is too old to restart itself; stopping it ends its {agents} agents")
}

/// The pids holding `lock`, found with `lsof -t`, else `fuser`. `Err` is the
/// line to print.
fn lock_holders(lock: &Path) -> Result<Vec<i32>, String> {
    // `fuser` takes no `-t`: it prints the pids on stdout (and the file name
    // on stderr), so only stdout is read for both.
    let out = match Command::new("lsof").arg("-t").arg(lock).output() {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match Command::new("fuser").arg(lock).output() {
                Ok(o) => o,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(
                        "cannot find which process runs fleetd: neither lsof nor fuser \
                                is installed; stop fleetd by hand (pkill fleetd)"
                            .into(),
                    );
                }
                Err(e) => return Err(format!("cannot run fuser: {e}")),
            }
        }
        Err(e) => return Err(format!("cannot run lsof: {e}")),
    };
    let me = std::process::id() as i32;
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|p| {
            p.trim_end_matches(|c: char| !c.is_ascii_digit())
                .parse()
                .ok()
        })
        .filter(|&p| p != me)
        .collect())
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// Stops the lock holder with SIGTERM and waits for it to exit.
fn stop_old(state_dir: &Path) -> Next {
    let lock = state_dir.join("fleetd.lock");
    let pid = match lock_holders(&lock).as_deref() {
        Ok([pid]) => *pid,
        Ok([]) => {
            eprintln!(
                "fleetd: no process holds {}; stop fleetd by hand",
                lock.display()
            );
            return Next::Exit(1);
        }
        Ok(many) => {
            let many: Vec<_> = many.iter().map(i32::to_string).collect();
            eprintln!(
                "fleetd: more than one process holds {} ({}); stop fleetd by hand",
                lock.display(),
                many.join(" ")
            );
            return Next::Exit(1);
        }
        Err(msg) => {
            eprintln!("fleetd: {msg}");
            return Next::Exit(1);
        }
    };
    // SAFETY: a plain signal to the process that holds our lock file.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + STOP_WAIT;
    while pid_alive(pid) {
        if Instant::now() >= deadline {
            eprintln!(
                "the old fleetd (pid {pid}) did not stop; stop it with kill -9 {pid} and run fleetd again"
            );
            return Next::Exit(1);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("stopped the old fleetd");
    Next::Start
}

#[cfg(test)]
mod tests {
    use super::*;
    use Decision::*;

    fn id(path: &str, ino: u64) -> BinaryIdentity {
        BinaryIdentity {
            path: path.into(),
            ino,
            ..Default::default()
        }
    }

    #[test]
    fn classify_table() {
        let mine = id("/a/fleetd", 1);
        assert_eq!(classify(&id("/a/fleetd", 1), &mine), Running::Same);
        assert_eq!(classify(&id("/a/fleetd", 2), &mine), Running::Different);
        assert_eq!(classify(&id("/b/fleetd", 1), &mine), Running::Different);
        assert_eq!(classify(&BinaryIdentity::default(), &mine), Running::TooOld);
    }

    #[test]
    fn decide_table() {
        let none = Flags::default();
        let up = Flags {
            upgrade: true,
            ..none
        };
        let force = Flags {
            force_stop: true,
            ..none
        };
        let both = Flags {
            upgrade: true,
            force_stop: true,
        };
        // (running, flags, interactive, expected)
        let cases = [
            (Running::Same, none, true, AlreadyRunning),
            (Running::Same, both, true, AlreadyRunning),
            (Running::Different, none, true, Ask(Question::Restart)),
            (Running::Different, none, false, ExplainDifferent),
            (Running::Different, up, false, Restart),
            (Running::Different, up, true, Restart),
            // --force-stop is for old daemons only.
            (Running::Different, force, false, ExplainDifferent),
            (Running::TooOld, none, true, Ask(Question::Stop)),
            (Running::TooOld, none, false, ExplainTooOld),
            (Running::TooOld, force, false, Stop),
            (Running::TooOld, force, true, Stop),
            // --upgrade never stops a daemon.
            (Running::TooOld, up, false, ExplainTooOld),
        ];
        for (running, flags, interactive, want) in cases {
            assert_eq!(
                decide(running, flags, interactive),
                want,
                "{running:?} {flags:?} interactive={interactive}"
            );
        }
    }

    #[test]
    fn answers() {
        assert_eq!(Question::Restart.answered(true), Restart);
        assert_eq!(Question::Restart.answered(false), LeaveAlone);
        assert_eq!(Question::Stop.answered(true), Stop);
        assert_eq!(Question::Stop.answered(false), LeaveAlone);
    }
}
