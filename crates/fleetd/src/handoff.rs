//! What a daemon needs to restart into a new binary: its own identity, the
//! handoff file passed to the successor, and the pre-flight check the old
//! daemon runs on the new binary before it commits to `exec`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
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
    let mut child = Command::new(binary)
        .arg("--handoff-check")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| NOT_EXEC.to_string())?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("check timed out".into());
            }
        }
    };
    if !status.success() {
        return Err(format!(
            "check failed (exit {})",
            status.code().unwrap_or(-1)
        ));
    }
    // The child has exited, so this does not block; the output is a line or
    // two (a child that fills the pipe never exits and times out above).
    let mut out = String::new();
    if let Some(stdout) = child.stdout.take() {
        let _ = stdout.take(4096).read_to_string(&mut out);
    }
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
