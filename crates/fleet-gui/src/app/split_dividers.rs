//! The split dividers over the pane cards (ADR 0045): a resize strip on
//! each gap between two sides of a split. Dragging one moves it, keeping
//! a minimum pane size; a double-click evens it. The geometry is
//! [`Layout::dividers`]; this only turns the mouse into its calls.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, Bounds, CursorStyle, DispatchPhase, Entity, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Styled,
    canvas, div, px, relative,
};

use super::app_state::AppState;
use crate::core::layout::{Axis, Layout, SplitId};
use crate::core::style::chrome;

/// A divider being dragged.
#[derive(Clone, Copy, Debug)]
struct Grab {
    split: SplitId,
    axis: Axis,
    /// The pointer's distance from the line when it was grabbed, so the
    /// line follows the pointer's travel rather than jumping to it.
    offset: f32,
}

/// The drag in progress and the pane area's bounds last frame (the
/// strips are laid out in fractions; a drag needs pixels).
#[derive(Default)]
pub(super) struct SplitDrag {
    grab: Rc<Cell<Option<Grab>>>,
    area: Rc<Cell<Bounds<Pixels>>>,
}

fn cursor(axis: Axis) -> CursorStyle {
    match axis {
        Axis::Horizontal => CursorStyle::ResizeLeftRight,
        Axis::Vertical => CursorStyle::ResizeUpDown,
    }
}

/// `p` along `axis` in `area`, and the area's extent along it.
fn along(axis: Axis, p: gpui::Point<Pixels>, area: Bounds<Pixels>) -> f32 {
    let local = p - area.origin;
    match axis {
        Axis::Horizontal => f32::from(local.x),
        Axis::Vertical => f32::from(local.y),
    }
}

impl SplitDrag {
    /// The strips for `layout`'s dividers, and an element filling the
    /// pane area that tracks its bounds and follows a drag; children of
    /// the pane area, after the cards so the strips are on top.
    pub(super) fn render(&self, layout: &Layout, app: &Entity<AppState>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let (grab, area) = (self.grab.clone(), self.area.clone());
        let track = app.clone();
        out.push(
            canvas(
                {
                    let area = area.clone();
                    move |bounds, _, _| area.set(bounds)
                },
                move |_, _, window, _| {
                    if let Some(g) = grab.get() {
                        // Off the strip mid-drag, the cursor stays.
                        window.set_window_cursor_style(cursor(g.axis));
                    }
                    let (g_move, a_move) = (grab.clone(), area.clone());
                    window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
                        let Some(g) = g_move.get() else { return };
                        if phase != DispatchPhase::Bubble {
                            return;
                        }
                        if ev.pressed_button != Some(MouseButton::Left) {
                            // Released where no mouse-up reached us.
                            g_move.set(None);
                            window.refresh();
                            return;
                        }
                        let b = a_move.get();
                        let at = along(g.axis, ev.position, b) - g.offset;
                        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
                        track.update(cx, |a, cx| a.move_divider(g.split, at, w, h, cx));
                    });
                    let g_up = grab.clone();
                    window.on_mouse_event(move |ev: &MouseUpEvent, phase, window, _| {
                        if phase == DispatchPhase::Bubble
                            && ev.button == MouseButton::Left
                            && g_up.take().is_some()
                        {
                            window.refresh();
                        }
                    });
                },
            )
            .absolute()
            .size_full()
            .into_any_element(),
        );
        for d in layout.dividers(1.0, 1.0) {
            let (split, axis) = (d.split, d.axis);
            let (grab, area, app) = (self.grab.clone(), self.area.clone(), app.clone());
            let strip = div()
                .id(("split-divider", split.0))
                .debug_selector(move || format!("split-divider-{}", split.0))
                .absolute()
                .cursor(cursor(axis))
                .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    if ev.click_count >= 2 {
                        grab.set(None);
                        app.update(cx, |a, cx| a.reset_divider(split, cx));
                    } else {
                        let b = area.get();
                        let len = match axis {
                            Axis::Horizontal => f32::from(b.size.width),
                            Axis::Vertical => f32::from(b.size.height),
                        };
                        let offset = along(axis, ev.position, b) - d.at * len;
                        grab.set(Some(Grab {
                            split,
                            axis,
                            offset,
                        }));
                    }
                    window.refresh();
                });
            let half = px(chrome::DIVIDER_HIT / 2.0);
            // A zero-thickness line at the divider, its strip centred on it.
            let line = match axis {
                Axis::Horizontal => div()
                    .absolute()
                    .left(relative(d.at))
                    .top(relative(d.span.y))
                    .h(relative(d.span.h))
                    .child(
                        strip
                            .top_0()
                            .h_full()
                            .left(-half)
                            .w(px(chrome::DIVIDER_HIT)),
                    ),
                Axis::Vertical => div()
                    .absolute()
                    .top(relative(d.at))
                    .left(relative(d.span.x))
                    .w(relative(d.span.w))
                    .child(
                        strip
                            .left_0()
                            .w_full()
                            .top(-half)
                            .h(px(chrome::DIVIDER_HIT)),
                    ),
            };
            out.push(line.into_any_element());
        }
        out
    }
}
