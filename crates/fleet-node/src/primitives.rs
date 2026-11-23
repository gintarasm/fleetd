//! Node primitives (ADR 0091): `Exec`, `ReadFile` and `ListDir`.
//!
//! Plain Rust types and functions, so the daemon's request handler and the
//! workflow host (in process, ADR 0092) call the same code. Everything here
//! blocks, so it runs on blocking threads, never on the PTY output path;
//! nothing starts a thread or timer until a call is made.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::Semaphore;

use crate::env::{EnvVars, LoginEnv};

/// Default `Exec` timeout (ADR 0025).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
/// Longest `Exec` timeout.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(3600);
/// SIGTERM, then SIGKILL this much later (ADR 0071).
pub const TERM_GRACE: Duration = Duration::from_secs(2);
/// Stdout and stderr together; a reply must fit `MAX_FRAME` (16 MiB).
pub const MAX_OUTPUT: usize = 15 * 1024 * 1024;
/// Largest `Exec` stdin.
pub const MAX_STDIN: usize = 1024 * 1024;
/// Execs running at once on a node; more wait in FIFO order.
pub const MAX_CONCURRENT_EXECS: usize = 8;
/// Largest `ReadFile` reply.
pub const MAX_READ: usize = MAX_OUTPUT;
/// Most `ListDir` entries.
pub const MAX_ENTRIES: usize = 10_000;

#[derive(Debug, Clone, Default)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub clear_env: bool,
    pub stdin: Vec<u8>,
    /// `ZERO`: [`DEFAULT_TIMEOUT`].
    pub timeout: Duration,
    /// 0 or above [`MAX_OUTPUT`]: [`MAX_OUTPUT`].
    pub max_output: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecResult {
    /// -1 when a signal ended the child.
    pub exit_code: i32,
    pub signal: i32,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub duration: Duration,
}

