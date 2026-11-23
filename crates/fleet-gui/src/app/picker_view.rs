//! A fuzzy picker: a query field over a floating list, drawn from
//! [`core::picker::Picker`](crate::core::picker::Picker). ↑/↓ (or ⌃P/⌃N)
//! move the selection, Enter or a click picks, Esc dismisses. The
//! command palette is one; the agent switcher is another, without the
//! query field ([`PickerView::list`]); the Send sheet's agent list is a
//! third, with a checkbox on each row ([`PickerView::multi`]); Search All
//! Agents is a fourth, whose owner lists the rows for each query, under
//! agent headings ([`PickerView::search`], ADR 0098).

use std::collections::HashSet;

use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, FontWeight, HighlightStyle,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement, Render,
    ScrollHandle, SharedString, StatefulInteractiveElement, Styled, StyledText, Subscription,
    Window, div, px,
};

use super::controls::check_box;
use super::text_field::TextField;
use super::theme::{Theme, Typography, current as theme};
use crate::core::picker::{Picker, PickerItem, Ranked, Recents, hit_ranges};
use crate::core::style::{radius, space, text};

/// The list's height before it scrolls.
const LIST_MAX_HEIGHT: f32 = 360.0;
/// The checkbox list's height before it scrolls (inside a sheet).
const MULTI_LIST_MAX_HEIGHT: f32 = 220.0;
/// The picker's width.
pub const PICKER_WIDTH: f32 = 560.0;

pub enum PickerEvent {
    /// The item with this [`PickerItem::key`] was picked.
    Pick(String),
    Dismiss,
}

/// The query of a [`PickerView::search`] changed (its owner lists rows).
pub struct PickerQuery(pub String);

pub struct PickerView {
    query: Entity<TextField>,
    picker: Picker,
    scroll: ScrollHandle,
    empty_text: SharedString,
    /// The query field is drawn and has the keyboard; else the list has.
    show_query: bool,
    /// A list of checkboxes, inside a sheet: the keys checked. A pick
    /// asks the owner to toggle one ([`PickerView::set_checked`]).
    checked: Option<HashSet<String>>,
    /// [`PickerView::search`]: a heading above each row that has one.
    sections: Vec<Option<String>>,
    /// A line under the rows (a search cut short).
    footer: Option<SharedString>,
    focus: FocusHandle,
    _query_changed: Subscription,
}

