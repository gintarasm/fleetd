//! Headless VT grid per agent (decision 0005): PTY output is fed through a
//! [`Terminal`]; on (re)attach a client receives [`Terminal::snapshot`] and
//! then the live stream, so reattach cost is O(screen).
//!
//! The snapshot carries the visible grid, the active SGR pen, the cursor, the
//! saved cursor (DECSC), the kitty keyboard flags and the terminal modes a
//! client needs to keep interacting (alt screen, input, mouse, cursor
//! visibility, wrap, origin, insert, LNM). While the alt screen is active it
//! carries the primary screen behind it too, so `?1049l` on the client shows
//! what it shows on the node. Known limits:
//! - The scroll region (DECSTBM) is not carried: alacritty keeps it private and
//!   exposes no accessor, so the client starts with a full-screen region.
//! - Kitty keyboard flags: only the current flags of each screen are carried
//!   (at most one stack level on the client), not the stack below them;
//!   alacritty keeps the stacks private.
//! - Not carried either: charsets (also those in the saved cursor), tab stops,
//!   hyperlinks and soft-wrap (WRAPLINE) markers.
//!
//! Synchronized updates (`?2026h` … `?2026l`) are buffered by the parser; a
//! frame whose ESU never arrives is applied on the next [`Terminal::feed`] after
//! its timeout (150 ms) expires. [`Terminal::flush_pending_sync`] applies a
//! pending frame at once (before a snapshot, so the snapshot plus the bytes
//! that follow reproduce the screen).
//!
//! Replies to terminal queries (DSR/CPR, DA, …) are collected and returned by
//! [`Terminal::take_replies`]; the owner decides whether to send them to the
//! agent.
//!
//! Rows that scroll off the top of the primary screen are rendered as ANSI
//! lines the moment they scroll off and queued with `CSI 3 J` (erase saved
//! lines) as [`HistoryEvent`]s, which [`Terminal::drain_history`] hands out in
//! stream order. The parser drives alacritty through a thin handler that does
//! this (see `recorder`); alacritty itself keeps no scrollback. A row is one
//! grid row: a long line soft-wrapped by the agent's terminal yields several
//! rows.

mod history;
mod parse;
mod recorder;
mod view;

pub use history::HistoryEvent;
pub use parse::{cells_text, parse_history_line};
pub use view::{Cell, CellStyle, Color, CursorShape, CursorView, Modes, Palette, Underline};

use history::{History, Pen, glyph, last_non_blank, put_cells, put_glyph};
use recorder::Recorder;

use std::cell::RefCell;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Cursor, Dimensions, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell as GridCell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Handler, KeyboardModes, KeyboardModesApplyBehavior, Processor, Rgb,
};

/// Grid size handed to alacritty. Scrollback is disabled: rows are rendered as
/// history as they scroll off (see [`recorder`]).
struct Size {
    cols: usize,
    rows: usize,
}

impl Size {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: usize::from(cols.max(1)),
            rows: usize::from(rows.max(1)),
        }
    }
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Kitty keyboard flags a client terminal supports (disambiguate escape
/// codes only); a client's query reply is masked to these.
pub const KITTY_SUPPORTED_FLAGS: u8 = 0b1;

/// State only a client terminal has: the palette that answers colour queries.
struct ClientState {
    palette: Mutex<Palette>,
}

/// Collects the bytes alacritty wants written back to the PTY, and the window
/// title.
///
/// The node's listener drops the kitty keyboard query reply (`CSI ? flags u`)
/// and ignores colour queries: kitty keyboard tracking is on only so snapshots
/// can carry the flags, and the node must not tell an agent that the (unknown)
/// client terminal supports the protocol. A client listener (see
/// [`Terminal::new_client`]) keeps the kitty reply, masked to
/// [`KITTY_SUPPORTED_FLAGS`], and answers OSC 4/10/11/12 from its palette.
#[derive(Clone, Default)]
struct Replies {
    buf: Arc<Mutex<Vec<u8>>>,
    title: Arc<Mutex<Option<String>>>,
    /// A BEL arrived since [`Terminal::take_bell`] last looked.
    bell: Arc<AtomicBool>,
    client: Option<Arc<ClientState>>,
}

impl Replies {
    fn push(&self, bytes: &[u8]) {
        self.buf
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(bytes);
    }
}

impl EventListener for Replies {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                if let Some(digits) = kitty_keyboard_report(&text) {
                    if self.client.is_none() {
                        return;
                    }
                    let flags =
                        digits.parse::<u32>().unwrap_or(0) & u32::from(KITTY_SUPPORTED_FLAGS);
                    self.push(format!("\x1b[?{flags}u").as_bytes());
                    return;
                }
                self.push(text.as_bytes());
            }
            Event::ColorRequest(index, format) => {
                if let Some(client) = &self.client {
                    let rgb = client
                        .palette
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .rgb(index);
                    self.push(
                        format(Rgb {
                            r: rgb.0,
                            g: rgb.1,
                            b: rgb.2,
                        })
                        .as_bytes(),
                    );
                }
            }
            Event::Title(title) => {
                *self.title.lock().unwrap_or_else(|e| e.into_inner()) = Some(title);
            }
            Event::ResetTitle => {
                *self.title.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            Event::Bell => self.bell.store(true, Ordering::Relaxed),
            _ => {}
        }
    }
}

/// The digits of `CSI ? <digits> u`, alacritty's reply to a kitty keyboard
/// flags query.
fn kitty_keyboard_report(text: &str) -> Option<&str> {
    text.strip_prefix("\x1b[?")
        .and_then(|rest| rest.strip_suffix('u'))
        .filter(|digits| digits.bytes().all(|b| b.is_ascii_digit()))
}

/// Headless terminal emulator holding the visible screen of one agent.
pub struct Terminal {
    /// In a `RefCell` so [`Terminal::snapshot`] can take `&self`: while the alt
    /// screen is active it swaps to the primary screen to read it and restores
    /// the terminal before returning. `&mut self` methods use `get_mut` (no
    /// runtime check).
    term: RefCell<Term<Replies>>,
    parser: Processor,
    replies: Replies,
    history: History,
}

