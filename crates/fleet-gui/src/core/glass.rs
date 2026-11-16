//! Liquid glass (macOS 26's `NSGlassEffectView`, ADR 0049): which backdrop
//! the workspace window gets for `[gui] glass`, the system and the
//! accessibility settings, and where the glass panes go. The shell
//! (`app::native`) places the views; this decides.

use serde::Deserialize;

use super::style::chrome;
use super::theme::{Rgba, Tokens};

/// `[gui] glass`: liquid glass behind the window, or behind its chrome
/// only, or none (today's blur).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GlassChoice {
    /// Today's blur under the theme's tint (ADR 0031).
    #[default]
    Off,
    /// One glass pane behind all of the gpui content.
    Window,
    /// Glass behind the bar and the sidebar; the panes keep the blur.
    Chrome,
}

impl GlassChoice {
    pub const ALL: [GlassChoice; 3] = [GlassChoice::Off, GlassChoice::Window, GlassChoice::Chrome];

    pub fn title(self) -> &'static str {
        match self {
            GlassChoice::Off => "Off",
            GlassChoice::Window => "Window",
            GlassChoice::Chrome => "Sidebar and bar",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            GlassChoice::Off => "off",
            GlassChoice::Window => "window",
            GlassChoice::Chrome => "chrome",
        }
    }
}

/// What the workspace window draws behind the gpui content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backdrop {
    /// Reduce Transparency: a plain window, the tokens flattened.
    Opaque,
    /// gpui's blurred backdrop under the theme's tint (ADR 0031).
    Blur,
    /// A transparent window over one glass pane; gpui draws no tint.
    GlassWindow,
    /// The blurred backdrop, with glass panes over it behind the bar and
    /// the sidebar; gpui tints only the panes' area.
    GlassChrome,
}

impl Backdrop {
    /// The backdrop for the setting, given whether this macOS has
    /// `NSGlassEffectView` (26 and later) and whether Reduce Transparency
    /// is on. Reduce Transparency wins over everything; without glass the
    /// setting falls back to the blur.
    pub fn resolve(choice: GlassChoice, glass_available: bool, reduce_transparency: bool) -> Self {
        match choice {
            _ if reduce_transparency => Backdrop::Opaque,
            GlassChoice::Off => Backdrop::Blur,
            _ if !glass_available => Backdrop::Blur,
            GlassChoice::Window => Backdrop::GlassWindow,
            GlassChoice::Chrome => Backdrop::GlassChrome,
        }
    }

    /// The window itself is translucent with gpui's blurred backdrop
    /// (`WindowBackgroundAppearance::Blurred`), which the blur radius
    /// setting then applies to.
    pub fn blurred(self) -> bool {
        matches!(self, Backdrop::Blur | Backdrop::GlassChrome)
    }

    /// The glass panes to place, back to front.
    pub fn regions(self) -> &'static [GlassRegion] {
        match self {
            Backdrop::Opaque | Backdrop::Blur => &[],
            Backdrop::GlassWindow => &[GlassRegion::Window],
            Backdrop::GlassChrome => &[GlassRegion::Bar, GlassRegion::Sidebar],
        }
    }

    /// Where gpui paints the theme's window tint: under glass it does not
    /// (the glass carries the tint), elsewhere it does.
    pub fn tint(self) -> Tint {
        match self {
            Backdrop::Opaque | Backdrop::Blur => Tint {
                chrome: true,
                content: true,
            },
            Backdrop::GlassWindow => Tint {
                chrome: false,
                content: false,
            },
            Backdrop::GlassChrome => Tint {
                chrome: false,
                content: true,
            },
        }
    }
}

/// How much of the window tint's alpha the glass takes as its own tint.
/// Glass tints its content itself and keeps its edges and highlights
/// readable, so a full-strength tint would only turn it into a flat fill.
pub const GLASS_TINT_SHARE: f32 = 0.5;

/// The glass panes' tint: the theme's window colour, lighter.
pub fn glass_tint(tokens: &Tokens) -> Rgba {
    Rgba {
        a: tokens.window.a * GLASS_TINT_SHARE,
        ..tokens.window
    }
}

/// Which parts of the window gpui paints the theme's tint behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tint {
    /// The bar and the sidebar.
    pub chrome: bool,
    /// The panes' area.
    pub content: bool,
}

impl Tint {
    /// One fill on the root covers it all.
    pub fn everywhere(self) -> bool {
        self.chrome && self.content
    }
}

/// A rectangle in window points, from the top-left corner of the content.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// One glass pane. Its frame follows the window's fixed chrome metrics
/// (`style::chrome`), so the views resize with the window by their
/// autoresizing masks alone; nothing is moved per frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlassRegion {
    Window,
    Bar,
    Sidebar,
}

