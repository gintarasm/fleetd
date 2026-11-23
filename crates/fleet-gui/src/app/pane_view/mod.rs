//! A terminal pane view: keyboard, IME, paste and mouse input for one
//! pane, drawn by
//! [`TerminalElement`](crate::app::terminal_element::TerminalElement).

use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gpui::{
    AnyView, AnyWindowHandle, Bounds, ClipboardEntry, ClipboardItem, Context, EventEmitter,
    ExternalPaths, FocusHandle, Font, Image, KeyDownEvent, Pixels, Point, Size, Subscription, Task,
    Window, px,
};

use crate::app::controls;
use crate::app::fonts::terminal_font;
use crate::app::keymap::{
    ClearScrollback, Copy, Paste, ScrollPageDown, ScrollPageUp, ScrollToBottom, ScrollToTop,
    SelectAll, to_key,
};
use crate::app::native;
use crate::core::config::GuiConfig;
use crate::core::cursor_blink::{Blink, BlinkChoice};
use crate::core::drop_paste::{
    DROP_NEEDS_LOCAL, IMAGE_NOT_SAVED, PasteKind, dropped_text, image_extension, paste_kind,
    save_pasted_image,
};
use crate::core::feedback::{self, Tween};
use crate::core::keys::{encode_key, encode_paste};
use crate::core::mouse::encode_focus;
use crate::core::pane::{PaneModel, ViewScroll};
use crate::core::scroll_feedback::{Fade, end_jumps_to_bottom};
use crate::core::selection::LineRef;
use crate::core::session::{PaneId, Session};
use crate::core::settings::OptionAsMeta;

mod find;
mod ime;
mod mouse;
mod render;
#[cfg(test)]
mod tests;

use crate::core::pane_chrome::restartable;
pub use crate::core::pane_chrome::{PaneChrome, grid_size};
// The workspace tests ask what an empty pane shows.
#[cfg(test)]
pub(crate) use crate::core::pane_chrome::empty_state;
use find::FindBar;
pub(crate) use ime::ime_cell;
use mouse::Drag;

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

/// The grid as last laid out: where it is and how big its cells are.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GridLayout {
    pub origin: Point<Pixels>,
    /// The element's size (the grid plus what is left over at its edges).
    pub size: Size<Pixels>,
    pub cell_w: Pixels,
    pub line_h: Pixels,
    pub cols: u16,
    pub rows: u16,
}

/// A header button, asking the workspace to act on this pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneEvent {
    SplitRight,
    SplitDown,
    ToggleMaximize,
    Close,
    /// The summary bar's Restart: the ended agent again, in this pane.
    Restart,
    /// A double-click on the name: rename the pane's agent.
    Rename,
    /// Reply… on the question strip: the Answer sheet (ADR 0078).
    Answer,
    /// A choice button on the question strip: answer with choice `n`.
    Choose(usize),
    /// Edit Labels… in the pane's menu.
    EditLabels,
    /// The menu's Duplicate Agent…: the New agent sheet, prefilled with
    /// the pane's agent (ADR 0084).
    Duplicate,
    /// The header's Stop: kill the pane's running agent (after asking).
    Kill,
    /// Remove, in the header or the summary bar: forgets the ended agent.
    Remove,
    /// A right-click asks for the pane's context menu: on the header, or
    /// on the grid (`on_grid`) while the agent does not take the mouse.
    ContextMenu {
        on_grid: bool,
    },
    /// A message for the toast (a drop or paste that cannot work here).
    Notice(&'static str),
    /// The first-run welcome's buttons.
    NewAgent,
    NewTerminal,
    InstallSkill,
    ShowShortcuts,
}

impl EventEmitter<PaneEvent> for PaneView {}

