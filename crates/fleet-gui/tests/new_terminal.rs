//! New terminal (issue #21): the spawn request it sends.

use fleet_gui::core::spawn::{
    DEFAULT_SHELL, KIND_LABEL, SHELL_KIND, login_shell, new_terminal, terminal_dir,
};
use fleet_gui::core::target::Target;

#[test]
fn the_shell_is_shell_else_zsh() {
    assert_eq!(
        login_shell(Some("/opt/homebrew/bin/fish")),
        "/opt/homebrew/bin/fish"
    );
    assert_eq!(login_shell(Some("")), DEFAULT_SHELL);
    assert_eq!(login_shell(Some("  ")), DEFAULT_SHELL);
    assert_eq!(login_shell(None), DEFAULT_SHELL);
    assert_eq!(DEFAULT_SHELL, "/bin/zsh");
}

#[test]
fn the_folder_is_the_focused_agents_then_the_last_used_then_home() {
    let recent = ["/last".to_owned(), "/older".to_owned()];
    assert_eq!(terminal_dir(Some("/agent"), &recent, "/home/me"), "/agent");
    assert_eq!(terminal_dir(None, &recent, "/home/me"), "/last");
    // An agent with no cwd on record does not count.
    assert_eq!(terminal_dir(Some(""), &recent, "/home/me"), "/last");
    assert_eq!(terminal_dir(None, &[], "/home/me"), "/home/me");
    assert_eq!(terminal_dir(None, &[String::new()], "/home/me"), "/home/me");
}

#[test]
fn a_local_terminal_is_a_login_shell_labelled_kind_shell() {
    let req = new_terminal(&Target::Local, Some("/bin/bash"), "/work".into());
    assert_eq!(req.program, "/bin/bash");
    assert_eq!(req.args, ["-l"]);
    assert_eq!(req.cwd, "/work");
    assert_eq!(req.labels.len(), 1);
    assert_eq!(req.labels[KIND_LABEL], SHELL_KIND);
    assert_eq!((KIND_LABEL, SHELL_KIND), ("kind", "shell"));
    // Not a reserved key (ADR 0025).
    assert!(!KIND_LABEL.starts_with("fleet."));

    let req = new_terminal(&Target::Local, None, "/work".into());
    assert_eq!(
        (req.program.as_str(), req.args),
        ("/bin/zsh", vec!["-l".to_owned()])
    );
}

#[test]
fn a_remote_terminal_runs_the_remote_shell() {
    // The GUI's $SHELL means nothing on another host: the remote one's is
    // looked up there, with the same fallback.
    let remote = Target::Ssh {
        dest: "me@box".into(),
    };
    let req = new_terminal(&remote, Some("/bin/bash"), String::new());
    assert_eq!(req.program, "/bin/sh");
    assert_eq!(req.args, ["-c", "exec \"${SHELL:-/bin/zsh}\" -l"]);
    assert_eq!(req.cwd, "");
    assert_eq!(req.labels[KIND_LABEL], SHELL_KIND);
}
