//! The Settings window (issue #28) in the gpui shell: it opens once, and
//! each control writes `config.toml` and takes effect without a restart.
#![cfg(target_os = "macos")]

use std::path::Path;

use fleet_gui::app::{
    AppState, SettingsWindow, TextSetting, Theme, Workspace, open_settings, settings_window,
};
use fleet_gui::core::config::{GuiConfig, load_config};
use fleet_gui::core::glass::{Backdrop, GlassChoice};
use fleet_gui::core::settings::{OptionAsMeta, Section};
use fleet_gui::core::target::Target;
use fleet_gui::core::theme::Appearance;
use gpui::{AppContext, Entity, Modifiers, TestAppContext, VisualTestContext, WindowHandle};

struct Setup {
    app: Entity<AppState>,
    ws: WindowHandle<Workspace>,
    settings: WindowHandle<SettingsWindow>,
}

fn setup(cx: &mut TestAppContext, dir: &Path, config: GuiConfig) -> Setup {
    let dir = dir.to_owned();
    let app = cx.new(|cx| AppState::without_session(Target::Local, dir, config, Theme::dark(), cx));
    cx.update(|cx| AppState::install(app.clone(), cx));
    let state = app.clone();
    let ws = cx.add_window(|window, cx| Workspace::new(state, window, cx));
    cx.update(open_settings);
    cx.run_until_parked();
    let settings = cx
        .update(|cx| settings_window(cx))
        .expect("Settings is open");
    Setup { app, ws, settings }
}

fn click(cx: &mut TestAppContext, s: &Setup, selector: &'static str) {
    let mut vcx = VisualTestContext::from_window(s.settings.into(), cx);
    vcx.run_until_parked();
    let at = vcx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} is drawn"))
        .center();
    vcx.simulate_click(at, Modifiers::none());
    vcx.run_until_parked();
}

fn config(cx: &mut TestAppContext, s: &Setup) -> GuiConfig {
    s.app.read_with(cx, |a, _| a.config().clone())
}

fn show(cx: &mut TestAppContext, s: &Setup, section: &'static str) {
    click(cx, s, section);
}

#[gpui::test]
fn settings_opens_once(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(cx, dir.path(), GuiConfig::default());
    let windows = cx.update(|cx| cx.windows().len());
    assert_eq!(windows, 2, "the workspace and Settings");
    cx.update(open_settings);
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| cx.windows().len()), 2);
    assert_eq!(cx.update(|cx| settings_window(cx)), Some(s.settings));
    let section = s.settings.read_with(cx, |w, _| w.section()).unwrap();
    assert_eq!(section, Section::Appearance);
}

#[gpui::test]
fn font_size_applies_to_open_panes_and_is_saved(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(cx, dir.path(), GuiConfig::default());
    click(cx, &s, "font-size-up");
    assert_eq!(config(cx, &s).font_size, 14.0);
    let pane = s.app.read_with(cx, |a, _| a.focused());
    let size = s.app.read_with(cx, |a, cx| {
        a.pane_view(pane).expect("pane").read(cx).font_size()
    });
    assert_eq!(size, 14.0, "the open pane follows at once");
    assert_eq!(load_config(dir.path()).unwrap().font_size, 14.0);
}

#[gpui::test]
fn theme_choice_overrides_the_system_appearance(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(cx, dir.path(), GuiConfig::default());
    click(cx, &s, "theme-Dark");
    let appearance = s.app.read_with(cx, |a, _| a.theme().appearance);
    assert_eq!(appearance, Appearance::Dark);
    click(cx, &s, "theme-Light");
    let appearance = s.app.read_with(cx, |a, _| a.theme().appearance);
    assert_eq!(appearance, Appearance::Light);
    assert!(
        std::fs::read_to_string(dir.path().join("config.toml"))
            .unwrap()
            .contains("theme = \"light\"")
    );
}

#[gpui::test]
fn window_opacity_tints_the_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(cx, dir.path(), GuiConfig::default());
    let before = s.app.read_with(cx, |a, _| a.theme().window.a);
    click(cx, &s, "opacity-up");
    let after = s.app.read_with(cx, |a, _| a.theme().window.a);
    assert_eq!(config(cx, &s).window_opacity, 0.86);
    assert!(after > before, "{before} -> {after}");
}

#[gpui::test]
fn glass_choice_applies_live_where_the_system_has_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(cx, dir.path(), GuiConfig::default());
    let backdrop = |cx: &mut TestAppContext| s.ws.read_with(cx, |w, _| w.backdrop()).unwrap();
    let before = backdrop(cx);
    assert!(
        matches!(before, Backdrop::Blur | Backdrop::Opaque),
        "off by default: {before:?}"
    );
    click(cx, &s, "glass-Sidebar and bar");
    let c = config(cx, &s);
    if c.glass == GlassChoice::Chrome {
        // macOS 26 without Reduce Transparency: the control is live.
        assert_eq!(backdrop(cx), Backdrop::GlassChrome);
        assert_eq!(load_config(dir.path()).unwrap().glass, GlassChoice::Chrome);
        click(cx, &s, "glass-Window");
        assert_eq!(backdrop(cx), Backdrop::GlassWindow);
        click(cx, &s, "glass-Off");
        assert_eq!(backdrop(cx), Backdrop::Blur);
        assert_eq!(load_config(dir.path()).unwrap().glass, GlassChoice::Off);
    } else {
        // No glass here, or Reduce Transparency: the control is inert.
        assert_eq!(c.glass, GlassChoice::Off);
        assert_eq!(backdrop(cx), before);
    }
}

