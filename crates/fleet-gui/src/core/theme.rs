//! Colour tokens for the dark and light appearance: the terminal palette
//! (it also answers OSC 4/10/11 queries) and the window's surfaces, text
//! and accents. The window is translucent: `window` tints the blurred
//! desktop behind it, the sidebar and the bar draw on that tint, and the
//! pane cards (`card`, the palette's background) float over it.

use fleet_term::Palette;

/// An sRGB colour with straight (not premultiplied) alpha.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

/// An opaque colour from `0xRRGGBB`.
pub const fn rgb(hex: u32) -> Rgba {
    rgba(hex, 1.0)
}

/// `0xRRGGBB` at alpha `a`.
pub const fn rgba(hex: u32, a: f32) -> Rgba {
    Rgba {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
        a,
    }
}

impl Rgba {
    pub const fn from_palette((r, g, b): (u8, u8, u8), a: f32) -> Rgba {
        Rgba { r, g, b, a }
    }

    /// This colour drawn over the opaque `below`.
    pub fn over(self, below: Rgba) -> Rgba {
        let mix = |top: u8, bottom: u8| {
            let v = f32::from(top) * self.a + f32::from(bottom) * (1.0 - self.a);
            v.round().clamp(0.0, 255.0) as u8
        };
        Rgba {
            r: mix(self.r, below.r),
            g: mix(self.g, below.g),
            b: mix(self.b, below.b),
            a: 1.0,
        }
    }

    /// WCAG relative luminance (alpha ignored).
    pub fn luminance(self) -> f64 {
        let lin = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }
}

/// WCAG contrast ratio of two opaque colours (1 to 21).
pub fn contrast(a: Rgba, b: Rgba) -> f64 {
    let (la, lb) = (a.luminance(), b.luminance());
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// The system appearance the window follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

/// Every colour the shell draws with, for one appearance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tokens {
    pub appearance: Appearance,
    pub palette: Palette,
    /// The tint over the blurred desktop: behind the bar and the sidebar.
    pub window: Rgba,
    /// The pane cards (the palette's background, nearly opaque).
    pub card: Rgba,
    /// Sheets, the error toast, tooltips: above the cards.
    pub raised: Rgba,
    /// Text field wells.
    pub field: Rgba,
    /// Fills under the pointer, while pressed, and of the focused tab.
    pub hover: Rgba,
    pub pressed: Rgba,
    pub selected: Rgba,
    /// Card and control outlines; thin rules.
    pub border: Rgba,
    pub separator: Rgba,
    /// Primary, secondary (paths, second lines) and tertiary (hints,
    /// placeholders, disabled) text.
    pub text: Rgba,
    pub secondary: Rgba,
    pub tertiary: Rgba,
    /// The system blue; replaced by the user's accent colour where the
    /// platform tells it.
    pub accent: Rgba,
    /// Over selected terminal cells.
    pub selection: Rgba,
    /// Behind the IME's marked text.
    pub marked: Rgba,
    pub danger: Rgba,
    pub success: Rgba,
    pub shadow: Rgba,
}

