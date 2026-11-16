//! The inferred agent status (issue #32, ADR 0042, moved here by ADR 0075):
//! working, needs you, idle, done or failed, derived from what a client or
//! the node has: the agent's state and exit code, its last output line and
//! the time of its last output. Shared and free of any UI so the GUI and
//! the node (`fleet wait --state waiting`, exit reports) agree.
//!
//! A heuristic, kept in [`inferred_status`] alone. A status the agent
//! *reported* (`fleet report`, `AgentInfo.reported`) wins over it where
//! both exist.

use crate::{AgentInfo, AgentState, Reported, ReportedState};

/// The label key New terminal sets. Not a `fleet.` key: those are the
/// core's (ADR 0025), and the core never interprets labels.
pub const KIND_LABEL: &str = "kind";
/// [`KIND_LABEL`]'s value for a plain shell, so the sidebar can style it
/// and status detection can skip it.
pub const SHELL_KIND: &str = "shell";

/// How long a prompt-like last line must have been quiet for the node to
/// call an agent waiting (spec §2). The GUI uses the shorter [`QUIET_MS`].
pub const WAITING_QUIET_MS: i64 = 30_000;

/// Output this recent means the agent is working. Agent CLIs redraw a
/// spinner while they work, so a few quiet seconds mean they stopped.
pub const QUIET_MS: i64 = 3_000;

/// One agent's status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentStatus {
    /// Output in the last [`QUIET_MS`].
    Working,
    /// Quiet and its last line looks like a prompt, or it rang the bell.
    NeedsYou,
    /// Running, quiet, and nothing says it waits for input (or no output
    /// seen yet).
    Idle,
    /// Exited 0.
    Done,
    /// Exited non-zero (or with no code), or lost.
    Failed,
}

impl AgentStatus {
    /// For tooltips and menus.
    pub fn label(self) -> &'static str {
        match self {
            AgentStatus::Working => "Working",
            AgentStatus::NeedsYou => "Needs you",
            AgentStatus::Idle => "Idle",
            AgentStatus::Done => "Done",
            AgentStatus::Failed => "Failed",
        }
    }

    /// The agent ended (done or failed).
    pub fn ended(self) -> bool {
        matches!(self, AgentStatus::Done | AgentStatus::Failed)
    }
}

impl ReportedState {
    /// The word `fleet report` takes and `fleet ls` / `fleet tree` print.
    pub fn name(self) -> &'static str {
        match self {
            ReportedState::Unspecified => "unknown",
            ReportedState::ReportedWorking => "working",
            ReportedState::ReportedWaiting => "waiting",
            ReportedState::ReportedDone => "done",
            ReportedState::ReportedError => "error",
        }
    }

    /// The state a [`ReportedState::name`] names (not "unknown").
    pub fn from_name(name: &str) -> Option<Self> {
        [
            ReportedState::ReportedWorking,
            ReportedState::ReportedWaiting,
            ReportedState::ReportedDone,
            ReportedState::ReportedError,
        ]
        .into_iter()
        .find(|s| s.name() == name)
    }
}

/// The agent's own status report, while it counts: the agent is running
/// and not a shell. An ended agent speaks through its exit (done or
/// failed), not through what it last said (ADR 0075).
pub fn live_report(info: &AgentInfo) -> Option<&Reported> {
    if info.state() != AgentState::Running || is_shell(info) {
        return None;
    }
    info.reported
        .as_ref()
        .filter(|r| r.state() != ReportedState::Unspecified)
}

/// A plain login shell from New terminal: no status.
pub fn is_shell(info: &AgentInfo) -> bool {
    info.labels.get(KIND_LABEL).map(String::as_str) == Some(SHELL_KIND)
}

/// The inferred status of `info` at `now_ms`. `last_line` and
/// `last_output_unix_ms` are its last output line and time (`AgentActivity`);
/// `rang` is an unseen bell; `quiet_ms` is how long output must have been
/// absent before a prompt-like line means waiting ([`QUIET_MS`] for the
/// GUI, [`WAITING_QUIET_MS`] for the node). `None` for a shell, or a state
/// the client does not know.
pub fn inferred_status(
    info: &AgentInfo,
    last_line: &str,
    last_output_unix_ms: Option<i64>,
    rang: bool,
    now_ms: i64,
    quiet_ms: i64,
) -> Option<AgentStatus> {
    if is_shell(info) {
        return None;
    }
    match info.state() {
        AgentState::Exited => Some(match info.exit_code {
            Some(0) => AgentStatus::Done,
            _ => AgentStatus::Failed,
        }),
        AgentState::Lost => Some(AgentStatus::Failed),
        AgentState::Unspecified => None,
        AgentState::Running => Some(if rang {
            AgentStatus::NeedsYou
        } else {
            match last_output_unix_ms {
                Some(at) if now_ms - at < quiet_ms => AgentStatus::Working,
                Some(_) if looks_like_prompt(last_line) => AgentStatus::NeedsYou,
                _ => AgentStatus::Idle,
            }
        }),
    }
}

/// Footer hints agent TUIs show while they wait for you (lowercase).
const PROMPT_HINTS: [&str; 9] = [
    "(y/n)",
    "[y/n]",
    "(yes/no)",
    "for shortcuts",
    "shift+tab to cycle",
    "esc to cancel",
    "enter to confirm",
    "enter to select",
    "press enter",
];

/// Characters a prompt line ends with.
const PROMPT_ENDS: [char; 9] = ['?', '>', '❯', '›', '»', ':', '$', '#', '%'];

