//! Drawing the pane: its card, header, question strip, find bar, banners
//! and the first-run welcome.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    Animation, AnimationExt, BoxShadow, Context, ExternalPaths, Hsla, InteractiveElement,
    IntoElement, KeyContext, ModifiersChangedEvent, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, point, px,
};

use super::{FLASH_MS, PaneEvent, PaneView, unix_ms};
use crate::app::agent_mark::{self, agent_color};
use crate::app::controls::{self, ButtonKind};
use crate::app::ended_view::ended_bar;
use crate::app::icons;
use crate::app::keymap::{FINDING, TERMINAL_CONTEXT};
use crate::app::native;
use crate::app::question_view::{Click, question_block};
use crate::app::status_dot::status_dot;
use crate::app::tail_text::tail_text;
use crate::app::terminal_element::TerminalElement;
use crate::app::theme::{Theme, Typography, current as theme};
use crate::core::cursor_blink;
use crate::core::feedback::{self, Tween};
use crate::core::icons::Icon;
use crate::core::identity::agent_colour;
use crate::core::kill::{AgentButtons, agent_buttons};
use crate::core::pane::PaneState;
use crate::core::pane_chrome::{
    HeaderParts, PaneChrome, banner, draws_cursor, empty_message, empty_state, header_parts,
    restartable,
};
use crate::core::shortcuts::EmptyPane;
use crate::core::style::{self, radius, space, text};

/// The focus ring's colour at full strength: the agent's colour, else the
/// accent.
pub fn focus_ring(chrome: &PaneChrome, theme: &Theme) -> Hsla {
    match chrome.agent {
        Some(id) => agent_color(id, theme),
        None => theme.accent.opacity(0.55),
    }
}

/// The header's terminal icon: in the agent's colour, focused or not, so
/// every pane names its agent; without one, the accent when focused.
pub fn header_icon_color(chrome: &PaneChrome, theme: &Theme) -> Hsla {
    match (chrome.agent, chrome.focused) {
        (Some(id), _) => agent_color(id, theme),
        (None, true) => theme.accent,
        (None, false) => theme.tertiary,
    }
}

/// Width of the find bar's text field.
const FIND_FIELD_WIDTH: f32 = 180.0;

/// The pane header's hover group (its agent buttons show on hover).
const PANE_HEADER_GROUP: &str = "pane-header";

