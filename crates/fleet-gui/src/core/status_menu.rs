//! The menu-bar status menu, as data: the shell turns it into an `NSMenu`
//! each time the menu opens.

use std::path::PathBuf;

use fleet_proto::AgentState;

use super::session::AgentRow;
use super::sidebar::{program_name, tilde};
use super::target::Target;

/// Running agents listed in the menu before "…".
pub const MAX_MENU_AGENTS: usize = 20;

/// What the menu is built from.
#[derive(Clone, Debug)]
pub struct StatusMenuState {
    pub target: Target,
    /// A fleetd is connected.
    pub live: bool,
    pub agents: Vec<AgentRow>,
    pub home: PathBuf,
}

/// One menu line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusEntry {
    /// A disabled line: "fleetd: local", "fleetd: me@box", or "…" after
    /// the last listed agent.
    Header(String),
    Show,
    Agent {
        id: u64,
        label: String,
    },
    NewAgent,
    SwitchFleetd,
    InstallSkill,
    Separator,
    Quit,
}

impl StatusEntry {
    /// The menu item's text (empty for a separator).
    pub fn title(&self) -> &str {
        match self {
            StatusEntry::Header(text) | StatusEntry::Agent { label: text, .. } => text,
            StatusEntry::Show => "Show fleetd",
            StatusEntry::NewAgent => "New agent…",
            StatusEntry::SwitchFleetd => "Switch fleetd…",
            StatusEntry::InstallSkill => "Install agent skill",
            StatusEntry::Separator => "",
            StatusEntry::Quit => "Quit fleet",
        }
    }
}

/// The header, "Show fleetd", the running agents (oldest first, at most
/// [`MAX_MENU_AGENTS`], then "…") between separators, the commands and Quit.
pub fn status_menu(state: &StatusMenuState) -> Vec<StatusEntry> {
    let header = match (&state.target, state.live) {
        (_, false) => "fleetd: not connected".to_owned(),
        (Target::Local, true) => "fleetd: local".to_owned(),
        (Target::Ssh { dest }, true) => format!("fleetd: {dest}"),
    };
    let mut menu = vec![
        StatusEntry::Header(header),
        StatusEntry::Show,
        StatusEntry::Separator,
    ];
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
                        "{} · {}",
                        program_name(&r.info.program),
                        tilde(&r.info.cwd, &state.home)
                    ),
                }),
        );
        if running.len() > MAX_MENU_AGENTS {
            menu.push(StatusEntry::Header("…".to_owned()));
        }
        menu.push(StatusEntry::Separator);
    }
    menu.extend([
        StatusEntry::NewAgent,
        StatusEntry::SwitchFleetd,
        StatusEntry::InstallSkill,
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
            agents,
            home: PathBuf::from("/Users/me"),
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
                StatusEntry::SwitchFleetd,
                StatusEntry::InstallSkill,
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
    fn unconnected_menu_says_so() {
        let mut s = state(Target::Local, Vec::new());
        s.live = false;
        assert_eq!(
            status_menu(&s)[0],
            StatusEntry::Header("fleetd: not connected".into())
        );
    }
}
