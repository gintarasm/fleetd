//! A terminal pane view: keyboard, IME, paste and mouse input for one
//! pane, drawn by [`TerminalElement`].

use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use fleet_term::CursorShape;
use gpui::{
    AnyWindowHandle, Bounds, ClipboardItem, Context, EventEmitter, FocusHandle, Font, FontWeight,
    InteractiveElement, IntoElement, KeyDownEvent, Modifiers, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent,
    SharedString, StatefulInteractiveElement, Styled, Subscription, UTF16Selection, Window, div,
    font, point, px, size,
};

use super::keymap::{Copy, Paste, TERMINAL_CONTEXT, to_key};
use super::tail_text::tail_text;
use super::terminal_element::TerminalElement;
use super::theme::current as theme;
use crate::core::config::GuiConfig;
use crate::core::keys::{KeyMods, encode_key, encode_paste};
use crate::core::mouse::{MouseButton, MouseKind, encode_mouse, wheel_as_arrows};
use crate::core::pane::{PaneModel, PaneState, ViewRow};
use crate::core::selection::{LineRef, SelPoint};
use crate::core::session::{PaneId, Session};

/// At most this many lines are scrolled (or reported) per wheel event.
const MAX_WHEEL_LINES: f32 = 100.0;

/// Where the pane's input goes: the [`Session`], or a recording fake in
/// tests.
pub trait InputSink: Send + Sync {
    fn input(&self, pane: PaneId, bytes: Vec<u8>);
    fn resize(&self, pane: PaneId, cols: u16, rows: u16);
    /// The view scrolled: fetch older history if it now needs it.
    fn check_paging(&self, _pane: PaneId) {}
}

impl InputSink for Session {
    fn input(&self, pane: PaneId, bytes: Vec<u8>) {
        Session::input(self, pane, bytes);
    }

    fn resize(&self, pane: PaneId, cols: u16, rows: u16) {
        Session::resize(self, pane, cols, rows);
    }

    fn check_paging(&self, pane: PaneId) {
        Session::check_paging(self, pane);
    }
}

/// The `(col, row)` of the cell under `pos` in a grid of `cols`×`rows`
/// cells at `bounds.origin`, clamped inside the grid.
pub fn point_to_cell(
    pos: Point<Pixels>,
    bounds: Bounds<Pixels>,
    cell_w: Pixels,
    line_h: Pixels,
    cols: u16,
    rows: u16,
) -> (u16, u16) {
    let fit = |offset: Pixels, cell: Pixels, n: u16| {
        let at = f32::from(offset) / f32::from(cell);
        if at.is_finite() && at > 0.0 {
            // `as` saturates; the `min` keeps it on the grid.
            (at.floor() as u16).min(n.saturating_sub(1))
        } else {
            0
        }
    };
    (
        fit(pos.x - bounds.origin.x, cell_w, cols),
        fit(pos.y - bounds.origin.y, line_h, rows),
    )
}

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

/// What a held mouse button is doing.
#[derive(Clone, Copy, Debug)]
enum Drag {
    /// Extending the selection.
    Select,
    /// Reported to the agent; `at` is the cell last reported.
    Report { button: MouseButton, at: (u16, u16) },
}

/// What the workspace tells a pane about itself, for its header and
/// banners.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaneChrome {
    /// 1-based, in reading order.
    pub number: usize,
    /// The layout puts an agent here.
    pub has_agent: bool,
    /// The agent list says that agent exited or is lost.
    pub agent_ended: bool,
    pub program: Option<String>,
    /// The agent's directory, with `~`.
    pub cwd: Option<String>,
    /// A spawn into this pane is on its way.
    pub starting: bool,
    /// When the control connection retries, while it is down.
    pub reconnect_in: Option<Duration>,
    /// The pane has keyboard focus (drawn in the header).
    pub focused: bool,
}

/// The line drawn over the grid, if any.
pub fn banner(chrome: &PaneChrome, state: PaneState) -> Option<String> {
    let disconnected = || match chrome.reconnect_in {
        Some(d) => format!(
            "disconnected — reconnecting in {}s",
            d.as_secs_f32().ceil().max(1.0) as u64
        ),
        None => "disconnected — reconnecting".to_owned(),
    };
    match state {
        PaneState::Disconnected => Some(disconnected()),
        _ if !chrome.has_agent => None,
        PaneState::Exited(code) => Some(format!("exited ({code})")),
        PaneState::Attaching => Some("attaching…".to_owned()),
        PaneState::Lost => Some("the agent is gone".to_owned()),
        PaneState::Live => None,
    }
}

