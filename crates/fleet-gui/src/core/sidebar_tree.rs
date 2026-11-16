//! The sidebar's tree (ADR 0074, spec §6): rows nested under the agent
//! that spawned them (`fleet.parent`, via `fleet_client::tree`), the
//! disclosure of parents with many children, and what Clear exited removes
//! once agents have children. Plain data, no gpui.

use std::collections::{HashMap, HashSet};

use fleet_client::tree::{Tree, parent_of};

use super::session::AgentRow;
use super::sidebar::ended;

/// A parent with more children than this starts collapsed.
pub const COLLAPSE_OVER: usize = 3;

/// A row's disclosure triangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disclosure {
    /// No children: no triangle.
    Leaf,
    Open,
    Closed,
}

/// How a row sits in the tree; parallel to the rows of a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowLayout {
    /// Roots are 0.
    pub depth: usize,
    pub disclosure: Disclosure,
    /// Children under this row (as filtered), shown or not.
    pub children: usize,
    /// Drawn only because a row under it matches the filter.
    pub context: bool,
}

impl RowLayout {
    pub const FLAT: RowLayout = RowLayout {
        depth: 0,
        disclosure: Disclosure::Leaf,
        children: 0,
        context: false,
    };
}

/// Whether a parent with `children` shows them: what the user chose this
/// session, else open up to [`COLLAPSE_OVER`] children.
pub fn is_open(id: u64, children: usize, chosen: &HashMap<u64, bool>) -> bool {
    chosen
        .get(&id)
        .copied()
        .unwrap_or(children <= COLLAPSE_OVER)
}

/// Opens or collapses `id`'s children, flipping what is shown now.
pub fn toggle_disclosure(chosen: &mut HashMap<u64, bool>, id: u64, children: usize) {
    let open = is_open(id, children, chosen);
    chosen.insert(id, !open);
}

/// `rows` (one group, in the order roots should appear) arranged as a
/// tree: each row's children under it, by id. A parent outside the group
/// makes a row a root here. Returns the rows to draw with their layouts.
///
/// Without `filtering`, every row is kept and collapsed parents hide their
/// children. With it, only rows `matched` accepts and their ancestors are
/// kept, every branch is open, and ancestors that do not match are marked
/// [`RowLayout::context`].
pub fn arrange(
    rows: &[AgentRow],
    matched: &dyn Fn(&AgentRow) -> bool,
    filtering: bool,
    chosen: &HashMap<u64, bool>,
) -> (Vec<AgentRow>, Vec<RowLayout>) {
    let mut by_id: Vec<&AgentRow> = rows.iter().collect();
    by_id.sort_by_key(|r| r.info.id);
    let tree = Tree::of(by_id.iter().map(|r| &r.info));
    let row_of: HashMap<u64, &AgentRow> = rows.iter().map(|r| (r.info.id, r)).collect();
    let hits: HashSet<u64> = rows
        .iter()
        .filter(|r| matched(r))
        .map(|r| r.info.id)
        .collect();
    let mut keep: HashSet<u64> = HashSet::new();
    if filtering {
        for &h in &hits {
            keep.insert(h);
            keep.extend(tree.ancestors(h));
        }
    } else {
        keep.extend(row_of.keys());
    }
    let mut out = (Vec::new(), Vec::new());
    // Depth-first, each root's turn in `rows` order.
    let mut stack: Vec<(u64, usize)> = rows
        .iter()
        .map(|r| r.info.id)
        .filter(|id| tree.parent(*id).is_none() && keep.contains(id))
        .map(|id| (id, 0))
        .rev()
        .collect();
    while let Some((id, depth)) = stack.pop() {
        let kids: Vec<u64> = tree
            .children(id)
            .iter()
            .copied()
            .filter(|k| keep.contains(k))
            .collect();
        let open = filtering || is_open(id, kids.len(), chosen);
        out.0.push(row_of[&id].clone());
        out.1.push(RowLayout {
            depth,
            disclosure: match (kids.is_empty(), open) {
                (true, _) => Disclosure::Leaf,
                (false, true) => Disclosure::Open,
                (false, false) => Disclosure::Closed,
            },
            children: kids.len(),
            context: filtering && !hits.contains(&id),
        });
        if open {
            stack.extend(kids.into_iter().rev().map(|k| (k, depth + 1)));
        }
    }
    out
}

/// What Clear exited removes: every ended agent with no running agent
/// below it. An ended parent stays while a descendant still runs.
pub fn removable_ended(rows: &[AgentRow]) -> Vec<u64> {
    let tree = Tree::of(rows.iter().map(|r| &r.info));
    let row_of: HashMap<u64, &AgentRow> = rows.iter().map(|r| (r.info.id, r)).collect();
    rows.iter()
        .filter(|r| ended(r))
        .filter(|r| tree.descendants(r.info.id).iter().all(|d| ended(row_of[d])))
        .map(|r| r.info.id)
        .collect()
}

