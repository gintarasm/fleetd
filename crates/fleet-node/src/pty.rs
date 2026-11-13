//! PTY host: spawns agents in pseudo-terminals and drains their output.
//!
//! Each agent gets a reader thread that counts its output and hands it to an
//! [`OutputSink`]; input, resize and kill go through [`PtyHost`].

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
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

/// Receives an agent's PTY output on its reader thread.
///
/// Calls arrive in order on one thread: `on_spawn` once before any output,
/// `on_output` per read, then `on_exit` once after the output is drained and
/// the child reaped. Implementations must not block for long: the agent stalls
/// while its output is not read.
pub trait OutputSink: Send + 'static {
    /// Hands the sink a handle to the agent's input (e.g. to answer terminal
    /// queries). The default ignores it.
    fn on_spawn(&mut self, _input: AgentInput) {}
    fn on_output(&mut self, bytes: &[u8]);
    fn on_exit(&mut self, code: i32);
}

/// Discards output; `PtyHost::spawn` still counts it in [`AgentStats`].
struct DiscardSink;

impl OutputSink for DiscardSink {
    fn on_output(&mut self, _bytes: &[u8]) {}
    fn on_exit(&mut self, _code: i32) {}
}

/// Writes to an agent's PTY input; cheap to clone. Fails once the agent has
/// exited.
///
/// Input and replies go through our own duplicate of the master fd, never
/// through portable-pty's `take_writer()`: its writer sends `\n` + VEOF with a
/// blocking write when dropped, which hangs forever on Linux once the slave is
/// closed and the input queue is full. The reader thread reads from the same
/// duplicate, so a running agent costs three fds: the master, this duplicate
/// and its scrollback file.
#[derive(Clone)]
pub struct AgentInput(Arc<InputPort>);

struct InputPort {
    /// The duplicate of the master fd, shared with the reader thread; `None`
    /// once closed. The reader therefore sees the O_NONBLOCK flag set while
    /// a reply is written, and retries on EAGAIN.
    file: Mutex<Option<Arc<File>>>,
    /// Set by [`AgentInput::close`]. A writer that holds the lock while it is
    /// set closes the fd itself, so closing never waits for a blocked write.
    closed: AtomicBool,
}

impl AgentInput {
    /// The input and the reader's handle to the same duplicate of the master.
    fn new(master: &dyn MasterPty) -> io::Result<(Self, Arc<File>)> {
        let fd = master
            .as_raw_fd()
            .ok_or_else(|| io::Error::other("PTY master has no fd"))?;
        // SAFETY: the master owns this fd and outlives the borrow.
        let file = File::from(unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?);
        let file = Arc::new(file);
        let input = Self(Arc::new(InputPort {
            file: Mutex::new(Some(Arc::clone(&file))),
            closed: AtomicBool::new(false),
        }));
        Ok((input, file))
    }

    /// Writes all of `bytes`, blocking while the agent's input queue is full.
    pub fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let written = {
            let file = lock(&self.0.file);
            let file = file.as_ref().ok_or_else(exited)?;
            (&**file).write_all(bytes)
        };
        if self.0.closed.load(Ordering::SeqCst) {
            self.release();
        }
        written
    }

    /// Best-effort write that never blocks: a reply is dropped when the input
    /// is busy (another writer holds it) or the agent's input queue is full,
    /// and is cut short if it only partly fits. Returns whether all of `bytes`
    /// were written. For terminal query replies from the reader thread, which
    /// must never wait on an agent that does not read its input.
    pub fn try_write_reply(&self, bytes: &[u8]) -> bool {
        let Ok(file) = self.0.file.try_lock() else {
            return false;
        };
        let Some(file) = file.as_ref() else {
            return false;
        };
        let fd = file.as_raw_fd();
        // A poll for POLLOUT is not enough: macOS reports a PTY master
        // writable with only two bytes free, and a blocking write then waits
        // for the rest. So the write itself is made non-blocking. The flag
        // lives on the shared file description, but is only set while we hold
        // the input lock (excluding other writers); the reader retries EAGAIN.
        // SAFETY: fcntl/write on an fd we own, with a valid buffer.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return false;
            }
            let n = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
            libc::fcntl(fd, libc::F_SETFL, flags);
            usize::try_from(n) == Ok(bytes.len())
        }
    }

    /// Closes the input without ever blocking: if a writer holds it (possibly
    /// blocked on a full queue), that writer closes it when its write returns.
    fn close(&self) {
        self.0.closed.store(true, Ordering::SeqCst);
        self.release();
    }

    /// Takes the fd out of the lock, if free, and closes it after unlocking.
    fn release(&self) {
        let file = match self.0.file.try_lock() {
            Ok(mut file) => file.take(),
            Err(TryLockError::Poisoned(e)) => e.into_inner().take(),
            Err(TryLockError::WouldBlock) => None,
        };
        drop(file);
    }
}