#[gpui::test]
fn motion_override_beats_the_system_setting(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(cx, dir.path(), GuiConfig::default());
    show(cx, &s, "settings-tab-Behaviour");
    click(cx, &s, "motion-Reduced");
    assert!(s.ws.read_with(cx, |w, _| w.reduces_motion()).unwrap());
    click(cx, &s, "motion-Full");
    assert!(!s.ws.read_with(cx, |w, _| w.reduces_motion()).unwrap());
}

#[gpui::test]
fn switches_toggle_their_settings(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(cx, dir.path(), GuiConfig::default());
    show(cx, &s, "settings-tab-Behaviour");
    click(cx, &s, "switch-confirm-kill");
    click(cx, &s, "switch-notifications");
    let c = config(cx, &s);
    assert!(!c.confirm_kill && c.notifications);
    let saved = load_config(dir.path()).unwrap();
    assert!(!saved.confirm_kill && saved.notifications);
    click(cx, &s, "switch-confirm-kill");
    assert!(config(cx, &s).confirm_kill);
}

#[gpui::test]
fn focus_mode_and_celebration_switches_start_off_and_save(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(cx, dir.path(), GuiConfig::default());
    show(cx, &s, "settings-tab-Behaviour");
    let c = config(cx, &s);
    assert!(!c.focus_mode && !c.celebrate);
    click(cx, &s, "switch-focus-mode");
    click(cx, &s, "switch-celebrate");
    let c = config(cx, &s);
    assert!(c.focus_mode && c.celebrate);
    let saved = load_config(dir.path()).unwrap();
    assert!(saved.focus_mode && saved.celebrate);
}

#[gpui::test]
fn text_fields_apply_on_return_and_when_they_lose_focus(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(cx, dir.path(), GuiConfig::default());
    show(cx, &s, "settings-tab-Agents");
    let mut vcx = VisualTestContext::from_window(s.settings.into(), cx);
    vcx.update(|window, _| window.activate_window());
    let view = s.settings.root(&mut vcx).unwrap();

    // Return applies the focused field.
    vcx.update(|window, cx| {
        view.update(cx, |w, cx| {
            w.set_field_text(
                TextSetting::DefaultCommand,
                "codex --model 'o 3'".into(),
                cx,
            );
            w.focus_field(TextSetting::DefaultCommand, window, cx);
        });
    });
    vcx.run_until_parked();
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    let c = s.app.read_with(&vcx, |a, _| a.config().clone());
    assert_eq!(c.default_command, ["codex", "--model", "o 3"]);

    // Moving to another field applies the one left.
    vcx.update(|window, cx| {
        view.update(cx, |w, cx| {
            w.focus_field(TextSetting::DefaultDir, window, cx);
            w.set_field_text(TextSetting::DefaultDir, "  ~/work ".into(), cx);
        });
    });
    vcx.run_until_parked();
    vcx.update(|window, cx| {
        view.update(cx, |w, cx| {
            w.focus_field(TextSetting::DefaultCommand, window, cx)
        });
    });
    vcx.run_until_parked();
    let c = s.app.read_with(&vcx, |a, _| a.config().clone());
    assert_eq!(c.default_dir, "~/work");
    // The field shows what was kept.
    let shown = view.read_with(&vcx, |w, cx| w.field_text(TextSetting::DefaultDir, cx));
    assert_eq!(shown, "~/work");
    assert_eq!(load_config(dir.path()).unwrap().default_dir, "~/work");
}

#[gpui::test]
fn a_command_that_does_not_parse_is_shown_not_applied(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(cx, dir.path(), GuiConfig::default());
    let view = s.settings.root(cx).unwrap();
    view.update(cx, |w, cx| {
        w.set_field_text(TextSetting::DefaultCommand, "claude 'open".into(), cx);
        w.commit(TextSetting::DefaultCommand, cx);
    });
    let error = view.read_with(cx, |w, _| w.error().map(str::to_owned));
    assert!(error.is_some_and(|e| e.contains("cannot parse")));
    assert_eq!(config(cx, &s).default_command, ["claude"]);
    assert!(!dir.path().join("config.toml").exists());
}

#[gpui::test]
fn opening_settings_reads_edits_made_to_the_file(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(cx, dir.path(), GuiConfig::default());
    s.settings
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    std::fs::write(
        dir.path().join("config.toml"),
        "[gui]\nfont_size = 18\ntheme = \"dark\"\n",
    )
    .unwrap();
    cx.update(open_settings);
    cx.run_until_parked();
    let c = config(cx, &s);
    assert_eq!(
        (c.font_size, c.theme),
        (18.0, fleet_gui::core::settings::ThemeChoice::Dark)
    );
    let reopened = cx.update(|cx| settings_window(cx)).unwrap();
    assert_ne!(reopened, s.settings, "a new window");
}

#[gpui::test]
fn option_as_meta_has_a_four_way_picker(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(cx, dir.path(), GuiConfig::default());
    show(cx, &s, "settings-tab-Behaviour");
    for (selector, want) in [
        ("option-as-meta-Left", OptionAsMeta::Left),
        ("option-as-meta-Right", OptionAsMeta::Right),
        ("option-as-meta-None", OptionAsMeta::None),
        ("option-as-meta-Both", OptionAsMeta::Both),
    ] {
        click(cx, &s, selector);
        assert_eq!(config(cx, &s).option_as_meta, want);
        assert_eq!(load_config(dir.path()).unwrap().option_as_meta, want);
    }
}
