//! A terminal pane view: keyboard, IME and paste input for one pane, drawn
//! by [`TerminalElement`].

use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard};

use gpui::{
    App, Bounds, ClipboardItem, Context, FocusHandle, Font, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Pixels, Point, Render, Styled, Subscription, UTF16Selection,
    Window, div, font, point, px, size,
};

use super::keymap::{Copy, Paste, TERMINAL_CONTEXT, to_key};
use super::terminal_element::TerminalElement;
use super::theme::Theme;
use crate::core::config::GuiConfig;
use crate::core::keys::{encode_key, encode_paste};
use crate::core::pane::PaneModel;
use crate::core::session::{PaneId, Session};

/// Where the pane's input goes: the [`Session`], or a recording fake in
/// tests.
pub trait InputSink: Send + Sync {
    fn input(&self, pane: PaneId, bytes: Vec<u8>);
    fn resize(&self, pane: PaneId, cols: u16, rows: u16);
}

impl InputSink for Session {
    fn input(&self, pane: PaneId, bytes: Vec<u8>) {
        Session::input(self, pane, bytes);
    }

    fn resize(&self, pane: PaneId, cols: u16, rows: u16) {
        Session::resize(self, pane, cols, rows);
    }
}

/// `(cols, rows)` of whole cells that fit, at least 1×1.
pub fn grid_size(bounds_w: f32, bounds_h: f32, cell_w: f32, line_h: f32) -> (u16, u16) {
    let fit = |len: f32, cell: f32| {
        if cell > 0.0 && len.is_finite() {
            (len / cell).floor().clamp(1.0, f32::from(u16::MAX)) as u16
        } else {
            1
        }
    };
    (fit(bounds_w, cell_w), fit(bounds_h, line_h))
}

/// The grid as last laid out: where it is and how big its cells are.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GridLayout {
    pub origin: Point<Pixels>,
    pub cell_w: Pixels,
    pub line_h: Pixels,
    pub cols: u16,
    pub rows: u16,
}

pub struct PaneView {
    pane: PaneId,
    model: Arc<Mutex<PaneModel>>,
    sink: Arc<dyn InputSink>,
    focus: FocusHandle,
    option_as_meta: bool,
    font: Font,
    font_size: Pixels,
    /// IME preedit text, not yet sent.
    marked: Option<String>,
    marked_selection: Option<Range<usize>>,
    pub(crate) layout: Option<GridLayout>,
    _activation: Subscription,
}

impl PaneView {
    pub fn new(
        pane: PaneId,
        model: Arc<Mutex<PaneModel>>,
        sink: Arc<dyn InputSink>,
        config: &GuiConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // The cursor turns hollow when the window loses activation.
        let activation = cx.observe_window_activation(window, |_, _, cx| cx.notify());
        PaneView {
            pane,
            model,
            sink,
            focus: cx.focus_handle(),
            option_as_meta: config.option_as_meta,
            font: font(config.font_family.clone()),
            font_size: px(config.font_size),
            marked: None,
            marked_selection: None,
            layout: None,
            _activation: activation,
        }
    }

    pub fn pane(&self) -> PaneId {
        self.pane
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn model(&self) -> MutexGuard<'_, PaneModel> {
        self.model.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sends input and returns the view to the live grid.
    fn send(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if bytes.is_empty() {
            return;
        }
        self.sink.input(self.pane, bytes);
        let mut model = self.model();
        if model.scroll_offset() > 0 {
            model.scroll_to_bottom();
            cx.notify();
        }
    }

    fn key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // While composing, the IME owns the keys.
        if self.marked.is_some() {
            return;
        }
        let Some(key) = to_key(&ev.keystroke) else {
            return;
        };
        let modes = self.model().terminal().modes();
        // `None`: the key is text; it reaches `replace_text_in_range`.
        if let Some(bytes) = encode_key(&key, &modes, self.option_as_meta) {
            self.send(bytes, cx);
            cx.stop_propagation();
        }
    }

    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.model().selection_text();
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let modes = self.model().terminal().modes();
        self.send(encode_paste(&text, &modes), cx);
    }

    /// The cursor cell in window coordinates.
    fn cursor_bounds(&self) -> Option<Bounds<Pixels>> {
        let layout = self.layout?;
        let cursor = self.model().terminal().cursor_view();
        Some(Bounds::new(
            point(
                layout.origin.x + layout.cell_w * f32::from(cursor.col),
                layout.origin.y + layout.line_h * f32::from(cursor.row),
            ),
            size(layout.cell_w, layout.line_h),
        ))
    }
}

impl Render for PaneView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let focused = window.is_window_active() && self.focus.is_focused(window);
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
            marked: self.marked.clone(),
        };
        div()
            .key_context(TERMINAL_CONTEXT)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .size_full()
            .bg(theme.bg())
            .child(element)
    }
}