impl Terminal {
    /// Creates a blank screen; zero dimensions are clamped to 1.
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_replies(cols, rows, Replies::default())
    }

    fn with_replies(cols: u16, rows: u16, replies: Replies) -> Self {
        let size = Size::new(cols, rows);
        let history = History::new(size.rows);
        Self {
            term: RefCell::new(Term::new(history::config(0), &size, replies.clone())),
            parser: Processor::new(),
            replies,
            history,
        }
    }

    /// Creates a blank client-side screen: like [`Terminal::new`], but kitty
    /// keyboard query replies are kept (flags masked to
    /// [`KITTY_SUPPORTED_FLAGS`]) and OSC 4/10/11/12 colour queries are
    /// answered from `palette`.
    pub fn new_client(cols: u16, rows: u16, palette: Palette) -> Self {
        let replies = Replies {
            client: Some(Arc::new(ClientState {
                palette: Mutex::new(palette),
            })),
            ..Replies::default()
        };
        Self::with_replies(cols, rows, replies)
    }

    /// Replaces the palette that answers colour queries. No effect on a node
    /// terminal ([`Terminal::new`]).
    pub fn set_palette(&mut self, palette: Palette) {
        if let Some(client) = &self.replies.client {
            *client.palette.lock().unwrap_or_else(|e| e.into_inner()) = palette;
        }
    }

    /// Terminal modes a client needs for input encoding and drawing.
    pub fn modes(&self) -> Modes {
        view::modes(*self.term.borrow().mode())
    }

    /// Cursor position and shape (`Hidden` when `?25l`).
    pub fn cursor_view(&self) -> CursorView {
        let term = self.term.borrow();
        let point = term.grid().cursor.point;
        let style = term.cursor_style();
        let shape = if term.mode().contains(TermMode::SHOW_CURSOR) {
            view::cursor_shape(style.shape)
        } else {
            CursorShape::Hidden
        };
        CursorView {
            row: to_u16(point.line.0.max(0) as usize),
            col: to_u16(point.column.0),
            shape,
            blinking: style.blinking,
        }
    }

    /// Replaces `out` with the cells of visible row `row` (wide-char spacers
    /// omitted); empty when `row` is out of range.
    pub fn row_cells(&self, row: u16, out: &mut Vec<Cell>) {
        out.clear();
        let term = self.term.borrow();
        let grid = term.grid();
        if usize::from(row) >= grid.screen_lines() {
            return;
        }
        view::push_row(&grid[Line(i32::from(row))], out);
    }

    /// The OSC 8 hyperlink of visible cell `(row, col)`: its URI and the
    /// columns `start..end` of the run of cells on that row carrying the same
    /// link. `None` off the grid or on a cell without one. Links are kept only
    /// on cells written live, not in snapshots or history (see the crate docs).
    pub fn hyperlink_at(&self, row: u16, col: u16) -> Option<(String, u16, u16)> {
        let term = self.term.borrow();
        let grid = term.grid();
        let (row, col) = (usize::from(row), usize::from(col));
        if row >= grid.screen_lines() || col >= grid.columns() {
            return None;
        }
        let cells = &grid[Line(row as i32)];
        let link = cells[Column(col)].hyperlink()?;
        let same = |c: usize| cells[Column(c)].hyperlink().as_ref() == Some(&link);
        let start = (0..col)
            .rev()
            .take_while(|&c| same(c))
            .last()
            .unwrap_or(col);
        let end = (col + 1..grid.columns())
            .find(|&c| !same(c))
            .unwrap_or(grid.columns());
        Some((link.uri().to_owned(), to_u16(start), to_u16(end)))
    }

    /// Window title set by OSC 0/2, `None` if unset or reset.
    pub fn title(&self) -> Option<String> {
        self.replies
            .title
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Whether a BEL arrived since the last call (clears it).
    pub fn take_bell(&self) -> bool {
        self.replies.bell.swap(false, Ordering::Relaxed)
    }

    /// Feeds raw PTY output through the VT parser.
    ///
    /// Rows that scroll off the primary screen become [`HistoryEvent::Line`]s
    /// and `CSI 3 J` on the primary screen a [`HistoryEvent::Cleared`] (see
    /// [`Terminal::drain_history`]).
    pub fn feed(&mut self, bytes: &[u8]) {
        self.flush_expired_sync();
        let mut recorder = Recorder {
            term: self.term.get_mut(),
            history: &mut self.history,
        };
        self.parser.advance(&mut recorder, bytes);
    }

    /// Takes the history events produced since the last call, in stream order.
    ///
    /// Events accumulate until drained; the owner is expected to drain after
    /// every [`Terminal::feed`].
    pub fn drain_history(&mut self) -> Vec<HistoryEvent> {
        std::mem::take(&mut self.history.events)
    }

    /// Applies a buffered synchronized update now, whether or not its ESU or
    /// timeout has arrived. Call before [`Terminal::snapshot`]: bytes buffered
    /// mid-frame would otherwise be missing from the snapshot.
    pub fn flush_pending_sync(&mut self) {
        if self.parser.sync_timeout().sync_timeout().is_some() {
            self.stop_sync();
        }
    }

    /// Applies a synchronized update whose timeout has expired without an ESU.
    fn flush_expired_sync(&mut self) {
        let expired = self
            .parser
            .sync_timeout()
            .sync_timeout()
            .is_some_and(|deadline| deadline <= Instant::now());
        if expired {
            self.stop_sync();
        }
    }

    /// Applies the buffered synchronized update.
    fn stop_sync(&mut self) {
        let mut recorder = Recorder {
            term: self.term.get_mut(),
            history: &mut self.history,
        };
        self.parser.stop_sync(&mut recorder);
    }

    /// Takes the replies to terminal queries produced since the last call.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.replies.buf.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Resizes the screen (reflowing the primary screen); zero dimensions are clamped to 1.
    ///
    /// Rows a shrink pushes off the top of the primary screen become history.
    ///
    /// Alacritty pushes those rows (and rows a narrower reflow pushes off)
    /// into its scrollback, so the resize runs with the scrollback enabled
    /// and the rows are collected from it right after (`history::settle`).
    /// While the alt screen is active that reads the primary screen behind it
    /// via [`with_primary`]; its saved cursor then becomes its cursor (see
    /// there), which after such a resize is the saved cursor moved with the
    /// content.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let size = Size::new(cols, rows);
        let term = self.term.get_mut();
        if (term.columns(), term.screen_lines()) == (size.cols, size.rows) {
            return;
        }
        term.set_options(history::config(history::RESIZE_SLACK));
        term.resize(size);
        // `Term::resize` resets the scroll region.
        self.history.region = 0..term.screen_lines() as i32;
        let history = &mut self.history;
        if term.mode().contains(TermMode::ALT_SCREEN) {
            with_primary(term, |term| history::settle(term, history));
        } else {
            history::settle(term, history);
        }
    }

    /// Current size as `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        let term = self.term.borrow();
        let grid = term.grid();
        (to_u16(grid.columns()), to_u16(grid.screen_lines()))
    }

    /// ANSI byte sequence that redraws the visible screen into a fresh terminal of
    /// the same size: text, SGR attributes, the active pen and the cursor position.
    ///
    /// Layout: when the alt screen is active, first the primary screen (as
    /// below, but without the saved cursor and the modes), then `\x1b[?1049h`.
    /// Then the active screen:
    /// `\x1b[0m\x1b[H\x1b[2J`, the grid, the saved cursor (positioned with its
    /// pen, then `\x1b7`; omitted when it is a fresh terminal's), the pen, the
    /// kitty keyboard flags, the non-default modes, and `\x1b[{row};{col}H`
    /// (1-based). A pending autowrap (of the cursor or the saved cursor) is
    /// recreated by re-emitting the glyph under it after its CUP.
    ///
    /// Takes `&self`, but with the alt screen active it swaps to the primary
    /// screen and back (see [`with_primary`]); the terminal is unchanged
    /// afterwards.
    pub fn snapshot(&self) -> Vec<u8> {
        let mut term = self.term.borrow_mut();
        let (cols, rows) = (term.grid().columns(), term.grid().screen_lines());
        let alt = term.mode().contains(TermMode::ALT_SCREEN);
        let screens = if alt { 2 } else { 1 };
        let mut out = Vec::with_capacity(screens * rows * (cols + 8) + 128);
        let mut pen = Pen::default();
        let mut kitty_pushed = false;
        if alt {
            with_primary(&mut term, |term| {
                let grid = term.grid();
                put_grid(&mut out, &mut pen, grid);
                // No DECSC: the client's `?1049h` overwrites the primary's
                // saved cursor with its cursor (alacritty; xterm saves it with
                // DECSC), and the node's equals the cursor (see `with_primary`).
                pen.transition(&mut out, Pen::of(&grid.cursor.template));
                kitty_pushed = put_kitty_flags(&mut out, *term.mode(), false);
                put_cursor(&mut out, &mut pen, grid, &grid.cursor);
            });
            out.extend_from_slice(b"\x1b[?1049h");
        }
        let grid = term.grid();
        let mode = *term.mode();
        put_grid(&mut out, &mut pen, grid);
        // Before the modes: DECSC/DECRC positions are absolute, so the saved
        // cursor is placed while origin mode is still off.
        if !is_fresh_cursor(&grid.saved_cursor) {
            put_cursor(&mut out, &mut pen, grid, &grid.saved_cursor);
            out.extend_from_slice(b"\x1b7");
        }
        pen.transition(&mut out, Pen::of(&grid.cursor.template));
        put_kitty_flags(&mut out, mode, kitty_pushed);
        put_modes(&mut out, mode);
        put_cursor(&mut out, &mut pen, grid, &grid.cursor);
        out
    }

    /// Visible screen as one `String` per row, trailing spaces trimmed.
    pub fn visible_text(&self) -> Vec<String> {
        let term = self.term.borrow();
        let grid = term.grid();
        (0..grid.screen_lines())
            .map(|row| row_text(grid, row))
            .collect()
    }

    /// The bottom-most visible row that is not blank, trimmed of
    /// surrounding whitespace; `None` for a blank screen. Reads rows from the
    /// bottom up and stops at the first non-blank one.
    pub fn last_nonblank_line(&self) -> Option<String> {
        let term = self.term.borrow();
        let grid = term.grid();
        (0..grid.screen_lines()).rev().find_map(|row| {
            let text = row_text(grid, row);
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        })
    }

    /// Cursor position as 0-based `(row, col)`.
    pub fn cursor(&self) -> (u16, u16) {
        let point = self.term.borrow().grid().cursor.point;
        (to_u16(point.line.0.max(0) as usize), to_u16(point.column.0))
    }

    /// Rendering attributes of one cell, for roundtrip comparisons in tests.
    #[cfg(test)]
    fn cell_attrs(&self, row: u16, col: u16) -> Pen {
        Pen::of(&self.term.borrow().grid()[Line(i32::from(row))][Column(usize::from(col))])
    }

    /// Terminal modes, for roundtrip comparisons in tests.
    #[cfg(test)]
    fn mode(&self) -> TermMode {
        *self.term.borrow().mode()
    }

    /// Saved cursor (DECSC) as `(row, col, pending wrap, pen)`, for roundtrip
    /// comparisons in tests.
    #[cfg(test)]
    fn saved_cursor(&self) -> (i32, usize, bool, Pen) {
        let term = self.term.borrow();
        let saved = &term.grid().saved_cursor;
        (
            saved.point.line.0,
            saved.point.column.0,
            saved.input_needs_wrap,
            Pen::of(&saved.template),
        )
    }

    /// Pending wrap and pen of the cursor, for roundtrip comparisons in tests.
    #[cfg(test)]
    fn cursor_state(&self) -> (bool, Pen) {
        let term = self.term.borrow();
        let cursor = &term.grid().cursor;
        (cursor.input_needs_wrap, Pen::of(&cursor.template))
    }
}

