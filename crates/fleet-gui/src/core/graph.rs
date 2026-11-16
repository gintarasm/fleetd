//! The spawned-by tree the graph view draws (spec §7, ADR 0073): who
//! spawned whom from the `fleet.parent` label, the project each agent
//! belongs to, the graph's filter and its subtree actions. Geometry is in
//! [`super::graph_layout`].

use std::collections::{HashMap, HashSet};
use std::path::Path;

use fleet_proto::AgentInfo;

use super::session::AgentRow;
use super::sidebar::{ended, query_matches, tilde};

/// The reserved label fleetd writes on an agent another agent spawned
/// (ADR 0033): the parent's id, local now and `node/id` from M6.
pub const PARENT_LABEL: &str = "fleet.parent";

/// The agent's parent from [`PARENT_LABEL`]: `"7"` or `"node/7"` (the id
/// part; every agent is on the one node until M6). `None` without the
/// label or when it does not parse.
pub fn parent_of(info: &AgentInfo) -> Option<u64> {
    let v = info.labels.get(PARENT_LABEL)?;
    let id = v.rsplit('/').next()?;
    id.trim().parse().ok()
}

/// The spawned-by forest over a set of agents. Siblings and roots are
/// ordered by id. A parent that is not in the set (removed) or that would
/// close a cycle leaves its child a root.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Forest {
    roots: Vec<u64>,
    children: HashMap<u64, Vec<u64>>,
    parent: HashMap<u64, u64>,
}

impl Forest {
    /// From `(id, parent)` pairs.
    pub fn build(nodes: impl IntoIterator<Item = (u64, Option<u64>)>) -> Forest {
        let nodes: HashMap<u64, Option<u64>> = nodes.into_iter().collect();
        let mut parent: HashMap<u64, u64> = nodes
            .iter()
            .filter_map(|(&id, &p)| {
                p.filter(|p| *p != id && nodes.contains_key(p))
                    .map(|p| (id, p))
            })
            .collect();
        let mut ids: Vec<u64> = nodes.keys().copied().collect();
        ids.sort_unstable();
        // Break cycles: the smallest id on a cycle reaches itself first and
        // becomes a root.
        for &id in &ids {
            let mut seen = HashSet::new();
            let mut at = id;
            while let Some(&p) = parent.get(&at) {
                if p == id {
                    parent.remove(&id);
                    break;
                }
                if !seen.insert(p) {
                    break;
                }
                at = p;
            }
        }
        let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
        let mut roots = Vec::new();
        for &id in &ids {
            match parent.get(&id) {
                Some(&p) => children.entry(p).or_default().push(id),
                None => roots.push(id),
            }
        }
        Forest {
            roots,
            children,
            parent,
        }
    }

    /// From the agent list.
    pub fn of_rows(rows: &[AgentRow]) -> Forest {
        Forest::build(rows.iter().map(|r| (r.info.id, parent_of(&r.info))))
    }

    pub fn roots(&self) -> &[u64] {
        &self.roots
    }

    pub fn children(&self, id: u64) -> &[u64] {
        self.children.get(&id).map_or(&[], Vec::as_slice)
    }

    pub fn parent(&self, id: u64) -> Option<u64> {
        self.parent.get(&id).copied()
    }

    pub fn contains(&self, id: u64) -> bool {
        self.parent.contains_key(&id) || self.roots.contains(&id)
    }

    /// The root of `id`'s tree.
    pub fn root_of(&self, id: u64) -> u64 {
        let mut at = id;
        while let Some(p) = self.parent(at) {
            at = p;
        }
        at
    }

    /// `id`'s distance from its root (a root is 0).
    pub fn depth(&self, id: u64) -> usize {
        let mut d = 0;
        let mut at = id;
        while let Some(p) = self.parent(at) {
            d += 1;
            at = p;
        }
        d
    }

    /// `id` and everything below it, parents before children, with their
    /// depth below `id`.
    pub fn subtree(&self, id: u64) -> Vec<(u64, usize)> {
        let mut out = Vec::new();
        let mut stack = vec![(id, 0)];
        while let Some((at, d)) = stack.pop() {
            out.push((at, d));
            for &c in self.children(at).iter().rev() {
                stack.push((c, d + 1));
            }
        }
        out
    }

    /// `ids` and all their ancestors.
    pub fn with_ancestors(&self, ids: impl IntoIterator<Item = u64>) -> HashSet<u64> {
        let mut out = HashSet::new();
        for id in ids {
            let mut at = Some(id);
            while let Some(a) = at {
                if !out.insert(a) {
                    break;
                }
                at = self.parent(a);
            }
        }
        out
    }
}

