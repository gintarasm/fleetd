//! The graph view's geometry (spec §7, ADR 0073): a tidy tree, roots on
//! the left and children to the right, inside a box per project inside a
//! box for the machine; its edges, the viewport (pan and zoom), culling to
//! it, and keyboard steps between cards. Logical pixels in world
//! coordinates; gpui-free.
//!
//! **The tree.** A subtree takes a band of rows: one row for a leaf or a
//! collapsed node, else the sum of its children's bands. A parent sits on
//! its first child's row, in the column of its depth. Positions depend on
//! the tree alone (siblings and roots ordered by id, projects by title),
//! so an agent coming or going moves only what is below it: its later
//! siblings' subtrees, later trees and later project boxes.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::graph::Forest;
use super::layout::Rect;

/// Card and spacing metrics.
pub mod metrics {
    pub const CARD_W: f32 = 232.0;
    pub const CARD_H: f32 = 84.0;
    /// Between columns (the edges' elbows sit in the middle).
    pub const COL_GAP: f32 = 48.0;
    pub const ROW_GAP: f32 = 12.0;
    /// Around the content of a project box, and its title strip.
    pub const BOX_PAD: f32 = 16.0;
    pub const BOX_TITLE: f32 = 28.0;
    pub const BOX_GAP: f32 = 16.0;
    /// Around the project boxes inside the machine box, and its title strip.
    pub const MACHINE_PAD: f32 = 16.0;
    pub const MACHINE_TITLE: f32 = 32.0;
    /// Around the machine box.
    pub const MARGIN: f32 = 24.0;
    pub const COL: f32 = CARD_W + COL_GAP;
    pub const ROW: f32 = CARD_H + ROW_GAP;
}
use metrics::*;

/// What a box groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoxKind {
    Machine,
    Project,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroupBox {
    pub kind: BoxKind,
    pub title: String,
    pub rect: Rect,
}

/// One agent's card.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub id: u64,
    pub rect: Rect,
    pub depth: usize,
    /// Shown children (under the filter), drawn or not.
    pub children: usize,
    /// Its children are hidden; the card shows [`Card::children`].
    pub collapsed: bool,
}

/// A parent-to-child line: right edge of the parent, an elbow in the
/// column gap, left edge of the child.
#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    pub parent: u64,
    pub child: u64,
    pub points: [(f32, f32); 4],
}

impl Edge {
    /// The rectangle the line lies in.
    pub fn bounds(&self) -> Rect {
        let xs = self.points.iter().map(|p| p.0);
        let ys = self.points.iter().map(|p| p.1);
        let (x0, x1) = (
            xs.clone().fold(f32::MAX, f32::min),
            xs.fold(f32::MIN, f32::max),
        );
        let (y0, y1) = (
            ys.clone().fold(f32::MAX, f32::min),
            ys.fold(f32::MIN, f32::max),
        );
        Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        }
    }
}

/// Whether `a` and `b` overlap (touching edges count).
pub fn intersects(a: &Rect, b: &Rect) -> bool {
    a.x <= b.x + b.w && b.x <= a.x + a.w && a.y <= b.y + b.h && b.y <= a.y + a.h
}

/// What [`layout`] reads.
pub struct LayoutInput<'a> {
    pub forest: &'a Forest,
    /// The machine box's title.
    pub machine: &'a str,
    /// The project box an agent's tree goes in, by the tree's root.
    pub project: &'a dyn Fn(u64) -> String,
    /// Agents whose children are hidden.
    pub collapsed: &'a HashSet<u64>,
    /// Under a filter, the agents shown (matches and their ancestors).
    pub shown: Option<&'a HashSet<u64>>,
}

/// The laid-out graph.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphLayout {
    /// In placement order: by box, tree, then parent before children.
    pub cards: Vec<Card>,
    pub edges: Vec<Edge>,
    /// The machine box first, then the project boxes.
    pub boxes: Vec<GroupBox>,
    /// The extent of everything, margin included.
    pub width: f32,
    pub height: f32,
    index: HashMap<u64, usize>,
}

