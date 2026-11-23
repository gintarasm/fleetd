//! Kill / Restart / Remove on the sidebar's selection (issue #138, ADR
//! 0099). Each action applies only to the selected agents it fits: Kill
//! to the running ones, Restart and Remove to the ones that exited or
//! are lost. What each offers, what it is called, and why it is
//! unavailable.

use super::session::AgentRow;
use super::sidebar::ended;

/// The selected agents each bulk action applies to, in the selection's
/// order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BulkTargets {
    /// Running: Kill.
    pub running: Vec<u64>,
    /// Exited or lost: Restart and Remove.
    pub ended: Vec<u64>,
}

impl BulkTargets {
    /// Splits `selected` by what `agents` say; an id the list does not
    /// carry is dropped.
    pub fn of(selected: &[u64], agents: &[AgentRow]) -> Self {
        let mut out = BulkTargets::default();
        for &id in selected {
            if let Some(row) = agents.iter().find(|r| r.info.id == id) {
                if ended(row) {
                    out.ended.push(id);
                } else {
                    out.running.push(id);
                }
            }
        }
        out
    }
}

/// A bulk action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulkAction {
    Kill,
    Restart,
    Remove,
}

impl BulkAction {
    /// The agents of `t` it applies to.
    pub fn targets(self, t: &BulkTargets) -> &[u64] {
        match self {
            BulkAction::Kill => &t.running,
            BulkAction::Restart | BulkAction::Remove => &t.ended,
        }
    }

    /// "Kill 3 Agents…", "Restart 2 Exited", "Remove 2 Exited" (with
    /// one: "Kill 1 Agent…").
    pub fn title(self, n: usize) -> String {
        let agents = if n == 1 { "Agent" } else { "Agents" };
        match self {
            // It asks first.
            BulkAction::Kill => format!("Kill {n} {agents}…"),
            BulkAction::Restart => format!("Restart {n} Exited"),
            BulkAction::Remove => format!("Remove {n} Exited"),
        }
    }

    /// Why it is unavailable with no target, or `None` if it has some.
    pub fn disabled_reason(self, t: &BulkTargets) -> Option<&'static str> {
        if !self.targets(t).is_empty() {
            return None;
        }
        Some(match self {
            BulkAction::Kill => "none of the selected agents is running",
            BulkAction::Restart | BulkAction::Remove => "none of the selected agents has exited",
        })
    }

    /// The menu line: the title, or "Kill Agents… (none of the selected
    /// agents is running)" when it does not apply.
    pub fn menu_title(self, t: &BulkTargets) -> String {
        match self.disabled_reason(t) {
            None => self.title(self.targets(t).len()),
            Some(why) => {
                let base = match self {
                    BulkAction::Kill => "Kill Agents…",
                    BulkAction::Restart => "Restart Exited",
                    BulkAction::Remove => "Remove Exited",
                };
                format!("{base} ({why})")
            }
        }
    }
}

/// The confirm sheet's question: "Kill 3 agents?".
pub fn kill_many_question(n: usize) -> String {
    format!("Kill {n} {}?", if n == 1 { "agent" } else { "agents" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::{AgentInfo, AgentState};

    fn row(id: u64, state: AgentState) -> AgentRow {
        let mut info = AgentInfo {
            id,
            ..Default::default()
        };
        info.set_state(state);
        AgentRow {
            info,
            last_line: String::new(),
            last_output_unix_ms: None,
        }
    }

    fn agents() -> Vec<AgentRow> {
        vec![
            row(1, AgentState::Running),
            row(2, AgentState::Exited),
            row(3, AgentState::Running),
            row(4, AgentState::Lost),
        ]
    }

    #[test]
    fn the_selection_is_split_by_what_each_action_fits() {
        let t = BulkTargets::of(&[4, 1, 2, 3, 99], &agents());
        assert_eq!(t.running, vec![1, 3]);
        assert_eq!(t.ended, vec![4, 2]);
        assert_eq!(BulkAction::Kill.targets(&t), &[1, 3]);
        assert_eq!(BulkAction::Restart.targets(&t), &[4, 2]);
        assert_eq!(BulkAction::Remove.targets(&t), &[4, 2]);
    }

    #[test]
    fn titles_count_what_they_apply_to() {
        assert_eq!(BulkAction::Kill.title(3), "Kill 3 Agents…");
        assert_eq!(BulkAction::Kill.title(1), "Kill 1 Agent…");
        assert_eq!(BulkAction::Restart.title(2), "Restart 2 Exited");
        assert_eq!(BulkAction::Remove.title(2), "Remove 2 Exited");
        assert_eq!(kill_many_question(3), "Kill 3 agents?");
    }

    #[test]
    fn an_action_with_no_target_says_why() {
        let running_only = BulkTargets::of(&[1, 3], &agents());
        assert!(BulkAction::Kill.disabled_reason(&running_only).is_none());
        assert_eq!(
            BulkAction::Restart.menu_title(&running_only),
            "Restart Exited (none of the selected agents has exited)"
        );
        let ended_only = BulkTargets::of(&[2, 4], &agents());
        assert_eq!(
            BulkAction::Kill.menu_title(&ended_only),
            "Kill Agents… (none of the selected agents is running)"
        );
        assert_eq!(
            BulkAction::Remove.menu_title(&ended_only),
            "Remove 2 Exited"
        );
    }
}
