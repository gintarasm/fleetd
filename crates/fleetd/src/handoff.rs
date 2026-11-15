//! What a daemon needs to restart into a new binary: its own identity, the
//! handoff file passed to the successor, and the pre-flight check the old
//! daemon runs on the new binary before it commits to `exec`.
//!
//! Here only what every start runs; the restart itself is in `transfer`.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::RawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use fleet_proto::{BinaryIdentity, HANDOFF_VERSION};

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

/// Deletes a leftover handoff file. Called on every normal start, after the
/// instance lock is taken: with the lock held no other fleetd can be
/// mid-restart on this state dir, so any file here is from a restart that
/// never finished and describes agents this process did not inherit.
pub fn remove_stale(state_dir: &Path) {
    let _ = fs::remove_file(state_dir.join(HANDOFF_FILE));
}

/// The environment variable naming the record's fd in the successor.
pub const HANDOFF_FD_ENV: &str = "FLEETD_HANDOFF_FD";
/// The environment variable listing the handed-over agents' pids
/// (comma-separated), so a successor that cannot read the record can still
/// end them: they are its children, holding inherited terminals, and must
/// not run on untracked.
pub const HANDOFF_PIDS_ENV: &str = "FLEETD_HANDOFF_PIDS";

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

mod transfer;
pub use transfer::*;

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::handoff::HandoffAgent;
    use fleet_proto::handoff::HandoffRecord;
    use std::os::fd::IntoRawFd;
    use std::os::unix::fs::PermissionsExt;

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
        let own = "/opt/own/fleetd";
        assert_eq!(
            resolve_binary("/opt/x/fleetd", own).unwrap(),
            Path::new("/opt/x/fleetd")
        );
        assert!(resolve_binary("rel/fleetd", own).unwrap().is_absolute());
        // `sh` is on every PATH the tests run with.
        let sh = resolve_binary("sh", own).unwrap();
        assert!(sh.is_absolute() && sh.ends_with("sh"));
        assert_eq!(
            resolve_binary("no-such-fleetd-binary", own).unwrap_err(),
            "no no-such-fleetd-binary on the daemon's PATH"
        );
        // Empty: the daemon's own binary, never `fleetd` on PATH.
        assert_eq!(resolve_binary("", own).unwrap(), Path::new(own));
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