impl Render for PaneView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.follow_activation(window, cx);
        // New output while find is open: scan it (off the UI thread).
        if self.find.is_some() {
            self.continue_search(cx);
        }
        let theme = theme(cx);
        let focused = window.is_window_active() && self.focus.is_focused(window);
        let reduce_motion = controls::reduces_motion(cx);
        let now = Instant::now();
        let (ring, fading) = self.ring(now);
        let dim = feedback::dim_alpha(self.chrome.dims, ring);
        // "Copied" holds then fades; the selection glows then settles.
        let (copied_alpha, glow, copy_moving) = match &self.copied {
            None => (0.0, 0.0, false),
            Some(_) if reduce_motion => (1.0, 0.0, false),
            Some((at, _)) => {
                let flash = Tween::new(*at, feedback::COPIED_MS);
                let glow = Tween::new(*at, feedback::GLOW_MS);
                (
                    feedback::flash_alpha(flash.progress(now)),
                    feedback::glow(glow.value(now)),
                    flash.running(now),
                )
            }
        };
        if fading || copy_moving {
            window.request_animation_frame();
        }
        let (state, title, observing, app_blinks) = {
            let model = self.model();
            let state = model.state();
            let observing = state == PaneState::Live && !model.is_primary();
            let app_blinks = model.terminal().cursor_view().blinking;
            (state, model.title(), observing, app_blinks)
        };
        let show_cursor = draws_cursor(&self.chrome, state);
        let blinks = show_cursor
            && cursor_blink::blinks(self.cursor_blink, app_blinks, focused, reduce_motion);
        self.follow_blink(blinks, cx);
        let scrollbar = self.scrollbar_opacity(window, cx);
        self.scrollbar_shown = scrollbar.is_some();
        let element = TerminalElement {
            view: cx.entity(),
            pane: self.pane,
            model: Arc::clone(&self.model),
            sink: Arc::clone(&self.sink),
            focus: self.focus.clone(),
            theme,
            font: self.font.clone(),
            font_size: self.font_size,
            focused,
            show_cursor,
            cursor_shown: !blinks || self.blink.shown(now),
            marked: self.marked.clone(),
            selection_glow: glow,
            link: self.hover,
            scrollbar,
        };
        let chrome = &self.chrome;
        let HeaderParts {
            title: title_text,
            detail,
            git,
            path,
        } = header_parts(chrome, title.as_deref());
        let icon_button = |id: &'static str, icon: Icon, event: PaneEvent| {
            controls::icon_button(id, icon, &theme, true)
                .debug_selector(move || format!("pane-{}-{id}", chrome.number))
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(event)))
        };
        // Stop for a running agent, Restart and Remove for an ended one:
        // always on the focused pane, on hover on the others.
        let ended = chrome.agent_ended || restartable(chrome, state);
        let agent_buttons = match agent_buttons(chrome.has_agent, ended) {
            AgentButtons::None => Vec::new(),
            AgentButtons::Stop => vec![("stop", Icon::Stop, PaneEvent::Kill)],
            // Restart once the pane holds the final screen (as ⌘R).
            AgentButtons::RestartRemove => restartable(chrome, state)
                .then_some(("restart-agent", Icon::Restart, PaneEvent::Restart))
                .into_iter()
                .chain([("remove", Icon::Trash, PaneEvent::Remove)])
                .collect(),
        };
        let agent_buttons = (!agent_buttons.is_empty()).then(|| {
            div()
                .flex()
                .flex_row()
                .flex_none()
                .children(agent_buttons.into_iter().map(|(id, icon, event)| {
                    icon_button(id, icon, event).when(!chrome.focused, |b| {
                        b.invisible()
                            .group_hover(PANE_HEADER_GROUP, |s| s.visible())
                    })
                }))
        });
        // The agent's open question, between the header and the screen (ADR
        // 0078).
        let question = chrome.question.as_ref().zip(chrome.agent).map(|(q, id)| {
            question_block(
                "pane",
                id,
                q,
                &theme,
                |i| -> Click {
                    Box::new(cx.listener(move |_, _, _, cx| cx.emit(PaneEvent::Choose(i))))
                },
                Box::new(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Answer))),
            )
            .flex_none()
            .m(px(space::XS))
        });
        let header = div()
            .id("pane-header")
            .group(PANE_HEADER_GROUP)
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|_, _, _, cx| cx.emit(PaneEvent::ContextMenu { on_grid: false })),
            )
            .flex()
            .flex_row()
            .flex_none()
            .h(px(style::chrome::PANE_HEADER_HEIGHT))
            .pl(px(space::MD))
            .pr(px(space::XS))
            .items_center()
            .gap(px(space::SM - space::XXS))
            .type_style(text::CALLOUT)
            .child({
                let number = chrome.number;
                match chrome.agent.filter(|_| chrome.agent_emoji) {
                    Some(id) => agent_mark::emoji(id)
                        .debug_selector(move || format!("pane-{number}-identity-emoji"))
                        .into_any_element(),
                    None => {
                        let name = chrome.agent.map(|id| agent_colour(id).name);
                        icons::icon(Icon::Terminal)
                            .text_color(header_icon_color(chrome, &theme))
                            .debug_selector(move || match name {
                                Some(name) => format!("pane-{number}-identity-{name}"),
                                None => format!("pane-{number}-icon"),
                            })
                            .into_any_element()
                    }
                }
            })
            .children(chrome.status.map(|s| {
                let number = chrome.number;
                status_dot(
                    s,
                    chrome.status_fill,
                    &theme,
                    unix_ms(),
                    (self.reduce_motion)(),
                )
                .debug_selector(move || format!("pane-{number}-status-{}", s.label()))
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_w_0()
                    .gap(px(space::SM - space::XXS))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child({
                        let number = chrome.number;
                        let named = chrome.agent.is_some();
                        div()
                            .id("pane-name")
                            .debug_selector(move || format!("pane-{number}-name"))
                            .flex_none()
                            .type_style(text::HEADLINE)
                            .text_color(if chrome.focused {
                                theme.text
                            } else {
                                theme.secondary
                            })
                            .child(SharedString::from(title_text))
                            .on_click(cx.listener(move |_, ev: &gpui::ClickEvent, _, cx| {
                                if named && ev.click_count() == 2 {
                                    cx.emit(PaneEvent::Rename);
                                }
                            }))
                    })
                    .children((!detail.is_empty()).then(|| {
                        let number = chrome.number;
                        div()
                            .debug_selector(move || format!("pane-{number}-detail"))
                            .flex_none()
                            .text_color(theme.tertiary)
                            .child(SharedString::from(detail))
                    }))
                    .children((!git.is_empty()).then(|| {
                        let number = chrome.number;
                        let shown = git.clone();
                        div()
                            .flex()
                            .flex_row()
                            .flex_shrink()
                            .min_w_0()
                            .gap(px(space::XS))
                            .text_color(theme.secondary)
                            .child(div().flex_none().text_color(theme.tertiary).child("·"))
                            .child(
                                div()
                                    .debug_selector(move || format!("pane-{number}-git-{shown}"))
                                    .min_w_0()
                                    .truncate()
                                    .child(SharedString::from(git)),
                            )
                    }))
                    .children((!path.is_empty()).then(|| {
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .min_w_0()
                            .gap(px(space::XS))
                            .text_color(theme.secondary)
                            .child(div().flex_none().text_color(theme.tertiary).child("·"))
                            .child(tail_text(path))
                    })),
            )
            .children(observing.then(|| {
                div()
                    .flex_none()
                    .px(px(space::SM - space::XXS))
                    .rounded(px(radius::SM))
                    .bg(theme.hover)
                    .type_style(text::SUBHEADLINE)
                    .text_color(theme.secondary)
                    .child("observing")
            }))
            .children((chrome.number > 0).then(|| {
                div()
                    .flex_none()
                    // Apart from a long path that ends next to it.
                    .ml(px(space::SM - space::XXS))
                    .mr(px(space::XS))
                    .type_style(text::SUBHEADLINE)
                    .text_color(theme.tertiary)
                    .child(SharedString::from(format!("⌘{}", chrome.number)))
            }))
            .children(agent_buttons)
            .child(icon_button(
                "split-right",
                Icon::SplitRight,
                PaneEvent::SplitRight,
            ))
            .child(icon_button(
                "split-down",
                Icon::SplitDown,
                PaneEvent::SplitDown,
            ))
            .child(icon_button(
                "maximize",
                if chrome.maximized {
                    Icon::Restore
                } else {
                    Icon::Maximize
                },
                PaneEvent::ToggleMaximize,
            ))
            .child(icon_button("close", Icon::Close, PaneEvent::Close));
        let mut grid = div()
            .relative()
            .flex_1()
            .min_h_0()
            .mx(px(space::SM))
            .mb(px(space::SM - space::XXS))
            .overflow_hidden()
            .child(element);
        let find_bar = self.find.as_ref().map(|bar| {
            let number = chrome.number;
            let label = self.find_label().unwrap_or_default();
            let case_on = self.find_case;
            let step = |id: &'static str, icon: Icon, up: bool| {
                controls::icon_button(id, icon, &theme, true)
                    .debug_selector(move || format!("pane-{number}-{id}"))
                    .on_click(cx.listener(move |v, _, _, cx| v.step_find(up, cx)))
            };
            div()
                .id("find")
                .debug_selector(move || format!("pane-{number}-find"))
                .occlude()
                .on_key_down(cx.listener(Self::find_key_down))
                .absolute()
                .top(px(space::XS))
                .right(px(space::XS))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::XS))
                .p(px(space::XS))
                .rounded(px(radius::MD))
                .border_1()
                .border_color(theme.border)
                .bg(theme.raised)
                .shadow_md()
                .type_style(text::CALLOUT)
                .child(div().w(px(FIND_FIELD_WIDTH)).child(bar.field.clone()))
                .child(
                    div()
                        .flex_none()
                        .min_w(px(space::XL * 3.0))
                        .text_color(theme.secondary)
                        .whitespace_nowrap()
                        .child(SharedString::from(label)),
                )
                .child(
                    div()
                        .id("find-case")
                        .debug_selector(move || format!("pane-{number}-find-case"))
                        .flex_none()
                        .px(px(space::XS))
                        .rounded(px(radius::SM))
                        .bg(if case_on {
                            theme.selected
                        } else {
                            gpui::transparent_black()
                        })
                        .text_color(if case_on { theme.text } else { theme.tertiary })
                        .cursor_pointer()
                        .hover(move |s| s.bg(theme.hover))
                        .child("Aa")
                        .on_click(cx.listener(|v, _, _, cx| v.toggle_find_case(cx))),
                )
                .child(step("find-up", Icon::ChevronUp, true))
                .child(step("find-down", Icon::ChevronDown, false))
                .child(
                    controls::icon_button("find-close", Icon::Close, &theme, true)
                        .debug_selector(move || format!("pane-{number}-find-close"))
                        .on_click(cx.listener(|v, _, window, cx| v.close_find(window, cx))),
                )
        });
        let find_open = find_bar.is_some();
        if let Some(text) = banner(chrome, state) {
            // Below the find bar when that is open.
            let top = if find_open {
                space::XS * 4.0 + style::chrome::CONTROL_HEIGHT + space::SM
            } else {
                space::XS
            };
            grid = grid.child(
                div()
                    .absolute()
                    .top(px(top))
                    .right(px(space::XS))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::SM))
                    .px(px(space::MD))
                    .py(px(space::XS))
                    .rounded(px(radius::MD))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.raised)
                    .shadow_md()
                    .text_color(theme.text)
                    .type_style(text::CALLOUT)
                    .child(SharedString::from(text)),
            );
        }
        if self.copied.is_some() {
            let number = chrome.number;
            grid = grid.child(
                div()
                    .absolute()
                    .top(px(space::XS))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .debug_selector(move || format!("pane-{number}-copied"))
                            .px(px(space::SM))
                            .py(px(space::XXS))
                            .rounded(px(radius::MD))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.raised)
                            .shadow_md()
                            .text_color(theme.text)
                            .type_style(text::CALLOUT)
                            .opacity(copied_alpha)
                            .child("Copied"),
                    ),
            );
        }
        let chooser = self.chooser.clone().filter(|_| !chrome.has_agent);
        if chooser.is_none() && empty_state(chrome) == EmptyPane::Welcome {
            grid = grid.child(welcome(&theme, cx));
        }
        if let Some(chooser) = chooser {
            grid = grid.child(chooser);
        } else if let Some(text) = empty_message(chrome) {
            grid = grid.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .type_style(text::BODY)
                    .text_color(theme.tertiary)
                    .child(text),
            );
        }
        grid = grid.children(find_bar);
        // Files from Finder over the pane: an accent outline (and tint),
        // as for an agent dragged here (ADR 0059), and the drop lands
        // here. Built only while something is dragged.
        let drop_zone = cx.has_active_drag().then(|| {
            let accent = theme.accent;
            let number = chrome.number;
            div()
                .debug_selector(move || format!("pane-{number}-file-drop"))
                .absolute()
                .inset_0()
                .rounded(px(radius::LG))
                .drag_over::<ExternalPaths>(move |s, _, _, _| {
                    let s = s.border_2().border_color(accent);
                    if native::reduce_transparency() {
                        s
                    } else {
                        s.bg(accent.opacity(0.16))
                    }
                })
                .on_drop(cx.listener(Self::drop_paths))
        });
        div()
            .key_context({
                // ⌘⇧G is Find Previous only while finding (ADR 0073).
                let mut kc = KeyContext::new_with_defaults();
                kc.add(TERMINAL_CONTEXT);
                if self.find.is_some() {
                    kc.add(FINDING);
                }
                kc
            })
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            // Keyboard focus path only: ⌘ pressed over an unfocused pane shows
            // its link on the next pointer move.
            .on_modifiers_changed(cx.listener(|v, ev: &ModifiersChangedEvent, window, cx| {
                v.modifiers_changed(ev.modifiers, window.mouse_position(), cx);
            }))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::open_find))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_previous))
            .on_action(cx.listener(Self::scroll_page_up))
            .on_action(cx.listener(Self::scroll_page_down))
            .on_action(cx.listener(Self::scroll_to_top))
            .on_action(cx.listener(Self::scroll_to_bottom))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::clear_scrollback))
            .size_full()
            .flex()
            .flex_col()
            .rounded(px(radius::LG))
            .overflow_hidden()
            // A card lifted off the window's material.
            .shadow(vec![BoxShadow {
                color: theme.shadow,
                offset: point(px(0.), px(1.)),
                blur_radius: px(space::XS),
                spread_radius: px(0.),
            }])
            .border_1()
            .border_color(ring_color(&theme, focus_ring(chrome, &theme), ring))
            .bg(theme.card)
            .relative()
            .when(!self.headless, |d| d.child(header))
            .children(question.filter(|_| !self.headless))
            .child(grid)
            .children(self.flash.as_ref().map(|(n, _)| {
                let ring = div()
                    .absolute()
                    .inset_0()
                    .rounded(px(radius::LG))
                    .border_2()
                    .border_color(theme.warning);
                if (self.reduce_motion)() {
                    ring.into_any_element()
                } else {
                    ring.with_animation(
                        ("bell-flash", *n),
                        Animation::new(Duration::from_millis(FLASH_MS)),
                        |el, t| el.opacity(1.0 - t),
                    )
                    .into_any_element()
                }
            }))
            .children(ended_bar(self, state, &theme, cx))
            .children(drop_zone)
            .children((dim > 0.0).then(|| {
                // The veil only draws: clicks and keys go to the pane.
                div()
                    .debug_selector(move || format!("pane-{}-dim", chrome.number))
                    .absolute()
                    .inset_0()
                    .bg(theme.window.opacity(dim))
            }))
    }
}

