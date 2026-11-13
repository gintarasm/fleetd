//! Rows scrolled off the primary screen, rendered as ANSI history lines.
//!
//! Also holds the cell and SGR rendering that [`Terminal::snapshot`](crate::Terminal::snapshot)
//! shares with history lines.

use std::io::Write as _;

use std::ops::Range;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Grid, Row};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor};

/// What happened to the scrollback since the last
/// [`Terminal::drain_history`](crate::Terminal::drain_history).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryEvent {
    /// One grid row that scrolled off the top of the primary screen, as ANSI:
    /// drawn from the default pen, trailing default blanks trimmed, ending
    /// with `\x1b[0m` if the pen is not the default by then. No newline.
    Line(Vec<u8>),
    /// The agent erased the saved lines (`CSI 3 J`) on the primary screen.
    Cleared,
}

/// Scrollback rows alacritty may hold during [`Terminal::resize`](crate::Terminal::resize),
/// which collects them right after (see [`settle`]). Otherwise it keeps none:
/// rows are rendered by [`Recorder`](crate::recorder::Recorder) the moment they
/// scroll off.
pub(crate) const RESIZE_SLACK: usize = 10_000;

/// History events not yet drained, plus what [`Recorder`](crate::recorder::Recorder)
/// needs to know when a row scrolls off.
pub(crate) struct History {
    pub(crate) events: Vec<HistoryEvent>,
    /// Mirror of alacritty's scroll region (DECSTBM), which it keeps private:
    /// screen rows `start..end`. Maintained exactly as `Term` maintains its
    /// own (set by `set_scrolling_region`, reset by resize and RIS).
    pub(crate) region: Range<i32>,
}

impl History {
    pub(crate) fn new(rows: usize) -> Self {
        Self {
            events: Vec::new(),
            region: 0..rows as i32,
        }
    }

    /// Renders rows `0..n` of `grid`, about to scroll off, as history lines.
    pub(crate) fn scroll_off(&mut self, grid: &Grid<Cell>, n: usize) {
        let n = n.min(grid.screen_lines());
        self.events.reserve(n);
        for row in 0..n {
            self.events
                .push(HistoryEvent::Line(render_line(&grid[Line(row as i32)])));
        }
    }
}

/// Alacritty config with `history` rows of scrollback.
pub(crate) fn config(history: usize) -> Config {
    Config {
        scrolling_history: history,
        // Track the kitty keyboard flags so snapshots can carry them.
        kitty_keyboard: true,
        ..Config::default()
    }
}

/// Collects the rows a resize pushed into the scrollback of `term`'s active
/// grid (the primary screen: the caller swaps to it first if needed), oldest
/// first, then disables the scrollback again and frees its row cache, so no
/// history stays in alacritty between calls.
pub(crate) fn settle<T: EventListener>(term: &mut Term<T>, history: &mut History) {
    debug_assert!(!term.mode().contains(TermMode::ALT_SCREEN));
    let grid = term.grid();
    let n = grid.history_size();
    history.events.reserve(n);
    for i in (1..=n).rev() {
        history
            .events
            .push(HistoryEvent::Line(render_line(&grid[Line(-(i as i32))])));
    }
    term.grid_mut().clear_history();
    term.set_options(config(0));
    term.grid_mut().truncate();
}

/// One row as a self-contained ANSI line (see [`HistoryEvent::Line`]).
pub(crate) fn render_line(row: &Row<Cell>) -> Vec<u8> {
    let Some(end) = last_non_blank(row) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(end + 16);
    let mut pen = Pen::default();
    put_cells(&mut out, &mut pen, row, end);
    if pen != Pen::default() {
        out.extend_from_slice(b"\x1b[0m");
    }
    out
}

/// Index of the last cell of `row` that is not a default blank.
pub(crate) fn last_non_blank(row: &Row<Cell>) -> Option<usize> {
    row[..].iter().rposition(|cell| !is_blank(cell))
}

/// Draws the cells `0..=end` of `row`, changing `pen` as needed.
pub(crate) fn put_cells(out: &mut Vec<u8>, pen: &mut Pen, row: &Row<Cell>, end: usize) {
    for cell in &row[..=Column(end)] {
        // Fast path: a plain ASCII cell drawn with the current pen.
        if cell.extra.is_none()
            && pen.underline.is_none()
            && cell.c.is_ascii()
            && cell.flags & !SGR_MASK == Flags::empty()
            && cell.flags == pen.flags
            && cell.fg == pen.fg
            && cell.bg == pen.bg
        {
            out.push(cell.c as u8);
            continue;
        }
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        pen.transition(out, Pen::of(cell));
        put_glyph(out, cell);
    }
}

