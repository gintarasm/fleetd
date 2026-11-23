//! A one-line text field that edits like an `NSTextField`: a caret that
//! blinks, mouse and Shift selection (double click: a word, triple: all),
//! ⌥ word and ⌘ line moves, ⌘A, ⌘C/⌘X/⌘V, ⌘Z/⇧⌘Z, the Emacs keys
//! Cocoa supports (⌃A ⌃E ⌃B ⌃F ⌃D ⌃H ⌃K), and IME composition. Editing
//! is [`TextEdit`]; this draws it and turns events into its commands.
//!
//! [`TextField::multiline`] makes a text box instead (the Send sheet's
//! message, ADR 0083): Return types a line break, the text wraps at the
//! field's width, ↑/↓ move by row, ⌘←/⌘→ to the line's ends, and the
//! box scrolls to keep the caret in view.

use std::ops::Range;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::{
    App, Bounds, BoxShadow, ClipboardItem, ContentMask, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, GlobalElementId,
    InspectorElementId, InteractiveElement, IntoElement, KeyDownEvent, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, ShapedLine,
    SharedString, Style, Styled, Task, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window,
    WrappedLine, div, fill, point, px, relative, size,
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
    /// The blink timer, while the field is focused.
    blink: Option<Task<()>>,
    /// A text box: how many rows it shows ([`TextField::multiline`]).
    rows: Option<usize>,
    /// A text box's last frame: its wrapped text and where it was drawn.
    last_block: Option<(Block, Bounds<Pixels>)>,
    /// How far a text box is scrolled up to keep the caret in view.
    scroll_y: Pixels,
}

impl TextField {
    pub fn new(text: String, placeholder: &str, cx: &mut Context<Self>) -> Self {
        TextField {
            edit: TextEdit::new(text),
            focus: cx.focus_handle(),
            placeholder: SharedString::from(placeholder.to_owned()),
            last: None,
            scroll_x: px(0.),
            selecting: false,
            caret_on: true,
            drawn_focused: false,
            blink: None,
            rows: None,
            last_block: None,
            scroll_y: px(0.),
        }
    }

    /// The field is a stop in its sheet's or Settings' Tab order (ADR 0101).
    pub fn tab_stop(mut self) -> Self {
        self.focus = self.focus.clone().tab_stop(true);
        self
    }

    /// A text box `rows` rows tall: line breaks are kept, Return types
    /// one (⌘Return still reaches the sheet).
    pub fn multiline(text: String, placeholder: &str, rows: usize, cx: &mut Context<Self>) -> Self {
        TextField {
            edit: TextEdit::multiline(text),
            rows: Some(rows.max(1)),
            ..TextField::new(String::new(), placeholder, cx)
        }
    }

