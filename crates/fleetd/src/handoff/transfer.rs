//! The restart-only half of [`super`]: writing and taking the handoff
//! record, the pre-flight check, the `exec`, and adopting or abandoning what
//! a predecessor handed over.
//!
//! A module of its own so that its code (and the generic code it
//! instantiates) sits in a codegen unit no normal start executes: code that
//! shares pages with startup code is paged in on every start, and the idle
//! daemon's resident memory is gated (ADR 0014).

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fleet_proto::HANDOFF_VERSION;
use fleet_proto::handoff::HandoffRecord;
use prost::Message;

use super::{HANDOFF_FD_ENV, HANDOFF_FILE, HANDOFF_PIDS_ENV, remove_stale};

/// Writes `<state_dir>/handoff.bin` (mode 0600) and returns an fd opened on
/// it, which the successor inherits (by number, in the record's caller's
/// hands) and passes to [`take_record`]. Replaces any earlier file.
pub fn write_record(state_dir: &Path, rec: &HandoffRecord) -> io::Result<OwnedFd> {
    let path = state_dir.join(HANDOFF_FILE);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    // `mode` is masked by the umask and ignored for an existing file.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(&rec.encode_to_vec())?;
    file.sync_all()?;
    Ok(OwnedFd::from(File::open(&path)?))
}

/// Reads the record from `fd` (which it takes ownership of and closes), then
/// deletes the file so a later start cannot mistake it for a live handoff.
/// Refuses a record whose `version` is not [`HANDOFF_VERSION`].
///
/// The file is deleted even when the record is bad: it is one-shot.
pub fn take_record(fd: RawFd, state_dir: &Path) -> io::Result<HandoffRecord> {
    // SAFETY: the caller hands over an open fd it owns (inherited across exec).
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut buf = Vec::new();
    let read = file.read_to_end(&mut buf);
    drop(file);
    remove_stale(state_dir);
    read?;
    let rec = HandoffRecord::decode(buf.as_slice())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("handoff record: {e}")))?;
    if rec.version != HANDOFF_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "handoff record version {} (this binary reads {HANDOFF_VERSION})",
                rec.version
            ),
        ));
    }
    Ok(rec)
}

/// Runs `<binary> --handoff-check` and checks it speaks our handoff version.
/// Synchronous: the child is killed at `timeout`. The `Err` is a reason fit
/// for the user.
pub fn run_check(binary: &Path, timeout: Duration) -> Result<(), String> {
    const NOT_EXEC: &str = "not an executable file";
    match fs::metadata(binary) {
        Ok(m) if m.is_file() && m.permissions().mode() & 0o111 != 0 => {}
        _ => return Err(NOT_EXEC.into()),
    }
    // Its own process group, so a descendant it leaves behind (holding the
    // pipe, or still sleeping) is killed with it and cannot outlive the check.
    let mut child = Command::new(binary)
        .arg("--handoff-check")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| NOT_EXEC.to_string())?;
    let group = child.id() as libc::pid_t;
    // Whatever the outcome, nothing from the check is left running. The
    // group id outlives the reaped child while any member remains; with none
    // left this is ESRCH.
    let kill_group = || {
        // SAFETY: plain signal to the process group we created.
        unsafe { libc::kill(-group, libc::SIGKILL) };
    };
    let mut stdout = child.stdout.take().expect("stdout is piped");
    // Never block on a pipe a grandchild may hold open: the output is read
    // non-blocking, once, after the child has exited.
    // SAFETY: fcntl on an fd we own.
    unsafe {
        let fd = stdout.as_raw_fd();
        libc::fcntl(
            fd,
            libc::F_SETFL,
            libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK,
        );
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                kill_group();
                let _ = child.wait();
                return Err("check timed out".into());
            }
        }
    };
    // The child wrote before it exited, so its output is already in the pipe
    // (a child that fills the pipe never exits and times out above).
    let mut out = Vec::new();
    let _ = (&mut stdout).take(4096).read_to_end(&mut out);
    kill_group();
    if !status.success() {
        return Err(format!(
            "check failed (exit {})",
            status.code().unwrap_or(-1)
        ));
    }
    let out = String::from_utf8_lossy(&out);
    let common = out
        .lines()
        .filter_map(|l| l.strip_prefix("handoff ")?.trim().parse::<u32>().ok())
        .any(|v| v == HANDOFF_VERSION);
    if common {
        Ok(())
    } else {
        Err("no common handoff version".into())
    }
}