pub const DARK_PALETTE: Palette = Palette {
    fg: (0xdc, 0xdc, 0xde),
    bg: (0x1f, 0x20, 0x24),
    cursor: (0xdc, 0xdc, 0xde),
    ansi: [
        (0x1f, 0x20, 0x24),
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

pub const LIGHT_PALETTE: Palette = Palette {
    fg: (0x24, 0x29, 0x2e),
    bg: (0xfd, 0xfd, 0xfc),
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

impl Tokens {
    pub fn for_appearance(appearance: Appearance) -> Tokens {
        match appearance {
            Appearance::Dark => Tokens::dark(),
            Appearance::Light => Tokens::light(),
        }
    }

    /// These tokens with the window tint at alpha `a` (`[gui]
    /// window_opacity`), kept within the range text stays readable in.
    pub fn with_window_opacity(self, a: f32) -> Tokens {
        use super::settings::{OPACITY_MAX, OPACITY_MIN};
        Tokens {
            window: Rgba {
                a: a.clamp(OPACITY_MIN, OPACITY_MAX),
                ..self.window
            },
            ..self
        }
    }

    /// These tokens for an opaque window (Reduce Transparency is on): the
    /// window tint as it looks over a plain backdrop, and the cards and
    /// sheets as they look over that.
    pub fn opaque(self) -> Tokens {
        let backdrop = match self.appearance {
            Appearance::Dark => rgb(0x000000),
            Appearance::Light => rgb(0xffffff),
        };
        let window = self.window.over(backdrop);
        Tokens {
            window,
            card: self.card.over(window),
            raised: self.raised.over(window),
            ..self
        }
    }

    pub fn dark() -> Tokens {
        Tokens {
            appearance: Appearance::Dark,
            palette: DARK_PALETTE,
            window: rgba(0x141518, 0.84),
            card: Rgba::from_palette(DARK_PALETTE.bg, 0.97),
            raised: rgba(0x2a2b30, 0.98),
            field: rgba(0x000000, 0.28),
            hover: rgba(0xffffff, 0.06),
            pressed: rgba(0xffffff, 0.11),
            selected: rgba(0xffffff, 0.09),
            border: rgba(0xffffff, 0.09),
            separator: rgba(0xffffff, 0.06),
            text: rgb(0xe8e8ea),
            secondary: rgb(0x9fa1a8),
            tertiary: rgb(0x6c6f77),
            accent: rgb(0x0a84ff),
            selection: rgba(0x3f8cf0, 0.35),
            marked: rgb(0x3a3d44),
            danger: rgb(0xff6961),
            success: rgb(0x32d74b),
            shadow: rgba(0x000000, 0.45),
        }
    }

    pub fn light() -> Tokens {
        Tokens {
            appearance: Appearance::Light,
            palette: LIGHT_PALETTE,
            window: rgba(0xeeeeee, 0.84),
            card: Rgba::from_palette(LIGHT_PALETTE.bg, 0.98),
            raised: rgba(0xffffff, 0.98),
            field: rgba(0xffffff, 0.85),
            hover: rgba(0x000000, 0.05),
            pressed: rgba(0x000000, 0.09),
            selected: rgba(0x000000, 0.07),
            border: rgba(0x000000, 0.12),
            separator: rgba(0x000000, 0.08),
            text: rgb(0x1d1d1f),
            secondary: rgb(0x5f5f66),
            tertiary: rgb(0x8e8e94),
            accent: rgb(0x007aff),
            selection: rgba(0x3a78e0, 0.28),
            marked: rgb(0xdde4ee),
            danger: rgb(0xd70015),
            success: rgb(0x248a3d),
            shadow: rgba(0x000000, 0.18),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Appearance, Rgba, Tokens, contrast, rgb};

    const WHITE: Rgba = rgb(0xffffff);
    const BLACK: Rgba = rgb(0x000000);

    /// What a surface looks like over the brightest and darkest desktop
    /// the blur can show.
    fn backdrops(surface: Rgba) -> [Rgba; 2] {
        [surface.over(WHITE), surface.over(BLACK)]
    }

    fn check(t: &Tokens) {
        let name = format!("{:?}", t.appearance);
        // The bar and the sidebar: text on the window tint.
        for bg in backdrops(t.window) {
            let c = contrast(t.text, bg);
            assert!(c >= 4.5, "{name}: text on the window {c:.2}");
            let c = contrast(t.secondary, bg);
            assert!(c >= 3.0, "{name}: secondary text on the window {c:.2}");
        }
        // The cards: over the window over either desktop.
        for below in backdrops(t.window) {
            let card = t.card.over(below);
            let c = contrast(t.text, card);
            assert!(c >= 7.0, "{name}: text on a card {c:.2}");
            let c = contrast(t.secondary, card);
            assert!(c >= 4.5, "{name}: secondary text on a card {c:.2}");
            let c = contrast(t.tertiary, card);
            assert!(c >= 2.5, "{name}: tertiary text on a card {c:.2}");
            let fg = Rgba::from_palette(t.palette.fg, 1.0);
            let c = contrast(fg, card);
            assert!(c >= 7.0, "{name}: terminal text on a card {c:.2}");
        }
        // Sheets.
        for below in backdrops(t.window) {
            let raised = t.raised.over(below);
            assert!(contrast(t.text, raised) >= 7.0, "{name}: text on a sheet");
        }
        assert!(contrast(t.danger, t.raised.over(t.window.over(BLACK))) >= 3.0);
    }

    #[test]
    fn text_is_readable_over_any_desktop() {
        check(&Tokens::dark());
        check(&Tokens::light());
    }

    #[test]
    fn the_window_is_translucent_and_cards_nearly_opaque() {
        for t in [Tokens::dark(), Tokens::light()] {
            assert!(t.window.a < 1.0 && t.window.a >= 0.7, "{:?}", t.appearance);
            assert!(t.card.a >= 0.95, "{:?}", t.appearance);
            // Cards are the terminal's own background, so a program that
            // paints the default background blends in.
            let p = t.palette.bg;
            assert_eq!((t.card.r, t.card.g, t.card.b), p);
        }
    }

    #[test]
    fn opaque_tokens_keep_the_look_without_alpha() {
        for t in [Tokens::dark(), Tokens::light()] {
            let o = t.opaque();
            assert_eq!(o.window.a, 1.0);
            assert_eq!(o.card.a, 1.0);
            assert_eq!(o.raised.a, 1.0);
            assert!(contrast(o.text, o.window) >= 4.5);
            assert!(contrast(o.text, o.card) >= 7.0);
            // Only the surfaces change.
            assert_eq!(o.text, t.text);
            assert_eq!(o.palette, t.palette);
        }
    }

    #[test]
    fn text_stays_readable_across_the_window_opacity_range() {
        use crate::core::settings::{OPACITY_MAX, OPACITY_MIN};
        for t in [Tokens::dark(), Tokens::light()] {
            for a in [OPACITY_MIN, 0.84, OPACITY_MAX] {
                let o = t.with_window_opacity(a);
                assert_eq!(o.window.a, a);
                check(&o);
            }
            assert_eq!(t.with_window_opacity(0.1).window.a, OPACITY_MIN);
            assert_eq!(t.with_window_opacity(0.84), t);
        }
    }

    #[test]
    fn appearance_picks_tokens() {
        assert_eq!(Tokens::for_appearance(Appearance::Dark), Tokens::dark());
        assert_eq!(Tokens::for_appearance(Appearance::Light), Tokens::light());
    }

    #[test]
    fn compositing_and_contrast() {
        let half = super::rgba(0xffffff, 0.5);
        assert_eq!(half.over(BLACK), rgb(0x808080));
        assert!((contrast(WHITE, BLACK) - 21.0).abs() < 1e-9);
        assert!((contrast(WHITE, WHITE) - 1.0).abs() < 1e-9);
        assert_eq!(
            super::rgb(0x0a84ff),
            Rgba {
                r: 0x0a,
                g: 0x84,
                b: 0xff,
                a: 1.0
            }
        );
    }
}