impl GraphLayout {
    pub fn card(&self, id: u64) -> Option<&Card> {
        self.index.get(&id).map(|&i| &self.cards[i])
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    /// The cards that overlap `view` (world coordinates).
    pub fn cards_in(&self, view: &Rect) -> impl Iterator<Item = &Card> {
        self.cards.iter().filter(move |c| intersects(&c.rect, view))
    }

    /// The edges that overlap `view`.
    pub fn edges_in(&self, view: &Rect) -> impl Iterator<Item = &Edge> {
        self.edges
            .iter()
            .filter(move |e| intersects(&e.bounds(), view))
    }

    /// The boxes that overlap `view`.
    pub fn boxes_in(&self, view: &Rect) -> impl Iterator<Item = &GroupBox> {
        self.boxes.iter().filter(move |b| intersects(&b.rect, view))
    }
}

/// Lays out the forest; see the module docs.
pub fn layout(input: &LayoutInput<'_>) -> GraphLayout {
    let shown = |id: &u64| input.shown.is_none_or(|s| s.contains(id));
    let mut by_project: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for &root in input.forest.roots().iter().filter(|r| shown(r)) {
        by_project
            .entry((input.project)(root))
            .or_default()
            .push(root);
    }
    let mut out = GraphLayout::default();
    if by_project.is_empty() {
        return out;
    }
    let box_x = MARGIN + MACHINE_PAD;
    let mut y = MARGIN + MACHINE_TITLE;
    let mut widest = 0.0f32;
    let mut project_boxes = Vec::new();
    for (title, roots) in by_project {
        let origin = (box_x + BOX_PAD, y + BOX_TITLE);
        let mut placer = Placer {
            input,
            shown: &shown,
            origin,
            out: &mut out,
            max_depth: 0,
        };
        let mut rows = 0;
        for root in roots {
            rows += placer.place(root, 0, rows);
        }
        let cols = placer.max_depth + 1;
        let w = 2.0 * BOX_PAD + cols as f32 * COL - COL_GAP;
        let h = BOX_TITLE + rows as f32 * ROW - ROW_GAP + BOX_PAD;
        project_boxes.push(GroupBox {
            kind: BoxKind::Project,
            title,
            rect: Rect { x: box_x, y, w, h },
        });
        widest = widest.max(w);
        y += h + BOX_GAP;
    }
    let machine = Rect {
        x: MARGIN,
        y: MARGIN,
        w: widest + 2.0 * MACHINE_PAD,
        h: y - BOX_GAP + MACHINE_PAD - MARGIN,
    };
    out.width = machine.x + machine.w + MARGIN;
    out.height = machine.y + machine.h + MARGIN;
    out.boxes.push(GroupBox {
        kind: BoxKind::Machine,
        title: input.machine.to_owned(),
        rect: machine,
    });
    out.boxes.extend(project_boxes);
    out.index = out
        .cards
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id, i))
        .collect();
    out
}

struct Placer<'a, 'b> {
    input: &'a LayoutInput<'a>,
    shown: &'a dyn Fn(&u64) -> bool,
    origin: (f32, f32),
    out: &'b mut GraphLayout,
    max_depth: usize,
}

impl Placer<'_, '_> {
    /// Places `id`'s subtree from `row`; returns the rows it takes.
    fn place(&mut self, id: u64, depth: usize, row: usize) -> usize {
        self.max_depth = self.max_depth.max(depth);
        let rect = Rect {
            x: self.origin.0 + depth as f32 * COL,
            y: self.origin.1 + row as f32 * ROW,
            w: CARD_W,
            h: CARD_H,
        };
        let kids: Vec<u64> = self
            .input
            .forest
            .children(id)
            .iter()
            .copied()
            .filter(|k| (self.shown)(k))
            .collect();
        let collapsed = !kids.is_empty() && self.input.collapsed.contains(&id);
        self.out.cards.push(Card {
            id,
            rect,
            depth,
            children: kids.len(),
            collapsed,
        });
        if kids.is_empty() || collapsed {
            return 1;
        }
        let mut r = row;
        for k in kids {
            let child_y = self.origin.1 + r as f32 * ROW;
            r += self.place(k, depth + 1, r);
            let (px, py) = (rect.x + rect.w, rect.y + rect.h / 2.0);
            let cx = rect.x + COL;
            let cy = child_y + CARD_H / 2.0;
            let mid = px + COL_GAP / 2.0;
            self.out.edges.push(Edge {
                parent: id,
                child: k,
                points: [(px, py), (mid, py), (mid, cy), (cx, cy)],
            });
        }
        r - row
    }
}

