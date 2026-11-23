//! An agent's open question as the sidebar row and the pane header show it
//! (ADR 0078): the text, one button per choice, and Reply… for the free
//! text (the Answer sheet). The graph card shows the text and opens the
//! same sheet.

use gpui::{
    ClickEvent, Div, InteractiveElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Window, div, px,
};

use super::controls::{ButtonKind, button};
use super::theme::{Theme, Typography};
use crate::core::question::OpenQuestion;
use crate::core::style::{radius, space, text};

/// A click handler, boxed so the sidebar's and the pane's contexts can both
/// supply theirs.
pub type Click = Box<dyn Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static>;

/// The question block of agent `id`; `where_` names the surface (`row`,
/// `pane`) in element ids. `choice(i)` handles a click on choice `i`,
/// `reply` the click on Reply….
pub fn question_block(
    where_: &'static str,
    id: u64,
    q: &OpenQuestion,
    theme: &Theme,
    choice: impl Fn(usize) -> Click,
    reply: Click,
) -> Div {
    let buttons = q.choices.iter().enumerate().map(|(i, c)| {
        button(
            SharedString::from(format!("{where_}-choice-{id}-{i}")),
            SharedString::from(c.clone()),
            theme,
            ButtonKind::Secondary,
            true,
        )
        .debug_selector({
            let label = c.clone();
            move || format!("{where_}-choice-{id}-{label}")
        })
        .on_click(choice(i))
    });
    let reply = button(
        SharedString::from(format!("{where_}-reply-{id}")),
        "Reply…",
        theme,
        ButtonKind::Primary,
        true,
    )
    .debug_selector(move || format!("{where_}-reply-{id}"))
    .on_click(reply);
    div()
        .flex()
        .flex_col()
        .gap(px(space::XS))
        .p(px(space::SM))
        .rounded(px(radius::MD))
        .bg(theme.warning_tint)
        .child(
            div()
                .debug_selector({
                    let shown = q.text.clone();
                    move || format!("{where_}-question-{id}-{shown}")
                })
                .type_style(text::SUBHEADLINE)
                .text_color(theme.text)
                .child(SharedString::from(q.text.clone())),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(space::XS))
                .children(buttons)
                .child(reply),
        )
}
