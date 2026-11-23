//! Shared controls in the macOS style: push buttons, sheet cards, field
//! labels and list rows, drawn with the theme's tokens.

use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    Animation, AnimationExt, AnyElement, App, BoxShadow, Context, Div, ElementId, Global,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement, Render,
    SharedString, Stateful, StatefulInteractiveElement, Styled, Window, div, point, px,
};

use super::icons;
use super::theme::{Theme, Typography};
use crate::core::icons::Icon;
use crate::core::keyboard_nav::{FocusMove, focus_move};
use crate::core::style::{chrome, motion, radius, space, text};

/// A plain text tooltip.
pub struct Tooltip {
    pub text: SharedString,
    pub theme: Theme,
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(space::SM))
            .py(px(space::XS))
            .rounded(px(radius::MD))
            .border_1()
            .border_color(self.theme.border)
            .bg(self.theme.raised)
            .shadow_md()
            .text_color(self.theme.text)
            .type_style(text::CALLOUT)
            .child(self.text.clone())
    }
}

/// The default (accent-filled) button or a plain one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    Primary,
    Secondary,
}

/// A push button; `enabled: false` draws it dimmed (the caller adds no
/// click handler then).
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    theme: &Theme,
    kind: ButtonKind,
    enabled: bool,
) -> Stateful<Div> {
    let theme = *theme;
    let b = div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(chrome::CONTROL_HEIGHT))
        .px(px(space::MD))
        .rounded(px(radius::MD))
        .type_style(text::BODY)
        .child(label.into());
    let b = match kind {
        ButtonKind::Primary => b
            .bg(theme.accent)
            .text_color(gpui::white())
            .font_weight(gpui::FontWeight::MEDIUM),
        ButtonKind::Secondary => b
            .border_1()
            .border_color(theme.border)
            .bg(theme.hover)
            .text_color(theme.text),
    };
    if enabled {
        b.cursor_pointer()
            .hover(move |s| match kind {
                ButtonKind::Primary => s.opacity(0.9),
                ButtonKind::Secondary => s.bg(theme.pressed),
            })
            .active(|s| s.opacity(0.8))
    } else {
        b.opacity(0.45).cursor_default()
    }
}

/// The accent halo around a focused control (as around a focused text
/// field).
pub fn focus_ring(theme: &Theme) -> BoxShadow {
    BoxShadow {
        color: theme.accent.opacity(0.35),
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(3.),
    }
}

/// Makes `el` a stop in the Tab order of its sheet or of Settings, drawn
/// with [`focus_ring`] while it has the focus (ADR 0101). Space clicks it.
pub fn tab_stop(el: Stateful<Div>, theme: &Theme) -> Stateful<Div> {
    let ring = focus_ring(theme);
    el.tab_index(0).focus(move |s| s.shadow(vec![ring]))
}

/// Tab and Shift-Tab in a sheet or Settings: the focus moves to the next
/// or previous stop, wrapping. True when the key was one of them.
pub fn move_focus(ev: &KeyDownEvent, window: &mut Window) -> bool {
    let m = &ev.keystroke.modifiers;
    match focus_move(
        &ev.keystroke.key,
        m.shift,
        m.platform || m.control || m.alt || m.function,
    ) {
        Some(FocusMove::Next) => window.focus_next(),
        Some(FocusMove::Prev) => window.focus_prev(),
        None => return false,
    }
    true
}

/// Return never clicks the focused control (gpui does, on key up): in a
/// sheet Return is the default button, in Settings it applies a field.
/// Space clicks it, as on macOS.
pub fn return_is_not_a_click<E: InteractiveElement>(el: E) -> E {
    el.capture_key_up(|ev, window, _| {
        if ev.keystroke.key == "enter" {
            window.prevent_default();
        }
    })
}

/// The card of a sheet over the window.
pub fn sheet_card(id: &'static str, width: f32, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(width))
        .flex()
        .flex_col()
        .gap(px(space::MD))
        .p(px(space::XL - space::XS))
        .rounded(px(radius::LG + 2.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.raised)
        .shadow_lg()
        .text_color(theme.text)
        .type_style(text::BODY)
}

/// A sheet's title.
pub fn sheet_title(title: &'static str) -> Div {
    div().type_style(text::TITLE).child(title)
}

/// A field's label above it.
pub fn field_label(label: &'static str, theme: &Theme) -> Div {
    div()
        .type_style(text::SUBHEADLINE)
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.secondary)
        .child(label)
}