/// The character a cell renders; a leading wide-char spacer renders as blank.
pub(crate) fn glyph(cell: &Cell) -> char {
    if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
        ' '
    } else {
        cell.c
    }
}

#[inline]
pub(crate) fn put_glyph(out: &mut Vec<u8>, cell: &Cell) {
    let c = glyph(cell);
    // Fast path: most cells are ASCII without combining characters.
    if c.is_ascii() && cell.extra.is_none() {
        out.push(c as u8);
        return;
    }
    let mut buf = [0; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    for &c in cell.zerowidth().unwrap_or_default() {
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }
}

/// A cell that `\x1b[2J` after `\x1b[0m` already produces.
fn is_blank(cell: &Cell) -> bool {
    glyph(cell) == ' '
        && cell.zerowidth().is_none_or(<[char]>::is_empty)
        && Pen::of(cell) == Pen::default()
}

/// SGR flags that affect rendering and survive a roundtrip, with their SGR codes.
const SGR_FLAGS: [(Flags, &str); 11] = [
    (Flags::BOLD, "1"),
    (Flags::DIM, "2"),
    (Flags::ITALIC, "3"),
    (Flags::UNDERLINE, "4"),
    (Flags::DOUBLE_UNDERLINE, "4:2"),
    (Flags::UNDERCURL, "4:3"),
    (Flags::DOTTED_UNDERLINE, "4:4"),
    (Flags::DASHED_UNDERLINE, "4:5"),
    (Flags::INVERSE, "7"),
    (Flags::HIDDEN, "8"),
    (Flags::STRIKEOUT, "9"),
];

/// The union of [`SGR_FLAGS`].
const SGR_MASK: Flags = {
    let mut mask = Flags::empty();
    let mut i = 0;
    while i < SGR_FLAGS.len() {
        mask = mask.union(SGR_FLAGS[i].0);
        i += 1;
    }
    mask
};

/// Graphic rendition of a cell (the "pen" that drew it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pen {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline: Option<Color>,
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
            underline: None,
        }
    }
}

impl Pen {
    pub(crate) fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & SGR_MASK,
            underline: cell.underline_color(),
        }
    }

    /// Emits the SGR sequence that changes `self` into `to`, then becomes `to`.
    /// Turning any flag off resets first; otherwise only the differences are sent.
    #[inline]
    pub(crate) fn transition(&mut self, out: &mut Vec<u8>, to: Pen) {
        if *self != to {
            self.change(out, to);
        }
    }

    #[inline(never)]
    fn change(&mut self, out: &mut Vec<u8>, to: Pen) {
        let mut sgr = Sgr::start(out);
        let mut base = *self;
        if !(base.flags - to.flags).is_empty()
            || (base.underline.is_some() && to.underline.is_none())
        {
            sgr.param("0");
            base = Pen::default();
        }
        for (flag, code) in SGR_FLAGS {
            if to.flags.contains(flag) && !base.flags.contains(flag) {
                sgr.param(code);
            }
        }
        if to.fg != base.fg {
            sgr.color(to.fg, 30, 90, 38, 39);
        }
        if to.bg != base.bg {
            sgr.color(to.bg, 40, 100, 48, 49);
        }
        if to.underline != base.underline
            && let Some(color) = to.underline
        {
            sgr.color(color, 0, 0, 58, 59);
        }
        sgr.finish();
        *self = to;
    }
}

/// Writer for a single `CSI ... m` sequence.
struct Sgr<'a> {
    out: &'a mut Vec<u8>,
    first: bool,
}

impl<'a> Sgr<'a> {
    fn start(out: &'a mut Vec<u8>) -> Self {
        out.extend_from_slice(b"\x1b[");
        Self { out, first: true }
    }

    fn sep(&mut self) {
        if !self.first {
            self.out.push(b';');
        }
        self.first = false;
    }

    fn param(&mut self, code: &str) {
        self.sep();
        self.out.extend_from_slice(code.as_bytes());
    }

