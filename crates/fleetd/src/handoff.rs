//! What a daemon needs to restart into a new binary: its own identity, the
//! handoff file passed to the successor, and the pre-flight check the old
//! daemon runs on the new binary before it commits to `exec`.

use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fleet_proto::handoff::HandoffRecord;
use fleet_proto::{BinaryIdentity, HANDOFF_VERSION};
use prost::Message;

/// The handoff file's name in the state dir.
pub const HANDOFF_FILE: &str = "handoff.bin";

/// What `fleetd --handoff-check` prints: the handoff versions this binary
/// reads, one `handoff N` line each.
pub fn check_output() -> String {
    format!("handoff {HANDOFF_VERSION}\n")
}

/// Who this daemon is: a token fresh per process start (a restart gets a new
/// one, so a client can tell the daemon was replaced) and the binary it runs.
#[derive(Debug, Clone)]
pub struct Identity {
    pub instance: u64,
    pub binary: BinaryIdentity,
}

impl Identity {
    /// One 8-byte `/dev/urandom` read and one `stat` of the running
    /// executable. Neither failing stops the daemon: it is only an identity,
    /// so a failed read falls back to the clock and pid (still distinct per
    /// start) and a failed `stat` leaves the binary fields empty.
    pub fn current() -> Identity {
        Identity {
            instance: random_u64().unwrap_or_else(|| {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos() as u64);
                nanos ^ (u64::from(std::process::id()) << 32)
            }),
            binary: current_binary().unwrap_or_default(),
        }
    }
}

fn random_u64() -> Option<u64> {
    let mut buf = [0u8; 8];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .ok()?;
    Some(u64::from_ne_bytes(buf))
}

fn current_binary() -> io::Result<BinaryIdentity> {
    let path = std::env::current_exe()?;
    let m = fs::metadata(&path)?;
    Ok(BinaryIdentity {
        path: path.to_string_lossy().into_owned(),
        dev: m.dev(),
        ino: m.ino(),
        size: m.size(),
        mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
    })
}

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

/// Deletes a leftover handoff file. Called on every normal start, after the
/// instance lock is taken: with the lock held no other fleetd can be
/// mid-restart on this state dir, so any file here is from a restart that
/// never finished and describes agents this process did not inherit.
pub fn remove_stale(state_dir: &Path) {
    let _ = fs::remove_file(state_dir.join(HANDOFF_FILE));
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

/// The environment variable naming the record's fd in the successor.
pub const HANDOFF_FD_ENV: &str = "FLEETD_HANDOFF_FD";
/// The environment variable listing the handed-over agents' pids
/// (comma-separated), so a successor that cannot read the record can still
/// end them: they are its children, holding inherited terminals, and must
/// not run on untracked.
pub const HANDOFF_PIDS_ENV: &str = "FLEETD_HANDOFF_PIDS";

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

/// What a predecessor's [`exec`] passed in the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handed {
    /// The record's fd; `None` if the variable did not name an open fd
    /// above 2.
    pub record_fd: Option<RawFd>,
    /// The agents' pids, as far as they parsed.
    pub pids: Vec<i32>,
}

/// Removes [`HANDOFF_FD_ENV`] and [`HANDOFF_PIDS_ENV`] from this process's
/// environment, always, so neither reaches an agent, the login-env capture
/// or a later restart; returns what they said if this process was started
/// by a predecessor's [`exec`] (the fd variable set).
///
/// # Safety
///
/// Modifies the environment: no other thread may exist yet (call it first
/// in `main`).
pub unsafe fn take_handed() -> Option<Handed> {
    let fd = std::env::var_os(HANDOFF_FD_ENV);
    let pids = std::env::var_os(HANDOFF_PIDS_ENV);
    // SAFETY: the caller guarantees this process is single-threaded.
    unsafe {
        std::env::remove_var(HANDOFF_FD_ENV);
        std::env::remove_var(HANDOFF_PIDS_ENV);
    }
    let fd = fd?;
    let pids = pids
        .as_deref()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    // Only an fd that is open is handed on to be owned (and closed).
    // SAFETY: fcntl on any integer only queries it.
    let open = |fd: RawFd| fd > 2 && unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0;
    Some(Handed {
        record_fd: fd
            .to_str()
            .and_then(|f| f.parse().ok())
            .filter(|&f| open(f)),
        pids,
    })
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
/// Only pids that are children of this process are signalled: an exec
/// keeps the agents as children, and a stray variable must not make the
/// daemon kill arbitrary process groups. Call before opening any file of
/// this process's own (before logging): every fd above 2 is closed.
pub fn abandon(pids: &[i32]) {
    let children: Vec<i32> = pids.iter().copied().filter(|&p| is_child(p)).collect();
    fleet_node::end_unadopted(&children);
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

/// Resolves the binary a restart asked for: empty means `fleetd` on the
/// daemon's `PATH`; a name without `/` is looked up on `PATH`; a path is
/// made absolute (against the daemon's working directory). Whether it is
/// executable is the check's business. The `Err` is fit for the user.
pub fn resolve_binary(binary: &str) -> Result<PathBuf, String> {
    let name = if binary.is_empty() { "fleetd" } else { binary };
    if name.contains('/') {
        return std::path::absolute(name).map_err(|e| format!("{name}: {e}"));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
        .ok_or_else(|| format!("no {name} on the daemon's PATH"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::handoff::HandoffAgent;
    use std::os::fd::IntoRawFd;

    fn record(version: u32) -> HandoffRecord {
        HandoffRecord {
            version,
            listener_fd: 3,
            lock_fd: 4,
            previous_instance: 77,
            frozen_unix_ms: 1,
            agents: vec![HandoffAgent {
                id: 1,
                pid: 99,
                cols: 80,
                rows: 24,
                snapshot: b"screen".to_vec(),
                terminal: Some(fleet_proto::handoff::handoff_agent::Terminal::Fd(7)),
            }],
        }
    }

    #[test]
    fn record_round_trips_and_file_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let rec = record(HANDOFF_VERSION);
        let fd = write_record(dir.path(), &rec).unwrap();
        let path = dir.path().join(HANDOFF_FILE);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let got = take_record(fd.into_raw_fd(), dir.path()).unwrap();
        assert_eq!(got, rec);
        assert!(!path.exists());
    }

    #[test]
    fn record_of_unknown_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let fd = write_record(dir.path(), &record(HANDOFF_VERSION + 1)).unwrap();
        let err = take_record(fd.into_raw_fd(), dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(!dir.path().join(HANDOFF_FILE).exists());
    }

    #[test]
    fn binaries_resolve_by_path_or_on_path() {
        assert_eq!(
            resolve_binary("/opt/x/fleetd").unwrap(),
            Path::new("/opt/x/fleetd")
        );
        assert!(resolve_binary("rel/fleetd").unwrap().is_absolute());
        // `sh` is on every PATH the tests run with.
        let sh = resolve_binary("sh").unwrap();
        assert!(sh.is_absolute() && sh.ends_with("sh"));
        assert_eq!(
            resolve_binary("no-such-fleetd-binary").unwrap_err(),
            "no no-such-fleetd-binary on the daemon's PATH"
        );
    }

    #[test]
    fn identity_instances_differ_and_binary_is_this_executable() {
        let (a, b) = (Identity::current(), Identity::current());
        assert_ne!(a.instance, b.instance);
        assert_eq!(
            a.binary.path,
            std::env::current_exe().unwrap().to_string_lossy()
        );
        assert!(a.binary.ino != 0 && a.binary.size != 0);
    }
}
