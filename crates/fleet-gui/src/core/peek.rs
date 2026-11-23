//! Peek (issue #142, ADR 0103): an agent's live screen in a popover,
//! over the layout and without changing it. The popover's connection is a
//! read-only `observe` attach (ADR 0022) under its own pane id; typing in
//! it still reaches the agent.

use std::time::Duration;

use super::broadcast::RowSelection;
use super::session::PaneId;

/// The peek's pane id. Layout ids count up from 1, so this one is never a
/// layout's pane.
pub const PEEK_PANE: PaneId = PaneId(u32::MAX);

/// How long the pointer rests on a graph card before it peeks.
pub const HOVER_HOLD: Duration = Duration::from_millis(700);

/// The popover takes at most this share of the window's width and height.
pub const MAX_SHARE: f32 = 0.85;

/// The peek's font never shrinks below this many points to fit.
pub const MIN_FONT: f32 = 8.0;

/// The row Space peeks at in the sidebar: the last row clicked (a ⌘-click
/// keeps the keyboard in the sidebar), else the last row picked.
pub fn sidebar_target(selection: &RowSelection) -> Option<u64> {
    selection
        .anchor()
        .or_else(|| selection.ids().last().copied())
}

/// What a key typed in the peek does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeekKey {
    /// Escape: the peek closes.
    Close,
    /// ⌘Return: the agent opens in the focused pane.
    OpenInPane,
    /// Anything else goes to the agent.
    Type,
}

/// Classifies a key (gpui's key name and whether ⌘, ⌃, ⌥ or ⇧ are held).
pub fn peek_key(key: &str, cmd: bool, ctrl: bool, alt: bool, shift: bool) -> PeekKey {
    match key {
        "escape" if !cmd && !ctrl && !alt && !shift => PeekKey::Close,
        "enter" if cmd && !ctrl && !alt && !shift => PeekKey::OpenInPane,
        _ => PeekKey::Type,
    }
}

/// The pointer resting on a graph card: the card peeks once it has rested
/// [`HOVER_HOLD`] (a timer measures it, and fires the wait's token).
/// Moving to another card starts the wait again; leaving the card, or a
/// press (a drag, a click), cancels it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HoverHold {
    /// The card waited on, and the wait's token.
    resting: Option<(u64, u64)>,
    waits: u64,
}

impl HoverHold {
    /// The pointer entered card `id`. A new wait's token, for the timer to
    /// fire, if the wait (re)starts.
    pub fn enter(&mut self, id: u64) -> Option<u64> {
        if self.resting.is_some_and(|(r, _)| r == id) {
            return None;
        }
        self.waits += 1;
        self.resting = Some((id, self.waits));
        Some(self.waits)
    }

    /// The pointer left card `id` (another card's enter already replaced
    /// it, in which case nothing changes).
    pub fn leave(&mut self, id: u64) {
        if self.resting.is_some_and(|(r, _)| r == id) {
            self.resting = None;
        }
    }

    /// A press anywhere: no peek until the pointer rests on a card again.
    pub fn cancel(&mut self) {
        self.resting = None;
    }

    /// The hold timer of wait `token` fired: the card to peek at, if the
    /// pointer still rests there since that wait began. Once.
    pub fn fire(&mut self, token: u64) -> Option<u64> {
        let (id, t) = self.resting?;
        if t != token {
            return None;
        }
        self.resting = None;
        Some(id)
    }

    /// The card waiting, if any.
    pub fn resting(&self) -> Option<u64> {
        self.resting.map(|(id, _)| id)
    }
}

/// How the peek draws an agent's grid of `grid` cells (the agent's size:
/// an observer never resizes it) when a cell is `cell` pixels wide and high
/// at `font` points, in a window of `window` pixels: the font, shrunk (not
/// below [`MIN_FONT`]) so the grid fits in [`MAX_SHARE`] of the window,
/// and the grid's size in pixels at that font (at most that share).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    pub font: f32,
    pub width: f32,
    pub height: f32,
}

pub fn fit(grid: (u16, u16), cell: (f32, f32), font: f32, window: (f32, f32)) -> Fit {
    fit_within(
        grid,
        cell,
        font,
        (window.0 * MAX_SHARE, window.1 * MAX_SHARE),
    )
}

