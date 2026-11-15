//! Design tokens the shell draws with: the 4/8 px spacing grid, the type
//! scale (the macOS text styles, set in the system font, SF Pro), corner
//! radii and the window chrome's metrics. Plain numbers in logical pixels,
//! so a native shell could share them.

/// The base unit: every spacing, size and radius is a multiple of it.
pub const GRID: f32 = 4.0;

/// `steps` grid units.
pub const fn grid(steps: u8) -> f32 {
    steps as f32 * GRID
}

/// Spacing between and around elements.
pub mod space {
    use super::grid;

    pub const XXS: f32 = grid(1) / 2.0;
    pub const XS: f32 = grid(1);
    pub const SM: f32 = grid(2);
    pub const MD: f32 = grid(3);
    pub const LG: f32 = grid(4);
    pub const XL: f32 = grid(6);
}

/// Corner radii.
pub mod radius {
    /// Small fills: badges, the tab's focus bar.
    pub const SM: f32 = 4.0;
    /// Controls: buttons, fields, sidebar rows, menu-like lists.
    pub const MD: f32 = 6.0;
    /// Cards: panes, sheets.
    pub const LG: f32 = 10.0;
}

/// A font weight from the macOS text styles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    Regular,
    Medium,
    Semibold,
    Bold,
}

impl Weight {
    /// The CSS / OpenType weight value (400 regular .. 700 bold).
    pub fn value(self) -> f32 {
        match self {
            Weight::Regular => 400.0,
            Weight::Medium => 500.0,
            Weight::Semibold => 600.0,
            Weight::Bold => 700.0,
        }
    }
}

/// One step of the type scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle {
    pub size: f32,
    pub line_height: f32,
    pub weight: Weight,
}

const fn style(size: f32, line_height: f32, weight: Weight) -> TextStyle {
    TextStyle {
        size,
        line_height,
        weight,
    }
}

/// The macOS text styles (Human Interface Guidelines, "Typography":
/// Title 3 15, Headline 13 bold, Body 13, Callout 12, Subheadline 11,
/// Caption 10), in the system font.
pub mod text {
    use super::{TextStyle, Weight, style};

    /// Sheet titles.
    pub const TITLE: TextStyle = style(15.0, 20.0, Weight::Semibold);
    /// The titlebar's target name, pane titles.
    pub const HEADLINE: TextStyle = style(13.0, 16.0, Weight::Semibold);
    /// Sidebar rows, field text, buttons.
    pub const BODY: TextStyle = style(13.0, 16.0, Weight::Regular);
    /// Pane paths, banners, tooltips, list items in sheets.
    pub const CALLOUT: TextStyle = style(12.0, 16.0, Weight::Regular);
    /// Second lines, sidebar section headers, field labels.
    pub const SUBHEADLINE: TextStyle = style(11.0, 14.0, Weight::Regular);
    /// Sidebar section headers.
    pub const SECTION: TextStyle = style(11.0, 14.0, Weight::Semibold);
    /// Timestamps and counts.
    pub const CAPTION: TextStyle = style(10.0, 12.0, Weight::Regular);

    /// Every style, for checks.
    pub const ALL: [TextStyle; 7] = [
        TITLE,
        HEADLINE,
        BODY,
        CALLOUT,
        SUBHEADLINE,
        SECTION,
        CAPTION,
    ];
}

/// The window chrome.
pub mod chrome {
    use super::grid;

    /// The unified bar across the top: titlebar and toolbar in one.
    pub const BAR_HEIGHT: f32 = grid(10);
    /// The traffic lights' leading inset (AppKit's own is 20 in a toolbar
    /// window; 16 lines them up with the sidebar's content).
    pub const TRAFFIC_LIGHTS_X: f32 = grid(4);
    /// Height of one traffic-light button.
    pub const TRAFFIC_LIGHT_SIZE: f32 = 14.0;
    /// Room the three traffic lights take from the bar's leading edge,
    /// their inset included, before the bar's own content may start.
    pub const TRAFFIC_LIGHTS_WIDTH: f32 = grid(20);
    /// Width of the sidebar.
    pub const SIDEBAR_WIDTH: f32 = grid(60);
    /// Between pane cards and around them.
    pub const PANE_GAP: f32 = grid(2);
    /// The pane card's header.
    pub const PANE_HEADER_HEIGHT: f32 = grid(8);
    /// A square icon button, and the icon in it.
    pub const ICON_BUTTON: f32 = grid(6);
    pub const ICON: f32 = 14.0;
    /// Text fields and buttons in sheets.
    pub const CONTROL_HEIGHT: f32 = grid(7);

