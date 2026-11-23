//! gpui shell (macOS only).

mod agent_mark;
mod answer_sheet;
mod app_state;
mod attention;
mod bench;
mod connect;
mod context_menu;
mod action_sheet;
mod controls;
mod diff_pane;
mod ended_view;
mod fonts;
mod graph_lines;
mod graph_view;
mod icons;
mod keymap;
mod kill_sheet;
mod labels_sheet;
mod native;
mod new_agent;
mod output;
mod palette;
mod pane_drag;
mod pane_view;
mod peek_view;
mod picker_view;
mod question_view;
mod rename;
mod resident;
mod search_all_view;
mod send_sheet;
mod settings_window;
mod sidebar_view;
mod split_chooser;
mod split_dividers;
mod status_dot;
mod status_item;
mod tail_text;
mod terminal_element;
mod text_field;
mod theme;
mod titlebar_view;
mod workspace;

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{AppContext, Application, Menu, MenuItem};

pub use app_state::{AppState, SessionApi};
pub use connect::{
    Backend, ConnectSheet, RealBackend, StartupAttempt, show_connect_sheet, startup_work,
    switch_fleetd, switch_target,
};
pub use keymap::{CommandPalette, bind_keys, to_key};
pub use pane_view::{InputSink, PaneChrome, PaneView, grid_size};
pub use picker_view::{PickerEvent, PickerView};
pub use settings_window::{SettingsWindow, TextSetting, open_settings, settings_window};
pub use split_chooser::{ChooserEvent, SplitChooserView};
pub use terminal_element::TerminalElement;
pub use theme::Theme;
pub use workspace::{Workspace, open_main_window};

use crate::core::config::{GuiConfig, load_config};
use crate::core::target::Target;

/// Starts the app. The window opens at once, unconnected; then the startup
/// flow connects to `target` (`--target`), else the saved connection, else
/// a local fleetd that answers, and otherwise shows the connect sheet. A
/// daemon is only ever started from the sheet. Closing the window keeps
/// the app, the session and every pane attached in the menu bar (no Dock
/// icon; the status menu reopens it); Quit detaches everything and leaves
/// the agents running.
pub fn run(state_dir: PathBuf, target: Option<Target>) {
    bench::init();
    let (config, config_error) = match load_config(&state_dir) {
        Ok(c) => (c, None),
        Err(e) => {
            let message = format!("{e}; using the default settings");
            eprintln!("fleet-gui: {message}");
            (GuiConfig::default(), Some(message))
        }
    };
    // Off the main thread while the app launches.
    fonts::preload(&config.font_family);
    // `--target` beats `[gui] connection`.
    let saved = target.or_else(|| config.connection.clone());
    let backend: Arc<dyn Backend> = Arc::new(RealBackend::new(state_dir.clone()));
    // The connect runs while the app launches and the window opens.
    let attempt = StartupAttempt::start(backend.clone(), saved.clone());

    let app = Application::new().with_assets(icons::Assets);
    // Unconnected, the reopened window asks again.
    app.on_reopen(resident::reopen);
    app.run(move |cx| {
        keymap::bind_keys(cx);
        cx.on_action(|_: &keymap::Quit, cx| cx.quit());
        cx.on_action(|_: &keymap::SwitchFleetd, cx| switch_fleetd(cx));
        cx.on_action(|_: &keymap::InstallSkill, cx| resident::install_agent_skill(cx));
        cx.on_action(|_: &keymap::OpenSettings, cx| open_settings(cx));
        // `[gui] theme` before any window draws (the themes then follow).
        native::set_app_appearance(config.theme);
        cx.set_menus(vec![
            Menu {
                name: "fleet".into(),
                items: vec![
                    MenuItem::action("Settings…", keymap::OpenSettings),
                    MenuItem::action("Command Palette…", keymap::CommandPalette),
                    MenuItem::action("Send to Agents…", keymap::SendToAgents),
                    MenuItem::action("Next Agent That Needs You", keymap::NextNeedsYou),
                    MenuItem::separator(),
                    MenuItem::action("Remove Exited Agents…", keymap::RemoveExited),
                    MenuItem::separator(),
                    MenuItem::action("Switch fleetd…", keymap::SwitchFleetd),
                    MenuItem::action("Install agent skill", keymap::InstallSkill),
                    MenuItem::separator(),
                    MenuItem::action("Quit fleet", keymap::Quit),
                ],
            },
            Menu {
                // AppKit adds AutoFill, Dictation and Emoji & Symbols to it.
                name: "Edit".into(),
                items: vec![
                    MenuItem::action("Undo", keymap::Undo),
                    MenuItem::action("Redo", keymap::Redo),
                    MenuItem::separator(),
                    MenuItem::action("Cut", keymap::Cut),
                    MenuItem::action("Copy", keymap::Copy),
                    MenuItem::action("Paste", keymap::Paste),
                    MenuItem::action("Select All", keymap::SelectAll),
                    MenuItem::action("Clear Scrollback", keymap::ClearScrollback),
                    MenuItem::separator(),
                    MenuItem::action("Find…", keymap::Find),
                    MenuItem::action("Find Next", keymap::FindNext),
                    MenuItem::action("Find Previous", keymap::FindPrevious),
                    MenuItem::action("Search All Agents…", keymap::SearchAllAgents),
                ],
            },
            Menu {
                name: "View".into(),
                items: vec![
                    MenuItem::action("Bigger", keymap::ZoomIn),
                    MenuItem::action("Smaller", keymap::ZoomOut),
                    MenuItem::action("Actual Size", keymap::ZoomReset),
                ],
            },
            Menu {
                name: "Window".into(),
                items: vec![
                    MenuItem::action("New Tab", keymap::NewTab),
                    MenuItem::action("Close Tab", keymap::CloseTab),
                    MenuItem::separator(),
                    MenuItem::action("Show Next Tab", keymap::NextTab),
                    MenuItem::action("Show Previous Tab", keymap::PrevTab),
                ],
            },
            Menu {
                name: "Help".into(),
                items: vec![MenuItem::action(
                    "Keyboard Shortcuts",
                    keymap::ShowShortcuts,
                )],
            },
        ]);
        connect::install_backend(backend, cx);
        let theme = Theme::for_appearance(cx.window_appearance());
        let shown = saved.clone().unwrap_or(Target::Local);
        let state = cx.new(|cx| AppState::without_session(shown, state_dir, config, theme, cx));
        AppState::install(state.clone(), cx);
        // Shown in the error toast too: launched from the Dock, stderr is unseen.
        if let Some(message) = config_error {
            state.update(cx, |s, cx| s.set_config_error(message, cx));
        }
        // Connected already (the usual case): the session starts, and
        // lists the agents, while the window opens. Otherwise the startup
        // flow waits for the attempt over the open window.
        match attempt.into_result() {
            Ok(Ok(target)) => {
                switch_target(target, cx);
                open_main_window(cx);
            }
            Ok(Err(reason)) => {
                open_main_window(cx);
                connect::begin_startup(StartupAttempt::finished(saved, Err(reason)), cx);
            }
            Err(attempt) => {
                open_main_window(cx);
                connect::begin_startup(attempt, cx);
            }
        }
        resident::install(cx);
        attention::install(cx);
        context_menu::install(cx);

        // Detach only, well inside gpui's 100 ms quit budget; agents run on.
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
