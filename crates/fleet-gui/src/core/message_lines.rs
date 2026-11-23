//! Message lines in the graph (spec §7, ADRs 0089 and 0106): which recent
//! agent-to-agent messages are drawn, how faded, along which route, and
//! which one the pointer is on. Gpui-free; the view paints the routes in
//! the edges `canvas` pass.
//!
//! **Routing (ADR 0106).** A line is an orthogonal path that never enters
//! a card. The band layout leaves two kinds of free channel: the gap
//! between two columns (columns line up across every project box, so a
//! column gap is free from top to bottom) and the gap between two rows (no
//! card spans it, in any column). A line leaves its sender by the side
//! facing the route, runs in a *lane* of a column gap (offset from the
//! gap's middle, where the tree's elbows run), crosses to another column
//! gap along a row gap when it must pass a column, and enters the
//! recipient by the side facing it, with the arrowhead on that border.
//! Lines sharing a gap get different lanes, lines sharing a card side
//! different ports (never the middle, where tree edges attach).
//!
//! The lines never feed back into the layout: they read card rectangles
//! and nothing else. Routes are computed again only when the layout or the
//! set of drawn pairs changes ([`RouteCache`]).

use std::collections::{HashMap, HashSet};

use super::graph_layout::metrics::{CARD_H, CARD_W, COL_GAP, ROW_GAP};
use super::graph_layout::{GraphLayout, intersects};
use super::layout::Rect;

/// A message draws a line for this long.
pub const WINDOW_MS: i64 = 5 * 60_000;
/// At most this many lines are drawn (the newest pairs).
pub const MAX_LINES: usize = 24;
/// Fade steps: lines of one step share one path (one `paint_path`).
pub const STEPS: usize = 3;
/// Barbs of the arrowhead, in world units.
const BARB: f32 = 7.0;
/// A line is under the pointer within this many screen pixels.
pub const HIT_PX: f32 = 5.0;
/// Lanes in a column gap: offsets from its middle (the tree's elbows).
/// The two directions of a pair take two of them, so they run apart.
const COL_LANES: [f32; 8] = [6.0, -6.0, 9.0, -9.0, 12.0, -12.0, 15.0, -15.0];
/// Lanes in a row gap: offsets from its middle.
const ROW_LANES: [f32; 5] = [0.0, -2.5, 2.5, -4.5, 4.5];
/// Ports on a card side: offsets from its middle (where tree edges attach).
const PORTS: [f32; 8] = [14.0, -14.0, 22.0, -22.0, 30.0, -30.0, 36.0, -36.0];

/// What a message is (the kinds that draw a line, ADR 0089).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Kind {
    #[default]
    Message,
    Question,
    Answer,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Message => "message",
            Kind::Question => "question",
            Kind::Answer => "answer",
        }
    }
}

/// One message as the GUI learns it: who wrote to whom, what and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub id: u64,
    pub from: u64,
    pub to: u64,
    pub kind: Kind,
    pub at_ms: i64,
}

/// A directed pair worth a line: its newest message in the window, and how
/// many it sent in the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pair {
    pub from: u64,
    pub to: u64,
    /// Fade step of the newest message (0 newest … [`STEPS`]` - 1`).
    pub step: usize,
    pub kind: Kind,
    pub at_ms: i64,
    pub count: usize,
}

/// The fade step of a message `age_ms` old: 0 while fresh, up to
/// [`STEPS`]` - 1` near the end of the window; `None` once expired (or from
/// the future by a clock slip: then it is fresh).
pub fn step_of(age_ms: i64) -> Option<usize> {
    if age_ms >= WINDOW_MS {
        return None;
    }
    let age = age_ms.max(0);
    Some((age as usize * STEPS / WINDOW_MS as usize).min(STEPS - 1))
}

/// The opacity of fade step `step`, for a base colour.
pub fn opacity_of(step: usize) -> f32 {
    [0.9, 0.55, 0.25][step.min(STEPS - 1)]
}

