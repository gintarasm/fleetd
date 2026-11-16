//! Killing an agent from its pane header, its sidebar row or ⌘⇧W
//! (issue #27): what the agent's buttons offer, and the question asked
//! first unless `[gui] confirm_kill` is off.

/// The agent buttons of a pane header or a sidebar row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentButtons {
    /// No agent (an empty pane).
    None,
    /// A running agent: Stop, which kills it (after asking).
    Stop,
    /// An agent that exited or is lost: Restart and Remove.
    RestartRemove,
}

/// What a pane or row with an agent (`has_agent`) offers; `ended`: the
/// agent exited or is lost.
pub fn agent_buttons(has_agent: bool, ended: bool) -> AgentButtons {
    match (has_agent, ended) {
        (false, _) => AgentButtons::None,
        (true, false) => AgentButtons::Stop,
        (true, true) => AgentButtons::RestartRemove,
    }
}

/// The confirmation's question: "Kill agent 7 (claude, ~/fleetd)?". The
/// program and directory are left out when unknown (an agent the list
/// does not carry yet).
pub fn kill_question(agent_id: u64, program: Option<&str>, cwd: Option<&str>) -> String {
    let parts: Vec<&str> = [program, cwd]
        .into_iter()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .collect();
    if parts.is_empty() {
        format!("Kill agent {agent_id}?")
    } else {
        format!("Kill agent {agent_id} ({})?", parts.join(", "))
    }
}

/// What a kill request does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillFlow {
    /// Show the question; only its Kill kills.
    Ask,
    /// Kill at once ("Don't ask again" was checked).
    Kill,
}

/// The flow for `[gui] confirm_kill`.
pub fn kill_flow(confirm: bool) -> KillFlow {
    if confirm {
        KillFlow::Ask
    } else {
        KillFlow::Kill
    }
}