/// What the layout depends on: the view lays out again only when this
/// changes (agents come or go, a parent, project or collapse changes, or
/// the filter's result does), never on activity.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Signature {
    pub machine: String,
    /// `(id, parent, project)` by id.
    pub nodes: Vec<(u64, Option<u64>, String)>,
    pub collapsed: Vec<u64>,
    pub shown: Option<Vec<u64>>,
}

impl Signature {
    pub fn new(
        machine: &str,
        forest: &Forest,
        project: &dyn Fn(u64) -> String,
        collapsed: &HashSet<u64>,
        shown: Option<&HashSet<u64>>,
    ) -> Self {
        let mut nodes: Vec<(u64, Option<u64>, String)> = Vec::new();
        let mut stack: Vec<u64> = forest.roots().to_vec();
        while let Some(id) = stack.pop() {
            nodes.push((id, forest.parent(id), project(id)));
            stack.extend_from_slice(forest.children(id));
        }
        nodes.sort_by_key(|n| n.0);
        let mut collapsed: Vec<u64> = collapsed.iter().copied().collect();
        collapsed.sort_unstable();
        let shown = shown.map(|s| {
            let mut v: Vec<u64> = s.iter().copied().collect();
            v.sort_unstable();
            v
        });
        Signature {
            machine: machine.to_owned(),
            nodes,
            collapsed,
            shown,
        }
    }
}

/// The smallest and largest zoom.
pub const MIN_ZOOM: f32 = 0.25;
pub const MAX_ZOOM: f32 = 2.0;
/// One ⌘+ / ⌘− step.
pub const ZOOM_STEP: f32 = 1.2;
/// Kept between a card revealed by the keyboard and the view's edge.
const REVEAL_PAD: f32 = 24.0;

/// Which part of the world the view shows: the world point at the view's
/// top-left corner, and the scale (screen px per world px).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub origin: (f32, f32),
    pub zoom: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport {
            origin: (0.0, 0.0),
            zoom: 1.0,
        }
    }
}

impl Viewport {
    pub fn to_screen(&self, p: (f32, f32)) -> (f32, f32) {
        (
            (p.0 - self.origin.0) * self.zoom,
            (p.1 - self.origin.1) * self.zoom,
        )
    }

    pub fn to_world(&self, p: (f32, f32)) -> (f32, f32) {
        (
            p.0 / self.zoom + self.origin.0,
            p.1 / self.zoom + self.origin.1,
        )
    }

    pub fn rect_to_screen(&self, r: &Rect) -> Rect {
        let (x, y) = self.to_screen((r.x, r.y));
        Rect {
            x,
            y,
            w: r.w * self.zoom,
            h: r.h * self.zoom,
        }
    }

    /// The world rectangle a `size` view shows.
    pub fn world_rect(&self, size: (f32, f32)) -> Rect {
        Rect {
            x: self.origin.0,
            y: self.origin.1,
            w: size.0 / self.zoom,
            h: size.1 / self.zoom,
        }
    }

    /// Moves the content by `delta` screen pixels (a drag or a scroll).
    pub fn pan_by(&mut self, delta: (f32, f32)) {
        self.origin.0 -= delta.0 / self.zoom;
        self.origin.1 -= delta.1 / self.zoom;
    }

    /// Scales by `factor` keeping the world point under `at` (screen) put.
    pub fn zoom_at(&mut self, at: (f32, f32), factor: f32) {
        let anchor = self.to_world(at);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.origin = (anchor.0 - at.0 / self.zoom, anchor.1 - at.1 / self.zoom);
    }