impl GlassRegion {
    /// The view's identifier, to find it again.
    pub fn id(self) -> &'static str {
        match self {
            GlassRegion::Window => "fleet.glass.window",
            GlassRegion::Bar => "fleet.glass.bar",
            GlassRegion::Sidebar => "fleet.glass.sidebar",
        }
    }

    /// Its frame in a content area `width` × `height` points.
    pub fn frame(self, width: f32, height: f32) -> Frame {
        let bar = chrome::BAR_HEIGHT.min(height);
        match self {
            GlassRegion::Window => Frame {
                x: 0.0,
                y: 0.0,
                w: width,
                h: height,
            },
            GlassRegion::Bar => Frame {
                x: 0.0,
                y: 0.0,
                w: width,
                h: bar,
            },
            GlassRegion::Sidebar => Frame {
                x: 0.0,
                y: bar,
                w: chrome::SIDEBAR_WIDTH.min(width),
                h: height - bar,
            },
        }
    }

    /// How it follows a resize: (width grows, height grows). A pane that
    /// does not grow in a direction keeps its distance to the top-left.
    pub fn grows(self) -> (bool, bool) {
        match self {
            GlassRegion::Window => (true, true),
            GlassRegion::Bar => (true, false),
            GlassRegion::Sidebar => (false, true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::theme::Appearance;

    #[test]
    fn reduce_transparency_wins_over_every_choice() {
        for choice in GlassChoice::ALL {
            for available in [false, true] {
                assert_eq!(Backdrop::resolve(choice, available, true), Backdrop::Opaque);
            }
        }
    }

    #[test]
    fn without_glass_every_choice_falls_back_to_the_blur() {
        for choice in GlassChoice::ALL {
            assert_eq!(Backdrop::resolve(choice, false, false), Backdrop::Blur);
        }
    }

    #[test]
    fn with_glass_the_choice_picks_the_backdrop() {
        let r = |c| Backdrop::resolve(c, true, false);
        assert_eq!(r(GlassChoice::Off), Backdrop::Blur);
        assert_eq!(r(GlassChoice::Window), Backdrop::GlassWindow);
        assert_eq!(r(GlassChoice::Chrome), Backdrop::GlassChrome);
    }

    #[test]
    fn the_default_is_off() {
        assert_eq!(GlassChoice::default(), GlassChoice::Off);
    }

    #[test]
    fn only_the_blurred_backdrops_take_the_blur_radius() {
        assert!(Backdrop::Blur.blurred());
        assert!(Backdrop::GlassChrome.blurred());
        assert!(!Backdrop::GlassWindow.blurred());
        assert!(!Backdrop::Opaque.blurred());
    }

    #[test]
    fn gpui_tints_where_there_is_no_glass() {
        for b in [Backdrop::Opaque, Backdrop::Blur] {
            assert!(b.regions().is_empty());
            assert!(b.tint().everywhere());
        }
        assert_eq!(Backdrop::GlassWindow.regions(), &[GlassRegion::Window]);
        let t = Backdrop::GlassWindow.tint();
        assert!(!t.chrome && !t.content);
        assert_eq!(
            Backdrop::GlassChrome.regions(),
            &[GlassRegion::Bar, GlassRegion::Sidebar]
        );
        let t = Backdrop::GlassChrome.tint();
        assert!(!t.chrome && t.content);
    }

    #[test]
    fn regions_cover_the_chrome_and_follow_resizes() {
        let (w, h) = (1200.0, 760.0);
        let bar = GlassRegion::Bar.frame(w, h);
        assert_eq!(
            bar,
            Frame {
                x: 0.0,
                y: 0.0,
                w,
                h: chrome::BAR_HEIGHT
            }
        );
        let side = GlassRegion::Sidebar.frame(w, h);
        assert_eq!(side.y, chrome::BAR_HEIGHT, "under the bar");
        assert_eq!(side.w, chrome::SIDEBAR_WIDTH);
        assert_eq!(side.y + side.h, h, "down to the bottom");
        let all = GlassRegion::Window.frame(w, h);
        assert_eq!((all.w, all.h), (w, h));
        assert_eq!(GlassRegion::Window.grows(), (true, true));
        assert_eq!(GlassRegion::Bar.grows(), (true, false));
        assert_eq!(GlassRegion::Sidebar.grows(), (false, true));
    }

    #[test]
    fn regions_stay_inside_a_tiny_window() {
        let side = GlassRegion::Sidebar.frame(100.0, 20.0);
        assert!(side.w <= 100.0 && side.h >= 0.0);
        assert!(GlassRegion::Bar.frame(100.0, 20.0).h <= 20.0);
    }

    #[test]
    fn region_ids_are_distinct_and_ours() {
        let ids = [GlassRegion::Window, GlassRegion::Bar, GlassRegion::Sidebar].map(|r| r.id());
        assert!(ids.iter().all(|id| id.starts_with("fleet.glass.")));
        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[1], ids[2]);
        assert_ne!(ids[0], ids[2]);
    }

    #[test]
    fn the_glass_tint_is_the_window_colour_lighter() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let t = Tokens::for_appearance(appearance).with_window_opacity(0.9);
            let g = glass_tint(&t);
            assert_eq!((g.r, g.g, g.b), (t.window.r, t.window.g, t.window.b));
            assert!((g.a - 0.9 * GLASS_TINT_SHARE).abs() < 1e-6);
        }
    }
}
