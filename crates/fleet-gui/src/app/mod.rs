//! gpui shell (macOS only).

mod keymap;
mod pane_view;
mod terminal_element;
mod theme;

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};

pub use keymap::to_key;
pub use pane_view::{InputSink, PaneView, grid_size};
pub use terminal_element::TerminalElement;
pub use theme::Theme;

use crate::core::config::{GuiConfig, load_config};
use crate::core::session::{PaneId, Session, UiEvent};
use crate::core::target::Target;

/// Temporary: the agent the single pane attaches to, until the workspace
/// (Task 10) lands.
const DEV_AGENT_ENV: &str = "FLEET_GUI_DEV_AGENT";

/// Starts the app: one window with one pane attached to the agent in
/// `FLEET_GUI_DEV_AGENT`.
pub fn run(state_dir: PathBuf, target: Option<Target>) {
    let config = load_config(&state_dir).unwrap_or_else(|e| {
        eprintln!("fleet-gui: {e}; using the default settings");
        GuiConfig::default()
    });
    let target = target
        .or_else(|| config.connection.clone())
        .unwrap_or(Target::Local);
    let agent = std::env::var(DEV_AGENT_ENV).ok().and_then(|v| {
        let id = v.trim().parse::<u64>().ok();
        if id.is_none() {
            eprintln!("fleet-gui: {DEV_AGENT_ENV}={v:?} is not an agent id");
        }
        id
    });
    if agent.is_none() {
        eprintln!("fleet-gui: set {DEV_AGENT_ENV}=<agent id> to attach a pane");
    }

    Application::new().run(move |cx| {
        keymap::bind_keys(cx);
        let (ui_tx, ui_rx) = async_channel::unbounded();
        let session = Session::start(target, state_dir, ui_tx);
        let theme = Theme::for_appearance(cx.window_appearance());
        session.set_palette(theme.palette);
        cx.set_global(theme);

        let pane = PaneId(1);
        let model = session.open_pane(pane, agent);
        let sink: Arc<dyn InputSink> = Arc::new(session.clone());
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(960.), px(640.)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("fleet".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let window = cx
            .open_window(options, |window, cx| {
                let view = cx.new(|cx| PaneView::new(pane, model, sink, &config, window, cx));
                window.focus(&view.read(cx).focus_handle().clone());
                view
            })
            .expect("open the window");

        let palette_session = session.clone();
        window
            .update(cx, |_, window, _| {
                window
                    .observe_window_appearance(move |window, cx| {
                        let theme = Theme::for_appearance(window.appearance());
                        palette_session.set_palette(theme.palette);
                        cx.set_global(theme);
                        window.refresh();
                    })
                    .detach();
            })
            .expect("window is open");

        // Wake the view for model changes, coalescing what is queued.
        let view = window.entity(cx).expect("window is open").downgrade();
        cx.spawn(async move |cx| {
            while let Ok(first) = ui_rx.recv().await {
                let mut changed = false;
                for event in
                    std::iter::once(first).chain(std::iter::from_fn(|| ui_rx.try_recv().ok()))
                {
                    match event {
                        UiEvent::PaneChanged(p) if p == pane => changed = true,
                        UiEvent::Error(e) => eprintln!("fleet-gui: {e}"),
                        _ => {}
                    }
                }
                if changed && view.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();

        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.on_app_quit(move |_| {
            session.clone().shutdown();
            async {}
        })
        .detach();
    });
}
