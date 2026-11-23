//! The Settings window's model (issue #28): what each control changes in
//! `[gui]` of `config.toml`, and how it is written back.

use std::path::Path;

use fleet_gui::core::config::{GuiConfig, load_config};
use fleet_gui::core::cursor_blink::BlinkChoice;
use fleet_gui::core::glass::GlassChoice;
use fleet_gui::core::settings::{
    BLUR_MAX, BLUR_MIN, Change, FONT_SIZE_MAX, FONT_SIZE_MIN, MotionChoice, OPACITY_MAX,
    OPACITY_MIN, OptionAsMeta, SHORTCUTS, Section, ThemeChoice, key_label, parse_command_line,
    save_changes,
};
use fleet_gui::core::theme::Appearance;

fn write(dir: &Path, text: &str) {
    std::fs::write(dir.join("config.toml"), text).unwrap();
}

fn read(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("config.toml")).unwrap()
}

#[test]
fn defaults_follow_the_system_and_keep_todays_look() {
    let c = GuiConfig::default();
    assert_eq!(c.theme, ThemeChoice::System);
    assert_eq!(c.reduce_motion, MotionChoice::System);
    assert_eq!(c.blur, 40.0);
    assert_eq!(c.window_opacity, 0.84);
    assert!(c.confirm_kill);
    assert!(!c.notifications, "off unless asked for (ADR 0042)");
    assert_eq!(c.default_dir, "");
    assert_eq!(c.glass, GlassChoice::Off, "glass stays off until decided");
}

#[test]
fn new_keys_are_read_from_the_gui_table() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "[gui]\ntheme = \"light\"\nreduce_motion = \"on\"\nblur = 20\nwindow_opacity = 0.9\n\
         confirm_kill = false\nnotifications = false\ndefault_dir = \"~/src\"\n",
    );
    let c = load_config(dir.path()).unwrap();
    assert_eq!(c.theme, ThemeChoice::Light);
    assert_eq!(c.reduce_motion, MotionChoice::On);
    assert_eq!(c.blur, 20.0);
    assert_eq!(c.window_opacity, 0.9);
    assert!(!c.confirm_kill);
    assert!(!c.notifications);
    assert_eq!(c.default_dir, "~/src");
}

#[test]
fn an_unknown_theme_is_an_error_naming_the_file() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "[gui]\ntheme = \"sepia\"\n");
    let e = load_config(dir.path()).unwrap_err().to_string();
    assert!(e.contains("config.toml"), "{e}");
}

#[test]
fn theme_and_motion_choices_resolve_against_the_system() {
    assert_eq!(
        ThemeChoice::System.resolve(Appearance::Light),
        Appearance::Light
    );
    assert_eq!(
        ThemeChoice::Dark.resolve(Appearance::Light),
        Appearance::Dark
    );
    assert_eq!(
        ThemeChoice::Light.resolve(Appearance::Dark),
        Appearance::Light
    );
    assert!(MotionChoice::System.resolve(true));
    assert!(!MotionChoice::System.resolve(false));
    assert!(MotionChoice::On.resolve(false));
    assert!(!MotionChoice::Off.resolve(true));
    assert_eq!(
        ThemeChoice::ALL.map(ThemeChoice::title),
        ["System", "Light", "Dark"]
    );
    assert_eq!(
        MotionChoice::ALL.map(MotionChoice::title),
        ["System", "Reduced", "Full"]
    );
}

#[test]
fn a_change_reports_its_key_only_when_it_changes_something() {
    let mut c = GuiConfig::default();
    assert_eq!(
        Change::Theme(ThemeChoice::Dark).apply(&mut c),
        Some("theme")
    );
    assert_eq!(c.theme, ThemeChoice::Dark);
    assert_eq!(Change::Theme(ThemeChoice::Dark).apply(&mut c), None);
    assert_eq!(
        Change::ConfirmKill(false).apply(&mut c),
        Some("confirm_kill")
    );
    assert_eq!(
        Change::OptionAsMeta(OptionAsMeta::None).apply(&mut c),
        Some("option_as_meta")
    );
    assert_eq!(
        Change::Notifications(true).apply(&mut c),
        Some("notifications")
    );
    assert_eq!(
        Change::ReduceMotion(MotionChoice::Off).apply(&mut c),
        Some("reduce_motion")
    );
    assert_eq!(
        Change::DefaultCommand(vec!["codex".into()]).apply(&mut c),
        Some("default_command")
    );
    assert_eq!(c.default_command, ["codex"]);
}

