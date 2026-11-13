//! The VT handler `Terminal` parses through: it renders each row of the
//! primary screen into the history the moment alacritty scrolls it off, then
//! lets alacritty (which keeps no scrollback) apply the operation.
//!
//! Alacritty 0.26 moves rows into scrollback in `Grid::scroll_up` when the
//! scrolled region starts at row 0 (also when it ends above the bottom), on
//! the primary screen only (the alt screen has none). The operations that get
//! there, mirrored here:
//! - LF/VT/FF, IND, NEL (`linefeed`, `newline`) with the cursor on the bottom
//!   margin;
//! - autowrap: printing (`input`) or a tab (`put_tab`) with a pending wrap, or
//!   a wide character in the last column, with the cursor on the bottom
//!   margin and autowrap on;
//! - SU (`scroll_up`) and DL (`delete_lines`) with the cursor on row 0;
//! - ED 2 (`clear_screen(All)`), which scrolls every row up to the last
//!   non-empty one into scrollback (a blank screen yields no rows here,
//!   where alacritty would push one blank row);
//! - shrinking resizes, handled by `Terminal::resize` (see `history::settle`).
//!
//! ED 3 (`clear_screen(Saved)`) on the primary screen queues
//! [`HistoryEvent::Cleared`]. Because these are the parser's callbacks, a
//! synchronized update's buffered operations are seen when they are applied.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, GridCell};
use alacritty_terminal::index::Line;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::cursor_icon::CursorIcon;
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Rgb,
    ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};
use unicode_width::UnicodeWidthChar;

use crate::history::{self, History, HistoryEvent};

/// `Term` plus the history sink, for one call into the parser.
pub(crate) struct Recorder<'a, T> {
    pub(crate) term: &'a mut Term<T>,
    pub(crate) history: &'a mut History,
}

impl<T: EventListener> Recorder<'_, T> {
    fn primary(&self) -> bool {
        !self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Whether a line feed now scrolls row 0 into scrollback: the cursor is on
    /// the bottom margin of a region that starts at the top, on the primary
    /// screen (alacritty's `linefeed`: `cursor + 1 == region.end`).
    #[inline]
    fn line_feed_scrolls(&self) -> bool {
        let region = &self.history.region;
        region.start == 0
            && self.term.grid().cursor.point.line.0 + 1 == region.end
            && self.primary()
    }

    /// Records rows `0..n` (`n` already clamped to the region) as history.
    fn scroll_off(&mut self, n: usize) {
        self.history.scroll_off(self.term.grid(), n);
    }

    /// Rows a scroll of `lines` from row 0 moves into scrollback (alacritty's
    /// `scroll_up_relative` clamps to the region's height).
    fn clamp_to_region(&self, lines: usize) -> usize {
        let region = &self.history.region;
        lines.min((region.end - region.start).max(0) as usize)
    }

    /// Before `input(c)` on the bottom margin with a pending wrap or in the
    /// last column: how many times printing `c` wraps (alacritty's `input`:
    /// a pending wrap wraps first, then a wide character that does not fit
    /// wraps again).
    #[inline(never)]
    fn wraps_before_input(&self, c: char) -> usize {
        let Some(width @ 1..) = c.width() else {
            return 0;
        };
        if !self.term.mode().contains(TermMode::LINE_WRAP) {
            return 0;
        }
        let cursor = &self.term.grid().cursor;
        let pending = cursor.input_needs_wrap;
        let column = if pending { 0 } else { cursor.point.column.0 };
        let wide = width == 2 && column + 1 >= self.term.columns();
        usize::from(pending) + usize::from(wide)
    }

    /// Number of rows ED 2 scrolls into scrollback: up to the last row with
    /// a non-empty cell (alacritty's `Grid::clear_viewport`).
    fn rows_cleared_by_ed2(&self) -> usize {
        let grid = self.term.grid();
        (0..grid.screen_lines())
            .rev()
            .find(|&row| {
                grid[Line(row as i32)][..]
                    .iter()
                    .any(|cell| !cell.is_empty())
            })
            .map_or(0, |row| row + 1)
    }
}

