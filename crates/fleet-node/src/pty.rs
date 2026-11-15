//! PTY host: spawns agents in pseudo-terminals and drains their output.
//!
//! Each agent gets a reader thread that counts its output and hands it to an
//! [`OutputSink`]; input, resize and kill go through [`PtyHost`], which
//! implements [`AgentHost`].
//!
//! Freezing (for a daemon restart) must cost the output path nothing: the
//! reader still makes one blocking `read` per chunk and only adds an atomic
//! load of the host's `frozen` flag before it. A freeze sets the flag and
//! interrupts each reader and input writer with [`FREEZE_SIGNAL`], whose
//! handler is installed without `SA_RESTART`, so a blocked `read` or `write`
//! returns EINTR having consumed nothing. A reader parks only between reads,
//! never holding bytes its sink has not had. (Reading non-blocking and
//! sleeping in `poll` on the master and a stop pipe was tried first: a reader
//! that keeps up with its agent then pays EAGAIN + `poll` + `read` per wake-up,
//! which cost +77% host CPU per MiB in the 50-agent flood.)

use crate::host::{AdoptSpec, AgentHost, FrozenAgent, TerminalHandle};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, TryLockError};
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
    freeze: Arc<Freeze>,
}

impl AgentInput {
    /// The input and the reader's handle to the same duplicate of the master.
    fn new(master: &dyn MasterPty, freeze: &Arc<Freeze>) -> io::Result<(Self, Arc<File>)> {
        let fd = master
            .as_raw_fd()
            .ok_or_else(|| io::Error::other("PTY master has no fd"))?;
        // SAFETY: the master owns this fd and outlives the borrow.
        let file = File::from(unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?);
        let file = Arc::new(file);
        let input = Self(Arc::new(InputPort {
            file: Mutex::new(Some(Arc::clone(&file))),
            closed: AtomicBool::new(false),
            freeze: Arc::clone(freeze),
        }));
        Ok((input, file))
    }

    /// Writes all of `bytes`, blocking while the agent's input queue is full.
    /// Fails with "fleetd is restarting" while the host is frozen, also when
    /// the freeze comes mid-way (the rest of `bytes` is dropped).
    pub fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let written = {
            let file = lock(&self.0.file);
            let file = file.as_ref().ok_or_else(exited)?;
            let freeze = &self.0.freeze;
            freeze.as_writer(|| write_unless_frozen(file.as_raw_fd(), bytes, freeze))
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

/// Writes all of `bytes` to the blocking `fd` until done or the host is
/// frozen. Not `Write::write_all`: that retries the EINTR a freeze causes.
fn write_unless_frozen(fd: RawFd, mut bytes: &[u8], freeze: &Freeze) -> io::Result<()> {
    while !bytes.is_empty() {
        if freeze.is_frozen() {
            return Err(restarting());
        }
        // SAFETY: write on an open fd, with a valid buffer.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        match usize::try_from(n) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(_) => {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
        }
    }
    Ok(())
}

fn restarting() -> io::Error {
    io::Error::other("fleetd is restarting")
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

/// Interrupts readers and input writers for a freeze. Nothing else in fleetd
/// uses it; agents get the default disposition back when they exec.
const FREEZE_SIGNAL: libc::c_int = libc::SIGUSR2;

/// How often a freeze re-sends [`FREEZE_SIGNAL`] to a thread that has not
/// stopped yet: a signal that lands between the `frozen` check and the
/// blocking call it guards is lost.
const FREEZE_RESEND: Duration = Duration::from_millis(5);

extern "C" fn on_freeze_signal(_: libc::c_int) {}

/// Installs the no-op [`FREEZE_SIGNAL`] handler once per process, without
/// `SA_RESTART`, so the syscall it interrupts fails with EINTR.
fn install_freeze_handler() -> io::Result<()> {
    static INSTALLED: OnceLock<Result<(), i32>> = OnceLock::new();
    let installed = INSTALLED.get_or_init(|| {
        // SAFETY: a zeroed sigaction with an empty mask and no flags is valid;
        // the handler is async-signal-safe (it does nothing).
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_freeze_signal as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(FREEZE_SIGNAL, &action, std::ptr::null_mut()) < 0 {
                return Err(io::Error::last_os_error().raw_os_error().unwrap_or(0));
            }
        }
        Ok(())
    });
    installed.map_err(io::Error::from_raw_os_error)
}

/// A thread to interrupt. `pthread_t` is a pointer on macOS, so not `Send`;
/// it is only ever passed to `pthread_kill`, while the thread is known to be
/// alive (registered under [`Freeze::state`]).
#[derive(Clone, Copy)]
struct Thread(libc::pthread_t);

// SAFETY: see `Thread`; the value is an opaque id, never dereferenced.
unsafe impl Send for Thread {}

impl Thread {
    fn current() -> Self {
        // SAFETY: pthread_self has no preconditions.
        Self(unsafe { libc::pthread_self() })
    }