    /// Keeps some of a `content` (width, height) world in a `size` view:
    /// it cannot be panned entirely out of sight.
    pub fn clamp(&mut self, content: (f32, f32), size: (f32, f32)) {
        let (vw, vh) = (size.0 / self.zoom, size.1 / self.zoom);
        let keep = MARGIN * 2.0;
        self.origin.0 = self
            .origin
            .0
            .clamp(keep - vw, (content.0 - keep).max(keep - vw));
        self.origin.1 = self
            .origin
            .1
            .clamp(keep - vh, (content.1 - keep).max(keep - vh));
    }

    /// Pans the least that brings `r` (world) fully into a `size` view.
    pub fn reveal(&mut self, r: &Rect, size: (f32, f32)) {
        let view = self.world_rect(size);
        let pad = REVEAL_PAD / self.zoom;
        let axis = |origin: f32, len: f32, lo: f32, hi: f32| {
            // Too big to fit, or above / left of the view: align its start.
            if hi - lo + 2.0 * pad > len || lo - pad < origin {
                lo - pad
            } else if hi + pad > origin + len {
                hi + pad - len
            } else {
                origin
            }
        };
        self.origin = (
            axis(view.x, view.w, r.x, r.x + r.w),
            axis(view.y, view.h, r.y, r.y + r.h),
        );
    }
}

/// An arrow key in the graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// To the parent.
    Left,
    /// To the first child shown.
    Right,
    /// To the nearest card above in the same column.
    Up,
    /// To the nearest card below in the same column.
    Down,
}