fn to_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// Runs `f` on the primary screen while the alt screen is active, then puts the
/// terminal back as it was. Relies on what alacritty 0.26's [`Term::swap_alt`]
/// does (pinned by the test `swap_alt_behaves_as_snapshot_expects`):
/// - alt -> primary swaps the grids and the kitty keyboard stacks, recomputes
///   the kitty flags from the primary's stack and flips `ALT_SCREEN`; nothing
///   is lost.
/// - primary -> alt also sets the alt grid's cursor to the primary cursor,
///   overwrites the primary's saved cursor with its cursor, clears the alt
///   grid, and recomputes the kitty flags from the alt stack's top (losing
///   flags set with `CSI = … u`, which leaves the stack alone).
///
/// So the alt grid (with its cursor and saved cursor) is cloned and put back,
/// and the alt kitty flags are re-applied. The primary's saved cursor cannot
/// be kept apart from its cursor: entering the alt screen (`?1049h`, the same
/// `swap_alt`) already set it to the cursor, and the two differ only after a
/// resize while the alt screen is active (the inactive primary is reflowed:
/// the cursor moves with the text, the saved cursor is only clamped). Then the
/// saved cursor becomes the cursor; the cursor, which `?1049l` shows, is kept.
/// Selection and damage are not kept (unused here).
fn with_primary<R>(term: &mut Term<Replies>, f: impl FnOnce(&mut Term<Replies>) -> R) -> R {
    debug_assert!(term.mode().contains(TermMode::ALT_SCREEN));
    let alt_grid = term.grid().clone();
    let alt_kitty = *term.mode() & TermMode::KITTY_KEYBOARD_PROTOCOL;
    term.swap_alt();
    let result = f(term);
    term.swap_alt();
    *term.grid_mut() = alt_grid;
    if *term.mode() & TermMode::KITTY_KEYBOARD_PROTOCOL != alt_kitty {
        Handler::set_keyboard_mode(
            term,
            KeyboardModes::from_bits_truncate(kitty_flags(alt_kitty)),
            KeyboardModesApplyBehavior::Replace,
        );
    }
    result
}

/// Clears the screen (`\x1b[0m\x1b[H\x1b[2J`, so `pen` becomes the default)
/// and draws `grid`'s non-blank cells.
/// A visible row's text, trailing spaces trimmed.
fn row_text(grid: &Grid<GridCell>, row: usize) -> String {
    let line = &grid[Line(row as i32)];
    let mut text = String::with_capacity(grid.columns());
    for col in 0..grid.columns() {
        let cell = &line[Column(col)];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        text.push(glyph(cell));
        text.extend(cell.zerowidth().unwrap_or_default());
    }
    text.truncate(text.trim_end_matches(' ').len());
    text
}

fn put_grid(out: &mut Vec<u8>, pen: &mut Pen, grid: &Grid<GridCell>) {
    out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
    *pen = Pen::default();
    for row in 0..grid.screen_lines() {
        let line = &grid[Line(row as i32)];
        let Some(end) = last_non_blank(line) else {
            continue;
        };
        if row > 0 {
            cup(out, row, 0);
        }
        put_cells(out, pen, line, end);
    }
}

/// Moves to `cursor` and sets its pen. A pending autowrap is recreated by
/// re-emitting the glyph under the cursor (the whole wide character if the
/// cursor is on its spacer) with that cell's pen.
fn put_cursor(out: &mut Vec<u8>, pen: &mut Pen, grid: &Grid<GridCell>, cursor: &Cursor<GridCell>) {
    let row = (cursor.point.line.0.max(0) as usize).min(grid.screen_lines() - 1);
    let col = cursor.point.column.0.min(grid.columns() - 1);
    cup(out, row, col);
    if cursor.input_needs_wrap {
        let line = &grid[Line(row as i32)];
        let start = if line[Column(col)].flags.contains(Flags::WIDE_CHAR_SPACER) {
            col.saturating_sub(1)
        } else {
            col
        };
        let cell = &line[Column(start)];
        if start != col {
            cup(out, row, start);
        }
        pen.transition(out, Pen::of(cell));
        put_glyph(out, cell);
    }
    pen.transition(out, Pen::of(&cursor.template));
}

/// Whether DECRC in a fresh terminal already restores `cursor`, as far as the
/// snapshot carries it (position, pending wrap and pen; not charsets).
fn is_fresh_cursor(cursor: &Cursor<GridCell>) -> bool {
    cursor.point.line.0 == 0
        && cursor.point.column.0 == 0
        && !cursor.input_needs_wrap
        && Pen::of(&cursor.template) == Pen::default()
}

/// Kitty keyboard flags: the `TermMode` bit and its protocol value.
const KITTY_FLAGS: [(TermMode, KeyboardModes); 5] = [
    (
        TermMode::DISAMBIGUATE_ESC_CODES,
        KeyboardModes::DISAMBIGUATE_ESC_CODES,
    ),
    (
        TermMode::REPORT_EVENT_TYPES,
        KeyboardModes::REPORT_EVENT_TYPES,
    ),
    (
        TermMode::REPORT_ALTERNATE_KEYS,
        KeyboardModes::REPORT_ALTERNATE_KEYS,
    ),
    (
        TermMode::REPORT_ALL_KEYS_AS_ESC,
        KeyboardModes::REPORT_ALL_KEYS_AS_ESC,
    ),
    (
        TermMode::REPORT_ASSOCIATED_TEXT,
        KeyboardModes::REPORT_ASSOCIATED_TEXT,
    ),
];

/// The kitty keyboard flags (protocol bit values) set in `mode`.
fn kitty_flags(mode: TermMode) -> u8 {
    KITTY_FLAGS
        .iter()
        .filter(|(flag, _)| mode.contains(*flag))
        .fold(0, |acc, (_, bits)| acc | bits.bits())
}

/// Emits `mode`'s kitty keyboard flags; returns whether it pushed a stack level.
///
/// Pushes one level (`CSI > flags u`) when the flags are non-zero, so the one
/// `CSI < u` that `fleet attach` sends on exit pops it. If an earlier screen of
/// this snapshot already `pushed` (the primary behind an active alt screen), it
/// sets the flags instead (`CSI = flags ; 1 u`, also when zero): a fresh
/// terminal enters the alt screen with an empty stack of its own, while a
/// client that is already on its alt screen (`fleet attach`) ignores `?1049h`
/// and would otherwise keep the primary's flags or end up two levels deep.
fn put_kitty_flags(out: &mut Vec<u8>, mode: TermMode, pushed: bool) -> bool {
    let flags = kitty_flags(mode);
    if pushed {
        let _ = write!(out, "\x1b[={flags};1u");
        false
    } else if flags != 0 {
        let _ = write!(out, "\x1b[>{flags}u");
        true
    } else {
        false
    }
}

