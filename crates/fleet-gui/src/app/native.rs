//! AppKit (objc2) calls for what gpui 0.2.2 does not offer: dragging the
//! window by our own titlebar, the strength of the window's blur, the
//! system accent colour, the app's appearance and the accessibility
//! display settings. Main thread only; elsewhere, or without the event
//! they need, they do nothing.

use gpui::Window;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{ClassType, MainThreadMarker, msg_send};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication, NSColor,
    NSColorSpace, NSEventType, NSView, NSVisualEffectView, NSWorkspace,
};
use objc2_foundation::{NSArray, NSNumber, NSObjectProtocol, NSString};

use crate::core::settings::ThemeChoice;

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

/// Draws every window of the app light, dark, or as the system does
/// (`[gui] theme`). AppKit then reports the new effective appearance to
/// each window, so the themes, the blur's material, native menus and
/// alerts all follow, through the same path as a system change.
pub fn set_app_appearance(choice: ThemeChoice) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // SAFETY: AppKit's appearance name constants are immutable statics.
    let name = unsafe {
        match choice {
            ThemeChoice::System => None,
            ThemeChoice::Light => Some(NSAppearanceNameAqua),
            ThemeChoice::Dark => Some(NSAppearanceNameDarkAqua),
        }
    };
    let appearance = name.and_then(NSAppearance::appearanceNamed);
    NSApplication::sharedApplication(mtm).setAppearance(appearance.as_deref());
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

/// Sets the blur radius, in points, of the window's blurred backdrop.
/// gpui's `Blurred` background is an `NSVisualEffectView` whose blur AppKit
/// fixes; this finds that view's `gaussianBlur` filter and sets its
/// radius. AppKit rebuilds the filter when the appearance changes, so call
/// this again then. Returns how many filters it set: 0 when there is no
/// blur (Reduce Transparency, or a future AppKit that names it otherwise),
/// and the window is left as AppKit drew it.
pub fn set_backdrop_blur(window: &Window, radius: f64) -> usize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    if MainThreadMarker::new().is_none() {
        return 0;
    }
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return 0;
    };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        return 0;
    };
    // SAFETY: gpui's AppKit handle points at the window's live content
    // view, and this runs on the main thread (checked above).
    let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
    let Some(content) = view.window().and_then(|w| w.contentView()) else {
        return 0;
    };
    let mut set = 0;
    blur_in_views(&content, radius, &mut set);
    set
}

fn blur_in_views(view: &NSView, radius: f64, set: &mut usize) {
    if view.isKindOfClass(NSVisualEffectView::class()) {
        // SAFETY: `layer` is an NSView property; nil when not layer-backed.
        let layer: Option<Retained<AnyObject>> = unsafe { msg_send![view, layer] };
        if let Some(layer) = layer {
            blur_in_layers(&layer, radius, set);
        }
    }
    for sub in view.subviews() {
        blur_in_views(&sub, radius, set);
    }
}

fn blur_in_layers(layer: &AnyObject, radius: f64, set: &mut usize) {
    // SAFETY: `filters` and `sublayers` are CALayer properties (arrays or
    // nil); a CAFilter's `name` and `inputRadius` are key-value coded,
    // which is how AppKit itself configures them.
    unsafe {
        let filters: Option<Retained<NSArray<AnyObject>>> = msg_send![layer, filters];
        if let Some(filters) = filters {
            let key_name = NSString::from_str("name");
            let gaussian = NSString::from_str("gaussianBlur");
            let key_radius = NSString::from_str("inputRadius");
            let value = NSNumber::new_f64(radius);
            let mut hit = false;
            for filter in filters.iter() {
                let name: Option<Retained<AnyObject>> =
                    msg_send![&*filter, valueForKey: &*key_name];
                let is_blur = name.is_some_and(|n| {
                    let eq: bool = msg_send![&*n, isEqual: &*gaussian];
                    eq
                });
                if is_blur {
                    let _: () = msg_send![&*filter, setValue: &*value, forKey: &*key_radius];
                    hit = true;
                    *set += 1;
                }
            }
            if hit {
                // A layer copies its filters: hand the changed ones back.
                let _: () = msg_send![layer, setFilters: &*filters];
            }
        }
        let subs: Option<Retained<NSArray<AnyObject>>> = msg_send![layer, sublayers];
        if let Some(subs) = subs {
            for sub in subs.iter() {
                blur_in_layers(&sub, radius, set);
            }
        }
    }
}
