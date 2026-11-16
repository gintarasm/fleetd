//! Drag and drop of agents and panes (ADR 0059): a sidebar row or a pane
//! header is pressed, the pointer travels past [`DRAG_THRESHOLD`], and
//! where it is released decides what happens. A pane's middle shows or
//! swaps; a band along each of its edges splits or moves beside it. The
//! shell only feeds pointer positions in and draws [`highlight`].

use super::layout::{Dir, Rect};
use super::session::PaneId;
use super::style::chrome;

/// How far the pointer travels, in px, before a press becomes a drag (a
/// shorter one stays a click).
pub const DRAG_THRESHOLD: f32 = 4.0;

/// The share of a pane's width (height) that is its left or right (top
/// or bottom) drop band.
pub const EDGE_SHARE: f32 = 0.25;

/// What is being dragged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Payload {
    /// A sidebar row.
    Agent(u64),
    /// A pane, by its header.
    Pane(PaneId),
}

/// Where in a pane a drop lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    /// The middle: show there, or swap.
    Center,
    /// A band along an edge: split there, or move beside.
    Edge(Dir),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropTarget {
    pub pane: PaneId,
    pub zone: Zone,
}

/// What a drop does to the panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropAction {
    /// Show `agent` in `pane`.
    Show { pane: PaneId, agent: u64 },
    /// Split `pane`, the new pane on its `dir` side showing `agent`.
    SplitShow { pane: PaneId, dir: Dir, agent: u64 },
    /// Swap two panes.
    Swap { pane: PaneId, with: PaneId },
    /// Move `pane` beside `target`, on its `dir` side.
    Move {
        pane: PaneId,
        target: PaneId,
        dir: Dir,
    },
}

/// A press that may become a drag.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Drag {
    pub payload: Payload,
    from: (f32, f32),
    active: bool,
    /// Where the pointer is now (after the threshold).
    target: Option<DropTarget>,
}

impl Drag {
    pub fn press(payload: Payload, x: f32, y: f32) -> Drag {
        Drag {
            payload,
            from: (x, y),
            active: false,
            target: None,
        }
    }

    /// The pointer moved to `x`, `y`; `true` once it is a drag.
    pub fn moved(&mut self, x: f32, y: f32) -> bool {
        if !self.active {
            let (dx, dy) = (x - self.from.0, y - self.from.1);
            self.active = dx.hypot(dy) >= DRAG_THRESHOLD;
        }
        self.active
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn target(&self) -> Option<DropTarget> {
        self.target
    }

    /// Follows the pointer at `x`, `y` over the drawn `panes` (no target
    /// before the threshold; `can_split`: a pane can be added); `true` if
    /// the target changed.
    pub fn retarget(&mut self, panes: &[(PaneId, Rect)], can_split: bool, x: f32, y: f32) -> bool {
        let t = if self.active {
            target_at(panes, can_split, self.payload, x, y)
        } else {
            None
        };
        let changed = t != self.target;
        self.target = t;
        changed
    }

    /// Released: what to do, if it was a drag over a target.
    pub fn release(self) -> Option<DropAction> {
        if !self.active {
            return None;
        }
        action(self.payload, self.target?)
    }
}

/// Where `x`, `y` falls in `r`: the band along the nearest edge if
/// within [`EDGE_SHARE`] of it, else the middle; `None` outside.
pub fn zone_at(r: Rect, x: f32, y: f32) -> Option<Zone> {
    if !(r.w > 0.0 && r.h > 0.0) || x < r.x || y < r.y || x >= r.x + r.w || y >= r.y + r.h {
        return None;
    }
    let (u, v) = ((x - r.x) / r.w, (y - r.y) / r.h);
    // Each edge's distance as a share of the pane's extent across it.
    let edges = [
        (u, Dir::Left),
        (1.0 - u, Dir::Right),
        (v, Dir::Up),
        (1.0 - v, Dir::Down),
    ];
    let (d, dir) = edges
        .into_iter()
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .expect("four edges");
    Some(if d < EDGE_SHARE {
        Zone::Edge(dir)
    } else {
        Zone::Center
    })
}

/// The drop target under `x`, `y` among the drawn `panes`. A pane is
/// never its own target; an agent's edge is its middle unless a pane can
/// be added (`can_split`: the layout, not just the panes drawn, is below
/// `MAX_PANES`).
pub fn target_at(
    panes: &[(PaneId, Rect)],
    can_split: bool,
    payload: Payload,
    x: f32,
    y: f32,
) -> Option<DropTarget> {
    let (pane, zone) = panes
        .iter()
        .find_map(|(p, r)| Some((*p, zone_at(*r, x, y)?)))?;
    let zone = match payload {
        Payload::Pane(src) if src == pane => return None,
        Payload::Agent(_) if !can_split => Zone::Center,
        _ => zone,
    };
    Some(DropTarget { pane, zone })
}

/// What dropping `payload` on `target` does; `None` for a pane on itself.
pub fn action(payload: Payload, target: DropTarget) -> Option<DropAction> {
    let pane = target.pane;
    Some(match (payload, target.zone) {
        (Payload::Agent(agent), Zone::Center) => DropAction::Show { pane, agent },
        (Payload::Agent(agent), Zone::Edge(dir)) => DropAction::SplitShow { pane, dir, agent },
        (Payload::Pane(src), _) if src == pane => return None,
        (Payload::Pane(src), Zone::Center) => DropAction::Swap {
            pane: src,
            with: pane,
        },
        (Payload::Pane(src), Zone::Edge(dir)) => DropAction::Move {
            pane: src,
            target: pane,
            dir,
        },
    })
}

/// The area to highlight for `zone` of a pane at `r`: all of it for the
/// middle, the half a new or moved pane would take for an edge.
pub fn highlight(r: Rect, zone: Zone) -> Rect {
    match zone {
        Zone::Center => r,
        Zone::Edge(Dir::Left) => Rect { w: r.w / 2.0, ..r },
        Zone::Edge(Dir::Right) => Rect {
            x: r.x + r.w / 2.0,
            w: r.w / 2.0,
            ..r
        },
        Zone::Edge(Dir::Up) => Rect { h: r.h / 2.0, ..r },
        Zone::Edge(Dir::Down) => Rect {
            y: r.y + r.h / 2.0,
            h: r.h / 2.0,
            ..r
        },
    }
}

/// `y` is on the header of the pane card laid out at `r` (the card is
/// inset by half the gap, its header on top), `x` across it.
pub fn on_header(r: Rect, x: f32, y: f32) -> bool {
    let inset = chrome::PANE_GAP / 2.0;
    let top = r.y + inset;
    x >= r.x + inset && x < r.x + r.w - inset && y >= top && y < top + chrome::PANE_HEADER_HEIGHT
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: Rect = Rect {
        x: 100.0,
        y: 0.0,
        w: 400.0,
        h: 200.0,
    };

    fn two() -> Vec<(PaneId, Rect)> {
        vec![
            (
                PaneId(1),
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 100.0,
                    h: 200.0,
                },
            ),
            (PaneId(2), R),
        ]
    }

