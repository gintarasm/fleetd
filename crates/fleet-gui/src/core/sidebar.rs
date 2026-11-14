//! Sidebar grouping of agents (ADR 0025: group by `project` label, else cwd).

use std::path::Path;

use fleet_proto::AgentState;

use super::session::AgentRow;

#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub title: String,
    pub agents: Vec<AgentRow>,
}

/// The agent exited or is lost: what Restart, Remove and Clear exited
/// act on.
pub fn ended(row: &AgentRow) -> bool {
    matches!(row.info.state(), AgentState::Exited | AgentState::Lost)
}

/// `cwd` with the home directory shown as `~`.
pub fn tilde(cwd: &str, home: &Path) -> String {
    match Path::new(cwd).strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => cwd.to_string(),
    }
}

/// The program's file name (`/bin/sh` shows as `sh`).
pub fn program_name(program: &str) -> String {
    Path::new(program)
        .file_name()
        .map_or_else(|| program.to_owned(), |n| n.to_string_lossy().into_owned())
}

fn title_of(row: &AgentRow, home: &Path) -> String {
    match row.info.labels.get("project") {
        Some(p) if !p.is_empty() => p.clone(),
        _ => tilde(&row.info.cwd, home),
    }
}

/// Groups sorted by title; within a group running agents first, then newest first.
pub fn group_agents(rows: &[AgentRow], home: &Path) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for row in rows {
        let title = title_of(row, home);
        match groups.iter_mut().find(|g| g.title == title) {
            Some(g) => g.agents.push(row.clone()),
            None => groups.push(Group {
                title,
                agents: vec![row.clone()],
            }),
        }
    }
    groups.sort_by(|a, b| a.title.cmp(&b.title));
    for g in &mut groups {
        g.agents.sort_by_key(|r| {
            (
                r.info.state() != AgentState::Running,
                std::cmp::Reverse(r.info.created_unix_ms),
            )
        });
    }
    groups
}

/// Compact age of the last output: `5s`, `2m`, `3h`, `4d`; empty when unknown.
pub fn since(last_output_unix_ms: Option<i64>, now_ms: i64) -> String {
    let Some(last) = last_output_unix_ms else {
        return String::new();
    };
    let s = ((now_ms - last) / 1000).max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::AgentInfo;

    fn row(id: u64, cwd: &str, project: Option<&str>, state: AgentState, created: i64) -> AgentRow {
        let mut info = AgentInfo {
            id,
            cwd: cwd.into(),
            created_unix_ms: created,
            ..Default::default()
        };
        info.set_state(state);
        if let Some(p) = project {
            info.labels.insert("project".into(), p.into());
        }
        AgentRow {
            info,
            last_line: String::new(),
            last_output_unix_ms: None,
        }
    }

    #[test]
    fn groups_by_project_label_then_cwd_with_tilde() {
        let home = Path::new("/Users/me");
        let rows = [
            row(1, "/Users/me/code/a", Some("zeta"), AgentState::Running, 1),
            row(2, "/Users/me/code/b", Some("zeta"), AgentState::Running, 2),
            row(3, "/Users/me/code/c", None, AgentState::Running, 3),
            row(4, "/Users/me", None, AgentState::Running, 4),
            row(5, "/srv/x", None, AgentState::Running, 5),
        ];
        let g = group_agents(&rows, home);
        let titles: Vec<&str> = g.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(titles, vec!["/srv/x", "zeta", "~", "~/code/c"]);
        assert_eq!(g[1].agents.len(), 2);
    }

    #[test]
    fn running_sort_before_exited() {
        let home = Path::new("/h");
        let rows = [
            row(1, "/w", None, AgentState::Exited, 100),
            row(2, "/w", None, AgentState::Running, 10),
            row(3, "/w", None, AgentState::Running, 20),
            row(4, "/w", None, AgentState::Lost, 50),
        ];
        let g = group_agents(&rows, home);
        let ids: Vec<u64> = g[0].agents.iter().map(|r| r.info.id).collect();
        assert_eq!(ids, vec![3, 2, 1, 4]);
    }

    #[test]
    fn since_formats() {
        assert_eq!(since(None, 1_000), "");
        assert_eq!(since(Some(0), 5_000), "5s");
        assert_eq!(since(Some(0), 120_000), "2m");
        assert_eq!(since(Some(0), 3 * 3_600_000), "3h");
        assert_eq!(since(Some(0), 4 * 86_400_000), "4d");
        assert_eq!(since(Some(10_000), 5_000), "0s");
    }
}
