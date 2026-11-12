//! Headless VT grid per agent (decision 0005): PTY output is fed through a
//! [`Terminal`]; on (re)attach a client receives [`Terminal::snapshot`] and
//! then the live stream, so reattach cost is O(screen).

use std::io::Write;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};

/// Grid size handed to alacritty. Scrollback is disabled: history lives in the
/// per-agent ring file (Task 3), and keeping it here would cost memory per agent.
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

/// Headless terminal emulator holding the visible screen of one agent.
pub struct Terminal {
    term: Term<VoidListener>,
    parser: Processor,
}

impl Terminal {
    /// Creates a blank screen; zero dimensions are clamped to 1.
    pub fn new(cols: u16, rows: u16) -> Self {
        let config = Config {
            scrolling_history: 0,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &Size::new(cols, rows), VoidListener),
            parser: Processor::new(),
        }
    }

    /// Feeds raw PTY output through the VT parser.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Resizes the screen (reflowing the primary screen); zero dimensions are clamped to 1.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(Size::new(cols, rows));
    }

    /// Current size as `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        let grid = self.term.grid();
        (to_u16(grid.columns()), to_u16(grid.screen_lines()))
    }

    /// ANSI byte sequence that redraws the visible screen into a fresh terminal of
    /// the same size: text, SGR attributes, the active pen and the cursor position.
    ///
    /// Starts with `\x1b[0m\x1b[H\x1b[2J` and ends with `\x1b[{row};{col}H`
    /// (1-based), except when the cursor has a pending autowrap: then the glyph
    /// under the cursor is re-emitted after the CUP to recreate that state.
    pub fn snapshot(&self) -> Vec<u8> {
        let grid = self.term.grid();
        let (cols, rows) = (grid.columns(), grid.screen_lines());
        let mut out = Vec::with_capacity(rows * (cols + 8) + 64);
        out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
        let mut pen = Pen::default();
        for row in 0..rows {
            let line = &grid[Line(row as i32)];
            let Some(end) = (0..cols).rev().find(|&c| !is_blank(&line[Column(c)])) else {
                continue;
            };
            if row > 0 {
                cup(&mut out, row, 0);
            }
            for col in 0..=end {
                let cell = &line[Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                pen.transition(&mut out, Pen::of(cell));
                put_glyph(&mut out, cell);
            }
        }

        let cursor = &grid.cursor;
        let (row, col) = (cursor.point.line.0 as usize, cursor.point.column.0);
        pen.transition(&mut out, Pen::of(&cursor.template));
        cup(&mut out, row, col);
        if cursor.input_needs_wrap {
            let line = &grid[cursor.point.line];
            let start = if line[Column(col)].flags.contains(Flags::WIDE_CHAR_SPACER) {
                col.saturating_sub(1)
            } else {
                col
            };
            let cell = &line[Column(start)];
            if start != col {
                cup(&mut out, row, start);
            }
            pen.transition(&mut out, Pen::of(cell));
            put_glyph(&mut out, cell);
            pen.transition(&mut out, Pen::of(&cursor.template));
        }
        out
    }

    /// Visible screen as one `String` per row, trailing spaces trimmed.
    pub fn visible_text(&self) -> Vec<String> {
        let grid = self.term.grid();
        (0..grid.screen_lines())
            .map(|row| {
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
            })
            .collect()
    }

    /// Cursor position as 0-based `(row, col)`.
    pub fn cursor(&self) -> (u16, u16) {
        let point = self.term.grid().cursor.point;
        (to_u16(point.line.0.max(0) as usize), to_u16(point.column.0))
    }

    /// Rendering attributes of one cell, for roundtrip comparisons in tests.
    #[cfg(test)]
    fn cell_attrs(&self, row: u16, col: u16) -> Pen {
        Pen::of(&self.term.grid()[Line(i32::from(row))][Column(usize::from(col))])
    }
}

fn to_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// The character a cell renders; a leading wide-char spacer renders as blank.
fn glyph(cell: &Cell) -> char {
    if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
        ' '
    } else {
        cell.c
    }
}

fn put_glyph(out: &mut Vec<u8>, cell: &Cell) {
    let mut buf = [0; 4];
    out.extend_from_slice(glyph(cell).encode_utf8(&mut buf).as_bytes());
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

/// 0-based CUP.
fn cup(out: &mut Vec<u8>, row: usize, col: usize) {
    let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
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
struct Pen {
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
    fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & sgr_mask(),
            underline: cell.underline_color(),
        }
    }

    /// Emits the SGR sequence that changes `self` into `to`, then becomes `to`.
    /// Turning any flag off resets first; otherwise only the differences are sent.
    fn transition(&mut self, out: &mut Vec<u8>, to: Pen) {
        if *self == to {
            return;
        }
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

    /// Deterministic xorshift64* PRNG for fixture generation.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Agent-like ANSI output of at least `len` bytes: printable text, SGR
    /// colors (16, 256, truecolor), bold/italic/underline/inverse, CRLF,
    /// `\x1b[2K\r` line rewrites and cursor moves.
    fn fake_output(seed: u64, len: usize) -> Vec<u8> {
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
}
