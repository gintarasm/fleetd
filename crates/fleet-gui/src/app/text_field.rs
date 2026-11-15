//! A one-line text field that edits like an `NSTextField`: a caret that
//! blinks, mouse and Shift selection (double click: a word, triple: all),
//! ⌥ word and ⌘ line moves, ⌘A, ⌘C/⌘X/⌘V, ⌘Z/⇧⌘Z, the Emacs keys
//! Cocoa supports (⌃A ⌃E ⌃B ⌃F ⌃D ⌃H ⌃K), and IME composition. Editing
//! is [`TextEdit`]; this draws it and turns events into its commands.

use std::ops::Range;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::{
    App, Bounds, BoxShadow, ClipboardItem, ContentMask, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, GlobalElementId,
    InspectorElementId, InteractiveElement, IntoElement, KeyDownEvent, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, ShapedLine,
    SharedString, Style, Styled, Task, TextRun, UTF16Selection, UnderlineStyle, Window, div, fill,
    point, px, relative, size,
};

use super::keymap::{Copy, Cut, Paste, Redo, SelectAll, TEXT_FIELD_CONTEXT, Undo};
use super::theme::{Typography, current as theme};
use crate::core::style::{chrome, radius, space, text};
use crate::core::text_edit::{Movement, TextEdit};

/// How long the caret is shown, then hidden, while it blinks (AppKit's
/// default period).
const BLINK: Duration = Duration::from_millis(530);

pub struct TextField {
    edit: TextEdit,
    focus: FocusHandle,
    placeholder: SharedString,
    /// The last frame's shaped text and where it was drawn: clicks and
    /// the IME's candidate window are placed by it.
    last: Option<(ShapedLine, Bounds<Pixels>)>,
    /// How far the text is scrolled left to keep the caret in view.
    scroll_x: Pixels,
    /// A left button drag is extending the selection.
    selecting: bool,
    /// The blinking caret is in its shown half.
    caret_on: bool,
    /// The field was focused when last drawn (only then does it blink).
    drawn_focused: bool,
    _blink: Task<()>,
}

impl TextField {
    pub fn new(text: String, placeholder: &str, cx: &mut Context<Self>) -> Self {
        let blink = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK).await;
                let alive = this.update(cx, |f, cx| {
                    if f.drawn_focused {
                        f.caret_on = !f.caret_on;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        TextField {
            edit: TextEdit::new(text),
            focus: cx.focus_handle(),
            placeholder: SharedString::from(placeholder.to_owned()),
            last: None,
            scroll_x: px(0.),
            selecting: false,
            caret_on: true,
            drawn_focused: false,
            _blink: blink,
        }
    }

    pub fn text(&self) -> &str {
        self.edit.text()
    }

    /// Replaces the text (a pick from a list); ⌘Z brings the old back.
    pub fn set_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.edit.set_text(&text);
        self.changed(cx);
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// After any edit or caret move: the caret shows at once.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.caret_on = true;
        cx.notify();
    }

    fn key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // While composing, the IME owns the keys.
        if self.edit.marked().is_some() {
            return;
        }
        let m = &ev.keystroke.modifiers;
        let e = &mut self.edit;
        let moved = |m_plain, m_alt, m_cmd| {
            if ev.keystroke.modifiers.platform {
                m_cmd
            } else if ev.keystroke.modifiers.alt {
                m_alt
            } else {
                m_plain
            }
        };
        let handled = match (ev.keystroke.key.as_str(), m.control) {
            ("left", false) => {
                e.move_caret(
                    moved(Movement::Left, Movement::WordLeft, Movement::Home),
                    m.shift,
                );
                true
            }
            ("right", false) => {
                e.move_caret(
                    moved(Movement::Right, Movement::WordRight, Movement::End),
                    m.shift,
                );
                true
            }
            ("up" | "home", false) | ("a", true) => {
                e.move_caret(Movement::Home, m.shift);
                true
            }
            ("down" | "end", false) | ("e", true) => {
                e.move_caret(Movement::End, m.shift);
                true
            }
            ("b", true) => {
                e.move_caret(Movement::Left, false);
                true
            }
            ("f", true) => {
                e.move_caret(Movement::Right, false);
                true
            }
            ("backspace", false) if m.platform => {
                e.delete_to_start();
                true
            }
            ("backspace", false) if m.alt => {
                e.delete_word_back();
                true
            }
            ("backspace", _) | ("h", true) => {
                e.backspace();
                true
            }
            ("delete", _) | ("d", true) => {
                e.delete_forward();
                true
            }
            ("k", true) => {
                e.delete_to_end();
                true
            }
            _ => false,
        };
        if handled {
            self.changed(cx);
            cx.stop_propagation();
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.edit.select_all();
        self.changed(cx);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.edit.selected_text();
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
        }
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.edit.cut() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.changed(cx);
        }
    }

    /// Pastes the clipboard's first line (the field is one line).
    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
            let line = text.lines().next().unwrap_or("");
            self.edit.unmark();
            self.edit.insert(line);
            self.changed(cx);
        }
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if self.edit.undo() {
            self.changed(cx);
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if self.edit.redo() {
            self.changed(cx);
        }
    }

    /// The byte offset under window x `x` in the last frame's text.
    fn index_at(&self, x: Pixels) -> Option<usize> {
        let (line, bounds) = self.last.as_ref()?;
        if self.edit.text().is_empty() {
            return Some(0);
        }
        Some(line.closest_index_for_x(x - bounds.left() + self.scroll_x))
    }

    fn mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let Some(at) = self.index_at(ev.position.x) else {
            return;
        };
        match ev.click_count {
            2 => self.edit.select_word_at(at),
            n if n >= 3 => self.edit.select_all(),
            _ => self.edit.set_caret(at, ev.modifiers.shift),
        }
        self.selecting = ev.click_count <= 1;
        self.changed(cx);
    }

    fn mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        if ev.pressed_button != Some(MouseButton::Left) {
            self.selecting = false;
            return;
        }
        if let Some(at) = self.index_at(ev.position.x) {
            self.edit.set_caret(at, true);
            self.changed(cx);
        }
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    /// The window bounds of byte range `r` in the last frame's text.
    fn bounds_of(&self, r: Range<usize>) -> Option<Bounds<Pixels>> {
        let (line, bounds) = self.last.as_ref()?;
        let x = |i| bounds.left() + line.x_for_index(i) - self.scroll_x;
        Some(Bounds::from_corners(
            point(x(r.start), bounds.top()),
            point(x(r.end), bounds.bottom()),
        ))
    }
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let focused = self.focus.is_focused(window);
        div()
            .key_context(TEXT_FIELD_CONTEXT)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
            .cursor(CursorStyle::IBeam)
            .flex()
            .flex_row()
            .items_center()
            .h(px(chrome::CONTROL_HEIGHT))
            .px(px(space::SM))
            .rounded(px(radius::MD))
            .border_1()
            .border_color(if focused { theme.accent } else { theme.border })
            // The focus ring: AppKit's accent halo around the field.
            .when(focused, |d| {
                d.shadow(vec![BoxShadow {
                    color: theme.accent.opacity(0.35),
                    offset: point(px(0.), px(0.)),
                    blur_radius: px(0.),
                    spread_radius: px(3.),
                }])
            })
            .bg(theme.field)
            .type_style(text::BODY)
            .text_color(theme.text)
            .overflow_hidden()
            .child(TextLine { field: cx.entity() })
    }
}

