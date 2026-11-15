//! Sidebar grouping of agents (ADR 0025: group by `project` label, else
//! cwd), and its filter and collapsed groups (issue #40, ADR 0060).

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use fleet_proto::AgentState;

use super::session::AgentRow;
use super::status::AgentStatus;

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

/// The sidebar's status filter (issue #40), over ADR 0042's statuses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum StatusFilter {
    #[default]
    All,
    /// Agents whose status is needs you (never a shell).
    NeedsYou,
    /// Running agents and shells, whatever their status.
    Running,
    /// Exited or lost: done and failed.
    Exited,
}

impl StatusFilter {
    pub const ALL: [StatusFilter; 4] = [
        StatusFilter::All,
        StatusFilter::NeedsYou,
        StatusFilter::Running,
        StatusFilter::Exited,
    ];

    pub fn label(self) -> &'static str {
        match self {
            StatusFilter::All => "All",
            StatusFilter::NeedsYou => "Needs you",
            StatusFilter::Running => "Running",
            StatusFilter::Exited => "Exited",
        }
    }

    /// Whether `row`, with `status` (`None` for a shell), passes.
    pub fn matches(self, row: &AgentRow, status: Option<AgentStatus>) -> bool {
        match self {
            StatusFilter::All => true,
            StatusFilter::NeedsYou => status == Some(AgentStatus::NeedsYou),
            StatusFilter::Running => row.info.state() == AgentState::Running,
            StatusFilter::Exited => ended(row),
        }
    }
}

/// What the sidebar shows: typed text and a status.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    pub query: String,
    pub status: StatusFilter,
}

impl Filter {
    /// Anything is filtered out.
    pub fn is_active(&self) -> bool {
        self.has_query() || self.status != StatusFilter::All
    }

    fn has_query(&self) -> bool {
        !self.query.trim().is_empty()
    }
}

/// Whether every word of `query` (ignoring case) is in the row's `name`
/// (as the row shows it), its program, its cwd (as stored or with `~`),
/// or a label (`key=value`). An empty query matches everything.
pub fn query_matches(query: &str, row: &AgentRow, name: &str, home: &Path) -> bool {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return true;
    }
    let mut hay = vec![
        name.to_owned(),
        program_name(&row.info.program),
        row.info.cwd.clone(),
        tilde(&row.info.cwd, home),
    ];
    hay.extend(row.info.labels.iter().map(|(k, v)| format!("{k}={v}")));
    let hay: Vec<String> = hay.iter().map(|h| h.to_lowercase()).collect();
    words
        .iter()
        .all(|w| hay.iter().any(|h| h.contains(w.as_str())))
}

/// A sidebar section: its agents that pass the filter, or none drawn
/// while it is collapsed.
#[derive(Clone, Debug, PartialEq)]
pub struct SidebarGroup {
    pub title: String,
    /// Drawn collapsed: `agents` is empty and the header shows `count`.
    pub collapsed: bool,
    /// How many of its agents pass the filter.
    pub count: usize,
    pub agents: Vec<AgentRow>,
}

/// [`group_agents`] narrowed by `filter`: groups with no agent passing it
/// are left out. Groups titled in `collapsed` draw collapsed, except while
/// text is typed (to show what matched; `collapsed` is not changed).
/// `name` is the name the row shows.
pub fn sidebar_groups(
    rows: &[AgentRow],
    home: &Path,
    filter: &Filter,
    name: &dyn Fn(&AgentRow) -> String,
    statuses: &HashMap<u64, AgentStatus>,
    collapsed: &BTreeSet<String>,
) -> Vec<SidebarGroup> {
    group_agents(rows, home)
        .into_iter()
        .filter_map(|g| {
            let agents: Vec<AgentRow> = g
                .agents
                .into_iter()
                .filter(|r| {
                    filter.status.matches(r, statuses.get(&r.info.id).copied())
                        && query_matches(&filter.query, r, &name(r), home)
                })
                .collect();
            if agents.is_empty() && filter.is_active() {
                return None;
            }
            let is_collapsed = collapsed.contains(&g.title) && !filter.has_query();
            Some(SidebarGroup {
                count: agents.len(),
                agents: if is_collapsed { Vec::new() } else { agents },
                collapsed: is_collapsed,
                title: g.title,
            })
        })
        .collect()
}

