//! An agent's identity mark (ADR 0064): its colour as a small rounded
//! square, or its emoji when `[gui] agent_emoji` is on. Drawn on the
//! sidebar row; the pane header tints its icon instead.

use gpui::{Div, Hsla, InteractiveElement, ParentElement, Styled, div, px};

use super::theme::{Theme, Typography, hsla};
use crate::core::identity::{agent_colour, agent_emoji};
use crate::core::style::text;

/// The swatch's side and corner: a little larger than the round status
/// dot, and square, so the two never read as one another.
const SWATCH: f32 = 9.0;
const SWATCH_RADIUS: f32 = 2.0;

/// The width the mark takes on the row, swatch or emoji alike, so the
/// titles line up either way.
pub const MARK_SLOT: f32 = 12.0;

/// Agent `id`'s colour in `theme`'s appearance.
pub fn agent_color(id: u64, theme: &Theme) -> Hsla {
    hsla(agent_colour(id).for_appearance(theme.appearance))
}

/// Agent `id`'s emoji, at caption size.
pub fn emoji(id: u64) -> Div {
    div()
        .flex_none()
        .type_style(text::CAPTION)
        .child(agent_emoji(id))
}

/// Agent `id`'s mark on its sidebar row (debug selector
/// `identity-<id>-<colour>`, or `identity-<id>-emoji`).
pub fn agent_mark(id: u64, theme: &Theme, use_emoji: bool) -> Div {
    let mark = if use_emoji {
        emoji(id).debug_selector(move || format!("identity-{id}-emoji"))
    } else {
        let name = agent_colour(id).name;
        div()
            .flex_none()
            .size(px(SWATCH))
            .rounded(px(SWATCH_RADIUS))
            .bg(agent_color(id, theme))
            .debug_selector(move || format!("identity-{id}-{name}"))
    };
    div()
        .flex()
        .flex_none()
        .w(px(MARK_SLOT))
        .justify_center()
        .items_center()
        .child(mark)
}