/// Modes restored by the snapshot: `(mode, set sequence, reset sequence)`. A
/// sequence is emitted when the mode differs from a fresh terminal's default.
/// ALT_SCREEN is handled separately (it must precede drawing). Origin mode homes
/// the cursor, so these go before the final CUP.
const MODES: [(TermMode, &[u8], &[u8]); 15] = [
    (TermMode::SHOW_CURSOR, b"\x1b[?25h", b"\x1b[?25l"),
    (TermMode::APP_CURSOR, b"\x1b[?1h", b"\x1b[?1l"),
    (TermMode::APP_KEYPAD, b"\x1b=", b"\x1b>"),
    (TermMode::MOUSE_REPORT_CLICK, b"\x1b[?1000h", b"\x1b[?1000l"),
    (TermMode::MOUSE_DRAG, b"\x1b[?1002h", b"\x1b[?1002l"),
    (TermMode::MOUSE_MOTION, b"\x1b[?1003h", b"\x1b[?1003l"),
    (TermMode::FOCUS_IN_OUT, b"\x1b[?1004h", b"\x1b[?1004l"),
    (TermMode::UTF8_MOUSE, b"\x1b[?1005h", b"\x1b[?1005l"),
    (TermMode::SGR_MOUSE, b"\x1b[?1006h", b"\x1b[?1006l"),
    (TermMode::ALTERNATE_SCROLL, b"\x1b[?1007h", b"\x1b[?1007l"),
    (TermMode::BRACKETED_PASTE, b"\x1b[?2004h", b"\x1b[?2004l"),
    (TermMode::LINE_WRAP, b"\x1b[?7h", b"\x1b[?7l"),
    (TermMode::ORIGIN, b"\x1b[?6h", b"\x1b[?6l"),
    (TermMode::INSERT, b"\x1b[4h", b"\x1b[4l"),
    (TermMode::LINE_FEED_NEW_LINE, b"\x1b[20h", b"\x1b[20l"),
];

fn put_modes(out: &mut Vec<u8>, mode: TermMode) {
    let default = TermMode::default();
    for (flag, set, reset) in MODES {
        match (mode.contains(flag), default.contains(flag)) {
            (true, false) => out.extend_from_slice(set),
            (false, true) => out.extend_from_slice(reset),
            _ => {}
        }
    }
}

