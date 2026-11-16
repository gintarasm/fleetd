//! Split layout: a binary tree of panes, at most `MAX_PANES` leaves. Each
//! split starts even (ratio 0.5); dragging its divider moves it, keeping a
//! minimum pane size (ADR 0045).

use serde::{Deserialize, Serialize};

use super::session::PaneId;

pub const MAX_PANES: usize = 4;

const EPS: f32 = 0.001;

/// A new or reset split's ratio: the first side's share.
pub const EVEN: f32 = 0.5;

/// The narrowest share either side of a split may have, whatever the
/// minimum pane size; also bounds a ratio read from disk.
const MIN_RATIO: f32 = 0.01;

/// A split's ratio from disk: an even one unless it is strictly inside
/// `MIN_RATIO..=1 - MIN_RATIO`.
fn sane_ratio(r: f32) -> f32 {
    if (MIN_RATIO..=1.0 - MIN_RATIO).contains(&r) {
        r
    } else {
        EVEN
    }
}

fn even() -> f32 {
    EVEN
}

/// A split, numbered in tree order (depth first, parent before children).
/// Valid until the tree changes shape: a drag holds one only while it lasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SplitId(pub usize);

/// The line between a split's two sides.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Divider {
    pub split: SplitId,
    /// The split's axis: a `Horizontal` split's divider is a vertical line.
    pub axis: Axis,
    /// Where the line is along the axis: x for `Horizontal`, y for `Vertical`.
    pub at: f32,
    /// The whole split's area; the line runs across it.
    pub span: Rect,
}

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

impl SavedLayout {
    /// Every leaf showing `old` shows `new` instead (an agent restarted or
    /// removed while this layout was not loaded); `true` if any did.
    pub fn replace_agent(&mut self, old: u64, new: Option<u64>) -> bool {
        self.root.replace_agent(old, new)
    }
}

impl SavedNode {
    fn replace_agent(&mut self, old: u64, new: Option<u64>) -> bool {
        // Iterative: a saved tree is read from disk and may be deep.
        let mut changed = false;
        let mut stack = vec![self];
        while let Some(node) = stack.pop() {
            match node {
                SavedNode::Leaf { agent, .. } => {
                    if *agent == Some(old) {
                        *agent = new;
                        changed = true;
                    }
                }
                SavedNode::Split { first, second, .. } => {
                    stack.push(second);
                    stack.push(first);
                }
            }
        }
        changed
    }
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
        /// The first side's share; absent in layouts saved before ADR 0045.
        #[serde(default = "even")]
        ratio: f32,
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
        ratio: f32,
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
                ratio,
                first,
                second,
            } => {
                let (a, b) = halves(*axis, *ratio, r);
                first.rects(a, out);
                second.rects(b, out);
            }
        }
    }

    fn dividers(&self, r: Rect, next: &mut usize, out: &mut Vec<Divider>) {
        let Node::Split {
            axis,
            ratio,
            first,
            second,
        } = self
        else {
            return;
        };
        let split = SplitId(*next);
        *next += 1;
        let (a, b) = halves(*axis, *ratio, r);
        let at = match axis {
            Axis::Horizontal => b.x,
            Axis::Vertical => b.y,
        };
        out.push(Divider {
            split,
            axis: *axis,
            at,
            span: r,
        });
        first.dividers(a, next, out);
        second.dividers(b, next, out);
    }

    /// The split numbered `*n` (counting down) in tree order.
    fn nth_split(&mut self, n: &mut usize) -> Option<&mut Node> {
        if matches!(self, Node::Leaf { .. }) {
            return None;
        }
        if *n == 0 {
            return Some(self);
        }
        *n -= 1;
        let Node::Split { first, second, .. } = self else {
            return None;
        };
        if let Some(found) = first.nth_split(n) {
            return Some(found);
        }
        second.nth_split(n)
    }

    /// The least extent along `along` this subtree needs for each of its
    /// panes to get `min` along it, its ratios unchanged.
    fn min_extent(&self, along: Axis, min: f32) -> f32 {
        match self {
            Node::Leaf { .. } => min,
            Node::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, b) = (first.min_extent(along, min), second.min_extent(along, min));
                if *axis == along {
                    (a / ratio).max(b / (1.0 - ratio))
                } else {
                    a.max(b)
                }
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

    fn replace_agent(&mut self, old: u64, new: Option<u64>, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf { pane, agent } => {
                if *agent == Some(old) {
                    *agent = new;
                    out.push(*pane);
                }
            }
            Node::Split { first, second, .. } => {
                first.replace_agent(old, new, out);
                second.replace_agent(old, new, out);
            }
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
                    ratio: EVEN,
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
                ratio,
                first,
                second,
            } => SavedNode::Split {
                axis: *axis,
                ratio: *ratio,
                first: Box::new(first.to_saved()),
                second: Box::new(second.to_saved()),
            },
        }
    }
}