    /// Where gpui is told to put the traffic lights (from the bar's top
    /// left corner) so they sit centred in a bar `bar_height` tall.
    pub fn traffic_lights_origin(bar_height: f32) -> (f32, f32) {
        let y = ((bar_height - TRAFFIC_LIGHT_SIZE) / 2.0).max(0.0).floor();
        (TRAFFIC_LIGHTS_X, y)
    }
}

/// Motion: durations of the short transitions, in milliseconds.
pub mod motion {
    /// Sheets fading and sliding in.
    pub const SHEET_MS: u64 = 160;
    /// The error toast sliding in.
    pub const TOAST_MS: u64 = 200;
    /// How far a sheet slides while it fades in.
    pub const SHEET_OFFSET: f32 = super::grid(2);

    /// macOS's standard ease-out: fast start, gentle stop. `t` in 0..=1.
    pub fn ease_out(t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        1.0 - (1.0 - t).powi(3)
    }
}

#[cfg(test)]
mod tests {
    use super::{chrome, grid, motion, radius, space, text};

    fn on_grid(v: f32, unit: f32) -> bool {
        (v / unit).fract() == 0.0
    }

    #[test]
    fn spacing_and_chrome_sit_on_the_grid() {
        for v in [space::XS, space::SM, space::MD, space::LG, space::XL] {
            assert!(on_grid(v, 4.0), "{v} is off the 4 px grid");
        }
        assert!(on_grid(space::XXS, 2.0));
        for v in [
            chrome::BAR_HEIGHT,
            chrome::TRAFFIC_LIGHTS_X,
            chrome::TRAFFIC_LIGHTS_WIDTH,
            chrome::SIDEBAR_WIDTH,
            chrome::PANE_GAP,
            chrome::PANE_HEADER_HEIGHT,
            chrome::ICON_BUTTON,
            chrome::CONTROL_HEIGHT,
            radius::SM,
        ] {
            assert!(on_grid(v, 4.0), "{v} is off the 4 px grid");
        }
        assert_eq!(grid(3), 12.0);
    }

    #[test]
    fn type_scale_is_ordered_and_readable() {
        for s in text::ALL {
            assert!(s.line_height >= s.size, "{s:?}: lines would overlap");
            assert!(s.size >= 10.0, "{s:?}: below the macOS minimum");
            assert!(on_grid(s.line_height, 2.0), "{s:?}: half-pixel lines");
        }
        let sizes: Vec<f32> = [text::TITLE, text::BODY, text::CALLOUT, text::SUBHEADLINE]
            .iter()
            .chain([&text::CAPTION])
            .map(|s| s.size)
            .collect();
        assert!(sizes.windows(2).all(|w| w[0] > w[1]), "{sizes:?}");
        assert!(text::HEADLINE.weight.value() > text::BODY.weight.value());
    }

    #[test]
    fn traffic_lights_are_centred_in_the_bar() {
        let (x, y) = chrome::traffic_lights_origin(chrome::BAR_HEIGHT);
        assert_eq!(x, chrome::TRAFFIC_LIGHTS_X);
        let below = chrome::BAR_HEIGHT - y - chrome::TRAFFIC_LIGHT_SIZE;
        assert!((below - y).abs() <= 1.0, "top {y}, bottom {below}");
        assert_eq!(
            chrome::traffic_lights_origin(4.0),
            (chrome::TRAFFIC_LIGHTS_X, 0.0)
        );
        // The three lights (14 px, 20 px apart) fit in the reserved room.
        assert!(x + 2.0 * 20.0 + chrome::TRAFFIC_LIGHT_SIZE < chrome::TRAFFIC_LIGHTS_WIDTH);
    }

    #[test]
    fn ease_out_runs_from_zero_to_one() {
        assert_eq!(motion::ease_out(0.0), 0.0);
        assert_eq!(motion::ease_out(1.0), 1.0);
        assert_eq!(motion::ease_out(2.0), 1.0);
        assert!(motion::ease_out(0.5) > 0.5, "ease-out is ahead of linear");
        let mut last = 0.0;
        for i in 1..=10 {
            let v = motion::ease_out(i as f32 / 10.0);
            assert!(v > last);
            last = v;
        }
    }
}
