//! Message lines in the graph view (ADRs 0089, 0106): routes them (once
//! per layout or pair-set change, [`RouteCache`]), groups them into a few
//! paths per frame, tracks what the pointer is over, and draws the hovered
//! line's tooltip and the legend. Kept out of `graph_view.rs`, which only
//! calls in.
//!
//! Hovering a line highlights it and both its cards and shows who wrote to
//! whom, what and when; a click on it selects the sender. Hovering a card
//! dims the lines that do not touch it. A card always wins over a line
//! ([`hover_at`]), so line hover and card hover never overlap.

use gpui::{
    AnyElement, BorderStyle, Bounds, Hsla, InteractiveElement, IntoElement, ParentElement,
    PathBuilder, Pixels, Point, SharedString, Styled, Window, div, point, px, quad,
};

use super::theme::{Theme, Typography};
use crate::core::graph_layout::{GraphLayout, Viewport};
use crate::core::layout::Rect;
use crate::core::message_lines::{
    self, Edge, HIT_PX, Hover, Pair, RouteCache, STEPS, hover_at, in_view, recent_pairs,
};
use crate::core::style::{radius, space, text};

/// Lines that do not touch the hovered card, at this opacity.
const DIM: f32 = 0.12;
/// Around a hovered line's cards, in world units.
const RING: f32 = 3.0;
/// The tooltip's offset from the pointer, in screen pixels.
const TIP_OFFSET: (f32, f32) = (12.0, 16.0);

#[derive(Default)]
pub(super) struct GraphLines {
    cache: RouteCache,
    /// The pairs drawn last frame (for the tooltip).
    pairs: Vec<Pair>,
    hover: Option<Hover>,
    /// Where the pointer was when the hover last changed (area
    /// coordinates): the tooltip's anchor.
    anchor: (f32, f32),
}

type Polyline = Vec<(f32, f32)>;

/// Lines painted as one path.
struct Group {
    lines: Vec<Polyline>,
    opacity: f32,
    /// Stroke width in world units.
    width: f32,
}

/// One frame's lines in area coordinates, ready for the canvas.
pub(super) struct LinePaint {
    /// A handful: one `paint_path` each.
    groups: Vec<Group>,
    /// The hovered line's two cards.
    rings: Vec<Rect>,
}

impl GraphLines {
    /// The lines to paint this frame: recent pairs between cards the
    /// layout has (`generation` changes with the layout), culled to `view`
    /// (world), one path per fade step, plus a dimmed one while a card is
    /// hovered and a bright one for the hovered line.
    pub fn prepare(
        &mut self,
        generation: usize,
        layout: &GraphLayout,
        edges: &[Edge],
        now_ms: i64,
        view: &Rect,
        vp: Viewport,
    ) -> LinePaint {
        self.pairs = recent_pairs(edges, now_ms, &|id| layout.card(id).is_some());
        let routes = self.cache.get(generation, layout, &self.pairs);
        if let Some(Hover::Line { from, to }) = self.hover
            && !routes.iter().any(|r| (r.from, r.to) == (from, to))
        {
            // Its line expired.
            self.hover = None;
        }
        // Fade steps, then dimmed, then the hovered line on top.
        let mut groups: Vec<Vec<Polyline>> = vec![Vec::new(); STEPS + 2];
        for r in in_view(routes, view) {
            let Some(pair) = self.pairs.iter().find(|p| (p.from, p.to) == (r.from, r.to)) else {
                continue;
            };
            let group = match self.hover {
                Some(Hover::Line { from, to }) if (from, to) == (r.from, r.to) => STEPS + 1,
                Some(Hover::Card(c)) if c != r.from && c != r.to => STEPS,
                _ => pair.step,
            };
            let screen = |p: &(f32, f32)| vp.to_screen(*p);
            groups[group].push(r.points.iter().map(screen).collect());
            for b in &r.barbs {
                groups[group].push(b.iter().map(screen).collect());
            }
        }
        let mut rings = Vec::new();
        if let Some(Hover::Line { from, to }) = self.hover {
            for id in [from, to] {
                if let Some(c) = layout.card(id) {
                    let r = &c.rect;
                    rings.push(vp.rect_to_screen(&Rect {
                        x: r.x - RING,
                        y: r.y - RING,
                        w: r.w + 2.0 * RING,
                        h: r.h + 2.0 * RING,
                    }));
                }
            }
        }
        // Oldest (faintest) first, so fresh lines lie on top.
        let group = |lines: &mut Vec<Polyline>, opacity, width| Group {
            lines: std::mem::take(lines),
            opacity,
            width,
        };
        let mut out = Vec::new();
        for (step, g) in groups.iter_mut().enumerate().take(STEPS).rev() {
            out.push(group(g, message_lines::opacity_of(step), 1.25));
        }
        out.push(group(&mut groups[STEPS], DIM, 1.25));
        out.push(group(&mut groups[STEPS + 1], 1.0, 2.25));
        out.retain(|g| !g.lines.is_empty());
        LinePaint { groups: out, rings }
    }