/// [`fit`] in a room of `room` pixels (all of it, not a share): the graph's
/// terminal panel (ADR 0105).
pub fn fit_within(grid: (u16, u16), cell: (f32, f32), font: f32, room: (f32, f32)) -> Fit {
    let (max_w, max_h) = (room.0.max(0.0), room.1.max(0.0));
    let (cols, rows) = (f32::from(grid.0.max(1)), f32::from(grid.1.max(1)));
    let (w, h) = (cols * cell.0, rows * cell.1);
    if w <= 0.0 || h <= 0.0 || font <= 0.0 {
        return Fit {
            font,
            width: max_w,
            height: max_h,
        };
    }
    let scale = (max_w / w).min(max_h / h).min(1.0);
    let font_fit = (font * scale).max(MIN_FONT.min(font));
    let k = font_fit / font;
    Fit {
        font: font_fit,
        width: (w * k).min(max_w),
        height: (h * k).min(max_h),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_target_is_the_last_row_clicked() {
        let mut s = RowSelection::default();
        assert_eq!(sidebar_target(&s), None);
        let order = [1, 2, 3];
        s.click(2, true, false, &order, Some(1));
        assert_eq!(sidebar_target(&s), Some(2));
        s.click(3, true, false, &order, Some(1));
        assert_eq!(sidebar_target(&s), Some(3));
        // A plain click moves the anchor too (focus then leaves the sidebar).
        s.click(1, false, false, &order, Some(1));
        assert_eq!(sidebar_target(&s), Some(1));
    }

    #[test]
    fn sidebar_target_falls_back_to_the_last_row_picked() {
        let mut s = RowSelection::default();
        s.click(2, true, false, &[1, 2], None);
        s.retain(|id| id != 2);
        assert_eq!(sidebar_target(&s), None);
        s.click(1, true, false, &[1, 2], None);
        s.click(2, true, false, &[1, 2], None);
        s.retain(|id| id != 2);
        assert_eq!(sidebar_target(&s), Some(1));
    }

    #[test]
    fn escape_closes_cmd_return_opens_the_rest_types() {
        assert_eq!(
            peek_key("escape", false, false, false, false),
            PeekKey::Close
        );
        assert_eq!(
            peek_key("enter", true, false, false, false),
            PeekKey::OpenInPane
        );
        assert_eq!(peek_key("enter", false, false, false, false), PeekKey::Type);
        assert_eq!(peek_key("escape", false, false, true, false), PeekKey::Type);
        assert_eq!(peek_key("enter", true, false, false, true), PeekKey::Type);
        assert_eq!(peek_key("a", false, false, false, false), PeekKey::Type);
        assert_eq!(peek_key("space", false, false, false, false), PeekKey::Type);
    }

    #[test]
    fn hover_hold_peeks_once_when_its_timer_fires() {
        let mut h = HoverHold::default();
        let t = h.enter(4).unwrap();
        assert_eq!(h.fire(t), Some(4));
        assert_eq!(h.fire(t), None);
    }

    #[test]
    fn hover_hold_moving_within_a_card_does_not_restart() {
        let mut h = HoverHold::default();
        let t = h.enter(4).unwrap();
        assert_eq!(h.enter(4), None);
        assert_eq!(h.fire(t), Some(4));
    }

    #[test]
    fn hover_hold_another_card_restarts_and_leave_cancels() {
        let mut h = HoverHold::default();
        let t4 = h.enter(4).unwrap();
        let t5 = h.enter(5).unwrap();
        // The old card's leave comes after the new card's enter.
        h.leave(4);
        assert_eq!(h.resting(), Some(5));
        assert_eq!(h.fire(t4), None, "the old wait's timer");
        assert_eq!(h.fire(t5), Some(5));
        let t6 = h.enter(6).unwrap();
        h.leave(6);
        assert_eq!(h.fire(t6), None);
        let t7 = h.enter(7).unwrap();
        h.cancel();
        assert_eq!(h.fire(t7), None);
        // Back on the same card after leaving: a new wait.
        let t8 = h.enter(7).unwrap();
        assert_ne!(t7, t8);
        assert_eq!(h.fire(t8), Some(7));
    }

    #[test]
    fn fit_keeps_the_font_when_the_grid_fits() {
        let f = fit((80, 24), (8.0, 16.0), 13.0, (1600.0, 1000.0));
        assert_eq!(f.font, 13.0);
        assert_eq!((f.width, f.height), (640.0, 384.0));
    }

    #[test]
    fn fit_shrinks_the_font_to_fit_the_window() {
        // 200×50 at 8×16 is 1600×800; 85% of 1000×800 is 850×680.
        let f = fit((200, 50), (8.0, 16.0), 16.0, (1000.0, 800.0));
        let k = 850.0 / 1600.0;
        assert!((f.font - 16.0 * k).abs() < 1e-3);
        assert!((f.width - 850.0).abs() < 1e-3);
        assert!(f.height <= 680.0);
    }

    #[test]
    fn fit_stops_at_the_minimum_font_and_caps_the_size() {
        let f = fit((1000, 400), (8.0, 16.0), 16.0, (800.0, 600.0));
        assert_eq!(f.font, MIN_FONT);
        assert_eq!((f.width, f.height), (680.0, 510.0));
    }

    #[test]
    fn fit_with_no_metrics_yet_takes_the_largest_size() {
        let f = fit((80, 24), (0.0, 0.0), 13.0, (1000.0, 800.0));
        assert_eq!(f.font, 13.0);
        assert_eq!((f.width, f.height), (850.0, 680.0));
    }

    #[test]
    fn peek_pane_is_no_layout_pane() {
        assert_eq!(PEEK_PANE, PaneId(u32::MAX));
    }
}