/// The exit banner offers Restart (as ⌘R does): the pane's agent exited
/// or is gone.
pub fn restartable(chrome: &PaneChrome, state: PaneState) -> bool {
    chrome.has_agent && matches!(state, PaneState::Exited(_) | PaneState::Lost)
}

/// The text centred in a pane with no agent.
pub fn empty_message(chrome: &PaneChrome) -> Option<&'static str> {
    match (chrome.has_agent, chrome.starting) {
        (true, _) => None,
        (false, true) => Some("starting…"),
        (false, false) => Some("Select an agent or press ⌘N"),
    }
}

/// Whether the grid draws a cursor: only for an agent that may still
/// take input (none in an empty pane, or once the agent exited or is gone).
pub fn draws_cursor(chrome: &PaneChrome, state: PaneState) -> bool {
    chrome.has_agent
        && !chrome.agent_ended
        && !matches!(state, PaneState::Exited(_) | PaneState::Lost)
}

/// The header's title and path: the OSC title if the agent set one, else
/// the program; and the directory with `~`.
pub fn header_parts(chrome: &PaneChrome, osc_title: Option<&str>) -> (String, String) {
    let title = osc_title
        .filter(|t| !t.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| chrome.program.clone())
        .unwrap_or_default();
    (title, chrome.cwd.clone().unwrap_or_default())
}

/// A header button, asking the workspace to act on this pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneEvent {
    SplitRight,
    SplitDown,
    ToggleMaximize,
    Close,
    /// The exit banner's Restart: the ended agent again, in this pane.
    Restart,
}

impl EventEmitter<PaneEvent> for PaneView {}

pub struct PaneView {
    pane: PaneId,
    pub(crate) chrome: PaneChrome,
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
    drag: Option<Drag>,
    /// Wheel movement not yet scrolled, in lines (positive up).
    wheel_lines: f32,
    /// Where the "new lines" pill was last drawn, if it was.
    pub(crate) pill: Option<Bounds<Pixels>>,
    /// The window whose activation the cursor follows, and that
    /// subscription (renewed when the view moves to a new window).
    activation: Option<(AnyWindowHandle, Subscription)>,
}

impl PaneView {
    pub fn new(
        pane: PaneId,
        model: Arc<Mutex<PaneModel>>,
        sink: Arc<dyn InputSink>,
        config: &GuiConfig,
        cx: &mut Context<Self>,
    ) -> Self {
        PaneView {
            pane,
            chrome: PaneChrome::default(),
            model,
            sink,
            focus: cx.focus_handle(),
            option_as_meta: config.option_as_meta,
            font: font(config.font_family.clone()),
            font_size: px(config.font_size),
            marked: None,
            marked_selection: None,
            layout: None,
            drag: None,
            wheel_lines: 0.0,
            pill: None,
            activation: None,
        }
    }

    pub fn pane(&self) -> PaneId {
        self.pane
    }

    pub fn model_handle(&self) -> &Arc<Mutex<PaneModel>> {
        &self.model
    }

    /// The grid size last laid out, if drawn yet.
    pub fn grid(&self) -> Option<(u16, u16)> {
        self.layout.map(|l| (l.cols, l.rows))
    }

    /// Replaces the header and banner facts; redraws if they changed.
    pub fn set_chrome(&mut self, chrome: PaneChrome, cx: &mut Context<Self>) {
        if self.chrome != chrome {
            self.chrome = chrome;
            cx.notify();
        }
    }

    /// The cursor turns hollow when the window loses activation. The view
    /// outlives its window (the app is resident), so this follows whichever
    /// window draws it.
    fn follow_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = window.window_handle();
        if self.activation.as_ref().is_some_and(|(h, _)| *h == handle) {
            return;
        }
        let sub = cx.observe_window_activation(window, |_, _, cx| cx.notify());
        self.activation = Some((handle, sub));
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn model(&self) -> MutexGuard<'_, PaneModel> {
        lock(&self.model)
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