fn exited() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "agent has exited")
}

fn unknown() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "unknown agent")
}

struct Agent {
    bytes_read: AtomicU64,
    exit_code: Mutex<Option<i32>>,
    /// Child pid; the child is a session (and process group) leader.
    pid: Option<u32>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// `None` once the agent has exited (closed to release the fd).
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    input: AgentInput,
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

/// Serialises `openpty` process-wide: macOS `ptsname` (used inside it) is not
/// reentrant, so `PtyHost::spawn` is safe to call from any thread.
static OPEN_PTY: Mutex<()> = Mutex::new(());

/// Open a PTY, retrying briefly: macOS occasionally reports ENXIO under
/// concurrent opens while PTY devices are being recycled.
fn open_pty(size: PtySize) -> io::Result<portable_pty::PtyPair> {
    let mut attempt = 0;
    loop {
        let opened = {
            let _guard = lock(&OPEN_PTY);
            native_pty_system().openpty(size)
        };
        match opened {
            Ok(pair) => return Ok(pair),
            Err(e) if attempt >= 5 => return Err(io::Error::other(e)),
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

    /// Spawns an agent whose output is counted and discarded.
    pub fn spawn(&self, spec: CommandSpec) -> io::Result<AgentId> {
        self.spawn_with_sink(spec, Box::new(DiscardSink))
    }

    /// Spawns an agent whose output is counted and handed to `sink`. Its
    /// environment is the daemon's plus `spec.env`.
    pub fn spawn_with_sink(
        &self,
        spec: CommandSpec,
        sink: Box<dyn OutputSink>,
    ) -> io::Result<AgentId> {
        self.spawn_with_base_env(spec, None, sink)
    }

    /// [`PtyHost::spawn_with_sink`], with `base` (if any) replacing the
    /// daemon's environment under `spec.env`.
    pub fn spawn_with_base_env(
        &self,
        spec: CommandSpec,
        base: Option<&[(OsString, OsString)]>,
        mut sink: Box<dyn OutputSink>,
    ) -> io::Result<AgentId> {
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
        if let Some(base) = base {
            cmd.env_clear();
            for (k, v) in base {
                cmd.env(k, v);
            }
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        // Before the spawn, so a failure here leaves no child behind.
        let (input, reader) = AgentInput::new(&*pair.master)?;
        let mut child = pair.slave.spawn_command(cmd).map_err(io::Error::other)?;
        // Close our copy of the slave so readers see EOF once the child exits.
        drop(pair.slave);

        let id = AgentId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let agent = Arc::new(Agent {
            bytes_read: AtomicU64::new(0),
            exit_code: Mutex::new(None),
            pid: child.process_id(),
            killer: Mutex::new(child.clone_killer()),
            master: Mutex::new(Some(pair.master)),
            input,
        });
        sink.on_spawn(agent.input.clone());
        *lock(&self.signal.running) += 1;
        lock(&self.agents).insert(id, Arc::clone(&agent));

        let signal = Arc::clone(&self.signal);
        let spawned = std::thread::Builder::new()
            .name(format!("pty-reader-{}", id.0))
            .spawn(move || {
                let mut buf = vec![0u8; READ_BUF];
                loop {
                    match (&*reader).read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            agent.bytes_read.fetch_add(n as u64, Ordering::Relaxed);
                            sink.on_output(&buf[..n]);
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        // O_NONBLOCK is set briefly while a reply is written.
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => break, // EIO on Linux once the slave closes
                    }
                }
                let code = match child.wait() {
                    Ok(status) => status.exit_code() as i32,
                    Err(_) => -1,
                };
                *lock(&agent.exit_code) = Some(code);
                agent.input.close();
                lock(&agent.master).take();
                sink.on_exit(code);
                *lock(&signal.running) -= 1;
                signal.cv.notify_all();
            });
        if let Err(e) = spawned {
            // The child would run unattended; stop it.
            if let Some(agent) = lock(&self.agents).remove(&id) {
                let _ = lock(&agent.killer).kill();
            }
            *lock(&self.signal.running) -= 1;
            return Err(e);
        }
        Ok(id)
    }

    fn agent(&self, id: AgentId) -> io::Result<Arc<Agent>> {
        lock(&self.agents).get(&id).cloned().ok_or_else(unknown)
    }

    pub fn stats(&self, id: AgentId) -> Option<AgentStats> {
        let agent = lock(&self.agents).get(&id).cloned()?;
        let exit_code = *lock(&agent.exit_code);
        Some(AgentStats {
            bytes_read: agent.bytes_read.load(Ordering::Relaxed),
            exit_code,
        })
    }

    /// Writes `bytes` to the agent's terminal input.
    pub fn write_input(&self, id: AgentId, bytes: &[u8]) -> io::Result<()> {
        self.agent(id)?.input.write_all(bytes)
    }

    /// Resizes the agent's PTY (the child gets SIGWINCH).
    pub fn resize(&self, id: AgentId, cols: u16, rows: u16) -> io::Result<()> {
        let agent = self.agent(id)?;
        let master = lock(&agent.master);
        let master = master.as_ref().ok_or_else(exited)?;
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)
    }

    /// Sends SIGTERM to the agent's process group, so children it started
    /// (which may hold the PTY open) go too. A no-op once the agent has exited.
    pub fn kill(&self, id: AgentId) -> io::Result<()> {
        self.signal_group(id, libc::SIGTERM)
    }

    /// Like [`PtyHost::kill`], but SIGKILL.
    pub fn force_kill(&self, id: AgentId) -> io::Result<()> {
        self.signal_group(id, libc::SIGKILL)
    }

    fn signal_group(&self, id: AgentId, sig: libc::c_int) -> io::Result<()> {
        let agent = self.agent(id)?;
        // Skip finished agents: their pid may have been reused. A narrow window
        // remains between the reader thread reaping the child and recording
        // its exit code; holding the lock keeps it from widening while we signal.
        let exit_code = lock(&agent.exit_code);
        if exit_code.is_some() {
            return Ok(());
        }
        let Some(pid) = agent.pid else {
            return lock(&agent.killer).kill();
        };
        let pgid = libc::pid_t::try_from(pid).map_err(io::Error::other)?;
        // SAFETY: killpg has no memory-safety preconditions.
        if unsafe { libc::killpg(pgid, sig) } == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(()); // the whole group already exited
        }
        Err(err)
    }

    /// Forgets a finished agent. Fails with `ResourceBusy` while it runs.
    pub fn remove(&self, id: AgentId) -> io::Result<()> {
        let mut agents = lock(&self.agents);
        let agent = agents.get(&id).ok_or_else(unknown)?;
        if lock(&agent.exit_code).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "agent is still running",
            ));
        }
        agents.remove(&id);
        Ok(())
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
