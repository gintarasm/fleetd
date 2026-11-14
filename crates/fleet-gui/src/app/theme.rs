//! The dark and light palettes, chosen by the window appearance. The
//! terminal palette also answers OSC 4/10/11 queries (through
//! `Session::set_palette`).

use fleet_term::{Color, Palette};
use gpui::{App, Global, Hsla, Rgba, WindowAppearance};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub palette: Palette,
    /// Translucent fill drawn over selected cells.
    pub selection: Hsla,
    pub header_bg: Hsla,
    pub dim_text: Hsla,
    /// Background of the IME's marked (uncommitted) text.
    pub marked_bg: Hsla,
    /// Behind the sidebar and the pane cards (cards use `bg`).
    pub window_bg: Hsla,
    /// The one accent colour: the focused tab's bar, focus borders.
    pub accent: Hsla,
    /// 1 px card borders and separators.
    pub border: Hsla,
}

impl Global for Theme {}

const DARK: Palette = Palette {
    fg: (0xd8, 0xd8, 0xd8),
    bg: (0x1b, 0x1c, 0x1f),
    cursor: (0xd8, 0xd8, 0xd8),
    ansi: [
        (0x1b, 0x1c, 0x1f),
        (0xe0, 0x6c, 0x75),
        (0x98, 0xc3, 0x79),
        (0xe5, 0xc0, 0x7b),
        (0x61, 0xaf, 0xef),
        (0xc6, 0x78, 0xdd),
        (0x56, 0xb6, 0xc2),
        (0xc8, 0xcc, 0xd4),
        (0x5c, 0x63, 0x70),
        (0xf0, 0x7c, 0x85),
        (0xa8, 0xd3, 0x89),
        (0xf5, 0xd0, 0x8b),
        (0x71, 0xbf, 0xff),
        (0xd6, 0x88, 0xed),
        (0x66, 0xc6, 0xd2),
        (0xf0, 0xf0, 0xf0),
    ],
};

const LIGHT: Palette = Palette {
    fg: (0x24, 0x29, 0x2e),
    bg: (0xfb, 0xfb, 0xfa),
    cursor: (0x24, 0x29, 0x2e),
    ansi: [
        (0x24, 0x29, 0x2e),
        (0xc8, 0x28, 0x29),
        (0x3a, 0x86, 0x2c),
        (0x9a, 0x6a, 0x00),
        (0x20, 0x5e, 0xc4),
        (0x8e, 0x3c, 0xa8),
        (0x10, 0x80, 0x8c),
        (0xbc, 0xbc, 0xbc),
        (0x6a, 0x73, 0x7d),
        (0xe0, 0x40, 0x40),
        (0x4c, 0xa0, 0x3c),
        (0xb8, 0x84, 0x10),
        (0x3a, 0x78, 0xe0),
        (0xa8, 0x54, 0xc4),
        (0x20, 0x9a, 0xa6),
        (0xe8, 0xe8, 0xe8),
    ],
};

impl Theme {
    pub fn dark() -> Self {
        Theme {
            palette: DARK,
            selection: Hsla {
                h: 0.6,
                s: 0.6,
                l: 0.55,
                a: 0.35,
            },
            header_bg: rgb((0x25, 0x26, 0x2a)),
            dim_text: rgb((0x8a, 0x8f, 0x98)),
            marked_bg: rgb((0x3a, 0x3d, 0x44)),
            window_bg: rgb((0x13, 0x14, 0x16)),
            accent: rgb((0x61, 0xaf, 0xef)),
            border: rgb((0x2c, 0x2e, 0x33)),
        }
    }

    pub fn light() -> Self {
        Theme {
            palette: LIGHT,
            selection: Hsla {
                h: 0.6,
                s: 0.7,
                l: 0.55,
                a: 0.28,
            },
            header_bg: rgb((0xec, 0xec, 0xea)),
            dim_text: rgb((0x6a, 0x73, 0x7d)),
            marked_bg: rgb((0xdd, 0xe4, 0xee)),
            window_bg: rgb((0xe9, 0xe9, 0xe7)),
            accent: rgb((0x20, 0x5e, 0xc4)),
            border: rgb((0xd6, 0xd6, 0xd3)),
        }
    }

    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Theme::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Theme::light(),
        }
    }

    pub fn fg(&self) -> Hsla {
        rgb(self.palette.fg)
    }

    pub fn bg(&self) -> Hsla {
        rgb(self.palette.bg)
    }

    pub fn cursor(&self) -> Hsla {
        rgb(self.palette.cursor)
    }

    /// A cell colour; `Default` is the foreground or the background, by slot.
    pub fn resolve(&self, color: Color, foreground: bool) -> Hsla {
        match color {
            Color::Default if foreground => self.fg(),
            Color::Default => self.bg(),
            Color::Named(n) | Color::Indexed(n) => rgb(self.palette.rgb(usize::from(n))),
            Color::Rgb(r, g, b) => rgb((r, g, b)),
        }
    }
}

/// The current theme (dark until the window sets one).
pub fn current(cx: &App) -> Theme {
    cx.try_global::<Theme>()
        .copied()
        .unwrap_or_else(Theme::dark)
}

pub fn rgb((r, g, b): (u8, u8, u8)) -> Hsla {
    Rgba {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: 1.0,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::Theme;
    use fleet_term::Color;
    use gpui::WindowAppearance;

    #[test]
    fn appearance_picks_palette() {
        assert_eq!(
            Theme::for_appearance(WindowAppearance::VibrantDark).palette,
            Theme::dark().palette
        );
        assert_eq!(
            Theme::for_appearance(WindowAppearance::Dark).palette,
            Theme::dark().palette
        );
        assert_eq!(
            Theme::for_appearance(WindowAppearance::Light).palette,
            Theme::light().palette
        );
        assert_eq!(
            Theme::for_appearance(WindowAppearance::VibrantLight).palette,
            Theme::light().palette
        );
    }

    #[test]
    fn default_colour_follows_slot() {
        let t = Theme::dark();
        assert_eq!(t.resolve(Color::Default, true), t.fg());
        assert_eq!(t.resolve(Color::Default, false), t.bg());
        assert_eq!(
            t.resolve(Color::Named(1), true),
            super::rgb(t.palette.ansi[1])
        );
    }
}