    pub(crate) fn mouse_down(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus);
        if self.pill.is_some_and(|p| p.contains(&ev.position)) {
            self.drag = None;
            self.model().scroll_to_bottom();
            cx.notify();
            return;
        }
        let Some(button) = to_button(ev.button) else {
            return;
        };
        let mut model = lock(&self.model);
        let Some(at) = cell_at(self.layout, &model, ev.position) else {
            return;
        };
        if reports(&model, ev.modifiers) {
            let modes = model.terminal().modes();
            drop(model);
            self.drag = Some(Drag::Report { button, at });
            let bytes = encode_mouse(
                button,
                MouseKind::Press,
                at.0,
                at.1,
                mods(ev.modifiers),
                &modes,
            );
            if let Some(bytes) = bytes {
                self.send(bytes, cx);
            }
            return;
        }
        if button != MouseButton::Left {
            return;
        }
        let p = sel_point(&model, at);
        match ev.click_count {
            0 | 1 => model.select_start(p),
            2 => model.select_word(p),
            _ => model.select_line(p),
        }
        drop(model);
        self.drag = Some(Drag::Select);
        cx.notify();
    }

    pub(crate) fn mouse_move(
        &mut self,
        ev: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag else {
            return;
        };
        let pressed = ev.pressed_button.and_then(to_button);
        let mut model = lock(&self.model);
        let Some(at) = cell_at(self.layout, &model, ev.position) else {
            return;
        };
        match drag {
            // The release happened where it was not seen.
            Drag::Select if pressed != Some(MouseButton::Left) => self.drag = None,
            Drag::Select => {
                let p = sel_point(&model, at);
                model.select_extend(p);
                cx.notify();
            }
            Drag::Report { button, .. } if pressed != Some(button) => self.drag = None,
            Drag::Report { at: last, .. } if last == at => {}
            Drag::Report { button, .. } => {
                let modes = model.terminal().modes();
                drop(model);
                self.drag = Some(Drag::Report { button, at });
                let kind = MouseKind::Drag;
                if let Some(bytes) =
                    encode_mouse(button, kind, at.0, at.1, mods(ev.modifiers), &modes)
                {
                    self.send(bytes, cx);
                }
            }
        }
    }

    pub(crate) fn mouse_up(
        &mut self,
        ev: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag else {
            return;
        };
        let Drag::Report { button, at: last } = drag else {
            self.drag = None;
            return;
        };
        if to_button(ev.button) != Some(button) {
            return;
        }
        self.drag = None;
        let model = lock(&self.model);
        let at = cell_at(self.layout, &model, ev.position).unwrap_or(last);
        let modes = model.terminal().modes();
        drop(model);
        let kind = MouseKind::Release;
        if let Some(bytes) = encode_mouse(button, kind, at.0, at.1, mods(ev.modifiers), &modes) {
            self.send(bytes, cx);
        }
    }

    pub(crate) fn scroll_wheel(
        &mut self,
        ev: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(layout) = self.layout else {
            return;
        };
        let lines = match ev.delta {
            ScrollDelta::Lines(d) => d.y,
            ScrollDelta::Pixels(d) => f32::from(d.y) / f32::from(layout.line_h),
        };
        if !lines.is_finite() || lines == 0.0 {
            return;
        }
        // A change of direction drops what the other way left over.
        if self.wheel_lines != 0.0 && self.wheel_lines.signum() != lines.signum() {
            self.wheel_lines = 0.0;
        }
        self.wheel_lines += lines;
        let whole = self.wheel_lines.trunc();
        self.wheel_lines -= whole;
        let lines = whole.clamp(-MAX_WHEEL_LINES, MAX_WHEEL_LINES) as i32;
        if lines == 0 {
            return;
        }

        let mut model = lock(&self.model);
        let modes = model.terminal().modes();
        let bytes = if reports(&model, ev.modifiers) {
            let (col, row) = cell_at(self.layout, &model, ev.position).unwrap_or((0, 0));
            let button = if lines > 0 {
                MouseButton::WheelUp
            } else {
                MouseButton::WheelDown
            };
            encode_mouse(
                button,
                MouseKind::Press,
                col,
                row,
                mods(ev.modifiers),
                &modes,
            )
            .map(|one| one.repeat(lines.unsigned_abs() as usize))
        } else {
            wheel_as_arrows(lines, &modes)
        };
        if let Some(bytes) = bytes {
            drop(model);
            self.send(bytes, cx);
            return;
        }
        model.scroll_lines(lines);
        drop(model);
        self.sink.check_paging(self.pane);
        cx.notify();
    }
}

fn lock(model: &Mutex<PaneModel>) -> MutexGuard<'_, PaneModel> {
    model.lock().unwrap_or_else(|e| e.into_inner())
}

fn to_button(button: gpui::MouseButton) -> Option<MouseButton> {
    match button {
        gpui::MouseButton::Left => Some(MouseButton::Left),
        gpui::MouseButton::Middle => Some(MouseButton::Middle),
        gpui::MouseButton::Right => Some(MouseButton::Right),
        gpui::MouseButton::Navigate(_) => None,
    }
}

