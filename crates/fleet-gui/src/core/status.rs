//! Agent status at a glance (issue #32, ADR 0042): working, needs you,
//! idle, done or failed, derived from what the GUI already has: the
//! agent's state and exit code, its last line and last-output time
//! (`AgentActivity`), and whether a pane showing it rang the bell.
//!
//! A heuristic, kept in [`agent_status`] alone; agent hooks replace it in
//! M5 (ADR 0008). Plain shells (`kind=shell`) get no status.

use std::collections::{HashMap, HashSet};

use fleet_proto::AgentInfo;

use super::session::AgentRow;

use fleet_proto::ReportedState;
pub use fleet_proto::status::{AgentStatus, QUIET_MS, is_shell, live_report, looks_like_prompt};

/// How a status dot is drawn (ADR 0033, 0075): solid for a status the agent
/// reported (`fleet report`) or that is a fact (it exited), hollow for one
/// inferred from a running agent's output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DotFill {
    Solid,
    #[default]
    Hollow,
}

/// The fill of the dot for `status` of `info`.
pub fn dot_fill(info: &AgentInfo, status: AgentStatus) -> DotFill {
    if live_report(info).is_some() || status.ended() {
        DotFill::Solid
    } else {
        DotFill::Hollow
    }
}

/// The status an agent's report says (ADR 0075).
fn reported_status(state: ReportedState) -> Option<AgentStatus> {
    match state {
        ReportedState::ReportedWorking => Some(AgentStatus::Working),
        ReportedState::ReportedWaiting => Some(AgentStatus::NeedsYou),
        ReportedState::ReportedDone => Some(AgentStatus::Done),
        ReportedState::ReportedError => Some(AgentStatus::Failed),
        ReportedState::Unspecified => None,
    }
}

/// What a row or pane shows, as the GUI names it to the user: a shell
/// from New terminal is never called an agent (ADR 0071), though the node
/// runs it as one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Subject {
    #[default]
    Agent,
    Shell,
}

impl Subject {
    pub fn of(info: &AgentInfo) -> Self {
        if is_shell(info) {
            Subject::Shell
        } else {
            Subject::Agent
        }
    }

    /// "agent" or "shell", for mid-sentence use.
    pub fn noun(self) -> &'static str {
        match self {
            Subject::Agent => "agent",
            Subject::Shell => "shell",
        }
    }

    /// "Agent" or "Shell", for menu titles.
    pub fn title(self) -> &'static str {
        match self {
            Subject::Agent => "Agent",
            Subject::Shell => "Shell",
        }
    }
}

/// The status of `row` at `now_ms`: the agent's own report while it runs,
/// else inferred (the exit for an ended agent); `rang` is an unseen bell from a pane
/// showing it. `None` for a shell, or a state the GUI does not know.
pub fn agent_status(row: &AgentRow, rang: bool, now_ms: i64) -> Option<AgentStatus> {
    // What the agent said about itself beats a guess (ADR 0075).
    if let Some(status) = live_report(&row.info).and_then(|r| reported_status(r.state())) {
        return Some(status);
    }
    fleet_proto::status::inferred_status(
        &row.info,
        &row.last_line,
        row.last_output_unix_ms,
        rang,
        now_ms,
        QUIET_MS,
    )
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
    use fleet_proto::AgentState;

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

    fn reported(mut r: AgentRow, state: fleet_proto::ReportedState) -> AgentRow {
        r.info.reported = Some(fleet_proto::Reported {
            state: state.into(),
            message: "m".into(),
            at_unix_ms: 1,
        });
        r
    }

    #[test]
    fn a_reported_status_wins_over_the_inferred_one_and_is_solid() {
        use fleet_proto::ReportedState as R;
        // Quiet with no prompt would be idle; a bell would need you.
        let quiet = row(AgentState::Running, None, "compiling", Some(NOW - 60_000));
        assert_eq!(agent_status(&quiet, false, NOW), Some(AgentStatus::Idle));
        assert_eq!(dot_fill(&quiet.info, AgentStatus::Idle), DotFill::Hollow);
        for (state, want) in [
            (R::ReportedWorking, AgentStatus::Working),
            (R::ReportedWaiting, AgentStatus::NeedsYou),
            (R::ReportedDone, AgentStatus::Done),
            (R::ReportedError, AgentStatus::Failed),
        ] {
            let r = reported(quiet.clone(), state);
            assert_eq!(agent_status(&r, false, NOW), Some(want), "{state:?}");
            assert_eq!(agent_status(&r, true, NOW), Some(want), "bell, {state:?}");
            assert_eq!(dot_fill(&r.info, want), DotFill::Solid);
        }
        // Counted like any needs-you.
        let waiting = reported(quiet, R::ReportedWaiting);
        assert_eq!(
            needs_you_count(&statuses(&[waiting], &HashSet::new(), NOW)),
            1
        );
    }

    #[test]
    fn an_ended_agent_ignores_its_last_report_and_is_solid() {
        let r = reported(
            row(AgentState::Exited, Some(2), "", None),
            fleet_proto::ReportedState::ReportedWorking,
        );
        assert_eq!(agent_status(&r, false, NOW), Some(AgentStatus::Failed));
        // The exit is a fact, not an inference.
        assert_eq!(dot_fill(&r.info, AgentStatus::Failed), DotFill::Solid);
        let shell = {
            let mut s = reported(
                row(AgentState::Running, None, "$", Some(0)),
                fleet_proto::ReportedState::ReportedWorking,
            );
            s.info.labels.insert("kind".into(), "shell".into());
            s
        };
        assert_eq!(agent_status(&shell, false, NOW), None);
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
