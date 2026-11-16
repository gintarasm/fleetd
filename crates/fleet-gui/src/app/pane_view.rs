//! A terminal pane view: keyboard, IME, paste and mouse input for one
//! pane, drawn by [`TerminalElement`].

use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use fleet_term::CursorShape;
use gpui::prelude::FluentBuilder;
use gpui::{
    Animation, AnimationExt, AnyView, AnyWindowHandle, AppContext, Bounds, BoxShadow,
    ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Font, InteractiveElement,
    IntoElement, KeyDownEvent, Modifiers, ModifiersChangedEvent, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent,
    SharedString, Size, StatefulInteractiveElement, Styled, Subscription, Task, UTF16Selection,
    Window, div, point, px, size,
};

use super::controls::{self, ButtonKind};
use super::ended_view::ended_bar;
use super::fonts::terminal_font;
use super::icons;
use super::keymap::{Copy, Find, FindNext, FindPrevious, Paste, TERMINAL_CONTEXT, to_key};
use super::native;
use super::status_dot::status_dot;
use super::tail_text::tail_text;
use super::terminal_element::TerminalElement;
use super::text_field::TextField;
use super::theme::{Theme, Typography, current as theme};
use crate::core::config::GuiConfig;
use crate::core::find::status_label;
use crate::core::icons::Icon;
use crate::core::keys::{KeyMods, encode_key, encode_paste};
use crate::core::kill::{AgentButtons, agent_buttons};
use crate::core::links::{Files, Link, LinkTarget, Osc8, editor_command, link_at, pick_editor};
use crate::core::mouse::{MouseButton, MouseKind, encode_mouse, wheel_as_arrows};
use crate::core::pane::{PaneModel, PaneState, ViewRow};
use crate::core::scroll_feedback::{
    Fade, HIT_WIDTH, INSET, Thumb, Wake, end_jumps_to_bottom, offset_for_thumb, thumb,
};
use crate::core::selection::{LineRef, SelPoint};
use crate::core::session::{PaneId, Session};
use crate::core::shortcuts::{EmptyPane, HINT, empty_pane};
use crate::core::status::AgentStatus;
use crate::core::style::{self, radius, space, text};

/// At most this many lines are scrolled (or reported) per wheel event.
const MAX_WHEEL_LINES: f32 = 100.0;

/// History lines a find step scans, off the UI thread, holding the pane's
/// model lock (about a millisecond; ADR 0053).
const FIND_STEP_LINES: usize = 2000;

/// Width of the find bar's text field.
const FIND_FIELD_WIDTH: f32 = 180.0;

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
    /// The element's size (the grid plus what is left over at its edges).
    pub size: Size<Pixels>,
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
    /// Dragging the scrollbar's thumb, held `grab` points below its top.
    Thumb { grab: f32 },
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
    /// The agent's directory when it is on this machine: file paths in the
    /// pane resolve against it and become links (ADR 0046).
    pub link_dir: Option<String>,
    /// A spawn into this pane is on its way.
    pub starting: bool,
    /// When the control connection retries, while it is down.
    pub reconnect_in: Option<Duration>,
    /// The pane has keyboard focus (drawn in the header).
    pub focused: bool,
    /// The pane fills the window (its button offers Restore).
    pub maximized: bool,
    /// The agent's status (ADR 0042); none for a shell or no agent.
    pub status: Option<AgentStatus>,
    /// The agent list arrived and is empty: an empty pane welcomes.
    pub no_agents: bool,
    /// How long the ended agent ran, if known (the summary bar).
    pub run_time: Option<Duration>,
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
        PaneState::Attaching => Some("attaching…".to_owned()),
        // The summary bar at the bottom says how it ended.
        PaneState::Exited(_) | PaneState::Lost | PaneState::Live => None,
    }
}

/// The summary bar offers Restart (as ⌘R does): the pane's agent exited
/// or is gone.
pub fn restartable(chrome: &PaneChrome, state: PaneState) -> bool {
    chrome.has_agent && matches!(state, PaneState::Exited(_) | PaneState::Lost)
}

/// What a pane with no agent shows.
pub fn empty_state(chrome: &PaneChrome) -> EmptyPane {
    empty_pane(chrome.has_agent, chrome.starting, chrome.no_agents)
}

/// The text centred in a pane with no agent (not the welcome).
pub fn empty_message(chrome: &PaneChrome) -> Option<&'static str> {
    match empty_state(chrome) {
        EmptyPane::Starting => Some("starting…"),
        EmptyPane::Hint => Some(HINT),
        EmptyPane::None | EmptyPane::Welcome => None,
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

/// The pane header's hover group (its agent buttons show on hover).
const PANE_HEADER_GROUP: &str = "pane-header";

/// A header button, asking the workspace to act on this pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneEvent {
    SplitRight,
    SplitDown,
    ToggleMaximize,
    Close,
    /// The summary bar's Restart: the ended agent again, in this pane.
    Restart,
    /// The header's Stop: kill the pane's running agent (after asking).
    Kill,
    /// Remove, in the header or the summary bar: forgets the ended agent.
    Remove,
    /// A right-click asks for the pane's context menu: on the header, or
    /// on the grid (`on_grid`) while the agent does not take the mouse.
    ContextMenu {
        on_grid: bool,
    },
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
    /// The window whose activation the cursor follows, and that
    /// subscription (renewed when the view moves to a new window).
    activation: Option<(AnyWindowHandle, Subscription)>,
    /// What to start here, while the pane a split opened is empty (the
    /// split chooser); drawn in place of [`empty_message`].
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
    /// The last query, brought back when find opens again.
    find_query: String,
    find_case: bool,
    /// The search running off the UI thread, while lines are left.
    find_task: Option<Task<()>>,
}