/// 0-based CUP.
fn cup(out: &mut Vec<u8>, row: usize, col: usize) {
    let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn palette() -> Palette {
        let mut ansi = [(0, 0, 0); 16];
        for (i, c) in ansi.iter_mut().enumerate() {
            *c = (i as u8, 0, 0);
        }
        Palette {
            fg: (0xcd, 0xd6, 0xf4),
            bg: (0x1e, 0x1e, 0x2e),
            cursor: (0xf5, 0xe0, 0xdc),
            ansi,
        }
    }

    const QUERIES: &[u8] = b"\x1b[>1u\x1b[?u\x1b]11;?\x1b\\\x1b[6n";

    #[test]
    fn hyperlink_at_gives_the_osc8_uri_and_its_columns() {
        let mut t = Terminal::new_client(40, 4, palette());
        t.feed(b"see \x1b]8;;https://a.io/pr/1\x1b\\the PR\x1b]8;;\x1b\\ now\r\n");
        t.feed(b"\x1b]8;id=x;https://b.io\x07\xe7\x95\x8cb\x1b]8;;\x07");
        assert_eq!(t.hyperlink_at(0, 3), None);
        let want = Some(("https://a.io/pr/1".to_owned(), 4, 10));
        assert_eq!(t.hyperlink_at(0, 4), want);
        assert_eq!(t.hyperlink_at(0, 9), want);
        assert_eq!(t.hyperlink_at(0, 10), None);
        // A wide cell and its spacer.
        let want = Some(("https://b.io".to_owned(), 0, 3));
        assert_eq!(t.hyperlink_at(1, 1), want);
        assert_eq!(t.hyperlink_at(1, 2), want);
        assert_eq!(t.hyperlink_at(9, 0), None);
        assert_eq!(t.hyperlink_at(0, 99), None);
    }

    #[test]
    fn client_terminal_answers_kitty_and_color_queries() {
        let mut t = Terminal::new_client(80, 24, palette());
        t.feed(QUERIES);
        assert_eq!(
            t.take_replies(),
            b"\x1b[?1u\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\\x1b[1;1R"
        );
    }

    #[test]
    fn client_color_reply_mirrors_bel_terminator_and_palette_slots() {
        let mut t = Terminal::new_client(80, 24, palette());
        t.feed(b"\x1b]10;?\x07\x1b]4;1;?\x07\x1b]4;16;?\x07");
        assert_eq!(
            t.take_replies(),
            b"\x1b]10;rgb:cdcd/d6d6/f4f4\x07\x1b]4;1;rgb:0101/0000/0000\x07\x1b]4;16;rgb:0000/0000/0000\x07"
        );
        let mut p = palette();
        p.bg = (1, 2, 3);
        t.set_palette(p);
        t.feed(b"\x1b]11;?\x07");
        assert_eq!(t.take_replies(), b"\x1b]11;rgb:0101/0202/0303\x07");
    }

    #[test]
    fn client_masks_unsupported_kitty_flags() {
        let mut t = Terminal::new_client(80, 24, palette());
        t.feed(b"\x1b[>31u\x1b[?u");
        assert_eq!(t.take_replies(), b"\x1b[?1u");
    }

    #[test]
    fn node_terminal_still_drops_kitty_and_color_replies() {
        let mut t = Terminal::new(80, 24);
        t.feed(QUERIES);
        assert_eq!(t.take_replies(), b"\x1b[1;1R");
    }

    #[test]
    fn row_cells_reports_style_and_wide_chars() {
        let mut t = Terminal::new(80, 24);
        t.feed("\x1b[1;31mA\x1b[0m\u{754c}b".as_bytes());
        let mut cells = Vec::new();
        t.row_cells(0, &mut cells);
        assert_eq!(cells.len(), 79);
        let bold_red = CellStyle {
            fg: Color::Named(1),
            bold: true,
            ..CellStyle::default()
        };
        assert_eq!(
            (cells[0].ch, cells[0].style, cells[0].wide),
            ('A', bold_red, false)
        );
        assert_eq!(
            (cells[1].ch, cells[1].style, cells[1].wide),
            ('\u{754c}', CellStyle::default(), true)
        );
        assert_eq!((cells[2].ch, cells[2].wide), ('b', false));
        assert!(cells[3..].iter().all(|c| *c == Cell::default()));
        assert_eq!(cells_text(&cells[..3]), "A\u{754c}b");
        t.row_cells(99, &mut cells);
        assert!(cells.is_empty());
    }

    #[test]
    fn modes_track_bracketed_paste_mouse_and_kitty() {
        let mut t = Terminal::new(80, 24);
        assert!(t.modes().show_cursor);
        t.feed(b"\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[>1u");
        let m = t.modes();
        assert!(m.bracketed_paste && m.mouse_drag && m.sgr_mouse);
        assert!(!m.mouse_click && !m.alt_screen);
        assert_eq!(m.kitty_flags, 1);
    }

    #[test]
    fn cursor_view_reports_position_and_shape() {
        let mut t = Terminal::new(80, 24);
        t.feed(b"\x1b[3;5H\x1b[6 q");
        assert_eq!(
            t.cursor_view(),
            CursorView {
                row: 2,
                col: 4,
                shape: CursorShape::Beam,
                blinking: false,
            }
        );
        t.feed(b"\x1b[?25l");
        assert_eq!(t.cursor_view().shape, CursorShape::Hidden);
    }

    #[test]
    fn cursor_view_keeps_the_decscusr_blink_bit() {
        let mut t = Terminal::new(80, 24);
        assert!(!t.cursor_view().blinking, "steady until asked");
        t.feed(b"\x1b[5 q");
        let c = t.cursor_view();
        assert_eq!((c.shape, c.blinking), (CursorShape::Beam, true));
        t.feed(b"\x1b[2 q");
        let c = t.cursor_view();
        assert_eq!((c.shape, c.blinking), (CursorShape::Block, false));
        t.feed(b"\x1b[3 q");
        let c = t.cursor_view();
        assert_eq!((c.shape, c.blinking), (CursorShape::Underline, true));
        // `?12h` (att610) turns blinking on for the current shape.
        t.feed(b"\x1b[4 q\x1b[?12h");
        assert!(t.cursor_view().blinking);
    }

    /// The underline kind and colour of row 0's first cell after `sgr`.
    fn underline_of(sgr: &str) -> (Underline, Color) {
        let mut t = Terminal::new(20, 3);
        t.feed(format!("\x1b[{sgr}mx").as_bytes());
        let mut row = Vec::new();
        t.row_cells(0, &mut row);
        (row[0].style.underline, row[0].style.underline_color)
    }

    #[test]
    fn row_cells_carry_the_underline_kind_and_colour() {
        assert_eq!(underline_of("0"), (Underline::None, Color::Default));
        assert_eq!(underline_of("4"), (Underline::Single, Color::Default));
        assert_eq!(underline_of("4:2"), (Underline::Double, Color::Default));
        assert_eq!(underline_of("4:3"), (Underline::Curly, Color::Default));
        assert_eq!(underline_of("4:4"), (Underline::Dotted, Color::Default));
        assert_eq!(underline_of("4:5"), (Underline::Dashed, Color::Default));
        assert_eq!(
            underline_of("4:3;58:2::255:0:0"),
            (Underline::Curly, Color::Rgb(255, 0, 0))
        );
        assert_eq!(
            underline_of("4;58;5;196"),
            (Underline::Single, Color::Indexed(196))
        );
        assert_eq!(
            underline_of("4;58;5;196;59"),
            (Underline::Single, Color::Default)
        );
    }

    #[test]
    fn history_lines_keep_the_underline_kind_and_colour() {
        let mut t = Terminal::new(30, 2);
        let mut rendered = Vec::new();
        let mut parsed = Vec::new();
        let mut row = Vec::new();
        for line in [
            "\x1b[4:3;58:2::255:0:0merr\x1b[0m ok\r\n",
            "\x1b[4:2ma\x1b[4:4mb\x1b[4:5;58;5;33mc\x1b[59md\x1b[24me\r\n",
            "\x1b[4;58;5;1mf\x1b[0m\r\n",
            "x\r\n",
        ] {
            t.row_cells(0, &mut row);
            t.feed(line.as_bytes());
            for ev in t.drain_history() {
                if let HistoryEvent::Line(l) = ev {
                    rendered.push(row.clone());
                    let mut cells = Vec::new();
                    parse_history_line(&l, &mut cells);
                    parsed.push(cells);
                }
            }
        }
        assert!(parsed.len() >= 2);
        assert!(
            parsed
                .iter()
                .flatten()
                .any(|c| c.style.underline == Underline::Curly
                    && c.style.underline_color == Color::Rgb(255, 0, 0))
        );
        for (p, r) in parsed.iter().zip(&rendered) {
            assert_eq!(p[..], r[..p.len()]);
        }
    }

    #[test]
    fn parse_history_line_matches_rendered_rows() {
        let mut t = Terminal::new(20, 3);
        let mut rendered: Vec<Vec<Cell>> = Vec::new();
        let mut parsed: Vec<Vec<Cell>> = Vec::new();
        let mut row = Vec::new();
        for i in 0..8 {
            let line = format!(
                "\x1b[1;3{}mred{i}\x1b[0m \x1b[48;5;{}m\u{754c}\x1b[38;2;1;2;3;4mx\x1b[0m e\u{301}\r\n",
                1 + i % 7,
                20 + i
            );
            // Row 0 as drawn before this feed scrolls it off.
            t.row_cells(0, &mut row);
            t.feed(line.as_bytes());
            for ev in t.drain_history() {
                if let HistoryEvent::Line(l) = ev {
                    rendered.push(row.clone());
                    let mut cells = Vec::new();
                    parse_history_line(&l, &mut cells);
                    parsed.push(cells);
                }
            }
        }
        assert!(!parsed.is_empty());
        assert_eq!(parsed.len(), rendered.len());
        for (p, r) in parsed.iter().zip(&rendered) {
            assert!(!p.is_empty());
            assert_eq!(p[..], r[..p.len()]);
            assert!(r[p.len()..].iter().all(|c| *c == Cell::default()));
        }
    }

    #[test]
    fn parse_history_line_ignores_non_sgr_escapes() {
        let mut cells = vec![Cell::default()];
        parse_history_line(b"a\x1b[2Kb\x1b]0;t\x07c", &mut cells);
        assert_eq!(cells_text(&cells), "abc");
    }

    #[test]
    fn take_bell_reports_a_bel_once() {
        let mut t = Terminal::new(80, 24);
        assert!(!t.take_bell());
        t.feed(b"done\x07");
        assert!(t.take_bell());
        assert!(!t.take_bell());
        // BEL ending an OSC string is a terminator, not a bell.
        t.feed(b"\x1b]2;title\x07");
        assert!(!t.take_bell());
    }

    #[test]
    fn title_follows_osc_2() {
        let mut t = Terminal::new(80, 24);
        assert_eq!(t.title(), None);
        t.feed(b"\x1b]2;hello\x07");
        assert_eq!(t.title().as_deref(), Some("hello"));
    }

    #[test]
    fn last_nonblank_line_is_the_lowest_non_blank_row_trimmed() {
        let mut t = Terminal::new(20, 5);
        assert_eq!(t.last_nonblank_line(), None);
        t.feed(b"first\r\n   second \x1b[1mbold\x1b[0m  \r\n\r\n");
        assert_eq!(t.last_nonblank_line().as_deref(), Some("second bold"));
        t.feed(b"\x1b[5;1H  \xe4\xb8\xad  ");
        assert_eq!(t.last_nonblank_line().as_deref(), Some("\u{4e2d}"));
    }

    /// Deterministic xorshift64* PRNG for fixture generation.
    pub(crate) struct Rng(pub(crate) u64);

    impl Rng {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        pub(crate) fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Agent-like ANSI output of at least `len` bytes: printable text, SGR
    /// colors (16, 256, truecolor), bold/italic/underline/inverse, CRLF,
    /// `\x1b[2K\r` line rewrites and cursor moves.
    pub(crate) fn fake_output(seed: u64, len: usize) -> Vec<u8> {
        const WORDS: &[&str] = &[
            "compiling",
            "fleet",
            "agent",
            "ok",
            "error:",
            "warning",
            "src/lib.rs",
            "->",
            "{",
            "}",
            "fn",
            "main()",
            "test",
            "passed",
            "1234",
            "λ",
            "✓",
            "…",
        ];
        let mut rng = Rng(seed | 1);
        let mut out = Vec::with_capacity(len + 256);
        while out.len() < len {
            match rng.below(20) {
                0..=8 => {
                    out.extend_from_slice(WORDS[rng.below(WORDS.len() as u64) as usize].as_bytes());
                    out.push(b' ');
                }
                9 => out.extend_from_slice(format!("\x1b[3{}m", rng.below(8)).as_bytes()),
                10 => out.extend_from_slice(format!("\x1b[4{}m", rng.below(8)).as_bytes()),
                11 => out.extend_from_slice(format!("\x1b[38;5;{}m", rng.below(256)).as_bytes()),
                12 => out.extend_from_slice(
                    format!(
                        "\x1b[48;2;{};{};{}m",
                        rng.below(256),
                        rng.below(256),
                        rng.below(256)
                    )
                    .as_bytes(),
                ),
                13 => {
                    let attr = [1, 3, 4, 7, 22, 23, 24, 27][rng.below(8) as usize];
                    out.extend_from_slice(format!("\x1b[{attr}m").as_bytes());
                }
                14 => out.extend_from_slice(b"\x1b[0m"),
                15 | 16 => out.extend_from_slice(b"\r\n"),
                17 => out.extend_from_slice(b"\x1b[2K\r"),
                18 => out.extend_from_slice(
                    format!("\x1b[{};{}H", 1 + rng.below(40), 1 + rng.below(120)).as_bytes(),
                ),
                _ => {
                    let dir = b"ABCD"[rng.below(4) as usize];
                    out.extend_from_slice(format!("\x1b[{}", 1 + rng.below(5)).as_bytes());
                    out.push(dir);
                }
            }
        }
        out
    }

    fn roundtrip(a: &Terminal) -> Terminal {
        let (cols, rows) = a.size();
        let mut b = Terminal::new(cols, rows);
        b.feed(&a.snapshot());
        b
    }

    fn assert_same_screen(a: &Terminal, b: &Terminal) {
        assert_eq!(a.visible_text(), b.visible_text());
        assert_eq!(a.cursor(), b.cursor());
        assert_eq!(a.cursor_state(), b.cursor_state(), "cursor wrap/pen");
        assert_eq!(a.saved_cursor(), b.saved_cursor(), "saved cursor");
        assert_eq!(a.mode(), b.mode());
        let (cols, rows) = a.size();
        for row in 0..rows {
            for col in 0..cols {
                assert_eq!(
                    a.cell_attrs(row, col),
                    b.cell_attrs(row, col),
                    "cell attrs differ at row {row} col {col}"
                );
            }
        }
    }

    #[test]
    fn plain_text_lands_on_grid() {
        let mut t = Terminal::new(20, 4);
        t.feed(b"hello\r\nworld");
        let text = t.visible_text();
        assert_eq!(text.len(), 4);
        assert_eq!(text[0], "hello");
        assert_eq!(text[1], "world");
        assert_eq!(text[2], "");
        assert_eq!(t.cursor(), (1, 5));
    }

    #[test]
    fn cursor_movement_and_overwrite() {
        let mut t = Terminal::new(20, 4);
        t.feed(b"abc\x1b[2Dx");
        assert_eq!(t.visible_text()[0], "axc");
        assert_eq!(t.cursor(), (0, 2));
    }

    #[test]
    fn snapshot_has_documented_framing() {
        let mut t = Terminal::new(20, 4);
        t.feed(b"hi\r\nthere\x1b[1;2H");
        let snap = t.snapshot();
        assert!(snap.starts_with(b"\x1b[0m\x1b[H\x1b[2J"));
        assert!(
            snap.ends_with(b"\x1b[1;2H"),
            "{:?}",
            String::from_utf8_lossy(&snap)
        );
    }

    #[test]
    fn snapshot_roundtrip_reproduces_screen() {
        let mut a = Terminal::new(120, 40);
        a.feed(&fake_output(0x5eed, 256 * 1024));
        let b = roundtrip(&a);
        assert_same_screen(&a, &b);
        assert!(a.visible_text().iter().any(|l| !l.is_empty()));
    }

    #[test]
    fn snapshot_roundtrip_many_seeds_and_sizes() {
        for (seed, cols, rows) in [(1, 80, 24), (2, 120, 40), (3, 40, 10), (4, 200, 60)] {
            let mut a = Terminal::new(cols, rows);
            a.feed(&fake_output(seed, 64 * 1024));
            assert_same_screen(&a, &roundtrip(&a));
        }
    }

    #[test]
    fn snapshot_preserves_colors() {
        let mut a = Terminal::new(40, 5);
        a.feed(b"plain \x1b[31mred\x1b[0m \x1b[1;3;4;7;38;5;200;48;2;1;2;3mfancy\x1b[0m");
        let red = a.cell_attrs(0, 6);
        assert_ne!(red, a.cell_attrs(0, 0), "red cell should differ from plain");
        let b = roundtrip(&a);
        assert_eq!(b.cell_attrs(0, 6), red);
        assert_eq!(b.cell_attrs(0, 0), a.cell_attrs(0, 0));
        assert_eq!(b.cell_attrs(0, 10), a.cell_attrs(0, 10));
        assert_same_screen(&a, &b);
    }

    #[test]
    fn snapshot_preserves_background_only_cells_and_pen() {
        let mut a = Terminal::new(20, 3);
        // Trailing blank cells with a non-default background, then a pen left
        // active (green) for the next live output.
        a.feed(b"x\x1b[44m   \x1b[0m\r\n\x1b[32m");
        let mut b = roundtrip(&a);
        assert_same_screen(&a, &b);
        a.feed(b"live");
        b.feed(b"live");
        assert_same_screen(&a, &b);
    }

    #[test]
    fn snapshot_roundtrips_wide_and_combining_chars() {
        let mut a = Terminal::new(10, 3);
        a.feed("中x\r\ne\u{301}!".as_bytes());
        assert_eq!(a.visible_text()[0], "中x");
        assert_eq!(a.visible_text()[1], "e\u{301}!");
        let b = roundtrip(&a);
        assert_same_screen(&a, &b);
    }

    #[test]
    fn snapshot_full_last_row_does_not_scroll() {
        let mut a = Terminal::new(5, 2);
        a.feed(b"abcde\r\nfghij");
        let b = roundtrip(&a);
        assert_same_screen(&a, &b);
        assert_eq!(b.visible_text(), vec!["abcde", "fghij"]);
    }

    #[test]
    fn snapshot_preserves_pending_wrap() {
        for input in ["abcde", "abc中"] {
            let mut a = Terminal::new(5, 3);
            a.feed(input.as_bytes());
            let mut b = roundtrip(&a);
            assert_same_screen(&a, &b);
            a.feed(b"f");
            b.feed(b"f");
            assert_same_screen(&a, &b);
            assert_eq!(b.visible_text()[1], "f", "input {input:?}");
        }
    }

    /// Feeds `setup` into A, roundtrips the snapshot into B and checks B's modes.
    fn roundtrip_modes(setup: &str, expect_set: TermMode, expect_unset: TermMode) -> Terminal {
        let mut a = Terminal::new(20, 5);
        a.feed(setup.as_bytes());
        assert!(
            a.mode().contains(expect_set),
            "setup {setup:?} did not set {expect_set:?}"
        );
        let b = roundtrip(&a);
        assert!(
            b.mode().contains(expect_set),
            "{setup:?}: B lacks {expect_set:?}: {:?}",
            b.mode()
        );
        assert!(
            !b.mode().intersects(expect_unset),
            "{setup:?}: B has {expect_unset:?}"
        );
        assert_same_screen(&a, &b);
        b
    }

    #[test]
    fn snapshot_preserves_alt_screen() {
        let setup = "primary\x1b[?1049h\x1b[2;3Halt \x1b[31mred";
        let mut a = Terminal::new(20, 5);
        a.feed(setup.as_bytes());
        let mut b = roundtrip_modes(setup, TermMode::ALT_SCREEN, TermMode::empty());
        assert_eq!(b.visible_text(), a.visible_text());
        assert_eq!(b.visible_text()[1], "  alt red");
        let snap = a.snapshot();
        assert!(snap.starts_with(b"\x1b[0m\x1b[H\x1b[2Jprimary"));
        let alt = b"\x1b[?1049h\x1b[0m\x1b[H\x1b[2J";
        assert_eq!(
            snap.windows(alt.len()).filter(|w| w == alt).count(),
            1,
            "{:?}",
            String::from_utf8_lossy(&snap)
        );
        // Leaving the alt screen shows the primary screen on the client too.
        a.feed(b"\x1b[?1049l");
        b.feed(b"\x1b[?1049l");
        assert!(!b.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(b.visible_text()[0], "primary");
        assert_same_screen(&a, &b);
    }

    /// Feeds `setup` into A, roundtrips into B, checks the screens match, then
    /// feeds each of `after` into both and checks again after each.
    fn roundtrip_then(cols: u16, rows: u16, setup: &[u8], after: &[&[u8]]) -> (Terminal, Terminal) {
        let mut a = Terminal::new(cols, rows);
        a.feed(setup);
        let mut b = roundtrip(&a);
        assert_same_screen(&a, &b);
        for (i, bytes) in after.iter().enumerate() {
            a.feed(bytes);
            b.feed(bytes);
            assert!(
                a.visible_text() == b.visible_text() && a.cursor() == b.cursor(),
                "after step {i} ({:?}): A {:?} at {:?}, B {:?} at {:?}",
                String::from_utf8_lossy(bytes),
                a.visible_text(),
                a.cursor(),
                b.visible_text(),
                b.cursor()
            );
            assert_same_screen(&a, &b);
        }
        (a, b)
    }

    #[test]
    fn snapshot_preserves_saved_cursor() {
        // DECSC with a pen, then elsewhere with another pen; DECRC on both.
        let (_, b) = roundtrip_then(
            20,
            5,
            b"hello\x1b[2;3H\x1b[1;31;48;5;200m\x1b7\x1b[0m\x1b[4;1Hworld\x1b[32m",
            &[b"\x1b8X", b"Y\x1b[1;1H\x1b8Z"],
        );
        assert_eq!(b.visible_text()[1], "  ZY");
        // CSI s / CSI u.
        roundtrip_then(20, 5, b"ab\x1b[3;7H\x1b[s\x1b[H", &[b"\x1b[ux"]);
        // A saved cursor at the home position with only a pen differs from a
        // fresh terminal's.
        roundtrip_then(20, 5, b"\x1b[7m\x1b7\x1b[0m\x1b[3;3H", &[b"\x1b8inv"]);
        // A saved cursor at the fresh default needs nothing.
        let mut a = Terminal::new(20, 5);
        a.feed(b"\x1b7abc");
        assert!(!a.snapshot().windows(2).any(|w| w == b"\x1b7"));
    }

    #[test]
    fn snapshot_preserves_saved_cursor_pending_wrap() {
        for setup in ["abcde", "abc中", "ab\x1b[1;5H\x1b[4m中\x1b[0m"] {
            let setup = format!("{setup}\x1b7\x1b[3;1H");
            let (a, _) = roundtrip_then(5, 4, setup.as_bytes(), &[b"\x1b8f", b"g"]);
            assert!(
                a.saved_cursor().2 || setup.contains("中\x1b[0m"),
                "{setup:?}"
            );
        }
        // Saved with a pending wrap while the cursor sits elsewhere.
        roundtrip_then(5, 4, b"abcde\x1b7\x1b[3;2Hq", &[b"\x1b8f"]);
    }

    #[test]
    fn snapshot_saved_cursor_with_origin_mode() {
        // DECSC/DECRC positions are absolute; origin mode is applied after the
        // saved cursor is placed.
        roundtrip_then(
            20,
            6,
            b"ab\x1b[3;4H\x1b7\x1b[?6h\x1b[2;2H",
            &[b"\x1b8Z", b"\x1b[1;1HW"],
        );
        roundtrip_then(20, 6, b"\x1b[?6h\x1b[4;5H\x1b7\x1b[H", &[b"\x1b8Z"]);
    }

    #[test]
    fn snapshot_preserves_kitty_keyboard_flags() {
        for (setup, flags) in [
            ("\x1b[>1u", TermMode::DISAMBIGUATE_ESC_CODES),
            ("\x1b[>31u", TermMode::KITTY_KEYBOARD_PROTOCOL),
            (
                "\x1b[>1u\x1b[>5u",
                TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALTERNATE_KEYS,
            ),
            // Set without a push, and changed with union/difference.
            (
                "\x1b[=3;1u",
                TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_EVENT_TYPES,
            ),
            (
                "\x1b[>1u\x1b[=8;2u\x1b[=1;3u",
                TermMode::REPORT_ALL_KEYS_AS_ESC,
            ),
        ] {
            let b = roundtrip_modes(setup, flags, TermMode::KITTY_KEYBOARD_PROTOCOL - flags);
            assert_eq!(
                b.mode() & TermMode::KITTY_KEYBOARD_PROTOCOL,
                flags,
                "{setup:?}"
            );
        }
        let mut a = Terminal::new(20, 5);
        a.feed(b"x\x1b[>9u");
        let snap = a.snapshot();
        let push = b"\x1b[>9u";
        assert!(snap.windows(push.len()).any(|w| w == push));
        // Popping the one level the snapshot pushed clears the flags on both.
        roundtrip_then(20, 5, b"\x1b[>1u", &[b"\x1b[<u"]);
        // No flags: nothing emitted.
        let mut a = Terminal::new(20, 5);
        a.feed(b"\x1b[>1u\x1b[<u");
        assert!(!a.snapshot().ends_with(b"u"));
        assert!(!a.snapshot().windows(3).any(|w| w == b"\x1b[>"));
    }

    #[test]
    fn kitty_keyboard_query_is_not_answered() {
        let mut t = Terminal::new(20, 5);
        t.feed(b"\x1b[>1u\x1b[?u\x1b[6n");
        assert_eq!(t.take_replies(), b"\x1b[1;1R");
    }

    /// Everything a test can observe about `t`, primary and alt screen alike,
    /// for checking that a snapshot leaves the terminal untouched.
    fn observe(t: &Terminal) -> Vec<String> {
        let (cols, rows) = t.size();
        let mut seen = t.visible_text();
        seen.push(format!(
            "{:?} {:?} {:?} {:?}",
            t.cursor(),
            t.cursor_state(),
            t.saved_cursor(),
            t.mode()
        ));
        for row in 0..rows {
            for col in 0..cols {
                seen.push(format!("{:?}", t.cell_attrs(row, col)));
            }
        }
        seen
    }

    /// A primary screen behind an alt screen, with a pen, saved cursors, kitty
    /// flags and modes on both.
    const PRIMARY_AND_ALT: &[u8] = concat!(
        "prompt$ \x1b[1;34mls\x1b[0m\r\nfile1  \x1b[32mfile2\x1b[0m\r\n",
        "\x1b[2;3H\x1b[4m\x1b7\x1b[0m\x1b[3;1H\x1b[>1u\x1b[35mrun ",
        "\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[>11u\x1b[=3;1u",
        "\x1b[44m\x1b[2J\x1b[1;1Htitle\x1b[0m\x1b[3;5H\x1b[7m\x1b7\x1b[0mbody\x1b[31m",
    )
    .as_bytes();

    #[test]
    fn snapshot_carries_primary_screen_behind_alt_screen() {
        let (a, b) = roundtrip_then(
            30,
            6,
            PRIMARY_AND_ALT,
            &[
                b"\x1b8more",
                b"\x1b[?1049l",
                b"done\x1b8!",
                b"\x1b[<u",
                // Re-entering the alt screen finds A's old alt stack (`>11`, not
                // carried) on the node; clear it to compare the rest.
                b"\x1b[?1049h\x1b[<9uagain",
                b"\x1b[?1049l",
            ],
        );
        assert_eq!(b.visible_text()[0], "prompt$ ls");
        assert_eq!(b.visible_text()[1], "file1  file2");
        assert_eq!(
            a.mode() & TermMode::KITTY_KEYBOARD_PROTOCOL,
            TermMode::empty()
        );

        // Primary flags survive the alt screen on the client too.
        roundtrip_then(30, 6, PRIMARY_AND_ALT, &[b"\x1b[?1049l", b"\x1b[<u"]);
        // Alt flags zero, primary flags set: the client's alt screen gets 0.
        roundtrip_then(
            30,
            6,
            b"\x1b[>1uprimary\x1b[?1049halt",
            &[b"\x1b[?1049l", b"\x1b[<u"],
        );
        // Primary flags zero, alt flags set.
        roundtrip_then(30, 6, b"primary\x1b[?1049h\x1b[>8ualt", &[b"\x1b[?1049l"]);
        // A blank primary and an alt saved cursor left over from an earlier
        // alt-screen session.
        roundtrip_then(
            30,
            6,
            b"\x1b[?1049h\x1b[2;2H\x1b7\x1b[?1049l\x1b[2J\x1b[H\x1b[?1049h",
            &[b"\x1b8x", b"\x1b[?1049l"],
        );
        // Pending wraps on both screens.
        roundtrip_then(
            5,
            3,
            b"abcde\x1b[?1049hvwxyz",
            &[b"1", b"\x1b[?1049l", b"2"],
        );
    }

    #[test]
    fn snapshot_leaves_terminal_unchanged() {
        let setups: [&[u8]; 4] = [
            PRIMARY_AND_ALT,
            b"plain\x1b[2;2H\x1b7\x1b[>5u",
            // Alt flags set without a push (lost by a bare swap_alt round trip).
            b"x\x1b[?1049h\x1b[=5;1uy",
            b"abcde\x1b[?1049hvwxyz",
        ];
        for setup in setups {
            let mut a = Terminal::new(5, 3);
            let mut twin = Terminal::new(5, 3);
            a.feed(setup);
            twin.feed(setup);
            for _ in 0..2 {
                a.snapshot();
            }
            let steps: [&[u8]; 6] = [
                b"",
                b"\x1b8q",
                b"\x1b[?1049l",
                b"\x1b8r",
                b"\x1b[?1049h",
                b"\x1b8s",
            ];
            for step in steps {
                a.feed(step);
                twin.feed(step);
                assert_eq!(observe(&a), observe(&twin), "{setup:?} then {step:?}");
            }
        }
    }

    #[test]
    fn snapshot_after_resize_on_alt_screen() {
        // Resizing reflows the inactive primary screen: alacritty moves its
        // cursor to (1, 1) while its saved cursor stays at (1, 3). The resize
        // then collects the primary's history through `with_primary`, so the
        // saved cursor becomes the cursor (a documented loss; see there) on
        // every terminal, and the snapshot keeps both; B matches A.
        let setup = b"0123456789abc\x1b[?1049halt";
        let mut a = Terminal::new(10, 4);
        let mut twin = Terminal::new(10, 4);
        for t in [&mut a, &mut twin] {
            t.feed(setup);
            t.resize(6, 3);
        }
        let mut b = roundtrip(&a);
        assert_same_screen(&a, &b);
        for step in [&b"\x1b[?1049l"[..], b"Z"] {
            for t in [&mut a, &mut b, &mut twin] {
                t.feed(step);
            }
            assert_same_screen(&a, &b);
            assert_eq!(a.visible_text(), twin.visible_text());
            assert_eq!(a.cursor(), twin.cursor());
            assert_eq!(a.mode(), twin.mode());
        }
        assert_eq!(a.visible_text()[1], "cZ");
        assert_eq!(twin.saved_cursor().1, 1, "saved cursor lost to the cursor");
        assert_eq!(a.saved_cursor().1, 1);
    }

    #[test]
    fn snapshot_roundtrip_random_with_alt_screen() {
        for (seed, cols, rows) in [(11, 80, 24), (12, 120, 40), (13, 40, 10)] {
            let mut a = Terminal::new(cols, rows);
            a.feed(&fake_output(seed, 32 * 1024));
            a.feed(b"\x1b7\x1b[?1049h");
            a.feed(&fake_output(seed + 100, 32 * 1024));
            let mut b = roundtrip(&a);
            assert_same_screen(&a, &b);
            a.feed(b"\x1b[?1049l");
            b.feed(b"\x1b[?1049l");
            assert_same_screen(&a, &b);
            assert!(a.visible_text().iter().any(|l| !l.is_empty()));
        }
    }

    /// Pins the alacritty `swap_alt` behaviour `with_primary` relies on; if an
    /// alacritty upgrade changes it, this fails and `with_primary` needs review.
    #[test]
    fn swap_alt_behaves_as_snapshot_expects() {
        let mut t = Terminal::new(10, 4);
        t.feed(b"prim\x1b[2;2H\x1b[>1u\x1b[?1049h\x1b[H\x1b[>4ualt\x1b[3;3H\x1b7\x1b[4;4H");
        let term = t.term.get_mut();
        let kitty = |term: &Term<Replies>| *term.mode() & TermMode::KITTY_KEYBOARD_PROTOCOL;
        let text = |term: &Term<Replies>| -> String {
            (0..4).map(|c| term.grid()[Line(0)][Column(c)].c).collect()
        };
        assert_eq!(text(term), "alt ");
        assert_eq!(kitty(term), TermMode::REPORT_ALTERNATE_KEYS);
        let alt_saved = term.grid().saved_cursor.point;

        // alt -> primary: the primary grid with its cursor, saved cursor
        // (`?1049h` saved it) and kitty flags.
        term.swap_alt();
        assert!(!term.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(text(term), "prim");
        assert_eq!(term.grid().cursor.point, term.grid().saved_cursor.point);
        assert_eq!(term.grid().cursor.point.line.0, 1);
        assert_eq!(kitty(term), TermMode::DISAMBIGUATE_ESC_CODES);
        term.grid_mut().cursor.point.column = Column(7);

        // primary -> alt: the alt grid is cleared and gets the primary cursor,
        // and the primary's saved cursor is overwritten by its cursor.
        term.swap_alt();
        assert!(term.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(text(term), "    ");
        assert_eq!(term.grid().cursor.point.column, Column(7));
        assert_eq!(
            term.grid().saved_cursor.point,
            alt_saved,
            "alt saved cursor kept"
        );
        assert_eq!(kitty(term), TermMode::REPORT_ALTERNATE_KEYS);
        term.swap_alt();
        assert_eq!(term.grid().saved_cursor.point.column, Column(7));
        assert_eq!(text(term), "prim");
    }

    #[test]
    fn snapshot_preserves_input_modes() {
        roundtrip_modes("\x1b[?2004h", TermMode::BRACKETED_PASTE, TermMode::empty());
        roundtrip_modes("\x1b[?1h", TermMode::APP_CURSOR, TermMode::empty());
        roundtrip_modes("\x1b=", TermMode::APP_KEYPAD, TermMode::empty());
        roundtrip_modes("\x1b[?1004h", TermMode::FOCUS_IN_OUT, TermMode::empty());
    }

    #[test]
    fn snapshot_preserves_mouse_modes() {
        roundtrip_modes(
            "\x1b[?1000h\x1b[?1006h",
            TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE,
            TermMode::empty(),
        );
        roundtrip_modes(
            "\x1b[?1002h\x1b[?1005h",
            TermMode::MOUSE_DRAG | TermMode::UTF8_MOUSE,
            TermMode::empty(),
        );
        roundtrip_modes("\x1b[?1003h", TermMode::MOUSE_MOTION, TermMode::empty());
    }

    #[test]
    fn snapshot_preserves_cursor_visibility_and_wrap() {
        roundtrip_modes("ab\x1b[?25l", TermMode::empty(), TermMode::SHOW_CURSOR);
        roundtrip_modes("ab\x1b[?7l", TermMode::empty(), TermMode::LINE_WRAP);
        roundtrip_modes(
            "ab\x1b[?1007l",
            TermMode::empty(),
            TermMode::ALTERNATE_SCROLL,
        );
    }

    #[test]
    fn snapshot_preserves_origin_insert_and_newline_modes() {
        roundtrip_modes("ab\x1b[?6h\x1b[3;4H", TermMode::ORIGIN, TermMode::empty());
        roundtrip_modes("ab\x1b[4h", TermMode::INSERT, TermMode::empty());
        roundtrip_modes(
            "ab\x1b[20h",
            TermMode::LINE_FEED_NEW_LINE,
            TermMode::empty(),
        );
    }

    #[test]
    fn snapshot_size_bounded() {
        let mut a = Terminal::new(120, 40);
        let chunk = fake_output(7, 1024 * 1024);
        for _ in 0..8 {
            a.feed(&chunk);
        }
        let snap = a.snapshot();
        assert!(snap.len() < 64 * 1024, "snapshot is {} bytes", snap.len());
    }

    #[test]
    fn resize_keeps_content() {
        let mut t = Terminal::new(20, 5);
        t.feed(b"first\r\nsecond\r\nthird");
        t.resize(40, 10);
        assert_eq!(t.size(), (40, 10));
        let text = t.visible_text();
        assert_eq!(text.len(), 10);
        assert_eq!(&text[..3], &["first", "second", "third"]);
        t.resize(30, 5);
        assert_eq!(&t.visible_text()[..3], &["first", "second", "third"]);
        assert_same_screen(&t, &roundtrip(&t));
    }

    #[test]
    fn query_replies_are_collected() {
        let mut t = Terminal::new(20, 5);
        assert!(t.take_replies().is_empty());
        t.feed(b"ab\x1b[6n");
        assert_eq!(t.take_replies(), b"\x1b[1;3R");
        assert!(t.take_replies().is_empty(), "replies are taken once");
        t.feed(b"\x1b[c");
        assert!(t.take_replies().starts_with(b"\x1b[?"), "DA reply");
    }

    #[test]
    fn expired_sync_update_is_applied() {
        let mut t = Terminal::new(20, 5);
        t.feed(b"\x1b[?2026hframe");
        assert_eq!(t.visible_text()[0], "", "buffered until ESU or timeout");
        t.flush_expired_sync();
        assert_eq!(t.visible_text()[0], "", "timeout not reached yet");
        std::thread::sleep(Duration::from_millis(200));
        t.flush_expired_sync();
        assert_eq!(t.visible_text()[0], "frame");

        // An expired update is also applied by the next feed, before new bytes.
        let mut t = Terminal::new(20, 5);
        t.feed(b"\x1b[?2026hone ");
        std::thread::sleep(Duration::from_millis(200));
        t.feed(b"two");
        assert_eq!(t.visible_text()[0], "one two");
    }

    #[test]
    fn pending_sync_update_is_applied_on_demand() {
        let mut t = Terminal::new(20, 5);
        t.flush_pending_sync(); // no-op without a pending update
        t.feed(b"\x1b[?2026hhalf ");
        t.flush_pending_sync();
        assert_eq!(t.visible_text()[0], "half");
        // The rest of the frame and its ESU then apply normally.
        t.feed(b"done\x1b[?2026l");
        assert_eq!(t.visible_text()[0], "half done");
    }
}