impl EventEmitter<PickerEvent> for PickerView {}
impl EventEmitter<PickerQuery> for PickerView {}

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
                cx.emit(PickerQuery(text));
                cx.notify();
            }
        });
        PickerView {
            query,
            picker: Picker::new(items, recents),
            scroll: ScrollHandle::new(),
            empty_text: "No matches".into(),
            show_query: true,
            checked: None,
            sections: Vec::new(),
            footer: None,
            focus: cx.focus_handle(),
            _query_changed: sub,
        }
    }

    /// A list without a query field: the keys move and pick, typing does
    /// nothing.
    pub fn list(items: Vec<PickerItem>, recents: Recents, cx: &mut Context<Self>) -> Self {
        PickerView {
            show_query: false,
            ..PickerView::new(items, recents, "", cx)
        }
    }

    /// A list with a checkbox on each row, drawn flat inside a sheet:
    /// Enter or a click picks (the owner toggles the row's box).
    pub fn multi(
        items: Vec<PickerItem>,
        placeholder: &str,
        checked: HashSet<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        PickerView {
            checked: Some(checked),
            ..PickerView::new(items, Recents::default(), placeholder, cx)
        }
    }

    /// A picker whose owner lists the rows for each query
    /// ([`PickerQuery`], [`PickerView::set_listed`]).
    pub fn search(placeholder: &str, status: &str, cx: &mut Context<Self>) -> Self {
        let mut view = PickerView::new(Vec::new(), Recents::default(), placeholder, cx);
        view.picker.set_listed(Vec::new(), Vec::new());
        view.empty_text = SharedString::from(status.to_owned());
        view
    }

    /// Shows `items` in order, each title highlighted at `hits[i]`, row `i`
    /// under the heading `sections[i]` if any; `status` is shown when there
    /// are no rows, else under them.
    pub fn set_listed(
        &mut self,
        items: Vec<PickerItem>,
        hits: Vec<Vec<usize>>,
        sections: Vec<Option<String>>,
        status: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let empty = items.is_empty();
        self.picker.set_listed(items, hits);
        self.sections = sections;
        let status = status.map(SharedString::from);
        if empty {
            self.empty_text = status.unwrap_or_default();
            self.footer = None;
        } else {
            self.footer = status;
        }
        self.scroll.scroll_to_item(0);
        cx.notify();
    }

    /// Each row's heading ([`PickerView::set_listed`]).
    pub fn sections(&self) -> &[Option<String>] {
        &self.sections
    }

    /// The rows checked ([`PickerView::multi`]).
    pub fn set_checked(&mut self, keys: HashSet<String>, cx: &mut Context<Self>) {
        self.checked = Some(keys);
        cx.notify();
    }

    /// The query field's text.
    pub fn query_field(&self) -> &Entity<TextField> {
        &self.query
    }

    /// Focus goes to the query field (or the list, without one).
    pub fn focus(&self, window: &mut Window, cx: &App) {
        if self.show_query {
            window.focus(self.query.read(cx).focus_handle());
        } else {
            window.focus(&self.focus);
        }
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

    /// Moves the selection by `delta` rows, wrapping around.
    pub fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.picker.move_selection(delta);
        self.scroll
            .scroll_to_item(self.child_index(self.picker.selected()));
        cx.notify();
    }

    /// Selects row `row` of the ranked list.
    pub fn select(&mut self, row: usize, cx: &mut Context<Self>) {
        self.picker.select(row);
        self.scroll
            .scroll_to_item(self.child_index(self.picker.selected()));
        cx.notify();
    }

    /// The list child that draws row `row`: headings are children too.
    fn child_index(&self, row: usize) -> usize {
        let headings = self
            .picker
            .ranked()
            .iter()
            .take(row + 1)
            .filter(|r| matches!(self.sections.get(r.item), Some(Some(_))))
            .count();
        row + headings
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
            color: Some(if selected { t.on_accent } else { t.text }),
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
            (t.on_accent, t.on_accent_dim)
        } else {
            (t.text, t.secondary)
        };
        let theme = *t;
        let check = self
            .checked
            .as_ref()
            .map(|keys| check_box(keys.contains(&item.key), t));
        let line = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SM))
            .children(check)
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
            }));
        let preview = (!item.preview.is_empty()).then(|| {
            div()
                .debug_selector(move || format!("picker-preview-{row}"))
                .truncate()
                .type_style(text::CALLOUT)
                .text_color(dim)
                .child(SharedString::from(item.preview.clone()))
        });
        div()
            .id(("picker-row", row))
            .debug_selector(move || format!("picker-row-{row}"))
            .flex()
            .flex_col()
            .px(px(space::SM))
            .py(px(space::XS + space::XXS))
            .rounded(px(radius::MD))
            .cursor_pointer()
            .text_color(fg)
            .when(selected, |r| r.bg(theme.accent))
            .when(!selected, |r| r.hover(move |s| s.bg(theme.hover)))
            .child(line)
            .children(preview)
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
        let mut rows = Vec::with_capacity(ranked.len());
        for (i, r) in ranked.iter().enumerate() {
            if let Some(Some(heading)) = self.sections.get(r.item) {
                rows.push(
                    div()
                        .debug_selector(move || format!("picker-section-{i}"))
                        .px(px(space::SM))
                        .pt(px(if i == 0 { space::XXS } else { space::SM }))
                        .pb(px(space::XXS))
                        .truncate()
                        .type_style(text::CALLOUT)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.secondary)
                        .child(SharedString::from(heading.clone()))
                        .into_any_element(),
                );
            }
            rows.push(self.render_row(i, r, &t, cx).into_any_element());
        }
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
                .max_h(px(if self.checked.is_some() {
                    MULTI_LIST_MAX_HEIGHT
                } else {
                    LIST_MAX_HEIGHT
                }))
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .children(rows)
        };
        let card = div()
            .id("picker")
            .debug_selector(|| "picker".to_owned())
            .flex()
            .flex_col()
            .gap(px(space::XS));
        let card = if self.checked.is_some() {
            // Inside a sheet: no card of its own.
            card.w_full()
        } else {
            card.w(px(PICKER_WIDTH))
                // Narrower inside a small pane (the split chooser's list).
                .max_w_full()
                .p(px(space::SM))
                .rounded(px(radius::LG + 2.0))
                .border_1()
                .border_color(t.border)
                .bg(t.raised)
                .shadow_lg()
        };
        card.text_color(t.text)
            .type_style(text::BODY)
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::key_down))
            // Clicks and the wheel stop here, never reaching the panes.
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when(self.show_query, |card| {
                card.child(self.query.clone())
                    .child(div().h(px(1.)).bg(t.separator))
            })
            .child(list)
            .children(self.footer.clone().filter(|_| !ranked.is_empty()).map(|f| {
                div()
                    .debug_selector(|| "picker-footer".to_owned())
                    .px(px(space::SM))
                    .type_style(text::CALLOUT)
                    .text_color(t.tertiary)
                    .child(f)
            }))
    }
}
