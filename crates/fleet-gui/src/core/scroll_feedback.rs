//! Scroll feedback (decision 0063): the overlay scrollbar of a pane (its
//! thumb, dragging it, its fade) and when End jumps back to the live grid.
//! The "New output" chip's decision is [`PaneModel::new_output`].
//!
//! Lengths are in points; the shell supplies the clock, so the fade is
//! tested without one.
//!
//! [`PaneModel::new_output`]: super::pane::PaneModel::new_output

use std::time::{Duration, Instant};

use super::keys::{Key, KeyCode};
use super::style::motion;

/// The thumb never gets shorter than this.
pub const MIN_THUMB: f32 = 24.0;
/// Width of the drawn thumb.
pub const BAR_WIDTH: f32 = 6.0;
/// Width of the strip along the right edge that takes presses while the
/// scrollbar shows (wider than the thumb, to be easy to hit).
pub const HIT_WIDTH: f32 = 14.0;
/// Gap between the thumb and the pane's edges.
pub const INSET: f32 = 2.0;

/// The scrollbar fading in once scrolling starts.
pub const FADE_IN: Duration = Duration::from_millis(motion::SCROLLBAR_FADE_IN_MS);
/// How long it stays after the last scroll.
pub const HOLD: Duration = Duration::from_millis(motion::SCROLLBAR_HOLD_MS);
/// The scrollbar fading out after the hold.
pub const FADE_OUT: Duration = Duration::from_millis(motion::SCROLLBAR_FADE_OUT_MS);

/// Where the thumb is on its track, from the track's top.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thumb {
    pub top: f32,
    pub height: f32,
}

/// The thumb on a track `track` long, for `history` loaded lines above
/// `rows` grid rows, the view scrolled up by `offset` lines. `None` when
/// there is nothing to scroll or no track.
pub fn thumb(track: f32, history: u64, rows: u16, offset: u64) -> Option<Thumb> {
    if history == 0 || track.is_nan() || track <= 0.0 {
        return None;
    }
    let total = history as f64 + f64::from(rows);
    let share = (f64::from(rows) / total) as f32;
    let height = (track * share).max(MIN_THUMB).min(track);
    let above = history - offset.min(history);
    let top = (track - height) * (above as f64 / history as f64) as f32;
    Some(Thumb { top, height })
}

/// The scroll offset that puts a thumb `height` long at `top` on a track
/// `track` long ([`thumb`] backwards), clamped to `0..=history`.
pub fn offset_for_thumb(track: f32, height: f32, history: u64, top: f32) -> u64 {
    let room = track - height;
    if history == 0 || room.is_nan() || room <= 0.0 || !top.is_finite() {
        return 0;
    }
    let frac = f64::from((top / room).clamp(0.0, 1.0));
    let above = (frac * history as f64).round() as u64;
    history - above.min(history)
}

/// End (unmodified) while the view is scrolled up returns it to the live
/// grid instead of reaching the agent.
pub fn end_jumps_to_bottom(key: &Key, scrolled: bool) -> bool {
    scrolled && key.code == KeyCode::End && key.mods == Default::default()
}

/// What the shell has to do next to keep the scrollbar's look current.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// Nothing changes until the next scroll: no timer, no frames.
    Idle,
    /// Redraw once this much later (the hold ends).
    After(Duration),
    /// Animating: redraw on the next frame.
    Frame,
}

/// The scrollbar's fade: in while scrolling starts, held while scrolling
/// goes on (and while the pointer holds it), out after [`HOLD`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Fade {
    /// When the current showing began (fading in from there), `None` while
    /// hidden.
    since: Option<Instant>,
    /// The last scroll.
    last: Option<Instant>,
    /// The pointer is on it or dragging it: no fading out.
    held: bool,
}

impl Fade {
    /// The view scrolled (or the thumb was let go) at `now`.
    pub fn show(&mut self, now: Instant) {
        if self.since.is_none() {
            self.since = Some(now);
        } else if self.fading_out(now) {
            // Straight back, no second fade-in.
            self.since = now.checked_sub(FADE_IN).or(self.since);
        }
        self.last = Some(now);
    }

    /// The pointer took (`true`) or left the scrollbar; only a showing one
    /// is held.
    pub fn hold(&mut self, on: bool) {
        self.held = on && self.since.is_some();
    }

    pub fn is_held(&self) -> bool {
        self.held
    }

    /// Shown or fading: the shell asks [`Fade::opacity`] and [`Fade::wake`]
    /// only then.
    pub fn is_active(&self) -> bool {
        self.since.is_some()
    }

    fn fading_out(&self, now: Instant) -> bool {
        !self.held && self.last.is_some_and(|l| now.duration_since(l) > HOLD)
    }

    /// 0 (hidden) to 1.
    pub fn opacity(&self, now: Instant, reduce_motion: bool) -> f32 {
        let (Some(since), Some(last)) = (self.since, self.last) else {
            return 0.0;
        };
        if self.held {
            return 1.0;
        }
        let after_last = now.duration_since(last);
        if reduce_motion {
            return if after_last < HOLD { 1.0 } else { 0.0 };
        }
        let fade_in = motion::ease_out(ratio(now.duration_since(since), FADE_IN));
        let fade_out = match after_last.checked_sub(HOLD) {
            None => 1.0,
            Some(out) => 1.0 - motion::ease_out(ratio(out, FADE_OUT)),
        };
        fade_in.min(fade_out)
    }

    /// When the look next changes, at `now`.
    pub fn wake(&self, now: Instant, reduce_motion: bool) -> Wake {
        let (Some(since), Some(last)) = (self.since, self.last) else {
            return Wake::Idle;
        };
        if self.held {
            return Wake::Idle;
        }
        let after_last = now.duration_since(last);
        if !reduce_motion && now.duration_since(since) < FADE_IN {
            return Wake::Frame;
        }
        if after_last < HOLD {
            return Wake::After(HOLD - after_last);
        }
        if !reduce_motion && after_last < HOLD + FADE_OUT {
            return Wake::Frame;
        }
        Wake::Idle
    }

    /// Forgets a fade that has ended, so idle frames skip it.
    pub fn settle(&mut self, now: Instant, reduce_motion: bool) {
        if !self.held && self.wake(now, reduce_motion) == Wake::Idle {
            self.since = None;
            self.last = None;
        }
    }
}

/// `d / of`, 1 when `of` is zero.
fn ratio(d: Duration, of: Duration) -> f32 {
    if of.is_zero() {
        1.0
    } else {
        (d.as_secs_f32() / of.as_secs_f32()).min(1.0)
    }
}