/// Stop subtree (spec §7): the running agents of `root`'s subtree, deepest
/// first (then by id), so no parent outlives the stop long enough to spawn
/// into a half-stopped subtree. Each gets its own `KillAgent`.
pub fn stop_order(forest: &Forest, root: u64, running: &dyn Fn(u64) -> bool) -> Vec<u64> {
    let mut nodes: Vec<(u64, usize)> = forest
        .subtree(root)
        .into_iter()
        .filter(|(id, _)| running(*id))
        .collect();
    nodes.sort_by_key(|&(id, d)| (std::cmp::Reverse(d), id));
    nodes.into_iter().map(|(id, _)| id).collect()
}

/// Remove subtree (spec §7): every id of `root`'s subtree when all of
/// them have ended, else `None` (the action is not offered).
pub fn removable(forest: &Forest, root: u64, is_ended: &dyn Fn(u64) -> bool) -> Option<Vec<u64>> {
    let ids: Vec<u64> = forest.subtree(root).into_iter().map(|(id, _)| id).collect();
    ids.iter().all(|&id| is_ended(id)).then_some(ids)
}

/// Running agents in each id's subtree (for "Stop subtree" and the
/// question before it).
pub fn running_in(forest: &Forest, root: u64, rows: &HashMap<u64, &AgentRow>) -> usize {
    forest
        .subtree(root)
        .iter()
        .filter(|(id, _)| rows.get(id).is_some_and(|r| !ended(r)))
        .count()
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
    forest: &Forest,
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
    Some(forest.with_ancestors(matches.map(|r| r.info.id)))
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn parent_label_parses_local_and_node_qualified_ids() {
        let mut r = row(2, Some(1));
        assert_eq!(parent_of(&r.info), Some(1));
        r.info.labels.insert(PARENT_LABEL.into(), "mac/7".into());
        assert_eq!(parent_of(&r.info), Some(7));
        r.info.labels.insert(PARENT_LABEL.into(), "x".into());
        assert_eq!(parent_of(&r.info), None);
        r.info.labels.clear();
        assert_eq!(parent_of(&r.info), None);
    }

    #[test]
    fn forest_orders_siblings_and_roots_by_id() {
        let f = Forest::build([
            (5, None),
            (9, Some(5)),
            (6, Some(5)),
            (2, None),
            (7, Some(6)),
        ]);
        assert_eq!(f.roots(), [2, 5]);
        assert_eq!(f.children(5), [6, 9]);
        assert_eq!(f.children(6), [7]);
        assert_eq!(f.parent(7), Some(6));
        assert_eq!(f.depth(7), 2);
        assert_eq!(f.root_of(7), 5);
        assert_eq!(f.subtree(5), [(5, 0), (6, 1), (7, 2), (9, 1)]);
    }

    #[test]
    fn a_missing_parent_or_a_cycle_makes_a_root() {
        // 3's parent was removed; 10 -> 11 -> 10 is a cycle (labels a
        // client could not write, but never trusted to be a tree).
        let f = Forest::build([(3, Some(1)), (10, Some(11)), (11, Some(10)), (4, Some(4))]);
        assert_eq!(f.roots(), [3, 4, 10]);
        assert_eq!(f.children(10), [11]);
        assert!(f.contains(11) && !f.contains(1));
    }

    #[test]
    fn with_ancestors_keeps_the_path_to_the_root() {
        let f = Forest::build([(1, None), (2, Some(1)), (3, Some(2)), (4, Some(1))]);
        let s = f.with_ancestors([3]);
        assert_eq!(s, HashSet::from([1, 2, 3]));
    }

    #[test]
    fn stop_order_is_deepest_first_and_skips_ended() {
        let f = Forest::build([
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
        let f = Forest::build([(1, None), (2, Some(1)), (3, Some(1))]);
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
        let f = Forest::of_rows(&rows);
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
    fn running_in_counts_live_agents_of_a_subtree() {
        let mut rows = vec![row(1, None), row(2, Some(1)), row(3, Some(2))];
        rows[1].info.set_state(AgentState::Exited);
        let f = Forest::of_rows(&rows);
        let by_id: HashMap<u64, &AgentRow> = rows.iter().map(|r| (r.info.id, r)).collect();
        assert_eq!(running_in(&f, 1, &by_id), 2);
        assert_eq!(running_in(&f, 2, &by_id), 1);
    }
}
