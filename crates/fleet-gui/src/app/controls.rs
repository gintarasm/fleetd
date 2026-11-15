//! Shared controls in the macOS style: push buttons, sheet cards, field
//! labels and list rows, drawn with the theme's tokens.

use gpui::prelude::FluentBuilder;
use gpui::{
    Div, ElementId, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString,
    Stateful, StatefulInteractiveElement, Styled, div, px,
};

use super::theme::{Theme, Typography};
use crate::core::style::{chrome, radius, space, text};

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

/// Error text in a sheet.
pub fn error_text(message: impl IntoElement, theme: &Theme) -> Div {
    div()
        .type_style(text::CALLOUT)
        .text_color(theme.danger)
        .child(message)
}

/// A square toolbar button holding a glyph; `enabled: false` draws it
/// dimmed (the caller adds no click handler then). A press on it never
/// reaches what is behind it (the bar's window drag).
pub fn icon_button(
    id: &'static str,
    glyph: &'static str,
    theme: &Theme,
    enabled: bool,
) -> Stateful<Div> {
    let theme = *theme;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(chrome::ICON_BUTTON))
        .rounded(px(radius::MD))
        .text_color(theme.secondary)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(glyph)
        .when(enabled, |b| {
            b.cursor_pointer()
                .hover(move |s| s.bg(theme.hover).text_color(theme.text))
                .active(move |s| s.bg(theme.pressed))
        })
        .when(!enabled, |b| b.text_color(theme.tertiary))
}
