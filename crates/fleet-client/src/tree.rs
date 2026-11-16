//! The spawned-by tree (ADR 0074): agents arranged by the reserved
//! `fleet.parent` label (ADR 0025, spec §1). Shared by the CLI's
//! `fleet tree` and the GUI's sidebar, and free of any UI.
//!
//! The label is the only source. A parent that is not in the list (removed,
//! unknown, out of the caller's scope) makes the agent a root. A parent
//! chain that loops back on itself cannot come from the node, but must not
//! hang a client: every agent on the loop is a root.

use std::collections::{HashMap, HashSet};

use fleet_proto::AgentInfo;

/// The label that names an agent's parent (a local id until M6).
pub const PARENT_LABEL: &str = "fleet.parent";

/// The parent the agent's label names, if it holds a local id.
pub fn parent_of(info: &AgentInfo) -> Option<u64> {
    info.labels.get(PARENT_LABEL)?.trim().parse().ok()
}

/// A forest over agent ids. Siblings keep the order the agents were given
/// in, so a caller sorts once and every level follows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    roots: Vec<u64>,
    children: HashMap<u64, Vec<u64>>,
    parent: HashMap<u64, u64>,
}

/// One line of a pre-order walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Visit {
    pub id: u64,
    /// Roots are 0.
    pub depth: usize,
    /// For each ancestor level below the root, then this node's own level:
    /// whether that node is the last of its siblings. What box-drawing
    /// needs: `│  ` for an ancestor that is not last, blanks otherwise, then
    /// `└─` or `├─`.
    pub last: Vec<bool>,
}

impl Tree {
    /// The forest of `(id, parent)` pairs, in the order given. A repeated id
    /// counts once.
    pub fn new(agents: impl IntoIterator<Item = (u64, Option<u64>)>) -> Self {
        let mut order = Vec::new();
        let mut wanted: HashMap<u64, Option<u64>> = HashMap::new();
        for (id, parent) in agents {
            if wanted.insert(id, parent).is_none() {
                order.push(id);
            }
        }
        // Only a parent we know of, and never oneself.
        let mut parent: HashMap<u64, u64> = wanted
            .iter()
            .filter_map(|(&id, p)| {
                let p = (*p)?;
                (p != id && wanted.contains_key(&p)).then_some((id, p))
            })
            .collect();
        // Break loops: each agent on one becomes a root.
        let mut on_loop = HashSet::new();
        for &start in &order {
            let mut seen = HashSet::new();
            let mut at = start;
            while let Some(&p) = parent.get(&at) {
                if !seen.insert(at) {
                    break;
                }
                at = p;
            }
            if seen.contains(&at) && parent.contains_key(&at) {
                // `at` is on a loop: walk it once.
                let mut n = at;
                loop {
                    on_loop.insert(n);
                    n = parent[&n];
                    if n == at {
                        break;
                    }
                }
            }
        }
        parent.retain(|id, _| !on_loop.contains(id));
        let mut roots = Vec::new();
        let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
        for &id in &order {
            match parent.get(&id) {
                Some(&p) => children.entry(p).or_default().push(id),
                None => roots.push(id),
            }
        }
        Tree {
            roots,
            children,
            parent,
        }
    }