    /// `normal`/`bright` are the 8-color bases (unused for underline color, which
    /// always uses the extended form); `ext` is 38/48/58, `default` 39/49/59.
    fn color(&mut self, color: Color, normal: u8, bright: u8, ext: u8, default: u8) {
        self.sep();
        let _ = match color {
            Color::Named(named) => match named as usize {
                n @ 0..8 if normal != 0 => write!(self.out, "{}", usize::from(normal) + n),
                n @ 8..16 if bright != 0 => write!(self.out, "{}", usize::from(bright) + n - 8),
                n @ 0..16 => write!(self.out, "{ext};5;{n}"),
                // Foreground/Background and the renderer-only named colors.
                _ => write!(self.out, "{default}"),
            },
            Color::Indexed(i) => write!(self.out, "{ext};5;{i}"),
            Color::Spec(rgb) => write!(self.out, "{ext};2;{};{};{}", rgb.r, rgb.g, rgb.b),
        };
    }

    fn finish(self) {
        self.out.push(b'm');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Terminal;
    use alacritty_terminal::grid::Dimensions;

    /// The text of `line` with its SGR sequences removed.
    fn strip_sgr(line: &[u8]) -> String {
        let mut out = Vec::new();
        let mut i = 0;
        while i < line.len() {
            if line[i..].starts_with(b"\x1b[") {
                let end = line[i + 2..]
                    .iter()
                    .position(|&b| b == b'm')
                    .expect("unterminated SGR");
                i += 2 + end + 1;
            } else {
                out.push(line[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    /// Text of every event: lines stripped of SGR, `Cleared` as `"<cleared>"`.
    fn texts(events: &[HistoryEvent]) -> Vec<String> {
        events
            .iter()
            .map(|e| match e {
                HistoryEvent::Line(l) => strip_sgr(l),
                HistoryEvent::Cleared => "<cleared>".to_owned(),
            })
            .collect()
    }

    #[test]
    fn scrolled_lines_become_history() {
        let mut t = Terminal::new(20, 10);
        for i in 0..30 {
            t.feed(format!("l{i}\r\n").as_bytes());
        }
        let expected: Vec<String> = (0..=20).map(|i| format!("l{i}")).collect();
        assert_eq!(texts(&t.drain_history()), expected);
        assert!(t.drain_history().is_empty(), "drain empties the queue");
    }

    #[test]
    fn line_rendering_is_self_contained() {
        let mut t = Terminal::new(20, 2);
        // Rows scrolled in while the pen has a background are filled with it.
        t.feed(b"\x1b[1;31mred\x1b[0m plain   \r\nplain\r\n\x1b[44m\r\n\r\n\r\n");
        let mut blue = b"\x1b[44m".to_vec();
        blue.extend_from_slice(&[b' '; 20]);
        blue.extend_from_slice(b"\x1b[0m");
        assert_eq!(
            t.drain_history(),
            [
                HistoryEvent::Line(b"\x1b[1;31mred\x1b[0m plain".to_vec()),
                HistoryEvent::Line(b"plain".to_vec()),
                HistoryEvent::Line(Vec::new()),
                HistoryEvent::Line(blue),
            ]
        );
    }

    #[test]
    fn colored_line_roundtrips() {
        let mut t = Terminal::new(20, 3);
        t.feed(b"\x1b[31mred\x1b[0m ok\x1b[48;2;1;2;3mbg\x1b[0m");
        let attrs: Vec<_> = (0..8).map(|col| t.cell_attrs(0, col)).collect();
        t.feed(b"\r\n\r\n\r\n");
        let events = t.drain_history();
        let [HistoryEvent::Line(line)] = &events[..] else {
            panic!("expected one line, got {events:?}");
        };
        assert!(line.ends_with(b"\x1b[0m"), "{line:?}");
        let mut fresh = Terminal::new(20, 3);
        fresh.feed(line);
        let got: Vec<_> = (0..8).map(|col| fresh.cell_attrs(0, col)).collect();
        assert_eq!(got, attrs);
        assert_eq!(fresh.visible_text()[0], "red okbg");
    }

    #[test]
    fn wrapped_long_line_yields_rows() {
        let mut t = Terminal::new(120, 5);
        t.feed(&[b'x'; 300]);
        t.feed(b"\r\n\r\n\r\n\r\n\r\n");
        let lens: Vec<usize> = texts(&t.drain_history()).iter().map(String::len).collect();
        assert_eq!(lens, [120, 120, 60]);
    }

    #[test]
    fn many_lines_in_one_feed_lose_nothing() {
        let mut t = Terminal::new(120, 40);
        let mut input = Vec::new();
        for i in 0..65_536 {
            input.extend_from_slice(format!("{i}\r\n").as_bytes());
        }
        t.feed(&input);
        let texts = texts(&t.drain_history());
        assert_eq!(texts.len(), 65_536 - 39);
        for (i, text) in texts.iter().enumerate() {
            assert_eq!(*text, i.to_string());
        }
    }

    #[test]
    fn clear_in_same_chunk_orders_lines_and_clear() {
        let mut t = Terminal::new(20, 10);
        let mut input = b"a\r\n".repeat(50);
        input.extend_from_slice(b"\x1b[3J");
        input.extend_from_slice(&b"b\r\n".repeat(50));
        t.feed(&input);
        // The 9 `a` rows still on screen at the clear scroll off after it.
        let mut expected = vec!["a"; 41];
        expected.push("<cleared>");
        expected.extend(["a"; 9]);
        expected.extend(["b"; 41]);
        assert_eq!(texts(&t.drain_history()), expected);
    }

    #[test]
    fn clear_split_across_feeds_is_detected() {
        let mut t = Terminal::new(20, 10);
        t.feed(b"\x1b[3");
        assert!(t.drain_history().is_empty());
        t.feed(b"J");
        assert_eq!(t.drain_history(), [HistoryEvent::Cleared]);
    }

    #[test]
    fn alt_screen_produces_no_history() {
        let mut t = Terminal::new(20, 10);
        t.feed(b"\x1b[?1049h");
        t.feed(&b"x\r\n".repeat(50));
        t.feed(b"\x1b[3J");
        assert!(t.drain_history().is_empty());
        t.feed(b"\x1b[?1049l");
        assert!(t.drain_history().is_empty());
    }

    #[test]
    fn resize_shrink_turns_rows_into_history() {
        let mut t = Terminal::new(20, 10);
        for i in 0..9 {
            t.feed(format!("l{i}\r\n").as_bytes());
        }
        t.resize(20, 5);
        assert_eq!(texts(&t.drain_history()), ["l0", "l1", "l2", "l3", "l4"]);
        assert_eq!(t.visible_text(), ["l5", "l6", "l7", "l8", ""]);
    }

    #[test]
    fn sync_update_lines_are_captured_when_applied() {
        let mut t = Terminal::new(20, 2);
        t.feed(b"\x1b[?2026ha\r\nb\r\nc\r\n");
        assert!(t.drain_history().is_empty(), "frame still buffered");
        t.flush_pending_sync();
        assert_eq!(texts(&t.drain_history()), ["a", "b"]);
    }

    #[test]
    fn scroll_up_sequence_moves_rows_into_history() {
        let mut t = Terminal::new(20, 5);
        t.feed(b"l0\r\nl1\r\nl2\x1b[2S");
        assert_eq!(texts(&t.drain_history()), ["l0", "l1"]);
        assert_eq!(t.visible_text(), ["l2", "", "", "", ""]);
        // Clamped to the region: at most one screen of rows.
        t.feed(b"\x1b[99S");
        assert_eq!(texts(&t.drain_history()), ["l2", "", "", "", ""]);
    }

    #[test]
    fn scroll_region_below_top_produces_no_history() {
        let mut t = Terminal::new(20, 5);
        t.feed(b"head\x1b[2;5r\x1b[5;1H");
        for i in 0..10 {
            t.feed(format!("x{i}\r\n").as_bytes());
        }
        t.feed(b"\x1b[3S\x1b[2;1H\x1b[2M");
        assert!(t.drain_history().is_empty());
        assert_eq!(t.visible_text()[0], "head");
    }

    #[test]
    fn scroll_region_from_top_feeds_history() {
        // As in alacritty: a region starting at the top scrolls into history
        // even when it ends above the bottom; the rows below stay put.
        let mut t = Terminal::new(20, 5);
        t.feed(b"\x1b[5;1Hfoot\x1b[1;3r\x1b[3;1Ha\r\nb\r\nc\r\n");
        assert_eq!(texts(&t.drain_history()), ["", "", "a"]);
        assert_eq!(t.visible_text(), ["b", "c", "", "", "foot"]);
    }

    #[test]
    fn erase_display_pushes_screen_into_history() {
        let mut t = Terminal::new(20, 5);
        t.feed(b"a\r\nb\r\n\x1b[2J");
        assert_eq!(texts(&t.drain_history()), ["a", "b"]);
        t.feed(b"\x1b[2J");
        assert!(t.drain_history().is_empty(), "blank screen: no rows");
    }

    #[test]
    fn delete_lines_on_top_row_feeds_history() {
        let mut t = Terminal::new(20, 5);
        t.feed(b"a\r\nb\r\nc\x1b[H\x1b[2M");
        assert_eq!(texts(&t.drain_history()), ["a", "b"]);
        t.feed(b"\x1b[2;1H\x1b[M");
        assert!(t.drain_history().is_empty(), "below the top row");
    }

    #[test]
    fn autowrap_on_bottom_row_feeds_history() {
        let mut t = Terminal::new(4, 2);
        // Pending wrap, then a character.
        t.feed(b"top\r\nabcde");
        assert_eq!(texts(&t.drain_history()), ["top"]);
        // A wide character that does not fit in the last column.
        t.feed("\r\nxyz中".as_bytes());
        assert_eq!(texts(&t.drain_history()), ["abcd", "e"]);
        assert_eq!(t.visible_text(), ["xyz", "中"]);
        // A tab with a pending wrap.
        t.feed(b"\r\nwxyz\t");
        assert_eq!(texts(&t.drain_history()), ["xyz", "中"]);
        // No autowrap: nothing scrolls.
        t.feed(b"\x1b[?7l\r\nlongerline");
        assert_eq!(texts(&t.drain_history()), ["wxyz"]);
    }

    #[test]
    fn resize_on_alt_screen_is_captured_at_once() {
        let mut t = Terminal::new(20, 10);
        for i in 0..9 {
            t.feed(format!("l{i}\r\n").as_bytes());
        }
        t.feed(b"\x1b[?1049halt");
        t.resize(20, 5);
        // The rows the shrink pushed off the primary screen behind the alt
        // screen are history now; alacritty keeps none of them.
        assert_eq!(texts(&t.drain_history()), ["l0", "l1", "l2", "l3", "l4"]);
        let primary_history =
            crate::with_primary(&mut t.term.borrow_mut(), |term| term.grid().history_size());
        assert_eq!(primary_history, 0);
        // `?1049h` put the alt cursor on row 9; the shrink scrolled it to 4.
        assert_eq!(t.visible_text()[4], "alt", "alt screen kept");
        t.feed(b"\x1b[?1049lnext\r\n");
        assert_eq!(texts(&t.drain_history()), ["l5"]);
        assert_eq!(t.visible_text(), ["l6", "l7", "l8", "next", ""]);
        assert_eq!(t.term.borrow().grid().history_size(), 0);
    }

    #[test]
    fn reset_after_alt_screen_resize_keeps_its_rows() {
        let mut t = Terminal::new(20, 10);
        for i in 0..9 {
            t.feed(format!("l{i}\r\n").as_bytes());
        }
        t.feed(b"\x1b[?1049h");
        t.resize(20, 5);
        t.feed(b"\x1bc");
        assert_eq!(texts(&t.drain_history()), ["l0", "l1", "l2", "l3", "l4"]);
    }

    #[test]
    fn deccolm_set_resets_scroll_region() {
        // `?3h` resets alacritty's scroll region; rows must still scroll off.
        let mut t = Terminal::new(20, 5);
        t.feed(b"\x1b[2;5r\x1b[?3ha\r\nb\r\nc\r\nd\r\ne\r\n\r\n\r\n");
        assert_eq!(texts(&t.drain_history()), ["a", "b", "c"]);
    }

    #[test]
    fn deccolm_reset_resets_scroll_region() {
        // After `?3l` row 3 is no longer a bottom margin: nothing scrolls.
        let mut t = Terminal::new(20, 5);
        t.feed(b"\x1b[1;3r\x1b[?3l\x1b[3;1Hz\n\n");
        assert!(t.drain_history().is_empty());
        assert_eq!(t.visible_text()[2], "z");
    }

    #[test]
    fn clear_inside_sync_update_keeps_order() {
        let mut t = Terminal::new(20, 10);
        let mut input = b"\x1b[?2026h".to_vec();
        input.extend_from_slice(&b"a\r\n".repeat(50));
        input.extend_from_slice(b"\x1b[3J");
        input.extend_from_slice(&b"b\r\n".repeat(50));
        input.extend_from_slice(b"\x1b[?2026l");
        t.feed(&input);
        let mut expected = vec!["a"; 41];
        expected.push("<cleared>");
        expected.extend(["a"; 9]);
        expected.extend(["b"; 41]);
        assert_eq!(texts(&t.drain_history()), expected);
    }

    #[test]
    fn grid_holds_no_history_after_feed() {
        let mut t = Terminal::new(120, 40);
        let output = crate::tests::fake_output(7, 256 * 1024);
        for chunk in output.chunks(5000) {
            t.feed(chunk);
            assert_eq!(t.term.borrow().grid().history_size(), 0);
        }
        assert!(!t.drain_history().is_empty());
        t.feed(&b"\r\n".repeat(10_000));
        assert_eq!(t.term.borrow().grid().history_size(), 0);
    }
}