    #[test]
    fn zones_are_the_middle_and_a_band_along_each_edge() {
        assert_eq!(zone_at(R, 300.0, 100.0), Some(Zone::Center));
        assert_eq!(zone_at(R, 110.0, 100.0), Some(Zone::Edge(Dir::Left)));
        assert_eq!(zone_at(R, 490.0, 100.0), Some(Zone::Edge(Dir::Right)));
        assert_eq!(zone_at(R, 300.0, 10.0), Some(Zone::Edge(Dir::Up)));
        assert_eq!(zone_at(R, 300.0, 190.0), Some(Zone::Edge(Dir::Down)));
        // The band is a share of each extent: 100 px across, 50 px down.
        assert_eq!(zone_at(R, 199.0, 100.0), Some(Zone::Edge(Dir::Left)));
        assert_eq!(zone_at(R, 201.0, 100.0), Some(Zone::Center));
        assert_eq!(zone_at(R, 300.0, 49.0), Some(Zone::Edge(Dir::Up)));
        assert_eq!(zone_at(R, 300.0, 51.0), Some(Zone::Center));
        // In a corner, the nearer edge (as a share) wins.
        assert_eq!(zone_at(R, 105.0, 20.0), Some(Zone::Edge(Dir::Left)));
        assert_eq!(zone_at(R, 140.0, 2.0), Some(Zone::Edge(Dir::Up)));
        // Outside.
        assert_eq!(zone_at(R, 99.0, 100.0), None);
        assert_eq!(zone_at(R, 500.0, 100.0), None);
        assert_eq!(zone_at(R, 300.0, -1.0), None);
        assert_eq!(zone_at(Rect { w: 0.0, ..R }, 100.0, 100.0), None);
    }

    #[test]
    fn an_agent_targets_any_pane_and_a_pane_any_but_itself() {
        let panes = two();
        let agent = Payload::Agent(7);
        assert_eq!(
            target_at(&panes, true, agent, 50.0, 100.0),
            Some(DropTarget {
                pane: PaneId(1),
                zone: Zone::Center
            })
        );
        assert_eq!(
            target_at(&panes, true, agent, 490.0, 100.0),
            Some(DropTarget {
                pane: PaneId(2),
                zone: Zone::Edge(Dir::Right)
            })
        );
        let own = Payload::Pane(PaneId(2));
        assert_eq!(target_at(&panes, true, own, 300.0, 100.0), None);
        assert_eq!(target_at(&panes, true, own, 490.0, 100.0), None);
        assert_eq!(
            target_at(&panes, true, own, 5.0, 100.0),
            Some(DropTarget {
                pane: PaneId(1),
                zone: Zone::Edge(Dir::Left)
            })
        );
        assert_eq!(target_at(&panes, true, agent, 600.0, 100.0), None);
    }