fn mods(m: Modifiers) -> KeyMods {
    KeyMods {
        shift: m.shift,
        alt: m.alt,
        ctrl: m.control,
    }
}

/// Whether mouse events go to the agent rather than to the selection: a
/// mouse mode is on, the view follows the grid, and Shift is not held.
fn reports(model: &PaneModel, m: Modifiers) -> bool {
    let modes = model.terminal().modes();
    let mouse = modes.mouse_click || modes.mouse_drag || modes.mouse_motion;
    mouse && !m.shift && model.scroll_offset() == 0
}

/// The drawn grid's cell under `pos`, clamped onto it.
fn cell_at(
    layout: Option<GridLayout>,
    model: &PaneModel,
    pos: Point<Pixels>,
) -> Option<(u16, u16)> {
    let l = layout?;
    let (cols, rows) = model.terminal().size();
    let grid = Bounds::new(
        l.origin,
        size(l.cell_w * f32::from(cols), l.line_h * f32::from(rows)),
    );
    Some(point_to_cell(pos, grid, l.cell_w, l.line_h, cols, rows))
}

/// The line and column visible cell `(col, row)` shows.
fn sel_point(model: &PaneModel, (col, row): (u16, u16)) -> SelPoint {
    let line = match model.view_row(row) {
        ViewRow::History(abs) => LineRef::History(abs),
        ViewRow::Grid(r) => LineRef::Grid(r),
    };
    SelPoint { line, col }
}

