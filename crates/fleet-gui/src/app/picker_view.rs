//! A fuzzy picker: a query field over a floating list, drawn from
//! [`core::picker::Picker`](crate::core::picker::Picker). ↑/↓ (or ⌃P/⌃N)
//! move the selection, Enter or a click picks, Esc dismisses. The
//! command palette is one; any chooser can be another.

use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FontWeight, HighlightStyle, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, StyledText, Subscription, Window, div, px,
};

use super::text_field::TextField;
use super::theme::{Theme, Typography, current as theme};
use crate::core::picker::{Picker, PickerItem, Ranked, Recents, hit_ranges};
use crate::core::style::{radius, space, text};

/// The list's height before it scrolls.
const LIST_MAX_HEIGHT: f32 = 360.0;
/// The picker's width.
pub const PICKER_WIDTH: f32 = 560.0;

pub enum PickerEvent {
    /// The item with this [`PickerItem::key`] was picked.
    Pick(String),
    Dismiss,
}

pub struct PickerView {
    query: Entity<TextField>,
    picker: Picker,
    scroll: ScrollHandle,
    empty_text: SharedString,
    _query_changed: Subscription,
}

impl EventEmitter<PickerEvent> for PickerView {}

impl PickerView {
    pub fn new(
        items: Vec<PickerItem>,
        recents: Recents,
        placeholder: &str,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| TextField::new(String::new(), placeholder, cx));
        // The field notifies on every edit and caret move.
        let sub = cx.observe(&query, |this: &mut Self, query, cx| {
            let text = query.read(cx).text().to_owned();
            if text != this.picker.query() {
                this.picker.set_query(&text);
                this.scroll.scroll_to_item(0);
                cx.notify();
            }
        });
        PickerView {
            query,
            picker: Picker::new(items, recents),
            scroll: ScrollHandle::new(),
            empty_text: "No matches".into(),
            _query_changed: sub,
        }
    }

    /// Focus goes to the query field.
    pub fn focus(&self, window: &mut Window, cx: &App) {
        window.focus(self.query.read(cx).focus_handle());
    }

    pub fn picker(&self) -> &Picker {
        &self.picker
    }

    /// Picks the selected item, if any.
    pub fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = self.picker.selected_item() {
            cx.emit(PickerEvent::Pick(item.key.clone()));
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.picker.move_selection(delta);
        self.scroll.scroll_to_item(self.picker.selected());
        cx.notify();
    }

    /// Before the field sees them: the keys that drive the list.
    fn key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        let plain = !m.platform && !m.alt && !m.control && !m.shift;
        match (ev.keystroke.key.as_str(), plain, m.control) {
            ("up", true, _) | ("p", false, true) => self.move_selection(-1, cx),
            ("down", true, _) | ("n", false, true) => self.move_selection(1, cx),
            ("enter", true, _) => self.confirm(cx),
            ("escape", true, _) => cx.emit(PickerEvent::Dismiss),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn render_row(
        &self,
        row: usize,
        ranked: &Ranked,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let item = &self.picker.items()[ranked.item];
        let selected = row == self.picker.selected();
        let hit = HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
            color: Some(if selected { gpui::white() } else { t.text }),
            ..HighlightStyle::default()
        };
        let title = StyledText::new(item.title.clone()).with_highlights(
            hit_ranges(&item.title, &ranked.title_hits)
                .into_iter()
                .map(|r| (r, hit)),
        );
        let detail = (!item.detail.is_empty()).then(|| {
            StyledText::new(item.detail.clone()).with_highlights(
                hit_ranges(&item.detail, &ranked.detail_hits)
                    .into_iter()
                    .map(|r| (r, hit)),
            )
        });
        let (fg, dim) = if selected {
            (gpui::white(), gpui::white().opacity(0.8))
        } else {
            (t.text, t.secondary)
        };
        let theme = *t;
        div()
            .id(("picker-row", row))
            .debug_selector(move || format!("picker-row-{row}"))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SM))
            .px(px(space::SM))
            .py(px(space::XS + space::XXS))
            .rounded(px(radius::MD))
            .cursor_pointer()
            .text_color(fg)
            .when(selected, |r| r.bg(theme.accent))
            .when(!selected, |r| r.hover(move |s| s.bg(theme.hover)))
            .child(
                div()
                    .flex_none()
                    .max_w(px(PICKER_WIDTH * 0.6))
                    .truncate()
                    .child(title),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .type_style(text::CALLOUT)
                    .text_color(dim)
                    .children(detail),
            )
            .children(item.shortcut.clone().map(|s| {
                div()
                    .flex_none()
                    .type_style(text::CALLOUT)
                    .text_color(dim)
                    .child(SharedString::from(s))
            }))
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if this.picker.selected() != row {
                    this.picker.select(row);
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.picker.select(row);
                this.confirm(cx);
            }))
    }
}

impl Render for PickerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let ranked: Vec<Ranked> = self.picker.ranked().to_vec();
        let rows: Vec<_> = ranked
            .iter()
            .enumerate()
            .map(|(i, r)| self.render_row(i, r, &t, cx).into_any_element())
            .collect();
        let list = if rows.is_empty() {
            div()
                .id("picker-list")
                .px(px(space::SM))
                .py(px(space::SM))
                .type_style(text::CALLOUT)
                .text_color(t.tertiary)
                .child(self.empty_text.clone())
        } else {
            div()
                .id("picker-list")
                .flex()
                .flex_col()
                .max_h(px(LIST_MAX_HEIGHT))
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .children(rows)
        };
        div()
            .id("picker")
            .debug_selector(|| "picker".to_owned())
            .w(px(PICKER_WIDTH))
            .flex()
            .flex_col()
            .gap(px(space::XS))
            .p(px(space::SM))
            .rounded(px(radius::LG + 2.0))
            .border_1()
            .border_color(t.border)
            .bg(t.raised)
            .shadow_lg()
            .text_color(t.text)
            .type_style(text::BODY)
            .capture_key_down(cx.listener(Self::key_down))
            // Clicks and the wheel stop here, never reaching the panes.
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(self.query.clone())
            .child(div().h(px(1.)).bg(t.separator))
            .child(list)
    }
}
