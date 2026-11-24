//! Helpers shared by fleetd's integration tests (`mod support;`).
//!
//! Files a test executes are written by a child process, never this one.
//! Tests run on many threads, and a process another thread forks while this
//! one holds a file open for writing keeps that fd until its own exec: if
//! the file is executed in that window, the exec fails with ETXTBSY ("Text
//! file busy", Linux). A child that writes the file and exits leaves no
//! write fd in this process to inherit (#159, #188, #193).

// Each test binary uses only some of these.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

/// Writes `contents` to `path` (replacing it) and makes it executable
/// (0755), from a `sh` child.
pub fn write_exec(path: &Path, contents: &str) {
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

/// [`write_exec`] of a `#!/bin/sh` script with `body`, at `dir/name`.
pub fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    write_exec(&path, &format!("#!/bin/sh\n{body}\n"));
    path
}

/// Copies the executable `from` to `to` (a new file there), from a `cp`
/// child, rather than with `fs::copy`, which holds `to` open for writing.
pub fn copy_exec(from: &Path, to: &Path) {
    let status = Command::new("cp").arg(from).arg(to).status().unwrap();
    assert!(
        status.success(),
        "copying {} to {}",
        from.display(),
        to.display()
    );
}
