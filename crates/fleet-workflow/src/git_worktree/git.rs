//! `git` through the host's `Exec` (ADR 0093): never a prompt
//! (`GIT_TERMINAL_PROMPT=0`), paths unquoted (`core.quotepath=off`), the C
//! locale for stable messages, and reads that never take an optional lock
//! (`--no-optional-locks`, `GIT_OPTIONAL_LOCKS=0`), so they never block an
//! agent's own git.

use crate::error::ActionError;
use crate::host::{ExecOutput, ExecRequest, Host, HostResult};

/// A read, or a command that changes the repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Read,
    Write,
}

/// How long one git command may take, unless said otherwise.
pub const TIMEOUT_MS: u64 = 120_000;
/// `fetch_base`'s fetch (M4 spec §3).
pub const FETCH_TIMEOUT_MS: u64 = 30_000;

pub struct Git<'a> {
    pub host: &'a dyn Host,
}

/// The argv and env of `git args` in `mode`.
pub fn request(cwd: &str, args: &[&str], mode: Mode, timeout_ms: u64) -> ExecRequest {
    let mut argv = vec!["git".to_string(), "-c".into(), "core.quotepath=off".into()];
    if mode == Mode::Read {
        argv.push("--no-optional-locks".into());
    }
    argv.extend(args.iter().map(|a| (*a).to_string()));
    let mut env = crate::host::Labels::new();
    env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    env.insert("LC_ALL".into(), "C".into());
    if mode == Mode::Read {
        env.insert("GIT_OPTIONAL_LOCKS".into(), "0".into());
    }
    ExecRequest {
        argv,
        cwd: cwd.to_owned(),
        env,
        timeout_ms,
        ..Default::default()
    }
}

/// What a failed command said, for an error message.
pub fn failure(args: &[&str], out: &ExecOutput) -> String {
    let said = String::from_utf8_lossy(&out.stderr);
    let said = said.trim();
    let what = if out.timed_out {
        "timed out".to_string()
    } else if said.is_empty() {
        format!("exited with {:?}", out.exit_code)
    } else {
        said.to_owned()
    };
    format!("git {}: {what}", args.join(" "))
}

impl Git<'_> {
    /// Runs `git args` in `cwd`, whatever its exit.
    pub async fn run(&self, cwd: &str, args: &[&str], mode: Mode) -> HostResult<ExecOutput> {
        self.host.exec(request(cwd, args, mode, TIMEOUT_MS)).await
    }

    /// Runs `git args` and returns its stdout, or fails with its stderr.
    pub async fn ok(&self, cwd: &str, args: &[&str], mode: Mode) -> HostResult<String> {
        let out = self.run(cwd, args, mode).await?;
        if out.exit_code == Some(0) {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(ActionError::failed(failure(args, &out)))
        }
    }

    /// [`Git::ok`], trimmed of the trailing newline.
    pub async fn line(&self, cwd: &str, args: &[&str]) -> HostResult<String> {
        Ok(self.ok(cwd, args, Mode::Read).await?.trim_end().to_owned())
    }

    /// Whether a read exits 0 (`merge-base --is-ancestor`, `show-ref`).
    pub async fn test(&self, cwd: &str, args: &[&str]) -> HostResult<bool> {
        let out = self.run(cwd, args, Mode::Read).await?;
        match out.exit_code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(ActionError::failed(failure(args, &out))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_take_no_optional_locks_and_never_prompt() {
        let r = request("/r", &["status", "-z"], Mode::Read, 5);
        assert_eq!(
            r.argv,
            ["git", "-c", "core.quotepath=off", "--no-optional-locks", "status", "-z"]
        );
        assert_eq!(r.env["GIT_TERMINAL_PROMPT"], "0");
        assert_eq!(r.env["GIT_OPTIONAL_LOCKS"], "0");
        assert_eq!((r.cwd.as_str(), r.timeout_ms), ("/r", 5));
        let w = request("/r", &["worktree", "add"], Mode::Write, 5);
        assert_eq!(w.argv, ["git", "-c", "core.quotepath=off", "worktree", "add"]);
        assert_eq!(w.env["GIT_TERMINAL_PROMPT"], "0");
        assert!(!w.env.contains_key("GIT_OPTIONAL_LOCKS"));
    }
}
