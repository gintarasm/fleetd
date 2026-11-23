//! The summary bar at the bottom of a pane whose agent ended (issue #45):
//! exit code in the success or failure colour, run time, and Restart, Copy
//! last output and Remove. The logic is in [`crate::core::ended`]; this
//! only draws it. No motion, and only opaque theme fills (Reduce Motion and
//! Reduce Transparency need nothing more).

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, ClipboardItem, Context, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, div, px,
};

use super::icons;
use super::pane_view::{PaneEvent, PaneView};
use super::theme::{Theme, Typography};
use crate::core::ended::{EndedSummary, last_output};
use crate::core::icons::Icon;
use crate::core::pane::PaneState;
use crate::core::style::{grid, radius, space, text};

/// Height of the bar's buttons.
const BUTTON_HEIGHT: f32 = grid(6);

/// The summary bar for a pane in `state`, if its agent ended.
pub(crate) fn ended_bar(
    view: &PaneView,
    state: PaneState,
    theme: &Theme,
    cx: &mut Context<PaneView>,
) -> Option<AnyElement> {
    let chrome = &view.chrome;
    if !chrome.has_agent {
        return None;
    }
    let summary = EndedSummary::of(state, chrome.run_time)?;
    let number = chrome.number;
    let tone = if summary.succeeded() {
        theme.success
    } else {
        theme.danger
    };
    let button = |id: &'static str, icon: Option<Icon>, label: &'static str, primary: bool| {
        let fg = if primary { theme.on_accent } else { theme.text };
        div()
            .id(id)
            .debug_selector(move || format!("pane-{number}-{id}"))
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(px(space::XS))
            .h(px(BUTTON_HEIGHT))
            .px(px(space::SM))
            .rounded(px(radius::SM))
            .type_style(text::CALLOUT)
            .text_color(fg)
            .cursor_pointer()
            .map(|b| {
                if primary {
                    b.bg(theme.accent)
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .hover(|s| s.opacity(0.9))
                } else {
                    let pressed = theme.pressed;
                    b.border_1()
                        .border_color(theme.border)
                        .bg(theme.hover)
                        .hover(move |s| s.bg(pressed))
                }
            })
            .active(|s| s.opacity(0.8))
            .children(icon.map(|i| icons::sized(i, 12.0).text_color(fg)))
            .child(label)
    };
    let bar = div()
        .id("ended-summary")
        .debug_selector(move || format!("pane-{number}-ended"))
        .flex()
        .flex_row()
        .flex_wrap()
        .flex_none()
        .items_center()
        .gap(px(space::SM))
        .mx(px(space::SM))
        .mb(px(space::SM))
        .pl(px(space::MD))
        .pr(px(space::XS))
        .py(px(space::XS))
        .rounded(px(radius::MD))
        .border_1()
        .border_color(tone.opacity(0.45))
        .bg(theme.raised)
        .type_style(text::CALLOUT)
        .child(
            div()
                .flex_none()
                .size(px(space::SM))
                .rounded_full()
                .bg(tone),
        )
        .child(
            div()
                .flex_none()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(tone)
                .child(SharedString::from(summary.headline())),
        )
        .children(summary.run_time_text().map(|t| {
            div()
                .flex_none()
                .text_color(theme.secondary)
                .child(SharedString::from(t))
        }))
        .child(div().flex_1())
        .child(
            button("restart", Some(Icon::Restart), "Restart", true)
                .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Restart))),
        )
        .when(chrome.show_changes, |bar| {
            bar.child(
                button("show-changes", None, "Show Changes", false)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::ShowChanges))),
            )
        })
        .child(
            button("copy-output", None, "Copy last output", false)
                .on_click(cx.listener(|view, _, _, cx| copy_last_output(view, cx))),
        )
        .child(
            button("remove", Some(Icon::Trash), "Remove", false)
                .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Remove))),
        );
    Some(bar.into_any_element())
}

/// Copies the pane's last screenful (its grid, not the scrolled view).
fn copy_last_output(view: &mut PaneView, cx: &mut Context<PaneView>) {
    let rows = {
        let model = view
            .model_handle()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        model.terminal().visible_text()
    };
    if let Some(text) = last_output(&rows) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
}