impl<T: EventListener> Handler for Recorder<'_, T> {
    #[inline]
    fn input(&mut self, c: char) {
        let cursor = &self.term.grid().cursor;
        if (cursor.input_needs_wrap || cursor.point.column.0 + 1 >= self.term.columns())
            && self.line_feed_scrolls()
        {
            let n = self.wraps_before_input(c);
            self.scroll_off(n);
        }
        Handler::input(&mut *self.term, c);
    }

    #[inline]
    fn put_tab(&mut self, count: u16) {
        if self.term.grid().cursor.input_needs_wrap
            && self.term.mode().contains(TermMode::LINE_WRAP)
            && self.line_feed_scrolls()
        {
            self.scroll_off(1);
        }
        Handler::put_tab(&mut *self.term, count);
    }

    #[inline]
    fn linefeed(&mut self) {
        if self.line_feed_scrolls() {
            self.scroll_off(1);
        }
        Handler::linefeed(&mut *self.term);
    }

    #[inline]
    fn newline(&mut self) {
        if self.line_feed_scrolls() {
            self.scroll_off(1);
        }
        Handler::newline(&mut *self.term);
    }

    fn scroll_up(&mut self, lines: usize) {
        if self.history.region.start == 0 && self.primary() {
            let n = self.clamp_to_region(lines);
            self.scroll_off(n);
        }
        Handler::scroll_up(&mut *self.term, lines);
    }

    fn delete_lines(&mut self, lines: usize) {
        // Alacritty scrolls `origin..region.end` up from the cursor row; only
        // from row 0 (inside a region starting there) do rows reach scrollback.
        let origin = self.term.grid().cursor.point.line.0;
        if origin == 0 && self.history.region.start == 0 && self.primary() {
            let lines = lines.min(self.term.screen_lines());
            if lines > 0 && self.history.region.contains(&origin) {
                let n = self.clamp_to_region(lines);
                self.scroll_off(n);
            }
        }
        Handler::delete_lines(&mut *self.term, lines);
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        let primary = self.primary();
        let saved = matches!(mode, ClearMode::Saved);
        if primary && matches!(mode, ClearMode::All) {
            let n = self.rows_cleared_by_ed2();
            self.scroll_off(n);
        }
        Handler::clear_screen(&mut *self.term, mode);
        if primary && saved {
            self.history.events.push(HistoryEvent::Cleared);
        }
    }

    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        // Mirrors `Term::set_scrolling_region`.
        let screen_lines = self.term.screen_lines();
        let end = bottom.unwrap_or(screen_lines);
        if top < end {
            let screen_lines = screen_lines as i32;
            self.history.region =
                (top as i32 - 1).min(screen_lines)..(end as i32).min(screen_lines);
        }
        Handler::set_scrolling_region(&mut *self.term, top, bottom);
    }

    fn reset_state(&mut self) {
        Handler::reset_state(&mut *self.term);
        self.history.region = 0..self.term.screen_lines() as i32;
        history::settle(self.term, self.history);
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        Handler::unset_private_mode(&mut *self.term, mode);
        // Leaving the alt screen: finish a resize staged while it was active.
        if self.history.staged {
            history::settle(self.term, self.history);
        }
    }

    // Everything else is delegated unchanged.

    #[inline]
    fn set_title(&mut self, a0: Option<String>) {
        Handler::set_title(&mut *self.term, a0);
    }

    #[inline]
    fn set_cursor_style(&mut self, a0: Option<CursorStyle>) {
        Handler::set_cursor_style(&mut *self.term, a0);
    }

    #[inline]
    fn set_cursor_shape(&mut self, shape: CursorShape) {
        Handler::set_cursor_shape(&mut *self.term, shape);
    }

    #[inline]
    fn goto(&mut self, line: i32, col: usize) {
        Handler::goto(&mut *self.term, line, col);
    }

    #[inline]
    fn goto_line(&mut self, line: i32) {
        Handler::goto_line(&mut *self.term, line);
    }

    #[inline]
    fn goto_col(&mut self, col: usize) {
        Handler::goto_col(&mut *self.term, col);
    }

    #[inline]
    fn insert_blank(&mut self, a0: usize) {
        Handler::insert_blank(&mut *self.term, a0);
    }

    #[inline]
    fn move_up(&mut self, a0: usize) {
        Handler::move_up(&mut *self.term, a0);
    }

    #[inline]
    fn move_down(&mut self, a0: usize) {
        Handler::move_down(&mut *self.term, a0);
    }

    #[inline]
    fn identify_terminal(&mut self, intermediate: Option<char>) {
        Handler::identify_terminal(&mut *self.term, intermediate);
    }

    #[inline]
    fn device_status(&mut self, a0: usize) {
        Handler::device_status(&mut *self.term, a0);
    }

    #[inline]
    fn move_forward(&mut self, col: usize) {
        Handler::move_forward(&mut *self.term, col);
    }

    #[inline]
    fn move_backward(&mut self, col: usize) {
        Handler::move_backward(&mut *self.term, col);
    }

    #[inline]
    fn move_down_and_cr(&mut self, row: usize) {
        Handler::move_down_and_cr(&mut *self.term, row);
    }

    #[inline]
    fn move_up_and_cr(&mut self, row: usize) {
        Handler::move_up_and_cr(&mut *self.term, row);
    }

    #[inline]
    fn backspace(&mut self) {
        Handler::backspace(&mut *self.term);
    }

    #[inline]
    fn carriage_return(&mut self) {
        Handler::carriage_return(&mut *self.term);
    }

    #[inline]
    fn bell(&mut self) {
        Handler::bell(&mut *self.term);
    }

    #[inline]
    fn substitute(&mut self) {
        Handler::substitute(&mut *self.term);
    }

    #[inline]
    fn set_horizontal_tabstop(&mut self) {
        Handler::set_horizontal_tabstop(&mut *self.term);
    }

    #[inline]
    fn scroll_down(&mut self, a0: usize) {
        Handler::scroll_down(&mut *self.term, a0);
    }

    #[inline]
    fn insert_blank_lines(&mut self, a0: usize) {
        Handler::insert_blank_lines(&mut *self.term, a0);
    }

    #[inline]
    fn erase_chars(&mut self, a0: usize) {
        Handler::erase_chars(&mut *self.term, a0);
    }

    #[inline]
    fn delete_chars(&mut self, a0: usize) {
        Handler::delete_chars(&mut *self.term, a0);
    }

    #[inline]
    fn move_backward_tabs(&mut self, count: u16) {
        Handler::move_backward_tabs(&mut *self.term, count);
    }

    #[inline]
    fn move_forward_tabs(&mut self, count: u16) {
        Handler::move_forward_tabs(&mut *self.term, count);
    }

    #[inline]
    fn save_cursor_position(&mut self) {
        Handler::save_cursor_position(&mut *self.term);
    }

    #[inline]
    fn restore_cursor_position(&mut self) {
        Handler::restore_cursor_position(&mut *self.term);
    }

    #[inline]
    fn clear_line(&mut self, mode: LineClearMode) {
        Handler::clear_line(&mut *self.term, mode);
    }

    #[inline]
    fn clear_tabs(&mut self, mode: TabulationClearMode) {
        Handler::clear_tabs(&mut *self.term, mode);
    }

    #[inline]
    fn set_tabs(&mut self, interval: u16) {
        Handler::set_tabs(&mut *self.term, interval);
    }

    #[inline]
    fn reverse_index(&mut self) {
        Handler::reverse_index(&mut *self.term);
    }

    #[inline]
    fn terminal_attribute(&mut self, attr: Attr) {
        Handler::terminal_attribute(&mut *self.term, attr);
    }

    #[inline]
    fn set_mode(&mut self, mode: Mode) {
        Handler::set_mode(&mut *self.term, mode);
    }

    #[inline]
    fn unset_mode(&mut self, mode: Mode) {
        Handler::unset_mode(&mut *self.term, mode);
    }

    #[inline]
    fn report_mode(&mut self, mode: Mode) {
        Handler::report_mode(&mut *self.term, mode);
    }

    #[inline]
    fn set_private_mode(&mut self, mode: PrivateMode) {
        Handler::set_private_mode(&mut *self.term, mode);
    }

    #[inline]
    fn report_private_mode(&mut self, mode: PrivateMode) {
        Handler::report_private_mode(&mut *self.term, mode);
    }

    #[inline]
    fn set_keypad_application_mode(&mut self) {
        Handler::set_keypad_application_mode(&mut *self.term);
    }

    #[inline]
    fn unset_keypad_application_mode(&mut self) {
        Handler::unset_keypad_application_mode(&mut *self.term);
    }

    #[inline]
    fn set_active_charset(&mut self, a0: CharsetIndex) {
        Handler::set_active_charset(&mut *self.term, a0);
    }

    #[inline]
    fn configure_charset(&mut self, a0: CharsetIndex, a1: StandardCharset) {
        Handler::configure_charset(&mut *self.term, a0, a1);
    }

    #[inline]
    fn set_color(&mut self, a0: usize, a1: Rgb) {
        Handler::set_color(&mut *self.term, a0, a1);
    }

    #[inline]
    fn dynamic_color_sequence(&mut self, a0: String, a1: usize, a2: &str) {
        Handler::dynamic_color_sequence(&mut *self.term, a0, a1, a2);
    }

    #[inline]
    fn reset_color(&mut self, a0: usize) {
        Handler::reset_color(&mut *self.term, a0);
    }

    #[inline]
    fn clipboard_store(&mut self, a0: u8, a1: &[u8]) {
        Handler::clipboard_store(&mut *self.term, a0, a1);
    }

    #[inline]
    fn clipboard_load(&mut self, a0: u8, a1: &str) {
        Handler::clipboard_load(&mut *self.term, a0, a1);
    }

    #[inline]
    fn decaln(&mut self) {
        Handler::decaln(&mut *self.term);
    }

    #[inline]
    fn push_title(&mut self) {
        Handler::push_title(&mut *self.term);
    }

    #[inline]
    fn pop_title(&mut self) {
        Handler::pop_title(&mut *self.term);
    }

    #[inline]
    fn text_area_size_pixels(&mut self) {
        Handler::text_area_size_pixels(&mut *self.term);
    }

    #[inline]
    fn text_area_size_chars(&mut self) {
        Handler::text_area_size_chars(&mut *self.term);
    }

    #[inline]
    fn set_hyperlink(&mut self, a0: Option<Hyperlink>) {
        Handler::set_hyperlink(&mut *self.term, a0);
    }

    #[inline]
    fn set_mouse_cursor_icon(&mut self, a0: CursorIcon) {
        Handler::set_mouse_cursor_icon(&mut *self.term, a0);
    }

    #[inline]
    fn report_keyboard_mode(&mut self) {
        Handler::report_keyboard_mode(&mut *self.term);
    }

    #[inline]
    fn push_keyboard_mode(&mut self, mode: KeyboardModes) {
        Handler::push_keyboard_mode(&mut *self.term, mode);
    }

    #[inline]
    fn pop_keyboard_modes(&mut self, to_pop: u16) {
        Handler::pop_keyboard_modes(&mut *self.term, to_pop);
    }

    #[inline]
    fn set_keyboard_mode(&mut self, mode: KeyboardModes, behavior: KeyboardModesApplyBehavior) {
        Handler::set_keyboard_mode(&mut *self.term, mode, behavior);
    }

    #[inline]
    fn set_modify_other_keys(&mut self, mode: ModifyOtherKeys) {
        Handler::set_modify_other_keys(&mut *self.term, mode);
    }

    #[inline]
    fn report_modify_other_keys(&mut self) {
        Handler::report_modify_other_keys(&mut *self.term);
    }

    #[inline]
    fn set_scp(&mut self, char_path: ScpCharPath, update_mode: ScpUpdateMode) {
        Handler::set_scp(&mut *self.term, char_path, update_mode);
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::Line;
    use alacritty_terminal::term::Term;
    use alacritty_terminal::vte::ansi::Processor;

    use crate::history::{HistoryEvent, config, render_line};
    use crate::tests::{Rng, fake_output};
    use crate::{Replies, Size, Terminal};

    /// Agent-like output plus the operations that scroll or clear: SU, DL,
    /// ED 2, scroll regions, IND/NEL, tabs, wide characters, autowrap and
    /// LNM toggles and alt screen round trips (no ED 3, no resize).
    ///
    /// Starts by scrolling 50 rows off: alacritty's ED 2 on a blank screen
    /// pushes one blank row when its scrollback is empty (and none otherwise),
    /// which the recorder deliberately does not reproduce.
    fn scrolly_output(seed: u64, len: usize) -> Vec<u8> {
        let mut rng = Rng(seed | 1);
        let mut out = b"warm\r\n".repeat(50);
        while out.len() < len {
            let part: Vec<u8> = match rng.below(16) {
                0 => format!("\x1b[{}S", 1 + rng.below(6)).into_bytes(),
                1 => format!("\x1b[{};1H\x1b[{}M", 1 + rng.below(3), 1 + rng.below(4)).into_bytes(),
                2 => b"\x1b[2J".to_vec(),
                3 => {
                    let top = 1 + rng.below(4);
                    format!("\x1b[{top};{}r", top + 1 + rng.below(12)).into_bytes()
                }
                4 => b"\x1b[r".to_vec(),
                5 => b"\x1bD\x1bE".to_vec(),
                6 => b"\t\t\tab\t".to_vec(),
                7 => "中文字符λ✓".repeat(1 + rng.below(8) as usize).into_bytes(),
                8 => b"\x1b[?7l".to_vec(),
                9 => b"\x1b[?7h".to_vec(),
                10 => b"\x1b[20h\n\n\x1b[20l".to_vec(),
                11 => b"\x1b[?1049hx\r\n\r\n\x1b[?1049l".to_vec(),
                12 => format!("\x1b[{};{}H", 1 + rng.below(16), 1 + rng.below(40)).into_bytes(),
                _ => fake_output(rng.next(), 300),
            };
            out.extend_from_slice(&part);
        }
        out
    }

    /// Our history against alacritty's own scrollback for the same input.
    fn assert_matches_alacritty(cols: u16, rows: u16, input: &[u8], chunk: usize) {
        let mut ours = Terminal::new(cols, rows);
        let mut lines = Vec::new();
        for piece in input.chunks(chunk) {
            ours.feed(piece);
            for event in ours.drain_history() {
                match event {
                    HistoryEvent::Line(line) => lines.push(line),
                    HistoryEvent::Cleared => panic!("no ED 3 in the input"),
                }
            }
        }
        let size = Size::new(cols, rows);
        let mut reference = Term::new(config(1_000_000), &size, Replies::default());
        // Same chunks: vte 0.15 parses a UTF-8 character split across
        // `advance` calls differently in rare cases (it can drop the next
        // character), so both sides must see the same splits.
        let mut parser: Processor = Processor::new();
        for piece in input.chunks(chunk) {
            parser.advance(&mut reference, piece);
        }
        let grid = reference.grid();
        let expected: Vec<Vec<u8>> = (1..=grid.history_size())
            .rev()
            .map(|i| render_line(&grid[Line(-(i as i32))]))
            .collect();
        for (i, (got, want)) in lines.iter().zip(&expected).enumerate() {
            assert_eq!(
                String::from_utf8_lossy(got),
                String::from_utf8_lossy(want),
                "history line {i} ({cols}x{rows})"
            );
        }
        assert_eq!(
            lines.len(),
            expected.len(),
            "history length ({cols}x{rows})"
        );
        let term = ours.term.borrow();
        for row in 0..grid.screen_lines() {
            assert_eq!(
                render_line(&term.grid()[Line(row as i32)]),
                render_line(&grid[Line(row as i32)]),
                "screen row {row}"
            );
        }
        assert_eq!(term.grid().cursor.point, grid.cursor.point);
    }

    #[test]
    fn history_matches_alacritty_scrollback() {
        for seed in 1..=40 {
            let input = scrolly_output(seed, 64 * 1024);
            for (cols, rows) in [(40, 16), (7, 3), (120, 40)] {
                assert_matches_alacritty(cols, rows, &input, 1 + (seed as usize * 997) % 5000);
            }
        }
    }

    #[test]
    fn history_matches_alacritty_on_agent_output() {
        let input = fake_output(3, 256 * 1024);
        assert_matches_alacritty(120, 40, &input, 4096);
    }
}
