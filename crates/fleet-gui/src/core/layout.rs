//! Split layout: a binary tree of panes, at most `MAX_PANES` leaves, ratio 0.5 per split.

use serde::{Deserialize, Serialize};

use super::session::PaneId;

pub const MAX_PANES: usize = 4;

const EPS: f32 = 0.001;

/// Split direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    /// Side by side.
    Horizontal,
    /// Stacked.
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Saved split tree for one target (pane ids and agent ids).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedLayout {
    pub root: SavedNode,
}

impl Default for SavedLayout {
    fn default() -> Self {
        Layout::single().to_saved()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SavedNode {
    Leaf {
        pane: u32,
        agent: Option<u64>,
    },
    Split {
        axis: Axis,
        first: Box<SavedNode>,
        second: Box<SavedNode>,
    },
}

#[derive(Clone, Debug, PartialEq)]
enum Node {
    Leaf {
        pane: PaneId,
        agent: Option<u64>,
    },
    Split {
        axis: Axis,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf { pane, .. } => out.push(*pane),
            Node::Split { first, second, .. } => {
                first.leaves(out);
                second.leaves(out);
            }
        }
    }

    fn rects(&self, r: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Node::Leaf { pane, .. } => out.push((*pane, r)),
            Node::Split {
                axis,
                first,
                second,
            } => {
                let (a, b) = match axis {
                    Axis::Horizontal => {
                        let w = r.w / 2.0;
                        (Rect { w, ..r }, Rect { x: r.x + w, w, ..r })
                    }
                    Axis::Vertical => {
                        let h = r.h / 2.0;
                        (Rect { h, ..r }, Rect { y: r.y + h, h, ..r })
                    }
                };
                first.rects(a, out);
                second.rects(b, out);
            }
        }
    }

    fn leaf(&self, id: PaneId) -> Option<Option<u64>> {
        match self {
            Node::Leaf { pane, agent } => (*pane == id).then_some(*agent),
            Node::Split { first, second, .. } => first.leaf(id).or_else(|| second.leaf(id)),
        }
    }

    fn leaf_mut(&mut self, id: PaneId) -> Option<&mut Option<u64>> {
        match self {
            Node::Leaf { pane, agent } => (*pane == id).then_some(agent),
            Node::Split { first, second, .. } => first.leaf_mut(id).or_else(|| second.leaf_mut(id)),
        }
    }

    /// Replaces leaf `id` by a split holding it and `new`. False if absent.
    fn split(&mut self, id: PaneId, axis: Axis, new: PaneId) -> bool {
        match self {
            Node::Leaf { pane, .. } if *pane == id => {
                let orig = std::mem::replace(
                    self,
                    Node::Leaf {
                        pane: new,
                        agent: None,
                    },
                );
                *self = Node::Split {
                    axis,
                    first: Box::new(orig),
                    second: Box::new(Node::Leaf {
                        pane: new,
                        agent: None,
                    }),
                };
                true
            }
            Node::Leaf { .. } => false,
            Node::Split { first, second, .. } => {
                first.split(id, axis, new) || second.split(id, axis, new)
            }
        }
    }

    /// Removes leaf `id`, collapsing its parent into the sibling. Returns the
    /// sibling's first pane (the new focus) when found. Never called on a lone root leaf.
    fn close(&mut self, id: PaneId) -> Option<PaneId> {
        let Node::Split { first, second, .. } = self else {
            return None;
        };
        let is_leaf = |n: &Node| matches!(n, Node::Leaf { pane, .. } if *pane == id);
        let survivor = if is_leaf(first) {
            Some(std::mem::replace(
                &mut **second,
                Node::Leaf {
                    pane: id,
                    agent: None,
                },
            ))
        } else if is_leaf(second) {
            Some(std::mem::replace(
                &mut **first,
                Node::Leaf {
                    pane: id,
                    agent: None,
                },
            ))
        } else {
            None
        };
        match survivor {
            Some(s) => {
                let mut v = Vec::new();
                s.leaves(&mut v);
                *self = s;
                v.first().copied()
            }
            None => first.close(id).or_else(|| second.close(id)),
        }
    }

    fn to_saved(&self) -> SavedNode {
        match self {
            Node::Leaf { pane, agent } => SavedNode::Leaf {
                pane: pane.0,
                agent: *agent,
            },
            Node::Split {
                axis,
                first,
                second,
            } => SavedNode::Split {
                axis: *axis,
                first: Box::new(first.to_saved()),
                second: Box::new(second.to_saved()),
            },
        }
    }
}

/// Deepest saved split nesting read; `MAX_PANES` leaves need far less.
const MAX_SAVED_DEPTH: usize = 16;

