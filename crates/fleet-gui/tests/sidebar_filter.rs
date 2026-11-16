//! Sidebar filter and collapsible groups (issue #40, ADR 0060), through
//! the core the shell draws from.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use fleet_gui::core::session::AgentRow;
use fleet_gui::core::sidebar::{Filter, StatusFilter, sidebar_groups, toggle_collapsed};
use fleet_gui::core::status::{AgentStatus, statuses};
use fleet_proto::{AgentInfo, AgentState};

fn agent(id: u64, cwd: &str, project: &str, state: AgentState) -> AgentRow {
    let mut info = AgentInfo {
        id,
        program: "claude".into(),
        cwd: cwd.into(),
        created_unix_ms: i64::try_from(id).unwrap(),
        ..Default::default()
    };
    info.set_state(state);
    if state == AgentState::Exited {
        info.exit_code = Some(0);
    }
    info.labels.insert("project".into(), project.into());
    AgentRow {
        info,
        last_line: String::new(),
        last_output_unix_ms: None,
    }
}

fn shown(groups: &[fleet_gui::core::sidebar::SidebarGroup]) -> Vec<(&str, bool, usize, Vec<u64>)> {
    groups
        .iter()
        .map(|g| {
            (
                g.title.as_str(),
                g.collapsed,
                g.count,
                g.agents.iter().map(|r| r.info.id).collect(),
            )
        })
        .collect()
}

#[test]
fn filter_status_and_collapse_together() {
    let home = Path::new("/Users/me");
    let mut waiting = agent(3, "/Users/me/web", "web", AgentState::Running);
    waiting.last_line = "Do you want to proceed? (y/n)".into();
    waiting.last_output_unix_ms = Some(0);
    let rows = vec![
        agent(1, "/Users/me/fleetd", "fleetd", AgentState::Running),
        agent(2, "/Users/me/fleetd", "fleetd", AgentState::Exited),
        waiting,
    ];
    let st = statuses(&rows, &Default::default(), 60_000);
    assert_eq!(st[&3], AgentStatus::NeedsYou);
    let name = |r: &AgentRow| format!("tab {}", r.info.id);
    let mut collapsed = BTreeSet::new();

    let all = sidebar_groups(&rows, home, &Filter::default(), &name, &st, &collapsed);
    assert_eq!(
        shown(&all),
        [("fleetd", false, 2, vec![1, 2]), ("web", false, 1, vec![3])]
    );

    let needs = Filter {
        query: String::new(),
        status: StatusFilter::NeedsYou,
    };
    let g = sidebar_groups(&rows, home, &needs, &name, &st, &collapsed);
    assert_eq!(shown(&g), [("web", false, 1, vec![3])]);

    toggle_collapsed(&mut collapsed, "fleetd");
    let g = sidebar_groups(&rows, home, &Filter::default(), &name, &st, &collapsed);
    assert_eq!(
        shown(&g),
        [("fleetd", true, 2, vec![]), ("web", false, 1, vec![3])]
    );

    // Typed text opens a collapsed group to show its match.
    let typed = Filter {
        query: "~/fleetd tab 2".into(),
        status: StatusFilter::All,
    };
    let g = sidebar_groups(&rows, home, &typed, &name, &st, &collapsed);
    assert_eq!(shown(&g), [("fleetd", false, 1, vec![2])]);

    let none: HashMap<u64, AgentStatus> = HashMap::new();
    let g = sidebar_groups(&rows, home, &needs, &name, &none, &collapsed);
    assert!(g.is_empty());
}
