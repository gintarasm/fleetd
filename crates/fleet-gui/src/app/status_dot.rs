//! The status dot drawn on a sidebar row and a pane header (ADR 0042).

use gpui::{Div, Hsla, Styled, div, px};

use super::theme::Theme;
use crate::core::status::{AgentStatus, DotFill, dot_dimmed};

/// The dot's diameter.
const DOT: f32 = 8.0;

/// The colour of `status`'s dot.
pub fn dot_color(status: AgentStatus, theme: &Theme) -> Hsla {
    match status {
        AgentStatus::Working => theme.accent,
        AgentStatus::NeedsYou => theme.warning,
        AgentStatus::Idle => theme.tertiary,
        AgentStatus::Done => theme.success,
        AgentStatus::Failed => theme.danger,
    }
}

/// A dot in `status`'s colour: solid for a reported (or exit) status, a
/// ring for one inferred from output (ADR 0075), dimmed on a working
/// agent's off-beat ([`dot_dimmed`]).
pub fn status_dot(
    status: AgentStatus,
    fill: DotFill,
    theme: &Theme,
    now_ms: i64,
    reduce_motion: bool,
) -> Div {
    let color = dot_color(status, theme);
    let dot = div().flex_none().size(px(DOT)).rounded_full();
    let dot = match fill {
        DotFill::Solid => dot.bg(color),
        DotFill::Hollow => dot.border_1().border_color(color),
    };
    if dot_dimmed(status, now_ms, reduce_motion) {
        dot.opacity(0.45)
    } else {
        dot
    }
}