/// `execve`s `binary` in this process (same pid, so the agents stay its
/// children) as `<binary> --state-dir <state_dir>`, with this process's
/// environment plus [`HANDOFF_FD_ENV`] (the record's fd) and
/// [`HANDOFF_PIDS_ENV`] (`pids`). Only `record_fd` and `keep` (listener,
/// lock, agents' terminals) pass through: close-on-exec is cleared on them
/// just before, every other fd keeps it.
///
/// Returns only on failure, with close-on-exec restored and the record fd
/// closed, so the caller can thaw and serve on.
pub fn exec(
    binary: &Path,
    state_dir: &Path,
    record_fd: OwnedFd,
    keep: &[RawFd],
    pids: &[i32],
) -> io::Error {
    let record = record_fd.as_raw_fd();
    let pids = pids
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    // Everything is allocated before the fds are touched.
    let (path, argv, envp) = match exec_args(binary, state_dir, record, &pids) {
        Ok(args) => args,
        Err(e) => return e,
    };
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let mut envp_ptrs: Vec<*const libc::c_char> = envp.iter().map(|e| e.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());
    let fds: Vec<RawFd> = keep.iter().copied().chain([record]).collect();
    let e = match set_cloexec(&fds, false) {
        Ok(()) => {
            // SAFETY: NUL-terminated strings and null-terminated pointer
            // arrays, all alive across the call.
            unsafe { libc::execve(path.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr()) };
            io::Error::last_os_error()
        }
        Err(e) => e,
    };
    let _ = set_cloexec(&fds, true);
    drop(record_fd);
    e
}

type ExecArgs = (CString, Vec<CString>, Vec<CString>);

fn exec_args(binary: &Path, state_dir: &Path, record: RawFd, pids: &str) -> io::Result<ExecArgs> {
    let c = |b: &[u8]| {
        CString::new(b)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in an argument"))
    };
    let path = c(binary.as_os_str().as_bytes())?;
    let argv = vec![
        path.clone(),
        c(b"--state-dir")?,
        c(state_dir.as_os_str().as_bytes())?,
    ];
    let mut envp = Vec::new();
    for (k, v) in std::env::vars_os() {
        if k == HANDOFF_FD_ENV || k == HANDOFF_PIDS_ENV {
            continue;
        }
        let mut entry = k.into_vec();
        entry.push(b'=');
        entry.extend_from_slice(v.as_bytes());
        // A variable with a NUL cannot be passed on (nor can it exist).
        if let Ok(entry) = CString::new(entry) {
            envp.push(entry);
        }
    }
    envp.push(c(format!("{HANDOFF_FD_ENV}={record}").as_bytes())?);
    envp.push(c(format!("{HANDOFF_PIDS_ENV}={pids}").as_bytes())?);
    Ok((path, argv, envp))
}

/// Sets or clears close-on-exec on each fd; stops at the first failure.
fn set_cloexec(fds: &[RawFd], on: bool) -> io::Result<()> {
    for &fd in fds {
        // SAFETY: fcntl on an integer fd is memory-safe; it fails on a
        // closed one.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let flags = if on {
            flags | libc::FD_CLOEXEC
        } else {
            flags & !libc::FD_CLOEXEC
        };
        // SAFETY: as above.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The listener and the lock a predecessor handed over, adopted from the
/// record's fd numbers once they check out: the listener a unix socket
/// bound to `socket_path`, the lock a regular file. Both are marked
/// close-on-exec again; the listener is non-blocking, as `startup` leaves it.
pub fn adopt_listener_and_lock(
    rec: &HandoffRecord,
    socket_path: &Path,
) -> io::Result<(StdUnixListener, File)> {
    let bad = |what: &str| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("handoff record: {what}"),
        )
    };
    let (listener_fd, lock_fd) = (rec.listener_fd, rec.lock_fd);
    if listener_fd <= 2 || lock_fd <= 2 || listener_fd == lock_fd {
        return Err(bad("bad listener or lock fd"));
    }
    if file_type(listener_fd) != Some(libc::S_IFSOCK) {
        return Err(bad("the listener fd is not a socket"));
    }
    if file_type(lock_fd) != Some(libc::S_IFREG) {
        return Err(bad("the lock fd is not a file"));
    }
    set_cloexec(&[listener_fd, lock_fd], true)?;
    // SAFETY: both fds are open (fstat succeeded), of the expected type, and
    // the record hands them to this process; nothing else owns them.
    let (listener, lock) = unsafe {
        (
            StdUnixListener::from_raw_fd(listener_fd),
            File::from_raw_fd(lock_fd),
        )
    };
    let addr = listener.local_addr()?;
    if addr.as_pathname() != Some(socket_path) {
        return Err(bad("the listener is not this state dir's socket"));
    }
    listener.set_nonblocking(true)?;
    Ok((listener, lock))
}

/// The `S_IFMT` bits of what `fd` is open on; `None` if it is not open.
fn file_type(fd: RawFd) -> Option<libc::mode_t> {
    // SAFETY: fstat with a valid out-pointer; it fails on a closed fd.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(fd, &mut st) } == 0).then_some(st.st_mode & libc::S_IFMT)
}

