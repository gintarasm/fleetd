//! Font zoom (issue #38) in the gpui shell: ⌘+ / ⌘− / ⌘0 change every
//! pane's font at once, the grid reflows, and nothing is saved.
#![cfg(target_os = "macos")]

use fleet_gui::app::{AppState, Theme, Workspace, bind_keys};
use fleet_gui::core::config::{GuiConfig, load_config};
use fleet_gui::core::target::Target;
use gpui::{AppContext, Entity, TestAppContext, VisualTestContext};

fn setup(cx: &mut TestAppContext, dir: &std::path::Path) -> (Entity<AppState>, VisualTestContext) {
    cx.update(bind_keys);
    let dir = dir.to_owned();
    let app = cx.new(|cx| {
        AppState::without_session(Target::Local, dir, GuiConfig::default(), Theme::dark(), cx)
    });
    cx.update(|cx| AppState::install(app.clone(), cx));
    let state = app.clone();
    let ws = cx.add_window(|window, cx| Workspace::new(state, window, cx));
    let mut vcx = VisualTestContext::from_window(ws.into(), cx);
    vcx.update(|window, _| window.activate_window());
    vcx.run_until_parked();
    (app, vcx)
}

/// The focused pane's font size and grid.
fn pane(app: &Entity<AppState>, vcx: &mut VisualTestContext) -> (f32, (u16, u16)) {
    vcx.run_until_parked();
    app.read_with(vcx, |a, cx| {
        let v = a.pane_view(a.focused()).expect("pane").read(cx);
        (v.font_size(), v.grid().expect("laid out"))
    })
}

#[gpui::test]
fn cmd_plus_minus_and_zero_zoom_the_panes(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (app, mut vcx) = setup(cx, dir.path());
    let (size, grid) = pane(&app, &mut vcx);
    assert_eq!(size, 13.0);

    vcx.simulate_keystrokes("cmd-= cmd-=");
    let (bigger, bigger_grid) = pane(&app, &mut vcx);
    assert_eq!(bigger, 15.0);
    assert!(
        bigger_grid.0 < grid.0 && bigger_grid.1 < grid.1,
        "the grid reflows: {grid:?} -> {bigger_grid:?}"
    );
    assert_eq!(app.read_with(&vcx, |a, _| a.shown_font_size()), 15.0);

    vcx.simulate_keystrokes("cmd--");
    assert_eq!(pane(&app, &mut vcx).0, 14.0);

    vcx.simulate_keystrokes("cmd-0");
    assert_eq!(
        pane(&app, &mut vcx),
        (13.0, grid),
        "back to the configured size"
    );

    // ⌘+ typed with Shift zooms too.
    vcx.simulate_keystrokes("cmd-+");
    assert_eq!(pane(&app, &mut vcx).0, 14.0);

    // Not a setting: the config and the file keep the configured size.
    assert_eq!(app.read_with(&vcx, |a, _| a.config().font_size), 13.0);
    assert_eq!(load_config(dir.path()).unwrap().font_size, 13.0);
    assert!(!dir.path().join("config.toml").exists(), "nothing written");
}

#[gpui::test]
fn a_font_size_chosen_in_settings_ends_the_zoom(cx: &mut TestAppContext) {
    use fleet_gui::core::settings::Change;
    let dir = tempfile::tempdir().unwrap();
    let (app, mut vcx) = setup(cx, dir.path());
    vcx.simulate_keystrokes("cmd-= cmd-= cmd-=");
    assert_eq!(pane(&app, &mut vcx).0, 16.0);
    app.update(&mut vcx, |a, cx| {
        a.change_settings(vec![Change::FontSize(12.0)], cx)
    })
    .unwrap();
    assert_eq!(pane(&app, &mut vcx).0, 12.0, "what Settings chose shows");
    vcx.simulate_keystrokes("cmd-=");
    assert_eq!(pane(&app, &mut vcx).0, 13.0, "zoom steps from there");
}