/// The parent a row's label names, whether or not it is listed.
pub fn parent(row: &AgentRow) -> Option<u64> {
    parent_of(&row.info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::AgentState;

    fn row(id: u64, parent: Option<u64>, state: AgentState) -> AgentRow {
        let mut info = fleet_proto::AgentInfo {
            id,
            ..Default::default()
        };
        info.set_state(state);
        if let Some(p) = parent {
            info.labels.insert("fleet.parent".into(), p.to_string());
        }
        AgentRow {
            info,
            last_line: String::new(),
            last_output_unix_ms: None,
        }
    }

    fn run(id: u64, parent: Option<u64>) -> AgentRow {
        row(id, parent, AgentState::Running)
    }

    fn ids(rows: &[AgentRow]) -> Vec<u64> {
        rows.iter().map(|r| r.info.id).collect()
    }

    fn all(_: &AgentRow) -> bool {
        true
    }

    #[test]
    fn children_follow_their_parent_by_id_and_roots_keep_the_order_given() {
        let rows = [run(9, None), run(3, Some(1)), run(1, None), run(2, Some(1))];
        let (out, layout) = arrange(&rows, &all, false, &HashMap::new());
        assert_eq!(ids(&out), [9, 1, 2, 3]);
        let depth: Vec<usize> = layout.iter().map(|l| l.depth).collect();
        assert_eq!(depth, [0, 0, 1, 1]);
        assert_eq!(layout[1].disclosure, Disclosure::Open);
        assert_eq!(layout[1].children, 2);
        assert_eq!(layout[0].disclosure, Disclosure::Leaf);
    }

    #[test]
    fn a_parent_missing_from_the_group_makes_a_root() {
        let rows = [run(5, Some(99)), run(6, Some(5))];
        let (out, layout) = arrange(&rows, &all, false, &HashMap::new());
        assert_eq!(ids(&out), [5, 6]);
        assert_eq!(layout[0].depth, 0);
        assert_eq!(layout[1].depth, 1);
    }

    #[test]
    fn more_than_three_children_start_collapsed_until_toggled() {
        let mut rows = vec![run(1, None)];
        rows.extend((2..=4).map(|i| run(i, Some(1))));
        let (out, _) = arrange(&rows, &all, false, &HashMap::new());
        assert_eq!(out.len(), 4, "three children are shown");
        rows.push(run(5, Some(1)));
        let mut chosen = HashMap::new();
        let (out, layout) = arrange(&rows, &all, false, &chosen);
        assert_eq!(ids(&out), [1]);
        assert_eq!(layout[0].disclosure, Disclosure::Closed);
        assert_eq!(layout[0].children, 4);
        toggle_disclosure(&mut chosen, 1, 4);
        let (out, _) = arrange(&rows, &all, false, &chosen);
        assert_eq!(out.len(), 5);
        toggle_disclosure(&mut chosen, 1, 4);
        assert_eq!(arrange(&rows, &all, false, &chosen).0.len(), 1);
        // A small family can be collapsed by hand too.
        let mut chosen = HashMap::new();
        toggle_disclosure(&mut chosen, 1, 2);
        assert!(!is_open(1, 2, &chosen));
    }

    #[test]
    fn a_filter_keeps_ancestors_of_a_match_and_opens_every_branch() {
        let mut rows = vec![row(1, None, AgentState::Exited), run(2, Some(1))];
        rows.extend((3..=7).map(|i| run(i, Some(1))));
        rows.push(run(8, Some(2)));
        let hit = |r: &AgentRow| r.info.id == 8;
        let (out, layout) = arrange(&rows, &hit, true, &HashMap::new());
        assert_eq!(ids(&out), [1, 2, 8]);
        let ctx: Vec<bool> = layout.iter().map(|l| l.context).collect();
        assert_eq!(ctx, [true, true, false]);
        let depth: Vec<usize> = layout.iter().map(|l| l.depth).collect();
        assert_eq!(depth, [0, 1, 2]);
        // No match: nothing.
        assert!(
            arrange(&rows, &|_| false, true, &HashMap::new())
                .0
                .is_empty()
        );
    }

    #[test]
    fn a_loop_does_not_hang_and_is_shown() {
        let rows = [run(1, Some(2)), run(2, Some(1))];
        let (out, _) = arrange(&rows, &all, false, &HashMap::new());
        assert_eq!(ids(&out), [1, 2]);
    }

    #[test]
    fn clear_exited_takes_ended_branches_but_not_ended_parents_of_running_agents() {
        use AgentState::*;
        let rows = [
            // Ended with a running grandchild: stays, with its ended child.
            row(1, None, Exited),
            row(2, Some(1), Exited),
            run(3, Some(2)),
            // An ended branch: all of it goes.
            row(4, None, Exited),
            row(5, Some(4), Lost),
            // A lone ended agent, and a running one.
            row(6, None, Lost),
            run(7, None),
        ];
        let mut got = removable_ended(&rows);
        got.sort_unstable();
        assert_eq!(got, [4, 5, 6]);
    }
}