/// For a successor that cannot use its handoff: ends the agents the
/// predecessor named in [`HANDOFF_PIDS_ENV`] (SIGTERM to each group,
/// SIGKILL after the shutdown grace, reaped), then closes every inherited
/// fd above 2 (their terminals, the record, the old listener and lock), so
/// a normal start follows with nothing of the handoff left open.
///
/// Only pids that are children of this process are signalled (see
/// [`end_children`]). Call before opening any file of this process's own
/// (before logging): every fd above 2 is closed.
pub fn abandon(pids: &[i32]) {
    end_children(pids);
    let fds: Vec<RawFd> = match fs::read_dir("/dev/fd") {
        Ok(dir) => dir
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .collect(),
        Err(_) => return,
    };
    // The directory's own fd is listed too; it is closed by now (EBADF).
    for fd in fds.into_iter().filter(|&fd| fd > 2) {
        // SAFETY: nothing in this process owns an fd above 2 yet; those
        // left are inherited ones no one will use.
        unsafe { libc::close(fd) };
    }
}

/// Ends those of `pids` that are children of this process (SIGTERM to each
/// group, SIGKILL after the shutdown grace, reaped); how many that was.
/// Only children: an exec keeps the agents as children, and a stray
/// variable or a bad record must not make the daemon kill arbitrary process
/// groups. A pid that has exited is reaped by the check and not counted.
pub fn end_children(pids: &[i32]) -> usize {
    let children: Vec<i32> = pids.iter().copied().filter(|&p| is_child(p)).collect();
    fleet_node::end_unadopted(&children);
    children.len()
}

/// Whether `pid` is a child of this process (reaping it if it has exited:
/// then it is gone, so "not a child" any more).
fn is_child(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let mut status = 0;
    // SAFETY: waitpid with a valid out-pointer.
    unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) == 0 }
}

/// Resolves the binary a restart asked for: empty means `own`, the path of
/// the binary this daemon runs (its [`Identity`](super::Identity)), so a
/// file rebuilt or reinstalled at that path is what it restarts onto; a name
/// without `/` is looked up on the daemon's `PATH`; any other path is made
/// absolute (against the daemon's working directory: clients send absolute
/// paths). Whether it is executable is the check's business: an `own` that
/// no longer exists (or is unknown, empty) fails it, with no `PATH`
/// fallback, as restarting onto some other build would be a surprise. The
/// `Err` is fit for the user.
pub fn resolve_binary(binary: &str, own: &str) -> Result<PathBuf, String> {
    if binary.is_empty() {
        return Ok(PathBuf::from(own));
    }
    if binary.contains('/') {
        return std::path::absolute(binary).map_err(|e| format!("{binary}: {e}"));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(binary))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
        .ok_or_else(|| format!("no {binary} on the daemon's PATH"))
}

// Test hooks for the integration tests, read from the environment in debug
// builds only: never in a release build. Not a cargo feature: a feature
// enabled by the crate's dev-dependency on itself does not reach the binary
// the tests run when the whole workspace is built (another member depends
// on this crate without it, and the binary gets that build).

/// Test hook: `FLEETD_TEST_RESTART_DELAY_MS` pauses a restart after its
/// check passed, before anything is frozen, so a test can replace the binary
/// in between.
pub(crate) fn test_delay_after_check() {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("FLEETD_TEST_RESTART_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// Test hook: `FLEETD_TEST_FROZEN_DELAY_MS` pauses a restart with its
/// agents frozen, before the record is written, so a test can signal the
/// daemon then.
pub(crate) fn test_delay_frozen() {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("FLEETD_TEST_FROZEN_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// Test hook: with `FLEETD_TEST_CORRUPT_HANDOFF` set, overwrites the written
/// record with bytes that do not decode, so the successor falls back.
pub(crate) fn test_corrupt_record(state_dir: &Path) {
    #[cfg(debug_assertions)]
    if std::env::var_os("FLEETD_TEST_CORRUPT_HANDOFF").is_some() {
        let _ = fs::write(state_dir.join(HANDOFF_FILE), b"not a handoff record");
    }
    #[cfg(not(debug_assertions))]
    let _ = state_dir;
}

/// Test hook: panics if the environment variable `var` is set, to test that
/// a panic at that point leaves nothing behind.
pub(crate) fn test_panic(var: &str) {
    #[cfg(debug_assertions)]
    if std::env::var_os(var).is_some() {
        panic!("test hook {var}");
    }
    #[cfg(not(debug_assertions))]
    let _ = var;
}

/// Test hook: with `FLEETD_TEST_SHORT_HANDOFF` set, the record leaves out
/// its last agent (whose pid is still passed), as a bad record might.
pub(crate) fn test_short_record(rec: &mut HandoffRecord) {
    #[cfg(debug_assertions)]
    if std::env::var_os("FLEETD_TEST_SHORT_HANDOFF").is_some() {
        rec.agents.pop();
    }
    #[cfg(not(debug_assertions))]
    let _ = rec;
}
