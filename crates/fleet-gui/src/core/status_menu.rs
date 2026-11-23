//! The menu-bar status menu, as data: the shell turns it into an `NSMenu`
//! each time the menu opens.

use std::collections::HashSet;
use std::path::PathBuf;

use fleet_proto::AgentState;

use super::session::AgentRow;
use super::sidebar::{ended, program_name, tilde};
use super::target::Target;

/// Running agents listed in the menu before "…".
pub const MAX_MENU_AGENTS: usize = 20;

/// What the menu is built from.
#[derive(Clone, Debug)]
pub struct StatusMenuState {
    pub target: Target,
    /// A fleetd is connected.
    pub live: bool,
    /// The control connection is down and being retried.
    pub reconnecting: bool,
    pub agents: Vec<AgentRow>,
    pub home: PathBuf,
    /// Agents that need you (ADR 0042): their lines say so.
    pub needs_you: HashSet<u64>,
    /// Notifications are on (`[gui] notifications`).
    pub notifications: bool,
}

/// One menu line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusEntry {
    /// A disabled line: "fleetd: local", "fleetd: me@box" (either with
    /// " (reconnecting)" while the connection is down), or "…" after the
    /// last listed agent.
    Header(String),
    Show,
    /// Shows the agent that has waited longest for you (ADR 0100); listed
    /// only while some agent needs you.
    NextNeedsYou,
    Agent {
        id: u64,
        label: String,
    },
    NewAgent,
    /// A plain login shell (`kind=shell`), ⌘T in the window.
    NewTerminal,
    /// Forgets every exited or lost agent; listed only when one exists.
    ClearExited,
    SwitchFleetd,
    /// Restarts the connected fleetd into the binary on disk; agents keep
    /// running. Listed for local and ssh targets alike.
    RestartFleetd,
    InstallSkill,
    /// Toggles macOS notifications (checked while on).
    Notifications(bool),
    Separator,
    Quit,
}

impl StatusEntry {
    /// The menu item's text (empty for a separator).
    pub fn title(&self) -> &str {
        match self {
            StatusEntry::Header(text) | StatusEntry::Agent { label: text, .. } => text,
            StatusEntry::Show => "Show fleetd",
            StatusEntry::NextNeedsYou => "Next agent that needs you",
            StatusEntry::NewAgent => "New agent…",
            StatusEntry::NewTerminal => "New terminal",
            StatusEntry::ClearExited => "Clear exited agents",
            StatusEntry::SwitchFleetd => "Switch fleetd…",
            StatusEntry::RestartFleetd => "Restart fleetd",
            StatusEntry::InstallSkill => "Install agent skill",
            StatusEntry::Notifications(_) => "Notify when agents need you",
            StatusEntry::Separator => "",
            StatusEntry::Quit => "Quit fleet",
        }
    }
}