    /// The tree of `agents`, by their `fleet.parent` labels.
    pub fn of<'a>(agents: impl IntoIterator<Item = &'a AgentInfo>) -> Self {
        Self::new(agents.into_iter().map(|a| (a.id, parent_of(a))))
    }

    pub fn roots(&self) -> &[u64] {
        &self.roots
    }

    /// The agent's children; none for a leaf or an unknown id.
    pub fn children(&self, id: u64) -> &[u64] {
        self.children.get(&id).map_or(&[], Vec::as_slice)
    }

    /// The agent's parent in this tree (`None` for a root).
    pub fn parent(&self, id: u64) -> Option<u64> {
        self.parent.get(&id).copied()
    }

    /// The agent's ancestors, nearest first.
    pub fn ancestors(&self, id: u64) -> Vec<u64> {
        let mut out = Vec::new();
        let mut at = id;
        while let Some(p) = self.parent(at) {
            out.push(p);
            at = p;
        }
        out
    }

    /// The root of the agent's tree (itself for a root or an unknown id).
    pub fn root_of(&self, id: u64) -> u64 {
        self.ancestors(id).last().copied().unwrap_or(id)
    }

    /// Everything below the agent, in pre-order.
    pub fn descendants(&self, id: u64) -> Vec<u64> {
        let mut out = Vec::new();
        let mut stack: Vec<u64> = self.children(id).iter().rev().copied().collect();
        while let Some(n) = stack.pop() {
            out.push(n);
            stack.extend(self.children(n).iter().rev());
        }
        out
    }

    /// Pre-order, roots first. `descend` says whether to show a node's
    /// children (a collapsed branch, or one the caller left out).
    pub fn walk(&self, descend: impl Fn(u64) -> bool) -> Vec<Visit> {
        let mut out = Vec::new();
        let mut stack: Vec<(u64, Vec<bool>)> = {
            let n = self.roots.len();
            self.roots
                .iter()
                .enumerate()
                .rev()
                .map(|(i, &id)| (id, vec![i + 1 == n]))
                .collect()
        };
        while let Some((id, last)) = stack.pop() {
            out.push(Visit {
                id,
                depth: last.len() - 1,
                last: last.clone(),
            });
            if descend(id) {
                let kids = self.children(id);
                for (i, &k) in kids.iter().enumerate().rev() {
                    let mut l = last.clone();
                    l.push(i + 1 == kids.len());
                    stack.push((k, l));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(pairs: &[(u64, Option<u64>)]) -> Tree {
        Tree::new(pairs.iter().copied())
    }

    #[test]
    fn nests_children_under_parents_in_input_order() {
        let tree = t(&[
            (1, None),
            (2, Some(1)),
            (3, Some(1)),
            (4, Some(2)),
            (5, None),
        ]);
        assert_eq!(tree.roots(), [1, 5]);
        assert_eq!(tree.children(1), [2, 3]);
        assert_eq!(tree.children(2), [4]);
        assert_eq!(tree.parent(4), Some(2));
        assert_eq!(tree.parent(1), None);
        assert_eq!(tree.descendants(1), [2, 4, 3]);
        assert_eq!(tree.ancestors(4), [2, 1]);
    }

    #[test]
    fn root_of_follows_the_parents_up() {
        let tree = t(&[(1, None), (2, Some(1)), (3, Some(2)), (9, Some(8))]);
        assert_eq!(tree.root_of(3), 1);
        assert_eq!(tree.root_of(1), 1);
        // A missing parent: the agent is its own root; so is an unknown id.
        assert_eq!(tree.root_of(9), 9);
        assert_eq!(tree.root_of(42), 42);
    }

    #[test]
    fn a_missing_parent_makes_a_root() {
        let tree = t(&[(2, Some(1)), (3, Some(2))]);
        assert_eq!(tree.roots(), [2]);
        assert_eq!(tree.children(2), [3]);
    }

    #[test]
    fn a_parent_can_come_after_its_child() {
        let tree = t(&[(2, Some(1)), (1, None)]);
        assert_eq!(tree.roots(), [1]);
        assert_eq!(tree.children(1), [2]);
    }

    #[test]
    fn loops_and_self_parents_are_roots_and_do_not_hang() {
        let tree = t(&[(1, Some(1)), (2, Some(3)), (3, Some(2)), (4, Some(3))]);
        assert_eq!(tree.roots(), [1, 2, 3]);
        // What hangs off a loop still hangs off it.
        assert_eq!(tree.children(3), [4]);
        assert_eq!(tree.walk(|_| true).len(), 4);
        let long = t(&[(1, Some(3)), (2, Some(1)), (3, Some(2))]);
        assert_eq!(long.roots(), [1, 2, 3]);
        assert!(long.ancestors(3).is_empty());
    }

    #[test]
    fn parent_label_is_read_from_the_agent() {
        let mut a = AgentInfo {
            id: 9,
            ..Default::default()
        };
        assert_eq!(parent_of(&a), None);
        a.labels.insert(PARENT_LABEL.into(), "4".into());
        assert_eq!(parent_of(&a), Some(4));
        // Not a local id (M6's `node/id`) or junk: no parent.
        a.labels.insert(PARENT_LABEL.into(), "other/4".into());
        assert_eq!(parent_of(&a), None);
        a.labels.insert(PARENT_LABEL.into(), "".into());
        assert_eq!(parent_of(&a), None);
    }

    #[test]
    fn of_uses_the_labels() {
        let mk = |id, parent: Option<&str>| {
            let mut a = AgentInfo {
                id,
                ..Default::default()
            };
            if let Some(p) = parent {
                a.labels.insert(PARENT_LABEL.into(), p.into());
            }
            a
        };
        let list = [mk(1, None), mk(2, Some("1"))];
        let tree = Tree::of(&list);
        assert_eq!(tree.children(1), [2]);
    }

    #[test]
    fn walk_reports_depth_and_last_flags_and_honours_collapse() {
        let tree = t(&[
            (1, None),
            (2, Some(1)),
            (3, Some(1)),
            (4, Some(2)),
            (5, None),
        ]);
        let w = tree.walk(|_| true);
        let ids: Vec<u64> = w.iter().map(|v| v.id).collect();
        assert_eq!(ids, [1, 2, 4, 3, 5]);
        let depth: Vec<usize> = w.iter().map(|v| v.depth).collect();
        assert_eq!(depth, [0, 1, 2, 1, 0]);
        assert_eq!(w[2].last, [false, false, true]);
        assert_eq!(w[3].last, [false, true]);
        assert_eq!(w[4].last, [true]);
        let ids: Vec<u64> = tree.walk(|id| id != 1).iter().map(|v| v.id).collect();
        assert_eq!(ids, [1, 5]);
    }
}