/// The pairs worth drawing: for each directed pair its newest message
/// still in the window (and the count in the window), newest pairs first,
/// at most [`MAX_LINES`], between agents `shown` accepts. Self-messages
/// draw nothing.
pub fn recent_pairs(edges: &[Edge], now_ms: i64, shown: &dyn Fn(u64) -> bool) -> Vec<Pair> {
    let mut live: Vec<&Edge> = edges
        .iter()
        .filter(|e| e.from != e.to && step_of(now_ms - e.at_ms).is_some())
        .filter(|e| shown(e.from) && shown(e.to))
        .collect();
    // Newest first (ids only grow), so the first of a pair is its newest.
    live.sort_by_key(|e| std::cmp::Reverse(e.id));
    let mut counts: HashMap<(u64, u64), usize> = HashMap::new();
    for e in &live {
        *counts.entry((e.from, e.to)).or_default() += 1;
    }
    let mut seen = HashSet::new();
    live.into_iter()
        .filter(|e| seen.insert((e.from, e.to)))
        .take(MAX_LINES)
        .map(|e| Pair {
            from: e.from,
            to: e.to,
            step: step_of(now_ms - e.at_ms).unwrap_or(0),
            kind: e.kind,
            at_ms: e.at_ms,
            count: counts[&(e.from, e.to)],
        })
        .collect()
}

/// A routed line in world space: a polyline from the sender's border to
/// the recipient's, and the two barbs of its arrowhead (each from the tip).
#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    pub from: u64,
    pub to: u64,
    pub points: Vec<(f32, f32)>,
    pub barbs: [[(f32, f32); 2]; 2],
    pub bounds: Rect,
}

impl Route {
    /// The straight pieces of the line (not the barbs).
    pub fn segments(&self) -> impl Iterator<Item = [(f32, f32); 2]> + '_ {
        self.points.windows(2).map(|w| [w[0], w[1]])
    }
}

/// Hands out lanes (or ports) per key in turn, wrapping when a key runs
/// out.
#[derive(Default)]
struct Slots(HashMap<(u8, i64, i64), usize>);

impl Slots {
    fn take(&mut self, kind: u8, key: (f32, f32), of: &[f32]) -> f32 {
        let key = (kind, key.0.round() as i64, key.1.round() as i64);
        let n = self.0.entry(key).or_default();
        let v = of[*n % of.len()];
        *n += 1;
        v
    }
}

const COL_GAP_KEY: u8 = 0;
const ROW_GAP_KEY: u8 = 1;
const PORT_KEY: u8 = 2;

/// Routes every pair whose two cards the layout has. Lanes and ports are
/// given out in `(from, to)` order, so the same set of pairs always gets
/// the same routes.
pub fn route_all(layout: &GraphLayout, pairs: &[(u64, u64)]) -> Vec<Route> {
    let mut sorted: Vec<(u64, u64)> = pairs.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut slots = Slots::default();
    sorted
        .into_iter()
        .filter_map(|(from, to)| {
            let (a, b) = (layout.card(from)?, layout.card(to)?);
            (from != to).then(|| route(from, to, &a.rect, &b.rect, &mut slots))
        })
        .collect()
}

/// The middle of the column gap right of `r` (`right`) or left of it.
fn gap_mid(r: &Rect, right: bool) -> f32 {
    if right {
        r.x + r.w + COL_GAP / 2.0
    } else {
        r.x - COL_GAP / 2.0
    }
}

/// A port on the side of `r` facing right (`right`) or left: one per line
/// using that side, in turn.
fn port(slots: &mut Slots, r: &Rect, right: bool) -> (f32, f32) {
    let x = if right { r.x + r.w } else { r.x };
    (x, r.y + r.h / 2.0 + slots.take(PORT_KEY, (x, r.y), &PORTS))
}

fn lane(slots: &mut Slots, mid: f32) -> f32 {
    mid + slots.take(COL_GAP_KEY, (mid, 0.0), &COL_LANES)
}