/// The field's text: the caret, the selection, the IME's marked text
/// underlined, scrolled to keep the caret in view.
struct TextLine {
    field: Entity<TextField>,
}

struct Prepaint {
    line: ShapedLine,
    scroll_x: Pixels,
    caret: Option<Bounds<Pixels>>,
    selection: Option<Bounds<Pixels>>,
    focused: bool,
}

impl IntoElement for TextLine {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for TextLine {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let theme = theme(cx);
        let field = self.field.read(cx);
        let edit = &field.edit;
        let style = window.text_style();
        let (shown, color) = if edit.text().is_empty() {
            (field.placeholder.clone(), theme.tertiary)
        } else {
            (SharedString::from(edit.text().to_owned()), style.color)
        };
        let run = TextRun {
            len: shown.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = match edit.marked() {
            Some(m) if !edit.text().is_empty() => [
                TextRun {
                    len: m.start,
                    ..run.clone()
                },
                TextRun {
                    len: m.end - m.start,
                    underline: Some(UnderlineStyle {
                        color: Some(color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: shown.len() - m.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|r| r.len > 0)
            .collect(),
            _ => vec![run],
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(shown, font_size, &runs, None);
        let focused = field.focus.is_focused(window);
        let (caret_at, sel) = if edit.text().is_empty() {
            (px(0.), 0..0)
        } else {
            (line.x_for_index(edit.caret()), edit.selection())
        };
        // Keep the caret in view, with a little room after it.
        let room = px(space::XS);
        let mut scroll_x = field.scroll_x;
        if caret_at - scroll_x > bounds.size.width - room {
            scroll_x = caret_at - bounds.size.width + room;
        }
        if caret_at < scroll_x {
            scroll_x = caret_at;
        }
        let max_scroll = (line.width - bounds.size.width + room).max(px(0.));
        scroll_x = scroll_x.clamp(px(0.), max_scroll);
        let x = |at: Pixels| bounds.left() + at - scroll_x;
        let caret = (focused && field.caret_on && sel.is_empty()).then(|| {
            Bounds::new(
                point(x(caret_at), bounds.top()),
                size(px(1.5), bounds.size.height),
            )
        });
        let selection = (!sel.is_empty()).then(|| {
            Bounds::from_corners(
                point(x(line.x_for_index(sel.start)), bounds.top()),
                point(x(line.x_for_index(sel.end)), bounds.bottom()),
            )
        });
        Prepaint {
            line,
            scroll_x,
            caret,
            selection,
            focused,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        p: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let theme = theme(cx);
        let focus = self.field.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.field.clone()),
            cx,
        );
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if let Some(sel) = p.selection {
                let color = if p.focused {
                    theme.accent.opacity(0.35)
                } else {
                    theme.secondary.opacity(0.25)
                };
                window.paint_quad(fill(sel, color));
            }
            let origin = point(bounds.left() - p.scroll_x, bounds.top());
            let _ = p.line.paint(origin, window.line_height(), window, cx);
            if let Some(caret) = p.caret {
                window.paint_quad(fill(caret, theme.accent));
            }
        });
        let line = p.line.clone();
        let (scroll_x, focused) = (p.scroll_x, p.focused);
        self.field.update(cx, |f, _| {
            f.last = Some((line, bounds));
            f.scroll_x = scroll_x;
            f.drawn_focused = focused;
        });
    }
}

impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let (text, range) = self.edit.text_for_utf16(&range);
        *adjusted_range = Some(range);
        Some(text)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.edit.selection_utf16(),
            reversed: self.edit.is_reversed(),
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.edit.marked_utf16()
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.edit.unmark();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit.replace_utf16(range, text);
        self.changed(cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit
            .replace_and_mark_utf16(range, new_text, new_selected_range);
        self.changed(cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let start = self.edit.from_utf16(range_utf16.start);
        let end = self.edit.from_utf16(range_utf16.end).max(start);
        self.bounds_of(start..end).or(Some(element_bounds))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let at = self.index_at(point.x)?;
        Some(self.edit.to_utf16(at))
    }
}

#[cfg(test)]
mod tests {
    use gpui::{ClipboardItem, Entity, TestAppContext, VisualTestContext};

    use super::TextField;
    use crate::app::keymap::bind_keys;

    fn field<'a>(
        cx: &'a mut TestAppContext,
        text: &str,
    ) -> (Entity<TextField>, &'a mut VisualTestContext) {
        cx.update(bind_keys);
        let text = text.to_owned();
        let (view, cx) = cx.add_window_view(|_, cx| TextField::new(text, "placeholder", cx));
        cx.update(|window, cx| window.focus(view.read(cx).focus_handle()));
        cx.run_until_parked();
        (view, cx)
    }

    fn text(view: &Entity<TextField>, cx: &mut VisualTestContext) -> String {
        view.read_with(cx, |f, _| f.text().to_owned())
    }

    #[gpui::test]
    fn typing_moves_and_edits_at_the_caret(cx: &mut TestAppContext) {
        let (view, cx) = field(cx, "held");
        cx.simulate_keystrokes("left");
        cx.simulate_input("l");
        assert_eq!(text(&view, cx), "helld");
        cx.simulate_keystrokes("cmd-left");
        cx.simulate_input("> ");
        assert_eq!(text(&view, cx), "> helld");
        cx.simulate_keystrokes("cmd-right alt-backspace");
        assert_eq!(text(&view, cx), "> ");
        cx.simulate_keystrokes("cmd-backspace");
        assert_eq!(text(&view, cx), "");
    }

    #[gpui::test]
    fn select_all_cut_paste_and_undo(cx: &mut TestAppContext) {
        let (view, cx) = field(cx, "~/src");
        cx.simulate_keystrokes("cmd-a cmd-x");
        assert_eq!(text(&view, cx), "");
        let clip = cx.read_from_clipboard().and_then(|c| c.text());
        assert_eq!(clip.as_deref(), Some("~/src"));
        cx.write_to_clipboard(ClipboardItem::new_string("/tmp\nsecond".into()));
        cx.simulate_keystrokes("cmd-v");
        assert_eq!(text(&view, cx), "/tmp", "one line only");
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&view, cx), "");
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&view, cx), "~/src");
        cx.simulate_keystrokes("cmd-shift-z");
        assert_eq!(text(&view, cx), "");
    }

    #[gpui::test]
    fn shift_selection_is_replaced_by_typing(cx: &mut TestAppContext) {
        let (view, cx) = field(cx, "claude --resume");
        cx.simulate_keystrokes("shift-alt-left");
        cx.simulate_input("x");
        assert_eq!(text(&view, cx), "claude --x");
        // ⌥← skips the dashes to the word before, like AppKit.
        cx.simulate_keystrokes("shift-alt-left");
        cx.simulate_input("y");
        assert_eq!(text(&view, cx), "claude --y");
        cx.simulate_keystrokes("ctrl-a ctrl-k");
        assert_eq!(text(&view, cx), "");
    }

    #[gpui::test]
    fn enter_and_escape_reach_the_sheet(cx: &mut TestAppContext) {
        let (view, cx) = field(cx, "abc");
        // Not handled by the field: they propagate (the sheet acts on them).
        cx.simulate_keystrokes("enter escape");
        assert_eq!(text(&view, cx), "abc");
    }
}