#[test]
fn numbers_step_within_their_ranges() {
    let mut c = GuiConfig::default();
    assert_eq!(
        Change::FontSize(13.0 + 1.0).apply(&mut c),
        Some("font_size")
    );
    assert_eq!(c.font_size, 14.0);
    Change::FontSize(200.0).apply(&mut c);
    assert_eq!(c.font_size, FONT_SIZE_MAX);
    Change::FontSize(1.0).apply(&mut c);
    assert_eq!(c.font_size, FONT_SIZE_MIN);
    assert_eq!(Change::FontSize(FONT_SIZE_MIN - 1.0).apply(&mut c), None);

    Change::Blur(500.0).apply(&mut c);
    assert_eq!(c.blur, BLUR_MAX);
    Change::Blur(-5.0).apply(&mut c);
    assert_eq!(c.blur, BLUR_MIN);

    Change::WindowOpacity(2.0).apply(&mut c);
    assert_eq!(c.window_opacity, OPACITY_MAX);
    Change::WindowOpacity(0.1).apply(&mut c);
    assert_eq!(c.window_opacity, OPACITY_MIN);
    // Stepping never drifts off two decimals.
    Change::WindowOpacity(0.84 - 0.04).apply(&mut c);
    assert_eq!(c.window_opacity, 0.8);
}

#[test]
fn text_settings_are_trimmed() {
    let mut c = GuiConfig::default();
    assert_eq!(
        Change::FontFamily("  Iosevka ".into()).apply(&mut c),
        Some("font_family")
    );
    assert_eq!(c.font_family, "Iosevka");
    // An empty family means the default face, not an empty name.
    Change::FontFamily("   ".into()).apply(&mut c);
    assert_eq!(c.font_family, "SF Mono");
    Change::DefaultDir("  ~/work  ".into()).apply(&mut c);
    assert_eq!(c.default_dir, "~/work");
}

#[test]
fn the_default_command_is_parsed_like_a_shell_line() {
    assert_eq!(
        parse_command_line("codex --model 'o 3'").unwrap(),
        ["codex", "--model", "o 3"]
    );
    assert!(parse_command_line("   ").is_err());
    assert!(parse_command_line("codex 'open").is_err());
}

#[test]
fn saving_writes_only_the_changed_keys_and_keeps_comments() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "# mine\n[gui]\nfont_size = 15 # big\nconnection = \"local\"\n[other]\nx = 1\n",
    );
    let mut c = load_config(dir.path()).unwrap();
    let keys: Vec<&str> = [
        Change::Theme(ThemeChoice::Dark),
        Change::DefaultCommand(vec!["codex".into(), "--yolo".into()]),
        Change::WindowOpacity(0.9),
    ]
    .into_iter()
    .filter_map(|ch| ch.apply(&mut c))
    .collect();
    save_changes(dir.path(), &c, &keys).unwrap();
    let out = read(dir.path());
    assert!(out.contains("# mine"), "{out}");
    assert!(out.contains("font_size = 15 # big"), "{out}");
    assert!(out.contains("[other]"), "{out}");
    assert!(out.contains("theme = \"dark\""), "{out}");
    assert!(!out.contains("blur"), "unchanged keys stay out: {out}");
    assert_eq!(load_config(dir.path()).unwrap(), c);
}

#[test]
fn saving_creates_the_file_when_missing() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = GuiConfig::default();
    let key = Change::Blur(20.0).apply(&mut c).unwrap();
    save_changes(dir.path(), &c, &[key]).unwrap();
    assert_eq!(load_config(dir.path()).unwrap().blur, 20.0);
}

#[test]
fn sections_are_appearance_behaviour_agents_keys() {
    assert_eq!(
        Section::ALL.map(Section::title),
        ["Appearance", "Behaviour", "Agents", "Keys"]
    );
}

#[test]
fn shortcuts_are_listed_with_mac_symbols() {
    assert_eq!(key_label("cmd-shift-d"), "⇧⌘D");
    assert_eq!(key_label("cmd-alt-left"), "⌥⌘←");
    assert_eq!(key_label("cmd-shift-enter"), "⇧⌘↩");
    assert_eq!(key_label("cmd-,"), "⌘,");
    assert!(SHORTCUTS.iter().any(|s| s.keys == "cmd-,"));
    assert!(SHORTCUTS.iter().all(|s| !s.what.is_empty()));
}

#[test]
fn focus_mode_and_celebration_are_off_by_default_and_saved() {
    let mut c = GuiConfig::default();
    assert!(!c.focus_mode);
    assert!(!c.celebrate, "the celebration is opt-in (#44)");
    let mut keys = Vec::new();
    keys.extend(Change::FocusMode(true).apply(&mut c));
    keys.extend(Change::Celebrate(true).apply(&mut c));
    assert_eq!(keys, vec!["focus_mode", "celebrate"]);
    assert_eq!(Change::FocusMode(true).apply(&mut c), None);
    let dir = tempfile::tempdir().unwrap();
    save_changes(dir.path(), &c, &keys).unwrap();
    let text = read(dir.path());
    assert!(text.contains("focus_mode = true"), "{text}");
    assert!(text.contains("celebrate = true"), "{text}");
    let back = load_config(dir.path()).unwrap();
    assert!(back.focus_mode && back.celebrate);
}

