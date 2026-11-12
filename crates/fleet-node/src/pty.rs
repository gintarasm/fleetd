//! PTY host: spawns agents in pseudo-terminals and drains their output.
//!
//! M0 counts and discards output; M1 will feed it into a terminal grid.

use portable_pty::{ChildKiller, CommandBuilder, PtySize, native_pty_system};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const READ_BUF: usize = 64 * 1024;

/// How to launch an agent process.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentStats {
    pub bytes_read: u64,
    /// `None` while running. Once finished it is always `Some`: the process
    /// exit code, or the status reported by the PTY layer when the child was
    /// killed by a signal (non-zero; 128 + signal number on Unix).
    pub exit_code: Option<i32>,
}

/// `wait_all` gave up before every agent finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitTimeout;

impl std::fmt::Display for WaitTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("timed out waiting for agents to exit")
    }
}

impl std::error::Error for WaitTimeout {}

struct Agent {
    stats: Mutex<AgentStats>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

/// Shared "an agent finished" signal for `wait_all`.
#[derive(Default)]
struct Signal {
    running: Mutex<usize>,
    cv: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Open a PTY, retrying briefly: macOS occasionally reports ENXIO under
/// concurrent opens while PTY devices are being recycled.
fn open_pty(size: PtySize) -> std::io::Result<portable_pty::PtyPair> {
    let mut attempt = 0;
    loop {
        match native_pty_system().openpty(size) {
            Ok(pair) => return Ok(pair),
            Err(e) if attempt >= 5 => return Err(std::io::Error::other(e)),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(20 * attempt));
            }
        }
    }
}

#[derive(Default)]
pub struct PtyHost {
    next_id: AtomicU64,
    agents: Mutex<HashMap<AgentId, Arc<Agent>>>,
    signal: Arc<Signal>,
}

impl PtyHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spawn(&self, spec: CommandSpec) -> std::io::Result<AgentId> {
        let pair = open_pty(PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(std::io::Error::other)?;
        // Close our copy of the slave so readers see EOF once the child exits.
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(std::io::Error::other)?;

        let id = AgentId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let agent = Arc::new(Agent {
            stats: Mutex::new(AgentStats {
                bytes_read: 0,
                exit_code: None,
            }),
            killer: Mutex::new(child.clone_killer()),
        });
        *lock(&self.signal.running) += 1;
        lock(&self.agents).insert(id, Arc::clone(&agent));

        let signal = Arc::clone(&self.signal);
        let spawned = std::thread::Builder::new()
            .name(format!("pty-reader-{}", id.0))
            .spawn(move || {
                let master = pair.master; // keep the PTY open until drained
                let mut buf = vec![0u8; READ_BUF];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => lock(&agent.stats).bytes_read += n as u64,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break, // EIO on Linux once the slave closes
                    }
                }
                let code = match child.wait() {
                    Ok(status) => status.exit_code() as i32,
                    Err(_) => -1,
                };
                lock(&agent.stats).exit_code = Some(code);
                drop(master);
                *lock(&signal.running) -= 1;
                signal.cv.notify_all();
            });
        if let Err(e) = spawned {
            lock(&self.agents).remove(&id);
            *lock(&self.signal.running) -= 1;
            return Err(e);
        }
        Ok(id)
    }

    pub fn stats(&self, id: AgentId) -> Option<AgentStats> {
        let agent = lock(&self.agents).get(&id).cloned()?;
        let stats = *lock(&agent.stats);
        Some(stats)
    }

    pub fn kill(&self, id: AgentId) -> std::io::Result<()> {
        let agent = lock(&self.agents)
            .get(&id)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "unknown agent"))?;
        lock(&agent.killer).kill()
    }

    /// Block until every spawned agent has exited and been fully drained.
    pub fn wait_all(&self, timeout: Duration) -> Result<(), WaitTimeout> {
        let deadline = Instant::now() + timeout;
        let mut running = lock(&self.signal.running);
        while *running > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(WaitTimeout);
            }
            running = self
                .signal
                .cv
                .wait_timeout(running, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Ok(())
    }
}
