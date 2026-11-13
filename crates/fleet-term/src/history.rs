//! Rows scrolled off the primary screen, rendered as ANSI history lines.
//!
//! Also holds the cell and SGR rendering that [`Terminal::snapshot`](crate::Terminal::snapshot)
//! shares with history lines.

use std::io::Write as _;

use alacritty_terminal::grid::{Dimensions, Row};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::{Cell, Flags};
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

/// Moves the rows alacritty holds in its scrollback into `events` as
/// [`HistoryEvent::Line`]s, oldest first, and empties the scrollback.
pub(crate) fn capture<T>(term: &mut Term<T>, events: &mut Vec<HistoryEvent>) {
    let grid = term.grid();
    let n = grid.history_size();
    if n == 0 {
        return;
    }
    events.reserve(n);
    for i in (1..=n).rev() {
        events.push(HistoryEvent::Line(render_line(&grid[Line(-(i as i32))])));
    }
    term.grid_mut().clear_history();
}

/// One row as a self-contained ANSI line (see [`HistoryEvent::Line`]).
fn render_line(row: &Row<Cell>) -> Vec<u8> {
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

fn sgr_mask() -> Flags {
    SGR_FLAGS
        .iter()
        .fold(Flags::empty(), |acc, &(flag, _)| acc | flag)
}

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
            flags: cell.flags & sgr_mask(),
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