fn theme(cx: &App) -> Theme {
    cx.try_global::<Theme>()
        .copied()
        .unwrap_or_else(Theme::dark)
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

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        self.marked_selection = None;
        cx.notify();
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
        self.cursor_bounds()
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gpui::{ClipboardItem, Entity, EntityInputHandler, TestAppContext, VisualTestContext};

    use super::{InputSink, PaneView, grid_size};
    use crate::app::keymap::{Paste, bind_keys};
    use crate::core::config::GuiConfig;
    use crate::core::pane::PaneModel;
    use crate::core::selection::{LineRef, SelPoint};
    use crate::core::session::PaneId;
    use fleet_proto::AttachOk;

    #[derive(Default)]
    struct Recorder {
        inputs: Mutex<Vec<Vec<u8>>>,
        resizes: Mutex<Vec<(u16, u16)>>,
    }

    impl InputSink for Recorder {
        fn input(&self, _pane: PaneId, bytes: Vec<u8>) {
            self.inputs.lock().unwrap().push(bytes);
        }

        fn resize(&self, _pane: PaneId, cols: u16, rows: u16) {
            self.resizes.lock().unwrap().push((cols, rows));
        }
    }

    impl Recorder {
        fn take(&self) -> Vec<Vec<u8>> {
            std::mem::take(&mut *self.inputs.lock().unwrap())
        }
    }

    fn pane(
        cx: &mut TestAppContext,
        model: PaneModel,
    ) -> (Entity<PaneView>, Arc<Recorder>, &mut VisualTestContext) {
        cx.update(bind_keys);
        let sink = Arc::new(Recorder::default());
        let model = Arc::new(Mutex::new(model));
        let sink_dyn: Arc<dyn InputSink> = sink.clone();
        let (view, cx) = cx.add_window_view(|window, cx| {
            PaneView::new(
                PaneId(1),
                model,
                sink_dyn,
                &GuiConfig::default(),
                window,
                cx,
            )
        });
        cx.update(|window, cx| window.focus(&view.read(cx).focus_handle().clone()));
        cx.run_until_parked();
        (view, sink, cx)
    }

    fn blank_model() -> PaneModel {
        PaneModel::new(crate::app::theme::Theme::dark().palette)
    }

    #[gpui::test]
    fn typing_sends_bytes_through_session(cx: &mut TestAppContext) {
        let (_view, sink, cx) = pane(cx, blank_model());
        sink.take(); // nothing typed yet
        cx.simulate_keystrokes("a enter alt-b ctrl-c");
        assert_eq!(
            sink.take(),
            vec![
                b"a".to_vec(),
                b"\r".to_vec(),
                b"\x1bb".to_vec(),
                b"\x03".to_vec()
            ]
        );
    }

    #[gpui::test]
    fn ime_marked_text_is_not_sent_until_committed(cx: &mut TestAppContext) {
        let (view, sink, cx) = pane(cx, blank_model());
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.replace_and_mark_text_in_range(None, "に", None, window, cx);
            });
        });
        assert!(sink.take().is_empty());
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                assert_eq!(v.marked_text_range(window, cx), Some(0..1));
                v.replace_text_in_range(None, "日本", window, cx);
                assert_eq!(v.marked_text_range(window, cx), None);
            });
        });
        assert_eq!(sink.take(), vec!["日本".as_bytes().to_vec()]);
    }

    #[gpui::test]
    fn paste_uses_bracketed_mode(cx: &mut TestAppContext) {
        let mut model = blank_model();
        model.on_output(b"\x1b[?2004h");
        assert!(model.terminal().modes().bracketed_paste);
        let (_view, sink, cx) = pane(cx, model);
        cx.write_to_clipboard(ClipboardItem::new_string("x\ny".to_owned()));
        cx.dispatch_action(Paste);
        assert_eq!(sink.take(), vec![b"\x1b[200~x\ry\x1b[201~".to_vec()]);
    }

    #[gpui::test]
    fn layout_resizes_the_session(cx: &mut TestAppContext) {
        let (view, sink, cx) = pane(cx, blank_model());
        let layout = view.read_with(cx, |v, _| v.layout).expect("laid out");
        let resizes = sink.resizes.lock().unwrap().clone();
        assert_eq!(resizes, vec![(layout.cols, layout.rows)]);
        // Not primary: the local grid keeps the agent's size.
        let grid = view.read_with(cx, |v, _| v.model().terminal().size());
        assert_eq!(grid, (80, 24));
    }

    #[gpui::test]
    fn primary_resizes_its_local_grid(cx: &mut TestAppContext) {
        let mut model = blank_model();
        model.on_attached(&AttachOk {
            agent_id: 1,
            cols: 80,
            rows: 24,
            primary: true,
            ..Default::default()
        });
        let (view, _sink, cx) = pane(cx, model);
        let (layout, grid) = view.read_with(cx, |v, _| (v.layout, v.model().terminal().size()));
        let layout = layout.expect("laid out");
        assert_eq!(grid, (layout.cols, layout.rows));
    }

    #[gpui::test]
    fn paints_styled_wide_selected_and_marked_cells(cx: &mut TestAppContext) {
        let mut model = blank_model();
        model.on_output(
            "\x1b[1;31mred\x1b[0m \x1b[7m日本\x1b[0m e\u{301} \x1b[4;9mx\x1b[0m".as_bytes(),
        );
        model.select_start(SelPoint {
            line: LineRef::Grid(0),
            col: 1,
        });
        model.select_extend(SelPoint {
            line: LineRef::Grid(0),
            col: 6,
        });
        let (view, _sink, cx) = pane(cx, model);
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.replace_and_mark_text_in_range(None, "に", None, window, cx);
            });
        });
        cx.run_until_parked();
        let marked = view.read_with(cx, |v, _| v.marked.clone());
        assert_eq!(marked.as_deref(), Some("に"));
    }

    #[test]
    fn resize_computes_cols_rows_from_cell_metrics() {
        assert_eq!(grid_size(800.0, 600.0, 8.0, 16.0), (100, 37));
        assert_eq!(grid_size(805.9, 607.9, 8.0, 16.0), (100, 37));
        assert_eq!(grid_size(3.0, 2.0, 8.0, 16.0), (1, 1));
        assert_eq!(grid_size(0.0, 0.0, 8.0, 16.0), (1, 1));
        assert_eq!(grid_size(100.0, 100.0, 0.0, 0.0), (1, 1));
    }
}
