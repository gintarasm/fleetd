//! What the graph view draws besides geometry (spec §7, ADR 0073): the
//! spawned-by tree (`fleet_client::tree`, ADR 0074) ordered by id, the project each agent
//! belongs to, the graph's filter and its subtree actions. Geometry is in
//! [`super::graph_layout`].

use std::collections::HashSet;
use std::path::Path;

use fleet_client::tree::{Tree, parent_of};
use fleet_proto::AgentInfo;

use super::ended::{format_run_time, run_time};
use super::question::open_question;
use super::session::{AgentRow, PaneId};
use super::sidebar::{ended, program_name, query_matches, tilde};
pub use super::status::DotFill;
use super::status::{AgentStatus, dot_fill};
use super::tabs::Tabs;

/// The spawned-by tree over `nodes` (`(id, parent)`), siblings and roots
/// by id. Parents resolve as everywhere else (`fleet_client::tree`): an
/// unknown parent or a loop makes a root.
pub fn tree_by_id(nodes: impl IntoIterator<Item = (u64, Option<u64>)>) -> Tree {
    let mut nodes: Vec<(u64, Option<u64>)> = nodes.into_iter().collect();
    nodes.sort_by_key(|n| n.0);
    Tree::new(nodes)
}

/// The tree of the agent list by `fleet.parent`, siblings by id.
pub fn tree_of(rows: &[AgentRow]) -> Tree {
    tree_by_id(rows.iter().map(|r| (r.info.id, parent_of(&r.info))))
}

/// `root` and everything below it, parents first, with their depth below
/// `root`.
fn subtree(tree: &Tree, root: u64) -> Vec<(u64, usize)> {
    let base = tree.ancestors(root).len();
    std::iter::once((root, 0))
        .chain(
            tree.descendants(root)
                .into_iter()
                .map(|id| (id, tree.ancestors(id).len() - base)),
        )
        .collect()
}

/// `ids` and all their ancestors.
pub fn with_ancestors(tree: &Tree, ids: impl IntoIterator<Item = u64>) -> HashSet<u64> {
    let mut out = HashSet::new();
    for id in ids {
        if out.insert(id) {
            out.extend(tree.ancestors(id));
        }
    }
    out
}

/// Stop subtree (spec §7): the running agents of `root`'s subtree, deepest
/// first (then by id), so no parent outlives the stop long enough to spawn
/// into a half-stopped subtree. Each gets its own `KillAgent`.
pub fn stop_order(tree: &Tree, root: u64, running: &dyn Fn(u64) -> bool) -> Vec<u64> {
    let mut nodes: Vec<(u64, usize)> = subtree(tree, root)
        .into_iter()
        .filter(|(id, _)| running(*id))
        .collect();
    nodes.sort_by_key(|&(id, d)| (std::cmp::Reverse(d), id));
    nodes.into_iter().map(|(id, _)| id).collect()
}

/// Remove subtree (spec §7): every id of `root`'s subtree when all of
/// them have ended, else `None` (the action is not offered).
pub fn removable(tree: &Tree, root: u64, is_ended: &dyn Fn(u64) -> bool) -> Option<Vec<u64>> {
    let ids: Vec<u64> = subtree(tree, root).into_iter().map(|(id, _)| id).collect();
    ids.iter().all(|&id| is_ended(id)).then_some(ids)
}

/// The project an agent belongs to (spec §7): its `project` label (as the
/// sidebar groups, ADR 0025), else the git root of its cwd when
/// `git_root` finds one, else its cwd; `~` for the home directory.
pub fn project_title(
    info: &AgentInfo,
    home: &Path,
    git_root: &dyn Fn(&str) -> Option<String>,
) -> String {
    if let Some(p) = info.labels.get("project").filter(|p| !p.is_empty()) {
        return p.clone();
    }
    let dir = git_root(&info.cwd).unwrap_or_else(|| info.cwd.clone());
    tilde(&dir, home)
}

/// The nearest directory at or above `dir` holding a `.git` (a directory,
/// or a worktree's file), checked with `exists`.
pub fn git_root_with(dir: &str, exists: &dyn Fn(&Path) -> bool) -> Option<String> {
    let mut at = Some(Path::new(dir));
    while let Some(d) = at {
        if d.as_os_str().is_empty() {
            break;
        }
        if exists(&d.join(".git")) {
            return Some(d.to_string_lossy().into_owned());
        }
        at = d.parent();
    }
    None
}

