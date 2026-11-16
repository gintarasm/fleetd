//! The dark and light themes, chosen by the window appearance: the core's
//! colour tokens ([`Tokens`]) as gpui colours. The terminal palette also
//! answers OSC 4/10/11 queries (through `Session::set_palette`).

use fleet_term::{Color, Palette};
use gpui::{App, FontWeight, Global, Hsla, Rgba as GpuiRgba, Styled, WindowAppearance, px};

use crate::core::style::TextStyle;
use crate::core::theme::{Appearance, Rgba, Tokens};

/// Every colour of [`Tokens`], ready to draw with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    /// What this theme was made from (for [`Theme::with_accent`]).
    tokens: Tokens,
    pub appearance: Appearance,
    pub palette: Palette,
    pub window: Hsla,
    pub card: Hsla,
    pub raised: Hsla,
    pub field: Hsla,
    pub hover: Hsla,
    pub pressed: Hsla,
    pub selected: Hsla,
    pub border: Hsla,
    pub separator: Hsla,
    pub text: Hsla,
    pub secondary: Hsla,
    pub tertiary: Hsla,
    pub accent: Hsla,
    /// Translucent fill drawn over selected cells.
    pub selection: Hsla,
    /// Background of the IME's marked (uncommitted) text.
    pub marked: Hsla,
    pub danger: Hsla,
    pub success: Hsla,
    pub shadow: Hsla,
}

impl Global for Theme {}

impl Theme {
    pub fn from_tokens(t: &Tokens) -> Self {
        Theme {
            tokens: *t,
            appearance: t.appearance,
            palette: t.palette,
            window: hsla(t.window),
            card: hsla(t.card),
            raised: hsla(t.raised),
            field: hsla(t.field),
            hover: hsla(t.hover),
            pressed: hsla(t.pressed),
            selected: hsla(t.selected),
            border: hsla(t.border),
            separator: hsla(t.separator),
            text: hsla(t.text),
            secondary: hsla(t.secondary),
            tertiary: hsla(t.tertiary),
            accent: hsla(t.accent),
            selection: hsla(t.selection),
            marked: hsla(t.marked),
            danger: hsla(t.danger),
            success: hsla(t.success),
            shadow: hsla(t.shadow),
        }
    }

    pub fn dark() -> Self {
        Theme::from_tokens(&Tokens::dark())
    }

    pub fn light() -> Self {
        Theme::from_tokens(&Tokens::light())
    }

    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        Theme::from_tokens(&Tokens::for_appearance(appearance_of(appearance)))
    }

    /// For an opaque window (Reduce Transparency).
    pub fn opaque_for_appearance(appearance: WindowAppearance) -> Self {
        Theme::from_tokens(&Tokens::for_appearance(appearance_of(appearance)).opaque())
    }

    /// This theme with `accent` (the user's system accent colour) for
    /// the accent and the selection tint; in light, darkened until it
    /// reads as text ([`Tokens::legible_accent`]).
    pub fn with_accent(self, accent: Hsla) -> Self {
        let c = accent.to_rgb();
        let u8_of = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        let accent = Rgba {
            r: u8_of(c.r),
            g: u8_of(c.g),
            b: u8_of(c.b),
            a: 1.0,
        };
        Theme::from_tokens(&self.tokens.with_accent(accent))
    }

    pub fn is_dark(&self) -> bool {
        self.appearance == Appearance::Dark
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

/// The core's appearance for gpui's.
pub fn appearance_of(appearance: WindowAppearance) -> Appearance {
    match appearance {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => Appearance::Dark,
        WindowAppearance::Light | WindowAppearance::VibrantLight => Appearance::Light,
    }
}

/// The current theme (dark until the window sets one).
pub fn current(cx: &App) -> Theme {
    cx.try_global::<Theme>()
        .copied()
        .unwrap_or_else(Theme::dark)
}

pub fn rgb((r, g, b): (u8, u8, u8)) -> Hsla {
    hsla(Rgba { r, g, b, a: 1.0 })
}

pub fn hsla(c: Rgba) -> Hsla {
    GpuiRgba {
        r: f32::from(c.r) / 255.0,
        g: f32::from(c.g) / 255.0,
        b: f32::from(c.b) / 255.0,
        a: c.a,
    }
    .into()
}

/// Sets a div's text to a type-scale step (in the window's font, the
/// system UI font: SF Pro).
pub trait Typography: Styled + Sized {
    fn type_style(self, style: TextStyle) -> Self {
        self.text_size(px(style.size))
            .line_height(px(style.line_height))
            .font_weight(FontWeight(style.weight.value()))
    }
}

impl<T: Styled> Typography for T {}

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

    #[test]
    fn accent_also_tints_the_selection() {
        let accent = super::rgb((0xff, 0x00, 0x00));
        let t = Theme::dark().with_accent(accent);
        assert_eq!(t.accent, accent);
        assert_eq!(t.selection.h, accent.h);
        assert_eq!(t.selection.a, Theme::dark().selection.a);
    }

    #[test]
    fn a_pale_accent_is_darkened_in_light() {
        use crate::core::theme::{Tokens, rgb};
        let yellow = super::rgb((0xff, 0xc6, 0x00));
        let t = Theme::light().with_accent(yellow);
        let want = Tokens::light().legible_accent(rgb(0xffc600));
        assert_eq!(t.accent, super::hsla(want));
        assert_ne!(t.accent, yellow);
        // Only the accent and the selection change.
        assert_eq!(t.text, Theme::light().text);
        assert_eq!(t.window, Theme::light().window);
    }
}