    #[test]
    fn with_no_room_for_a_pane_an_agent_edge_is_the_middle() {
        // Four panes, or one maximized of four: only it is drawn.
        let maximized = vec![(PaneId(1), R)];
        let at_edge = |p| target_at(&maximized, false, p, 110.0, 100.0).map(|t| t.zone);
        assert_eq!(at_edge(Payload::Agent(1)), Some(Zone::Center));
        // Moving a pane adds none.
        assert_eq!(
            at_edge(Payload::Pane(PaneId(2))),
            Some(Zone::Edge(Dir::Left))
        );
        assert_eq!(
            target_at(&maximized, true, Payload::Agent(1), 110.0, 100.0).map(|t| t.zone),
            Some(Zone::Edge(Dir::Left))
        );
    }

    #[test]
    fn drops_show_split_swap_or_move() {
        let (p, q) = (PaneId(1), PaneId(2));
        let at = |zone| DropTarget { pane: q, zone };
        assert_eq!(
            action(Payload::Agent(7), at(Zone::Center)),
            Some(DropAction::Show { pane: q, agent: 7 })
        );
        assert_eq!(
            action(Payload::Agent(7), at(Zone::Edge(Dir::Up))),
            Some(DropAction::SplitShow {
                pane: q,
                dir: Dir::Up,
                agent: 7
            })
        );
        assert_eq!(
            action(Payload::Pane(p), at(Zone::Center)),
            Some(DropAction::Swap { pane: p, with: q })
        );
        assert_eq!(
            action(Payload::Pane(p), at(Zone::Edge(Dir::Down))),
            Some(DropAction::Move {
                pane: p,
                target: q,
                dir: Dir::Down
            })
        );
        assert_eq!(action(Payload::Pane(q), at(Zone::Center)), None);
    }

    #[test]
    fn the_highlight_is_the_pane_or_the_half_it_would_get() {
        assert_eq!(highlight(R, Zone::Center), R);
        assert_eq!(highlight(R, Zone::Edge(Dir::Left)), Rect { w: 200.0, ..R });
        assert_eq!(
            highlight(R, Zone::Edge(Dir::Right)),
            Rect {
                x: 300.0,
                w: 200.0,
                ..R
            }
        );
        assert_eq!(highlight(R, Zone::Edge(Dir::Up)), Rect { h: 100.0, ..R });
        assert_eq!(
            highlight(R, Zone::Edge(Dir::Down)),
            Rect {
                y: 100.0,
                h: 100.0,
                ..R
            }
        );
    }

    #[test]
    fn a_press_becomes_a_drag_past_the_threshold_and_drops_on_release() {
        let panes = two();
        let mut d = Drag::press(Payload::Agent(7), 50.0, 100.0);
        // A short travel stays a click: no target, nothing on release.
        assert!(!d.moved(52.0, 101.0));
        assert!(!d.retarget(&panes, true, 52.0, 101.0));
        assert_eq!(d.target(), None);
        assert_eq!(d.release(), None);
        assert!(d.moved(300.0, 100.0));
        assert!(d.active());
        assert!(d.retarget(&panes, true, 300.0, 100.0));
        assert!(!d.retarget(&panes, true, 301.0, 100.0));
        // Once a drag, coming back near the press stays one.
        assert!(d.moved(50.0, 100.0));
        d.retarget(&panes, true, 300.0, 100.0);
        assert_eq!(
            d.release(),
            Some(DropAction::Show {
                pane: PaneId(2),
                agent: 7
            })
        );
        // Released off every pane: nothing.
        let mut off = Drag::press(Payload::Agent(7), 50.0, 100.0);
        off.moved(700.0, 100.0);
        off.retarget(&panes, true, 700.0, 100.0);
        assert_eq!(off.release(), None);
    }

    #[test]
    fn the_header_is_the_top_of_the_card_inside_the_gap() {
        let inset = chrome::PANE_GAP / 2.0;
        let top = R.y + inset;
        assert!(on_header(R, 300.0, top));
        assert!(on_header(R, 300.0, top + chrome::PANE_HEADER_HEIGHT - 1.0));
        assert!(!on_header(R, 300.0, top + chrome::PANE_HEADER_HEIGHT));
        assert!(!on_header(R, 300.0, top - 1.0));
        assert!(!on_header(R, R.x, top + 1.0));
        assert!(!on_header(R, R.x + R.w - inset, top + 1.0));
    }
}
