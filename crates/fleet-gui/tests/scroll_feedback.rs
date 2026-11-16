//! Scroll feedback (issue #39): the overlay scrollbar's thumb, dragging
//! it, its fade, and the "New output" chip.

use std::time::{Duration, Instant};

use fleet_gui::core::keys::{Key, KeyCode, KeyMods};
use fleet_gui::core::pane::PaneModel;
use fleet_gui::core::scroll_feedback::{
    FADE_IN, FADE_OUT, Fade, HOLD, MIN_THUMB, Wake, end_jumps_to_bottom, offset_for_thumb, thumb,
};
use fleet_proto::AttachOk;
use fleet_term::Palette;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn no_thumb_without_history() {
    assert_eq!(thumb(400.0, 0, 24, 0), None);
    assert_eq!(thumb(0.0, 100, 24, 0), None);
}

#[test]
fn the_thumb_is_the_visible_share_and_sits_at_the_view() {
    // 76 history lines + 24 rows: the view is a quarter of everything.
    let at_bottom = thumb(400.0, 76, 24, 0).unwrap();
    assert_eq!(at_bottom.height, 96.0);
    assert_eq!(at_bottom.top, 304.0);
    let at_top = thumb(400.0, 76, 24, 76).unwrap();
    assert_eq!(at_top.top, 0.0);
    let middle = thumb(400.0, 76, 24, 38).unwrap();
    assert_eq!(middle.top, 152.0);
}

#[test]
fn a_long_history_keeps_the_thumb_grabbable() {
    let t = thumb(400.0, 1_000_000, 24, 0).unwrap();
    assert_eq!(t.height, MIN_THUMB);
    assert_eq!(t.top, 400.0 - MIN_THUMB);
    // A track shorter than the minimum: the thumb fills it.
    let t = thumb(10.0, 1_000_000, 24, 0).unwrap();
    assert_eq!((t.top, t.height), (0.0, 10.0));
}

#[test]
fn dragging_the_thumb_maps_back_to_the_offset() {
    for offset in [0, 1, 19, 38, 75, 76] {
        let t = thumb(400.0, 76, 24, offset).unwrap();
        assert_eq!(offset_for_thumb(400.0, t.height, 76, t.top), offset);
    }
    // Past either end: clamped.
    assert_eq!(offset_for_thumb(400.0, 96.0, 76, -50.0), 76);
    assert_eq!(offset_for_thumb(400.0, 96.0, 76, 1000.0), 0);
    assert_eq!(offset_for_thumb(400.0, 96.0, 76, f32::NAN), 0);
    // Nothing to scroll: always the bottom.
    assert_eq!(offset_for_thumb(400.0, 400.0, 76, 0.0), 0);
    assert_eq!(offset_for_thumb(400.0, 96.0, 0, 0.0), 0);
}

#[test]
fn the_fade_is_idle_until_scrolling() {
    let now = Instant::now();
    let fade = Fade::default();
    assert!(!fade.is_active());
    assert_eq!(fade.opacity(now, false), 0.0);
    assert_eq!(fade.wake(now, false), Wake::Idle);
}

#[test]
fn the_scrollbar_fades_in_holds_and_fades_out() {
    let t0 = Instant::now();
    let mut fade = Fade::default();
    fade.show(t0);
    assert!(fade.is_active());
    // Fading in: animated, frame by frame.
    assert_eq!(fade.opacity(t0, false), 0.0);
    assert_eq!(fade.wake(t0, false), Wake::Frame);
    let half_in = fade.opacity(t0 + FADE_IN / 2, false);
    assert!(half_in > 0.0 && half_in < 1.0);
    // Shown: one timer to the end of the hold, no frames.
    let shown = t0 + FADE_IN;
    assert_eq!(fade.opacity(shown, false), 1.0);
    assert_eq!(fade.wake(shown, false), Wake::After(HOLD - FADE_IN));
    // Fading out after the hold.
    let out = t0 + HOLD + FADE_OUT / 2;
    let o = fade.opacity(out, false);
    assert!(o > 0.0 && o < 1.0, "{o}");
    assert_eq!(fade.wake(out, false), Wake::Frame);
    // Gone: idle again, nothing scheduled.
    let gone = t0 + HOLD + FADE_OUT;
    assert_eq!(fade.opacity(gone, false), 0.0);
    assert_eq!(fade.wake(gone, false), Wake::Idle);
    fade.settle(gone, false);
    assert!(!fade.is_active());
}

