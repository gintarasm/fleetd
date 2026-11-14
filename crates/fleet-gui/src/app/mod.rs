//! gpui shell (macOS only).

mod app_state;
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
pub use keymap::to_key;
pub use pane_view::{InputSink, PaneChrome, PaneView, grid_size};
pub use terminal_element::TerminalElement;
pub use theme::Theme;
pub use workspace::{Workspace, open_main_window};

use crate::core::config::{GuiConfig, load_config};
use crate::core::session::Session;
use crate::core::target::Target;

/// Starts the app: the workspace window over the target's saved layout.
/// Closing the window keeps the app, the session and every pane attached
/// (the Dock icon reopens it); Quit detaches everything.
pub fn run(state_dir: PathBuf, target: Option<Target>) {
    let config = load_config(&state_dir).unwrap_or_else(|e| {
        eprintln!("fleet-gui: {e}; using the default settings");
        GuiConfig::default()
    });
    let target = target
        .or_else(|| config.connection.clone())
        .unwrap_or(Target::Local);

    let app = Application::new();
    app.on_reopen(open_main_window);
    app.run(move |cx| {
        keymap::bind_keys(cx);
        cx.on_action(|_: &keymap::Quit, cx| cx.quit());
        cx.set_menus(vec![Menu {
            name: "fleet".into(),
            items: vec![MenuItem::action("Quit fleet", keymap::Quit)],
        }]);
        let (ui_tx, ui_rx) = async_channel::unbounded();
        let session = Session::start(target.clone(), state_dir.clone(), ui_tx);
        let theme = Theme::for_appearance(cx.window_appearance());
        let api: Arc<dyn SessionApi> = Arc::new(session.clone());
        let state = cx.new(|cx| AppState::new(api, ui_rx, target, state_dir, config, theme, cx));
        AppState::install(state.clone(), cx);
        open_main_window(cx);

        cx.on_app_quit(move |cx| {
            state.update(cx, |s, _| s.save_now());
            session.clone().shutdown();
            async {}
        })
        .detach();
    });
}
