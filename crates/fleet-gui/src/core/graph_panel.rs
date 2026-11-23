//! The graph's terminal panel (issue #168, ADR 0105): the selected card's
//! agent, live and interactive, docked to the right of the graph view.
//! Like the peek (ADR 0103) it is a read-only `observe` attach under its
//! own pane id, so it never resizes the agent nor changes the layout.

use super::session::PaneId;

/// The panel's pane id: below the peek's ([`super::peek::PEEK_PANE`]).
/// Layout ids count up from 1, so neither is ever a layout's pane.
pub const GRAPH_PANEL_PANE: PaneId = PaneId(u32::MAX - 1);

/// The panel's first width, as a share of the window's.
pub const DEFAULT_SHARE: f32 = 0.45;
/// The narrowest the panel may be dragged to.
pub const MIN_WIDTH: f32 = 320.0;
/// The panel never takes more than this share of the window.
pub const MAX_SHARE: f32 = 0.8;
/// Docked, the graph keeps at least this much width beside the panel;
/// a window narrower than that puts the panel over the graph instead.
pub const MIN_GRAPH: f32 = 420.0;

/// The panel's width in a window `window` wide: `wanted` (the width the
/// user dragged to this session, else the default share), kept between
/// [`MIN_WIDTH`] and [`MAX_SHARE`] of the window (and within the window).
pub fn width(wanted: Option<f32>, window: f32) -> f32 {
    let window = window.max(0.0);
    let w = wanted.unwrap_or(window * DEFAULT_SHARE);
    let max = (window * MAX_SHARE).max(MIN_WIDTH.min(window));
    w.clamp(MIN_WIDTH.min(max), max)
}

/// Where the panel goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Beside the graph, which gets the rest of the width.
    Docked,
    /// Over the graph's right side (a narrow window): the graph keeps its
    /// full width.
    Overlay,
}

/// Docked while the graph keeps [`MIN_GRAPH`] beside a panel `panel` wide.
pub fn placement(window: f32, panel: f32) -> Placement {
    if window - panel >= MIN_GRAPH {
        Placement::Docked
    } else {
        Placement::Overlay
    }
}

/// A drag of the divider from `from_x` to `to_x` (window x), with the panel
/// `start` wide when it began: the panel's new width (the divider is on
/// the panel's left edge, so moving left widens it).
pub fn dragged(start: f32, from_x: f32, to_x: f32, window: f32) -> f32 {
    width(Some(start + (from_x - to_x)), window)
}

/// What Esc does with the graph's keys (not the panel's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphEscape {
    /// The panel is open: close it.
    ClosePanel,
    /// The filter has text: clear it.
    ClearFilter,
    /// Back to the panes.
    Leave,
}

/// Esc in the graph: closes the panel first (Esc in the panel gave the
/// keys back to the graph, so a second Esc closes it), then clears the
/// filter, then leaves the graph.
pub fn graph_escape(panel_open: bool, filter_empty: bool) -> GraphEscape {
    if panel_open {
        GraphEscape::ClosePanel
    } else if !filter_empty {
        GraphEscape::ClearFilter
    } else {
        GraphEscape::Leave
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panel_pane_is_neither_the_peek_nor_a_layout_pane() {
        assert_ne!(GRAPH_PANEL_PANE, super::super::peek::PEEK_PANE);
        assert_eq!(GRAPH_PANEL_PANE, PaneId(u32::MAX - 1));
    }

    #[test]
    fn width_defaults_to_a_share_of_the_window() {
        assert_eq!(width(None, 2000.0), 900.0);
    }

    #[test]
    fn width_keeps_the_dragged_width_within_bounds() {
        assert_eq!(width(Some(700.0), 2000.0), 700.0);
        assert_eq!(width(Some(100.0), 2000.0), MIN_WIDTH);
        assert_eq!(width(Some(1900.0), 2000.0), 1600.0);
        // A window narrower than the minimum: the whole window.
        assert_eq!(width(Some(500.0), 300.0), 300.0);
        assert_eq!(width(None, 0.0), 0.0);
    }

    #[test]
    fn a_narrow_window_overlays_the_graph() {
        assert_eq!(placement(1600.0, 700.0), Placement::Docked);
        assert_eq!(placement(1120.0, 700.0), Placement::Docked);
        assert_eq!(placement(1100.0, 700.0), Placement::Overlay);
        assert_eq!(placement(600.0, 400.0), Placement::Overlay);
    }

    #[test]
    fn dragging_the_divider_left_widens_the_panel() {
        assert_eq!(dragged(600.0, 1000.0, 900.0, 2000.0), 700.0);
        assert_eq!(dragged(600.0, 1000.0, 1100.0, 2000.0), 500.0);
        assert_eq!(dragged(600.0, 1000.0, 1900.0, 2000.0), MIN_WIDTH);
    }

    #[test]
    fn esc_closes_the_panel_then_clears_the_filter_then_leaves() {
        assert_eq!(graph_escape(true, false), GraphEscape::ClosePanel);
        assert_eq!(graph_escape(true, true), GraphEscape::ClosePanel);
        assert_eq!(graph_escape(false, false), GraphEscape::ClearFilter);
        assert_eq!(graph_escape(false, true), GraphEscape::Leave);
    }
}