/// The header, "Show fleetd", the running agents (oldest first, at most
/// [`MAX_MENU_AGENTS`], then "…") between separators, the commands (Clear
/// exited only if an agent [`ended`]) and Quit.
pub fn status_menu(state: &StatusMenuState) -> Vec<StatusEntry> {
    let mut header = match (&state.target, state.live) {
        (_, false) => "fleetd: not connected".to_owned(),
        (Target::Local, true) => "fleetd: local".to_owned(),
        (Target::Ssh { dest }, true) => format!("fleetd: {dest}"),
    };
    if state.live && state.reconnecting {
        header.push_str(" (reconnecting)");
    }
    let mut menu = vec![StatusEntry::Header(header), StatusEntry::Show];
    if !state.needs_you.is_empty() {
        menu.push(StatusEntry::NextNeedsYou);
    }
    menu.push(StatusEntry::Separator);
    let mut running: Vec<&AgentRow> = state
        .agents
        .iter()
        .filter(|r| r.info.state() == AgentState::Running)
        .collect();
    running.sort_by_key(|r| (r.info.created_unix_ms, r.info.id));
    if !running.is_empty() {
        menu.extend(
            running
                .iter()
                .take(MAX_MENU_AGENTS)
                .map(|r| StatusEntry::Agent {
                    id: r.info.id,
                    label: format!(
                        "{} · {}{}",
                        program_name(&r.info.program),
                        tilde(&r.info.cwd, &state.home),
                        if state.needs_you.contains(&r.info.id) {
                            " — needs you"
                        } else {
                            ""
                        }
                    ),
                }),
        );
        if running.len() > MAX_MENU_AGENTS {
            menu.push(StatusEntry::Header("…".to_owned()));
        }
        menu.push(StatusEntry::Separator);
    }
    menu.extend([StatusEntry::NewAgent, StatusEntry::NewTerminal]);
    if state.agents.iter().any(ended) {
        menu.push(StatusEntry::ClearExited);
    }
    menu.extend([
        StatusEntry::SwitchFleetd,
        StatusEntry::RestartFleetd,
        StatusEntry::InstallSkill,
        StatusEntry::Notifications(state.notifications),
        StatusEntry::Separator,
        StatusEntry::Quit,
    ]);
    menu
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::AgentInfo;

    fn row(id: u64, program: &str, cwd: &str, state: AgentState) -> AgentRow {
        let mut info = AgentInfo {
            id,
            program: program.into(),
            cwd: cwd.into(),
            created_unix_ms: id as i64,
            ..Default::default()
        };
        info.set_state(state);
        AgentRow {
            info,
            last_line: String::new(),
            last_output_unix_ms: None,
        }
    }

    fn state(target: Target, agents: Vec<AgentRow>) -> StatusMenuState {
        StatusMenuState {
            target,
            live: true,
            reconnecting: false,
            agents,
            home: PathBuf::from("/Users/me"),
            needs_you: HashSet::new(),
            notifications: false,
        }
    }

    fn agents(menu: &[StatusEntry]) -> Vec<StatusEntry> {
        let first = menu
            .iter()
            .position(|e| *e == StatusEntry::Separator)
            .unwrap();
        let rest = &menu[first + 1..];
        let end = rest
            .iter()
            .position(|e| *e == StatusEntry::Separator)
            .unwrap();
        rest[..end].to_vec()
    }

    #[test]
    fn status_menu_lists_running_agents_between_separators() {
        let menu = status_menu(&state(
            Target::Local,
            vec![
                row(
                    1,
                    "/usr/local/bin/claude",
                    "/Users/me/src/a",
                    AgentState::Running,
                ),
                row(2, "sh", "/Users/me/src/b", AgentState::Exited),
                row(3, "sh", "/tmp", AgentState::Running),
                row(4, "codex", "/Users/me", AgentState::Lost),
            ],
        ));
        assert_eq!(
            menu,
            vec![
                StatusEntry::Header("fleetd: local".into()),
                StatusEntry::Show,
                StatusEntry::Separator,
                StatusEntry::Agent {
                    id: 1,
                    label: "claude · ~/src/a".into()
                },
                StatusEntry::Agent {
                    id: 3,
                    label: "sh · /tmp".into()
                },
                StatusEntry::Separator,
                StatusEntry::NewAgent,
                StatusEntry::NewTerminal,
                StatusEntry::ClearExited,
                StatusEntry::SwitchFleetd,
                StatusEntry::RestartFleetd,
                StatusEntry::InstallSkill,
                StatusEntry::Notifications(false),
                StatusEntry::Separator,
                StatusEntry::Quit,
            ]
        );

        // At most 20, then "…".
        let many = (1..=25)
            .map(|id| row(id, "sh", "/Users/me/w", AgentState::Running))
            .collect();
        let menu = status_menu(&state(
            Target::Ssh {
                dest: "me@box".into(),
            },
            many,
        ));
        assert_eq!(menu[0], StatusEntry::Header("fleetd: me@box".into()));
        let listed = agents(&menu);
        assert_eq!(listed.len(), MAX_MENU_AGENTS + 1);
        assert!(
            listed[..MAX_MENU_AGENTS]
                .iter()
                .all(|e| matches!(e, StatusEntry::Agent { label, .. } if label == "sh · ~/w"))
        );
        assert_eq!(listed[MAX_MENU_AGENTS], StatusEntry::Header("…".into()));
    }

    #[test]
    fn no_running_agents_leaves_one_separator_after_show() {
        let menu = status_menu(&state(
            Target::Local,
            vec![row(2, "sh", "/tmp", AgentState::Exited)],
        ));
        assert_eq!(
            &menu[..4],
            &[
                StatusEntry::Header("fleetd: local".into()),
                StatusEntry::Show,
                StatusEntry::Separator,
                StatusEntry::NewAgent,
            ]
        );
    }

    #[test]
    fn clear_exited_is_offered_only_when_an_agent_ended() {
        let running = vec![row(1, "sh", "/tmp", AgentState::Running)];
        assert!(
            !status_menu(&state(Target::Local, running.clone()))
                .contains(&StatusEntry::ClearExited)
        );
        for ended in [AgentState::Exited, AgentState::Lost] {
            let mut rows = running.clone();
            rows.push(row(2, "sh", "/tmp", ended));
            assert!(status_menu(&state(Target::Local, rows)).contains(&StatusEntry::ClearExited));
        }
        assert_eq!(StatusEntry::ClearExited.title(), "Clear exited agents");
    }

    #[test]
    fn status_menu_offers_restart_for_local_and_ssh() {
        for target in [
            Target::Local,
            Target::Ssh {
                dest: "me@box".into(),
            },
        ] {
            let menu = status_menu(&state(target, Vec::new()));
            let switch = menu
                .iter()
                .position(|e| *e == StatusEntry::SwitchFleetd)
                .unwrap();
            assert_eq!(menu[switch + 1], StatusEntry::RestartFleetd);
        }
        assert_eq!(StatusEntry::RestartFleetd.title(), "Restart fleetd");
    }

    #[test]
    fn unconnected_menu_says_so() {
        let mut s = state(Target::Local, Vec::new());
        s.live = false;
        assert_eq!(
            status_menu(&s)[0],
            StatusEntry::Header("fleetd: not connected".into())
        );
    }

    #[test]
    fn menu_says_reconnecting_while_the_control_connection_is_down() {
        let mut s = state(Target::Local, Vec::new());
        s.reconnecting = true;
        assert_eq!(
            status_menu(&s)[0],
            StatusEntry::Header("fleetd: local (reconnecting)".into())
        );
        s.target = Target::Ssh {
            dest: "me@box".into(),
        };
        assert_eq!(
            status_menu(&s)[0],
            StatusEntry::Header("fleetd: me@box (reconnecting)".into())
        );
    }

    #[test]
    fn agents_that_need_you_say_so_and_notifications_toggle() {
        let mut s = state(
            Target::Local,
            vec![
                row(1, "claude", "/tmp", AgentState::Running),
                row(2, "claude", "/tmp", AgentState::Running),
            ],
        );
        s.needs_you.insert(2);
        s.notifications = true;
        let menu = status_menu(&s);
        assert_eq!(
            agents(&menu),
            vec![
                StatusEntry::Agent {
                    id: 1,
                    label: "claude · /tmp".into()
                },
                StatusEntry::Agent {
                    id: 2,
                    label: "claude · /tmp — needs you".into()
                },
            ]
        );
        assert!(menu.contains(&StatusEntry::Notifications(true)));
        assert_eq!(
            StatusEntry::Notifications(true).title(),
            "Notify when agents need you"
        );
    }

    #[test]
    fn next_needs_you_is_listed_only_while_an_agent_needs_you() {
        let mut s = state(
            Target::Local,
            vec![row(1, "claude", "/tmp", AgentState::Running)],
        );
        assert!(!status_menu(&s).contains(&StatusEntry::NextNeedsYou));
        s.needs_you.insert(1);
        let menu = status_menu(&s);
        let at = menu
            .iter()
            .position(|e| *e == StatusEntry::NextNeedsYou)
            .unwrap();
        assert_eq!(menu[at - 1], StatusEntry::Show);
        assert_eq!(
            StatusEntry::NextNeedsYou.title(),
            "Next agent that needs you"
        );
    }
}
