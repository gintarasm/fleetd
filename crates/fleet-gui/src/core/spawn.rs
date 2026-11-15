//! What a spawn from the GUI asks for: the New agent sheet's request, and
//! New terminal's (a plain login shell, labelled `kind=shell`).

use std::collections::HashMap;

use super::target::Target;

/// The label key New terminal sets. Not a `fleet.` key: those are the
/// core's (ADR 0025), and the core never interprets labels.
pub const KIND_LABEL: &str = "kind";
/// [`KIND_LABEL`]'s value for a plain shell, so the sidebar can style it
/// and status detection can skip it.
pub const SHELL_KIND: &str = "shell";
/// The shell when `$SHELL` is unset or empty.
pub const DEFAULT_SHELL: &str = "/bin/zsh";

/// A spawn: program, arguments, directory and labels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpawnRequest {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub labels: HashMap<String, String>,
}

/// `$SHELL` (passed in as `shell`), else [`DEFAULT_SHELL`].
pub fn login_shell(shell: Option<&str>) -> String {
    match shell.map(str::trim) {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => DEFAULT_SHELL.to_owned(),
    }
}

/// New terminal's folder: the focused agent's cwd, else the last-used
/// folder (the first of `recent`), else `home`.
pub fn terminal_dir(focused_cwd: Option<&str>, recent: &[String], home: &str) -> String {
    focused_cwd
        .filter(|d| !d.is_empty())
        .or_else(|| recent.first().map(String::as_str).filter(|d| !d.is_empty()))
        .unwrap_or(home)
        .to_owned()
}

/// New terminal's spawn: a login shell in `cwd`, labelled `kind=shell`.
/// Locally the shell is `shell` (the GUI's `$SHELL`); on an ssh target
/// that means nothing, so the remote `$SHELL` is looked up there, with
/// the same fallback.
pub fn new_terminal(target: &Target, shell: Option<&str>, cwd: String) -> SpawnRequest {
    let (program, args) = match target {
        Target::Local => (login_shell(shell), vec!["-l".to_owned()]),
        Target::Ssh { .. } => (
            "/bin/sh".to_owned(),
            vec![
                "-c".to_owned(),
                format!("exec \"${{SHELL:-{DEFAULT_SHELL}}}\" -l"),
            ],
        ),
    };
    SpawnRequest {
        program,
        args,
        cwd,
        labels: HashMap::from([(KIND_LABEL.to_owned(), SHELL_KIND.to_owned())]),
    }
}
