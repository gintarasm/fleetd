//! The Settings window's model (ADR 0044): its sections, the choices it
//! offers, what each control changes in [`GuiConfig`], and how a change is
//! written back to `[gui]` of `config.toml`. The shell draws it.

use std::io;
use std::path::Path;

use serde::Deserialize;

use super::config::{DEFAULT_FONT_FAMILY, GuiConfig, save_gui_values};
use super::cursor_blink::BlinkChoice;
use super::glass::GlassChoice;
use super::theme::Appearance;

/// The window backdrop's default blur radius in points, a little over
/// AppKit's own (30).
pub const DEFAULT_BLUR: f32 = 40.0;
pub const BLUR_MIN: f32 = 0.0;
pub const BLUR_MAX: f32 = 80.0;
pub const BLUR_STEP: f32 = 10.0;

/// The window tint's default alpha (the theme's own).
pub const DEFAULT_OPACITY: f32 = 0.84;
/// Below this, text on the tint over a bright desktop gets too faint
/// (the theme's contrast tests hold down to it).
pub const OPACITY_MIN: f32 = 0.76;
pub const OPACITY_MAX: f32 = 1.0;
pub const OPACITY_STEP: f32 = 0.02;

pub const FONT_SIZE_MIN: f32 = 8.0;
pub const FONT_SIZE_MAX: f32 = 32.0;
pub const FONT_SIZE_STEP: f32 = 1.0;

/// `[gui] theme`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark];

    pub fn title(self) -> &'static str {
        match self {
            ThemeChoice::System => "System",
            ThemeChoice::Light => "Light",
            ThemeChoice::Dark => "Dark",
        }
    }

    /// The appearance to draw, given the system's.
    pub fn resolve(self, system: Appearance) -> Appearance {
        match self {
            ThemeChoice::System => system,
            ThemeChoice::Light => Appearance::Light,
            ThemeChoice::Dark => Appearance::Dark,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            ThemeChoice::System => "system",
            ThemeChoice::Light => "light",
            ThemeChoice::Dark => "dark",
        }
    }
}

/// `[gui] reduce_motion`: follow System Settings' Reduce Motion, or
/// override it either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MotionChoice {
    #[default]
    System,
    /// Always reduced.
    On,
    /// Always full motion.
    Off,
}

impl MotionChoice {
    pub const ALL: [MotionChoice; 3] = [MotionChoice::System, MotionChoice::On, MotionChoice::Off];

    pub fn title(self) -> &'static str {
        match self {
            MotionChoice::System => "System",
            MotionChoice::On => "Reduced",
            MotionChoice::Off => "Full",
        }
    }

    /// Motion is reduced, given the system setting.
    pub fn resolve(self, system: bool) -> bool {
        match self {
            MotionChoice::System => system,
            MotionChoice::On => true,
            MotionChoice::Off => false,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            MotionChoice::System => "system",
            MotionChoice::On => "on",
            MotionChoice::Off => "off",
        }
    }
}

/// A section of the Settings window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Section {
    #[default]
    Appearance,
    Behaviour,
    Agents,
    Keys,
}

impl Section {
    pub const ALL: [Section; 4] = [
        Section::Appearance,
        Section::Behaviour,
        Section::Agents,
        Section::Keys,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Behaviour => "Behaviour",
            Section::Agents => "Agents",
            Section::Keys => "Keys",
        }
    }
}

/// One control's new value.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    Theme(ThemeChoice),
    Blur(f32),
    WindowOpacity(f32),
    FontFamily(String),
    FontSize(f32),
    ReduceMotion(MotionChoice),
    CursorBlink(BlinkChoice),
    ConfirmKill(bool),
    Notifications(bool),
    OptionAsMeta(bool),
    DefaultCommand(Vec<String>),
    DefaultDir(String),
    FocusMode(bool),
    Celebrate(bool),
    Glass(GlassChoice),
}

/// Rounds to two decimals, so repeated steps do not drift.
fn round2(v: f32) -> f32 {
    (v * 100.0).round() / 100.0
}

fn set<T: PartialEq>(slot: &mut T, value: T, key: &'static str) -> Option<&'static str> {
    if *slot == value {
        return None;
    }
    *slot = value;
    Some(key)
}

impl Change {
    /// Applies the change, clamped to the control's range and trimmed;
    /// returns its `[gui]` key if the config changed.
    pub fn apply(self, c: &mut GuiConfig) -> Option<&'static str> {
        match self {
            Change::Theme(v) => set(&mut c.theme, v, "theme"),
            Change::Blur(v) => set(&mut c.blur, v.round().clamp(BLUR_MIN, BLUR_MAX), "blur"),
            Change::WindowOpacity(v) => set(
                &mut c.window_opacity,
                round2(v).clamp(OPACITY_MIN, OPACITY_MAX),
                "window_opacity",
            ),
            Change::FontFamily(v) => {
                let v = v.trim();
                let v = if v.is_empty() { DEFAULT_FONT_FAMILY } else { v };
                set(&mut c.font_family, v.to_owned(), "font_family")
            }
            Change::FontSize(v) => set(
                &mut c.font_size,
                v.round().clamp(FONT_SIZE_MIN, FONT_SIZE_MAX),
                "font_size",
            ),
            Change::ReduceMotion(v) => set(&mut c.reduce_motion, v, "reduce_motion"),
            Change::CursorBlink(v) => set(&mut c.cursor_blink, v, "cursor_blink"),
            Change::ConfirmKill(v) => set(&mut c.confirm_kill, v, "confirm_kill"),
            Change::Notifications(v) => set(&mut c.notifications, v, "notifications"),
            Change::OptionAsMeta(v) => set(&mut c.option_as_meta, v, "option_as_meta"),
            Change::DefaultCommand(v) => set(&mut c.default_command, v, "default_command"),
            Change::DefaultDir(v) => set(&mut c.default_dir, v.trim().to_owned(), "default_dir"),
            Change::FocusMode(v) => set(&mut c.focus_mode, v, "focus_mode"),
            Change::Celebrate(v) => set(&mut c.celebrate, v, "celebrate"),
            Change::Glass(v) => set(&mut c.glass, v, "glass"),
        }
    }
}

