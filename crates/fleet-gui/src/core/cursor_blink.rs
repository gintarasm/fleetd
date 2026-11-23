//! The terminal cursor's blink (decision 0096): whether a pane's cursor
//! blinks, and which half of the period it is in. The shell supplies the
//! clock and runs one timer, for the focused pane only.

use std::time::{Duration, Instant};

use serde::Deserialize;

/// How long the cursor is shown, then hidden, while it blinks (AppKit's
/// caret period, as the text fields use).
pub const PERIOD: Duration = Duration::from_millis(530);

/// `[gui] cursor_blink`: follow what the app asks for (DECSCUSR), or
/// override it either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlinkChoice {
    #[default]
    App,
    /// Always blinks.
    On,
    /// Never blinks.
    Off,
}

impl BlinkChoice {
    pub const ALL: [BlinkChoice; 3] = [BlinkChoice::App, BlinkChoice::On, BlinkChoice::Off];

    pub fn title(self) -> &'static str {
        match self {
            BlinkChoice::App => "As the app asks",
            BlinkChoice::On => "Always",
            BlinkChoice::Off => "Never",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BlinkChoice::App => "app",
            BlinkChoice::On => "on",
            BlinkChoice::Off => "off",
        }
    }
}

/// The cursor blinks: the choice allows it (or the app asked, under
/// [`BlinkChoice::App`]), the pane has the keyboard, and motion is not
/// reduced.
pub fn blinks(choice: BlinkChoice, app_asks: bool, focused: bool, reduce_motion: bool) -> bool {
    let wanted = match choice {
        BlinkChoice::App => app_asks,
        BlinkChoice::On => true,
        BlinkChoice::Off => false,
    };
    wanted && focused && !reduce_motion
}

/// A blink's phase: shown for a [`PERIOD`] from `since`, hidden for the
/// next, and so on. A keypress restarts it shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blink {
    since: Instant,
}

impl Blink {
    pub fn new(now: Instant) -> Self {
        Blink { since: now }
    }

    /// Starts again from the shown half.
    pub fn restart(&mut self, now: Instant) {
        self.since = now;
    }

    fn periods(&self, now: Instant) -> (u128, Duration) {
        let elapsed = now.saturating_duration_since(self.since);
        let period = PERIOD.as_nanos();
        let into = (elapsed.as_nanos() % period) as u64;
        (elapsed.as_nanos() / period, Duration::from_nanos(into))
    }

    /// The cursor is in its shown half at `now`.
    pub fn shown(&self, now: Instant) -> bool {
        self.periods(now).0.is_multiple_of(2)
    }

    /// How long until the cursor next turns on or off.
    pub fn until_toggle(&self, now: Instant) -> Duration {
        let (_, into) = self.periods(now);
        PERIOD.saturating_sub(into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn app_choice_follows_the_app_and_overrides_win() {
        assert!(blinks(BlinkChoice::App, true, true, false));
        assert!(!blinks(BlinkChoice::App, false, true, false));
        assert!(blinks(BlinkChoice::On, false, true, false));
        assert!(!blinks(BlinkChoice::Off, true, true, false));
    }

    #[test]
    fn never_blinks_unfocused_or_under_reduce_motion() {
        for choice in BlinkChoice::ALL {
            assert!(!blinks(choice, true, false, false), "{choice:?} unfocused");
            assert!(!blinks(choice, true, true, true), "{choice:?} reduced");
        }
    }

    #[test]
    fn the_cursor_alternates_shown_and_hidden_each_period() {
        let t0 = Instant::now();
        let b = Blink::new(t0);
        assert!(b.shown(t0));
        assert!(b.shown(t0 + MS(529)));
        assert!(!b.shown(t0 + MS(530)));
        assert!(!b.shown(t0 + MS(1059)));
        assert!(b.shown(t0 + MS(1060)));
    }

    #[test]
    fn until_toggle_is_the_rest_of_the_current_half() {
        let t0 = Instant::now();
        let b = Blink::new(t0);
        assert_eq!(b.until_toggle(t0), PERIOD);
        assert_eq!(b.until_toggle(t0 + MS(500)), MS(30));
        assert_eq!(b.until_toggle(t0 + MS(530)), PERIOD);
        assert_eq!(b.until_toggle(t0 + MS(700)), MS(360));
    }

    #[test]
    fn a_keypress_restarts_it_shown() {
        let t0 = Instant::now();
        let mut b = Blink::new(t0);
        assert!(!b.shown(t0 + MS(600)));
        b.restart(t0 + MS(600));
        assert!(b.shown(t0 + MS(600)));
        assert!(b.shown(t0 + MS(1100)));
        assert!(!b.shown(t0 + MS(1130)));
    }

    #[test]
    fn titles_and_keys_are_distinct() {
        let keys: Vec<_> = BlinkChoice::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(keys, ["app", "on", "off"]);
    }
}