/// `r` cut along `axis`, the first side taking `ratio` of it.
fn halves(axis: Axis, ratio: f32, r: Rect) -> (Rect, Rect) {
    match axis {
        Axis::Horizontal => {
            let w = r.w * ratio;
            (
                Rect { w, ..r },
                Rect {
                    x: r.x + w,
                    w: r.w - w,
                    ..r
                },
            )
        }
        Axis::Vertical => {
            let h = r.h * ratio;
            (
                Rect { h, ..r },
                Rect {
                    y: r.y + h,
                    h: r.h - h,
                    ..r
                },
            )
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
            ratio,
            first,
            second,
        } => {
            let Some(a) = from_saved_node(first, budget, depth + 1)? else {
                return Ok(None);
            };
            Ok(Some(match from_saved_node(second, budget, depth + 1)? {
                Some(b) => Node::Split {
                    axis: *axis,
                    ratio: sane_ratio(*ratio),
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

    /// Every split's divider in a `w` × `h` area, in tree order.
    pub fn dividers(&self, w: f32, h: f32) -> Vec<Divider> {
        let mut out = Vec::new();
        self.root.dividers(
            Rect {
                x: 0.0,
                y: 0.0,
                w,
                h,
            },
            &mut 0,
            &mut out,
        );
        out
    }

    /// `split`'s ratio, `None` for an unknown split.
    pub fn ratio(&self, split: SplitId) -> Option<f32> {
        // Tree order, as `dividers` numbers them.
        let mut n = split.0;
        let mut stack = vec![&self.root];
        while let Some(node) = stack.pop() {
            if let Node::Split {
                ratio,
                first,
                second,
                ..
            } = node
            {
                if n == 0 {
                    return Some(*ratio);
                }
                n -= 1;
                stack.push(second);
                stack.push(first);
            }
        }
        None
    }

    /// Moves `split`'s divider to `at` along its axis (in a `w` × `h`
    /// area), as near as it can while every pane keeps `min` along that
    /// axis; where the split is too small for that, it stays. `true` if
    /// the ratio changed.
    pub fn move_divider(&mut self, split: SplitId, at: f32, w: f32, h: f32, min: f32) -> bool {
        let Some(d) = self.dividers(w, h).into_iter().find(|d| d.split == split) else {
            return false;
        };
        let (start, len) = match d.axis {
            Axis::Horizontal => (d.span.x, d.span.w),
            Axis::Vertical => (d.span.y, d.span.h),
        };
        let Some(Node::Split {
            axis,
            ratio,
            first,
            second,
        }) = self.root.nth_split(&mut { split.0 })
        else {
            return false;
        };
        if len <= 0.0 {
            return false;
        }
        let lo = (first.min_extent(*axis, min) / len).max(MIN_RATIO);
        let hi = (1.0 - second.min_extent(*axis, min) / len).min(1.0 - MIN_RATIO);
        if lo.is_nan() || hi.is_nan() || lo > hi || !at.is_finite() {
            return false;
        }
        let r = ((at - start) / len).clamp(lo, hi);
        if r == *ratio {
            return false;
        }
        *ratio = r;
        true
    }

    /// Evens `split` again; `true` if it was not even.
    pub fn reset_divider(&mut self, split: SplitId) -> bool {
        match self.root.nth_split(&mut { split.0 }) {
            Some(Node::Split { ratio, .. }) if *ratio != EVEN => {
                *ratio = EVEN;
                true
            }
            _ => false,
        }
    }

    pub fn set_agent(&mut self, pane: PaneId, agent: Option<u64>) {
        if let Some(slot) = self.root.leaf_mut(pane) {
            *slot = agent;
        }
    }

    /// Every pane showing `old` shows `new` instead (`None`: empty); returns
    /// those panes, in tree order.
    pub fn replace_agent(&mut self, old: u64, new: Option<u64>) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.root.replace_agent(old, new, &mut out);
        out
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
            ratio: EVEN,
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
            ratio: EVEN,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn replace_agent_rewrites_every_leaf() {
        let (mut l, [a, b, c, d]) = four();
        l.set_agent(a, Some(5));
        l.set_agent(c, Some(5));
        l.set_agent(b, Some(6));
        let mut changed = l.replace_agent(5, Some(9));
        changed.sort_unstable_by_key(|p| p.0);
        let mut want = vec![a, c];
        want.sort_unstable_by_key(|p| p.0);
        assert_eq!(changed, want);
        assert_eq!(l.agent(a), Some(9));
        assert_eq!(l.agent(c), Some(9));
        assert_eq!(l.agent(b), Some(6));
        assert_eq!(l.agent(d), None);
    }

    #[test]
    fn replace_with_none_empties() {
        let (mut l, [a, b, ..]) = four();
        l.set_agent(a, Some(5));
        l.set_agent(b, Some(6));
        assert_eq!(l.replace_agent(5, None), vec![a]);
        assert_eq!(l.agent(a), None);
        assert_eq!(l.agent(b), Some(6));
        assert!(l.replace_agent(5, None).is_empty());
    }

    #[test]
    fn saved_layout_replace_agent() {
        let (mut l, [a, b, c, _]) = four();
        l.set_agent(a, Some(5));
        l.set_agent(c, Some(5));
        l.set_agent(b, Some(6));
        let mut saved = l.to_saved();
        assert!(saved.replace_agent(5, Some(9)));
        let back = Layout::from_saved(&saved);
        assert_eq!(back.agent(a), Some(9));
        assert_eq!(back.agent(c), Some(9));
        assert_eq!(back.agent(b), Some(6));
        assert!(!saved.replace_agent(5, Some(9)));
        assert!(saved.replace_agent(6, None));
        assert_eq!(Layout::from_saved(&saved).agent(b), None);
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
    // --- Resizable splits (issue #35) ---

    const MIN: f32 = 100.0;

    fn side_by_side() -> (Layout, PaneId, PaneId) {
        let mut l = Layout::single();
        let a = l.panes()[0];
        let b = l.split(a, Axis::Horizontal).unwrap();
        (l, a, b)
    }

    fn rect(l: &Layout, pane: PaneId) -> Rect {
        l.rects(W, H)
            .into_iter()
            .find(|(p, _)| *p == pane)
            .unwrap()
            .1
    }

    #[test]
    fn a_new_split_is_even_with_its_divider_between_the_panes() {
        let (l, ..) = side_by_side();
        let d = l.dividers(W, H);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].axis, Axis::Horizontal);
        assert_eq!(d[0].at, W / 2.0);
        assert_eq!(
            d[0].span,
            Rect {
                x: 0.0,
                y: 0.0,
                w: W,
                h: H
            }
        );
        assert_eq!(l.ratio(d[0].split), Some(0.5));
        assert!(Layout::single().dividers(W, H).is_empty());
    }

    #[test]
    fn moving_a_divider_resizes_both_sides() {
        let (mut l, a, b) = side_by_side();
        let split = l.dividers(W, H)[0].split;
        assert!(l.move_divider(split, 200.0, W, H, MIN));
        assert_eq!(rect(&l, a).w, 200.0);
        assert_eq!(rect(&l, b).x, 200.0);
        assert_eq!(rect(&l, b).w, 600.0);
        assert_eq!(l.dividers(W, H)[0].at, 200.0);
        assert_eq!(l.ratio(split), Some(0.25));
        // Unmoved: no change reported.
        assert!(!l.move_divider(split, 200.0, W, H, MIN));
    }

    #[test]
    fn a_stacked_divider_moves_on_y() {
        let mut l = Layout::single();
        let top = l.panes()[0];
        let bottom = l.split(top, Axis::Vertical).unwrap();
        let d = l.dividers(W, H)[0];
        assert_eq!((d.axis, d.at), (Axis::Vertical, H / 2.0));
        assert!(l.move_divider(d.split, 450.0, W, H, MIN));
        assert_eq!(rect(&l, top).h, 450.0);
        assert_eq!(rect(&l, bottom).y, 450.0);
        assert_eq!(rect(&l, bottom).h, 150.0);
    }

    #[test]
    fn a_divider_stops_at_the_minimum_pane_size() {
        let (mut l, a, b) = side_by_side();
        let split = l.dividers(W, H)[0].split;
        assert!(l.move_divider(split, 10.0, W, H, MIN));
        assert_eq!(rect(&l, a).w, MIN);
        assert!(l.move_divider(split, W + 50.0, W, H, MIN));
        assert_eq!(rect(&l, b).w, MIN);
    }

    #[test]
    fn the_minimum_counts_every_pane_behind_the_divider() {
        // a | (b | c): the right side holds two panes side by side, so it
        // keeps twice the minimum; stacked panes need only one.
        let (mut l, a, b) = side_by_side();
        let c = l.split(b, Axis::Horizontal).unwrap();
        let outer = l.dividers(W, H)[0].split;
        assert!(l.move_divider(outer, W, W, H, MIN));
        assert!((rect(&l, b).w - MIN).abs() < 0.01);
        assert!((rect(&l, c).w - MIN).abs() < 0.01);
        assert!((rect(&l, a).w - (W - 2.0 * MIN)).abs() < 0.01);

        let mut t = Layout::single();
        let top = t.panes()[0];
        t.split(top, Axis::Horizontal).unwrap();
        t.split(top, Axis::Vertical).unwrap();
        let outer = t.dividers(W, H)[0].split;
        assert!(t.move_divider(outer, W, W, H, MIN));
        assert!((rect(&t, top).w - (W - MIN)).abs() < 0.01);
    }

    #[test]
    fn a_divider_too_cramped_to_honour_the_minimum_stays_put() {
        let (mut l, ..) = side_by_side();
        let split = l.dividers(W, H)[0].split;
        assert!(!l.move_divider(split, 50.0, 150.0, H, MIN));
        assert_eq!(l.ratio(split), Some(0.5));
        assert!(!l.move_divider(split, f32::NAN, W, H, MIN));
        assert_eq!(l.ratio(split), Some(0.5));
    }

    #[test]
    fn an_inner_divider_moves_within_its_own_pane() {
        // a | (b / c): the inner divider spans the right half only.
        let (mut l, a, b) = side_by_side();
        let c = l.split(b, Axis::Vertical).unwrap();
        let d = l.dividers(W, H);
        assert_eq!(d.len(), 2);
        let inner = d[1];
        assert_eq!(inner.axis, Axis::Vertical);
        assert_eq!(inner.span.x, W / 2.0);
        assert_eq!(inner.span.w, W / 2.0);
        assert!(l.move_divider(inner.split, 200.0, W, H, MIN));
        assert_eq!(rect(&l, b).h, 200.0);
        assert_eq!(rect(&l, c).h, 400.0);
        assert_eq!(rect(&l, a).h, H);
    }

    #[test]
    fn resetting_a_divider_evens_it() {
        let (mut l, ..) = side_by_side();
        let split = l.dividers(W, H)[0].split;
        assert!(!l.reset_divider(split));
        l.move_divider(split, 200.0, W, H, MIN);
        assert!(l.reset_divider(split));
        assert_eq!(l.ratio(split), Some(0.5));
        assert!(!l.reset_divider(SplitId(9)));
        assert!(!l.move_divider(SplitId(9), 200.0, W, H, MIN));
        assert_eq!(l.ratio(SplitId(9)), None);
    }

    #[test]
    fn splitting_or_closing_keeps_other_ratios() {
        let (mut l, a, b) = side_by_side();
        let outer = l.dividers(W, H)[0].split;
        l.move_divider(outer, 200.0, W, H, MIN);
        let c = l.split(b, Axis::Vertical).unwrap();
        assert_eq!(rect(&l, a).w, 200.0);
        assert_eq!(rect(&l, c).h, H / 2.0);
        l.close(c);
        assert_eq!(rect(&l, a).w, 200.0);
        assert_eq!(rect(&l, b).w, 600.0);
    }

    #[test]
    fn ratios_are_saved_with_the_layout() {
        let (mut l, a, _) = side_by_side();
        let split = l.dividers(W, H)[0].split;
        l.move_divider(split, 200.0, W, H, MIN);
        let saved = l.to_saved();
        let text = toml::to_string(&saved).unwrap();
        let back = Layout::from_saved(&toml::from_str(&text).unwrap());
        assert_eq!(back, l);
        assert_eq!(rect(&back, a).w, 200.0);
    }

    #[test]
    fn a_layout_saved_before_ratios_reads_as_even() {
        let old = "[root.Split]\naxis = \"Horizontal\"\n\
                   [root.Split.first.Leaf]\npane = 1\n\
                   [root.Split.second.Leaf]\npane = 2\n";
        let saved: SavedLayout = toml::from_str(old).unwrap();
        let l = Layout::from_saved(&saved);
        assert_eq!(l.panes(), vec![PaneId(1), PaneId(2)]);
        assert_eq!(l.ratio(SplitId(0)), Some(0.5));
    }

    #[test]
    fn a_saved_ratio_out_of_range_reads_as_even() {
        for bad in [0.0, 1.0, -3.0, 7.0, f32::NAN, f32::INFINITY] {
            let saved = SavedLayout {
                root: SavedNode::Split {
                    axis: Axis::Vertical,
                    ratio: bad,
                    first: Box::new(leaf(1)),
                    second: Box::new(leaf(2)),
                },
            };
            assert_eq!(Layout::from_saved(&saved).ratio(SplitId(0)), Some(0.5));
        }
    }
}
