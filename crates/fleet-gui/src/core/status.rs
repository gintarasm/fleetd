//! Agent status at a glance (issue #32, ADR 0042): working, needs you,
//! idle, done or failed, derived from what the GUI already has: the
//! agent's state and exit code, its last line and last-output time
//! (`AgentActivity`), and whether a pane showing it rang the bell.
//!
//! A heuristic, kept in [`agent_status`] alone; agent hooks replace it in
//! M5 (ADR 0008). Plain shells (`kind=shell`) get no status.

use std::collections::{HashMap, HashSet};

use fleet_proto::{AgentInfo, AgentState};

use super::session::AgentRow;
use super::spawn::{KIND_LABEL, SHELL_KIND};

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

/// A plain login shell from New terminal: no status.
pub fn is_shell(info: &AgentInfo) -> bool {
    info.labels.get(KIND_LABEL).map(String::as_str) == Some(SHELL_KIND)
}

/// The status of `row` at `now_ms`; `rang` is an unseen bell from a pane
/// showing it. `None` for a shell, or a state the GUI does not know.
pub fn agent_status(row: &AgentRow, rang: bool, now_ms: i64) -> Option<AgentStatus> {
    if is_shell(&row.info) {
        return None;
    }
    match row.info.state() {
        AgentState::Exited => Some(match row.info.exit_code {
            Some(0) => AgentStatus::Done,
            _ => AgentStatus::Failed,
        }),
        AgentState::Lost => Some(AgentStatus::Failed),
        AgentState::Unspecified => None,
        AgentState::Running => Some(if rang {
            AgentStatus::NeedsYou
        } else {
            match row.last_output_unix_ms {
                Some(at) if now_ms - at < QUIET_MS => AgentStatus::Working,
                Some(_) if looks_like_prompt(&row.last_line) => AgentStatus::NeedsYou,
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

/// Every non-shell agent's status; `bells` are agents with an unseen bell.
pub fn statuses(rows: &[AgentRow], bells: &HashSet<u64>, now_ms: i64) -> HashMap<u64, AgentStatus> {
    rows.iter()
        .filter_map(|r| agent_status(r, bells.contains(&r.info.id), now_ms).map(|s| (r.info.id, s)))
        .collect()
}

/// How many agents need you.
pub fn needs_you_count(statuses: &HashMap<u64, AgentStatus>) -> usize {
    statuses
        .values()
        .filter(|s| **s == AgentStatus::NeedsYou)
        .count()
}

/// The Dock badge and menu-bar text for `count` agents needing you: none
/// for 0, else the number.
pub fn badge_label(count: usize) -> Option<String> {
    (count > 0).then(|| count.to_string())
}

/// Picks the status changes worth a notification: an agent starting to
/// need you, or ending. The first update only learns the statuses (agents
/// already waiting or ended at launch are not news), and an agent seen for
/// the first time is news only if it already ended.
#[derive(Debug, Default)]
pub struct Alerts {
    last: HashMap<u64, AgentStatus>,
    primed: bool,
}

impl Alerts {
    /// The agents whose status became [`AgentStatus::NeedsYou`],
    /// [`AgentStatus::Done`] or [`AgentStatus::Failed`] since the last
    /// update, by id.
    pub fn update(&mut self, now: &HashMap<u64, AgentStatus>) -> Vec<(u64, AgentStatus)> {
        let mut out: Vec<(u64, AgentStatus)> = if self.primed {
            now.iter()
                .filter(|(id, s)| {
                    let prev = self.last.get(id).copied();
                    match **s {
                        AgentStatus::NeedsYou => {
                            prev.is_some_and(|p| p != AgentStatus::NeedsYou && !p.ended())
                        }
                        AgentStatus::Done | AgentStatus::Failed => prev.is_none_or(|p| !p.ended()),
                        AgentStatus::Working | AgentStatus::Idle => false,
                    }
                })
                .map(|(id, s)| (*id, *s))
                .collect()
        } else {
            Vec::new()
        };
        out.sort_by_key(|(id, _)| *id);
        self.last = now.clone();
        self.primed = true;
        out
    }
}

/// A notification's title and body for agent `name` entering `status`;
/// `detail` is its last line (needs you) or folder.
pub fn alert_text(
    name: &str,
    status: AgentStatus,
    exit_code: Option<i32>,
    detail: &str,
) -> (String, String) {
    let title = match status {
        AgentStatus::NeedsYou => format!("{name} needs you"),
        AgentStatus::Done => format!("{name} finished"),
        AgentStatus::Failed => format!("{name} failed"),
        AgentStatus::Working | AgentStatus::Idle => name.to_owned(),
    };
    let body = match (status, exit_code) {
        (AgentStatus::Failed, Some(code)) => format!("Exited with code {code} · {detail}"),
        (AgentStatus::Failed, None) => format!("Lost · {detail}"),
        _ => detail.to_owned(),
    };
    (title, body)
}

/// Whether a status dot draws dimmed at `now_ms`: a working agent's dot
/// breathes, dimming every other second (driven by the sidebar's
/// one-second tick, so it costs no animation frames); steady under Reduce
/// Motion, and for every other status.
pub fn dot_dimmed(status: AgentStatus, now_ms: i64, reduce_motion: bool) -> bool {
    status == AgentStatus::Working && !reduce_motion && now_ms.div_euclid(1000) % 2 == 1
}

/// The notification identifier for agent `agent_id` of session `epoch`
/// (`AppState::session_epoch`): a newer notification for the same agent
/// replaces the older one.
pub fn alert_id(epoch: u64, agent_id: u64) -> String {
    format!("fleet-agent-{epoch}-{agent_id}")
}

/// The `(epoch, agent_id)` of an [`alert_id`].
pub fn alert_agent(identifier: &str) -> Option<(u64, u64)> {
    let (epoch, id) = identifier.strip_prefix("fleet-agent-")?.split_once('-')?;
    Some((epoch.parse().ok()?, id.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: AgentState, code: Option<i32>, line: &str, at: Option<i64>) -> AgentRow {
        let mut info = AgentInfo {
            id: 1,
            exit_code: code,
            ..Default::default()
        };
        info.set_state(state);
        AgentRow {
            info,
            last_line: line.into(),
            last_output_unix_ms: at,
        }
    }

    const NOW: i64 = 100_000;

    #[test]
    fn recent_output_is_working() {
        let r = row(AgentState::Running, None, "> ", Some(NOW - 500));
        assert_eq!(agent_status(&r, false, NOW), Some(AgentStatus::Working));
        let r = row(AgentState::Running, None, "x", Some(NOW - QUIET_MS + 1));
        assert_eq!(agent_status(&r, false, NOW), Some(AgentStatus::Working));
    }

    #[test]
    fn quiet_with_a_prompt_needs_you_else_idle() {
        let r = row(
            AgentState::Running,
            None,
            "Continue? ",
            Some(NOW - QUIET_MS),
        );
        assert_eq!(agent_status(&r, false, NOW), Some(AgentStatus::NeedsYou));
        let r = row(
            AgentState::Running,
            None,
            "compiling foo",
            Some(NOW - 60_000),
        );
        assert_eq!(agent_status(&r, false, NOW), Some(AgentStatus::Idle));
        // No output seen yet.
        let r = row(AgentState::Running, None, "", None);
        assert_eq!(agent_status(&r, false, NOW), Some(AgentStatus::Idle));
    }

    #[test]
    fn a_bell_needs_you_even_while_working() {
        let r = row(AgentState::Running, None, "x", Some(NOW));
        assert_eq!(agent_status(&r, true, NOW), Some(AgentStatus::NeedsYou));
    }

    #[test]
    fn exit_zero_is_done_anything_else_failed() {
        let r = row(AgentState::Exited, Some(0), "", None);
        assert_eq!(agent_status(&r, true, NOW), Some(AgentStatus::Done));
        let r = row(AgentState::Exited, Some(2), "", None);
        assert_eq!(agent_status(&r, false, NOW), Some(AgentStatus::Failed));
        let r = row(AgentState::Exited, None, "", None);
        assert_eq!(agent_status(&r, false, NOW), Some(AgentStatus::Failed));
        let r = row(AgentState::Lost, None, "", None);
        assert_eq!(agent_status(&r, false, NOW), Some(AgentStatus::Failed));
    }

    #[test]
    fn shells_and_unknown_states_have_no_status() {
        let mut r = row(AgentState::Running, None, "$", Some(0));
        r.info.labels.insert("kind".into(), "shell".into());
        assert_eq!(agent_status(&r, true, NOW), None);
        let r = row(AgentState::Unspecified, None, "", None);
        assert_eq!(agent_status(&r, false, NOW), None);
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

    #[test]
    fn counts_and_badges() {
        let mut rows = vec![
            row(AgentState::Running, None, "?", Some(0)),
            row(AgentState::Running, None, "x", Some(NOW)),
            row(AgentState::Exited, Some(0), "", None),
        ];
        rows[1].info.id = 2;
        rows[2].info.id = 3;
        let mut bells = HashSet::new();
        let s = statuses(&rows, &bells, NOW);
        assert_eq!(needs_you_count(&s), 1);
        bells.insert(2);
        let s = statuses(&rows, &bells, NOW);
        assert_eq!(needs_you_count(&s), 2);
        assert_eq!(s[&3], AgentStatus::Done);
        assert_eq!(badge_label(0), None);
        assert_eq!(badge_label(2).as_deref(), Some("2"));
    }

    #[test]
    fn alerts_on_entering_needs_you_or_ending_after_the_first_update() {
        use AgentStatus::*;
        let mut a = Alerts::default();
        // At launch: nothing is news.
        assert!(
            a.update(&HashMap::from([(1, NeedsYou), (2, Done)]))
                .is_empty()
        );
        // Still waiting, still done: no repeat.
        assert!(
            a.update(&HashMap::from([(1, NeedsYou), (2, Done)]))
                .is_empty()
        );
        let s = HashMap::from([(1, Working), (2, Done), (3, Idle)]);
        assert!(a.update(&s).is_empty());
        let s = HashMap::from([(1, NeedsYou), (2, Done), (3, Failed)]);
        assert_eq!(a.update(&s), vec![(1, NeedsYou), (3, Failed)]);
        // A new agent: needs-you is not news until seen otherwise; an
        // agent that ended before it was ever seen is.
        let s = HashMap::from([(1, NeedsYou), (4, NeedsYou), (5, Done)]);
        assert_eq!(a.update(&s), vec![(5, Done)]);
        // Idle to needs-you (a bell) is news.
        let s = HashMap::from([(4, Idle)]);
        assert!(a.update(&s).is_empty());
        assert_eq!(
            a.update(&HashMap::from([(4, NeedsYou)])),
            vec![(4, NeedsYou)]
        );
    }

    #[test]
    fn alert_texts() {
        let t = alert_text("claude", AgentStatus::NeedsYou, None, "Proceed?");
        assert_eq!(t, ("claude needs you".into(), "Proceed?".into()));
        let t = alert_text("claude", AgentStatus::Done, Some(0), "~/w");
        assert_eq!(t, ("claude finished".into(), "~/w".into()));
        let t = alert_text("claude", AgentStatus::Failed, Some(3), "~/w");
        assert_eq!(
            t,
            ("claude failed".into(), "Exited with code 3 · ~/w".into())
        );
        let t = alert_text("claude", AgentStatus::Failed, None, "~/w");
        assert_eq!(t.1, "Lost · ~/w");
    }

    #[test]
    fn alert_ids_round_trip() {
        assert_eq!(alert_id(3, 42), "fleet-agent-3-42");
        assert_eq!(alert_agent(&alert_id(3, 42)), Some((3, 42)));
        assert_eq!(alert_agent("fleet-agent-3"), None);
        assert_eq!(alert_agent("other-3-42"), None);
        assert_eq!(alert_agent("fleet-agent-x-42"), None);
    }

    #[test]
    fn only_a_working_dot_breathes_and_not_under_reduce_motion() {
        assert!(!dot_dimmed(AgentStatus::Working, 0, false));
        assert!(dot_dimmed(AgentStatus::Working, 1_500, false));
        assert!(!dot_dimmed(AgentStatus::Working, 2_000, false));
        assert!(!dot_dimmed(AgentStatus::Working, 1_500, true));
        assert!(!dot_dimmed(AgentStatus::NeedsYou, 1_500, false));
    }
}