/// The card an arrow moves the selection to from `from`, if any.
pub fn step(layout: &GraphLayout, from: u64, dir: Step) -> Option<u64> {
    let at = layout.card(from)?;
    match dir {
        Step::Left => layout
            .edges
            .iter()
            .find(|e| e.child == from)
            .map(|e| e.parent),
        Step::Right => layout
            .edges
            .iter()
            .find(|e| e.parent == from)
            .map(|e| e.child),
        Step::Up | Step::Down => {
            let same_col = layout.cards.iter().filter(|c| c.rect.x == at.rect.x);
            let dist = |c: &&Card| c.rect.y - at.rect.y;
            if dir == Step::Up {
                same_col
                    .filter(|c| dist(c) < 0.0)
                    .max_by(|a, b| dist(a).total_cmp(&dist(b)))
                    .map(|c| c.id)
            } else {
                same_col
                    .filter(|c| dist(c) > 0.0)
                    .min_by(|a, b| dist(a).total_cmp(&dist(b)))
                    .map(|c| c.id)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forest(nodes: &[(u64, Option<u64>)]) -> Forest {
        Forest::build(nodes.iter().copied())
    }

    fn lay(f: &Forest, collapsed: &[u64], shown: Option<&[u64]>) -> GraphLayout {
        lay_projects(f, collapsed, shown, &|_| "p".into())
    }

    fn lay_projects(
        f: &Forest,
        collapsed: &[u64],
        shown: Option<&[u64]>,
        project: &dyn Fn(u64) -> String,
    ) -> GraphLayout {
        let collapsed: HashSet<u64> = collapsed.iter().copied().collect();
        let shown: Option<HashSet<u64>> = shown.map(|s| s.iter().copied().collect());
        layout(&LayoutInput {
            forest: f,
            machine: "local",
            project,
            collapsed: &collapsed,
            shown: shown.as_ref(),
        })
    }

    /// `(column, row)` of each card, from its position.
    fn grid(l: &GraphLayout) -> Vec<(u64, usize, usize)> {
        let ox = l.cards[0].rect.x;
        let oy = l.boxes[1].rect.y + BOX_TITLE;
        l.cards
            .iter()
            .map(|c| {
                (
                    c.id,
                    ((c.rect.x - ox) / COL).round() as usize,
                    ((c.rect.y - oy) / ROW).round() as usize,
                )
            })
            .collect()
    }

    fn pos(l: &GraphLayout) -> HashMap<u64, (f32, f32)> {
        l.cards
            .iter()
            .map(|c| (c.id, (c.rect.x, c.rect.y)))
            .collect()
    }

    #[test]
    fn tidy_tree_puts_parents_on_their_first_childs_row() {
        // 1 ─┬ 2 ─┬ 4
        //    │    └ 5
        //    └ 3
        // 6
        let f = forest(&[
            (1, None),
            (2, Some(1)),
            (3, Some(1)),
            (4, Some(2)),
            (5, Some(2)),
            (6, None),
        ]);
        let l = lay(&f, &[], None);
        assert_eq!(
            grid(&l),
            [
                (1, 0, 0),
                (2, 1, 0),
                (4, 2, 0),
                (5, 2, 1),
                (3, 1, 2),
                (6, 0, 3)
            ]
        );
        assert_eq!(l.edges.len(), 4);
        let e = l.edges.iter().find(|e| e.child == 5).unwrap();
        let (p, c) = (l.card(2).unwrap().rect, l.card(5).unwrap().rect);
        assert_eq!(e.points[0], (p.x + p.w, p.y + p.h / 2.0));
        assert_eq!(e.points[3], (c.x, c.y + c.h / 2.0));
        assert_eq!(e.points[1].0, e.points[2].0);
    }

    #[test]
    fn an_agent_coming_moves_only_what_is_below_it() {
        let before = forest(&[(1, None), (2, Some(1)), (3, Some(1)), (6, None)]);
        // 7 joins as 2's child: 1, 2 keep their place; 3 and 6 move down.
        let after = forest(&[
            (1, None),
            (2, Some(1)),
            (3, Some(1)),
            (6, None),
            (7, Some(2)),
        ]);
        let (a, b) = (pos(&lay(&before, &[], None)), pos(&lay(&after, &[], None)));
        assert_eq!(a[&1], b[&1]);
        assert_eq!(a[&2], b[&2]);
        assert_eq!(b[&3].1 - a[&3].1, 0.0, "7 shares 2's row");
        // A second child of 2 does push 3 and 6 down a row.
        let more = forest(&[
            (1, None),
            (2, Some(1)),
            (3, Some(1)),
            (6, None),
            (7, Some(2)),
            (8, Some(2)),
        ]);
        let c = pos(&lay(&more, &[], None));
        assert_eq!((c[&1], c[&2], c[&7]), (b[&1], b[&2], b[&7]));
        assert_eq!(c[&3].1 - b[&3].1, ROW);
        assert_eq!(c[&6].1 - b[&6].1, ROW);
        assert_eq!(c[&3].0, b[&3].0);
    }

    #[test]
    fn an_agent_going_moves_only_what_is_below_it() {
        let before = forest(&[
            (1, None),
            (2, Some(1)),
            (3, Some(1)),
            (4, Some(1)),
            (9, None),
        ]);
        let after = forest(&[(1, None), (2, Some(1)), (4, Some(1)), (9, None)]);
        let (a, b) = (pos(&lay(&before, &[], None)), pos(&lay(&after, &[], None)));
        assert_eq!((a[&1], a[&2]), (b[&1], b[&2]));
        assert_eq!(a[&4].1 - b[&4].1, ROW);
        assert_eq!(a[&9].1 - b[&9].1, ROW);
    }

    #[test]
    fn layout_is_the_same_whatever_the_input_order() {
        let a = forest(&[(3, Some(1)), (1, None), (2, Some(1))]);
        let b = forest(&[(1, None), (2, Some(1)), (3, Some(1))]);
        assert_eq!(lay(&a, &[], None), lay(&b, &[], None));
    }

    #[test]
    fn collapse_hides_the_subtree_and_keeps_the_count() {
        let f = forest(&[
            (1, None),
            (2, Some(1)),
            (3, Some(1)),
            (4, Some(2)),
            (6, None),
        ]);
        let l = lay(&f, &[1], None);
        assert_eq!(grid(&l), [(1, 0, 0), (6, 0, 1)]);
        let c = l.card(1).unwrap();
        assert!(c.collapsed);
        assert_eq!(c.children, 2);
        assert!(l.edges.is_empty());
        // Collapsing a leaf is no collapse.
        let l = lay(&f, &[3], None);
        assert!(!l.card(3).unwrap().collapsed);
    }

    #[test]
    fn filter_shows_only_the_given_agents() {
        let f = forest(&[(1, None), (2, Some(1)), (3, Some(1)), (6, None)]);
        let l = lay(&f, &[], Some(&[1, 3]));
        assert_eq!(grid(&l), [(1, 0, 0), (3, 1, 0)]);
        assert_eq!(l.card(1).unwrap().children, 1);
        assert!(lay(&f, &[], Some(&[])).is_empty());
    }

    #[test]
    fn projects_are_boxes_sorted_by_title_inside_the_machine_box() {
        let f = forest(&[(1, None), (2, Some(1)), (5, None), (7, None)]);
        let project = |id: u64| {
            if id == 5 {
                "alpha".into()
            } else {
                "zeta".into()
            }
        };
        let l = lay_projects(&f, &[], None, &project);
        let titles: Vec<_> = l.boxes.iter().map(|b| (b.kind, b.title.as_str())).collect();
        assert_eq!(
            titles,
            [
                (BoxKind::Machine, "local"),
                (BoxKind::Project, "alpha"),
                (BoxKind::Project, "zeta")
            ]
        );
        let (machine, alpha, zeta) = (&l.boxes[0].rect, &l.boxes[1].rect, &l.boxes[2].rect);
        assert!(alpha.y + alpha.h < zeta.y);
        for b in [alpha, zeta] {
            assert!(b.x > machine.x && b.x + b.w < machine.x + machine.w);
            assert!(b.y > machine.y && b.y + b.h < machine.y + machine.h);
        }
        // Each card is inside its project's box; a child stays in its
        // root's box.
        for (id, b) in [(5, alpha), (1, zeta), (2, zeta), (7, zeta)] {
            let c = l.card(id).unwrap().rect;
            assert!(c.x >= b.x && c.x + c.w <= b.x + b.w, "{id}");
            assert!(c.y >= b.y + BOX_TITLE && c.y + c.h <= b.y + b.h, "{id}");
        }
        assert_eq!(l.width, machine.x + machine.w + MARGIN);
        assert_eq!(l.height, machine.y + machine.h + MARGIN);
        assert!(lay(&forest(&[]), &[], None).boxes.is_empty());
    }

    #[test]
    fn culling_keeps_what_overlaps_the_view() {
        let nodes: Vec<(u64, Option<u64>)> = (1..=50).map(|i| (i, None)).collect();
        let l = lay(&forest(&nodes), &[], None);
        let vp = Viewport::default();
        let view = vp.world_rect((800.0, 400.0));
        let shown: Vec<u64> = l.cards_in(&view).map(|c| c.id).collect();
        assert!(!shown.is_empty() && shown.len() < 10, "{shown:?}");
        assert_eq!(shown[0], 1);
        // Scrolled down: the first cards are gone.
        let mut vp = vp;
        vp.pan_by((0.0, -2000.0));
        let shown: Vec<u64> = l
            .cards_in(&vp.world_rect((800.0, 400.0)))
            .map(|c| c.id)
            .collect();
        assert!(!shown.contains(&1) && !shown.is_empty());
        let f = forest(&[(1, None), (2, Some(1))]);
        let l = lay(&f, &[], None);
        assert_eq!(l.edges_in(&view).count(), 1);
        let far = Rect {
            x: 5000.0,
            y: 5000.0,
            w: 10.0,
            h: 10.0,
        };
        assert_eq!(l.edges_in(&far).count(), 0);
        assert_eq!(l.boxes_in(&far).count(), 0);
    }

    #[test]
    fn signature_changes_with_the_tree_not_its_order() {
        let p = |_: u64| "p".to_owned();
        let none = HashSet::new();
        let a = Signature::new("m", &forest(&[(1, None), (2, Some(1))]), &p, &none, None);
        let b = Signature::new("m", &forest(&[(2, Some(1)), (1, None)]), &p, &none, None);
        assert_eq!(a, b);
        let c = Signature::new("m", &forest(&[(1, None), (2, None)]), &p, &none, None);
        assert_ne!(a, c);
        let d = Signature::new(
            "m",
            &forest(&[(1, None), (2, Some(1))]),
            &p,
            &HashSet::from([1]),
            None,
        );
        assert_ne!(a, d);
    }

    #[test]
    fn viewport_round_trips_and_zooms_about_a_point() {
        let mut vp = Viewport::default();
        vp.pan_by((100.0, 50.0));
        assert_eq!(vp.origin, (-100.0, -50.0));
        assert_eq!(vp.to_world(vp.to_screen((10.0, 20.0))), (10.0, 20.0));
        let at = (300.0, 200.0);
        let before = vp.to_world(at);
        vp.zoom_at(at, 1.5);
        assert_eq!(vp.zoom, 1.5);
        let after = vp.to_world(at);
        assert!((before.0 - after.0).abs() < 1e-3 && (before.1 - after.1).abs() < 1e-3);
        vp.zoom_at(at, 100.0);
        assert_eq!(vp.zoom, MAX_ZOOM);
        vp.zoom_at(at, 0.0001);
        assert_eq!(vp.zoom, MIN_ZOOM);
        let r = vp.rect_to_screen(&Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 40.0,
        });
        assert_eq!((r.w, r.h), (25.0, 10.0));
    }

    #[test]
    fn clamp_keeps_content_in_sight() {
        let mut vp = Viewport::default();
        vp.pan_by((-10_000.0, -10_000.0));
        vp.clamp((1000.0, 800.0), (600.0, 400.0));
        let view = vp.world_rect((600.0, 400.0));
        assert!(view.x < 1000.0 && view.y < 800.0);
        vp.pan_by((20_000.0, 20_000.0));
        vp.clamp((1000.0, 800.0), (600.0, 400.0));
        let view = vp.world_rect((600.0, 400.0));
        assert!(view.x + view.w > 0.0 && view.y + view.h > 0.0);
    }

    #[test]
    fn reveal_pans_the_least_to_show_a_card() {
        let mut vp = Viewport::default();
        let size = (800.0, 400.0);
        let inside = Rect {
            x: 100.0,
            y: 100.0,
            w: CARD_W,
            h: CARD_H,
        };
        vp.reveal(&inside, size);
        assert_eq!(vp.origin, (0.0, 0.0));
        let below = Rect {
            x: 100.0,
            y: 1000.0,
            w: CARD_W,
            h: CARD_H,
        };
        vp.reveal(&below, size);
        assert_eq!(vp.origin.0, 0.0);
        let view = vp.world_rect(size);
        assert!(below.y >= view.y && below.y + below.h <= view.y + view.h);
        vp.reveal(&inside, size);
        let view = vp.world_rect(size);
        assert!(inside.y >= view.y);
    }

    #[test]
    fn arrows_walk_the_tree() {
        // 1 ─┬ 2 ─ 4
        //    └ 3
        // 6
        let f = forest(&[
            (1, None),
            (2, Some(1)),
            (3, Some(1)),
            (4, Some(2)),
            (6, None),
        ]);
        let l = lay(&f, &[], None);
        assert_eq!(step(&l, 1, Step::Right), Some(2));
        assert_eq!(step(&l, 2, Step::Right), Some(4));
        assert_eq!(step(&l, 4, Step::Right), None);
        assert_eq!(step(&l, 4, Step::Left), Some(2));
        assert_eq!(step(&l, 1, Step::Left), None);
        assert_eq!(step(&l, 2, Step::Down), Some(3));
        assert_eq!(step(&l, 3, Step::Up), Some(2));
        assert_eq!(step(&l, 1, Step::Down), Some(6));
        assert_eq!(step(&l, 6, Step::Down), None);
        assert_eq!(step(&l, 99, Step::Down), None);
        // Collapsed: no step into hidden children.
        let l = lay(&f, &[1], None);
        assert_eq!(step(&l, 1, Step::Right), None);
    }
}