/// Collapses `title`, or expands it if collapsed.
pub fn toggle_collapsed(collapsed: &mut BTreeSet<String>, title: &str) {
    if !collapsed.remove(title) {
        collapsed.insert(title.to_owned());
    }
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
    use std::collections::HashMap;

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

    fn labelled(mut r: AgentRow, k: &str, v: &str) -> AgentRow {
        r.info.labels.insert(k.into(), v.into());
        r
    }

    fn name_of(r: &AgentRow) -> String {
        format!("agent-{}", r.info.id)
    }

    #[test]
    fn query_matches_name_cwd_and_labels_ignoring_case() {
        let home = Path::new("/Users/me");
        let mut r = row(7, "/Users/me/code/Fleetd", None, AgentState::Running, 1);
        r.info.program = "/usr/local/bin/claude".into();
        let r = labelled(r, "role", "Reviewer");
        let m = |q: &str| query_matches(q, &r, "Fix the Bell", home);
        assert!(m(""));
        assert!(m("   "));
        // The row's name, and the program it runs.
        assert!(m("bell"));
        assert!(m("CLAUDE"));
        // The cwd, as stored and as shown with `~`.
        assert!(m("code/fleetd"));
        assert!(m("~/code"));
        // Label keys, values and `key=value`.
        assert!(m("reviewer"));
        assert!(m("role=rev"));
        // Every word must match, somewhere.
        assert!(m("bell reviewer"));
        assert!(!m("bell coder"));
        assert!(!m("zzz"));
    }

    #[test]
    fn status_filters_use_the_agent_status_and_state() {
        let running = row(1, "/w", None, AgentState::Running, 1);
        let exited = row(2, "/w", None, AgentState::Exited, 1);
        let lost = row(3, "/w", None, AgentState::Lost, 1);
        use AgentStatus::*;
        let f = |s: StatusFilter, r: &AgentRow, st: Option<AgentStatus>| s.matches(r, st);
        assert!(f(StatusFilter::All, &running, Some(Working)));
        assert!(f(StatusFilter::All, &exited, None));
        assert!(f(StatusFilter::NeedsYou, &running, Some(NeedsYou)));
        assert!(!f(StatusFilter::NeedsYou, &running, Some(Working)));
        assert!(!f(StatusFilter::NeedsYou, &running, Some(Idle)));
        // A shell has no status: never needs you, but runs and exits.
        assert!(!f(StatusFilter::NeedsYou, &running, None));
        assert!(f(StatusFilter::Running, &running, Some(NeedsYou)));
        assert!(f(StatusFilter::Running, &running, Some(Idle)));
        assert!(f(StatusFilter::Running, &running, None));
        assert!(!f(StatusFilter::Running, &exited, Some(Done)));
        assert!(f(StatusFilter::Exited, &exited, Some(Done)));
        assert!(f(StatusFilter::Exited, &lost, Some(Failed)));
        assert!(f(StatusFilter::Exited, &exited, None));
        assert!(!f(StatusFilter::Exited, &running, Some(Working)));
        assert_eq!(
            StatusFilter::ALL.map(StatusFilter::label),
            ["All", "Needs you", "Running", "Exited"]
        );
    }

    #[test]
    fn filtered_groups_hide_misses_and_empty_groups() {
        let home = Path::new("/h");
        let rows = [
            labelled(
                row(1, "/a", Some("alpha"), AgentState::Running, 1),
                "role",
                "coder",
            ),
            row(2, "/a", Some("alpha"), AgentState::Exited, 2),
            row(3, "/b", Some("beta"), AgentState::Running, 3),
        ];
        let statuses = HashMap::from([
            (1, AgentStatus::NeedsYou),
            (2, AgentStatus::Done),
            (3, AgentStatus::Working),
        ]);
        let none = BTreeSet::new();
        let ids = |g: &[SidebarGroup]| -> Vec<(String, Vec<u64>)> {
            g.iter()
                .map(|g| {
                    (
                        g.title.clone(),
                        g.agents.iter().map(|r| r.info.id).collect(),
                    )
                })
                .collect()
        };
        let all = sidebar_groups(&rows, home, &Filter::default(), &name_of, &statuses, &none);
        assert_eq!(
            ids(&all),
            [("alpha".into(), vec![1, 2]), ("beta".into(), vec![3])]
        );
        assert!(all.iter().all(|g| !g.collapsed));

        let coder = Filter {
            query: "coder".into(),
            status: StatusFilter::All,
        };
        let g = sidebar_groups(&rows, home, &coder, &name_of, &statuses, &none);
        assert_eq!(ids(&g), [("alpha".into(), vec![1])]);

        let exited = Filter {
            query: String::new(),
            status: StatusFilter::Exited,
        };
        let g = sidebar_groups(&rows, home, &exited, &name_of, &statuses, &none);
        assert_eq!(ids(&g), [("alpha".into(), vec![2])]);

        // The name matches too.
        let named = Filter {
            query: "agent-3".into(),
            status: StatusFilter::Running,
        };
        let g = sidebar_groups(&rows, home, &named, &name_of, &statuses, &none);
        assert_eq!(ids(&g), [("beta".into(), vec![3])]);
        assert!(!Filter::default().is_active());
        assert!(named.is_active());
    }

    #[test]
    fn a_collapsed_group_keeps_its_header_and_count() {
        let home = Path::new("/h");
        let rows = [
            row(1, "/a", Some("alpha"), AgentState::Running, 1),
            row(2, "/a", Some("alpha"), AgentState::Exited, 2),
            row(3, "/b", Some("beta"), AgentState::Running, 3),
        ];
        let statuses = HashMap::new();
        let collapsed = BTreeSet::from(["alpha".to_owned()]);
        let g = sidebar_groups(
            &rows,
            home,
            &Filter::default(),
            &name_of,
            &statuses,
            &collapsed,
        );
        assert_eq!(g[0].title, "alpha");
        assert!(g[0].collapsed);
        assert!(g[0].agents.is_empty());
        assert_eq!(g[0].count, 2);
        assert!(!g[1].collapsed);
        assert_eq!((g[1].agents.len(), g[1].count), (1, 1));

        // A status filter keeps it collapsed and counts what matches.
        let running = Filter {
            query: String::new(),
            status: StatusFilter::Running,
        };
        let g = sidebar_groups(&rows, home, &running, &name_of, &statuses, &collapsed);
        assert!(g[0].collapsed);
        assert_eq!(g[0].count, 1);

        // Typed text opens it, to show what matched; the remembered
        // collapse is untouched.
        let typed = Filter {
            query: "/a".into(),
            status: StatusFilter::All,
        };
        let g = sidebar_groups(&rows, home, &typed, &name_of, &statuses, &collapsed);
        assert_eq!(g.len(), 1);
        assert!(!g[0].collapsed);
        assert_eq!(g[0].agents.len(), 2);
    }

    #[test]
    fn toggling_a_group_adds_then_removes_it() {
        let mut set = BTreeSet::new();
        toggle_collapsed(&mut set, "alpha");
        assert!(set.contains("alpha"));
        toggle_collapsed(&mut set, "alpha");
        assert!(set.is_empty());
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
