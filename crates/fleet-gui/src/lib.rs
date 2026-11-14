//! `fleet-gui`: the gpui client for fleetd. `core` is platform-neutral and
//! unit-tested everywhere; `app` (macOS only) is the gpui shell.

#[cfg(target_os = "macos")]
pub mod app;
pub mod core;