    fn interrupt(self) {
        // SAFETY: the thread is alive (see `Thread`) and the signal handled.
        unsafe { libc::pthread_kill(self.0, FREEZE_SIGNAL) };
    }
}

/// Host-wide freeze state, shared by every reader thread and input.
#[derive(Default)]
struct Freeze {
    /// Mirrors `state.frozen` for the check before every read and write.
    frozen: AtomicBool,
    state: Mutex<FreezeState>,
    /// Signalled when a reader parks or stops, a writer leaves, and on thaw.
    cv: Condvar,
}

#[derive(Default)]
struct FreezeState {
    frozen: bool,
    /// Readers whose agent's output has not ended, by agent.
    readers: HashMap<AgentId, Reader>,
    /// Threads inside [`AgentInput::write_all`], by a token of their own.
    writers: HashMap<u64, Thread>,
    next_writer: u64,
}

#[derive(Default)]
struct Reader {
    /// `None` until the reader thread has started.
    thread: Option<Thread>,
    parked: bool,
}

impl Freeze {
    fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::Acquire)
    }

    /// Called first by a reader thread, so a freeze can interrupt it.
    fn reader_started(&self, id: AgentId) {
        if let Some(reader) = lock(&self.state).readers.get_mut(&id) {
            reader.thread = Some(Thread::current());
        }
    }

    /// Called by a reader that saw `frozen`: acknowledges and waits for thaw.
    fn park(&self, id: AgentId) {
        let mut state = lock(&self.state);
        if let Some(reader) = state.readers.get_mut(&id) {
            reader.parked = true;
        }
        self.cv.notify_all();
        while state.frozen {
            state = self.cv.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        if let Some(reader) = state.readers.get_mut(&id) {
            reader.parked = false;
        }
    }

    /// Called by a reader once its agent's output has ended (and by a spawn
    /// whose reader never started).
    fn reader_done(&self, id: AgentId) {
        lock(&self.state).readers.remove(&id);
        self.cv.notify_all();
    }

    /// Runs `write` registered as a writer a freeze interrupts.
    fn as_writer<T>(&self, write: impl FnOnce() -> T) -> T {
        let token = {
            let mut state = lock(&self.state);
            let token = state.next_writer;
            state.next_writer += 1;
            state.writers.insert(token, Thread::current());
            token
        };
        let written = write();
        lock(&self.state).writers.remove(&token);
        self.cv.notify_all();
        written
    }

    /// Freezes and waits until every reader has parked or seen its output
    /// end and every writer has left; returns the agents of parked readers.
    /// Past `deadline`, thaws and fails with `TimedOut`.
    fn freeze(&self, deadline: Instant) -> io::Result<Vec<AgentId>> {
        install_freeze_handler()?;
        let mut state = lock(&self.state);
        if state.frozen {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "already frozen",
            ));
        }
        state.frozen = true;
        self.frozen.store(true, Ordering::SeqCst);
        loop {
            let mut waiting = false;
            for reader in state.readers.values().filter(|r| !r.parked) {
                waiting = true;
                // One not yet started sees the flag before its first read.
                if let Some(thread) = reader.thread {
                    thread.interrupt();
                }
            }
            for &thread in state.writers.values() {
                waiting = true;
                thread.interrupt();
            }
            if !waiting {
                return Ok(state.readers.keys().copied().collect());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.thaw_locked(&mut state);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "agents did not stop in time",
                ));
            }
            state = self
                .cv
                .wait_timeout(state, left.min(FREEZE_RESEND))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    fn thaw(&self) {
        self.thaw_locked(&mut lock(&self.state));
    }

    fn thaw_locked(&self, state: &mut FreezeState) {
        state.frozen = false;
        self.frozen.store(false, Ordering::SeqCst);
        self.cv.notify_all();
    }
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
    freeze: Arc<Freeze>,
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
        let (input, reader) = AgentInput::new(&*pair.master, &self.freeze)?;
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
        // Listed before the agent, so a freeze that lists it waits for it.
        lock(&self.freeze.state)
            .readers
            .insert(id, Reader::default());
        lock(&self.agents).insert(id, Arc::clone(&agent));

        let signal = Arc::clone(&self.signal);
        let freeze = Arc::clone(&self.freeze);
        let spawned = std::thread::Builder::new()
            .name(format!("pty-reader-{}", id.0))
            .spawn(move || {
                freeze.reader_started(id);
                let mut buf = vec![0u8; READ_BUF];
                loop {
                    // Only here, between reads: a freeze never strands bytes
                    // that were read but not handed to the sink. Its signal
                    // makes a blocked read fail with EINTR, back to here.
                    if freeze.is_frozen() {
                        freeze.park(id);
                        continue;
                    }
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
                freeze.reader_done(id);
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
            self.freeze.reader_done(id);
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

    /// See [`AgentHost::freeze`].
    pub fn freeze(&self, deadline: Instant) -> io::Result<Vec<FrozenAgent>> {
        let ids = self.freeze.freeze(deadline)?;
        let frozen = {
            let agents = lock(&self.agents);
            ids.into_iter()
                .map(|id| {
                    let agent = agents.get(&id).ok_or_else(unknown)?;
                    let pid = agent
                        .pid
                        .ok_or_else(|| io::Error::other("agent has no pid"))?;
                    let fd = lock(&agent.master)
                        .as_ref()
                        .and_then(|m| m.as_raw_fd())
                        .ok_or_else(|| io::Error::other("PTY master has no fd"))?;
                    Ok(FrozenAgent {
                        id,
                        pid,
                        terminal: TerminalHandle::Fd(fd),
                    })
                })
                .collect::<io::Result<Vec<_>>>()
        };
        if frozen.is_err() {
            self.thaw();
        }
        frozen
    }

    /// See [`AgentHost::thaw`].
    pub fn thaw(&self) {
        self.freeze.thaw();
    }
}

impl AgentHost for PtyHost {
    fn spawn_with_base_env(
        &self,
        spec: CommandSpec,
        base: Option<&[(OsString, OsString)]>,
        sink: Box<dyn OutputSink>,
    ) -> io::Result<AgentId> {
        PtyHost::spawn_with_base_env(self, spec, base, sink)
    }

    fn adopt(&self, _spec: AdoptSpec, _sink: Box<dyn OutputSink>) -> io::Result<AgentId> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn write_input(&self, id: AgentId, bytes: &[u8]) -> io::Result<()> {
        PtyHost::write_input(self, id, bytes)
    }

    fn resize(&self, id: AgentId, cols: u16, rows: u16) -> io::Result<()> {
        PtyHost::resize(self, id, cols, rows)
    }

    fn kill(&self, id: AgentId) -> io::Result<()> {
        PtyHost::kill(self, id)
    }

    fn force_kill(&self, id: AgentId) -> io::Result<()> {
        PtyHost::force_kill(self, id)
    }

    fn remove(&self, id: AgentId) -> io::Result<()> {
        PtyHost::remove(self, id)
    }

    fn wait_all(&self, timeout: Duration) -> Result<(), WaitTimeout> {
        PtyHost::wait_all(self, timeout)
    }

    fn stats(&self, id: AgentId) -> Option<AgentStats> {
        PtyHost::stats(self, id)
    }

    fn freeze(&self, deadline: Instant) -> io::Result<Vec<FrozenAgent>> {
        PtyHost::freeze(self, deadline)
    }

    fn thaw(&self) {
        PtyHost::thaw(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::TerminalHandle;

    fn sh(script: &str) -> CommandSpec {
        CommandSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
        }
    }

    #[derive(Clone, Default)]
    struct Collect(Arc<Mutex<Vec<u8>>>);

    impl OutputSink for Collect {
        fn on_output(&mut self, bytes: &[u8]) {
            lock(&self.0).extend_from_slice(bytes);
        }
        fn on_exit(&mut self, _code: i32) {}
    }

    impl Collect {
        fn len(&self) -> usize {
            lock(&self.0).len()
        }
    }

    #[test]
    fn freeze_stops_reading_without_losing_bytes() {
        let host = PtyHost::new();
        let out = Collect::default();
        let id = host
            .spawn_with_sink(
                sh("i=0; while :; do echo $i; i=$((i+1)); done"),
                Box::new(out.clone()),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let frozen = host
            .freeze(Instant::now() + Duration::from_secs(3))
            .unwrap();
        assert_eq!(frozen.len(), 1);
        let at_freeze = out.len();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(out.len(), at_freeze, "a frozen reader kept reading");
        host.thaw();
        std::thread::sleep(Duration::from_millis(100));
        assert!(out.len() > at_freeze, "a thawed reader did not resume");
        host.force_kill(id).unwrap();
        host.wait_all(Duration::from_secs(5)).unwrap();

        let text = String::from_utf8(lock(&out.0).clone()).unwrap();
        // `\r` is dropped: macOS can turn one `\n` into `\r\r\n` when the
        // tty's output queue fills mid-conversion (as it does while frozen).
        let text = text.replace('\r', "");
        // The last line may be cut short by the kill.
        let lines: Vec<&str> = text.split('\n').collect();
        let complete = &lines[..lines.len() - 1];
        assert!(complete.len() > 10, "too little output: {}", complete.len());
        for (want, line) in complete.iter().enumerate() {
            assert_eq!(line.parse::<usize>().ok(), Some(want), "line {want}");
        }
    }

    #[test]
    fn freeze_releases_a_blocked_input_write() {
        let host = Arc::new(PtyHost::new());
        let id = host.spawn(sh("sleep 30")).unwrap();
        let writer = {
            let host = Arc::clone(&host);
            // Whole lines: a canonical-mode tty discards a line beyond its
            // limit instead of blocking, but blocks once its lines fill it.
            let input = b"xxxxxxx\n".repeat((1 << 20) / 8);
            std::thread::spawn(move || host.write_input(id, &input))
        };
        // Let the write fill the input queue and block.
        std::thread::sleep(Duration::from_millis(200));
        assert!(!writer.is_finished(), "the write did not block");
        let start = Instant::now();
        host.freeze(Instant::now() + Duration::from_secs(3))
            .unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
        let err = writer.join().unwrap().unwrap_err();
        assert_eq!(err.to_string(), "fleetd is restarting");
        // New writes fail too while frozen.
        let err = host.write_input(id, b"x").unwrap_err();
        assert_eq!(err.to_string(), "fleetd is restarting");
        host.thaw();
        host.force_kill(id).unwrap();
        host.wait_all(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn frozen_agents_name_their_fd_and_pid() {
        let host = PtyHost::new();
        let a = host.spawn(sh("sleep 30")).unwrap();
        let b = host.spawn(sh("sleep 30")).unwrap();
        let exited = host.spawn(sh("exit 0")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while host.stats(exited).unwrap().exit_code.is_none() {
            assert!(Instant::now() < deadline, "agent never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut frozen = host
            .freeze(Instant::now() + Duration::from_secs(3))
            .unwrap();
        host.thaw();
        frozen.sort_by_key(|f| f.id.0);
        assert_eq!(frozen.len(), 2, "only live agents are frozen");
        let fds: Vec<_> = frozen
            .iter()
            .map(|f| match f.terminal {
                TerminalHandle::Fd(fd) => fd,
            })
            .collect();
        assert!(fds.iter().all(|&fd| fd >= 0), "{fds:?}");
        assert_ne!(fds[0], fds[1]);
        for (f, id) in frozen.iter().zip([a, b]) {
            assert_eq!(f.id, id);
            assert_eq!(Some(f.pid), host.agent(id).unwrap().pid);
        }
        for id in [a, b] {
            host.force_kill(id).unwrap();
        }
        host.wait_all(Duration::from_secs(5)).unwrap();
    }
}