/// The card's border at focus-ring strength `ring`: the plain border at
/// 0, the focus ring (`lit`) at 1, cross-faded between.
pub(super) fn ring_color(theme: &crate::app::theme::Theme, lit: Hsla, ring: f32) -> Hsla {
    if ring >= 1.0 {
        lit
    } else if ring <= 0.0 {
        theme.border
    } else {
        theme.border.blend(lit.opacity(ring))
    }
}

/// The first-run welcome (no agents yet): New agent, Terminal, Install
/// agent skill and the shortcuts, centred in the empty pane.
fn welcome(theme: &Theme, cx: &mut Context<PaneView>) -> impl IntoElement {
    let button = |id: &'static str, label: &'static str, kind, event: PaneEvent| {
        controls::button(id, label, theme, kind, true)
            .debug_selector(move || format!("welcome-{id}"))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(event)))
    };
    let new_agent = button(
        "new-agent",
        "New Agent  ⌘N",
        ButtonKind::Primary,
        PaneEvent::NewAgent,
    );
    let terminal = button(
        "terminal",
        "Terminal  ⌘T",
        ButtonKind::Secondary,
        PaneEvent::NewTerminal,
    );
    let skill = button(
        "install-skill",
        "Install Agent Skill",
        ButtonKind::Secondary,
        PaneEvent::InstallSkill,
    );
    let shortcuts = div()
        .id("shortcuts")
        .debug_selector(|| "welcome-shortcuts".to_owned())
        .type_style(text::CALLOUT)
        .text_color(theme.accent)
        .cursor_pointer()
        .hover(|s| s.opacity(0.8))
        .child("Keyboard shortcuts  ⌘/")
        .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::ShowShortcuts)));
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .id("welcome")
                .debug_selector(|| "welcome".to_owned())
                .flex()
                .flex_col()
                .items_center()
                .gap(px(space::MD))
                .text_color(theme.text)
                .child(div().type_style(text::TITLE).child("Welcome to fleet"))
                .child(
                    div()
                        .type_style(text::BODY)
                        .text_color(theme.secondary)
                        .child("Start an agent, or a plain shell, to get going."),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(space::SM))
                        .child(new_agent)
                        .child(terminal)
                        .child(skill),
                )
                .child(shortcuts),
        )
}