/// The find bar's field and its subscription.
struct FindBar {
    field: Entity<TextField>,
    _changes: Subscription,
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
            flash: None,
            flashes: 0,
            editor: config.editor.clone(),
            hover: None,
            open_file: Arc::new(spawn_opener),
            find: None,
            find_query: String::new(),
            find_case: false,
            find_task: None,
            chooser: None,
        }
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

    /// ⌘F: opens the find bar (or focuses it) with the last query
    /// selected.
    fn open_find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.is_none() {
            let query = self.find_query.clone();
            let field = cx.new(|cx| TextField::new(query, "Find", cx));
            let changes = cx.observe(&field, |v, field, cx| {
                let text = field.read(cx).text();
                if text != v.find_query {
                    v.find_query = text.to_owned();
                    v.search(cx);
                }
            });
            self.find = Some(FindBar {
                field,
                _changes: changes,
            });
            self.search(cx);
        }
        if let Some(bar) = &self.find {
            bar.field.update(cx, |f, cx| f.select_all_text(cx));
            window.focus(&bar.field.read(cx).focus_handle().clone());
        }
        cx.notify();
    }

    /// Starts searching for the find bar's query.
    fn search(&mut self, cx: &mut Context<Self>) {
        self.model().find_open(&self.find_query, self.find_case);
        self.find_task = None;
        self.continue_search(cx);
        cx.notify();
    }

    /// Scans what is left to scan in steps on the background executor, each
    /// under the model lock, redrawing as matches come in. Nothing here
    /// runs while drawing.
    fn continue_search(&mut self, cx: &mut Context<Self>) {
        if self.find_task.is_some() || !self.model().find_pending() {
            return;
        }
        let model = Arc::clone(&self.model);
        self.find_task = Some(cx.spawn(async move |view, cx| {
            loop {
                let m = Arc::clone(&model);
                let step = cx
                    .background_executor()
                    .spawn(async move { lock(&m).find_step(FIND_STEP_LINES) })
                    .await;
                let go_on = view.update(cx, |v, cx| {
                    if step.changed {
                        cx.notify();
                    }
                    if !step.more {
                        v.find_task = None;
                        // At the oldest loaded line: page in older history.
                        v.sink.check_paging(v.pane);
                    }
                    step.more
                });
                if !matches!(go_on, Ok(true)) {
                    break;
                }
            }
        }));
    }

    /// The next match up (Enter, ⌘G) or down (⇧Enter, ⌘⇧G).
    fn step_find(&mut self, up: bool, cx: &mut Context<Self>) {
        if self.find.is_none() {
            return;
        }
        if self.model().find_next(up) {
            self.sink.check_paging(self.pane);
            cx.notify();
        }
    }

    fn find_next(&mut self, _: &FindNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step_find(true, cx);
    }

    fn find_previous(&mut self, _: &FindPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.step_find(false, cx);
    }

    fn toggle_find_case(&mut self, cx: &mut Context<Self>) {
        self.find_case = !self.find_case;
        self.search(cx);
    }

    /// Esc: closes the find bar; the keys go back to the terminal.
    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find = None;
        self.find_task = None;
        self.model().find_close();
        window.focus(&self.focus);
        cx.notify();
    }

    fn find_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        match ev.keystroke.key.as_str() {
            "enter" if !m.platform => self.step_find(!m.shift, cx),
            "escape" => self.close_find(window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    /// The find bar's count, while it is open.
    pub(crate) fn find_label(&self) -> Option<String> {
        self.find.as_ref()?;
        let status = self.model().find_status()?;
        Some(status_label(status, !self.find_query.is_empty()))
    }

    /// The link underlined under the pointer: its line and columns.
    #[cfg(test)]
    pub(crate) fn hovered_link(&self) -> Option<(LineRef, u16, u16)> {
        self.hover
    }

    /// The link under `pos`, if any, on the line it is on.
    fn link_under(&self, pos: Point<Pixels>) -> Option<(LineRef, Link)> {
        let l = self.layout?;
        let model = self.model();
        let term = model.terminal();
        let (cols, rows) = term.size();
        let grid = Bounds::new(
            l.origin,
            size(l.cell_w * f32::from(cols), l.line_h * f32::from(rows)),
        );
        if !grid.contains(&pos) {
            return None;
        }
        let (col, row) = point_to_cell(pos, grid, l.cell_w, l.line_h, cols, rows);
        let mut cells = Vec::with_capacity(usize::from(cols));
        let (line, osc8) = match model.view_row(row) {
            ViewRow::History(abs) => {
                model.history_cells(abs, &mut cells);
                (LineRef::History(abs), None)
            }
            ViewRow::Grid(r) => {
                term.row_cells(r, &mut cells);
                (LineRef::Grid(r), term.hyperlink_at(r, col))
            }
        };
        let osc8 = osc8.as_ref().map(|(uri, start, end)| Osc8 {
            uri,
            start: *start,
            end: *end,
        });
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let is_file = |p: &std::path::Path| p.is_file();
        let files = self.chrome.link_dir.as_deref().map(|cwd| Files {
            cwd: std::path::Path::new(cwd),
            home: home.as_deref(),
            is_file: &is_file,
        });
        link_at(&cells, col, osc8, files.as_ref()).map(|link| (line, link))
    }

    /// Underlines the link under `pos` while ⌘ is held and no drag is on.
    fn update_hover(&mut self, pos: Point<Pixels>, m: Modifiers, cx: &mut Context<Self>) {
        let hover = (m.platform && self.drag.is_none())
            .then(|| self.link_under(pos))
            .flatten()
            .map(|(line, l)| (line, l.start, l.end));
        if hover != self.hover {
            self.hover = hover;
            cx.notify();
        }
    }

    pub(crate) fn modifiers_changed(
        &mut self,
        m: Modifiers,
        pos: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.update_hover(pos, m, cx);
    }

    /// Opens a link: URLs through the system, files in the editor.
    fn open_link(&self, target: LinkTarget, cx: &mut Context<Self>) {
        match target {
            LinkTarget::Url(url) => cx.open_url(&url),
            LinkTarget::File { path, line, col } => {
                let visual = std::env::var("VISUAL").ok();
                let env_editor = std::env::var("EDITOR").ok();
                let editor = pick_editor(&self.editor, visual.as_deref(), env_editor.as_deref());
                let argv = editor_command(editor.as_deref(), &path, line, col);
                (self.open_file)(argv, editor_command(None, &path, None, None));
            }
        }
    }

    /// The strip along the grid's right edge that the scrollbar takes
    /// presses in, in window coordinates.
    pub(crate) fn scrollbar_track(&self) -> Option<Bounds<Pixels>> {
        let l = self.layout?;
        let height = l.size.height - px(2.0 * INSET);
        (height > px(0.)).then(|| {
            Bounds::new(
                point(
                    l.origin.x + l.size.width - px(HIT_WIDTH),
                    l.origin.y + px(INSET),
                ),
                size(px(HIT_WIDTH), height),
            )
        })
    }

    /// The thumb on `track` for the view as it is.
    fn thumb_on(&self, track: Bounds<Pixels>) -> Option<Thumb> {
        let model = self.model();
        let rows = model.terminal().size().1;
        thumb(
            f32::from(track.size.height),
            model.scrollback().len(),
            rows,
            model.scroll_offset(),
        )
    }

    /// The view scrolled: the scrollbar shows (again) from now.
    fn show_scrollbar(&mut self) {
        if !self.fade.is_active() {
            self.fade_reduce_motion = (self.reduce_motion)();
        }
        self.fade.show(Instant::now());
    }

    /// The scrollbar's opacity this render, scheduling what keeps it
    /// current: frames while it fades, one timer to the end of its hold,
    /// nothing once it is gone (or never shown).
    fn scrollbar_opacity(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<f32> {
        if !self.fade.is_active() {
            return None;
        }
        let now = Instant::now();
        let rm = self.fade_reduce_motion;
        let opacity = self.fade.opacity(now, rm);
        match self.fade.wake(now, rm) {
            Wake::Frame => window.request_animation_frame(),
            Wake::After(d) => {
                let due = now + d;
                if self.fade_timer.as_ref().is_none_or(|(at, _)| *at != due) {
                    let task = cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(d).await;
                        let _ = this.update(cx, |_, cx| cx.notify());
                    });
                    self.fade_timer = Some((due, task));
                }
            }
            Wake::Idle => {
                self.fade.settle(now, rm);
                self.fade_timer = None;
            }
        }
        (opacity > 0.0).then_some(opacity)
    }

    /// Moves the view so the dragged thumb's top is `grab` above `y`.
    fn drag_thumb(&mut self, y: Pixels, grab: f32, cx: &mut Context<Self>) {
        let Some(track) = self.scrollbar_track() else {
            return;
        };
        let Some(t) = self.thumb_on(track) else {
            return;
        };
        let top = f32::from(y - track.origin.y) - grab;
        let track_h = f32::from(track.size.height);
        {
            let mut model = self.model();
            let history = model.scrollback().len();
            model.scroll_to(offset_for_thumb(track_h, t.height, history, top));
        }
        self.sink.check_paging(self.pane);
        cx.notify();
    }

    /// The pointer let go of the scrollbar: it fades after a new hold.
    fn release_scrollbar(&mut self, cx: &mut Context<Self>) {
        self.fade.hold(false);
        self.show_scrollbar();
        cx.notify();
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

    /// Settings changed: the terminal font and Option as Meta follow at
    /// once (the next layout resizes the grid if the cells changed).
    pub fn apply_config(&mut self, config: &GuiConfig, cx: &mut Context<Self>) {
        self.option_as_meta = config.option_as_meta;
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

    /// Some text is selected (the context menu's Copy).
    pub fn has_selection(&self) -> bool {
        self.model().has_selection()
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
        let mut model = self.model();
        if model.scroll_offset() > 0 {
            model.scroll_to_bottom();
            cx.notify();
        }
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
        if self.chip.is_some_and(|p| p.contains(&ev.position)) {
            self.drag = None;
            self.model().scroll_to_bottom();
            cx.notify();
            return;
        }
        let Some(button) = to_button(ev.button) else {
            return;
        };
        // A shown scrollbar takes presses on its strip: on the thumb it is
        // dragged from there, elsewhere the thumb jumps under the pointer.
        let track = self.scrollbar_track().filter(|t| {
            self.scrollbar_shown && button == MouseButton::Left && t.contains(&ev.position)
        });
        if let Some((track, t)) = track.and_then(|tr| self.thumb_on(tr).map(|t| (tr, t))) {
            let y = f32::from(ev.position.y - track.origin.y);
            let on_thumb = y >= t.top && y <= t.top + t.height;
            let grab = if on_thumb { y - t.top } else { t.height / 2.0 };
            self.drag = Some(Drag::Thumb { grab });
            self.fade.hold(true);
            if !on_thumb {
                self.drag_thumb(ev.position.y, grab, cx);
            }
            cx.notify();
            return;
        }
        // ⌘-click on a link opens it, even when the agent reports the mouse.
        let link = (button == MouseButton::Left && ev.modifiers.platform)
            .then(|| self.link_under(ev.position))
            .flatten();
        if let Some((_, link)) = link {
            self.drag = None;
            self.open_link(link.target, cx);
            return;
        }
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
        if button == MouseButton::Right {
            drop(model);
            cx.emit(PaneEvent::ContextMenu { on_grid: true });
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
            // Over a showing scrollbar, it stays.
            if self.fade.is_active() {
                let over = self.scrollbar_shown
                    && self
                        .scrollbar_track()
                        .is_some_and(|t| t.contains(&ev.position));
                if over != self.fade.is_held() {
                    if over {
                        self.fade.hold(true);
                    } else {
                        self.release_scrollbar(cx);
                    }
                }
            }
            self.update_hover(ev.position, ev.modifiers, cx);
            return;
        };
        let pressed = ev.pressed_button.and_then(to_button);
        if let Drag::Thumb { grab } = drag {
            if pressed == Some(MouseButton::Left) {
                self.drag_thumb(ev.position.y, grab, cx);
            } else {
                // The release happened where it was not seen.
                self.drag = None;
                self.release_scrollbar(cx);
            }
            return;
        }
        let mut model = lock(&self.model);
        let Some(at) = cell_at(self.layout, &model, ev.position) else {
            return;
        };
        match drag {
            // Handled above, without the model locked.
            Drag::Thumb { .. } => {}
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
        if let Drag::Thumb { .. } = drag {
            self.drag = None;
            self.release_scrollbar(cx);
            return;
        }
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
        self.show_scrollbar();
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
        // New output while find is open: scan it (off the UI thread).
        if self.find.is_some() {
            self.continue_search(cx);
        }
        let theme = theme(cx);
        let focused = window.is_window_active() && self.focus.is_focused(window);
        let (state, title, observing) = {
            let model = self.model();
            let state = model.state();
            let observing = state == PaneState::Live && !model.is_primary();
            (state, model.title(), observing)
        };
        let scrollbar = self.scrollbar_opacity(window, cx);
        self.scrollbar_shown = scrollbar.is_some();
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
            link: self.hover,
            scrollbar,
        };
        let chrome = &self.chrome;
        let (title_text, path) = header_parts(chrome, title.as_deref());
        let icon_button = |id: &'static str, icon: Icon, event: PaneEvent| {
            controls::icon_button(id, icon, &theme, true)
                .debug_selector(move || format!("pane-{}-{id}", chrome.number))
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(event)))
        };
        // Stop for a running agent, Restart and Remove for an ended one:
        // always on the focused pane, on hover on the others.
        let ended = chrome.agent_ended || restartable(chrome, state);
        let agent_buttons = match agent_buttons(chrome.has_agent, ended) {
            AgentButtons::None => Vec::new(),
            AgentButtons::Stop => vec![("stop", Icon::Stop, PaneEvent::Kill)],
            // Restart once the pane holds the final screen (as ⌘R).
            AgentButtons::RestartRemove => restartable(chrome, state)
                .then_some(("restart-agent", Icon::Restart, PaneEvent::Restart))
                .into_iter()
                .chain([("remove", Icon::Trash, PaneEvent::Remove)])
                .collect(),
        };
        let agent_buttons = (!agent_buttons.is_empty()).then(|| {
            div()
                .flex()
                .flex_row()
                .flex_none()
                .children(agent_buttons.into_iter().map(|(id, icon, event)| {
                    icon_button(id, icon, event).when(!chrome.focused, |b| {
                        b.invisible()
                            .group_hover(PANE_HEADER_GROUP, |s| s.visible())
                    })
                }))
        });
        let header = div()
            .id("pane-header")
            .group(PANE_HEADER_GROUP)
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|_, _, _, cx| cx.emit(PaneEvent::ContextMenu { on_grid: false })),
            )
            .flex()
            .flex_row()
            .flex_none()
            .h(px(style::chrome::PANE_HEADER_HEIGHT))
            .pl(px(space::MD))
            .pr(px(space::XS))
            .items_center()
            .gap(px(space::SM - space::XXS))
            .type_style(text::CALLOUT)
            .child(icons::icon(Icon::Terminal).text_color(if chrome.focused {
                theme.accent
            } else {
                theme.tertiary
            }))
            .children(chrome.status.map(|s| {
                let number = chrome.number;
                status_dot(s, &theme, unix_ms(), (self.reduce_motion)())
                    .debug_selector(move || format!("pane-{number}-status-{}", s.label()))
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_w_0()
                    .gap(px(space::SM - space::XXS))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(
                        div()
                            .flex_none()
                            .type_style(text::HEADLINE)
                            .text_color(if chrome.focused {
                                theme.text
                            } else {
                                theme.secondary
                            })
                            .child(SharedString::from(title_text)),
                    )
                    .children((!path.is_empty()).then(|| {
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .min_w_0()
                            .gap(px(space::XS))
                            .text_color(theme.secondary)
                            .child(div().flex_none().text_color(theme.tertiary).child("·"))
                            .child(tail_text(path))
                    })),
            )
            .children(observing.then(|| {
                div()
                    .flex_none()
                    .px(px(space::SM - space::XXS))
                    .rounded(px(radius::SM))
                    .bg(theme.hover)
                    .type_style(text::SUBHEADLINE)
                    .text_color(theme.secondary)
                    .child("observing")
            }))
            .children((chrome.number > 0).then(|| {
                div()
                    .flex_none()
                    // Apart from a long path that ends next to it.
                    .ml(px(space::SM - space::XXS))
                    .mr(px(space::XS))
                    .type_style(text::SUBHEADLINE)
                    .text_color(theme.tertiary)
                    .child(SharedString::from(format!("⌘{}", chrome.number)))
            }))
            .children(agent_buttons)
            .child(icon_button(
                "split-right",
                Icon::SplitRight,
                PaneEvent::SplitRight,
            ))
            .child(icon_button(
                "split-down",
                Icon::SplitDown,
                PaneEvent::SplitDown,
            ))
            .child(icon_button(
                "maximize",
                if chrome.maximized {
                    Icon::Restore
                } else {
                    Icon::Maximize
                },
                PaneEvent::ToggleMaximize,
            ))
            .child(icon_button("close", Icon::Close, PaneEvent::Close));
        let mut grid = div()
            .relative()
            .flex_1()
            .min_h_0()
            .mx(px(space::SM))
            .mb(px(space::SM - space::XXS))
            .overflow_hidden()
            .child(element);
        let find_bar = self.find.as_ref().map(|bar| {
            let number = chrome.number;
            let label = self.find_label().unwrap_or_default();
            let case_on = self.find_case;
            let step = |id: &'static str, icon: Icon, up: bool| {
                controls::icon_button(id, icon, &theme, true)
                    .debug_selector(move || format!("pane-{number}-{id}"))
                    .on_click(cx.listener(move |v, _, _, cx| v.step_find(up, cx)))
            };
            div()
                .id("find")
                .debug_selector(move || format!("pane-{number}-find"))
                .occlude()
                .on_key_down(cx.listener(Self::find_key_down))
                .absolute()
                .top(px(space::XS))
                .right(px(space::XS))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::XS))
                .p(px(space::XS))
                .rounded(px(radius::MD))
                .border_1()
                .border_color(theme.border)
                .bg(theme.raised)
                .shadow_md()
                .type_style(text::CALLOUT)
                .child(div().w(px(FIND_FIELD_WIDTH)).child(bar.field.clone()))
                .child(
                    div()
                        .flex_none()
                        .min_w(px(space::XL * 3.0))
                        .text_color(theme.secondary)
                        .whitespace_nowrap()
                        .child(SharedString::from(label)),
                )
                .child(
                    div()
                        .id("find-case")
                        .debug_selector(move || format!("pane-{number}-find-case"))
                        .flex_none()
                        .px(px(space::XS))
                        .rounded(px(radius::SM))
                        .bg(if case_on {
                            theme.selected
                        } else {
                            gpui::transparent_black()
                        })
                        .text_color(if case_on { theme.text } else { theme.tertiary })
                        .cursor_pointer()
                        .hover(move |s| s.bg(theme.hover))
                        .child("Aa")
                        .on_click(cx.listener(|v, _, _, cx| v.toggle_find_case(cx))),
                )
                .child(step("find-up", Icon::ChevronUp, true))
                .child(step("find-down", Icon::ChevronDown, false))
                .child(
                    controls::icon_button("find-close", Icon::Close, &theme, true)
                        .debug_selector(move || format!("pane-{number}-find-close"))
                        .on_click(cx.listener(|v, _, window, cx| v.close_find(window, cx))),
                )
        });
        let find_open = find_bar.is_some();
        if let Some(text) = banner(chrome, state) {
            // Below the find bar when that is open.
            let top = if find_open {
                space::XS * 4.0 + style::chrome::CONTROL_HEIGHT + space::SM
            } else {
                space::XS
            };
            grid = grid.child(
                div()
                    .absolute()
                    .top(px(top))
                    .right(px(space::XS))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::SM))
                    .px(px(space::MD))
                    .py(px(space::XS))
                    .rounded(px(radius::MD))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.raised)
                    .shadow_md()
                    .text_color(theme.text)
                    .type_style(text::CALLOUT)
                    .child(SharedString::from(text)),
            );
        }
        let chooser = self.chooser.clone().filter(|_| !chrome.has_agent);
        if chooser.is_none() && empty_state(chrome) == EmptyPane::Welcome {
            grid = grid.child(welcome(&theme, cx));
        }
        if let Some(chooser) = chooser {
            grid = grid.child(chooser);
        } else if let Some(text) = empty_message(chrome) {
            grid = grid.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .type_style(text::BODY)
                    .text_color(theme.tertiary)
                    .child(text),
            );
        }
        grid = grid.children(find_bar);
        div()
            .key_context(TERMINAL_CONTEXT)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            // Keyboard focus path only: ⌘ pressed over an unfocused pane shows
            // its link on the next pointer move.
            .on_modifiers_changed(cx.listener(|v, ev: &ModifiersChangedEvent, window, cx| {
                v.modifiers_changed(ev.modifiers, window.mouse_position(), cx);
            }))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::open_find))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_previous))
            .size_full()
            .flex()
            .flex_col()
            .rounded(px(radius::LG))
            .overflow_hidden()
            // A card lifted off the window's material.
            .shadow(vec![BoxShadow {
                color: theme.shadow,
                offset: point(px(0.), px(1.)),
                blur_radius: px(space::XS),
                spread_radius: px(0.),
            }])
            .border_1()
            .border_color(if chrome.focused {
                theme.accent.opacity(0.55)
            } else {
                theme.border
            })
            .bg(theme.card)
            .relative()
            .child(header)
            .child(grid)
            .children(self.flash.as_ref().map(|(n, _)| {
                let ring = div()
                    .absolute()
                    .inset_0()
                    .rounded(px(radius::LG))
                    .border_2()
                    .border_color(theme.warning);
                if (self.reduce_motion)() {
                    ring.into_any_element()
                } else {
                    ring.with_animation(
                        ("bell-flash", *n),
                        Animation::new(Duration::from_millis(FLASH_MS)),
                        |el, t| el.opacity(1.0 - t),
                    )
                    .into_any_element()
                }
            }))
            .children(ended_bar(self, state, &theme, cx))
    }
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
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