/// A clickable row of a list in a sheet (recent directories, targets).
pub fn list_row(
    id: impl Into<ElementId>,
    label: String,
    theme: &Theme,
    enabled: bool,
) -> Stateful<Div> {
    let theme = *theme;
    div()
        .id(id)
        .px(px(space::SM))
        .py(px(space::XS))
        .rounded(px(radius::MD))
        .type_style(text::CALLOUT)
        .truncate()
        .child(SharedString::from(label))
        .when(enabled, |row| {
            row.cursor_pointer()
                .hover(move |s| s.bg(theme.hover))
                .active(move |s| s.bg(theme.pressed))
        })
        .when(!enabled, |row| row.text_color(theme.tertiary))
}

/// A list in a sheet: rows on a recessed well.
pub fn list_well(id: &'static str, max_height: f32, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .p(px(space::XS))
        .max_h(px(max_height))
        .rounded(px(radius::MD))
        .border_1()
        .border_color(theme.separator)
        .bg(theme.field)
        .overflow_y_scroll()
}

/// A checkbox's box: accent-filled with a check when `checked`.
pub fn check_box(checked: bool, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(14.))
        .rounded(px(radius::SM))
        .border_1()
        .border_color(if checked { theme.accent } else { theme.border })
        .when(checked, |b| {
            b.bg(theme.accent)
                .child(icons::sized(Icon::Check, 11.0).text_color(gpui::white()))
        })
        .when(!checked, |b| b.bg(theme.field))
}

/// Error text in a sheet.
pub fn error_text(message: impl IntoElement, theme: &Theme) -> Div {
    div()
        .type_style(text::CALLOUT)
        .text_color(theme.danger)
        .child(message)
}

/// A square toolbar button holding an icon; `enabled: false` draws it
/// dimmed (the caller adds no click handler then). A press on it never
/// reaches what is behind it (the bar's window drag).
pub fn icon_button(id: &'static str, icon: Icon, theme: &Theme, enabled: bool) -> Stateful<Div> {
    let theme = *theme;
    let glyph = icons::icon(icon)
        .text_color(if enabled {
            theme.secondary
        } else {
            theme.tertiary
        })
        .when(enabled, |g| {
            g.group_hover(ICON_BUTTON_GROUP, move |s| s.text_color(theme.text))
        });
    div()
        .id(id)
        .group(ICON_BUTTON_GROUP)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(chrome::ICON_BUTTON))
        .rounded(px(radius::MD))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(glyph)
        .when(enabled, |b| {
            b.cursor_pointer()
                .hover(move |s| s.bg(theme.hover))
                .active(move |s| s.bg(theme.pressed))
        })
}

/// The hover group of an icon button (the icon brightens with its button).
const ICON_BUTTON_GROUP: &str = "icon-button";

/// Motion is reduced, app-wide: System Settings' Reduce Motion unless
/// `[gui] reduce_motion` overrides it. The workspace keeps it current;
/// panes read it for their own motion (#44).
#[derive(Clone, Copy, Debug, Default)]
pub struct ReducedMotion(pub bool);

impl Global for ReducedMotion {}

/// Motion is reduced (see [`ReducedMotion`]); false until set.
pub fn reduces_motion(cx: &App) -> bool {
    cx.try_global::<ReducedMotion>().is_some_and(|m| m.0)
}

/// `el` fading in while it slides `from` px (down if negative) into place,
/// over `ms` milliseconds, easing out; as is when `reduce_motion`.
/// Restarts each time `id` first appears.
pub fn appear(
    el: Div,
    id: impl Into<ElementId>,
    from: f32,
    ms: u64,
    reduce_motion: bool,
) -> AnyElement {
    if reduce_motion {
        return el.into_any_element();
    }
    el.with_animation(
        id,
        Animation::new(Duration::from_millis(ms)).with_easing(motion::ease_out),
        move |el, t| el.opacity(t).mt(px(from * (1.0 - t))),
    )
    .into_any_element()
}

/// Keeps a view redrawn while something changes over time: the next
/// frame for a fade, one timer for a later change, nothing once idle.
#[derive(Default)]
pub struct RedrawTimer(Option<(Instant, gpui::Task<()>)>);

impl RedrawTimer {
    /// `next`: when the view next changes (`Some(ZERO)`: it is animating),
    /// `None` when it no longer does.
    pub fn schedule<V: 'static>(
        &mut self,
        next: Option<Duration>,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<V>,
    ) {
        let Some(d) = next else {
            self.0 = None;
            return;
        };
        if d.is_zero() {
            self.0 = None;
            window.request_animation_frame();
            return;
        }
        let due = Instant::now() + d;
        // The same change asked for again (within a few ms) keeps its timer.
        let close =
            |at: &Instant| at.max(&due).duration_since(*at.min(&due)) < Duration::from_millis(5);
        if self.0.as_ref().is_some_and(|(at, _)| close(at)) {
            return;
        }
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(d).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        });
        self.0 = Some((due, task));
    }
}