impl Render for PaneView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.follow_activation(window, cx);
        let theme = theme(cx);
        let focused = window.is_window_active() && self.focus.is_focused(window);
        let (state, title, observing) = {
            let model = self.model();
            let state = model.state();
            let observing = state == PaneState::Live && !model.is_primary();
            (state, model.title(), observing)
        };
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
            show_cursor: draws_cursor(&self.chrome, state),
            marked: self.marked.clone(),
        };
        let chrome = &self.chrome;
        let (title_text, path) = header_parts(chrome, title.as_deref());
        let icon_button = |id: &'static str, glyph: &'static str, event: PaneEvent| {
            div()
                .id(id)
                .debug_selector(move || format!("pane-{}-{id}", chrome.number))
                .flex_none()
                .w(px(20.))
                .h(px(18.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .text_color(theme.dim_text)
                .cursor_pointer()
                .hover(|s| s.bg(theme.header_bg).text_color(theme.fg()))
                .child(glyph)
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(event)))
        };
        let header = div()
            .flex()
            .flex_row()
            .flex_none()
            .h(px(28.))
            .px(px(10.))
            .items_center()
            .gap(px(6.))
            .text_size(px(12.))
            .child(div().flex_none().text_color(theme.dim_text).child("❯"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_w_0()
                    .gap(px(6.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::BOLD)
                            .text_color(if chrome.focused {
                                theme.fg()
                            } else {
                                theme.dim_text
                            })
                            .child(SharedString::from(title_text)),
                    )
                    .children((!path.is_empty()).then(|| {
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .min_w_0()
                            .gap(px(4.))
                            .text_color(theme.dim_text)
                            .child(div().flex_none().child("·"))
                            .child(tail_text(path))
                    })),
            )
            .children(observing.then(|| {
                div()
                    .flex_none()
                    .text_color(theme.dim_text)
                    .child("observing")
            }))
            .children((chrome.number > 0).then(|| {
                div()
                    .flex_none()
                    // Apart from a long path that ends next to it.
                    .ml(px(6.))
                    .text_color(theme.dim_text)
                    .child(SharedString::from(chrome.number.to_string()))
            }))
            .child(icon_button("split-right", "◫", PaneEvent::SplitRight))
            .child(icon_button("split-down", "⊟", PaneEvent::SplitDown))
            .child(icon_button("maximize", "⤢", PaneEvent::ToggleMaximize))
            .child(icon_button("close", "✕", PaneEvent::Close));
        let mut grid = div()
            .relative()
            .flex_1()
            .min_h_0()
            .mx(px(8.))
            .mb(px(6.))
            .overflow_hidden()
            .child(element);
        if let Some(text) = banner(chrome, state) {
            let number = chrome.number;
            let restart = restartable(chrome, state).then(|| {
                div()
                    .id("restart")
                    .debug_selector(move || format!("pane-{number}-restart"))
                    .px(px(6.))
                    .rounded(px(3.))
                    .border_1()
                    .border_color(theme.dim_text)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.window_bg))
                    .child("Restart")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Restart)))
            });
            grid = grid.child(
                div()
                    .absolute()
                    .top(px(4.))
                    .right(px(4.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(2.))
                    .rounded(px(4.))
                    .bg(theme.header_bg)
                    .text_color(theme.fg())
                    .text_size(px(12.))
                    .child(SharedString::from(text))
                    .children(restart),
            );
        }
        if let Some(text) = empty_message(chrome) {
            grid = grid.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme.dim_text)
                    .child(text),
            );
        }
        div()
            .key_context(TERMINAL_CONTEXT)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .size_full()
            .flex()
            .flex_col()
            .rounded(px(10.))
            .overflow_hidden()
            .border_1()
            .border_color(if chrome.focused {
                theme.accent.opacity(0.45)
            } else {
                theme.border
            })
            .bg(theme.bg())
            .child(header)
            .child(grid)
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gpui::{
        Bounds, ClipboardItem, Entity, EntityInputHandler, Modifiers, MouseButton, MouseDownEvent,
        Pixels, Point, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase,
        VisualTestContext, bounds, point, px, size,
    };

    use super::{
        InputSink, PaneChrome, PaneView, banner, draws_cursor, empty_message, grid_size,
        header_parts, ime_cell, point_to_cell,
    };
    use crate::app::keymap::{Copy, Paste, bind_keys};
    use crate::core::config::GuiConfig;
    use crate::core::pane::PaneModel;
    use crate::core::pane::PaneState;
    use crate::core::selection::{LineRef, SelPoint};
    use crate::core::session::PaneId;
    use fleet_proto::AttachOk;
    use std::time::Duration;

    #[derive(Default)]
    struct Recorder {
        inputs: Mutex<Vec<Vec<u8>>>,
        resizes: Mutex<Vec<(u16, u16)>>,
        paging_checks: Mutex<u32>,
    }

    impl InputSink for Recorder {
        fn input(&self, _pane: PaneId, bytes: Vec<u8>) {
            self.inputs.lock().unwrap().push(bytes);
        }

        fn resize(&self, _pane: PaneId, cols: u16, rows: u16) {
            self.resizes.lock().unwrap().push((cols, rows));
        }

        fn check_paging(&self, _pane: PaneId) {
            *self.paging_checks.lock().unwrap() += 1;
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
        let (view, cx) = cx.add_window_view(|_window, cx| {
            PaneView::new(PaneId(1), model, sink_dyn, &GuiConfig::default(), cx)
        });
        cx.update(|window, cx| window.focus(&view.read(cx).focus_handle().clone()));
        cx.run_until_parked();
        (view, sink, cx)
    }

    fn blank_model() -> PaneModel {
        PaneModel::new(crate::app::theme::Theme::dark().palette)
    }

    /// A live pane with `n` history lines `h0..`, an 80×24 grid and the
    /// cursor on the last row.
    fn model_with_history(n: usize) -> PaneModel {
        let mut model = blank_model();
        model.on_attached(&AttachOk {
            agent_id: 1,
            cols: 80,
            rows: 24,
            history_tail: (0..n)
                .map(|i| format!("h{i}").into_bytes().into())
                .collect(),
            ..Default::default()
        });
        model.on_output(b"\x1b[24;1H");
        model
    }

    /// The middle of cell `(col, row)` in window coordinates.
    fn cell(
        view: &Entity<PaneView>,
        cx: &mut VisualTestContext,
        col: u16,
        row: u16,
    ) -> Point<Pixels> {
        let l = view.read_with(cx, |v, _| v.layout).expect("laid out");
        point(
            l.origin.x + l.cell_w * (f32::from(col) + 0.5),
            l.origin.y + l.line_h * (f32::from(row) + 0.5),
        )
    }

    fn wheel(cx: &mut VisualTestContext, position: Point<Pixels>, delta: ScrollDelta) {
        cx.simulate_event(ScrollWheelEvent {
            position,
            delta,
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
    }

    fn offset(view: &Entity<PaneView>, cx: &mut VisualTestContext) -> u64 {
        view.read_with(cx, |v, _| v.model().scroll_offset())
    }

    fn selected(view: &Entity<PaneView>, cx: &mut VisualTestContext) -> Option<String> {
        view.read_with(cx, |v, _| v.model().selection_text())
    }

    fn drag(
        cx: &mut VisualTestContext,
        from: Point<Pixels>,
        to: Point<Pixels>,
        modifiers: Modifiers,
    ) {
        cx.simulate_mouse_down(from, MouseButton::Left, modifiers);
        cx.simulate_mouse_move(to, MouseButton::Left, modifiers);
        cx.simulate_mouse_up(to, MouseButton::Left, modifiers);
    }

    #[gpui::test]
    fn wheel_scrolls_history_when_no_mouse_mode(cx: &mut TestAppContext) {
        let (view, sink, cx) = pane(cx, model_with_history(50));
        let at = cell(&view, cx, 3, 3);
        wheel(cx, at, ScrollDelta::Lines(point(0.0, 3.0)));
        assert_eq!(offset(&view, cx), 3);
        assert!(*sink.paging_checks.lock().unwrap() >= 1);
        assert!(sink.take().is_empty());

        // Pixel deltas scroll a line only once a whole line height adds up.
        let line_h = view.read_with(cx, |v, _| v.layout.unwrap().line_h);
        wheel(cx, at, ScrollDelta::Pixels(point(px(0.0), line_h * 0.6)));
        assert_eq!(offset(&view, cx), 3);
        wheel(cx, at, ScrollDelta::Pixels(point(px(0.0), line_h * 0.6)));
        assert_eq!(offset(&view, cx), 4);
        wheel(cx, at, ScrollDelta::Lines(point(0.0, -2.0)));
        assert_eq!(offset(&view, cx), 2);

        // A huge delta is clamped, and scrolling stops at the oldest line.
        wheel(cx, at, ScrollDelta::Lines(point(0.0, 1e9)));
        assert_eq!(offset(&view, cx), 50);
        // Typing returns to the live grid.
        cx.simulate_keystrokes("a");
        assert_eq!(offset(&view, cx), 0);
    }

    #[gpui::test]
    fn wheel_reports_when_mouse_mode_on(cx: &mut TestAppContext) {
        let mut model = model_with_history(50);
        model.on_output(b"\x1b[?1000h\x1b[?1006h");
        let (view, sink, cx) = pane(cx, model);
        sink.take();
        let at = cell(&view, cx, 3, 2);
        wheel(cx, at, ScrollDelta::Lines(point(0.0, 1.0)));
        assert_eq!(sink.take(), vec![b"\x1b[<64;4;3M".to_vec()]);
        wheel(cx, at, ScrollDelta::Lines(point(0.0, -2.0)));
        assert_eq!(sink.take(), vec![b"\x1b[<65;4;3M\x1b[<65;4;3M".to_vec()]);
        assert_eq!(offset(&view, cx), 0);
        // A huge delta is clamped to 100 reports.
        wheel(cx, at, ScrollDelta::Lines(point(0.0, 1e9)));
        assert_eq!(sink.take(), vec![b"\x1b[<64;4;3M".repeat(100)]);
    }

    #[gpui::test]
    fn wheel_on_alt_screen_sends_arrows(cx: &mut TestAppContext) {
        let mut model = model_with_history(50);
        model.on_output(b"\x1b[?1049h");
        let (view, sink, cx) = pane(cx, model);
        sink.take();
        let at = cell(&view, cx, 0, 0);
        wheel(cx, at, ScrollDelta::Lines(point(0.0, 2.0)));
        assert_eq!(sink.take(), vec![b"\x1b[A\x1b[A".to_vec()]);
        assert_eq!(offset(&view, cx), 0);
    }

    #[gpui::test]
    fn drag_selects_and_cmd_c_copies(cx: &mut TestAppContext) {
        let mut model = blank_model();
        model.on_output(b"hello world\r\nsecond line");
        let (view, sink, cx) = pane(cx, model);
        let (from, to) = (cell(&view, cx, 2, 0), cell(&view, cx, 5, 1));
        drag(cx, from, to, Modifiers::none());
        assert_eq!(selected(&view, cx).as_deref(), Some("llo world\nsecond"));

        cx.dispatch_action(Copy);
        let clip = cx.read_from_clipboard().and_then(|c| c.text());
        assert_eq!(clip.as_deref(), Some("llo world\nsecond"));

        // The keystrokes, not just the actions.
        cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
        cx.simulate_keystrokes("cmd-c");
        let clip = cx.read_from_clipboard().and_then(|c| c.text());
        assert_eq!(clip.as_deref(), Some("llo world\nsecond"));
        sink.take();
        cx.write_to_clipboard(ClipboardItem::new_string("pasted".to_owned()));
        cx.simulate_keystrokes("cmd-v");
        assert_eq!(sink.take(), vec![b"pasted".to_vec()]);

        // A plain click clears the selection.
        cx.simulate_click(from, Modifiers::none());
        assert_eq!(selected(&view, cx), None);
    }

    #[gpui::test]
    fn drag_selects_across_history_and_grid(cx: &mut TestAppContext) {
        let mut model = model_with_history(50);
        model.on_output(b"\x1b[1;1Hgrid top");
        let (view, _sink, cx) = pane(cx, model);
        let at = cell(&view, cx, 0, 0);
        wheel(cx, at, ScrollDelta::Lines(point(0.0, 2.0)));
        // Rows 0 and 1 show h48 and h49, row 2 the grid's first row.
        let (from, to) = (cell(&view, cx, 1, 1), cell(&view, cx, 3, 2));
        drag(cx, from, to, Modifiers::none());
        assert_eq!(selected(&view, cx).as_deref(), Some("49\ngrid"));
    }

    #[gpui::test]
    fn shift_bypasses_mouse_reporting(cx: &mut TestAppContext) {
        let mut model = blank_model();
        model.on_output(b"\x1b[?1002h\x1b[?1006hhello world");
        let (view, sink, cx) = pane(cx, model);
        sink.take();
        let (from, to) = (cell(&view, cx, 0, 0), cell(&view, cx, 4, 0));
        drag(cx, from, to, Modifiers::shift());
        assert!(sink.take().is_empty());
        assert_eq!(selected(&view, cx).as_deref(), Some("hello"));

        // Without Shift the button events are reported, not selected.
        drag(cx, from, to, Modifiers::none());
        assert_eq!(
            sink.take(),
            vec![
                b"\x1b[<0;1;1M".to_vec(),
                b"\x1b[<32;5;1M".to_vec(),
                b"\x1b[<0;5;1m".to_vec()
            ]
        );
    }

    #[gpui::test]
    fn double_click_selects_word_triple_click_line(cx: &mut TestAppContext) {
        let mut model = blank_model();
        model.on_output(b"cargo test -p fleet-gui");
        let (view, _sink, cx) = pane(cx, model);
        let at = cell(&view, cx, 16, 0);
        let down = |click_count| MouseDownEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::none(),
            click_count,
            first_mouse: false,
        };
        cx.simulate_event(down(1));
        cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
        cx.simulate_event(down(2));
        cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
        assert_eq!(selected(&view, cx).as_deref(), Some("fleet-gui"));
        cx.simulate_event(down(3));
        cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
        assert_eq!(
            selected(&view, cx).as_deref(),
            Some("cargo test -p fleet-gui")
        );
    }

    #[gpui::test]
    fn new_lines_pill_shows_while_scrolled_up_and_returns_to_bottom(cx: &mut TestAppContext) {
        let (view, _sink, cx) = pane(cx, model_with_history(50));
        let at = cell(&view, cx, 0, 0);
        wheel(cx, at, ScrollDelta::Lines(point(0.0, 5.0)));
        assert_eq!(view.read_with(cx, |v, _| v.pill), None);
        view.update(cx, |v, cx| {
            v.model().on_output(b"\r\nx\r\ny");
            cx.notify();
        });
        cx.run_until_parked();
        let pill = view.read_with(cx, |v, _| v.pill).expect("pill shown");
        assert_eq!(offset(&view, cx), 7);
        cx.simulate_click(pill.center(), Modifiers::none());
        assert_eq!(offset(&view, cx), 0);
        assert_eq!(view.read_with(cx, |v, _| v.pill), None);
        assert_eq!(selected(&view, cx), None);
    }

    #[gpui::test]
    fn ime_position_follows_the_cursor_else_the_top_left(cx: &mut TestAppContext) {
        let (view, _sink, cx) = pane(cx, model_with_history(50));
        let ime_bounds = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                view.update(cx, |v, cx| {
                    v.replace_and_mark_text_in_range(None, "に", None, window, cx);
                    let at = v.bounds_for_range(0..1, Bounds::default(), window, cx);
                    (at.map(|b| b.origin), ime_cell(&v.model()))
                })
            })
        };
        let l = view.read_with(cx, |v, _| v.layout).expect("laid out");
        // The cursor is on the last row.
        let (at, ime) = ime_bounds(cx);
        assert_eq!(ime, (0, 23));
        assert_eq!(at, Some(point(l.origin.x, l.origin.y + l.line_h * 23.0)));
        let top = cell(&view, cx, 0, 0);
        wheel(cx, top, ScrollDelta::Lines(point(0.0, 3.0)));
        let (at, ime) = ime_bounds(cx);
        assert_eq!(ime, (0, 0));
        assert_eq!(at, Some(l.origin));
        // The overlay draws there too (paint smoke test while scrolled).
        cx.run_until_parked();
    }

    #[gpui::test]
    fn unmark_text_commits_the_marked_text(cx: &mut TestAppContext) {
        let (view, sink, cx) = pane(cx, blank_model());
        cx.update(|window, cx| {
            view.update(cx, |v, cx| {
                v.replace_and_mark_text_in_range(None, "é", None, window, cx);
                v.unmark_text(window, cx);
                assert_eq!(v.marked_text_range(window, cx), None);
            });
        });
        assert_eq!(sink.take(), vec!["é".as_bytes().to_vec()]);
    }

    #[test]
    fn point_to_cell_clamps() {
        let b = bounds(point(px(10.0), px(20.0)), size(px(800.0), px(480.0)));
        let cell =
            |x: f32, y: f32| point_to_cell(point(px(x), px(y)), b, px(8.0), px(16.0), 80, 24);
        assert_eq!(cell(10.0, 20.0), (0, 0));
        assert_eq!(cell(10.0 + 8.0 * 2.5, 20.0 + 16.0 * 1.9), (2, 1));
        assert_eq!(cell(0.0, 0.0), (0, 0));
        assert_eq!(cell(5000.0, 5000.0), (79, 23));
        assert_eq!(cell(f32::NAN, -1.0), (0, 0));
        assert_eq!(
            point_to_cell(point(px(50.0), px(50.0)), b, px(8.0), px(16.0), 0, 0),
            (0, 0)
        );
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

    #[test]
    fn empty_pane_message_goes_by_the_layout_not_the_pane_state() {
        let empty = PaneChrome::default();
        // A pane with no agent is `Attaching`; it still says "select".
        assert_eq!(banner(&empty, PaneState::Attaching), None);
        assert_eq!(empty_message(&empty), Some("Select an agent or press ⌘N"));
        let starting = PaneChrome {
            starting: true,
            ..PaneChrome::default()
        };
        assert_eq!(empty_message(&starting), Some("starting…"));
        let shown = PaneChrome {
            has_agent: true,
            ..PaneChrome::default()
        };
        assert_eq!(empty_message(&shown), None);
        assert_eq!(
            banner(&shown, PaneState::Attaching).as_deref(),
            Some("attaching…")
        );
        assert_eq!(
            banner(&shown, PaneState::Exited(3)).as_deref(),
            Some("exited (3)")
        );
        assert_eq!(banner(&shown, PaneState::Live), None);
        assert_eq!(
            banner(&shown, PaneState::Disconnected).as_deref(),
            Some("disconnected — reconnecting")
        );
        let down = PaneChrome {
            reconnect_in: Some(Duration::from_millis(2500)),
            ..shown.clone()
        };
        assert_eq!(
            banner(&down, PaneState::Disconnected).as_deref(),
            Some("disconnected — reconnecting in 3s")
        );
    }

    #[test]
    fn cursor_only_for_an_agent_that_runs() {
        let empty = PaneChrome::default();
        assert!(!draws_cursor(&empty, PaneState::Attaching));
        assert!(!draws_cursor(&empty, PaneState::Live));
        let shown = PaneChrome {
            has_agent: true,
            ..PaneChrome::default()
        };
        assert!(draws_cursor(&shown, PaneState::Live));
        assert!(draws_cursor(&shown, PaneState::Attaching));
        assert!(draws_cursor(&shown, PaneState::Disconnected));
        assert!(!draws_cursor(&shown, PaneState::Exited(0)));
        assert!(!draws_cursor(&shown, PaneState::Lost));
        // Known to have ended, it gets none while disconnected either.
        let ended = PaneChrome {
            agent_ended: true,
            ..shown
        };
        assert!(!draws_cursor(&ended, PaneState::Disconnected));
    }

    #[test]
    fn header_prefers_the_osc_title() {
        let chrome = PaneChrome {
            program: Some("claude".into()),
            cwd: Some("~/dev".into()),
            ..PaneChrome::default()
        };
        assert_eq!(
            header_parts(&chrome, None),
            ("claude".into(), "~/dev".into())
        );
        assert_eq!(
            header_parts(&chrome, Some("Refactor")),
            ("Refactor".into(), "~/dev".into())
        );
        assert_eq!(header_parts(&chrome, Some("  ")).0, "claude");
    }
}
