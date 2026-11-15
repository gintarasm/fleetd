//! The unified bar across the top of the window: the traffic lights
//! (AppKit's, moved in by gpui), the fleetd it is connected to, and the
//! window's actions. Pressing anywhere else on it drags the window; a
//! double click zooms or minimises it, as the user's settings say.

use gpui::prelude::FluentBuilder;
use gpui::{
    AppContext, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, px,
};

use super::controls::{Tooltip, icon_button};
use super::icons;
use super::native;

use super::theme::{Theme, Typography};
use super::workspace::Workspace;
use crate::core::icons::Icon;
use crate::core::style::{chrome, radius, space, text};
use crate::core::titlebar::BarTitle;

/// What the bar draws.
pub struct BarData {
    pub title: BarTitle,
    /// Exited or lost agents (what Clear exited removes).
    pub ended: usize,
    /// Full screen hides the traffic lights: no room is kept for them.
    pub fullscreen: bool,
}

/// The Clear exited button's tooltip; `None` while it is disabled.
pub fn clear_exited_tooltip(ended: usize) -> Option<String> {
    match ended {
        0 => None,
        1 => Some("Remove 1 exited agent".to_owned()),
        n => Some(format!("Remove {n} exited agents")),
    }
}

pub fn render_bar(
    data: &BarData,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let tooltip_theme = *theme;
    let lights = if data.fullscreen {
        space::LG
    } else {
        chrome::TRAFFIC_LIGHTS_WIDTH
    };
    // The target block is as wide as the sidebar below it.
    let target = div()
        .id("bar-target")
        .debug_selector(|| "bar-target".to_owned())
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(space::SM))
        .w(px(chrome::SIDEBAR_WIDTH - lights - space::SM))
        .h(px(chrome::BAR_HEIGHT - space::SM))
        .px(px(space::SM))
        .rounded(px(radius::MD))
        .overflow_hidden()
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover))
        .active(|s| s.bg(theme.pressed))
        // Not a drag: the click opens the connect sheet.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(|this, _, window, cx| {
            this.open_connect_sheet(None, window, cx);
        }))
        .tooltip(move |_, cx| {
            let theme = tooltip_theme;
            cx.new(|_| Tooltip {
                text: "Switch fleetd…".into(),
                theme,
            })
            .into()
        })
        .child(icons::sized(Icon::Server, space::LG).text_color(theme.secondary))
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .child(
                    div()
                        .type_style(text::HEADLINE)
                        .text_color(theme.text)
                        .truncate()
                        .child(SharedString::from(data.title.title.clone())),
                )
                .child(
                    div()
                        .type_style(text::CAPTION)
                        .text_color(theme.secondary)
                        .truncate()
                        .child(SharedString::from(data.title.subtitle.clone())),
                ),
        );
    let clear_tip = clear_exited_tooltip(data.ended);
    let clear = icon_button("clear-exited", Icon::Trash, theme, clear_tip.is_some())
        .debug_selector(|| "clear-exited".to_owned())
        .when_some(clear_tip, |b, tip| {
            let tip = SharedString::from(tip);
            b.tooltip(move |_, cx| {
                let (text, theme) = (tip.clone(), tooltip_theme);
                cx.new(|_| Tooltip { text, theme }).into()
            })
            .on_click(cx.listener(|this, _, _, cx| this.clear_exited(cx)))
        });
    let new_agent = icon_button("new-agent", Icon::Plus, theme, true)
        .debug_selector(|| "new-agent".to_owned())
        .tooltip(move |_, cx| {
            let theme = tooltip_theme;
            cx.new(|_| Tooltip {
                text: "New agent (⌘N)".into(),
                theme,
            })
            .into()
        })
        .on_click(cx.listener(|this, _, window, cx| this.open_new_agent(window, cx)));
    div()
        .id("titlebar")
        .debug_selector(|| "titlebar".to_owned())
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .h(px(chrome::BAR_HEIGHT))
        .pr(px(space::SM))
        .gap(px(space::XS))
        .on_mouse_down(MouseButton::Left, |ev, window, _| {
            if ev.click_count >= 2 {
                window.titlebar_double_click();
            } else {
                native::begin_window_drag();
            }
        })
        .child(div().flex_none().w(px(lights)))
        .child(target)
        .child(div().flex_1())
        .child(clear)
        .child(new_agent)
}

#[cfg(test)]
mod tests {
    use super::clear_exited_tooltip;

    #[test]
    fn clear_exited_tooltip_counts_ended_agents() {
        assert_eq!(clear_exited_tooltip(0), None);
        assert_eq!(
            clear_exited_tooltip(1).as_deref(),
            Some("Remove 1 exited agent")
        );
        assert_eq!(
            clear_exited_tooltip(3).as_deref(),
            Some("Remove 3 exited agents")
        );
    }
}