/// The graph's `/` filter (spec §7): each word of `query` must be in the
/// card's name, program, cwd, a label (`key=value`), its status (as the
/// card names it) or be its id. Matches keep their ancestors shown.
/// `None` while the query is blank (everything is shown).
pub fn filter_shown(
    tree: &Tree,
    rows: &[AgentRow],
    query: &str,
    home: &Path,
    name: &dyn Fn(&AgentRow) -> String,
    status: &dyn Fn(u64) -> Option<&'static str>,
) -> Option<HashSet<u64>> {
    if query.trim().is_empty() {
        return None;
    }
    let matches = rows.iter().filter(|r| {
        // The status joins the name as text the query may match.
        let text = match status(r.info.id) {
            Some(s) => format!("{} {s}", name(r)),
            None => name(r),
        };
        query_matches(query, r, &text, home)
    });
    Some(with_ancestors(tree, matches.map(|r| r.info.id)))
}

/// A card's status dot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CardDot {
    pub status: AgentStatus,
    pub fill: DotFill,
}

/// The dot for an agent with `status` (`agent_status`: its report if it
/// made one, else inferred; `None` for a shell). Solid for a reported (or
/// exit) status, hollow for an inferred one (ADR 0075).
pub fn card_dot(info: &AgentInfo, status: Option<AgentStatus>) -> Option<CardDot> {
    status.map(|status| CardDot {
        status,
        fill: dot_fill(info, status),
    })
}

/// What a card shows (spec §7), gpui-free.
#[derive(Clone, Debug, PartialEq)]
pub struct CardContent {
    /// Its name, else program (as the sidebar names it).
    pub name: String,
    pub program: String,
    /// Its project or folder.
    pub project: String,
    pub dot: Option<CardDot>,
    /// The last output line, trimmed.
    pub activity: String,
    /// How long it has run (until its exit once ended).
    pub run_time: Option<String>,
    /// Exited or lost: the card is greyed.
    pub ended: bool,
    /// Needs you: the card gets the accent highlight.
    pub waiting: bool,
    /// While collapsed: "3 children".
    pub hidden: Option<String>,
    /// The open question's text (ADR 0078): shown in place of the last
    /// line, with a Reply… button.
    pub question: Option<String>,
}

/// The card of `row`; `children` is its shown child count when collapsed.
pub fn card_content(
    row: &AgentRow,
    name: String,
    project: String,
    inferred: Option<AgentStatus>,
    collapsed_children: Option<usize>,
    now_ms: i64,
) -> CardContent {
    let is_ended = ended(row);
    let until = if is_ended {
        row.info.exited_unix_ms
    } else {
        Some(now_ms)
    };
    let dot = card_dot(&row.info, inferred);
    CardContent {
        name,
        program: program_name(&row.info.program),
        project,
        dot,
        activity: row.last_line.trim().to_owned(),
        run_time: run_time(row.info.created_unix_ms, until).map(format_run_time),
        ended: is_ended,
        waiting: dot.is_some_and(|d| d.status == AgentStatus::NeedsYou),
        hidden: collapsed_children.map(|n| match n {
            1 => "1 child".to_owned(),
            n => format!("{n} children"),
        }),
        question: open_question(&row.info).map(|q| q.text),
    }
}

/// The pane already showing `agent`, as `(tab, pane)`: the shown tab's
/// focused pane first, then the shown tab's others, then other tabs'.
/// ⏎ in the graph focuses it instead of replacing what a pane shows.
pub fn pane_showing(tabs: &Tabs, agent: u64) -> Option<(usize, PaneId)> {
    if tabs.agent(tabs.focused()) == Some(agent) {
        return Some((tabs.active(), tabs.focused()));
    }
    let order =
        std::iter::once(tabs.active()).chain((0..tabs.len()).filter(|&i| i != tabs.active()));
    for i in order {
        let layout = tabs.tab_layout(i)?;
        if let Some(p) = layout
            .panes()
            .into_iter()
            .find(|&p| layout.agent(p) == Some(agent))
        {
            return Some((i, p));
        }
    }
    None
}

