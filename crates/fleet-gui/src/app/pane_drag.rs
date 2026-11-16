//! Drag and drop of agents and panes (ADR 0059): a sidebar row or a pane
//! header, pressed and moved past the threshold, drags; the drop target
//! under the pointer is highlighted, and a release there acts on it. Esc
//! cancels. Where a drop lands and what it does is [`crate::core::drag_drop`];
//! this only feeds it the mouse and draws the highlight.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, CursorStyle, DispatchPhase, Entity, InteractiveElement, IntoElement,
    MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Styled, Window,
    canvas, div, px, relative,
};

use super::app_state::AppState;
use super::theme::Theme;
use crate::core::drag_drop::{Drag, DropTarget, Payload, highlight, on_header};
use crate::core::layout::Rect;
use crate::core::session::PaneId;
use crate::core::style::radius;

/// The press or drag in progress, the pane area's bounds last frame, and
/// the panes drawn in it (fractions of it).
#[derive(Clone, Default)]
pub(super) struct PaneDrag {
    drag: Rc<Cell<Option<Drag>>>,
    area: Rc<Cell<Bounds<Pixels>>>,
    drawn: Rc<RefCell<Vec<(PaneId, Rect)>>>,
    /// The layout has room for another pane.
    can_split: Rc<Cell<bool>>,
    /// A drag ended since the last press: the click that ends it is not one.
    dragged: Rc<Cell<bool>>,
}

/// `p` in the pane area's own pixels.
fn local(p: Point<Pixels>, area: Bounds<Pixels>) -> (f32, f32) {
    let l = p - area.origin;
    (f32::from(l.x), f32::from(l.y))
}

/// The drawn panes' rects in the area's pixels.
fn in_pixels(drawn: &[(PaneId, Rect)], area: Bounds<Pixels>) -> Vec<(PaneId, Rect)> {
    let (w, h) = (f32::from(area.size.width), f32::from(area.size.height));
    drawn
        .iter()
        .map(|(p, r)| {
            (
                *p,
                Rect {
                    x: r.x * w,
                    y: r.y * h,
                    w: r.w * w,
                    h: r.h * h,
                },
            )
        })
        .collect()
}

impl PaneDrag {
    /// A sidebar row was pressed at `position` (window coordinates).
    pub(super) fn press_agent(&self, agent: u64, position: Point<Pixels>) {
        let (x, y) = local(position, self.area.get());
        self.dragged.set(false);
        self.drag
            .set(Some(Drag::press(Payload::Agent(agent), x, y)));
    }

    /// The card of `pane` was pressed at `position`: a press on its header
    /// may drag it.
    pub(super) fn press_card(&self, pane: PaneId, position: Point<Pixels>) {
        self.dragged.set(false);
        let area = self.area.get();
        let (x, y) = local(position, area);
        let on = in_pixels(&self.drawn.borrow(), area)
            .into_iter()
            .any(|(p, r)| p == pane && on_header(r, x, y));
        if on {
            self.drag.set(Some(Drag::press(Payload::Pane(pane), x, y)));
        }
    }

    /// The click ending a press: `false` if that press was a drag.
    pub(super) fn is_click(&self) -> bool {
        !self.dragged.take()
    }

    /// Esc: drops the drag in progress; `true` if there was one.
    pub(super) fn cancel(&self) -> bool {
        let was = self.drag.take().is_some_and(|d| d.active());
        self.dragged.set(was);
        was
    }

    /// Where a drag would drop now, if one is under way.
    pub(super) fn target(&self) -> Option<DropTarget> {
        self.drag.get().and_then(|d| d.target())
    }

    /// An element filling the pane area that tracks its bounds and follows
    /// the pointer, then the highlight over the target; children of the
    /// pane area, after the cards and dividers so they are on top. `drawn`
    /// is the panes drawn, as fractions of the area; `can_split`, whether
    /// the layout has room for another. Under Reduce
    /// Transparency the highlight is an outline only, with no tint.
    pub(super) fn render(
        &self,
        drawn: Vec<(PaneId, Rect)>,
        can_split: bool,
        app: &Entity<AppState>,
        theme: &Theme,
    ) -> Vec<AnyElement> {
        let target = self
            .drag
            .get()
            .and_then(|d| d.target())
            .and_then(|t| Some((t, drawn.iter().find(|(p, _)| *p == t.pane)?.1)));
        *self.drawn.borrow_mut() = drawn;
        self.can_split.set(can_split);
        let (drag, area, rects, dragged, room) = (
            self.drag.clone(),
            self.area.clone(),
            self.drawn.clone(),
            self.dragged.clone(),
            self.can_split.clone(),
        );
        let app = app.clone();
        let mut out = vec![
            canvas(
                {
                    let area = area.clone();
                    move |bounds, _, _| area.set(bounds)
                },
                move |_, _, window, _| {
                    if drag.get().is_some_and(|d| d.active()) {
                        window.set_window_cursor_style(CursorStyle::ClosedHand);
                    }
                    let (d_move, a_move) = (drag.clone(), area.clone());
                    window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, _| {
                        let Some(mut d) = d_move.get() else { return };
                        if phase != DispatchPhase::Bubble {
                            return;
                        }
                        if ev.pressed_button != Some(MouseButton::Left) {
                            // Released where no mouse-up reached us.
                            d_move.set(None);
                            window.refresh();
                            return;
                        }
                        let b = a_move.get();
                        let (x, y) = local(ev.position, b);
                        let was = d.active();
                        let now = d.moved(x, y);
                        let changed = d.retarget(&in_pixels(&rects.borrow(), b), room.get(), x, y);
                        d_move.set(Some(d));
                        if was != now || changed {
                            window.refresh();
                        }
                    });
                    let d_up = drag.clone();
                    window.on_mouse_event(move |ev: &MouseUpEvent, phase, window, cx| {
                        if phase != DispatchPhase::Bubble || ev.button != MouseButton::Left {
                            return;
                        }
                        let Some(d) = d_up.take() else { return };
                        if !d.active() {
                            return;
                        }
                        dragged.set(true);
                        if let Some(action) = d.release() {
                            drop_on(&app, action, window, cx);
                        }
                        window.refresh();
                    });
                },
            )
            .absolute()
            .size_full()
            .into_any_element(),
        ];
        if let Some((t, r)) = target {
            let h = highlight(r, t.zone);
            let outline = div()
                .size_full()
                .rounded(px(radius::MD))
                .border_2()
                .border_color(theme.accent);
            let shape = div()
                .debug_selector(|| "drop-highlight".to_owned())
                .absolute()
                .left(relative(h.x))
                .top(relative(h.y))
                .w(relative(h.w))
                .h(relative(h.h))
                .p(px(crate::core::style::chrome::PANE_GAP / 2.0))
                .child(if super::native::reduce_transparency() {
                    outline
                } else {
                    outline.bg(theme.accent.opacity(0.16))
                });
            out.push(shape.into_any_element());
        }
        out
    }
}

/// Carries out `action` and puts the keyboard in the pane it landed in.
fn drop_on(
    app: &Entity<AppState>,
    action: crate::core::drag_drop::DropAction,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(pane) = app.update(cx, |a, cx| a.drop_on(action, cx)) else {
        return;
    };
    let handle = app
        .read(cx)
        .pane_view(pane)
        .map(|v| v.read(cx).focus_handle().clone());
    if let Some(handle) = handle {
        window.focus(&handle);
    }
}
