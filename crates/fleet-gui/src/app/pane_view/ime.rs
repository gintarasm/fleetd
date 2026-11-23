//! The pane's IME: marked text and where the candidate window goes.

use std::ops::Range;

use fleet_term::CursorShape;
use gpui::{Bounds, Context, Pixels, Point, UTF16Selection, Window, point, size};

use super::PaneView;
use crate::core::pane::PaneModel;

/// Where the IME's marked text goes (and its candidate window): the
/// cursor cell when the cursor shows, else the top left. `(col, row)`.
pub(crate) fn ime_cell(model: &PaneModel) -> (u16, u16) {
    let cursor = model.terminal().cursor_view();
    if model.scroll_offset() == 0 && cursor.shape != CursorShape::Hidden {
        (cursor.col, cursor.row)
    } else {
        (0, 0)
    }
}

impl PaneView {
    /// The marked-text cell ([`ime_cell`]) in window coordinates.
    fn ime_bounds(&self) -> Option<Bounds<Pixels>> {
        let layout = self.layout?;
        let (col, row) = ime_cell(&self.model());
        Some(Bounds::new(
            point(
                layout.origin.x + layout.cell_w * f32::from(col),
                layout.origin.y + layout.line_h * f32::from(row),
            ),
            size(layout.cell_w, layout.line_h),
        ))
    }
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

impl gpui::EntityInputHandler for PaneView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        // The only text the IME can see is its own marked text.
        let marked: Vec<u16> = self
            .marked
            .as_deref()
            .unwrap_or("")
            .encode_utf16()
            .collect();
        let start = range.start.min(marked.len());
        let end = range.end.clamp(start, marked.len());
        *adjusted_range = Some(start..end);
        Some(String::from_utf16_lossy(&marked[start..end]))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.marked.as_deref().map_or(0, utf16_len);
        Some(UTF16Selection {
            range: self.marked_selection.clone().unwrap_or(end..end),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked.as_deref().map(|m| 0..utf16_len(m))
    }

    /// The IME gave up composing: what it had marked is committed.
    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked_selection = None;
        if let Some(text) = self.marked.take() {
            cx.notify();
            self.send(text.into_bytes(), cx);
        }
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.marked.take().is_some() {
            self.marked_selection = None;
            cx.notify();
        }
        self.send(text.as_bytes().to_vec(), cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if new_text.is_empty() {
            self.marked = None;
            self.marked_selection = None;
        } else {
            self.marked = Some(new_text.to_owned());
            self.marked_selection = new_selected_range;
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.ime_bounds()
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}
