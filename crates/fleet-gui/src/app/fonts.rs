//! The terminal font. SF Mono is not an installed family on most Macs
//! (gpui's lookup then falls back to Helvetica); its static faces ship
//! inside Terminal.app, so when the config asks for SF Mono the GUI reads
//! those files off the main thread at launch and registers them with
//! gpui's text system before the first pane resolves its font. A family
//! that still does not resolve falls back to Menlo.

use std::borrow::Cow;
use std::path::Path;
use std::sync::Mutex;
use std::thread::JoinHandle;

use gpui::{App, Font, Global, font};

use crate::core::config::{FALLBACK_FONT_FAMILY, SF_MONO_DIR, font_families, sf_mono_files};

type Faces = Vec<Vec<u8>>;

/// The faces being read at launch, taken by the first pane.
static PRELOAD: Mutex<Option<JoinHandle<Faces>>> = Mutex::new(None);

/// Starts reading the font files `configured` needs (SF Mono's from
/// Terminal.app), on a thread. Call once at launch.
pub fn preload(configured: &str) {
    let files = sf_mono_files(configured, Path::new(SF_MONO_DIR));
    if files.is_empty() {
        return;
    }
    let reader = std::thread::spawn(move || {
        files
            .iter()
            .filter_map(|f| std::fs::read(f).ok())
            .collect::<Faces>()
    });
    *PRELOAD.lock().unwrap_or_else(|e| e.into_inner()) = Some(reader);
}

/// Marks that the preloaded faces were handed to the text system.
struct Registered;

impl Global for Registered {}

/// Registers the preloaded faces, once, before any font is resolved.
fn register_preloaded(cx: &mut App) {
    if cx.has_global::<Registered>() {
        return;
    }
    cx.set_global(Registered);
    let reader = PRELOAD.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some(faces) = reader.and_then(|r| r.join().ok()) else {
        return;
    };
    if faces.is_empty() {
        return;
    }
    let faces = faces.into_iter().map(Cow::Owned).collect();
    if let Err(e) = cx.text_system().add_fonts(faces) {
        eprintln!("fleet-gui: cannot load SF Mono from Terminal.app: {e}");
    }
}

/// The terminal font: the first of [`font_families`] (the configured
/// family, then Menlo) that resolves to itself. gpui would otherwise fall
/// back to a proportional face for a missing family.
pub fn terminal_font(configured: &str, cx: &mut App) -> Font {
    register_preloaded(cx);
    let ts = cx.text_system();
    for family in font_families(configured) {
        let f = font(family.to_owned());
        let id = ts.resolve_font(&f);
        if ts
            .get_font_for_id(id)
            .is_some_and(|got| got.family == f.family)
        {
            return f;
        }
    }
    font(FALLBACK_FONT_FAMILY)
}
