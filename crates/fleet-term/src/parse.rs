//! Parses a history line (see [`HistoryEvent::Line`](crate::HistoryEvent))
//! into cells.

use alacritty_terminal::vte::ansi::{Attr, Handler, Processor};
use unicode_width::UnicodeWidthChar;

use crate::view::{Cell, CellStyle, color};

struct LineParser<'a> {
    out: &'a mut Vec<Cell>,
    style: CellStyle,
}

impl Handler for LineParser<'_> {
    fn input(&mut self, c: char) {
        match c.width() {
            Some(0) => {
                if let Some(last) = self.out.last_mut() {
                    let mut s = last.extra.take().map(String::from).unwrap_or_default();
                    s.push(c);
                    last.extra = Some(s.into());
                }
            }
            width => self.out.push(Cell {
                ch: c,
                extra: None,
                style: self.style,
                wide: width == Some(2),
            }),
        }
    }

    fn terminal_attribute(&mut self, attr: Attr) {
        let s = &mut self.style;
        match attr {
            Attr::Reset => *s = CellStyle::default(),
            Attr::Bold => s.bold = true,
            Attr::Dim => s.dim = true,
            Attr::Italic => s.italic = true,
            Attr::Underline
            | Attr::DoubleUnderline
            | Attr::Undercurl
            | Attr::DottedUnderline
            | Attr::DashedUnderline => s.underline = true,
            Attr::Reverse => s.inverse = true,
            Attr::Hidden => s.hidden = true,
            Attr::Strike => s.strikeout = true,
            Attr::CancelBold => s.bold = false,
            Attr::CancelBoldDim => {
                s.bold = false;
                s.dim = false;
            }
            Attr::CancelItalic => s.italic = false,
            Attr::CancelUnderline => s.underline = false,
            Attr::CancelReverse => s.inverse = false,
            Attr::CancelHidden => s.hidden = false,
            Attr::CancelStrike => s.strikeout = false,
            Attr::Foreground(c) => s.fg = color(c),
            Attr::Background(c) => s.bg = color(c),
            Attr::BlinkSlow | Attr::BlinkFast | Attr::CancelBlink | Attr::UnderlineColor(_) => {}
        }
    }
}

/// Parses an ANSI history line into `out` (cleared first): SGR and text only,
/// any other escape is ignored; starts from the default style.
pub fn parse_history_line(line: &[u8], out: &mut Vec<Cell>) {
    out.clear();
    let mut parser = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
    let mut handler = LineParser {
        out,
        style: CellStyle::default(),
    };
    parser.advance(&mut handler, line);
}

/// The text of `cells` (base characters and combiners).
pub fn cells_text(cells: &[Cell]) -> String {
    let mut s = String::with_capacity(cells.len());
    for c in cells {
        s.push(c.ch);
        if let Some(extra) = &c.extra {
            s.push_str(extra);
        }
    }
    s
}