    /// The pointer moved to `at` (area coordinates). True when what it is
    /// over changed in a way that shows (a redraw is needed).
    pub fn pointer_moved(&mut self, layout: &GraphLayout, vp: Viewport, at: (f32, f32)) -> bool {
        let routes = self.cache.routes();
        let world = vp.to_world(at);
        let now = hover_at(layout, routes, world, HIT_PX / vp.zoom);
        if now == self.hover {
            return false;
        }
        let was = std::mem::replace(&mut self.hover, now);
        self.anchor = at;
        // Moving between cards shows only when there are lines to dim.
        let line =
            matches!(was, Some(Hover::Line { .. })) || matches!(now, Some(Hover::Line { .. }));
        line || !routes.is_empty()
    }

    /// The pointer left the graph. True when that needs a redraw.
    pub fn pointer_left(&mut self) -> bool {
        self.hover.take().is_some() && !self.cache.routes().is_empty()
    }

    /// The hovered line: `(sender, recipient)`.
    pub fn hovered_line(&self) -> Option<(u64, u64)> {
        match self.hover {
            Some(Hover::Line { from, to }) => Some((from, to)),
            _ => None,
        }
    }

    /// The hovered line's tooltip, near the pointer.
    pub fn tooltip(
        &self,
        name: &dyn Fn(u64) -> String,
        now_ms: i64,
        theme: &Theme,
    ) -> Option<AnyElement> {
        let (from, to) = self.hovered_line()?;
        let pair = self.pairs.iter().find(|p| (p.from, p.to) == (from, to))?;
        let label = message_lines::tooltip(&name(from), &name(to), pair, now_ms);
        Some(
            div()
                .debug_selector(move || format!("graph-line-tip-{from}-{to}"))
                .absolute()
                .left(px(self.anchor.0 + TIP_OFFSET.0))
                .top(px(self.anchor.1 + TIP_OFFSET.1))
                .px(px(space::SM))
                .py(px(space::XXS))
                .rounded(px(radius::SM))
                .bg(theme.raised)
                .border_1()
                .border_color(theme.border)
                .shadow_md()
                .type_style(text::CAPTION)
                .text_color(theme.text)
                .whitespace_nowrap()
                .child(SharedString::from(label))
                .into_any_element(),
        )
    }
}

impl LinePaint {
    /// Paints the lines and the hovered line's card rings; `o` is the
    /// area's origin, `z` the zoom. A few `paint_path` calls.
    pub fn paint(&self, o: Point<Pixels>, z: f32, colour: Hsla, window: &mut Window) {
        let at = |p: (f32, f32)| point(o.x + px(p.0), o.y + px(p.1));
        for r in &self.rings {
            window.paint_quad(quad(
                Bounds::new(at((r.x, r.y)), gpui::size(px(r.w), px(r.h))),
                px((radius::LG + RING) * z),
                colour.opacity(0.0),
                px(2.),
                colour,
                BorderStyle::Solid,
            ));
        }
        for g in &self.groups {
            let mut path = PathBuilder::stroke(px((g.width * z).max(1.0)));
            for line in &g.lines {
                path.move_to(at(line[0]));
                for &p in &line[1..] {
                    path.line_to(at(p));
                }
            }
            if let Ok(path) = path.build() {
                window.paint_path(path, colour.opacity(g.opacity));
            }
        }
    }
}

/// The hint bar's legend: an accent swatch and what it means.
pub(super) fn legend(theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(space::XS))
        .type_style(text::CAPTION)
        .text_color(theme.tertiary)
        .child(div().w(px(14.)).h(px(2.)).rounded(px(1.)).bg(theme.accent))
        .child("messages (last 5 min)")
}
