//! Font zoom (issue #38): ⌘+ and ⌘− step the terminal font of every pane
//! by a point, ⌘0 returns to the configured `[gui] font_size`. The zoom is
//! window-wide and is not saved; a new configured size (Settings, or the
//! file edited by hand) ends it.

use super::settings::{FONT_SIZE_MAX, FONT_SIZE_MIN, FONT_SIZE_STEP};

/// A zoom command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zoom {
    In,
    Out,
    Reset,
}

/// The zoomed font size, and the configured size it was zoomed from.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FontZoom {
    /// `(configured, shown)`; only while `configured` is still the setting.
    zoomed: Option<(f32, f32)>,
}

impl FontZoom {
    /// The size the panes show, given the configured one.
    pub fn size(&self, configured: f32) -> f32 {
        match self.zoomed {
            Some((from, shown)) if from == configured => shown,
            _ => configured,
        }
    }

    /// The panes show a size other than the configured one.
    pub fn is_zoomed(&self, configured: f32) -> bool {
        self.size(configured) != configured
    }

    /// Applies `zoom`; returns whether the shown size changed. Steps land
    /// on whole points within the Settings range.
    pub fn apply(&mut self, zoom: Zoom, configured: f32) -> bool {
        let now = self.size(configured);
        let next = match zoom {
            Zoom::In => (now.round() + FONT_SIZE_STEP).clamp(FONT_SIZE_MIN, FONT_SIZE_MAX),
            Zoom::Out => (now.round() - FONT_SIZE_STEP).clamp(FONT_SIZE_MIN, FONT_SIZE_MAX),
            Zoom::Reset => configured,
        };
        self.zoomed = (next != configured).then_some((configured, next));
        next != now
    }
}
