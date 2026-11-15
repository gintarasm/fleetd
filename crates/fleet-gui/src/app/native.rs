//! AppKit (objc2) calls for what gpui 0.2.2 does not offer: dragging the
//! window by our own titlebar, the system accent colour and the
//! accessibility display settings. Main thread only; elsewhere, or without
//! the event they need, they do nothing.

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSColor, NSColorSpace, NSEventType, NSWorkspace};

/// Lets the window server move the window with the mouse, from the left
/// mouse-down being handled (a press on the unified bar). The window
/// server runs the drag, so this returns at once and gpui keeps drawing.
/// gpui 0.2.2's own `start_window_move` does nothing on macOS.
pub fn begin_window_drag() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(event) = NSApplication::sharedApplication(mtm).currentEvent() else {
        return;
    };
    if event.r#type() != NSEventType::LeftMouseDown {
        return;
    }
    if let Some(window) = event.window(mtm) {
        window.performWindowDragWithEvent(&event);
    }
}

/// The user's accent colour (System Settings › Appearance) as sRGB
/// `(r, g, b)` in 0..=1, if AppKit can say.
pub fn accent_color() -> Option<(f32, f32, f32)> {
    MainThreadMarker::new()?;
    let color =
        NSColor::controlAccentColor().colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
    let c = |v: f64| v.clamp(0.0, 1.0) as f32;
    Some((
        c(color.redComponent()),
        c(color.greenComponent()),
        c(color.blueComponent()),
    ))
}

/// System Settings › Accessibility › Display › Reduce transparency: the
/// window is then opaque.
pub fn reduce_transparency() -> bool {
    MainThreadMarker::new().is_some()
        && NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceTransparency()
}

/// System Settings › Accessibility › Display › Reduce motion: sheets and
/// the toast then appear without sliding.
pub fn reduce_motion() -> bool {
    MainThreadMarker::new().is_some()
        && NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}