/// Builds a tree from saved form, keeping the first `budget` leaves
/// (tree order). `Err` if nested deeper than `MAX_SAVED_DEPTH`.
fn from_saved_node(s: &SavedNode, budget: &mut usize, depth: usize) -> Result<Option<Node>, ()> {
    if depth > MAX_SAVED_DEPTH {
        return Err(());
    }
    if *budget == 0 {
        return Ok(None);
    }
    match s {
        SavedNode::Leaf { pane, agent } => {
            *budget -= 1;
            Ok(Some(Node::Leaf {
                pane: PaneId(*pane),
                agent: *agent,
            }))
        }
        SavedNode::Split {
            axis,
            first,
            second,
        } => {
            let Some(a) = from_saved_node(first, budget, depth + 1)? else {
                return Ok(None);
            };
            Ok(Some(match from_saved_node(second, budget, depth + 1)? {
                Some(b) => Node::Split {
                    axis: *axis,
                    first: Box::new(a),
                    second: Box::new(b),
                },
                None => a,
            }))
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    root: Node,
    next_id: u32,
}

impl Layout {
    pub fn single() -> Layout {
        Layout {
            root: Node::Leaf {
                pane: PaneId(1),
                agent: None,
            },
            next_id: 2,
        }
    }

    /// Splits `pane`; the new pane goes right / below. `None` at `MAX_PANES` or for an unknown pane.
    pub fn split(&mut self, pane: PaneId, axis: Axis) -> Option<PaneId> {
        if self.panes().len() >= MAX_PANES || !self.panes().contains(&pane) {
            return None;
        }
        let new = PaneId(self.next_id);
        self.root.split(pane, axis, new);
        self.next_id += 1;
        Some(new)
    }

    /// Closes `pane`, returning the new focus. Closing the last pane leaves a
    /// single empty pane (returned). `None` for an unknown pane.
    pub fn close(&mut self, pane: PaneId) -> Option<PaneId> {
        if !self.panes().contains(&pane) {
            return None;
        }
        if let Node::Leaf { agent, .. } = &mut self.root {
            *agent = None;
            return Some(pane);
        }
        self.root.close(pane)
    }

    /// Panes in reading order (top-left first): by row, then left to
    /// right, from where each pane sits, not from how the tree was split.
    pub fn panes(&self) -> Vec<PaneId> {
        let mut rects = self.rects(1.0, 1.0);
        rects.sort_by(|(_, a), (_, b)| a.y.total_cmp(&b.y));
        // Tops within EPS of a row's first top share that row.
        let mut row = 0usize;
        let mut row_top = f32::NEG_INFINITY;
        let mut keyed: Vec<(usize, f32, PaneId)> = rects
            .into_iter()
            .map(|(p, r)| {
                if r.y - row_top > EPS {
                    row += 1;
                    row_top = r.y;
                }
                (row, r.x, p)
            })
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        keyed.into_iter().map(|(_, _, p)| p).collect()
    }

    /// 1-based number of `pane` in reading order.
    pub fn pane_number(&self, pane: PaneId) -> Option<usize> {
        self.panes().iter().position(|p| *p == pane).map(|i| i + 1)
    }

    pub fn rects(&self, w: f32, h: f32) -> Vec<(PaneId, Rect)> {
        let mut v = Vec::new();
        self.root.rects(
            Rect {
                x: 0.0,
                y: 0.0,
                w,
                h,
            },
            &mut v,
        );
        v
    }

    /// The nearest pane lying in `dir` from `from` that overlaps it on the other axis.
    pub fn neighbour(&self, from: PaneId, dir: Dir, w: f32, h: f32) -> Option<PaneId> {
        let rects = self.rects(w, h);
        let (_, f) = *rects.iter().find(|(p, _)| *p == from)?;
        let overlap = |a0: f32, a1: f32, b0: f32, b1: f32| a1.min(b1) - a0.max(b0) > EPS;
        rects
            .iter()
            .filter(|(p, _)| *p != from)
            .filter_map(|(p, r)| {
                let (gap, ok) = match dir {
                    Dir::Left => (f.x - (r.x + r.w), overlap(f.y, f.y + f.h, r.y, r.y + r.h)),
                    Dir::Right => (r.x - (f.x + f.w), overlap(f.y, f.y + f.h, r.y, r.y + r.h)),
                    Dir::Up => (f.y - (r.y + r.h), overlap(f.x, f.x + f.w, r.x, r.x + r.w)),
                    Dir::Down => (r.y - (f.y + f.h), overlap(f.x, f.x + f.w, r.x, r.x + r.w)),
                };
                (ok && gap > -EPS).then_some((*p, gap))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(p, _)| p)
    }

    pub fn set_agent(&mut self, pane: PaneId, agent: Option<u64>) {
        if let Some(slot) = self.root.leaf_mut(pane) {
            *slot = agent;
        }
    }

    pub fn agent(&self, pane: PaneId) -> Option<u64> {
        self.root.leaf(pane).flatten()
    }

    pub fn to_saved(&self) -> SavedLayout {
        SavedLayout {
            root: self.root.to_saved(),
        }
    }

    /// Rebuilds a layout; more than `MAX_PANES` leaves are truncated to the
    /// first four in tree order. A corrupt one (a pane id twice, or nested too deep)
    /// gives [`Layout::single`].
    pub fn from_saved(saved: &SavedLayout) -> Layout {
        let mut budget = MAX_PANES;
        let Ok(Some(root)) = from_saved_node(&saved.root, &mut budget, 0) else {
            return Layout::single();
        };
        let mut l = Layout { root, next_id: 1 };
        let mut ids = l.panes();
        ids.sort_unstable_by_key(|p| p.0);
        ids.dedup();
        if ids.len() != l.panes().len() {
            return Layout::single();
        }
        l.next_id = l.panes().iter().map(|p| p.0).max().unwrap_or(0) + 1;
        l
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 800.0;
    const H: f32 = 600.0;

    fn four() -> (Layout, [PaneId; 4]) {
        // 1 | 2   then split 1 down (3), 2 down (4): grid
        let mut l = Layout::single();
        let a = l.panes()[0];
        let b = l.split(a, Axis::Horizontal).unwrap();
        let c = l.split(a, Axis::Vertical).unwrap();
        let d = l.split(b, Axis::Vertical).unwrap();
        (l, [a, b, c, d])
    }

    #[test]
    fn split_to_four_then_refuses() {
        let (mut l, [a, ..]) = four();
        assert_eq!(l.panes().len(), MAX_PANES);
        assert_eq!(l.split(a, Axis::Horizontal), None);
    }

    #[test]
    fn close_returns_sibling_focus() {
        let mut l = Layout::single();
        let a = l.panes()[0];
        let b = l.split(a, Axis::Horizontal).unwrap();
        assert_eq!(l.close(b), Some(a));
        assert_eq!(l.panes(), vec![a]);
        let b = l.split(a, Axis::Vertical).unwrap();
        assert_eq!(l.close(a), Some(b));
        assert_eq!(l.panes(), vec![b]);
        assert_eq!(l.close(PaneId(99)), None);
    }

    #[test]
    fn last_close_leaves_empty_single() {
        let mut l = Layout::single();
        let a = l.panes()[0];
        l.set_agent(a, Some(7));
        assert_eq!(l.close(a), Some(a));
        assert_eq!(l.panes(), vec![a]);
        assert_eq!(l.agent(a), None);
    }

    #[test]
    fn grid_numbers_row_major() {
        // Built depth-first as a | b, then a / c, b / d: a 2x2 grid.
        let (l, [a, b, c, d]) = four();
        assert_eq!(l.panes(), vec![a, b, c, d]);
        assert_eq!(l.pane_number(a), Some(1));
        assert_eq!(l.pane_number(b), Some(2));
        assert_eq!(l.pane_number(c), Some(3));
        assert_eq!(l.pane_number(d), Some(4));
        // Split the other way round: top / bottom, then each side by side.
        let mut t = Layout::single();
        let tl = t.panes()[0];
        let bl = t.split(tl, Axis::Vertical).unwrap();
        let tr = t.split(tl, Axis::Horizontal).unwrap();
        let br = t.split(bl, Axis::Horizontal).unwrap();
        assert_eq!(t.panes(), vec![tl, tr, bl, br]);
    }

    #[test]
    fn stacked_left_then_right_numbers_top_row_first() {
        // (top / bottom) | right: the right pane shares the top row.
        let mut l = Layout::single();
        let top = l.panes()[0];
        let right = l.split(top, Axis::Horizontal).unwrap();
        let bottom = l.split(top, Axis::Vertical).unwrap();
        assert_eq!(l.panes(), vec![top, right, bottom]);
        // Saved and restored, the numbering is the same.
        assert_eq!(Layout::from_saved(&l.to_saved()).panes(), l.panes());
    }

    #[test]
    fn pane_numbers_reading_order() {
        let mut l = Layout::single();
        let left = l.panes()[0];
        let right = l.split(left, Axis::Horizontal).unwrap();
        let bottom = l.split(right, Axis::Vertical).unwrap();
        assert_eq!(l.panes(), vec![left, right, bottom]);
        assert_eq!(l.pane_number(left), Some(1));
        assert_eq!(l.pane_number(right), Some(2));
        assert_eq!(l.pane_number(bottom), Some(3));
        let r = l.rects(W, H);
        assert!(r[1].1.y < r[2].1.y);
    }

    #[test]
    fn neighbour_directions() {
        let (l, [a, b, c, d]) = four();
        // a top-left, b top-right, c bottom-left, d bottom-right
        assert_eq!(l.neighbour(a, Dir::Right, W, H), Some(b));
        assert_eq!(l.neighbour(a, Dir::Down, W, H), Some(c));
        assert_eq!(l.neighbour(d, Dir::Left, W, H), Some(c));
        assert_eq!(l.neighbour(d, Dir::Up, W, H), Some(b));
        assert_eq!(l.neighbour(a, Dir::Left, W, H), None);
        assert_eq!(l.neighbour(a, Dir::Up, W, H), None);
        assert_eq!(l.neighbour(d, Dir::Right, W, H), None);
        assert_eq!(l.neighbour(d, Dir::Down, W, H), None);

        // left | (top-right / bottom-right): from left, Right is the nearest overlapping one
        let mut t = Layout::single();
        let left = t.panes()[0];
        let right = t.split(left, Axis::Horizontal).unwrap();
        let br = t.split(right, Axis::Vertical).unwrap();
        assert_eq!(t.neighbour(left, Dir::Right, W, H), Some(right));
        assert_eq!(t.neighbour(br, Dir::Left, W, H), Some(left));
        assert_eq!(t.neighbour(PaneId(99), Dir::Left, W, H), None);
    }

    #[test]
    fn rects_tile_exactly() {
        let (l, _) = four();
        let area: f32 = l.rects(W, H).iter().map(|(_, r)| r.w * r.h).sum();
        assert!((area - W * H).abs() < 0.01);
        let mut t = Layout::single();
        let a = t.panes()[0];
        let b = t.split(a, Axis::Horizontal).unwrap();
        t.split(b, Axis::Vertical).unwrap();
        let area: f32 = t.rects(W, H).iter().map(|(_, r)| r.w * r.h).sum();
        assert!((area - W * H).abs() < 0.01);
    }

    #[test]
    fn saved_roundtrip_and_truncation() {
        let (mut l, [a, _, c, _]) = four();
        l.set_agent(a, Some(11));
        l.set_agent(c, Some(12));
        let saved = l.to_saved();
        let back = Layout::from_saved(&saved);
        assert_eq!(back, l);

        // through toml (GuiState stores it that way)
        let text = toml::to_string(&saved).unwrap();
        let parsed: SavedLayout = toml::from_str(&text).unwrap();
        assert_eq!(parsed, saved);

        // truncation: 5 leaves keep the first 4
        let leaf = |pane| SavedNode::Leaf { pane, agent: None };
        let split = |first, second| SavedNode::Split {
            axis: Axis::Horizontal,
            first: Box::new(first),
            second: Box::new(second),
        };
        let five = SavedLayout {
            root: split(
                split(leaf(1), leaf(2)),
                split(leaf(3), split(leaf(4), leaf(5))),
            ),
        };
        let mut t = Layout::from_saved(&five);
        let ids: Vec<u32> = t.panes().iter().map(|p| p.0).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
        // fresh ids do not collide, and the cap still holds
        assert_eq!(t.split(PaneId(1), Axis::Vertical), None);
        t.close(PaneId(4));
        assert_eq!(t.split(PaneId(1), Axis::Vertical), Some(PaneId(5)));
    }

    fn leaf(pane: u32) -> SavedNode {
        SavedNode::Leaf { pane, agent: None }
    }

    fn split(first: SavedNode, second: SavedNode) -> SavedNode {
        SavedNode::Split {
            axis: Axis::Horizontal,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn saved_layout_with_duplicate_pane_ids_falls_back_to_one_pane() {
        let dup = SavedLayout {
            root: split(leaf(1), leaf(1)),
        };
        assert_eq!(Layout::from_saved(&dup), Layout::single());
    }

    #[test]
    fn saved_layout_nested_too_deep_falls_back_to_one_pane() {
        let mut node = leaf(1);
        for _ in 0..100_000 {
            node = split(node, leaf(2));
        }
        let deep = SavedLayout { root: node };
        assert_eq!(Layout::from_saved(&deep), Layout::single());
        // Dropping the deep tree recurses too; leak it in the test.
        std::mem::forget(deep);
    }
}
