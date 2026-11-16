//! Font zoom (issue #38): ⌘+ and ⌘− step the terminal font, ⌘0 resets it
//! to the configured size. Window-wide and not saved.

use fleet_gui::core::settings::{FONT_SIZE_MAX, FONT_SIZE_MIN};
use fleet_gui::core::zoom::{FontZoom, Zoom};

#[test]
fn unzoomed_is_the_configured_size() {
    let z = FontZoom::default();
    assert_eq!(z.size(13.0), 13.0);
    assert!(!z.is_zoomed(13.0));
}

#[test]
fn in_and_out_step_one_point_and_reset_returns() {
    let mut z = FontZoom::default();
    assert!(z.apply(Zoom::In, 13.0));
    assert!(z.apply(Zoom::In, 13.0));
    assert_eq!(z.size(13.0), 15.0);
    assert!(z.is_zoomed(13.0));
    assert!(z.apply(Zoom::Out, 13.0));
    assert_eq!(z.size(13.0), 14.0);
    assert!(z.apply(Zoom::Reset, 13.0));
    assert_eq!(z.size(13.0), 13.0);
    assert!(!z.is_zoomed(13.0));
    assert!(!z.apply(Zoom::Reset, 13.0), "nothing to reset");
}

#[test]
fn zoom_stops_at_the_font_size_range() {
    let mut z = FontZoom::default();
    for _ in 0..100 {
        z.apply(Zoom::In, 13.0);
    }
    assert_eq!(z.size(13.0), FONT_SIZE_MAX);
    assert!(!z.apply(Zoom::In, 13.0), "no change at the top");
    for _ in 0..100 {
        z.apply(Zoom::Out, 13.0);
    }
    assert_eq!(z.size(13.0), FONT_SIZE_MIN);
    assert!(!z.apply(Zoom::Out, 13.0), "no change at the bottom");
}

#[test]
fn zooming_back_to_the_configured_size_is_not_zoomed() {
    let mut z = FontZoom::default();
    z.apply(Zoom::In, 13.0);
    z.apply(Zoom::Out, 13.0);
    assert!(!z.is_zoomed(13.0));
}

#[test]
fn a_new_configured_size_ends_the_zoom() {
    // Settings (or a hand edit) chose a size: that is what shows.
    let mut z = FontZoom::default();
    z.apply(Zoom::In, 13.0);
    assert_eq!(z.size(16.0), 16.0);
    assert!(!z.is_zoomed(16.0));
    // Zooming then steps from the new size.
    z.apply(Zoom::Out, 16.0);
    assert_eq!(z.size(16.0), 15.0);
}

#[test]
fn an_out_of_range_configured_size_zooms_into_the_range() {
    let mut z = FontZoom::default();
    assert_eq!(z.size(40.0), 40.0, "shown as configured until zoomed");
    assert!(z.apply(Zoom::Out, 40.0));
    assert_eq!(z.size(40.0), FONT_SIZE_MAX);
    let mut z = FontZoom::default();
    z.apply(Zoom::In, 13.5);
    assert_eq!(z.size(13.5), 15.0, "steps land on whole points");
}
