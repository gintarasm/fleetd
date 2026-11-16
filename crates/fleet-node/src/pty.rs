//! PTY host: spawns agents in pseudo-terminals and drains their output.
//!
//! Each agent gets a reader thread that counts its output and hands it to an
//! [`OutputSink`]; input, resize and kill go through [`PtyHost`], which
//! implements [`AgentHost`]. An agent is either spawned by the host or adopted
//! from another daemon that froze it; both run on the same reader and input
//! path and differ only in how the host reaps the child (see
//! [`PtyHost::adopt`]).
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
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
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
    /// How much lower than the daemon the agent runs, as `nice -n` (ADR
    /// 0066): 0 keeps the daemon's priority, and the result is capped at
    /// [`MAX_NICE`].
    pub nice: u8,
}

/// The highest nice value an agent is given: Linux's maximum (macOS allows
/// 20).
pub const MAX_NICE: i32 = 19;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentStats {
    pub bytes_read: u64,
    /// `None` while running. Once finished it is always `Some`: the process
    /// exit code, or 1 when the child was killed by a signal (whatever the
    /// signal: portable-pty's `ExitStatus::exit_code` encoding, which adopted
    /// agents follow too), or -1 when it could not be reaped.
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
    /// The input and the reader's handle to the same duplicate of `master`.
    fn new(master: BorrowedFd<'_>, freeze: &Arc<Freeze>) -> io::Result<(Self, Arc<File>)> {
        let file = Arc::new(File::from(master.try_clone_to_owned()?));
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
    /// portable-pty's handle on a spawned child, for when it reports no pid;
    /// `None` for an adopted agent, whose pid is always known.
    killer: Option<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    /// `None` once the agent has exited (closed to release the fd).
    master: Mutex<Option<Master>>,
    input: AgentInput,
}

impl Agent {
    /// Ends a child no reader thread will ever reap or drain.
    fn stop_unattended(&self) {
        match (
            &self.killer,
            self.pid.and_then(|p| libc::pid_t::try_from(p).ok()),
        ) {
            (Some(killer), _) => drop(lock(killer).kill()),
            // SAFETY: killpg has no memory-safety preconditions.
            (None, Some(pgid)) => drop(unsafe { libc::killpg(pgid, libc::SIGKILL) }),
            (None, None) => {}
        }
    }
}

/// An agent's PTY master.
enum Master {
    /// Opened by this host for an agent it spawned.
    Spawned(Box<dyn MasterPty + Send>),
    /// Inherited from the daemon that froze the agent.
    Adopted(OwnedFd),
}

impl Master {
    fn raw_fd(&self) -> Option<RawFd> {
        match self {
            Self::Spawned(master) => master.as_raw_fd(),
            Self::Adopted(fd) => Some(fd.as_raw_fd()),
        }
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        match self {
            Self::Spawned(master) => master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(io::Error::other),
            Self::Adopted(fd) => set_window_size(fd.as_fd(), cols, rows),
        }
    }
}

/// `TIOCSWINSZ`, as portable-pty's own resize does it.
fn set_window_size(fd: BorrowedFd<'_>, cols: u16, rows: u16) -> io::Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: ioctl on an open fd with a pointer to a valid winsize.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ as _, &size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Takes ownership of an inherited fd and marks it close-on-exec, so agents
/// spawned later do not inherit it. Fails, owning nothing, if `fd` is not
/// open. Leaves its blocking mode alone: readers are blocking (see the module
/// docs), and the daemon that froze the agent left it blocking.
fn take_inherited_fd(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: fcntl on any integer is memory-safe; it fails on a closed fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is open and the caller hands it to us.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Reaps `pid`, a child of this process, and returns its exit code encoded
/// as portable-pty encodes a spawned child's (by going through the same
/// `std::process::ExitStatus` conversion), so an agent reports one code for
/// one kind of death whichever daemon spawned it.
fn reap(pid: libc::pid_t) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    let mut status = 0;
    loop {
        // SAFETY: waitpid with a valid out-pointer.
        let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
        if reaped == pid {
            let status = std::process::ExitStatus::from_raw(status);
            return portable_pty::ExitStatus::from(status).exit_code() as i32;
        }
        if reaped < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return -1; // not our child (ECHILD), as when portable-pty's wait fails
    }
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

    /// `pthread_kill`'s result: 0, or the error number.
    fn interrupt(self) -> libc::c_int {
        // SAFETY: the thread is alive (see `Thread`) and the signal handled.
        unsafe { libc::pthread_kill(self.0, FREEZE_SIGNAL) }
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
    writers: HashMap<u64, Writer>,
    next_writer: u64,
}

#[derive(Default)]
struct Reader {
    /// `None` until the reader thread has started.
    thread: Option<Thread>,
    parked: bool,
    /// What the current freeze did to this reader, for its timeout report.
    nudges: Nudges,
    /// Reads that failed with `EINTR` during the current freeze.
    interrupted: u32,
}

struct Writer {
    thread: Thread,
    nudges: Nudges,
}

/// Signals one freeze sent a thread, and the last `pthread_kill` error.
#[derive(Default, Clone, Copy)]
struct Nudges {
    sent: u32,
    error: Option<libc::c_int>,
}

impl Nudges {
    fn send(&mut self, thread: Thread) {
        self.sent = self.sent.saturating_add(1);
        match thread.interrupt() {
            0 => {}
            e => self.error = Some(e),
        }
    }
}

impl Freeze {
    fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::Acquire)
    }

    /// Called first by a reader thread, so a freeze can interrupt it. The
    /// guard deregisters it when dropped, also when the thread unwinds (a
    /// panicking sink): a dead thread left registered would be signalled by
    /// every later freeze and, never parking, make each one time out.
    fn reader_started(&self, id: AgentId) -> ReaderGuard<'_> {
        if let Some(reader) = lock(&self.state).readers.get_mut(&id) {
            reader.thread = Some(Thread::current());
        }
        ReaderGuard { freeze: self, id }
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

    /// Called by a reader whose read failed with `EINTR`; only counted, for
    /// a freeze's timeout report. Off the output path: only a freeze
    /// interrupts reads.
    fn interrupted(&self, id: AgentId) {
        if let Some(reader) = lock(&self.state).readers.get_mut(&id) {
            reader.interrupted = reader.interrupted.saturating_add(1);
        }
    }

    /// Called by a reader once its agent's output has ended (and by a spawn
    /// whose reader never started).
    fn reader_done(&self, id: AgentId) {
        lock(&self.state).readers.remove(&id);
        self.cv.notify_all();
    }

    /// Runs `write` registered as a writer a freeze interrupts; deregistered
    /// by a guard, so also if `write` unwinds.
    fn as_writer<T>(&self, write: impl FnOnce() -> T) -> T {
        let token = {
            let mut state = lock(&self.state);
            let token = state.next_writer;
            state.next_writer += 1;
            state.writers.insert(
                token,
                Writer {
                    thread: Thread::current(),
                    nudges: Nudges::default(),
                },
            );
            token
        };
        let _writing = WriterGuard {
            freeze: self,
            token,
        };
        write()
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
        for reader in state.readers.values_mut() {
            reader.nudges = Nudges::default();
            reader.interrupted = 0;
        }
        loop {
            let mut waiting = false;
            for reader in state.readers.values_mut().filter(|r| !r.parked) {
                waiting = true;
                // One not yet started sees the flag before its first read.
                if let Some(thread) = reader.thread {
                    reader.nudges.send(thread);
                }
            }
            for writer in state.writers.values_mut() {
                waiting = true;
                writer.nudges.send(writer.thread);
            }
            if !waiting {
                return Ok(state.readers.keys().copied().collect());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let holdouts = holdouts(&state);
                tracing::warn!(%holdouts, "freeze timed out");
                self.thaw_locked(&mut state);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("agents did not stop in time ({holdouts})"),
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

/// What a timed-out freeze still waits for, one entry per thread: e.g.
/// `reader 0: signalled 600x, read interrupted 0x` or `reader 3: not
/// started` or `writer: signalled 600x, pthread_kill error 3`. The host id
/// of a reader is the `N` of its `pty-reader-N` thread.
fn holdouts(state: &FreezeState) -> String {
    fn nudged(out: &mut String, n: Nudges) {
        out.push_str(&format!("signalled {}x", n.sent));
        if let Some(e) = n.error {
            out.push_str(&format!(", pthread_kill error {e}"));
        }
    }
    let mut parts = Vec::new();
    let mut readers: Vec<_> = state.readers.iter().filter(|(_, r)| !r.parked).collect();
    readers.sort_by_key(|(id, _)| id.0);
    for (id, reader) in readers {
        let mut part = format!("reader {}: ", id.0);
        if reader.thread.is_none() {
            part.push_str("not started");
        } else {
            nudged(&mut part, reader.nudges);
            part.push_str(&format!(", read interrupted {}x", reader.interrupted));
        }
        parts.push(part);
    }
    for writer in state.writers.values() {
        let mut part = "writer: ".to_owned();
        nudged(&mut part, writer.nudges);
        parts.push(part);
    }
    parts.join("; ")
}

/// Deregisters a reader thread when dropped; see [`Freeze::reader_started`].
struct ReaderGuard<'a> {
    freeze: &'a Freeze,
    id: AgentId,
}

impl Drop for ReaderGuard<'_> {
    fn drop(&mut self) {
        self.freeze.reader_done(self.id);
    }
}

/// Deregisters a writer thread when dropped; see [`Freeze::as_writer`].
struct WriterGuard<'a> {
    freeze: &'a Freeze,
    token: u64,
}

impl Drop for WriterGuard<'_> {
    fn drop(&mut self) {
        lock(&self.freeze.state).writers.remove(&self.token);
        self.freeze.cv.notify_all();
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

/// Where `nice(1)` lives on macOS and Linux.
const NICE_PATHS: [&str; 2] = ["/usr/bin/nice", "/bin/nice"];

/// The `nice -n` increment that takes an agent from the daemon's own nice
/// value to the daemon's plus `nice`, capped at [`MAX_NICE`]; `None` when
/// that changes nothing.
fn nice_increment(nice: u8) -> Option<u8> {
    if nice == 0 {
        return None;
    }
    // SAFETY: no memory effects; cannot fail for the calling process.
    let own = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    let increment = i32::from(nice).min(MAX_NICE - own);
    u8::try_from(increment).ok().filter(|&n| n > 0)
}

/// The command that runs `spec.program` lowered by `increment` (ADR 0066):
/// `nice -n <increment> -- <program> <args>`, so the agent lowers itself
/// before its `exec` and everything it forks inherits it. Setting the value
/// from here after the spawn is not reliable: on macOS a `setpriority` right
/// after portable-pty's spawn returns reads back, then is lost a few
/// milliseconds later (likely because the spawn returns before the child's
/// `execve` completes; see ADR 0066).
///
/// `nice` finds the program itself (on the agent's `PATH`, or relative to
/// its cwd), and would only report a missing one by exiting 127. So it is
/// looked up here first, the way portable-pty does, to fail the spawn as
/// before. `None` without a `nice` binary: the agent then runs unlowered.
fn niced_command(
    spec: &CommandSpec,
    base: Option<&[(OsString, OsString)]>,
    increment: u8,
) -> io::Result<Option<CommandBuilder>> {
    let Some(nice) = NICE_PATHS.iter().find(|p| is_executable(Path::new(p))) else {
        return Ok(None);
    };
    find_program(spec, base)?;
    let mut cmd = CommandBuilder::new(nice);
    cmd.args(["-n", &increment.to_string(), "--"]);
    cmd.arg(&spec.program);
    Ok(Some(cmd))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Checks that `spec.program` names an executable file: as a path (relative
/// to the agent's cwd) when it has a `/`, else on the `PATH` the agent gets
/// (`spec.env`'s, else `base`'s, else the daemon's).
fn find_program(spec: &CommandSpec, base: Option<&[(OsString, OsString)]>) -> io::Result<()> {
    let program = spec.program.as_path();
    let not_found = || {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("program {} not found", program.display()),
        )
    };
    if program.as_os_str().as_encoded_bytes().contains(&b'/') {
        let cwd = match &spec.cwd {
            Some(cwd) => cwd.clone(),
            None => std::env::current_dir()?,
        };
        return if is_executable(&cwd.join(program)) {
            Ok(())
        } else {
            Err(not_found())
        };
    }
    let path = match spec.env.iter().rev().find(|(k, _)| k == "PATH") {
        Some((_, v)) => Some(OsString::from(v)),
        None => match base {
            Some(base) => base
                .iter()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| v.clone()),
            None => std::env::var_os("PATH"),
        },
    };
    let found = path
        .iter()
        .flat_map(std::env::split_paths)
        .any(|dir| is_executable(&dir.join(program)));
    if found { Ok(()) } else { Err(not_found()) }
}

/// On Linux with autogroup on, CFS shares the CPU between sessions before
/// nice orders processes within one, and every agent is a session of its
/// own: the agent's autogroup gets its nice value too. Best effort: the file
/// is absent without autogroup.
#[cfg(target_os = "linux")]
fn lower_autogroup(pid: u32, increment: u8) {
    // SAFETY: no memory effects; cannot fail for the calling process.
    let own = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    let target = (own + i32::from(increment)).min(MAX_NICE);
    let _ = std::fs::write(format!("/proc/{pid}/autogroup"), target.to_string());
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
        sink: Box<dyn OutputSink>,
    ) -> io::Result<AgentId> {
        let pair = open_pty(PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let increment = nice_increment(spec.nice);
        let niced = match increment {
            Some(n) => niced_command(&spec, base, n)?,
            None => None,
        };
        let mut cmd = niced.unwrap_or_else(|| CommandBuilder::new(&spec.program));
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
        let master_fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| io::Error::other("PTY master has no fd"))?;
        // Before the spawn, so a failure here leaves no child behind.
        // SAFETY: `pair.master` owns this fd and outlives the borrow.
        let (input, reader) =
            AgentInput::new(unsafe { BorrowedFd::borrow_raw(master_fd) }, &self.freeze)?;
        let mut child = pair.slave.spawn_command(cmd).map_err(io::Error::other)?;
        #[cfg(target_os = "linux")]
        if let (Some(pid), Some(n)) = (child.process_id(), increment) {
            lower_autogroup(pid, n);
        }
        // Close our copy of the slave so readers see EOF once the child exits.
        drop(pair.slave);

        let agent = Agent {
            bytes_read: AtomicU64::new(0),
            exit_code: Mutex::new(None),
            pid: child.process_id(),
            killer: Some(Mutex::new(child.clone_killer())),
            master: Mutex::new(Some(Master::Spawned(pair.master))),
            input,
        };
        self.run(agent, reader, sink, move || match child.wait() {
            Ok(status) => status.exit_code() as i32,
            Err(_) => -1,
        })
    }

    /// Takes over a running agent that another daemon froze, as if this host
    /// had spawned it: same reader, input, freeze, kill and stats. The host
    /// owns `spec.terminal` from this call on, also when it fails.
    ///
    /// `spec.pid` must be a child of this process (an exec keeps children),
    /// reaped with `waitpid` once its output ends, in place of portable-pty's
    /// `Child::wait`; one that has already exited is reaped the same way. The
    /// terminal is set to `spec.cols` × `spec.rows`, which also checks that it
    /// is a terminal.
    pub fn adopt(&self, spec: AdoptSpec, sink: Box<dyn OutputSink>) -> io::Result<AgentId> {
        let master = match spec.terminal {
            TerminalHandle::Fd(fd) => take_inherited_fd(fd)?,
            // A holder process's socket (`TerminalHandle::HolderSocket`) will
            // be handled here.
        };
        // Not 0 or negative: `waitpid` and `killpg` would take those as
        // "any child" and "our own process group".
        let pid = libc::pid_t::try_from(spec.pid)
            .ok()
            .filter(|&pid| pid > 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid agent pid"))?;
        set_window_size(master.as_fd(), spec.cols, spec.rows)?;
        let (input, reader) = AgentInput::new(master.as_fd(), &self.freeze)?;
        let agent = Agent {
            bytes_read: AtomicU64::new(0),
            exit_code: Mutex::new(None),
            pid: Some(spec.pid),
            killer: None,
            master: Mutex::new(Some(Master::Adopted(master))),
            input,
        };
        self.run(agent, reader, sink, move || reap(pid))
    }

    /// Registers `agent` and starts its reader thread, which drains `reader`
    /// into `sink` until the output ends, then reaps the child with `reap`.
    /// If the thread cannot start, the child is stopped.
    fn run(
        &self,
        agent: Agent,
        reader: Arc<File>,
        mut sink: Box<dyn OutputSink>,
        reap: impl FnOnce() -> i32 + Send + 'static,
    ) -> io::Result<AgentId> {
        let id = AgentId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let agent = Arc::new(agent);
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
                let reading = freeze.reader_started(id);
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
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                            freeze.interrupted(id);
                        }
                        // O_NONBLOCK is set briefly while a reply is written.
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => break, // EIO on Linux once the slave closes
                    }
                }
                drop(reading);
                let code = reap();
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
                agent.stop_unattended();
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

    /// The agent's pid, which is also its session id (ADR 0072); `None`
    /// for an unknown agent or one spawned without a pid.
    pub fn pid(&self, id: AgentId) -> Option<u32> {
        lock(&self.agents).get(&id)?.pid
    }

    /// Writes `bytes` to the agent's terminal input.
    pub fn write_input(&self, id: AgentId, bytes: &[u8]) -> io::Result<()> {
        self.agent(id)?.input.write_all(bytes)
    }

    /// Resizes the agent's PTY (the child gets SIGWINCH).
    pub fn resize(&self, id: AgentId, cols: u16, rows: u16) -> io::Result<()> {
        let agent = self.agent(id)?;
        let master = lock(&agent.master);
        master.as_ref().ok_or_else(exited)?.resize(cols, rows)
    }

    /// Sends SIGHUP, then SIGTERM, to the agent's process group, so children
    /// it started (which may hold the PTY open) go too. SIGHUP is what a
    /// closed terminal sends: an interactive shell ignores SIGTERM but exits
    /// on it (ADR 0071). A no-op once the agent has exited.
    pub fn kill(&self, id: AgentId) -> io::Result<()> {
        self.signal_group(id, libc::SIGHUP)?;
        // SIGHUP may already have ended it: before its exit is recorded, a
        // group of zombies makes macOS killpg fail with EPERM. The group
        // took a signal, so a failed SIGTERM only means it is going.
        let _ = self.signal_group(id, libc::SIGTERM);
        Ok(())
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
            let killer = agent
                .killer
                .as_ref()
                .ok_or_else(|| io::Error::other("agent has no pid"))?;
            return lock(killer).kill();
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
                        .and_then(Master::raw_fd)
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

    fn adopt(&self, spec: AdoptSpec, sink: Box<dyn OutputSink>) -> io::Result<AgentId> {
        PtyHost::adopt(self, spec, sink)
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

    fn pid(&self, id: AgentId) -> Option<u32> {
        PtyHost::pid(self, id)
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
            nice: 0,
        }
    }

    /// The nice value of process `pid`.
    fn nice_of(pid: u32) -> i32 {
        // SAFETY: getpriority has no memory effects. -1 is a valid value,
        // so errno tells a failure apart.
        unsafe {
            *errno() = 0;
            let n = libc::getpriority(libc::PRIO_PROCESS, pid as _);
            assert_eq!(*errno(), 0, "getpriority({pid}) failed");
            n
        }
    }

    #[cfg(target_os = "macos")]
    unsafe fn errno() -> *mut i32 {
        unsafe { libc::__error() }
    }

    #[cfg(not(target_os = "macos"))]
    unsafe fn errno() -> *mut i32 {
        unsafe { libc::__errno_location() }
    }

    #[test]
    fn spawn_lowers_the_agent_by_its_nice() {
        let host = PtyHost::new();
        let own = nice_of(std::process::id());
        let id = host.spawn(CommandSpec { nice: 5, ..cat() }).unwrap();
        let pid = host.agent(id).unwrap().pid.unwrap();
        let want = (own + 5).min(MAX_NICE);
        // `nice` lowers itself and execs the agent right after the spawn.
        eventually("the agent lowered", || nice_of(pid) == want);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(nice_of(pid), want, "the lowering did not stick");
        host.kill(id).unwrap();
        host.wait_all(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn a_lowered_spawn_of_a_missing_program_fails() {
        let host = PtyHost::new();
        for program in ["/nonexistent/agent", "no-such-agent-program"] {
            let spec = CommandSpec {
                nice: 5,
                program: PathBuf::from(program),
                ..cat()
            };
            let e = host.spawn(spec).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::NotFound, "{program}");
        }
    }

    #[test]
    fn a_lowered_spawn_finds_the_program_on_the_agents_path() {
        let host = PtyHost::new();
        let out = Collect::default();
        let spec = CommandSpec {
            nice: 5,
            program: PathBuf::from("sh"),
            args: vec![
                "-c".into(),
                "echo found-$1".into(),
                "sh".into(),
                "--x".into(),
            ],
            env: vec![("PATH".into(), "/bin:/usr/bin".into())],
            ..cat()
        };
        host.spawn_with_sink(spec, Box::new(out.clone())).unwrap();
        host.wait_all(Duration::from_secs(5)).unwrap();
        assert!(out.contains(b"found---x"));
        assert_eq!(out.exit_code(), Some(0));
    }

    #[test]
    fn spawn_with_zero_nice_keeps_the_daemons_priority() {
        let host = PtyHost::new();
        let own = nice_of(std::process::id());
        let id = host.spawn(cat()).unwrap();
        let pid = host.agent(id).unwrap().pid.unwrap();
        assert_eq!(nice_of(pid), own);
        host.kill(id).unwrap();
        host.wait_all(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn a_lowered_agents_children_are_lowered_too() {
        // The child is forked after the spawn returned: it inherits.
        let host = PtyHost::new();
        let own = nice_of(std::process::id());
        let out = Collect::default();
        let id = host
            .spawn_with_sink(
                CommandSpec {
                    nice: 3,
                    ..sh("sleep 0.2; ps -o nice= -p $$; /bin/sh -c 'ps -o nice= -p $$'")
                },
                Box::new(out.clone()),
            )
            .unwrap();
        host.wait_all(Duration::from_secs(5)).unwrap();
        let text = String::from_utf8_lossy(&lock(&out.0)).into_owned();
        let nices: Vec<i32> = text
            .split_whitespace()
            .filter_map(|w| w.parse().ok())
            .collect();
        let want = (own + 3).min(MAX_NICE);
        assert_eq!(nices, vec![want, want], "output: {text:?}");
        let _ = id;
    }

    /// Collects an agent's output and its exit code.
    #[derive(Clone, Default)]
    struct Collect(Arc<Mutex<Vec<u8>>>, Arc<Mutex<Option<i32>>>);

    impl OutputSink for Collect {
        fn on_output(&mut self, bytes: &[u8]) {
            lock(&self.0).extend_from_slice(bytes);
        }
        fn on_exit(&mut self, code: i32) {
            *lock(&self.1) = Some(code);
        }
    }

    impl Collect {
        fn len(&self) -> usize {
            lock(&self.0).len()
        }

        fn contains(&self, needle: &[u8]) -> bool {
            lock(&self.0).windows(needle.len()).any(|w| w == needle)
        }

        fn exit_code(&self) -> Option<i32> {
            *lock(&self.1)
        }
    }

    /// Polls `done` every 10 ms for up to 5 s.
    fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn cat() -> CommandSpec {
        CommandSpec {
            program: PathBuf::from("/bin/cat"),
            args: vec![],
            ..sh("")
        }
    }

    /// The code a spawned agent reports when `kill` (SIGTERM) ends it.
    fn spawned_cat_killed_code() -> i32 {
        let host = PtyHost::new();
        let out = Collect::default();
        let id = host.spawn_with_sink(cat(), Box::new(out.clone())).unwrap();
        host.kill(id).unwrap();
        host.wait_all(Duration::from_secs(5)).unwrap();
        out.exit_code().unwrap()
    }

    #[test]
    fn adopted_agent_reads_writes_and_reports_exit() {
        let first = PtyHost::new();
        let pid = {
            let id = first.spawn(cat()).unwrap();
            first.agent(id).unwrap().pid.unwrap()
        };
        let frozen = first
            .freeze(Instant::now() + Duration::from_secs(3))
            .unwrap();
        let TerminalHandle::Fd(fd) = frozen[0].terminal;
        assert_eq!(frozen[0].pid, pid);
        // The test owns this duplicate; `first` keeps (and later closes) its own.
        // SAFETY: `fd` is `first`'s open master, alive while `first` is.
        let dup = unsafe { BorrowedFd::borrow_raw(fd) }
            .try_clone_to_owned()
            .unwrap();

        let second = PtyHost::new();
        let out = Collect::default();
        let id = second
            .adopt(
                AdoptSpec {
                    pid,
                    terminal: TerminalHandle::Fd(std::os::fd::IntoRawFd::into_raw_fd(dup)),
                    cols: 100,
                    rows: 30,
                },
                Box::new(out.clone()),
            )
            .unwrap();
        second.write_input(id, b"hi\n").unwrap();
        eventually("the adopted agent's output", || out.contains(b"hi"));
        assert_eq!(second.stats(id).unwrap().exit_code, None);
        assert!(second.remove(id).is_err(), "removed a running agent");
        // Adopting set the size; resizing changes it. `fd` is the same terminal.
        let size = || {
            // SAFETY: an all-zero winsize is valid.
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            // SAFETY: TIOCGWINSZ on `first`'s open master into a valid winsize.
            assert_eq!(
                unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut size) },
                0
            );
            (size.ws_col, size.ws_row)
        };
        assert_eq!(size(), (100, 30));
        second.resize(id, 120, 40).unwrap();
        assert_eq!(size(), (120, 40));

        second.kill(id).unwrap();
        second.wait_all(Duration::from_secs(5)).unwrap();
        // portable-pty reports any death by signal as 1 (its `ExitStatus`
        // keeps the signal apart), and an adopted agent reports the same.
        let spawned = spawned_cat_killed_code();
        assert_eq!(spawned, 1);
        assert_eq!(out.exit_code(), Some(spawned));
        assert_eq!(second.stats(id).unwrap().exit_code, Some(spawned));
        assert!(second.stats(id).unwrap().bytes_read >= 2);
        second.remove(id).unwrap();

        // `first`'s reader, thawed, sees the output end; its agent was reaped
        // by `second`, so `first` only has to let go of it.
        first.thaw();
        first.wait_all(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn adopted_agent_that_already_exited_is_reaped() {
        let sh = c"/bin/sh";
        let argv = [
            sh.as_ptr(),
            c"-c".as_ptr(),
            c"exit 7".as_ptr(),
            std::ptr::null(),
        ];
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: openpty with valid out-pointers and no name, termios or size.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0, "{}", io::Error::last_os_error());
        // SAFETY: the child calls only async-signal-safe functions, on
        // arguments prepared before the fork.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "{}", io::Error::last_os_error());
        if pid == 0 {
            // SAFETY: as above.
            unsafe {
                libc::setsid();
                libc::close(master);
                for fd in 0..3 {
                    libc::dup2(slave, fd);
                }
                libc::close(slave);
                libc::execv(sh.as_ptr(), argv.as_ptr());
                libc::_exit(127);
            }
        }
        // SAFETY: closing our copy of the slave, which we own.
        unsafe { libc::close(slave) };
        std::thread::sleep(Duration::from_millis(100));

        let host = PtyHost::new();
        let out = Collect::default();
        host.adopt(
            AdoptSpec {
                pid: u32::try_from(pid).unwrap(),
                terminal: TerminalHandle::Fd(master),
                cols: 80,
                rows: 24,
            },
            Box::new(out.clone()),
        )
        .unwrap();
        host.wait_all(Duration::from_secs(5)).unwrap();
        assert_eq!(out.exit_code(), Some(7));
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

    #[test]
    fn a_reader_whose_sink_panicked_does_not_hold_a_later_freeze() {
        struct Panics;
        impl OutputSink for Panics {
            fn on_output(&mut self, _bytes: &[u8]) {
                // Not `panic!`: no panic hook, so no noise in the test output.
                std::panic::resume_unwind(Box::new("sink bug"));
            }
            fn on_exit(&mut self, _code: i32) {}
        }
        let host = PtyHost::new();
        let broken = host
            .spawn_with_sink(sh("printf x; sleep 30"), Box::new(Panics))
            .unwrap();
        let healthy = host.spawn(sh("sleep 30")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while lock(&host.freeze.state).readers.contains_key(&broken) {
            assert!(
                Instant::now() < deadline,
                "the dead reader stayed registered"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let frozen = host
            .freeze(Instant::now() + Duration::from_secs(1))
            .unwrap();
        host.thaw();
        let ids: Vec<_> = frozen.iter().map(|f| f.id).collect();
        assert_eq!(ids, [healthy]);
        // The broken agent's reader never reaps it, so no `wait_all`.
        host.force_kill(broken).unwrap();
        host.force_kill(healthy).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while host.stats(healthy).unwrap().exit_code.is_none() {
            assert!(Instant::now() < deadline, "agent never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A timed-out freeze names what it still waited for and how that
    /// thread answered the signal, so a stuck restart can be diagnosed from
    /// the error alone.
    #[test]
    fn a_timed_out_freeze_names_its_holdouts() {
        /// Holds the reader inside `on_output` until released, so it can
        /// neither read nor park.
        struct Stuck(std::sync::mpsc::Receiver<()>);
        impl OutputSink for Stuck {
            fn on_output(&mut self, _bytes: &[u8]) {
                let _ = self.0.recv();
            }
            fn on_exit(&mut self, _code: i32) {}
        }
        let (release, rx) = std::sync::mpsc::channel();
        let host = PtyHost::new();
        let stuck = host
            .spawn_with_sink(sh("printf x; sleep 30"), Box::new(Stuck(rx)))
            .unwrap();
        let healthy = host.spawn(sh("sleep 30")).unwrap();
        eventually("the stuck agent's output", || {
            host.stats(stuck).unwrap().bytes_read > 0
        });
        let err = host
            .freeze(Instant::now() + Duration::from_millis(300))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let msg = err.to_string();
        assert!(
            msg.starts_with(&format!(
                "agents did not stop in time (reader {}: signalled ",
                stuck.0
            )),
            "{msg}"
        );
        assert!(msg.ends_with("x, read interrupted 0x)"), "{msg}");
        // Only the holdout is named: the healthy reader parked.
        assert!(!msg.contains(&format!("reader {}:", healthy.0)), "{msg}");
        release.send(()).unwrap();
        // Thawed: the next freeze succeeds.
        let frozen = host
            .freeze(Instant::now() + Duration::from_secs(3))
            .unwrap();
        host.thaw();
        assert_eq!(frozen.len(), 2);
        host.force_kill(stuck).unwrap();
        host.force_kill(healthy).unwrap();
        host.wait_all(Duration::from_secs(5)).unwrap();
    }
}