    /// Starts the caret blinking from its shown half (again, after an
    /// edit, so it stays solid while typing). The task ends once the
    /// field is drawn unfocused; the next focused frame starts it again.
    fn start_blink(&mut self, cx: &mut Context<Self>) {
        self.caret_on = true;
        self.blink = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK).await;
                let go_on = this.update(cx, |f, cx| {
                    if !f.drawn_focused {
                        f.caret_on = true;
                        f.blink = None;
                        return false;
                    }
                    f.caret_on = !f.caret_on;
                    cx.notify();
                    true
                });
                if !matches!(go_on, Ok(true)) {
                    break;
                }
            }
        }));
    }

    /// The caret is blinking (the field was drawn focused).
    #[cfg(test)]
    fn is_blinking(&self) -> bool {
        self.blink.is_some()
    }

    pub fn text(&self) -> &str {
        self.edit.text()
    }

    /// Replaces the text (a pick from a list); ⌘Z brings the old back.
    pub fn set_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.edit.set_text(&text);
        self.changed(cx);
    }

    /// Selects all the text (a find field reopened: typing replaces it).
    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        self.edit.select_all();
        self.changed(cx);
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Types `text` at the caret (a key typed elsewhere, handed over).
    pub fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        self.edit.unmark();
        self.edit.insert(text);
        self.changed(cx);
    }

    /// After any edit or caret move: the caret shows at once.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.caret_on = true;
        if self.blink.is_some() {
            self.start_blink(cx);
        }
        cx.notify();
    }

    fn key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // While composing, the IME owns the keys.
        if self.edit.marked().is_some() {
            return;
        }
        if self.rows.is_some() && self.box_key_down(ev) {
            self.changed(cx);
            cx.stop_propagation();
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

    /// A text box's own keys: Return, ↑/↓ by row, ⌘ and ⌃ moves to the
    /// line's ends. True if handled.
    fn box_key_down(&mut self, ev: &KeyDownEvent) -> bool {
        let m = &ev.keystroke.modifiers;
        let e = &mut self.edit;
        match ev.keystroke.key.as_str() {
            "enter" if !m.platform && !m.control && !m.alt => {
                e.unmark();
                e.insert("\n");
            }
            "left" if m.platform && !m.control => e.move_caret(Movement::LineStart, m.shift),
            "right" if m.platform && !m.control => e.move_caret(Movement::LineEnd, m.shift),
            "a" if m.control => e.move_caret(Movement::LineStart, m.shift),
            "e" if m.control => e.move_caret(Movement::LineEnd, m.shift),
            "up" if m.platform => e.move_caret(Movement::Home, m.shift),
            "down" if m.platform => e.move_caret(Movement::End, m.shift),
            "up" | "down" if !m.control && !m.alt => {
                let to = self.row_move(ev.keystroke.key == "down");
                self.edit.set_caret(to, m.shift);
            }
            "p" | "n" if m.control => {
                let to = self.row_move(ev.keystroke.key == "n");
                self.edit.set_caret(to, m.shift);
            }
            _ => return false,
        }
        true
    }

    /// Where ↑ (or ↓, `down`) takes the caret in the last frame's text
    /// box: the row above (below) at the same x, else the text's start
    /// (end).
    fn row_move(&self, down: bool) -> usize {
        let end = if down { self.edit.text().len() } else { 0 };
        let Some((block, _)) = &self.last_block else {
            return end;
        };
        let caret = self.edit.caret();
        let row = block.row_of(caret);
        let to = if down {
            Some(row + 1)
        } else {
            row.checked_sub(1)
        };
        match to.filter(|&r| r < block.rows.len()) {
            Some(to) => block.index_in_row(to, block.x_in_row(row, caret)),
            None => end,
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

    /// Pastes the clipboard's first line (a one-line field), or all of
    /// it (a text box).
    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
            let pasted = if self.rows.is_some() {
                text.as_str()
            } else {
                text.lines().next().unwrap_or("")
            };
            self.edit.unmark();
            self.edit.insert(pasted);
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

    /// The byte offset under window point `p` in the last frame's text.
    fn index_at(&self, p: Point<Pixels>) -> Option<usize> {
        if self.rows.is_some() {
            let (block, bounds) = self.last_block.as_ref()?;
            if self.edit.text().is_empty() {
                return Some(0);
            }
            let at = point(p.x - bounds.left(), p.y - bounds.top() + self.scroll_y);
            return Some(block.index_at(at));
        }
        let (line, bounds) = self.last.as_ref()?;
        if self.edit.text().is_empty() {
            return Some(0);
        }
        Some(line.closest_index_for_x(p.x - bounds.left() + self.scroll_x))
    }

    fn mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let Some(at) = self.index_at(ev.position) else {
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
        if let Some(at) = self.index_at(ev.position) {
            self.edit.set_caret(at, true);
            self.changed(cx);
        }
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    /// The window bounds of byte range `r` in the last frame's text (a
    /// text box: from its start, on the start's row).
    fn bounds_of(&self, r: Range<usize>) -> Option<Bounds<Pixels>> {
        if self.rows.is_some() {
            let (block, bounds) = self.last_block.as_ref()?;
            let start = block.position(r.start);
            let end = block.position(r.end);
            let end_x = if end.y == start.y { end.x } else { start.x };
            let top = bounds.top() + start.y - self.scroll_y;
            return Some(Bounds::from_corners(
                point(bounds.left() + start.x, top),
                point(bounds.left() + end_x, top + block.line_height),
            ));
        }
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
        let text_box = self.rows.is_some();
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
            .when(!text_box, |d| {
                d.items_center().h(px(chrome::CONTROL_HEIGHT))
            })
            .when(text_box, |d| {
                d.w_full().items_start().py(px(space::XS + space::XXS))
            })
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
            .when(!text_box, |d| d.child(TextLine { field: cx.entity() }))
            .when(text_box, |d| d.child(TextBlock { field: cx.entity() }))
    }
}

/// What a field draws (its text, or the placeholder dimmed), styled in
/// runs: the IME's marked text underlined.
fn shown_text(field: &TextField, window: &Window, cx: &App) -> (SharedString, Vec<TextRun>) {
    let theme = theme(cx);
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
    // Clamped: a stale range must never cut the text off a char.
    let marked = edit.marked().filter(|m| {
        m.start <= m.end
            && m.end <= shown.len()
            && shown.is_char_boundary(m.start)
            && shown.is_char_boundary(m.end)
    });
    let runs = match marked {
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
    (shown, runs)
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
        let field = self.field.read(cx);
        let edit = &field.edit;
        let style = window.text_style();
        let (shown, runs) = shown_text(field, window, cx);
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
        self.field.update(cx, |f, cx| {
            f.last = Some((line, bounds));
            f.scroll_x = scroll_x;
            f.drawn_focused = focused;
            if focused && f.blink.is_none() {
                f.start_blink(cx);
            }
        });
    }
}

/// A text box's text wrapped at its width: the logical lines (split at
/// `\n`) and the rows they wrap into.
#[derive(Clone)]
struct Block {
    lines: Vec<WrappedLine>,
    /// Each line's byte offset in the text.
    starts: Vec<usize>,
    rows: Vec<Row>,
    line_height: Pixels,
}

/// A row of a [`Block`]: part of line `line`, bytes `start..end` of the
/// text, `x0` into the line's unwrapped layout.
#[derive(Clone, Copy)]
struct Row {
    line: usize,
    start: usize,
    end: usize,
    x0: Pixels,
}

impl Block {
    fn new(lines: Vec<WrappedLine>, line_height: Pixels) -> Self {
        let mut starts = Vec::with_capacity(lines.len());
        let mut rows = Vec::new();
        let mut at = 0;
        for (li, line) in lines.iter().enumerate() {
            starts.push(at);
            let layout = &line.unwrapped_layout;
            let mut row_start = 0;
            let breaks = line.wrap_boundaries.iter().map(|b| {
                layout
                    .runs
                    .get(b.run_ix)
                    .and_then(|r| r.glyphs.get(b.glyph_ix))
                    .map_or(line.len(), |g| g.index)
            });
            for end in breaks.chain([line.len()]) {
                rows.push(Row {
                    line: li,
                    start: at + row_start,
                    end: at + end,
                    x0: layout.x_for_index(row_start),
                });
                row_start = end;
            }
            at += line.len() + 1;
        }
        Block {
            lines,
            starts,
            rows,
            line_height,
        }
    }

    fn height(&self) -> Pixels {
        self.line_height * self.rows.len().max(1) as f32
    }

    /// The row byte `at` is drawn on (at a wrap: the later row).
    fn row_of(&self, at: usize) -> usize {
        self.rows.iter().rposition(|r| r.start <= at).unwrap_or(0)
    }

    /// The x of byte `at` on row `row`.
    fn x_in_row(&self, row: usize, at: usize) -> Pixels {
        let r = self.rows[row];
        let layout = &self.lines[r.line].unwrapped_layout;
        layout.x_for_index(at.saturating_sub(self.starts[r.line])) - r.x0
    }

    /// Where byte `at` is drawn, from the block's top left.
    fn position(&self, at: usize) -> Point<Pixels> {
        if self.rows.is_empty() {
            return point(px(0.), px(0.));
        }
        let row = self.row_of(at);
        point(self.x_in_row(row, at), self.line_height * row as f32)
    }

    /// The byte closest to x `x` on row `row`.
    fn index_in_row(&self, row: usize, x: Pixels) -> usize {
        let r = self.rows[row];
        let layout = &self.lines[r.line].unwrapped_layout;
        let at = self.starts[r.line] + layout.closest_index_for_x(x + r.x0);
        at.clamp(r.start, r.end)
    }

    /// The byte closest to `p`, from the block's top left.
    fn index_at(&self, p: Point<Pixels>) -> usize {
        if self.rows.is_empty() {
            return 0;
        }
        let row = (p.y / self.line_height).floor().max(0.) as usize;
        self.index_in_row(row.min(self.rows.len() - 1), p.x)
    }

    /// The rectangles of selection `sel`, from the block's top left (a
    /// row's line break shows as a little width).
    fn selection_rects(&self, sel: &Range<usize>) -> Vec<Bounds<Pixels>> {
        let lh = self.line_height;
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, r)| sel.start <= r.end && sel.end >= r.start && !sel.is_empty())
            .filter_map(|(i, r)| {
                let from = sel.start.max(r.start);
                let to = sel.end.min(r.end);
                let x0 = self.x_in_row(i, from);
                let mut x1 = self.x_in_row(i, to);
                // The selection goes on past the row's line break.
                if sel.end > r.end && self.rows.get(i + 1).is_some_and(|n| n.line != r.line) {
                    x1 += px(4.);
                }
                (x1 > x0).then(|| {
                    Bounds::from_corners(point(x0, lh * i as f32), point(x1, lh * (i + 1) as f32))
                })
            })
            .collect()
    }
}

/// A text box's text ([`TextField::multiline`]): wrapped at its width,
/// `rows` rows tall, scrolled to keep the caret's row in view.
struct TextBlock {
    field: Entity<TextField>,
}

struct BlockPrepaint {
    block: Block,
    scroll_y: Pixels,
    caret: Option<Bounds<Pixels>>,
    selection: Vec<Bounds<Pixels>>,
    focused: bool,
}

impl IntoElement for TextBlock {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for TextBlock {
    type RequestLayoutState = ();
    type PrepaintState = BlockPrepaint;

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
        let rows = self.field.read(cx).rows.unwrap_or(1);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (window.line_height() * rows as f32).into();
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
    ) -> BlockPrepaint {
        let field = self.field.read(cx);
        let edit = &field.edit;
        let (shown, runs) = shown_text(field, window, cx);
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let lines = window
            .text_system()
            .shape_text(shown, font_size, &runs, Some(bounds.size.width), None)
            .map(|l| l.into_vec())
            .unwrap_or_default();
        let block = Block::new(lines, line_height);
        let focused = field.focus.is_focused(window);
        let (caret_at, sel) = if edit.text().is_empty() {
            (point(px(0.), px(0.)), 0..0)
        } else {
            (block.position(edit.caret()), edit.selection())
        };
        // Keep the caret's row in view.
        let mut scroll_y = field.scroll_y;
        if caret_at.y + line_height - scroll_y > bounds.size.height {
            scroll_y = caret_at.y + line_height - bounds.size.height;
        }
        if caret_at.y < scroll_y {
            scroll_y = caret_at.y;
        }
        let max_scroll = (block.height() - bounds.size.height).max(px(0.));
        scroll_y = scroll_y.clamp(px(0.), max_scroll);
        let origin = point(bounds.left(), bounds.top() - scroll_y);
        let caret = (focused && field.caret_on && sel.is_empty()).then(|| {
            Bounds::new(
                point(origin.x + caret_at.x, origin.y + caret_at.y),
                size(px(1.5), line_height),
            )
        });
        let selection = block
            .selection_rects(&sel)
            .into_iter()
            .map(|b| Bounds::new(point(origin.x + b.left(), origin.y + b.top()), b.size))
            .collect();
        BlockPrepaint {
            block,
            scroll_y,
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
        p: &mut BlockPrepaint,
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
        let lh = p.block.line_height;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            let color = if p.focused {
                theme.accent.opacity(0.35)
            } else {
                theme.secondary.opacity(0.25)
            };
            for sel in &p.selection {
                window.paint_quad(fill(*sel, color));
            }
            for (li, line) in p.block.lines.iter().enumerate() {
                let first_row = p.block.rows.iter().position(|r| r.line == li).unwrap_or(0);
                let origin = point(
                    bounds.left(),
                    bounds.top() - p.scroll_y + lh * first_row as f32,
                );
                let _ = line.paint(origin, lh, TextAlign::Left, None, window, cx);
            }
            if let Some(caret) = p.caret {
                window.paint_quad(fill(caret, theme.accent));
            }
        });
        let block = p.block.clone();
        let (scroll_y, focused) = (p.scroll_y, p.focused);
        self.field.update(cx, |f, cx| {
            f.last_block = Some((block, bounds));
            f.scroll_y = scroll_y;
            f.drawn_focused = focused;
            if focused && f.blink.is_none() {
                f.start_blink(cx);
            }
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
        let at = self.index_at(point)?;
        Some(self.edit.to_utf16(at))
    }
}

#[cfg(test)]
mod tests {
    use gpui::{ClipboardItem, Entity, EntityInputHandler, TestAppContext, VisualTestContext};

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
    fn select_all_and_cut_while_composing_then_composing_again(cx: &mut TestAppContext) {
        let (view, cx) = field(cx, "abc");
        view.update_in(cx, |f, window, cx| {
            f.replace_and_mark_text_in_range(None, "にほ", None, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-a cmd-x");
        view.update_in(cx, |f, window, cx| {
            f.replace_and_mark_text_in_range(None, "に", None, window, cx);
            f.replace_text_in_range(None, "日", window, cx);
        });
        cx.run_until_parked();
        assert_eq!(text(&view, cx), "日");
    }

    #[gpui::test]
    fn blinks_only_while_focused(cx: &mut TestAppContext) {
        let (view, cx) = field(cx, "abc");
        assert!(view.read_with(cx, |f, _| f.is_blinking()));
        cx.update(|window, _| window.blur());
        cx.run_until_parked();
        cx.executor().advance_clock(super::BLINK * 2);
        cx.run_until_parked();
        assert!(!view.read_with(cx, |f, _| f.is_blinking()));
        // Typing keeps the caret solid: an edit restarts the shown half.
        cx.update(|window, cx| window.focus(view.read(cx).focus_handle()));
        cx.run_until_parked();
        cx.executor().advance_clock(super::BLINK);
        cx.run_until_parked();
        assert!(!view.read_with(cx, |f, _| f.caret_on));
        cx.simulate_input("d");
        assert!(view.read_with(cx, |f, _| f.caret_on));
    }

    /// A text box: Return types a line break (⌘Return reaches the sheet),
    /// a paste keeps its lines, ⌘← and ⌃E go to the line's ends, ↑ row
    /// by row to the start, ⌘↓ to the end.
    #[gpui::test]
    fn a_text_box_keeps_lines(cx: &mut TestAppContext) {
        cx.update(bind_keys);
        let (view, cx) =
            cx.add_window_view(|_, cx| TextField::multiline(String::new(), "Message", 4, cx));
        cx.update(|window, cx| window.focus(view.read(cx).focus_handle()));
        cx.run_until_parked();
        cx.simulate_input("one");
        cx.simulate_keystrokes("enter cmd-enter");
        cx.simulate_input("two");
        assert_eq!(text(&view, cx), "one\ntwo");
        cx.write_to_clipboard(ClipboardItem::new_string("\nthree\r\nfour".into()));
        cx.simulate_keystrokes("cmd-v");
        assert_eq!(text(&view, cx), "one\ntwo\nthree\nfour");
        cx.simulate_keystrokes("cmd-left");
        cx.simulate_input("> ");
        assert_eq!(text(&view, cx), "one\ntwo\nthree\n> four");
        cx.simulate_keystrokes("cmd-left ctrl-e");
        cx.simulate_input("!");
        assert_eq!(text(&view, cx), "one\ntwo\nthree\n> four!");
        // ↑ row by row to the start (here a row per character: the test
        // window's field has no width to wrap at).
        for _ in 0..30 {
            cx.simulate_keystrokes("up");
        }
        cx.simulate_input("# ");
        cx.simulate_keystrokes("cmd-down");
        cx.simulate_input("?");
        assert_eq!(text(&view, cx), "# one\ntwo\nthree\n> four!?");
    }

    #[gpui::test]
    fn enter_and_escape_reach_the_sheet(cx: &mut TestAppContext) {
        let (view, cx) = field(cx, "abc");
        // Not handled by the field: they propagate (the sheet acts on them).
        cx.simulate_keystrokes("enter escape");
        assert_eq!(text(&view, cx), "abc");
    }
}
