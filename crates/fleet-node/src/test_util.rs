//! Helpers for this crate's unit tests.

use std::path::Path;
use std::process::Command;

/// Writes `contents` to `path` (replacing it) and makes it executable
/// (0755). A `sh` child writes it, never this process: a process another
/// test thread forks while this one held the file open for writing keeps
/// that fd until its own exec, and executing the file in that window fails
/// with ETXTBSY ("Text file busy", Linux; #188, #193).
pub(crate) fn write_exec(path: &Path, contents: &str) {
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"printf '%s' "$2" > "$1" && chmod 755 "$1""#)
        .arg("sh")
        .arg(path)
        .arg(contents)
        .status()
        .unwrap();
    assert!(status.success(), "writing {}", path.display());
}
