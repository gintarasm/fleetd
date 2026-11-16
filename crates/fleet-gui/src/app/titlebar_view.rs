//! The unified bar across the top of the window: the traffic lights
//! (AppKit's, moved in by gpui), the fleetd it is connected to, and the
//! window's actions. Pressing anywhere else on it drags the window; a
//! double click zooms or minimises it, as the user's settings say.

use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Context, Div, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, Stateful, StatefulInteractiveElement, Styled, Window, div, px,
};

use super::controls::{Tooltip, icon_button};
use super::icons;
use super::native;

use super::theme::{Theme, Typography};
use super::workspace::Workspace;
use crate::core::icons::Icon;
use crate::core::style::{chrome, radius, space, text};
use crate::core::titlebar::{BarTitle, UpdateButton};

/// What the bar draws.
pub struct BarData {
    pub title: BarTitle,
    /// Exited or lost agents (what Clear exited removes).
    pub ended: usize,
    /// Full screen hides the traffic lights: no room is kept for them.
    pub fullscreen: bool,
    /// Update fleetd, while a newer build is on disk or the restart onto
    /// it runs.
    pub update: Option<UpdateButton>,
    /// The workspace tabs' labels, in strip order (ADR 0061).
    pub tabs: Vec<String>,
    /// The shown tab (0-based).
    pub active_tab: usize,
}

/// The Clear exited button's tooltip; `None` while it is disabled.
pub fn clear_exited_tooltip(ended: usize) -> Option<String> {
    match ended {
        0 => None,
        1 => Some("Remove 1 exited agent".to_owned()),
        n => Some(format!("Remove {n} exited agents")),
    }
}

/// A press on the bar: drags the window; a double click zooms or
/// minimises it, as System Settings say.
fn drag_window(ev: &MouseDownEvent, window: &mut Window, _: &mut App) {
    if ev.click_count >= 2 {
        window.titlebar_double_click();
    } else {
        native::begin_window_drag();
    }
}

/// Laid over the bar while a sheet's backdrop covers the window: the
/// window still drags by its bar, whose buttons are inert meanwhile.
pub fn sheet_drag_strip(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(px(chrome::BAR_HEIGHT))
        .on_mouse_down(MouseButton::Left, drag_window)
}

/// The workspace tabs, once there are two or more: a chip per tab (a
/// click shows it; its ✕ closes it) and a New tab button. Not a drag.
fn render_tabs(data: &BarData, theme: &Theme, cx: &mut Context<Workspace>) -> Option<Div> {
    if data.tabs.len() < 2 {
        return None;
    }
    let tooltip_theme = *theme;
    let chips = data.tabs.iter().enumerate().map(|(i, label)| {
        let n = i + 1;
        let active = i == data.active_tab;
        let close = icon_button("tab-close", Icon::Close, theme, true)
            .debug_selector(move || format!("workspace-tab-close-{n}"))
            .size(px(chrome::ICON + space::XS))
            .tooltip(move |_, cx| {
                let theme = tooltip_theme;
                let text = if active {
                    "Close tab (⌘⌥W)".into()
                } else {
                    "Close tab".into()
                };
                cx.new(|_| Tooltip { text, theme }).into()
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.close_tab(i, window, cx);
            }));
        div()
            .id(("workspace-tab", i))
            .debug_selector(move || format!("workspace-tab-{n}"))
            .flex()
            .flex_row()
            .flex_shrink()
            .min_w_0()
            .max_w(px(chrome::SIDEBAR_WIDTH * 0.75))
            .items_center()
            .gap(px(space::XS))
            .h(px(chrome::BAR_HEIGHT - space::LG))
            .pl(px(space::SM))
            .pr(px(space::XXS))
            .rounded(px(radius::SM))
            .type_style(text::CALLOUT)
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when(active, |d| d.bg(theme.selected).text_color(theme.text))
            .when(!active, |d| {
                d.text_color(theme.secondary)
                    .hover(|s| s.bg(theme.hover))
                    .active(|s| s.bg(theme.pressed))
            })
            .tooltip(move |_, cx| {
                let theme = tooltip_theme;
                let text = if n <= 9 {
                    format!("Tab {n} (⌘⌥{n})").into()
                } else {
                    format!("Tab {n}").into()
                };
                cx.new(|_| Tooltip { text, theme }).into()
            })
            .on_click(cx.listener(move |this, _, window, cx| this.select_tab(i, window, cx)))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(label.clone())),
            )
            .child(close)
    });
    let new_tab = icon_button("new-tab", Icon::Plus, theme, true)
        .debug_selector(|| "new-tab".to_owned())
        .tooltip(move |_, cx| {
            let theme = tooltip_theme;
            cx.new(|_| Tooltip {
                text: "New tab (⌘⌥T)".into(),
                theme,
            })
            .into()
        })
        .on_click(cx.listener(|this, _, window, cx| this.add_tab(window, cx)));
    Some(
        div()
            .flex()
            .flex_row()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(px(space::XS))
            .px(px(space::SM))
            .overflow_hidden()
            .children(chips)
            .child(new_tab),
    )
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
    // Agents keep running through the restart, so the button is an
    // offer, not a warning: accent text, no fill.
    let update = data.update.map(|b| {
        div()
            .id("fleetd-update")
            .debug_selector(|| "fleetd-update".to_owned())
            .flex()
            .flex_none()
            .items_center()
            .h(px(chrome::BAR_HEIGHT - space::LG))
            .px(px(space::SM))
            .rounded(px(radius::SM))
            .border_1()
            .border_color(theme.border)
            .type_style(text::CALLOUT)
            .child(b.label)
            // Not a drag.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when(!b.enabled, |d| d.text_color(theme.secondary))
            .when(b.enabled, |d| {
                d.text_color(theme.accent)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.hover))
                    .active(|s| s.bg(theme.pressed))
                    .tooltip(move |_, cx| {
                        let theme = tooltip_theme;
                        cx.new(|_| Tooltip {
                            text: "Restart fleetd onto the build on disk; agents keep running"
                                .into(),
                            theme,
                        })
                        .into()
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.app().update(cx, |a, cx| a.restart_daemon(cx));
                    }))
            })
    });
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
        .on_mouse_down(MouseButton::Left, drag_window)
        .child(div().flex_none().w(px(lights)))
        .child(target)
        .child(render_tabs(data, theme, cx).unwrap_or_else(|| div().flex_1()))
        .children(update)
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