#[derive(Debug, Clone, Default)]
pub struct ReadFileRequest {
    pub path: PathBuf,
    pub offset: u64,
    /// 0 or above [`MAX_READ`]: [`MAX_READ`].
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileData {
    pub data: Vec<u8>,
    pub size: u64,
    pub mtime_unix_ms: i64,
    pub eof: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ListDirRequest {
    pub path: PathBuf,
    /// 0 or above [`MAX_ENTRIES`]: [`MAX_ENTRIES`].
    pub max_entries: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_unix_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirListing {
    pub entries: Vec<DirEntry>,
    pub truncated: bool,
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

// ---- Exec ----

/// Process groups of the Execs running now, so a daemon restart can kill
/// them (their own sessions would outlive the exec'd daemon, ADR 0091).
static LIVE: Mutex<BTreeSet<i32>> = Mutex::new(BTreeSet::new());

fn live() -> MutexGuard<'static, BTreeSet<i32>> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// SIGKILLs every running Exec's process group and reaps the leaders. For a
/// restart's exec and the daemon's stop (never a `Node`'s shutdown: the
/// registry is the process's, and tests run many nodes in one): the blocking threads that would
/// reap them are gone by then, and a zombie would outlive the exec (its
/// parent keeps the same pid).
pub fn kill_all_execs() {
    let groups: Vec<i32> = live().iter().copied().collect();
    for &pgid in &groups {
        kill_group(pgid, libc::SIGKILL);
    }
    for pgid in groups {
        for _ in 0..40 {
            // SAFETY: waitpid on our own child; the group leader is the pid.
            // ECHILD (-1): the Exec's thread reaped it first.
            if unsafe { libc::waitpid(pgid, std::ptr::null_mut(), libc::WNOHANG) } != 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn kill_group(pgid: i32, sig: i32) {
    // SAFETY: a plain signal to a group we created.
    unsafe { libc::kill(-pgid, sig) };
}

static EXEC_SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_EXECS);

/// Runs `req` on a blocking thread once one of the node's
/// [`MAX_CONCURRENT_EXECS`] slots is free (waiting callers are served in
/// arrival order, and hold no thread while they wait). `env` is the login
/// env agents start from (ADR 0024).
///
/// Dropping the future kills the child's process group, which is how a
/// closed connection ends its Execs.
pub async fn exec(req: ExecRequest, env: LoginEnv) -> io::Result<ExecResult> {
    exec_in(&EXEC_SLOTS, req, env, TERM_GRACE).await
}

async fn exec_in(
    slots: &Semaphore,
    req: ExecRequest,
    env: LoginEnv,
    grace: Duration,
) -> io::Result<ExecResult> {
    validate(&req)?;
    let _slot = slots.acquire().await.map_err(io::Error::other)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let _guard = CancelOnDrop(Arc::clone(&cancel));
    tokio::task::spawn_blocking(move || {
        // Waits (bounded) for a login-env capture still running.
        let base = env.wait();
        run(&req, base.as_deref(), &cancel, grace)
    })
    .await
    .map_err(io::Error::other)?
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn validate(req: &ExecRequest) -> io::Result<()> {
    if req.argv.first().is_none_or(|a| a.is_empty()) {
        return Err(invalid("argv is empty"));
    }
    if !req.cwd.is_absolute() {
        return Err(invalid("cwd must be an absolute path"));
    }
    if req.timeout > MAX_TIMEOUT {
        return Err(invalid("timeout is over one hour"));
    }
    if req.stdin.len() > MAX_STDIN {
        return Err(invalid("stdin is over 1 MiB"));
    }
    Ok(())
}

/// The child's environment: the base (the login env, else the daemon's own)
/// with `req.env` over it, or `req.env` alone with `clear_env`.
fn child_env(req: &ExecRequest, base: Option<&EnvVars>) -> EnvVars {
    let mut vars: EnvVars = if req.clear_env {
        Vec::new()
    } else {
        match base {
            Some(b) => b.clone(),
            None => std::env::vars_os().collect(),
        }
    };
    for (k, v) in &req.env {
        vars.retain(|(have, _)| have.as_os_str() != k.as_str());
        vars.push((k.into(), v.into()));
    }
    vars
}

/// `name` as `execvp` would find it on the child's `PATH`.
fn resolve(name: &str, env: &EnvVars) -> io::Result<PathBuf> {
    if name.contains('/') {
        return Ok(name.into());
    }
    let path = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map_or_else(|| OsString::from("/usr/bin:/bin"), |(_, v)| v.clone());
    for dir in std::env::split_paths(&path).filter(|d| d.is_absolute()) {
        let candidate = dir.join(name);
        let runnable = std::fs::metadata(&candidate)
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
        if runnable {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{name}: not found on the node's PATH"),
    ))
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Stdout and stderr, sharing one byte budget.
struct Capture {
    out: Vec<u8>,
    err: Vec<u8>,
    left: usize,
    out_cut: bool,
    err_cut: bool,
}

impl Capture {
    fn push(&mut self, is_err: bool, chunk: &[u8]) {
        let take = chunk.len().min(self.left);
        self.left -= take;
        let (buf, cut) = if is_err {
            (&mut self.err, &mut self.err_cut)
        } else {
            (&mut self.out, &mut self.out_cut)
        };
        buf.extend_from_slice(&chunk[..take]);
        *cut |= take < chunk.len();
    }
}

/// Reads what `fd` has now. `false` at EOF or on a hard error.
fn drain(fd: RawFd, is_err: bool, cap: &mut Capture, buf: &mut [u8]) -> bool {
    loop {
        // SAFETY: reads into a live buffer of the given length.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        match n {
            0 => return false,
            n if n > 0 => cap.push(is_err, &buf[..n as usize]),
            _ => match io::Error::last_os_error().kind() {
                io::ErrorKind::WouldBlock => return true,
                io::ErrorKind::Interrupted => {}
                _ => return false,
            },
        }
    }
}

/// Runs `req` to its end on this thread: spawns the child in its own
/// session (so its own process group, no controlling terminal), feeds its
/// stdin, collects its output within the cap, and on the timeout (or when
/// `cancel` is set) sends SIGTERM to the group and SIGKILL `grace` later.
/// Whatever the child left running in its group is killed at the end.
fn run(
    req: &ExecRequest,
    base: Option<&EnvVars>,
    cancel: &AtomicBool,
    grace: Duration,
) -> io::Result<ExecResult> {
    validate(req)?;
    let started = Instant::now();
    let env = child_env(req, base);
    let program = resolve(&req.argv[0], &env)?;
    let mut cmd = Command::new(program);
    cmd.arg0(&req.argv[0])
        .args(&req.argv[1..])
        .current_dir(&req.cwd)
        .env_clear()
        .envs(env)
        .stdin(if req.stdin.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: `setsid` is async-signal-safe; nothing else runs in the child.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    let mut child = cmd.spawn()?;
    let pgid = child.id() as i32;
    live().insert(pgid);
    let result = supervise(req, &mut child, pgid, cancel, grace, started);
    kill_group(pgid, libc::SIGKILL);
    live().remove(&pgid);
    let _ = child.wait();
    result
}

fn supervise(
    req: &ExecRequest,
    child: &mut Child,
    pgid: i32,
    cancel: &AtomicBool,
    grace: Duration,
    started: Instant,
) -> io::Result<ExecResult> {
    let timeout = if req.timeout.is_zero() {
        DEFAULT_TIMEOUT
    } else {
        req.timeout
    };
    let max = match req.max_output {
        0 => MAX_OUTPUT,
        n => n.min(MAX_OUTPUT),
    };
    let deadline = started + timeout;
    let mut cap = Capture {
        out: Vec::new(),
        err: Vec::new(),
        left: max,
        out_cut: false,
        err_cut: false,
    };
    let mut out = child.stdout.take().map(OwnedFd::from);
    let mut err = child.stderr.take().map(OwnedFd::from);
    let mut stdin = child.stdin.take().map(OwnedFd::from);
    for fd in out.iter().chain(&err).chain(&stdin) {
        set_nonblocking(fd.as_raw_fd())?;
    }
    let mut written = 0;
    let mut buf = vec![0u8; 64 * 1024];
    let (mut term_at, mut killed, mut timed_out) = (None::<Instant>, false, false);
    let status = loop {
        let now = Instant::now();
        if term_at.is_none() {
            let cancelled = cancel.load(Ordering::Relaxed);
            if cancelled || now >= deadline {
                timed_out = !cancelled;
                kill_group(pgid, libc::SIGTERM);
                term_at = Some(now);
            }
        }
        if let Some(t) = term_at
            && !killed
            && now >= t + grace
        {
            kill_group(pgid, libc::SIGKILL);
            killed = true;
        }
        let mut fds = Vec::with_capacity(3);
        let mut slots = [None; 3];
        for (i, (fd, events)) in [
            (out.as_ref(), libc::POLLIN),
            (err.as_ref(), libc::POLLIN),
            (stdin.as_ref(), libc::POLLOUT),
        ]
        .into_iter()
        .enumerate()
        {
            if let Some(fd) = fd {
                slots[i] = Some(fds.len());
                fds.push(libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events,
                    revents: 0,
                });
            }
        }
        // The next moment something is due: the deadline or the SIGKILL.
        let due = match term_at {
            None => deadline,
            Some(t) if !killed => t + grace,
            Some(_) => now + CHECK,
        };
        let limit = if out.is_none() && err.is_none() {
            // Both pipes are closed: only the exit is left to wait for.
            Duration::from_millis(1)
        } else {
            CHECK
        };
        let wait = due.saturating_duration_since(now).min(limit);
        // Rounded up: 0 would spin.
        let ms = wait.as_micros().div_ceil(1000).max(1) as libc::c_int;
        if fds.is_empty() {
            std::thread::sleep(wait.max(Duration::from_millis(1)));
        } else {
            // SAFETY: a valid array of `fds.len()` pollfds.
            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        }
        if let (Some(f), Some(i)) = (out.as_ref(), slots[0])
            && fds[i].revents != 0
            && !drain(f.as_raw_fd(), false, &mut cap, &mut buf)
        {
            out = None;
        }
        if let (Some(f), Some(i)) = (err.as_ref(), slots[1])
            && fds[i].revents != 0
            && !drain(f.as_raw_fd(), true, &mut cap, &mut buf)
        {
            err = None;
        }
        if let (Some(f), Some(i)) = (stdin.as_ref(), slots[2])
            && fds[i].revents != 0
        {
            // SAFETY: writes from a live slice of the given length.
            let n = unsafe {
                libc::write(
                    f.as_raw_fd(),
                    req.stdin[written..].as_ptr().cast(),
                    req.stdin.len() - written,
                )
            };
            if n > 0 {
                written += n as usize;
            }
            let blocked = n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock;
            // Done, or the child closed its stdin (EPIPE): close ours.
            if written >= req.stdin.len() || (n < 0 && !blocked) {
                stdin = None;
            }
        }
        if let Some(status) = child.try_wait()? {
            // Whatever the pipes still hold; stragglers are killed after.
            for (fd, is_err) in [(&out, false), (&err, true)] {
                if let Some(f) = fd {
                    drain(f.as_raw_fd(), is_err, &mut cap, &mut buf);
                }
            }
            break status;
        }
    };
    Ok(ExecResult {
        exit_code: status.code().unwrap_or(-1),
        signal: status.signal().unwrap_or(0),
        timed_out,
        stdout: cap.out,
        stderr: cap.err,
        stdout_truncated: cap.out_cut,
        stderr_truncated: cap.err_cut,
        duration: started.elapsed(),
    })
}

/// How often a running Exec looks at its cancel flag and the clock.
const CHECK: Duration = Duration::from_millis(50);

// ---- ReadFile / ListDir ----

fn unix_ms(t: io::Result<SystemTime>) -> i64 {
    match t.map(|t| t.duration_since(UNIX_EPOCH)) {
        Ok(Ok(d)) => d.as_millis() as i64,
        Ok(Err(e)) => -(e.duration().as_millis() as i64),
        Err(_) => 0,
    }
}

/// Up to `max_bytes` of a regular file from `offset` (symlinks followed).
pub fn read_file(req: &ReadFileRequest) -> io::Result<FileData> {
    if !req.path.is_absolute() {
        return Err(invalid("path must be absolute"));
    }
    // O_NONBLOCK: opening a FIFO with no writer must not wait for one.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&req.path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(invalid(format!(
            "{} is not a regular file",
            req.path.display()
        )));
    }
    let want = match req.max_bytes {
        0 => MAX_READ,
        n => n.min(MAX_READ),
    };
    let size = meta.len();
    let len = want.min(size.saturating_sub(req.offset) as usize);
    let mut data = vec![0u8; len];
    let mut got = 0;
    while got < len {
        match file.read_at(&mut data[got..], req.offset + got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    data.truncate(got);
    Ok(FileData {
        eof: req.offset.saturating_add(got as u64) >= size,
        data,
        size,
        mtime_unix_ms: unix_ms(meta.modified()),
    })
}

/// The entries of a directory, unsorted (not `.` and `..`, no recursion).
pub fn list_dir(req: &ListDirRequest) -> io::Result<DirListing> {
    if !req.path.is_absolute() {
        return Err(invalid("path must be absolute"));
    }
    let max = match req.max_entries {
        0 => MAX_ENTRIES,
        n => n.min(MAX_ENTRIES),
    };
    let mut listing = DirListing::default();
    for entry in std::fs::read_dir(&req.path)? {
        let entry = entry?;
        if listing.entries.len() == max {
            listing.truncated = true;
            break;
        }
        // The entry's own metadata: a symlink is not followed.
        let meta = entry.metadata()?;
        let t = meta.file_type();
        let kind = if t.is_symlink() {
            EntryKind::Symlink
        } else if t.is_dir() {
            EntryKind::Dir
        } else if t.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        listing.entries.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
            size: meta.len(),
            mtime_unix_ms: unix_ms(meta.modified()),
        });
    }
    Ok(listing)
}

#[cfg(test)]
mod tests;