/// Whether an agent's last non-blank line looks like it waits for input:
/// it ends like a prompt (`?`, `>`, `❯`, `:`, `$` …), carries a TUI's
/// waiting hint ("? for shortcuts", "(y/n)", "esc to cancel" …), or is a
/// box-drawing rule (the bottom of a TUI's input box).
pub fn looks_like_prompt(line: &str) -> bool {
    let line = line.trim();
    let Some(last) = line.chars().next_back() else {
        return false;
    };
    if PROMPT_ENDS.contains(&last) {
        return true;
    }
    let lower = line.to_lowercase();
    if PROMPT_HINTS.iter().any(|h| lower.contains(h)) {
        return true;
    }
    line.chars()
        .all(|c| ('\u{2500}'..='\u{257f}').contains(&c) || c == ' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(state: AgentState, code: Option<i32>) -> AgentInfo {
        let mut i = AgentInfo {
            id: 1,
            exit_code: code,
            ..Default::default()
        };
        i.set_state(state);
        i
    }

    const NOW: i64 = 100_000;

    fn st(i: &AgentInfo, line: &str, at: Option<i64>, rang: bool) -> Option<AgentStatus> {
        inferred_status(i, line, at, rang, NOW, QUIET_MS)
    }

    #[test]
    fn recent_output_is_working() {
        let i = info(AgentState::Running, None);
        assert_eq!(
            st(&i, "> ", Some(NOW - 500), false),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            st(&i, "x", Some(NOW - QUIET_MS + 1), false),
            Some(AgentStatus::Working)
        );
    }

    #[test]
    fn quiet_with_a_prompt_needs_you_else_idle() {
        let i = info(AgentState::Running, None);
        assert_eq!(
            st(&i, "Continue? ", Some(NOW - QUIET_MS), false),
            Some(AgentStatus::NeedsYou)
        );
        assert_eq!(
            st(&i, "compiling foo", Some(NOW - 60_000), false),
            Some(AgentStatus::Idle)
        );
        assert_eq!(st(&i, "", None, false), Some(AgentStatus::Idle));
    }

    #[test]
    fn the_nodes_quiet_period_is_longer() {
        let i = info(AgentState::Running, None);
        let at = Some(NOW - 10_000);
        assert_eq!(st(&i, "Continue? ", at, false), Some(AgentStatus::NeedsYou));
        assert_eq!(
            inferred_status(&i, "Continue? ", at, false, NOW, WAITING_QUIET_MS),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            inferred_status(
                &i,
                "Continue? ",
                Some(NOW - WAITING_QUIET_MS),
                false,
                NOW,
                WAITING_QUIET_MS
            ),
            Some(AgentStatus::NeedsYou)
        );
    }

    #[test]
    fn a_bell_needs_you_even_while_working() {
        let i = info(AgentState::Running, None);
        assert_eq!(st(&i, "x", Some(NOW), true), Some(AgentStatus::NeedsYou));
    }

    #[test]
    fn exit_zero_is_done_anything_else_failed() {
        assert_eq!(
            st(&info(AgentState::Exited, Some(0)), "", None, true),
            Some(AgentStatus::Done)
        );
        assert_eq!(
            st(&info(AgentState::Exited, Some(2)), "", None, false),
            Some(AgentStatus::Failed)
        );
        assert_eq!(
            st(&info(AgentState::Exited, None), "", None, false),
            Some(AgentStatus::Failed)
        );
        assert_eq!(
            st(&info(AgentState::Lost, None), "", None, false),
            Some(AgentStatus::Failed)
        );
    }

    #[test]
    fn reported_states_have_names_both_ways() {
        for n in ["working", "waiting", "done", "error"] {
            assert_eq!(ReportedState::from_name(n).unwrap().name(), n);
        }
        assert_eq!(ReportedState::from_name("busy"), None);
        assert_eq!(ReportedState::from_name("unknown"), None);
    }

    #[test]
    fn a_report_counts_while_the_agent_runs_and_is_not_a_shell() {
        let rep = Reported {
            state: ReportedState::ReportedWaiting.into(),
            message: "m".into(),
            at_unix_ms: 5,
        };
        let mut i = info(AgentState::Running, None);
        assert_eq!(live_report(&i), None);
        i.reported = Some(rep.clone());
        assert_eq!(live_report(&i), Some(&rep));
        i.labels.insert("kind".into(), "shell".into());
        assert_eq!(live_report(&i), None);
        i.labels.clear();
        i.set_state(AgentState::Exited);
        assert_eq!(live_report(&i), None);
        i.set_state(AgentState::Lost);
        assert_eq!(live_report(&i), None);
    }

    #[test]
    fn shells_and_unknown_states_have_no_status() {
        let mut i = info(AgentState::Running, None);
        i.labels.insert("kind".into(), "shell".into());
        assert_eq!(st(&i, "$", Some(0), true), None);
        assert_eq!(
            st(&info(AgentState::Unspecified, None), "", None, false),
            None
        );
    }

    #[test]
    fn prompt_lines() {
        for yes in [
            "Do you want to proceed?",
            "> ",
            "❯",
            "Password:",
            "user@host:~$",
            "Overwrite file? (y/N) [y/n]",
            "  ? for shortcuts",
            "⏵⏵ accept edits on (shift+tab to cycle)",
            "Esc to cancel · Tab to amend",
            "╰──────────────╯",
            "────────────",
        ] {
            assert!(looks_like_prompt(yes), "{yes:?}");
        }
        for no in [
            "",
            "   ",
            "Compiling fleet-gui v0.1.0",
            "✻ Thinking… (esc to interrupt)",
            "done.",
            "100%|████| 3/3",
        ] {
            assert!(!looks_like_prompt(no), "{no:?}");
        }
    }
}