pub struct PaneView {
    pane: PaneId,
    pub(crate) chrome: PaneChrome,
    model: Arc<Mutex<PaneModel>>,
    sink: Arc<dyn InputSink>,
    focus: FocusHandle,
    option_as_meta: OptionAsMeta,
    font: Font,
    font_size: Pixels,
    /// IME preedit text, not yet sent.
    marked: Option<String>,
    marked_selection: Option<Range<usize>>,
    pub(crate) layout: Option<GridLayout>,
    drag: Option<Drag>,
    /// Wheel movement not yet scrolled, in lines (positive up).
    wheel_lines: f32,
    /// Where the "New output" chip was last drawn, if it was.
    pub(crate) chip: Option<Bounds<Pixels>>,
    /// The overlay scrollbar's fade.
    pub(crate) fade: Fade,
    /// Reduce Motion, read when the scrollbar starts showing.
    fade_reduce_motion: bool,
    /// The scrollbar drew in the last render (it takes presses then).
    scrollbar_shown: bool,
    /// The redraw that ends the scrollbar's hold, and when it is due.
    fade_timer: Option<(Instant, Task<()>)>,
    /// Reads Reduce Motion (replaced in tests).
    pub(crate) reduce_motion: fn() -> bool,
    /// The window whose activation the cursor and focus reports follow,
    /// and the subscriptions to its activation and to this pane's focus
    /// (renewed when the view moves to a new window).
    activation: Option<(AnyWindowHandle, [Subscription; 3])>,
    /// The focus last told to the program (DECSET 1004, decision 0095):
    /// the window is active and focus is in this pane.
    reported_focus: bool,
    /// Where the pointer was last seen during a drag-select, and the
    /// repeating timer scrolling the view while it is past the grid's top
    /// or bottom (decision 0095).
    drag_at: Point<Pixels>,
    auto_scroll: Option<Task<()>>,
    /// The focus ring's (and the dim veil's) cross-fade since focus last
    /// moved to or from this pane.
    focus_fade: Option<Tween>,
    /// When the selection was last copied, while "Copied" shows, and the
    /// timer that hides it.
    copied: Option<(Instant, Task<()>)>,
    /// What to start here, while the pane a split opened is empty (the
    /// split chooser); drawn in place of
    /// [`empty_message`](crate::core::pane_chrome::empty_message).
    chooser: Option<AnyView>,
    /// The bell flash being drawn (its number, so each restarts the fade),
    /// and the task that ends it.
    flash: Option<(u32, Task<()>)>,
    flashes: u32,
    /// `[gui] editor`, for ⌘-clicked files.
    pub(crate) editor: String,
    /// The link under the pointer while ⌘ is held: its line and columns.
    hover: Option<(LineRef, u16, u16)>,
    /// Runs a command that opens a file, and the one to run when that
    /// cannot start (replaced in tests).
    pub(crate) open_file: Arc<dyn Fn(Vec<String>, Vec<String>)>,
    /// The find bar, while open.
    find: Option<FindBar>,
    /// Where a pasted image is saved (`$TMPDIR`; replaced in tests).
    pub(crate) paste_dir: std::path::PathBuf,
    /// The last query, brought back when find opens again.
    find_query: String,
    find_case: bool,
    /// The search running off the UI thread, while lines are left.
    find_task: Option<Task<()>>,
    /// `[gui] cursor_blink`.
    cursor_blink: BlinkChoice,
    /// The cursor's blink phase; each keypress restarts it shown.
    pub(crate) blink: Blink,
    /// Redraws this pane at each blink toggle, while its cursor blinks
    /// (only the focused pane's can: ADR 0096).
    pub(crate) blink_timer: Option<Task<()>>,
    /// Drawn without its header: the peek (ADR 0103) titles it.
    pub(crate) headless: bool,
}

/// Runs `argv` detached; when it cannot start (an editor not on the app's
/// `PATH`), opens the file in the default text editor instead.
fn spawn_opener(argv: Vec<String>, fallback: Vec<String>) {
    let run = |argv: &[String]| {
        std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    };
    let child = run(&argv).or_else(|_| run(&fallback));
    match child {
        // Reaped off the UI thread.
        Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
        Err(e) => eprintln!(
            "fleet-gui: cannot open {}: {e}",
            fallback.last().map_or("", String::as_str)
        ),
    }
}