fn route(from: u64, to: u64, a: &Rect, b: &Rect, slots: &mut Slots) -> Route {
    // Columns: the band layout puts every column at the same x in every
    // box, so comparing x compares columns.
    let (ca, cb) = (a.x, b.x);
    let same = (ca - cb).abs() < 1.0;
    let rightward = cb > ca + 1.0;
    // Which side of each card faces the route, and the gap it opens on.
    // Same column: both use the gap on their right.
    let (a_right, b_right) = if same {
        (true, true)
    } else if rightward {
        (true, false)
    } else {
        (false, true)
    };
    let start = port(slots, a, a_right);
    let tip = port(slots, b, b_right);
    let (mid_a, mid_b) = (gap_mid(a, a_right), gap_mid(b, b_right));
    let mut points = vec![start];
    if (mid_a - mid_b).abs() < 1.0 {
        // One gap: down (or up) a lane in it.
        let x = lane(slots, mid_a);
        points.extend([(x, start.1), (x, tip.1)]);
    } else {
        // Across columns: along the row gap next to the recipient, on the
        // side the sender is.
        let (xa, xb) = (lane(slots, mid_a), lane(slots, mid_b));
        let gap = if start.1 < b.y {
            b.y - ROW_GAP / 2.0
        } else {
            b.y + b.h + ROW_GAP / 2.0
        };
        let y = gap + slots.take(ROW_GAP_KEY, (gap, 0.0), &ROW_LANES);
        points.extend([(xa, start.1), (xa, y), (xb, y), (xb, tip.1)]);
    }
    points.push(tip);
    // The last piece is level; the barbs point back along it.
    let back = if b_right { BARB } else { -BARB };
    let barbs = [
        [tip, (tip.0 + back, tip.1 - BARB * 0.5)],
        [tip, (tip.0 + back, tip.1 + BARB * 0.5)],
    ];
    let all = points.iter().chain(barbs.iter().map(|b| &b[1]));
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in all {
        (x0, y0, x1, y1) = (x0.min(p.0), y0.min(p.1), x1.max(p.0), y1.max(p.1));
    }
    Route {
        from,
        to,
        points,
        barbs,
        bounds: Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        },
    }
}

/// Routes, kept until the layout or the set of drawn pairs changes: a frame
/// with neither change costs one comparison.
#[derive(Default)]
pub struct RouteCache {
    key: Option<(usize, Vec<(u64, u64)>)>,
    routes: Vec<Route>,
}

impl RouteCache {
    /// The routes for `pairs` over the layout numbered `generation` (a
    /// number that changes whenever the layout does).
    pub fn get(&mut self, generation: usize, layout: &GraphLayout, pairs: &[Pair]) -> &[Route] {
        let mut ids: Vec<(u64, u64)> = pairs.iter().map(|p| (p.from, p.to)).collect();
        ids.sort_unstable();
        let key = (generation, ids);
        if self.key.as_ref() != Some(&key) {
            self.routes = route_all(layout, &key.1);
            self.key = Some(key);
        }
        &self.routes
    }

    /// The routes as last computed.
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    pub fn clear(&mut self) {
        self.key = None;
        self.routes.clear();
    }
}

/// The routes that touch `view` (world space): culled like the cards.
pub fn in_view<'a>(routes: &'a [Route], view: &'a Rect) -> impl Iterator<Item = &'a Route> {
    routes.iter().filter(move |r| intersects(&r.bounds, view))
}

fn dist_to_segment(p: (f32, f32), s: [(f32, f32); 2]) -> f32 {
    let (a, b) = (s[0], s[1]);
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    let (qx, qy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - qx).powi(2) + (p.1 - qy).powi(2)).sqrt()
}

fn inside(p: (f32, f32), r: &Rect) -> bool {
    p.0 > r.x && p.0 < r.x + r.w && p.1 > r.y && p.1 < r.y + r.h
}

/// What the pointer at `p` (world) is over: a card, else the nearest line
/// within `tolerance` (world units: [`HIT_PX`] over the zoom). A card
/// always wins, so hovering a card never hovers a line and the reverse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hover {
    Card(u64),
    Line { from: u64, to: u64 },
}