/// The value of a `[gui]` key the Settings window writes.
fn value_of(c: &GuiConfig, key: &str) -> Option<toml_edit::Value> {
    let float = |v: f32| -> toml_edit::Value {
        // Whole numbers are written as integers (`font_size = 14`).
        if v.fract() == 0.0 {
            toml_edit::Value::from(v as i64)
        } else {
            toml_edit::Value::from(f64::from(round2(v)))
        }
    };
    Some(match key {
        "theme" => c.theme.as_str().into(),
        "blur" => float(c.blur),
        "window_opacity" => float(c.window_opacity),
        "font_family" => c.font_family.as_str().into(),
        "font_size" => float(c.font_size),
        "reduce_motion" => c.reduce_motion.as_str().into(),
        "cursor_blink" => c.cursor_blink.as_str().into(),
        "confirm_kill" => c.confirm_kill.into(),
        "notifications" => c.notifications.into(),
        "option_as_meta" => c.option_as_meta.into(),
        "default_command" => c
            .default_command
            .iter()
            .map(String::as_str)
            .collect::<toml_edit::Array>()
            .into(),
        "default_dir" => c.default_dir.as_str().into(),
        "focus_mode" => c.focus_mode.into(),
        "celebrate" => c.celebrate.into(),
        "glass" => c.glass.as_str().into(),
        _ => return None,
    })
}

/// Writes `keys` of `c` to `[gui]`, keeping comments and every other key.
pub fn save_changes(state_dir: &Path, c: &GuiConfig, keys: &[&str]) -> io::Result<()> {
    let values = keys
        .iter()
        .filter_map(|k| Some((*k, value_of(c, k)?)))
        .collect();
    save_gui_values(state_dir, values)
}

/// The default command as typed: split with shell quoting rules.
pub fn parse_command_line(line: &str) -> Result<Vec<String>, String> {
    let words = shell_words::split(line).map_err(|e| format!("cannot parse the command: {e}"))?;
    if words.is_empty() {
        return Err("enter a command".into());
    }
    Ok(words)
}

/// A keyboard shortcut, as listed in the Keys section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortcutRow {
    /// The binding, in gpui's keystroke syntax (`cmd-shift-d`).
    pub keys: &'static str,
    pub what: &'static str,
}

const fn row(keys: &'static str, what: &'static str) -> ShortcutRow {
    ShortcutRow { keys, what }
}

/// The app's shortcuts (`app::keymap` binds them; its tests check that
/// every one listed here is bound).
pub const SHORTCUTS: &[ShortcutRow] = &[
    row("cmd-n", "New agent"),
    row("cmd-t", "New terminal"),
    row("cmd-d", "Split right"),
    row("cmd-shift-d", "Split down"),
    row("cmd-w", "Close pane"),
    row("cmd-shift-enter", "Maximize or restore pane"),
    row("cmd-r", "Restart the pane's agent"),
    row("cmd-1", "Focus pane 1 (to 4: ⌘4)"),
    row("cmd-alt-left", "Focus the pane to the left"),
    row("cmd-alt-right", "Focus the pane to the right"),
    row("cmd-alt-up", "Focus the pane above"),
    row("cmd-alt-down", "Focus the pane below"),
    row("cmd-c", "Copy"),
    row("cmd-v", "Paste"),
    row("cmd-+", "Bigger font (all panes, not saved)"),
    row("cmd--", "Smaller font"),
    row("cmd-0", "Font size from Settings"),
    row("cmd-,", "Settings"),
    row("cmd-q", "Quit (agents keep running)"),
];

/// A binding in the macOS menu notation: `cmd-shift-d` is `⇧⌘D`.
pub fn key_label(keys: &str) -> String {
    let mut parts: Vec<&str> = keys.split('-').collect();
    // `cmd--` is ⌘ and the minus key.
    let key = if keys.ends_with("--") {
        parts.truncate(parts.len().saturating_sub(2));
        "-"
    } else {
        parts.pop().unwrap_or("")
    };
    let mut out = String::new();
    for (name, sym) in [("ctrl", "⌃"), ("alt", "⌥"), ("shift", "⇧"), ("cmd", "⌘")] {
        if parts.contains(&name) {
            out.push_str(sym);
        }
    }
    let key = match key {
        "left" => "←".to_owned(),
        "right" => "→".to_owned(),
        "up" => "↑".to_owned(),
        "down" => "↓".to_owned(),
        "enter" => "↩".to_owned(),
        "tab" => "⇥".to_owned(),
        "escape" => "⎋".to_owned(),
        "space" => "Space".to_owned(),
        k => k.to_uppercase(),
    };
    out.push_str(&key);
    out
}
