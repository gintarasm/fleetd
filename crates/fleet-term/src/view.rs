//! Cell, mode and cursor views of a [`Terminal`](crate::Terminal) for a
//! client that draws it, and the palette that answers colour queries.

use alacritty_terminal::grid::Row;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell as GridCell, Flags};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape as AnsiShape, NamedColor};

use crate::kitty_flags;

/// A cell colour. `Default` is the palette's foreground or background,
/// depending on the slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    /// ANSI colour 0..=15.
    Named(u8),
    /// xterm 256-colour index.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// How a cell is underlined (SGR 4, 4:0..4:5, 21, 24).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    /// Undercurl (`4:3`): diagnostics.
    Curly,
    Dotted,
    Dashed,
}

impl Underline {
    pub fn is_some(self) -> bool {
        self != Underline::None
    }
}

/// SGR attributes of a cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellStyle {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: Underline,
    /// SGR 58; `Default` is the text colour.
    pub underline_color: Color,
    pub strikeout: bool,
    pub inverse: bool,
    pub hidden: bool,
}

/// One drawn cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    /// Zero-width combiners.
    pub extra: Option<Box<str>>,
    pub style: CellStyle,
    /// Occupies this and the next column (the spacer is omitted from rows).
    pub wide: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            extra: None,
            style: CellStyle::default(),
            wide: false,
        }
    }
}

/// Terminal modes that affect input encoding and drawing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modes {
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub bracketed_paste: bool,
    pub mouse_click: bool,
    pub mouse_drag: bool,
    pub mouse_motion: bool,
    pub sgr_mouse: bool,
    pub utf8_mouse: bool,
    pub focus_events: bool,
    pub alt_screen: bool,
    pub alternate_scroll: bool,
    pub show_cursor: bool,
    /// Kitty keyboard protocol flags currently set (protocol bit values).
    pub kitty_flags: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
    Hidden,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorView {
    pub row: u16,
    pub col: u16,
    pub shape: CursorShape,
    /// The app asked for a blinking cursor (DECSCUSR 1/3/5, `?12h`).
    pub blinking: bool,
}

/// Colours that answer OSC 4/10/11/12 queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
    pub cursor: (u8, u8, u8),
    pub ansi: [(u8, u8, u8); 16],
}

impl Palette {
    /// Colour for an alacritty colour index: 0..=15 ansi, 16..=255 the xterm
    /// 256-colour cube and gray ramp, 256/257/258 foreground/background/cursor
    /// (alacritty's `NamedColor` numbering); other indexes fall back to the
    /// foreground.
    pub fn rgb(&self, index: usize) -> (u8, u8, u8) {
        match index {
            0..=15 => self.ansi[index],
            16..=231 => {
                let i = index as u8 - 16;
                let level = |n: u8| if n == 0 { 0 } else { 55 + 40 * n };
                (level(i / 36), level(i / 6 % 6), level(i % 6))
            }
            232..=255 => {
                let v = 8 + 10 * (index as u8 - 232);
                (v, v, v)
            }
            257 => self.bg,
            258 => self.cursor,
            _ => self.fg,
        }
    }
}

pub(crate) fn modes(mode: TermMode) -> Modes {
    Modes {
        app_cursor: mode.contains(TermMode::APP_CURSOR),
        app_keypad: mode.contains(TermMode::APP_KEYPAD),
        bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        mouse_click: mode.contains(TermMode::MOUSE_REPORT_CLICK),
        mouse_drag: mode.contains(TermMode::MOUSE_DRAG),
        mouse_motion: mode.contains(TermMode::MOUSE_MOTION),
        sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
        utf8_mouse: mode.contains(TermMode::UTF8_MOUSE),
        focus_events: mode.contains(TermMode::FOCUS_IN_OUT),
        alt_screen: mode.contains(TermMode::ALT_SCREEN),
        alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
        show_cursor: mode.contains(TermMode::SHOW_CURSOR),
        kitty_flags: kitty_flags(mode),
    }
}

pub(crate) fn cursor_shape(shape: AnsiShape) -> CursorShape {
    match shape {
        AnsiShape::Block | AnsiShape::HollowBlock => CursorShape::Block,
        AnsiShape::Underline => CursorShape::Underline,
        AnsiShape::Beam => CursorShape::Beam,
        AnsiShape::Hidden => CursorShape::Hidden,
    }
}

/// Maps an alacritty colour. The foreground, background and other special
/// slots (cursor, bright/dim foreground) are `Default`; the dim ANSI variants
/// map to the plain colour 0..=7 they dim.
pub(crate) fn color(c: AnsiColor) -> Color {
    match c {
        AnsiColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(i) => Color::Indexed(i),
        AnsiColor::Named(n) => match n {
            NamedColor::DimBlack => Color::Named(0),
            NamedColor::DimRed => Color::Named(1),
            NamedColor::DimGreen => Color::Named(2),
            NamedColor::DimYellow => Color::Named(3),
            NamedColor::DimBlue => Color::Named(4),
            NamedColor::DimMagenta => Color::Named(5),
            NamedColor::DimCyan => Color::Named(6),
            NamedColor::DimWhite => Color::Named(7),
            n if (n as usize) < 16 => Color::Named(n as u8),
            _ => Color::Default,
        },
    }
}

/// The underline a cell's flags draw (the first set, if several are).
fn underline(flags: Flags) -> Underline {
    if !flags.intersects(Flags::ALL_UNDERLINES) {
        Underline::None
    } else if flags.contains(Flags::UNDERLINE) {
        Underline::Single
    } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
        Underline::Double
    } else if flags.contains(Flags::UNDERCURL) {
        Underline::Curly
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        Underline::Dotted
    } else {
        Underline::Dashed
    }
}

pub(crate) fn push_row(row: &Row<GridCell>, out: &mut Vec<Cell>) {
    for cell in &row[..] {
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let extra: Option<Box<str>> = cell
            .zerowidth()
            .filter(|z| !z.is_empty())
            .map(|z| z.iter().collect::<String>().into());
        out.push(Cell {
            ch: crate::history::glyph(cell),
            extra,
            style: CellStyle {
                fg: color(cell.fg),
                bg: color(cell.bg),
                bold: cell.flags.contains(Flags::BOLD),
                dim: cell.flags.contains(Flags::DIM),
                italic: cell.flags.contains(Flags::ITALIC),
                underline: underline(cell.flags),
                underline_color: cell.underline_color().map_or(Color::Default, color),
                strikeout: cell.flags.contains(Flags::STRIKEOUT),
                inverse: cell.flags.contains(Flags::INVERSE),
                hidden: cell.flags.contains(Flags::HIDDEN),
            },
            wide: cell.flags.contains(Flags::WIDE_CHAR),
        });
    }
}