pub fn hover_at(
    layout: &GraphLayout,
    routes: &[Route],
    p: (f32, f32),
    tolerance: f32,
) -> Option<Hover> {
    let probe = Rect {
        x: p.0,
        y: p.1,
        w: 0.0,
        h: 0.0,
    };
    if let Some(c) = layout.cards_in(&probe).find(|c| inside(p, &c.rect)) {
        return Some(Hover::Card(c.id));
    }
    routes
        .iter()
        .filter_map(|r| {
            let d = r
                .segments()
                .map(|s| dist_to_segment(p, s))
                .fold(f32::MAX, f32::min);
            (d <= tolerance).then_some((d, r))
        })
        .min_by(|x, y| x.0.total_cmp(&y.0))
        .map(|(_, r)| Hover::Line {
            from: r.from,
            to: r.to,
        })
}

/// How long ago, for a tooltip: "just now", "1 min ago", "4 min ago".
pub fn age_label(age_ms: i64) -> String {
    match age_ms.max(0) / 60_000 {
        0 => "just now".to_owned(),
        m => format!("{m} min ago"),
    }
}

/// The tooltip of a hovered line: `ui2-167 → fleetd-wolf · message · 2 min
/// ago`, and `· 3 in 5 min` when the pair sent several.
pub fn tooltip(from: &str, to: &str, pair: &Pair, now_ms: i64) -> String {
    let mut s = format!(
        "{from} → {to} · {} · {}",
        pair.kind.label(),
        age_label(now_ms - pair.at_ms)
    );
    if pair.count > 1 {
        s.push_str(&format!(" · {} in 5 min", pair.count));
    }
    s
}