/// The first-run welcome (no agents yet): New agent, Terminal, Install
/// agent skill and the shortcuts, centred in the empty pane.
fn welcome(theme: &Theme, cx: &mut Context<PaneView>) -> impl IntoElement {
    let button = |id: &'static str, label: &'static str, kind, event: PaneEvent| {
        controls::button(id, label, theme, kind, true)
            .debug_selector(move || format!("welcome-{id}"))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(event)))
    };
    let new_agent = button(
        "new-agent",
        "New Agent  ⌘N",
        ButtonKind::Primary,
        PaneEvent::NewAgent,
    );
    let terminal = button(
        "terminal",
        "Terminal  ⌘T",
        ButtonKind::Secondary,
        PaneEvent::NewTerminal,
    );
    let skill = button(
        "install-skill",
        "Install Agent Skill",
        ButtonKind::Secondary,
        PaneEvent::InstallSkill,
    );
    let shortcuts = div()
        .id("shortcuts")
        .debug_selector(|| "welcome-shortcuts".to_owned())
        .type_style(text::CALLOUT)
        .text_color(theme.accent)
        .cursor_pointer()
        .hover(|s| s.opacity(0.8))
        .child("Keyboard shortcuts  ⌘/")
        .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::ShowShortcuts)));
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .id("welcome")
                .debug_selector(|| "welcome".to_owned())
                .flex()
                .flex_col()
                .items_center()
                .gap(px(space::MD))
                .text_color(theme.text)
                .child(div().type_style(text::TITLE).child("Welcome to fleet"))
                .child(
                    div()
                        .type_style(text::BODY)
                        .text_color(theme.secondary)
                        .child("Start an agent, or a plain shell, to get going."),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(space::SM))
                        .child(new_agent)
                        .child(terminal)
                        .child(skill),
                )
                .child(shortcuts),
        )
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
    use crate::core::pane::PaneState;
    use crate::core::pane::{PaneModel, ViewRow};
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

    fn hovered(view: &Entity<PaneView>, cx: &mut VisualTestContext) -> Option<(LineRef, u16, u16)> {
        view.read_with(cx, |v, _| v.hovered_link())
    }

    /// Records the commands the view would run to open files.
    fn record_opens(
        view: &Entity<PaneView>,
        cx: &mut VisualTestContext,
    ) -> Arc<Mutex<Vec<Vec<String>>>> {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&opened);
        view.update(cx, |v, _| {
            v.open_file = Arc::new(move |argv, _| sink.lock().unwrap().push(argv));
        });
        opened
    }

    #[gpui::test]
    fn cmd_hover_underlines_a_url_and_cmd_click_opens_it(cx: &mut TestAppContext) {
        let mut model = blank_model();
        model.on_output(b"PR https://a.io/p/1 ok");
        let (view, _sink, cx) = pane(cx, model);
        let at = cell(&view, cx, 6, 0);

        // Without Command: no link, and a click opens nothing.
        cx.simulate_mouse_move(at, None, Modifiers::none());
        assert_eq!(hovered(&view, cx), None);
        cx.simulate_click(at, Modifiers::none());
        assert_eq!(cx.opened_url(), None);

        cx.simulate_mouse_move(at, None, Modifiers::command());
        assert_eq!(hovered(&view, cx), Some((LineRef::Grid(0), 3, 19)));
        // Off the link.
        let p = cell(&view, cx, 20, 0);
        cx.simulate_mouse_move(p, None, Modifiers::command());
        assert_eq!(hovered(&view, cx), None);

        cx.simulate_click(at, Modifiers::command());
        assert_eq!(cx.opened_url().as_deref(), Some("https://a.io/p/1"));
        // The click does not start a selection.
        assert_eq!(selected(&view, cx), None);
    }

    #[gpui::test]
    fn pressing_and_releasing_cmd_over_a_link_toggles_the_underline(cx: &mut TestAppContext) {
        let mut model = blank_model();
        model.on_output(b"https://a.io");
        let (view, _sink, cx) = pane(cx, model);
        let p = cell(&view, cx, 2, 0);
        cx.simulate_mouse_move(p, None, Modifiers::none());
        cx.simulate_modifiers_change(Modifiers::command());
        assert_eq!(hovered(&view, cx), Some((LineRef::Grid(0), 0, 12)));
        cx.simulate_modifiers_change(Modifiers::none());
        assert_eq!(hovered(&view, cx), None);
    }

    #[gpui::test]
    fn cmd_click_wins_over_mouse_reporting(cx: &mut TestAppContext) {
        let mut model = blank_model();
        model.on_output(b"\x1b[?1000h\x1b[?1006hhttps://a.io");
        let (view, sink, cx) = pane(cx, model);
        sink.take();
        let p = cell(&view, cx, 2, 0);
        cx.simulate_click(p, Modifiers::command());
        assert_eq!(cx.opened_url().as_deref(), Some("https://a.io"));
        assert!(sink.take().is_empty());
    }

    #[gpui::test]
    fn an_osc8_link_opens_its_uri(cx: &mut TestAppContext) {
        let mut model = blank_model();
        model.on_output(b"see \x1b]8;;https://a.io/pr/9\x1b\\the PR\x1b]8;;\x1b\\.");
        let (view, _sink, cx) = pane(cx, model);
        let at = cell(&view, cx, 5, 0);
        cx.simulate_mouse_move(at, None, Modifiers::command());
        assert_eq!(hovered(&view, cx), Some((LineRef::Grid(0), 4, 10)));
        cx.simulate_click(at, Modifiers::command());
        assert_eq!(cx.opened_url().as_deref(), Some("https://a.io/pr/9"));
    }

    #[gpui::test]
    fn cmd_click_opens_an_existing_file_of_a_local_agent_in_the_editor(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        let mut model = blank_model();
        model.on_output(b"--> src/main.rs:10:5 and src/gone.rs:1");
        let (view, _sink, cx) = pane(cx, model);
        let opened = record_opens(&view, cx);
        let at = cell(&view, cx, 6, 0);

        // A remote agent (no local directory): no file links.
        cx.simulate_mouse_move(at, None, Modifiers::command());
        assert_eq!(hovered(&view, cx), None);

        let cwd = dir.path().to_string_lossy().into_owned();
        view.update(cx, |v, _| {
            v.chrome.link_dir = Some(cwd.clone());
            v.editor = "zed".to_owned();
        });
        cx.simulate_mouse_move(at, None, Modifiers::command());
        assert_eq!(hovered(&view, cx), Some((LineRef::Grid(0), 4, 20)));
        cx.simulate_click(at, Modifiers::command());
        let file = format!("{cwd}/src/main.rs:10:5");
        assert_eq!(*opened.lock().unwrap(), vec![vec!["zed".to_owned(), file]]);

        // A path that does not exist is no link.
        let p = cell(&view, cx, 28, 0);
        cx.simulate_mouse_move(p, None, Modifiers::command());
        assert_eq!(hovered(&view, cx), None);
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
    fn new_output_chip_shows_while_scrolled_up_and_returns_to_bottom(cx: &mut TestAppContext) {
        let (view, _sink, cx) = pane(cx, model_with_history(50));
        let at = cell(&view, cx, 0, 0);
        wheel(cx, at, ScrollDelta::Lines(point(0.0, 5.0)));
        assert_eq!(view.read_with(cx, |v, _| v.chip), None);
        // A redraw in place is no new output.
        view.update(cx, |v, cx| {
            v.model().on_output(b"\rspinning");
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.chip), None);
        view.update(cx, |v, cx| {
            v.model().on_output(b"\r\nx\r\ny");
            cx.notify();
        });
        cx.run_until_parked();
        let chip = view.read_with(cx, |v, _| v.chip).expect("chip shown");
        assert_eq!(offset(&view, cx), 7);
        cx.simulate_click(chip.center(), Modifiers::none());
        assert_eq!(offset(&view, cx), 0);
        assert_eq!(view.read_with(cx, |v, _| v.chip), None);
        assert_eq!(selected(&view, cx), None);
    }

    #[gpui::test]
    fn end_returns_a_scrolled_up_view_to_the_bottom(cx: &mut TestAppContext) {
        let (view, sink, cx) = pane(cx, model_with_history(50));
        let at = cell(&view, cx, 0, 0);
        wheel(cx, at, ScrollDelta::Lines(point(0.0, 5.0)));
        view.update(cx, |v, cx| {
            v.model().on_output(b"\r\nx");
            cx.notify();
        });
        cx.run_until_parked();
        assert!(view.read_with(cx, |v, _| v.chip.is_some()));
        cx.simulate_keystrokes("end");
        assert_eq!(offset(&view, cx), 0);
        assert!(sink.take().is_empty(), "End stays in the GUI");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.chip), None);
        // At the bottom, End goes to the agent.
        cx.simulate_keystrokes("end");
        assert_eq!(sink.take(), vec![b"\x1b[F".to_vec()]);
    }

    /// A pane whose scrollbar shows at once (Reduce Motion: no fade-in).
    fn pane_without_motion(
        cx: &mut TestAppContext,
        model: PaneModel,
    ) -> (Entity<PaneView>, Arc<Recorder>, &mut VisualTestContext) {
        let (view, sink, cx) = pane(cx, model);
        view.update(cx, |v, _| v.reduce_motion = || true);
        (view, sink, cx)
    }

    #[gpui::test]
    fn the_scrollbar_shows_only_once_the_view_scrolls(cx: &mut TestAppContext) {
        let (view, _sink, cx) = pane_without_motion(cx, model_with_history(50));
        // Idle: nothing shown, nothing scheduled.
        view.update(cx, |v, cx| {
            v.model().on_output(b"\r\nx");
            cx.notify();
        });
        cx.run_until_parked();
        let idle = view.read_with(cx, |v, _| {
            (
                v.scrollbar_shown,
                v.fade.is_active(),
                v.fade_timer.is_some(),
            )
        });
        assert_eq!(idle, (false, false, false));
        let at = cell(&view, cx, 0, 0);
        wheel(cx, at, ScrollDelta::Lines(point(0.0, 5.0)));
        cx.run_until_parked();
        let shown = view.read_with(cx, |v, _| (v.scrollbar_shown, v.fade_timer.is_some()));
        assert_eq!(shown, (true, true), "shown, one timer to fade it");
    }

    #[gpui::test]
    fn dragging_the_thumb_scrolls_the_view(cx: &mut TestAppContext) {
        let (view, sink, cx) = pane_without_motion(cx, model_with_history(50));
        let at = cell(&view, cx, 0, 0);
        wheel(cx, at, ScrollDelta::Lines(point(0.0, 1.0)));
        cx.run_until_parked();
        let track = view
            .read_with(cx, |v, _| v.scrollbar_track())
            .expect("laid out");
        let thumb = view.read_with(cx, |v, _| v.thumb_on(track)).expect("thumb");
        let grab = point(
            track.center().x,
            track.origin.y + px(thumb.top + thumb.height / 2.0),
        );
        let top = point(track.center().x, track.origin.y);
        cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
        assert!(view.read_with(cx, |v, _| v.fade.is_held()));
        cx.simulate_mouse_move(top, MouseButton::Left, Modifiers::none());
        assert_eq!(offset(&view, cx), 50, "dragged to the oldest line");
        let bottom = point(track.center().x, track.bottom() + px(100.0));
        cx.simulate_mouse_move(bottom, MouseButton::Left, Modifiers::none());
        assert_eq!(offset(&view, cx), 0, "dragged back to the live grid");
        cx.simulate_mouse_up(bottom, MouseButton::Left, Modifiers::none());
        assert!(!view.read_with(cx, |v, _| v.fade.is_held()));
        // Nothing was selected or sent, and paging was checked.
        assert_eq!(selected(&view, cx), None);
        assert!(sink.take().is_empty());
        assert!(*sink.paging_checks.lock().unwrap() >= 2);
    }

    #[gpui::test]
    fn a_press_on_the_track_jumps_the_thumb_there(cx: &mut TestAppContext) {
        let (view, _sink, cx) = pane_without_motion(cx, model_with_history(50));
        let at = cell(&view, cx, 0, 0);
        wheel(cx, at, ScrollDelta::Lines(point(0.0, 1.0)));
        cx.run_until_parked();
        let track = view
            .read_with(cx, |v, _| v.scrollbar_track())
            .expect("laid out");
        let top = point(track.center().x, track.origin.y + px(1.0));
        cx.simulate_mouse_down(top, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(top, MouseButton::Left, Modifiers::none());
        assert_eq!(offset(&view, cx), 50);
    }

    #[gpui::test]
    fn a_hidden_scrollbar_leaves_the_right_edge_to_the_selection(cx: &mut TestAppContext) {
        let (view, _sink, cx) = pane_without_motion(cx, model_with_history(50));
        let track = view
            .read_with(cx, |v, _| v.scrollbar_track())
            .expect("laid out");
        let edge = point(track.center().x, track.origin.y + px(1.0));
        cx.simulate_mouse_down(edge, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(edge, MouseButton::Left, Modifiers::none());
        assert_eq!(offset(&view, cx), 0);
        assert!(!view.read_with(cx, |v, _| v.fade.is_held()));
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

    /// A live 80×24 pane whose history has `needle` on lines 10 and 40.
    fn model_with_needles() -> PaneModel {
        let mut model = blank_model();
        model.on_attached(&AttachOk {
            agent_id: 1,
            cols: 80,
            rows: 24,
            history_tail: (0..50)
                .map(|i| match i {
                    10 | 40 => format!("a Needle {i}"),
                    _ => format!("h{i}"),
                })
                .map(|s| s.into_bytes().into())
                .collect(),
            ..Default::default()
        });
        model.on_output(b"\x1b[24;1H");
        model
    }

    fn find_status(
        view: &Entity<PaneView>,
        cx: &mut VisualTestContext,
    ) -> Option<crate::core::find::FindStatus> {
        view.read_with(cx, |v, _| v.model().find_status())
    }

    fn shows_line(view: &Entity<PaneView>, cx: &mut VisualTestContext, line: u64) -> bool {
        view.read_with(cx, |v, _| {
            let mut rows = Vec::new();
            v.model().view_rows(24, &mut rows);
            rows.contains(&ViewRow::History(line))
        })
    }

    #[gpui::test]
    fn cmd_f_finds_in_history_and_steps_through_matches(cx: &mut TestAppContext) {
        let (view, sink, cx) = pane(cx, model_with_needles());
        sink.take();
        cx.simulate_keystrokes("cmd-f");
        assert!(cx.debug_bounds("pane-0-find").is_some());
        cx.simulate_input("needle");
        cx.run_until_parked();
        // The query went to the find field, not to the agent.
        assert!(sink.take().is_empty());
        let status = find_status(&view, cx).unwrap();
        assert_eq!((status.index, status.count), (Some(2), 2));
        assert!(shows_line(&view, cx, 40));
        assert_eq!(
            view.read_with(cx, |v, _| v.find_label()).as_deref(),
            Some("2 of 2")
        );

        // Enter goes up to the older match, Shift-Enter back down.
        cx.simulate_keystrokes("enter");
        assert_eq!(find_status(&view, cx).unwrap().index, Some(1));
        assert!(shows_line(&view, cx, 10));
        cx.simulate_keystrokes("shift-enter");
        assert_eq!(find_status(&view, cx).unwrap().index, Some(2));
        cx.simulate_keystrokes("cmd-g");
        assert_eq!(find_status(&view, cx).unwrap().index, Some(1));
        cx.simulate_keystrokes("cmd-shift-g");
        assert_eq!(find_status(&view, cx).unwrap().index, Some(2));
        assert!(sink.take().is_empty());

        // Case sensitive: "needle" is not "Needle".
        let case = cx.debug_bounds("pane-0-find-case").unwrap().center();
        cx.simulate_click(case, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(find_status(&view, cx).unwrap().count, 0);
        assert_eq!(
            view.read_with(cx, |v, _| v.find_label()).as_deref(),
            Some("No matches")
        );
        cx.simulate_click(case, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(find_status(&view, cx).unwrap().count, 2);

        // Esc closes it and gives the keys back to the agent.
        cx.simulate_keystrokes("escape");
        assert_eq!(find_status(&view, cx), None);
        assert_eq!(view.read_with(cx, |v, _| v.find_label()), None);
        assert!(sink.take().is_empty());
        cx.simulate_keystrokes("enter");
        assert_eq!(sink.take(), vec![b"\r".to_vec()]);

        // ⌘F again brings the last query back, selected.
        cx.simulate_keystrokes("cmd-f");
        cx.run_until_parked();
        assert_eq!(find_status(&view, cx).unwrap().count, 2);
        cx.simulate_input("h4");
        cx.run_until_parked();
        // h4, h40..h49 minus the needle on 40.
        assert_eq!(find_status(&view, cx).unwrap().count, 10);
    }

    #[gpui::test]
    fn find_bar_buttons_step_and_close(cx: &mut TestAppContext) {
        let (view, _sink, cx) = pane(cx, model_with_needles());
        cx.simulate_keystrokes("cmd-f");
        cx.simulate_input("needle");
        cx.run_until_parked();
        let click = |cx: &mut VisualTestContext, id: &'static str| {
            let at = cx.debug_bounds(id).unwrap().center();
            cx.simulate_click(at, Modifiers::none());
            cx.run_until_parked();
        };
        click(cx, "pane-0-find-up");
        assert_eq!(find_status(&view, cx).unwrap().index, Some(1));
        click(cx, "pane-0-find-down");
        assert_eq!(find_status(&view, cx).unwrap().index, Some(2));
        click(cx, "pane-0-find-close");
        assert_eq!(find_status(&view, cx), None);
        // The keys go back to the terminal.
        let focused = cx.update(|window, cx| view.read(cx).focus_handle().is_focused(window));
        assert!(focused);
    }

    #[gpui::test]
    fn find_follows_output_while_open(cx: &mut TestAppContext) {
        let (view, _sink, cx) = pane(cx, model_with_needles());
        cx.simulate_keystrokes("cmd-f");
        cx.simulate_input("needle");
        cx.run_until_parked();
        let model = view.read_with(cx, |v, _| Arc::clone(v.model_handle()));
        model.lock().unwrap().on_output(b"\r\nneedle again");
        // The session notifies the view of output.
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(find_status(&view, cx).unwrap().count, 3);
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
        // The summary bar says how it ended, not a banner.
        assert_eq!(banner(&shown, PaneState::Exited(3)), None);
        assert_eq!(banner(&shown, PaneState::Lost), None);
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
