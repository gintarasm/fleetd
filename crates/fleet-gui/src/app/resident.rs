//! The resident app: closing the window keeps the app (and every pane
//! attached) in the menu bar; the status menu and the Dock bring it back.

use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{App, PromptLevel};

use super::app_state::AppState;
use super::connect::switch_fleetd;
use super::status_item::{install_status_item, set_accessory};
use super::workspace::open_main_window;
use crate::core::launch::{install_skill, skill_install_command};
use crate::core::status_menu::{StatusEntry, StatusMenuState};

/// Installs the status item and drops the Dock icon when the last window
/// closes (never quits: the panes stay attached).
pub fn install(cx: &mut App) {
    install_status_item(cx);
    cx.on_window_closed(|cx| {
        if cx.windows().is_empty() {
            eprintln!("fleet-gui: window closed; staying in the menu bar");
            set_accessory(true);
        }
    })
    .detach();
}

/// What the status menu shows for `app`.
pub fn menu_state(app: &AppState) -> StatusMenuState {
    StatusMenuState {
        target: app.target.clone(),
        live: app.is_live(),
        reconnecting: app.is_reconnecting(),
        agents: app.agents.clone(),
        home: app.home.clone(),
    }
}

/// "Show fleetd", and a Dock click with no window: brings the window
/// back as it was (unconnected, it asks for a fleetd again).
pub fn show(cx: &mut App) {
    set_accessory(false);
    match AppState::global(cx) {
        Some(a) if !a.read(cx).is_live() => switch_fleetd(cx),
        _ => open_main_window(cx),
    }
    cx.activate(true);
}

/// Opens (or raises) the window without the unconnected detour.
fn show_window(cx: &mut App) {
    set_accessory(false);
    open_main_window(cx);
    cx.activate(true);
}

/// Acts on a status-menu pick.
pub fn handle(entry: StatusEntry, cx: &mut App) {
    match entry {
        StatusEntry::Header(_) | StatusEntry::Separator => {}
        StatusEntry::Show => show(cx),
        StatusEntry::Agent { id, .. } => {
            show_window(cx);
            with_workspace(cx, |ws, window, cx| ws.show_agent(id, window, cx));
        }
        StatusEntry::NewAgent => {
            show_window(cx);
            with_workspace(cx, |ws, window, cx| ws.open_new_agent(window, cx));
        }
        StatusEntry::ClearExited => {
            if let Some(app) = AppState::global(cx) {
                app.read(cx).clear_exited();
            }
        }
        StatusEntry::SwitchFleetd => {
            set_accessory(false);
            switch_fleetd(cx);
            cx.activate(true);
        }
        StatusEntry::InstallSkill => install_agent_skill(cx),
        StatusEntry::Quit => cx.quit(),
    }
}

fn with_workspace(
    cx: &mut App,
    f: impl FnOnce(&mut super::Workspace, &mut gpui::Window, &mut gpui::Context<super::Workspace>),
) {
    if let Some(handle) = AppState::global(cx).and_then(|a| a.read(cx).window) {
        let _ = handle.update(cx, f);
    }
}

/// Set while one "Install agent skill" runs.
static SKILL_INSTALL_RUNNING: AtomicBool = AtomicBool::new(false);

/// Holds `flag` set until dropped; `None` if it already was.
struct InFlight(&'static AtomicBool);

impl InFlight {
    fn start(flag: &'static AtomicBool) -> Option<InFlight> {
        (!flag.swap(true, Ordering::SeqCst)).then_some(InFlight(flag))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Runs `fleet skill install` for the current target off the UI thread,
/// then shows the installed path or the error in an alert. Asked again
/// while one runs, it does nothing.
pub(super) fn install_agent_skill(cx: &mut App) {
    let Some(app) = AppState::global(cx) else {
        return;
    };
    let Some(running) = InFlight::start(&SKILL_INSTALL_RUNNING) else {
        eprintln!("fleet-gui: the agent skill install is already running");
        return;
    };
    let (target, state_dir) = {
        let a = app.read(cx);
        (a.target().clone(), a.state_dir().to_owned())
    };
    let run = cx.background_executor().spawn(async move {
        skill_install_command(&target, &state_dir)
            .map_err(|e| e.to_string())
            .and_then(install_skill)
    });
    cx.spawn(async move |cx| {
        let result = run.await;
        drop(running);
        let _ = cx.update(|cx| {
            show_window(cx);
            let (level, message, detail) = match &result {
                Ok(path) => (PromptLevel::Info, "Agent skill installed", path.as_str()),
                Err(e) => (
                    PromptLevel::Critical,
                    "Could not install the agent skill",
                    e.as_str(),
                ),
            };
            with_workspace(cx, |_, window, cx| {
                let answer = window.prompt(level, message, Some(detail), &["OK"], cx);
                cx.spawn(async move |_, _| {
                    let _ = answer.await;
                })
                .detach();
            });
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::InFlight;

    #[test]
    fn second_install_while_one_runs_is_refused() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        let first = InFlight::start(&FLAG);
        assert!(first.is_some());
        assert!(InFlight::start(&FLAG).is_none());
        drop(first);
        assert!(InFlight::start(&FLAG).is_some());
    }
}