/// Card sizes the routing relies on (a column gap fits the lanes, a card
/// side fits the ports).
const _: () = {
    assert!(COL_LANES[7].abs() + BARB < COL_GAP / 2.0);
    assert!(PORTS[7].abs() < CARD_H / 2.0);
    assert!(ROW_LANES[4].abs() < ROW_GAP / 2.0);
    assert!(CARD_W > 0.0);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::graph::tree_by_id;
    use crate::core::graph_layout::{Card, LayoutInput, layout};

    fn e(id: u64, from: u64, to: u64, at_ms: i64) -> Edge {
        Edge {
            id,
            from,
            to,
            kind: Kind::Message,
            at_ms,
        }
    }

    fn lay(nodes: &[(u64, Option<u64>)], project: &dyn Fn(u64) -> String) -> GraphLayout {
        let forest = tree_by_id(nodes.iter().copied());
        let collapsed = HashSet::new();
        let roots: HashMap<u64, u64> = nodes.iter().map(|n| (n.0, forest.root_of(n.0))).collect();
        let by_root = |id: u64| project(roots[&id]);
        layout(&LayoutInput {
            forest: &forest,
            machine: "This Mac",
            project: &by_root,
            collapsed: &collapsed,
            shown: None,
        })
    }

    /// The issue's tree: a parent with six children (some with their own
    /// children), a second root, and a tree in another project.
    fn fleet() -> GraphLayout {
        lay(
            &[
                (1, None),
                (2, Some(1)),
                (3, Some(1)),
                (4, Some(1)),
                (5, Some(1)),
                (6, Some(1)),
                (7, Some(1)),
                (8, Some(2)),
                (9, Some(2)),
                (10, Some(4)),
                (11, Some(10)),
                (12, None),
                (13, Some(12)),
                (20, None),
                (21, Some(20)),
                (22, Some(21)),
            ],
            &|root| if root == 20 { "b".into() } else { "a".into() },
        )
    }

    /// Whether segment `s` passes through the inside of `r` (touching its
    /// border does not count).
    fn crosses(s: [(f32, f32); 2], r: &Rect) -> bool {
        let eps = 0.01;
        let (x0, x1) = (s[0].0.min(s[1].0), s[0].0.max(s[1].0));
        let (y0, y1) = (s[0].1.min(s[1].1), s[0].1.max(s[1].1));
        // Orthogonal segments: overlap of open intervals.
        x1 > r.x + eps && x0 < r.x + r.w - eps && y1 > r.y + eps && y0 < r.y + r.h - eps
    }

    fn assert_clear(l: &GraphLayout, pairs: &[(u64, u64)]) {
        let routes = route_all(l, pairs);
        assert_eq!(routes.len(), pairs.len());
        for r in &routes {
            for s in r.segments().chain(r.barbs) {
                assert!(
                    s[0].0 == s[1].0 || s[0].1 == s[1].1 || r.barbs.contains(&s),
                    "orthogonal: {s:?}"
                );
                for c in &l.cards {
                    assert!(
                        !crosses(s, &c.rect),
                        "{}→{}: {s:?} crosses card {}",
                        r.from,
                        r.to,
                        c.id
                    );
                }
            }
        }
    }

    fn card(l: &GraphLayout, id: u64) -> &Card {
        l.card(id).unwrap()
    }

    #[test]
    fn siblings_route_around_the_cards_between_them() {
        let l = fleet();
        // Bottom sibling to the top one, past four siblings (and 10's
        // subtree, which pushes 5…7 down).
        assert_clear(&l, &[(7, 2), (2, 7), (3, 5), (5, 3)]);
    }

    #[test]
    fn child_and_parent_route_around_cards() {
        let l = fleet();
        // The issue's case: the bottom of six children reports to its
        // parent; and the parent writes back. Also a grandchild.
        assert_clear(&l, &[(7, 1), (1, 7), (11, 10), (10, 11), (11, 1), (1, 11)]);
    }

    #[test]
    fn cousins_and_other_trees_route_around_cards() {
        let l = fleet();
        assert_clear(
            &l,
            &[(8, 10), (10, 8), (9, 13), (13, 9), (11, 12), (12, 11)],
        );
    }

    #[test]
    fn cross_project_pairs_route_around_cards() {
        let l = fleet();
        assert_clear(
            &l,
            &[(22, 7), (7, 22), (21, 1), (1, 21), (20, 11), (11, 20)],
        );
    }

    #[test]
    fn every_pair_of_the_fleet_routes_clear_at_once() {
        let l = fleet();
        let ids: Vec<u64> = l.cards.iter().map(|c| c.id).collect();
        let pairs: Vec<(u64, u64)> = ids
            .iter()
            .flat_map(|&a| ids.iter().map(move |&b| (a, b)))
            .filter(|(a, b)| a != b)
            .collect();
        assert_clear(&l, &pairs);
    }

    #[test]
    fn a_line_leaves_and_enters_by_the_sides_facing_the_route() {
        let l = fleet();
        // Child to parent: out of the child's left side, into the parent's
        // right side, arrowhead pointing left into the parent.
        let r = &route_all(&l, &[(7, 1)])[0];
        let (c, p) = (card(&l, 7).rect, card(&l, 1).rect);
        let (start, tip) = (r.points[0], *r.points.last().unwrap());
        assert_eq!(start.0, c.x);
        assert!(start.1 > c.y && start.1 < c.y + c.h);
        assert_eq!(tip.0, p.x + p.w);
        assert!(tip.1 > p.y && tip.1 < p.y + p.h);
        assert!(r.barbs.iter().all(|b| b[0] == tip && b[1].0 > tip.0));
        // Parent to child: the other way.
        let r = &route_all(&l, &[(1, 7)])[0];
        let tip = *r.points.last().unwrap();
        assert_eq!(tip.0, c.x);
        assert!(r.barbs.iter().all(|b| b[1].0 < tip.0));
    }

    #[test]
    fn lines_keep_off_the_tree_edges() {
        let l = fleet();
        let routes = route_all(&l, &[(7, 1), (1, 7), (2, 7), (11, 1)]);
        for r in &routes {
            for s in r.segments() {
                for t in &l.edges {
                    for w in t.points.windows(2) {
                        // Parallel and on the same line (within 3 units),
                        // overlapping: running along a tree edge.
                        let vertical = s[0].0 == s[1].0 && w[0].0 == w[1].0;
                        let level = s[0].1 == s[1].1 && w[0].1 == w[1].1;
                        let along = if vertical {
                            (s[0].0 - w[0].0).abs() < 3.0
                                && s[0].1.max(s[1].1) > w[0].1.min(w[1].1)
                                && s[0].1.min(s[1].1) < w[0].1.max(w[1].1)
                        } else if level {
                            (s[0].1 - w[0].1).abs() < 3.0
                                && s[0].0.max(s[1].0) > w[0].0.min(w[1].0)
                                && s[0].0.min(s[1].0) < w[0].0.max(w[1].0)
                        } else {
                            false
                        };
                        assert!(!along, "{}→{} {s:?} along tree {w:?}", r.from, r.to);
                    }
                }
            }
        }
    }

    #[test]
    fn lines_sharing_a_gap_get_separate_lanes() {
        let l = fleet();
        // Both directions of a pair, and two more into the same parent.
        let routes = route_all(&l, &[(7, 1), (1, 7), (6, 1), (5, 1)]);
        let lanes: Vec<f32> = routes.iter().map(|r| r.points[1].0).collect();
        let distinct: HashSet<i64> = lanes.iter().map(|x| (x * 10.0) as i64).collect();
        assert_eq!(distinct.len(), lanes.len(), "lanes {lanes:?}");
        // And the ports on the parent's right side differ too.
        let tips: HashSet<i64> = routes
            .iter()
            .filter(|r| r.to == 1)
            .map(|r| r.points.last().unwrap().1 as i64)
            .collect();
        assert_eq!(tips.len(), 3);
    }

    #[test]
    fn routes_are_the_same_whatever_the_pair_order() {
        let l = fleet();
        let a = route_all(&l, &[(7, 1), (1, 7), (8, 10)]);
        let b = route_all(&l, &[(8, 10), (1, 7), (7, 1)]);
        assert_eq!(a, b);
    }

    #[test]
    fn missing_cards_route_nothing() {
        let l = fleet();
        assert!(route_all(&l, &[(1, 99), (99, 1), (1, 1)]).is_empty());
    }

    #[test]
    fn the_cache_routes_again_only_on_a_change() {
        let l = fleet();
        let p = |from, to| Pair {
            from,
            to,
            step: 0,
            kind: Kind::Message,
            at_ms: 0,
            count: 1,
        };
        let mut cache = RouteCache::default();
        let first = cache.get(1, &l, &[p(7, 1)]).to_vec();
        assert_eq!(first.len(), 1);
        // Same generation, same pairs (a step changed): same routes.
        let mut q = p(7, 1);
        q.step = 2;
        assert_eq!(cache.get(1, &l, &[q]), first.as_slice());
        assert_eq!(cache.get(1, &l, &[p(7, 1), p(1, 7)]).len(), 2);
        let other = lay(&[(1, None), (7, Some(1))], &|_| "a".into());
        let moved = cache.get(2, &other, &[p(7, 1)]).to_vec();
        assert_ne!(moved, first);
    }

    #[test]
    fn hovering_finds_a_line_within_the_tolerance_and_cards_win() {
        let l = fleet();
        let routes = route_all(&l, &[(7, 1)]);
        let r = &routes[0];
        // The middle of the long vertical piece.
        let (a, b) = (r.points[1], r.points[2]);
        let mid = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
        let line = Some(Hover::Line { from: 7, to: 1 });
        assert_eq!(hover_at(&l, &routes, mid, 4.0), line);
        assert_eq!(hover_at(&l, &routes, (mid.0 + 3.5, mid.1), 4.0), line);
        assert_eq!(hover_at(&l, &routes, (mid.0 + 4.5, mid.1), 4.0), None);
        // At 25 % zoom the same 5 px is 20 world units.
        assert_eq!(
            hover_at(&l, &routes, (mid.0 + 15.0, mid.1), HIT_PX / 0.25),
            line
        );
        // Inside a card: the card, never the line (its port is on the
        // border).
        let c = card(&l, 7).rect;
        let start = r.points[0];
        assert_eq!(
            hover_at(&l, &routes, (start.0 + 2.0, start.1), 4.0),
            Some(Hover::Card(7))
        );
        assert_eq!(
            hover_at(&l, &routes, (c.x + 50.0, c.y + 10.0), 4.0),
            Some(Hover::Card(7))
        );
    }

    #[test]
    fn culling_keeps_routes_that_touch_the_view() {
        let l = fleet();
        let routes = route_all(&l, &[(7, 1), (22, 21)]);
        // A small view just left of 22, where its line leaves.
        let c = card(&l, 22).rect;
        let view = Rect {
            x: c.x - 10.0,
            y: c.y,
            w: 10.0,
            h: c.h,
        };
        let got: Vec<(u64, u64)> = in_view(&routes, &view).map(|r| (r.from, r.to)).collect();
        assert_eq!(got, [(22, 21)]);
    }

    #[test]
    fn the_tooltip_says_who_what_and_when() {
        let pair = Pair {
            from: 1,
            to: 2,
            step: 0,
            kind: Kind::Message,
            at_ms: 0,
            count: 1,
        };
        assert_eq!(
            tooltip("ui2-167", "fleetd-wolf", &pair, 2 * 60_000 + 5_000),
            "ui2-167 → fleetd-wolf · message · 2 min ago"
        );
        let several = Pair {
            kind: Kind::Question,
            count: 3,
            ..pair
        };
        assert_eq!(
            tooltip("a", "b", &several, 10_000),
            "a → b · question · just now · 3 in 5 min"
        );
        assert_eq!(age_label(-1), "just now");
    }

    #[test]
    fn fading_goes_from_fresh_to_gone() {
        assert_eq!(step_of(0), Some(0));
        assert_eq!(step_of(-5_000), Some(0), "a clock slip is fresh");
        assert_eq!(step_of(WINDOW_MS / 2), Some(1));
        assert_eq!(step_of(WINDOW_MS - 1), Some(STEPS - 1));
        assert_eq!(step_of(WINDOW_MS), None);
        assert!(opacity_of(0) > opacity_of(1) && opacity_of(1) > opacity_of(2));
    }

    fn ids(p: &[Pair]) -> Vec<(u64, u64, usize)> {
        p.iter().map(|p| (p.from, p.to, p.step)).collect()
    }

    #[test]
    fn a_pair_keeps_its_newest_message_and_expired_ones_drop() {
        let now = 1_000_000;
        let edges = [
            e(1, 1, 2, now - 200_000),
            e(2, 1, 2, now - 1_000),
            e(3, 2, 1, now - 2_000),
            e(4, 3, 1, now - WINDOW_MS - 1),
            e(5, 4, 4, now),
        ];
        let got = recent_pairs(&edges, now, &|_| true);
        // (2,1) is a different pair from (1,2); the expired and the
        // self-message are gone; newest first.
        assert_eq!(ids(&got), [(2, 1, 0), (1, 2, 0)]);
        assert_eq!(got[1].count, 2, "both of 1→2's messages are in the window");
        assert_eq!(got[1].at_ms, now - 1_000, "the newest one's time");
        assert_eq!(got[0].count, 1);
    }

    #[test]
    fn lines_are_capped_to_the_newest_pairs() {
        let now = 10_000;
        let edges: Vec<Edge> = (1..=100)
            .map(|i| e(i, i, i + 1000, now - i as i64))
            .collect();
        let got = recent_pairs(&edges, now, &|_| true);
        assert_eq!(got.len(), MAX_LINES);
        assert_eq!(got[0].from, 100, "newest first");
    }

    #[test]
    fn hidden_cards_draw_no_line() {
        let edges = [e(1, 1, 2, 0), e(2, 1, 3, 0)];
        assert_eq!(ids(&recent_pairs(&edges, 0, &|id| id != 3)), [(1, 2, 0)]);
    }
}
