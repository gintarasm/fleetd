//! One line of text that, when too wide, keeps its end behind a leading
//! `…` (see [`tail_start`]). Takes the width its parent leaves it.

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, ShapedLine, SharedString, Style, Window, relative,
};

use crate::core::truncate::tail_start;

const ELLIPSIS: &str = "…";

pub struct TailText {
    text: SharedString,
}

/// `text` on one line, shortened from the start to fit.
pub fn tail_text(text: impl Into<SharedString>) -> TailText {
    TailText { text: text.into() }
}

/// What `text` shows in `max_w`, given its shaped width at each index.
fn fitted(
    text: &SharedString,
    max_w: Pixels,
    line: &ShapedLine,
    ellipsis_w: Pixels,
) -> SharedString {
    let total = line.width;
    let width_from = |i: usize| f32::from(total - line.x_for_index(i));
    match tail_start(text, max_w.into(), ellipsis_w.into(), width_from) {
        None => text.clone(),
        Some(i) => SharedString::from(format!("{ELLIPSIS}{}", &text[i..])),
    }
}

impl IntoElement for TailText {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for TailText {
    type RequestLayoutState = ();
    type PrepaintState = Option<ShapedLine>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let line_h = window.text_style().line_height_in_pixels(window.rem_size());
        // Whatever width the row leaves, never pushing its neighbours.
        let mut style = Style {
            flex_grow: 1.,
            flex_shrink: 1.,
            flex_basis: relative(0.).into(),
            ..Style::default()
        };
        style.min_size.width = Pixels::ZERO.into();
        style.size.height = line_h.into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) -> Option<ShapedLine> {
        if self.text.is_empty() {
            return None;
        }
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let shape = |text: SharedString| {
            let run = style.to_run(text.len());
            window
                .text_system()
                .shape_line(text, font_size, &[run], None)
        };
        let full = shape(self.text.clone());
        let ellipsis_w = shape(SharedString::new_static(ELLIPSIS)).width;
        let shown = fitted(&self.text, bounds.size.width, &full, ellipsis_w);
        Some(if shown == self.text {
            full
        } else {
            shape(shown)
        })
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        line: &mut Option<ShapedLine>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(line) = line {
            let _ = line.paint(bounds.origin, bounds.size.height, window, cx);
        }
    }
}