/// How long the bell flash lasts.
const FLASH_MS: u64 = 600;

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
            font: terminal_font(&config.font_family, cx),
            font_size: px(config.font_size),
            marked: None,
            marked_selection: None,
            layout: None,
            drag: None,
            wheel_lines: 0.0,
            chip: None,
            fade: Fade::default(),
            fade_reduce_motion: false,
            scrollbar_shown: false,
            fade_timer: None,
            reduce_motion: native::reduce_motion,
            activation: None,
            reported_focus: false,
            drag_at: Point::default(),
            auto_scroll: None,
            focus_fade: None,
            copied: None,
            flash: None,
            flashes: 0,
            editor: config.editor.clone(),
            hover: None,
            open_file: Arc::new(spawn_opener),
            find: None,
            paste_dir: std::env::temp_dir(),
            find_query: String::new(),
            find_case: false,
            find_task: None,
            chooser: None,
            cursor_blink: config.cursor_blink,
            blink: Blink::new(Instant::now()),
            blink_timer: None,
            headless: false,
        }
    }

    /// Sets the terminal font size alone (the peek shrinks its font to
    /// fit the agent's grid, ADR 0103).
    pub fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        if self.font_size != px(size) {
            self.font_size = px(size);
            cx.notify();
        }
    }

    /// The width and height of one cell at `size` points, in this view's
    /// font, as the grid lays it out.
    pub fn cell_size(&self, size: f32, window: &Window) -> (f32, f32) {
        let ts = window.text_system();
        let font_id = ts.resolve_font(&self.font);
        let size = px(size);
        let cell_w = ts
            .advance(font_id, size, 'm')
            .map_or(size * 0.6, |a| a.width);
        let line_h = (ts.ascent(font_id, size) + ts.descent(font_id, size).abs()).ceil();
        (cell_w.into(), line_h.into())
    }

    /// Draws `chooser` while the pane has no agent (or no longer).
    pub fn set_chooser(&mut self, chooser: Option<AnyView>, cx: &mut Context<Self>) {
        if self.chooser.as_ref().map(AnyView::entity_id) != chooser.as_ref().map(AnyView::entity_id)
        {
            self.chooser = chooser;
            cx.notify();
        }
    }

    /// The agent rang the bell: the card's border flashes (fading out,
    /// or shown and then removed under Reduce Motion).
    pub fn flash(&mut self, cx: &mut Context<Self>) {
        self.flashes = self.flashes.wrapping_add(1);
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(FLASH_MS))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.flash = None;
                cx.notify();
            });
        });
        self.flash = Some((self.flashes, task));
        cx.notify();
    }

    /// A bell flash is being drawn.
    pub fn flashing(&self) -> bool {
        self.flash.is_some()
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
            // Focus moved (not the first facts): the ring cross-fades.
            let moved = self.chrome.number > 0 && self.chrome.focused != chrome.focused;
            if moved && !controls::reduces_motion(cx) {
                self.focus_fade = Some(Tween::new(Instant::now(), feedback::FOCUS_FADE_MS));
            }
            self.chrome = chrome;
            cx.notify();
        }
    }

    /// Settings changed: the terminal font and Option as Meta follow at
    /// once (the next layout resizes the grid if the cells changed).
    pub fn apply_config(&mut self, config: &GuiConfig, cx: &mut Context<Self>) {
        self.option_as_meta = config.option_as_meta;
        if self.cursor_blink != config.cursor_blink {
            self.cursor_blink = config.cursor_blink;
            cx.notify();
        }
        let font = terminal_font(&config.font_family, cx);
        let font_size = px(config.font_size);
        if font != self.font || font_size != self.font_size {
            self.font = font;
            self.font_size = font_size;
            cx.notify();
        }
    }

    /// The terminal font size, in points.
    pub fn font_size(&self) -> f32 {
        f32::from(self.font_size)
    }

    /// The cursor turns hollow when the window loses activation, and the
    /// program hears of focus changes (decision 0095). The view outlives
    /// its window (the app is resident), so this follows whichever window
    /// draws it.
    fn follow_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = window.window_handle();
        if self.activation.as_ref().is_some_and(|(h, _)| *h == handle) {
            return;
        }
        let subs = [
            cx.observe_window_activation(window, |this, window, cx| {
                this.report_focus(window, cx);
                cx.notify();
            }),
            cx.on_focus_in(&self.focus, window, Self::report_focus),
            cx.on_focus_out(&self.focus, window, |this, _, window, cx| {
                this.report_focus(window, cx);
            }),
        ];
        self.activation = Some((handle, subs));
        // The listeners start after this frame, which may have moved focus
        // here (a new pane): look again once it is drawn.
        cx.defer_in(window, Self::report_focus);
    }

    /// Tells the program when the pane gains or loses focus (focus in this
    /// pane, find bar included, of the active window), while it asked for
    /// focus reports (DECSET 1004). The change is noted either way, so a
    /// program that turns the mode on later hears only later changes.
    fn report_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Focus in the pane or in it (the find bar, a split's chooser).
        let focused = window.is_window_active() && self.focus.contains_focused(window, cx);
        if focused == self.reported_focus {
            return;
        }
        self.reported_focus = focused;
        let modes = self.model().terminal().modes();
        if let Some(bytes) = encode_focus(focused, &modes) {
            // Not `send`: a focus report leaves a scrolled-up view where it is.
            self.sink.input(self.pane, bytes);
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Some text is selected (the context menu's Copy).
    /// The agent asked for bracketed paste (Send to Agents…, ADR 0083).
    pub fn bracketed_paste(&self) -> bool {
        self.model().terminal().modes().bracketed_paste
    }

    pub fn has_selection(&self) -> bool {
        self.model().has_selection()
    }

    /// The view can scroll from the keyboard, is scrolled up, and has
    /// history (the context menu's Scroll to Top, Scroll to Bottom and
    /// Clear Scrollback).
    pub fn scroll_state(&self) -> (bool, bool, bool) {
        let model = self.model();
        (
            model.can_scroll_view(),
            model.scroll_offset() > 0,
            !model.scrollback().is_empty(),
        )
    }

    /// The pane's agent exited or is gone (the context menu's Restart).
    pub fn restartable(&self) -> bool {
        restartable(&self.chrome, self.model().state())
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
        self.restart_blink(cx);
        let mut model = self.model();
        if model.scroll_offset() > 0 {
            model.scroll_to_bottom();
            cx.notify();
        }
    }

    /// Typing shows the cursor: the blink starts again from its shown
    /// half (the next frame restarts the timer to match).
    fn restart_blink(&mut self, cx: &mut Context<Self>) {
        self.blink.restart(Instant::now());
        if self.blink_timer.take().is_some() {
            cx.notify();
        }
    }

    /// Keeps the blink timer running while the cursor blinks, and only
    /// then: it wakes at each toggle and redraws this pane.
    fn follow_blink(&mut self, blinking: bool, cx: &mut Context<Self>) {
        if !blinking {
            self.blink_timer = None;
            return;
        }
        if self.blink_timer.is_some() {
            return;
        }
        // Blinking starts (focus came here, or a key was typed) shown.
        self.blink.restart(Instant::now());
        self.blink_timer = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(wait) = this.update(cx, |v, _| v.blink.until_toggle(Instant::now())) else {
                    break;
                };
                cx.background_executor().timer(wait).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // While composing, the IME owns the keys; the find field's keys
        // are its own.
        if self.marked.is_some() || !self.focus.is_focused(window) {
            return;
        }
        let Some(key) = to_key(&ev.keystroke) else {
            return;
        };
        let scrolled = self.model().scroll_offset() > 0;
        if end_jumps_to_bottom(&key, scrolled) {
            self.model().scroll_to_bottom();
            cx.notify();
            cx.stop_propagation();
            return;
        }
        let modes = self.model().terminal().modes();
        // `None`: the key is text; it reaches `replace_text_in_range`.
        let meta = key.mods.alt && {
            let (left, right) = native::option_sides();
            self.option_as_meta.applies(left, right)
        };
        if let Some(bytes) = encode_key(&key, &modes, meta) {
            self.send(bytes, cx);
            cx.stop_propagation();
        }
    }

    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.model().selection_text();
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            // "Copied" shows for a moment (one timer, no ticking).
            let hide = cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(feedback::COPIED_MS))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.copied = None;
                    cx.notify();
                });
            });
            self.copied = Some((Instant::now(), hide));
            cx.notify();
        }
    }

    /// "Copied" is showing.
    pub fn copied_shown(&self) -> bool {
        self.copied.is_some()
    }

    /// How strongly the focus ring is drawn now, 0..=1, and whether it is
    /// still fading (another frame is due).
    fn ring(&mut self, now: Instant) -> (f32, bool) {
        let fade = self.focus_fade.filter(|t| t.running(now));
        if fade.is_none() {
            self.focus_fade = None;
        }
        (
            feedback::ring(self.chrome.focused, fade.map(|t| t.value(now))),
            fade.is_some(),
        )
    }

    /// ⇧PageUp, ⇧PageDown, ⌘↑ and ⌘↓ (ADR 0082). On the alternate screen
    /// the key goes on to the program; ⌘↑ and ⌘↓ from the find field move
    /// its caret instead.
    fn scroll_view(&mut self, scroll: ViewScroll, window: &mut Window, cx: &mut Context<Self>) {
        let caret_key = matches!(scroll, ViewScroll::Top | ViewScroll::Bottom);
        if caret_key && !self.focus.is_focused(window) {
            cx.propagate();
            return;
        }
        let before = self.model().scroll_offset();
        if !self.model().scroll_view(scroll) {
            cx.propagate();
            return;
        }
        if self.model().scroll_offset() != before {
            self.show_scrollbar();
            self.sink.check_paging(self.pane);
            cx.notify();
        }
    }

    fn scroll_page_up(&mut self, _: &ScrollPageUp, window: &mut Window, cx: &mut Context<Self>) {
        self.scroll_view(ViewScroll::PageUp, window, cx);
    }

    fn scroll_page_down(
        &mut self,
        _: &ScrollPageDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scroll_view(ViewScroll::PageDown, window, cx);
    }

    fn scroll_to_top(&mut self, _: &ScrollToTop, window: &mut Window, cx: &mut Context<Self>) {
        self.scroll_view(ViewScroll::Top, window, cx);
    }

    fn scroll_to_bottom(
        &mut self,
        _: &ScrollToBottom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scroll_view(ViewScroll::Bottom, window, cx);
    }

    /// ⌘A: the loaded history and the grid, ready for ⌘C.
    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.model().select_all();
        cx.notify();
    }

    /// ⌘K: clears this view's history only (ADR 0082).
    fn clear_scrollback(&mut self, _: &ClearScrollback, _: &mut Window, cx: &mut Context<Self>) {
        self.model().clear_scrollback();
        cx.notify();
    }

    /// ⌘V: the clipboard's text; else its image, saved to a temp file,
    /// as that file's path (ADR 0081).
    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let text = item.text();
        let image = item.into_entries().find_map(|e| match e {
            ClipboardEntry::Image(image) => Some(image),
            ClipboardEntry::String(_) => None,
        });
        match paste_kind(text.is_some(), image.is_some(), self.chrome.local) {
            PasteKind::Text => self.paste_text(&text.unwrap_or_default(), cx),
            PasteKind::Image => {
                if let Some(image) = image {
                    self.paste_image(image, cx);
                }
            }
            PasteKind::Notice(text) => cx.emit(PaneEvent::Notice(text)),
            PasteKind::Nothing => {}
        }
    }

    /// Pastes `text` into the agent, bracketed when it asked for that.
    pub fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let modes = self.model().terminal().modes();
        self.send(encode_paste(text, &modes), cx);
    }

    /// Saves `image` to a new temp file off the UI thread, then pastes its
    /// path.
    fn paste_image(&mut self, image: Image, cx: &mut Context<Self>) {
        let Some(ext) = image_extension(image.format.mime_type()) else {
            return;
        };
        let dir = self.paste_dir.clone();
        let at = unix_ms();
        let saved = cx
            .background_executor()
            .spawn(async move { save_pasted_image(&dir, at, ext, &image.bytes) });
        cx.spawn(async move |this, cx| {
            let saved = saved.await;
            let _ = this.update(cx, |this, cx| match saved {
                Ok(path) => {
                    if let Some(text) = dropped_text(&[path.to_string_lossy()]) {
                        this.paste_text(&text, cx);
                    }
                }
                Err(e) => {
                    eprintln!("fleet-gui: cannot save the pasted image: {e}");
                    cx.emit(PaneEvent::Notice(IMAGE_NOT_SAVED));
                }
            });
        })
        .detach();
    }

    /// Files dropped from Finder: the pane gets the keyboard and their
    /// paths are typed (ADR 0081).
    fn drop_paths(&mut self, paths: &ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let paths: Vec<String> = paths
            .paths()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        self.type_paths(&paths, cx);
    }

    /// Types `paths`, quoted, into the pane's running agent; for an agent
    /// on another machine, a notice instead.
    pub(crate) fn type_paths(&mut self, paths: &[String], cx: &mut Context<Self>) {
        if !self.chrome.has_agent || self.chrome.agent_ended {
            return;
        }
        if !self.chrome.local {
            cx.emit(PaneEvent::Notice(DROP_NEEDS_LOCAL));
            return;
        }
        if let Some(text) = dropped_text(paths) {
            self.paste_text(&text, cx);
        }
    }
}

fn lock(model: &Mutex<PaneModel>) -> MutexGuard<'_, PaneModel> {
    model.lock().unwrap_or_else(|e| e.into_inner())
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
