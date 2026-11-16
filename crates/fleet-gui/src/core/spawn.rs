//! What a spawn from the GUI asks for: the New agent sheet's request
//! (with its name, ADR 0052), and New terminal's (a plain login shell,
//! labelled `kind=shell`).

use std::collections::HashMap;

use fleet_client::names::{NAME_LABEL, check_name, default_name};

use super::target::Target;

pub use fleet_proto::status::{KIND_LABEL, SHELL_KIND};
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

/// Splits a command line with shell quoting rules into program and args.
pub fn parse_command(line: &str) -> Result<(String, Vec<String>), String> {
    let mut words = shell_words::split(line)
        .map_err(|e| format!("cannot parse the command: {e}"))?
        .into_iter();
    let program = words.next().ok_or("enter a command")?;
    Ok((program, words.collect()))
}

/// The New agent sheet's Name field (ADR 0052): prefilled with the
/// generated name, which follows the command and directory as they are
/// typed until the user edits the field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameDraft {
    generated: String,
    edited: bool,
    /// The live agents' names, which the generated one avoids.
    taken: Vec<String>,
}

impl NameDraft {
    pub fn new(command: &str, cwd: &str, taken: Vec<String>) -> Self {
        let mut d = NameDraft {
            generated: String::new(),
            edited: false,
            taken,
        };
        d.generated = d.generate(command, cwd).unwrap_or_default();
        d
    }

    fn generate(&self, command: &str, cwd: &str) -> Option<String> {
        let (program, args) = parse_command(command).ok()?;
        let taken: Vec<&str> = self.taken.iter().map(String::as_str).collect();
        Some(default_name(&program, &args, cwd, &taken))
    }

    /// The name the field shows while not edited.
    pub fn generated(&self) -> &str {
        &self.generated
    }

    /// The user edited the field: it no longer follows the command.
    pub fn edited(&self) -> bool {
        self.edited
    }

    /// The command or directory changed: the field's new text, while the
    /// user has not edited it. A command that does not parse yet (an open
    /// quote) keeps the last name.
    pub fn inputs_changed(&mut self, command: &str, cwd: &str) -> Option<String> {
        if self.edited {
            return None;
        }
        let name = self.generate(command, cwd)?;
        (name != self.generated).then(|| {
            self.generated = name.clone();
            name
        })
    }

    /// The field now reads `text`; any text but the generated name is the
    /// user's edit.
    pub fn field_changed(&mut self, text: &str) {
        if text != self.generated {
            self.edited = true;
        }
    }

    /// The name to spawn with: the field's, checked; empty is the
    /// generated one.
    pub fn name(&self, text: &str) -> Result<String, String> {
        if text.trim().is_empty() && !self.generated.is_empty() {
            return Ok(self.generated.clone());
        }
        check_name(text)
    }
}

/// The New agent sheet's Labels field: `k=v` pairs separated by commas
/// (blank entries skipped). The name has its own field.
pub fn parse_label_list(text: &str) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    for item in text.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        let (k, v) = match item.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() => (k.trim(), v.trim()),
            _ => return Err(format!("label {item:?}: expected key=value")),
        };
        if k == NAME_LABEL {
            return Err("set the name in the Name field".into());
        }
        out.insert(k.to_owned(), v.to_owned());
    }
    Ok(out)
}

/// New terminal's spawn: a login shell in `cwd`, labelled `kind=shell`
/// and named `shell-<animal>` (unique among `taken`, the live agents'
/// names). Locally the shell is `shell` (the GUI's `$SHELL`); on an ssh
/// target that means nothing, so the remote `$SHELL` is looked up there,
/// with the same fallback.
pub fn new_terminal(
    target: &Target,
    shell: Option<&str>,
    cwd: String,
    taken: &[&str],
) -> SpawnRequest {
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
    // Named as the plain shell it is, whatever runs it on a remote host.
    let name = default_name("sh", &[], &cwd, taken);
    SpawnRequest {
        program,
        args,
        cwd,
        labels: HashMap::from([
            (KIND_LABEL.to_owned(), SHELL_KIND.to_owned()),
            (NAME_LABEL.to_owned(), name),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_uses_shell_quoting() {
        assert_eq!(
            parse_command("sh -c 'echo \"hi there\"'"),
            Ok((
                "sh".to_owned(),
                vec!["-c".to_owned(), "echo \"hi there\"".to_owned()]
            ))
        );
        assert_eq!(parse_command("claude"), Ok(("claude".to_owned(), vec![])));
        assert!(parse_command("   ").is_err());
        assert!(parse_command("echo 'open").is_err());
    }

    #[test]
    fn the_name_follows_the_command_until_edited() {
        let mut d = NameDraft::new("claude", "/w/fleetd", vec![]);
        assert!(d.generated().starts_with("fleetd-"), "{}", d.generated());
        let first = d.generated().to_owned();
        // The field shows it: not an edit.
        d.field_changed(&first);
        assert!(!d.edited());
        // Typing the prompt changes the name.
        let next = d
            .inputs_changed("claude 'fix the SIGTERM race'", "/w/fleetd")
            .expect("a new name");
        assert!(next.starts_with("sigterm-"), "{next}");
        d.field_changed(&next);
        // An open quote keeps the last name.
        assert_eq!(d.inputs_changed("claude 'fix the LTO", "/w/fleetd"), None);
        assert_eq!(d.generated(), next);
        // The same inputs: nothing to change.
        assert_eq!(
            d.inputs_changed("claude 'fix the SIGTERM race'", "/w/fleetd"),
            None
        );
        // The user edits it: the command no longer changes it.
        d.field_changed("my-name");
        assert!(d.edited());
        assert_eq!(d.inputs_changed("claude 'tune LTO'", "/w/fleetd"), None);
        assert_eq!(d.name("my-name"), Ok("my-name".into()));
    }

    #[test]
    fn the_generated_name_avoids_live_names_and_fills_an_empty_field() {
        let d = NameDraft::new("claude", "/w/fleetd", vec![]);
        let taken = vec![d.generated().to_owned()];
        let d2 = NameDraft::new("claude", "/w/fleetd", taken.clone());
        assert_ne!(d2.generated(), taken[0]);
        assert_eq!(d2.name("  "), Ok(d2.generated().to_owned()));
        assert!(d2.name("123").is_err());
    }

    #[test]
    fn label_list_parses_pairs_and_refuses_the_name() {
        assert_eq!(parse_label_list(""), Ok(HashMap::new()));
        assert_eq!(
            parse_label_list(" role=reviewer, ,team = core "),
            Ok(HashMap::from([
                ("role".to_owned(), "reviewer".to_owned()),
                ("team".to_owned(), "core".to_owned()),
            ]))
        );
        assert!(parse_label_list("role").is_err());
        assert!(parse_label_list("=x").is_err());
        assert!(parse_label_list("name=x").is_err());
    }

    #[test]
    fn new_terminal_is_a_named_shell() {
        let req = new_terminal(&Target::Local, Some("/bin/zsh"), "/w".into(), &[]);
        assert!(req.labels[NAME_LABEL].starts_with("shell-"), "{req:?}");
        let remote = Target::Ssh { dest: "box".into() };
        let taken = [req.labels[NAME_LABEL].as_str()];
        let r2 = new_terminal(&remote, None, "/w".into(), &taken);
        assert!(r2.labels[NAME_LABEL].starts_with("shell-"));
        assert_ne!(r2.labels[NAME_LABEL], req.labels[NAME_LABEL]);
    }
}
