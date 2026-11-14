//! gpui shell (macOS only).

mod app_state;
mod connect;
mod keymap;
mod new_agent;
mod pane_view;
mod sidebar_view;
mod terminal_element;
mod theme;
mod workspace;

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{AppContext, Application, Menu, MenuItem};

pub use app_state::{AppState, SessionApi};
pub use connect::{
    Backend, ConnectSheet, RealBackend, show_connect_sheet, switch_fleetd, switch_target,
};
pub use keymap::to_key;
pub use pane_view::{InputSink, PaneChrome, PaneView, grid_size};
pub use terminal_element::TerminalElement;
pub use theme::Theme;
pub use workspace::{Workspace, open_main_window};

use crate::core::config::{GuiConfig, load_config};
use crate::core::target::Target;

/// Starts the app. The window opens at once, unconnected; then the startup
/// flow connects to `target` (`--target`), else the saved connection, else
/// a local fleetd that answers, and otherwise shows the connect sheet. A
/// daemon is only ever started from the sheet. Closing the window keeps
/// the app, the session and every pane attached (the Dock icon reopens it);
/// Quit detaches everything.
pub fn run(state_dir: PathBuf, target: Option<Target>) {
    let config = load_config(&state_dir).unwrap_or_else(|e| {
        eprintln!("fleet-gui: {e}; using the default settings");
        GuiConfig::default()
    });
    // `--target` beats `[gui] connection`.
    let saved = target.or_else(|| config.connection.clone());

    let app = Application::new();
    // Unconnected, the reopened window asks again.
    app.on_reopen(|cx| match AppState::global(cx) {
        Some(a) if !a.read(cx).is_live() => switch_fleetd(cx),
        _ => open_main_window(cx),
    });
    app.run(move |cx| {
        keymap::bind_keys(cx);
        cx.on_action(|_: &keymap::Quit, cx| cx.quit());
        cx.on_action(|_: &keymap::SwitchFleetd, cx| switch_fleetd(cx));
        cx.set_menus(vec![Menu {
            name: "fleet".into(),
            items: vec![
                MenuItem::action("Switch fleetd…", keymap::SwitchFleetd),
                MenuItem::separator(),
                MenuItem::action("Quit fleet", keymap::Quit),
            ],
        }]);
        let backend: Arc<dyn Backend> = Arc::new(RealBackend {
            state_dir: state_dir.clone(),
        });
        connect::install_backend(backend, cx);
        let theme = Theme::for_appearance(cx.window_appearance());
        let shown = saved.clone().unwrap_or(Target::Local);
        let state = cx.new(|cx| AppState::without_session(shown, state_dir, config, theme, cx));
        AppState::install(state.clone(), cx);
        open_main_window(cx);
        connect::begin_startup(saved, cx);

        cx.on_app_quit(move |cx| {
            state.update(cx, |s, _| {
                s.save_now();
                s.shutdown_session();
            });
            async {}
        })
        .detach();
    });
}
