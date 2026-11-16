//! Message lines in the graph (spec §7, ADR 0089): which recent
//! agent-to-agent messages are drawn, how faded, and where. Gpui-free; the
//! view paints what [`lines`] returns in the edges `canvas` pass.
//!
//! The lines never feed back into the layout: they read card rectangles
//! and nothing else.

use std::collections::HashSet;

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
/// Opposite directions of a pair run this far apart, in world units.
const LANE: f32 = 3.5;

/// One message as the GUI learns it: who wrote to whom, and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub id: u64,
    pub from: u64,
    pub to: u64,
    pub at_ms: i64,
}

/// A line to draw: three world-space segments (the line and two arrowhead
/// barbs) and its fade step (0 newest … [`STEPS`]` - 1` oldest).
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub from: u64,
    pub to: u64,
    pub step: usize,
    pub segments: [[(f32, f32); 2]; 3],
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
/// still in the window, newest pairs first, at most [`MAX_LINES`], between
/// agents `shown` accepts. Self-messages draw nothing.
pub fn recent_pairs(
    edges: &[Edge],
    now_ms: i64,
    shown: &dyn Fn(u64) -> bool,
) -> Vec<(u64, u64, usize)> {
    let mut seen = HashSet::new();
    let mut newest: Vec<&Edge> = edges
        .iter()
        .filter(|e| e.from != e.to && step_of(now_ms - e.at_ms).is_some())
        .filter(|e| shown(e.from) && shown(e.to))
        .collect();
    // Newest first (ids only grow), so the first of a pair is its newest.
    newest.sort_by_key(|e| std::cmp::Reverse(e.id));
    newest
        .into_iter()
        .filter(|e| seen.insert((e.from, e.to)))
        .take(MAX_LINES)
        .map(|e| (e.from, e.to, step_of(now_ms - e.at_ms).unwrap_or(0)))
        .collect()
}

/// Where `ray` from the centre of `r` towards `target` leaves `r`.
fn border_point(r: &Rect, target: (f32, f32)) -> (f32, f32) {
    let c = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    let (dx, dy) = (target.0 - c.0, target.1 - c.1);
    if dx == 0.0 && dy == 0.0 {
        return c;
    }
    let sx = if dx == 0.0 {
        f32::MAX
    } else {
        (r.w / 2.0) / dx.abs()
    };
    let sy = if dy == 0.0 {
        f32::MAX
    } else {
        (r.h / 2.0) / dy.abs()
    };
    let s = sx.min(sy);
    (c.0 + dx * s, c.1 + dy * s)
}

/// The segments of a line from card `a` to card `b`: border to border,
/// shifted sideways by [`LANE`] (to the right of its direction, so the
/// reverse direction runs beside it), with an arrowhead at `b`.
pub fn route(a: &Rect, b: &Rect) -> [[(f32, f32); 2]; 3] {
    let ca = (a.x + a.w / 2.0, a.y + a.h / 2.0);
    let cb = (b.x + b.w / 2.0, b.y + b.h / 2.0);
    let (dx, dy) = (cb.0 - ca.0, cb.1 - ca.1);
    let len = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
    let (ux, uy) = (dx / len, dy / len);
    // Unit normal on the right of the direction of travel.
    let (nx, ny) = (-uy, ux);
    let off = (nx * LANE, ny * LANE);
    let shift = |p: (f32, f32)| (p.0 + off.0, p.1 + off.1);
    let start = shift(border_point(a, cb));
    let tip = shift(border_point(b, ca));
    let barb = |sign: f32| {
        // Back from the tip along the line, then sideways.
        let (bx, by) = (-ux * BARB, -uy * BARB);
        let (sx, sy) = (nx * BARB * 0.5 * sign, ny * BARB * 0.5 * sign);
        [tip, (tip.0 + bx + sx, tip.1 + by + sy)]
    };
    [[start, tip], barb(1.0), barb(-1.0)]
}

/// The lines to draw now: the recent pairs between cards the layout has,
/// that touch `view` (world space). Only these are painted: culled like
/// the cards and edges.
pub fn lines(layout: &GraphLayout, edges: &[Edge], now_ms: i64, view: &Rect) -> Vec<Line> {
    recent_pairs(edges, now_ms, &|id| layout.card(id).is_some())
        .into_iter()
        .filter_map(|(from, to, step)| {
            let (a, b) = (layout.card(from)?, layout.card(to)?);
            let segments = route(&a.rect, &b.rect);
            let (xs, ys) = (segments[0].map(|p| p.0), segments[0].map(|p| p.1));
            let bounds = Rect {
                x: xs[0].min(xs[1]),
                y: ys[0].min(ys[1]),
                w: (xs[0] - xs[1]).abs(),
                h: (ys[0] - ys[1]).abs(),
            };
            intersects(&bounds, view).then_some(Line {
                from,
                to,
                step,
                segments,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: u64, from: u64, to: u64, at_ms: i64) -> Edge {
        Edge {
            id,
            from,
            to,
            at_ms,
        }
    }

    fn rect(x: f32, y: f32) -> Rect {
        Rect {
            x,
            y,
            w: 100.0,
            h: 50.0,
        }
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
        assert_eq!(got, [(2, 1, 0), (1, 2, 0)]);
    }

    #[test]
    fn lines_are_capped_to_the_newest_pairs() {
        let now = 10_000;
        let edges: Vec<Edge> = (1..=100)
            .map(|i| e(i, i, i + 1000, now - i as i64))
            .collect();
        let got = recent_pairs(&edges, now, &|_| true);
        assert_eq!(got.len(), MAX_LINES);
        assert_eq!(got[0].0, 100, "newest first");
    }

    #[test]
    fn hidden_cards_draw_no_line() {
        let edges = [e(1, 1, 2, 0), e(2, 1, 3, 0)];
        assert_eq!(recent_pairs(&edges, 0, &|id| id != 3), [(1, 2, 0)]);
    }

    #[test]
    fn a_line_runs_border_to_border_with_an_arrowhead() {
        let (a, b) = (rect(0.0, 0.0), rect(300.0, 0.0));
        let [line, b1, b2] = route(&a, &b);
        // Level cards: the line leaves a's right edge and ends on b's left,
        // shifted down by the lane (right of travel when going right).
        assert!((line[0].0 - 100.0).abs() < 0.01 && (line[1].0 - 300.0).abs() < 0.01);
        assert!((line[0].1 - (25.0 + LANE)).abs() < 0.01);
        // Both barbs start at the tip and lie behind it.
        assert_eq!(b1[0], line[1]);
        assert_eq!(b2[0], line[1]);
        assert!(b1[1].0 < line[1].0 && b2[1].0 < line[1].0);
        assert!(b1[1].1 != b2[1].1);
    }

    #[test]
    fn opposite_directions_run_side_by_side() {
        let (a, b) = (rect(0.0, 0.0), rect(300.0, 0.0));
        let there = route(&a, &b)[0];
        let back = route(&b, &a)[0];
        assert!((there[0].1 - back[0].1).abs() > LANE);
    }

    #[test]
    fn a_vertical_pair_leaves_by_the_top_and_bottom() {
        let (a, b) = (rect(0.0, 0.0), rect(0.0, 200.0));
        let line = route(&a, &b)[0];
        assert!((line[0].1 - 50.0).abs() < 0.01 && (line[1].1 - 200.0).abs() < 0.01);
    }
}