/// The question before Stop subtree; `running` counts `name`'s subtree.
pub fn stop_question(name: &str, running: usize) -> String {
    match running {
        0 | 1 => format!("Stop {name}?"),
        2 => format!("Stop {name} and the agent it started?"),
        n => format!("Stop {name} and the {} agents below it?", n - 1),
    }
}

/// The question before Remove subtree of `count` ended agents; `None`
/// for one (a single Remove asks nothing, like the row menu's).
pub fn remove_question(name: &str, count: usize) -> Option<String> {
    (count > 1).then(|| format!("Remove {name} and the {} agents below it?", count - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_client::tree::PARENT_LABEL;
    use fleet_proto::AgentState;

    pub(crate) fn row(id: u64, parent: Option<u64>) -> AgentRow {
        let mut info = AgentInfo {
            id,
            program: "claude".into(),
            cwd: "/w".into(),
            ..Default::default()
        };
        info.set_state(AgentState::Running);
        if let Some(p) = parent {
            info.labels.insert(PARENT_LABEL.into(), p.to_string());
        }
        AgentRow {
            info,
            last_line: String::new(),
            last_output_unix_ms: None,
        }
    }

    #[test]
    fn tree_orders_siblings_and_roots_by_id() {
        let f = tree_by_id([
            (5, None),
            (9, Some(5)),
            (6, Some(5)),
            (2, None),
            (7, Some(6)),
        ]);
        assert_eq!(f.roots(), [2, 5]);
        assert_eq!(f.children(5), [6, 9]);
        assert_eq!(subtree(&f, 5), [(5, 0), (6, 1), (7, 2), (9, 1)]);
        assert_eq!(subtree(&f, 6), [(6, 0), (7, 1)]);
        let rows = vec![row(3, Some(1)), row(1, None), row(2, Some(1))];
        assert_eq!(tree_of(&rows).children(1), [2, 3]);
    }

    #[test]
    fn with_ancestors_keeps_the_path_to_the_root() {
        let f = tree_by_id([(1, None), (2, Some(1)), (3, Some(2)), (4, Some(1))]);
        assert_eq!(with_ancestors(&f, [3]), HashSet::from([1, 2, 3]));
    }

    #[test]
    fn stop_order_is_deepest_first_and_skips_ended() {
        let f = tree_by_id([
            (1, None),
            (2, Some(1)),
            (3, Some(2)),
            (4, Some(1)),
            (5, Some(4)),
        ]);
        let running = |id: u64| id != 4;
        assert_eq!(stop_order(&f, 1, &running), [3, 5, 2, 1]);
        assert_eq!(stop_order(&f, 4, &running), [5]);
    }

    #[test]
    fn removable_only_when_all_ended() {
        let f = tree_by_id([(1, None), (2, Some(1)), (3, Some(1))]);
        assert_eq!(removable(&f, 1, &|_| true), Some(vec![1, 2, 3]));
        assert_eq!(removable(&f, 1, &|id| id != 3), None);
        assert_eq!(removable(&f, 2, &|id| id != 3), Some(vec![2]));
    }

    #[test]
    fn project_is_label_then_git_root_then_cwd() {
        let home = Path::new("/Users/me");
        let mut r = row(1, None);
        r.info.cwd = "/Users/me/src/fleetd/crates/x".into();
        let git = |d: &str| git_root_with(d, &|p| p == Path::new("/Users/me/src/fleetd/.git"));
        assert_eq!(project_title(&r.info, home, &git), "~/src/fleetd");
        assert_eq!(
            project_title(&r.info, home, &|_| None),
            "~/src/fleetd/crates/x"
        );
        r.info.labels.insert("project".into(), "fleet".into());
        assert_eq!(project_title(&r.info, home, &git), "fleet");
    }

    #[test]
    fn git_root_stops_at_the_filesystem_root() {
        assert_eq!(git_root_with("/a/b", &|_| false), None);
        assert_eq!(
            git_root_with("/a/b", &|p| p == Path::new("/.git")),
            Some("/".into())
        );
    }

    #[test]
    fn filter_matches_name_status_label_and_keeps_ancestors() {
        let rows = vec![row(1, None), row(2, Some(1)), row(3, Some(1)), row(4, None)];
        let mut rows = rows;
        rows[2].info.labels.insert("role".into(), "reviewer".into());
        let f = tree_of(&rows);
        let home = Path::new("/h");
        let name = |r: &AgentRow| format!("agent{}", r.info.id);
        let status = |id: u64| (id == 2).then_some("Needs you");
        let shown = |q: &str| filter_shown(&f, &rows, q, home, &name, &status);
        assert_eq!(shown("  "), None);
        assert_eq!(shown("role=reviewer"), Some(HashSet::from([1, 3])));
        assert_eq!(shown("needs"), Some(HashSet::from([1, 2])));
        assert_eq!(shown("agent4"), Some(HashSet::from([4])));
        assert_eq!(shown("nothing"), Some(HashSet::new()));
    }

    #[test]
    fn card_shows_run_time_activity_and_a_hollow_inferred_dot() {
        let mut r = row(4, None);
        r.info.program = "/usr/bin/claude".into();
        r.info.created_unix_ms = 1_000;
        r.last_line = "  ? for shortcuts ".into();
        let c = card_content(
            &r,
            "rev".into(),
            "~/p".into(),
            Some(AgentStatus::NeedsYou),
            None,
            63_000,
        );
        assert_eq!(c.program, "claude");
        assert_eq!(c.activity, "? for shortcuts");
        assert_eq!(c.run_time.as_deref(), Some("1m 2s"));
        assert_eq!(
            c.dot,
            Some(CardDot {
                status: AgentStatus::NeedsYou,
                fill: DotFill::Hollow
            })
        );
        assert!(c.waiting && !c.ended && c.hidden.is_none());
        r.info.set_state(AgentState::Exited);
        r.info.exited_unix_ms = Some(4_000);
        let c = card_content(
            &r,
            "rev".into(),
            "~/p".into(),
            Some(AgentStatus::Done),
            Some(1),
            99_000,
        );
        assert_eq!(c.run_time.as_deref(), Some("3s"));
        assert!(c.ended && !c.waiting);
        assert_eq!(c.hidden.as_deref(), Some("1 child"));
        let shell = card_content(&r, "sh".into(), "~".into(), None, Some(3), 0);
        assert_eq!(shell.dot, None);
        assert_eq!(shell.hidden.as_deref(), Some("3 children"));
    }

    #[test]
    fn a_card_carries_the_open_question() {
        let mut r = row(3, None);
        let plain = card_content(&r, "a".into(), "~".into(), None, None, 0);
        assert_eq!(plain.question, None);
        r.info.reported = Some(fleet_proto::Reported {
            state: fleet_proto::ReportedState::ReportedWaiting.into(),
            message: String::new(),
            at_unix_ms: 1,
            question: Some(fleet_proto::Question {
                id: 1,
                text: "Which?".into(),
                choices: Vec::new(),
            }),
        });
        let asked = card_content(&r, "a".into(), "~".into(), None, None, 0);
        assert_eq!(asked.question.as_deref(), Some("Which?"));
    }

    #[test]
    fn questions_name_the_subtree() {
        assert_eq!(stop_question("lead", 1), "Stop lead?");
        assert_eq!(
            stop_question("lead", 2),
            "Stop lead and the agent it started?"
        );
        assert_eq!(
            stop_question("lead", 5),
            "Stop lead and the 4 agents below it?"
        );
        assert_eq!(remove_question("lead", 1), None);
        assert_eq!(
            remove_question("lead", 3).as_deref(),
            Some("Remove lead and the 2 agents below it?")
        );
    }

    #[test]
    fn pane_showing_prefers_the_focused_pane_then_the_shown_tab() {
        let mut tabs = Tabs::single();
        let first = tabs.focused();
        tabs.set_agent(first, Some(7));
        assert_eq!(pane_showing(&tabs, 7), Some((0, first)));
        assert_eq!(pane_showing(&tabs, 8), None);
        let second = tabs.new_tab().unwrap();
        tabs.set_agent(second, Some(8));
        // Tab 1 (0-based) is shown; 7 is in tab 0.
        assert_eq!(pane_showing(&tabs, 7), Some((0, first)));
        assert_eq!(pane_showing(&tabs, 8), Some((1, second)));
    }
}
