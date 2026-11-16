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
    /// Over find matches, and over the current one.
    pub find_match: Rgba,
    pub find_current: Rgba,
    pub danger: Rgba,
    pub success: Rgba,
    /// An agent that needs you (the system orange, accessible variant).
    pub warning: Rgba,
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
        (0x2f, 0x7d, 0x24),
        (0x8a, 0x5f, 0x00),
        (0x20, 0x5e, 0xc4),
        (0x8e, 0x3c, 0xa8),
        (0x0c, 0x72, 0x7d),
        (0xbc, 0xbc, 0xbc),
        (0x6a, 0x73, 0x7d),
        (0xd0, 0x38, 0x38),
        (0x3e, 0x83, 0x31),
        (0x94, 0x69, 0x0c),
        (0x37, 0x72, 0xd5),
        (0xa4, 0x52, 0xbf),
        (0x1b, 0x80, 0x8a),
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
            find_match: rgba(0xffd60a, 0.28),
            find_current: rgba(0xff9f0a, 0.6),
            danger: rgb(0xff6961),
            success: rgb(0x32d74b),
            warning: rgb(0xffb340),
            shadow: rgba(0x000000, 0.45),
        }
    }

    pub fn light() -> Tokens {
        Tokens {
            appearance: Appearance::Light,
            palette: LIGHT_PALETTE,
            window: rgba(0xf0f0f2, 0.94),
            card: Rgba::from_palette(LIGHT_PALETTE.bg, 0.98),
            raised: rgba(0xffffff, 0.98),
            field: rgba(0xffffff, 0.85),
            hover: rgba(0x000000, 0.05),
            pressed: rgba(0x000000, 0.09),
            selected: rgba(0x000000, 0.07),
            border: rgba(0x000000, 0.12),
            separator: rgba(0x000000, 0.08),
            text: rgb(0x1d1d1f),
            secondary: rgb(0x48484e),
            tertiary: rgb(0x5a5a5e),
            accent: rgb(0x007aff),
            selection: rgba(0x007aff, 0.22),
            marked: rgb(0xdde4ee),
            find_match: rgba(0xffcc00, 0.35),
            find_current: rgba(0xff9500, 0.55),
            danger: rgb(0xd70015),
            success: rgb(0x23873c),
            warning: rgb(0xc93400),
            shadow: rgba(0x000000, 0.18),
        }
        .with_accent(rgb(0x007aff))
    }

    /// These tokens in the user's accent colour: the accent, and the
    /// selection tint in its hue at the selection's own alpha.
    pub fn with_accent(self, accent: Rgba) -> Tokens {
        let accent = self.legible_accent(accent);
        Tokens {
            accent,
            selection: Rgba {
                a: self.selection.a,
                ..accent
            },
            ..self
        }
    }

    /// `accent` darkened, keeping its hue, just enough to read as text
    /// (WCAG AA, 4.5:1) on every light surface: the bar's buttons are
    /// labelled in it and the white label of a primary button sits on it.
    /// The system's light accents (blue, yellow, pink...) are too pale on
    /// a light grey. Dark mode keeps the accent as it is.
    pub fn legible_accent(&self, accent: Rgba) -> Rgba {
        if self.appearance == Appearance::Dark {
            return accent;
        }
        let surfaces = self.light_text_surfaces();
        let reads = |c: Rgba| surfaces.iter().all(|&s| contrast(c, s) >= 4.5);
        let scaled = |k: f32| {
            let s = |v: u8| (f32::from(v) * k).round() as u8;
            Rgba {
                r: s(accent.r),
                g: s(accent.g),
                b: s(accent.b),
                a: 1.0,
            }
        };
        let solid = Rgba { a: 1.0, ..accent };
        if reads(solid) {
            return solid;
        }
        // The largest scale that reads (black always does).
        let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
        for _ in 0..24 {
            let mid = (lo + hi) / 2.0;
            if reads(scaled(mid)) {
                lo = mid
            } else {
                hi = mid
            }
        }
        scaled(lo)
    }

    /// Every opaque colour text is drawn on in light, over a white and a
    /// black desktop: the window tint bare, hovered and selected (the
    /// sidebar's rows), the cards, the sheets and their fields.
    fn light_text_surfaces(&self) -> Vec<Rgba> {
        [rgb(0xffffff), rgb(0x000000)]
            .into_iter()
            .flat_map(|desktop| {
                let w = self.window.over(desktop);
                let raised = self.raised.over(w);
                [
                    w,
                    self.hover.over(w),
                    self.selected.over(w),
                    self.card.over(w),
                    raised,
                    self.field.over(raised),
                ]
            })
            .collect()
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

    /// The light window's surfaces text sits on, over either desktop:
    /// the bare tint, and a sidebar row hovered or selected on it.
    fn light_window_surfaces(t: &Tokens) -> Vec<Rgba> {
        backdrops(t.window)
            .into_iter()
            .flat_map(|w| [w, t.hover.over(w), t.selected.over(w)])
            .collect()
    }

    /// Cards, sheets and the field wells on sheets, over either desktop.
    fn light_raised_surfaces(t: &Tokens) -> Vec<Rgba> {
        backdrops(t.window)
            .into_iter()
            .flat_map(|w| {
                let raised = t.raised.over(w);
                [t.card.over(w), raised, t.field.over(raised)]
            })
            .collect()
    }

    fn min_contrast(fg: Rgba, surfaces: &[Rgba]) -> f64 {
        surfaces
            .iter()
            .map(|&s| contrast(fg, s))
            .fold(f64::INFINITY, f64::min)
    }

    /// WCAG AA for text (4.5:1) everywhere in light, and 3:1 for marks
    /// and outlines that carry meaning (status dots, the focus ring).
    fn check_light_aa(t: &Tokens) {
        let window = light_window_surfaces(t);
        let raised = light_raised_surfaces(t);
        let all: Vec<Rgba> = window.iter().chain(&raised).copied().collect();
        for (name, fg) in [
            ("text", t.text),
            ("secondary", t.secondary),
            ("tertiary", t.tertiary),
        ] {
            let c = min_contrast(fg, &all);
            assert!(c >= 4.5, "light {name} text {c:.2}");
        }
        // Each level stays a step apart from the next.
        assert!(t.secondary.luminance() > t.text.luminance() * 1.5);
        assert!(t.tertiary.luminance() > t.secondary.luminance() * 1.2);
        // The accent labels the bar's buttons and draws the focus ring.
        let c = min_contrast(t.accent, &all);
        assert!(c >= 4.5, "light accent text {c:.2}");
        let c = contrast(WHITE, t.accent);
        assert!(c >= 4.5, "white on a light primary button {c:.2}");
        // Errors are written on sheets; danger outlines the toast.
        let c = min_contrast(t.danger, &raised);
        assert!(c >= 4.5, "light danger text {c:.2}");
        let c = min_contrast(t.danger, &window);
        assert!(c >= 3.0, "light danger outline {c:.2}");
        let c = min_contrast(t.success, &window);
        assert!(c >= 3.0, "light running dot {c:.2}");
    }

    #[test]
    fn light_text_meets_wcag_aa_on_every_surface() {
        check_light_aa(&Tokens::light());
        check_light_aa(&Tokens::light().opaque());
    }

    #[test]
    fn light_terminal_colours_meet_wcag_aa() {
        let t = Tokens::light();
        let cards: Vec<Rgba> = backdrops(t.window)
            .into_iter()
            .map(|w| t.card.over(w))
            .collect();
        let p = t.palette;
        let fg = Rgba::from_palette(p.fg, 1.0);
        assert!(min_contrast(fg, &cards) >= 7.0);
        // 7 and 15 (white, bright white) are background colours on a
        // light terminal, as in Terminal.app's light profiles; every
        // other colour is text.
        for i in (0..16).filter(|&i| i != 7 && i != 15) {
            let c = min_contrast(Rgba::from_palette(p.ansi[i], 1.0), &cards);
            assert!(c >= 4.5, "light ansi {i} {c:.2}");
        }
        // Black text on a white background stays readable.
        let white = Rgba::from_palette(p.ansi[7], 1.0);
        assert!(contrast(Rgba::from_palette(p.ansi[0], 1.0), white) >= 4.5);
        // Selected text: the foreground stays AAA, colours stay visible.
        for card in cards {
            let sel = t.selection.over(card);
            // Still visibly a selection.
            assert!(contrast(sel, card) >= 1.3, "light selection {sel:?}");
            let c = contrast(fg, sel);
            assert!(c >= 7.0, "light selected text {c:.2}");
            for i in (0..16).filter(|&i| i != 7 && i != 15) {
                let c = contrast(Rgba::from_palette(p.ansi[i], 1.0), sel);
                assert!(c >= 3.0, "light selected ansi {i} {c:.2}");
            }
        }
    }

    #[test]
    fn light_accent_is_darkened_until_it_reads_as_text() {
        let t = Tokens::light();
        let all: Vec<Rgba> = light_window_surfaces(&t)
            .into_iter()
            .chain(light_raised_surfaces(&t))
            .collect();
        // The system's yellow and blue: too light as text on light grey.
        for user in [rgb(0xffc600), rgb(0x007aff), rgb(0xf74f9e)] {
            let accent = t.legible_accent(user);
            let c = min_contrast(accent, &all);
            assert!(c >= 4.5, "{user:?} -> {accent:?} {c:.2}");
            // As little as needed: a step lighter would fail.
            let lighter = Rgba {
                r: accent.r.saturating_add(3),
                g: accent.g.saturating_add(3),
                b: accent.b.saturating_add(3),
                a: 1.0,
            };
            assert!(min_contrast(lighter, &all) < 4.6, "{user:?} -> {accent:?}");
            // Same hue: the channels keep their order.
            let order = |c: Rgba| {
                let mut v = [(c.r, 0), (c.g, 1), (c.b, 2)];
                v.sort();
                v.map(|(_, i)| i)
            };
            assert_eq!(order(accent), order(user), "{user:?} -> {accent:?}");
        }
        // Dark enough already: kept.
        assert_eq!(t.legible_accent(rgb(0x1e40af)), rgb(0x1e40af));
        // Dark mode keeps the user's accent as it is.
        assert_eq!(Tokens::dark().legible_accent(rgb(0xffc600)), rgb(0xffc600));
    }

    #[test]
    fn with_accent_sets_the_accent_and_tints_the_selection() {
        let t = Tokens::light().with_accent(rgb(0xffc600));
        assert_eq!(t.accent, Tokens::light().legible_accent(rgb(0xffc600)));
        assert_eq!((t.selection.r, t.selection.g, t.selection.b), {
            let a = t.accent;
            (a.r, a.g, a.b)
        });
        assert_eq!(t.selection.a, Tokens::light().selection.a);
        check_light_aa(&t);
        let d = Tokens::dark().with_accent(rgb(0xff0000));
        assert_eq!(d.accent, rgb(0xff0000));
        assert_eq!(d.selection.a, Tokens::dark().selection.a);
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