#[test]
fn glass_is_read_written_and_rejects_unknown_values() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "[gui]\nglass = \"chrome\"\n");
    assert_eq!(load_config(dir.path()).unwrap().glass, GlassChoice::Chrome);
    let mut c = GuiConfig::default();
    assert_eq!(Change::Glass(GlassChoice::Off).apply(&mut c), None);
    let key = Change::Glass(GlassChoice::Window).apply(&mut c).unwrap();
    assert_eq!(key, "glass");
    save_changes(dir.path(), &c, &[key]).unwrap();
    assert!(read(dir.path()).contains("glass = \"window\""));
    assert_eq!(load_config(dir.path()).unwrap().glass, GlassChoice::Window);
    write(dir.path(), "[gui]\nglass = \"frosted\"\n");
    assert!(load_config(dir.path()).is_err());
}

#[test]
fn cursor_blink_follows_the_app_by_default_and_is_saved() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        load_config(dir.path()).unwrap().cursor_blink,
        BlinkChoice::App
    );
    write(dir.path(), "[gui]\ncursor_blink = \"off\"\n");
    assert_eq!(
        load_config(dir.path()).unwrap().cursor_blink,
        BlinkChoice::Off
    );
    let mut c = GuiConfig::default();
    assert_eq!(Change::CursorBlink(BlinkChoice::App).apply(&mut c), None);
    let key = Change::CursorBlink(BlinkChoice::On).apply(&mut c).unwrap();
    assert_eq!(key, "cursor_blink");
    save_changes(dir.path(), &c, &[key]).unwrap();
    assert!(read(dir.path()).contains("cursor_blink = \"on\""));
    assert_eq!(
        load_config(dir.path()).unwrap().cursor_blink,
        BlinkChoice::On
    );
    write(dir.path(), "[gui]\ncursor_blink = \"sometimes\"\n");
    assert!(load_config(dir.path()).is_err());
}

#[test]
fn option_as_meta_defaults_to_both_and_reads_each_side() {
    assert_eq!(GuiConfig::default().option_as_meta, OptionAsMeta::Both);
    for (text, want) in [
        ("\"left\"", OptionAsMeta::Left),
        ("\"right\"", OptionAsMeta::Right),
        ("\"both\"", OptionAsMeta::Both),
        ("\"none\"", OptionAsMeta::None),
        ("true", OptionAsMeta::Both),
        ("false", OptionAsMeta::None),
    ] {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &format!("[gui]\noption_as_meta = {text}\n"));
        let c = load_config(dir.path()).unwrap();
        assert_eq!(c.option_as_meta, want, "option_as_meta = {text}");
    }
}

#[test]
fn option_as_meta_follows_the_pressed_side() {
    // (left down, right down)
    let rows = [
        (
            OptionAsMeta::Both,
            [(true, false), (false, true)],
            [true, true],
        ),
        (
            OptionAsMeta::Left,
            [(true, false), (false, true)],
            [true, false],
        ),
        (
            OptionAsMeta::Right,
            [(true, false), (false, true)],
            [false, true],
        ),
        (
            OptionAsMeta::None,
            [(true, false), (false, true)],
            [false, false],
        ),
    ];
    for (mode, presses, want) in rows {
        for (i, (l, r)) in presses.into_iter().enumerate() {
            assert_eq!(mode.applies(l, r), want[i], "{mode:?} left={l} right={r}");
        }
    }
    // Side unknown (neither device bit): only Both can say yes.
    assert!(OptionAsMeta::Both.applies(false, false));
    assert!(!OptionAsMeta::Left.applies(false, false));
    // Both Options held: either side's mode applies.
    assert!(OptionAsMeta::Left.applies(true, true));
    assert!(OptionAsMeta::Right.applies(true, true));
}

#[test]
fn option_as_meta_is_saved_as_a_string() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "[gui]\noption_as_meta = true\n");
    let mut c = load_config(dir.path()).unwrap();
    assert_eq!(
        Change::OptionAsMeta(OptionAsMeta::Right).apply(&mut c),
        Some("option_as_meta")
    );
    save_changes(dir.path(), &c, &["option_as_meta"]).unwrap();
    assert!(read(dir.path()).contains("option_as_meta = \"right\""));
    assert_eq!(
        load_config(dir.path()).unwrap().option_as_meta,
        OptionAsMeta::Right
    );
}