#[test]
fn scrolling_again_keeps_it_shown_without_fading_in_again() {
    let t0 = Instant::now();
    let mut fade = Fade::default();
    fade.show(t0);
    let later = t0 + HOLD - ms(10);
    fade.show(later);
    assert_eq!(fade.opacity(later, false), 1.0);
    assert_eq!(fade.wake(later, false), Wake::After(HOLD));
    // Half faded out, a scroll brings it straight back.
    let mut fade = Fade::default();
    fade.show(t0);
    let fading = t0 + HOLD + FADE_OUT / 2;
    fade.show(fading);
    assert_eq!(fade.opacity(fading, false), 1.0);
}

#[test]
fn a_held_scrollbar_does_not_fade() {
    let t0 = Instant::now();
    let mut fade = Fade::default();
    fade.show(t0);
    fade.hold(true);
    let late = t0 + HOLD * 10;
    assert_eq!(fade.opacity(late, false), 1.0);
    assert_eq!(fade.wake(late, false), Wake::Idle);
    // Released: the hold starts again from the release.
    fade.hold(false);
    fade.show(late);
    assert_eq!(fade.wake(late, false), Wake::After(HOLD));
    // Holding a hidden scrollbar does not show it.
    let mut idle = Fade::default();
    idle.hold(true);
    assert!(!idle.is_active());
}

#[test]
fn reduce_motion_shows_and_hides_without_a_fade() {
    let t0 = Instant::now();
    let mut fade = Fade::default();
    fade.show(t0);
    assert_eq!(fade.opacity(t0, true), 1.0);
    assert_eq!(fade.wake(t0, true), Wake::After(HOLD));
    let end = t0 + HOLD;
    assert_eq!(fade.opacity(end, true), 0.0);
    assert_eq!(fade.wake(end, true), Wake::Idle);
    // Never a frame-by-frame redraw.
    for step in 0..40 {
        assert_ne!(fade.wake(t0 + ms(step * 50), true), Wake::Frame);
    }
}

fn model(history: usize) -> PaneModel {
    let mut m = PaneModel::new(Palette {
        fg: (255, 255, 255),
        bg: (0, 0, 0),
        cursor: (255, 255, 255),
        ansi: [(128, 128, 128); 16],
    });
    m.on_attached(&AttachOk {
        agent_id: 1,
        cols: 20,
        rows: 4,
        history_tail: (0..history)
            .map(|i| format!("h{i}").into_bytes().into())
            .collect(),
        ..Default::default()
    });
    m.on_output(b"\x1b[4;1H");
    m
}

#[test]
fn the_chip_shows_only_for_output_below_a_scrolled_up_view() {
    let mut m = model(30);
    m.on_output(b"\r\na");
    assert!(!m.new_output(), "following: no chip");
    m.scroll_lines(5);
    assert!(!m.new_output(), "scrolled, nothing new yet");
    m.on_output(b"\r\nb");
    assert!(m.new_output());
    // Scrolling partway down keeps it; the bottom clears it.
    m.scroll_lines(-2);
    assert!(m.new_output());
    m.scroll_to_bottom();
    assert!(!m.new_output());
}

#[test]
fn a_spinner_redrawing_in_place_is_not_new_output() {
    let mut m = model(30);
    m.scroll_lines(5);
    for frame in ["|", "/", "-", "\\"] {
        m.on_output(format!("\r{frame} working").as_bytes());
    }
    assert!(!m.new_output());
}

#[test]
fn scroll_to_sets_the_offset_clamped_to_the_history() {
    let mut m = model(30);
    m.scroll_to(12);
    assert_eq!(m.scroll_offset(), 12);
    m.scroll_to(10_000);
    assert_eq!(m.scroll_offset(), 30);
    m.on_output(b"\r\nx");
    assert!(m.new_output());
    m.scroll_to(0);
    assert!(!m.new_output());
}

fn key(code: KeyCode, mods: KeyMods) -> Key {
    Key {
        code,
        mods,
        text: None,
    }
}

#[test]
fn end_jumps_to_the_bottom_only_while_scrolled_up() {
    let plain = KeyMods::default();
    assert!(end_jumps_to_bottom(&key(KeyCode::End, plain), true));
    // At the bottom, End is the agent's.
    assert!(!end_jumps_to_bottom(&key(KeyCode::End, plain), false));
    // Modified End and other keys go to the agent.
    let shift = KeyMods {
        shift: true,
        ..plain
    };
    assert!(!end_jumps_to_bottom(&key(KeyCode::End, shift), true));
    assert!(!end_jumps_to_bottom(&key(KeyCode::Home, plain), true));
    assert!(!end_jumps_to_bottom(&key(KeyCode::Char('e'), plain), true));
}
