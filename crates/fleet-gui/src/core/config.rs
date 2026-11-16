use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::settings::{DEFAULT_BLUR, DEFAULT_OPACITY, MotionChoice, ThemeChoice};
use super::state::write_atomic;
use super::target::Target;

/// The terminal font when the config names none: SF Mono, the macOS
/// terminal face. It is rarely installed for apps (only with Apple's
/// developer download), but every Mac carries its static faces inside
/// Terminal.app ([`SF_MONO_DIR`]); the GUI registers those with its own
/// text system (see [`sf_mono_files`]).
pub const DEFAULT_FONT_FAMILY: &str = "SF Mono";

/// Where Terminal.app keeps SF Mono (macOS 10.15 and later).
pub const SF_MONO_DIR: &str =
    "/System/Applications/Utilities/Terminal.app/Contents/Resources/Fonts";

/// The SF Mono faces a terminal draws with: regular, bold, italic, bold
/// italic.
pub const SF_MONO_FACES: [&str; 4] = [
    "SF-Mono-Regular.otf",
    "SF-Mono-Bold.otf",
    "SF-Mono-RegularItalic.otf",
    "SF-Mono-BoldItalic.otf",
];

/// The font files to register for the configured terminal family: SF
/// Mono's faces from `dir` when the family is SF Mono, else none (other
/// families come from the system as installed).
pub fn sf_mono_files(configured: &str, dir: &Path) -> Vec<PathBuf> {
    if configured.trim() != DEFAULT_FONT_FAMILY {
        return Vec::new();
    }
    SF_MONO_FACES.iter().map(|f| dir.join(f)).collect()
}
/// Used when the configured family (or SF Mono) is not installed.
pub const FALLBACK_FONT_FAMILY: &str = "Menlo";

/// The terminal font families to try, in order: the configured one, then
/// Menlo (always on macOS).
pub fn font_families(configured: &str) -> Vec<&str> {
    let configured = configured.trim();
    let mut out = Vec::with_capacity(2);
    if !configured.is_empty() {
        out.push(configured);
    }
    if configured != FALLBACK_FONT_FAMILY {
        out.push(FALLBACK_FONT_FAMILY);
    }
    out
}

/// `[gui]` table of `<state-dir>/config.toml`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct GuiConfig {
    pub connection: Option<Target>,
    pub default_command: Vec<String>,
    pub font_family: String,
    pub font_size: f32,
    pub option_as_meta: bool,
    /// Light, dark, or the system's appearance.
    pub theme: ThemeChoice,
    /// The window backdrop's blur radius, in points.
    pub blur: f32,
    /// The alpha of the window tint over the blurred desktop.
    pub window_opacity: f32,
    /// Overrides System Settings' Reduce Motion, or follows it.
    pub reduce_motion: MotionChoice,
    /// Kill asks first.
    pub confirm_kill: bool,
    /// macOS notifications when an agent needs you or ends while the
    /// window is not focused (ADR 0042). Off unless asked for.
    pub notifications: bool,
    /// The New agent sheet's directory; empty: the most recent one.
    pub default_dir: String,
    /// The command line that opens ⌘-clicked files (`zed`, `code -r`);
    /// blank means `$VISUAL`, then `$EDITOR` (ADR 0046).
    pub editor: String,
}

impl Default for GuiConfig {
    fn default() -> Self {
        GuiConfig {
            connection: None,
            default_command: vec!["claude".to_owned()],
            font_family: DEFAULT_FONT_FAMILY.to_owned(),
            font_size: 13.0,
            option_as_meta: true,
            theme: ThemeChoice::System,
            blur: DEFAULT_BLUR,
            window_opacity: DEFAULT_OPACITY,
            reduce_motion: MotionChoice::System,
            confirm_kill: true,
            notifications: false,
            default_dir: String::new(),
            editor: String::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {}: {source}", path.display())]
    Read { path: PathBuf, source: io::Error },
    /// The toml error's own text carries the line and column.
    #[error("{}: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

#[derive(Deserialize, Default)]
struct File {
    #[serde(default)]
    gui: GuiConfig,
}

fn config_path(state_dir: &Path) -> PathBuf {
    state_dir.join("config.toml")
}

/// Missing file or missing `[gui]` gives defaults; a malformed file is an error.
pub fn load_config(state_dir: &Path) -> Result<GuiConfig, ConfigError> {
    let path = config_path(state_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(GuiConfig::default()),
        Err(source) => return Err(ConfigError::Read { path, source }),
    };
    toml::from_str::<File>(&text)
        .map(|f| f.gui)
        .map_err(|source| ConfigError::Parse { path, source })
}

/// Sets `[gui] connection`, leaving comments and every other key as they were.
pub fn save_connection(state_dir: &Path, target: &Target) -> io::Result<()> {
    save_gui_values(
        state_dir,
        vec![("connection", toml_edit::Value::from(target.to_string()))],
    )
}

/// Sets `[gui] confirm_kill`, leaving comments and every other key as they
/// were.
pub fn save_confirm_kill(state_dir: &Path, confirm: bool) -> io::Result<()> {
    save_gui_values(
        state_dir,
        vec![("confirm_kill", toml_edit::Value::from(confirm))],
    )
}

/// Sets `[gui] notifications`, leaving comments and every other key as they were.
pub fn save_notifications(state_dir: &Path, on: bool) -> io::Result<()> {
    save_gui_values(
        state_dir,
        vec![("notifications", toml_edit::Value::from(on))],
    )
}

/// Sets these keys of `[gui]`, leaving comments and every other key as
/// they were; creates the file (and the table) when missing. The file's
/// mode is kept.
pub fn save_gui_values(state_dir: &Path, values: Vec<(&str, toml_edit::Value)>) -> io::Result<()> {
    let path = config_path(state_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {e}", path.display()),
        )
    })?;
    if doc.get("gui").and_then(|i| i.as_table_like()).is_none() {
        doc["gui"] = toml_edit::table();
    }
    let gui = doc["gui"].as_table_like_mut().expect("gui is a table");
    for (key, value) in values {
        // Keep a trailing comment on a key that was already there.
        match gui.get_mut(key).and_then(|i| i.as_value_mut()) {
            Some(old) => {
                let decor = old.decor().clone();
                *old = value;
                *old.decor_mut() = decor;
            }
            None => {
                gui.insert(key, toml_edit::Item::Value(value));
            }
        }
    }
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).map_or(0o644, |m| m.permissions().mode() & 0o777);
    write_atomic(&path, doc.to_string().as_bytes(), mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, text: &str) {
        std::fs::write(dir.join("config.toml"), text).unwrap();
    }

    #[test]
    fn sf_mono_is_registered_from_terminal_only_when_configured() {
        let dir = Path::new("/T");
        let files = sf_mono_files("SF Mono", dir);
        assert_eq!(files.len(), 4);
        assert_eq!(files[0], Path::new("/T/SF-Mono-Regular.otf"));
        assert!(
            files
                .iter()
                .all(|f| f.extension().is_some_and(|e| e == "otf"))
        );
        assert!(sf_mono_files("Menlo", dir).is_empty());
        assert!(sf_mono_files("Iosevka", dir).is_empty());
    }

    #[test]
    fn font_falls_back_to_menlo() {
        assert_eq!(font_families("SF Mono"), ["SF Mono", "Menlo"]);
        assert_eq!(font_families("Iosevka"), ["Iosevka", "Menlo"]);
        assert_eq!(font_families("Menlo"), ["Menlo"]);
        assert_eq!(font_families("  "), ["Menlo"]);
    }

    #[test]
    fn missing_config_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let c = load_config(dir.path()).unwrap();
        assert_eq!(c.default_command, ["claude"]);
        assert_eq!(c.font_family, "SF Mono");
        assert_eq!(c.font_size, 13.0);
        assert!(c.option_as_meta);
        assert!(!c.notifications);
        assert_eq!(c.connection, None);
        assert_eq!(c.editor, "");
    }

    #[test]
    fn config_reads_the_editor() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "[gui]\neditor = \"zed\"\n");
        assert_eq!(load_config(dir.path()).unwrap().editor, "zed");
    }

    #[test]
    fn config_reads_gui_table() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "[gui]\ndefault_command = [\"codex\", \"--yolo\"]\nconnection = \"ssh://me@box\"\n",
        );
        let c = load_config(dir.path()).unwrap();
        assert_eq!(c.default_command, ["codex", "--yolo"]);
        assert_eq!(
            c.connection,
            Some(Target::Ssh {
                dest: "me@box".into()
            })
        );
        assert_eq!(c.font_size, 13.0);
    }

    #[test]
    fn malformed_config_error_names_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "[gui\nfont_size = 1\n");
        let e = load_config(dir.path()).unwrap_err().to_string();
        assert!(e.contains("config.toml"), "{e}");
        assert!(e.contains("line 1"), "{e}");
    }

    #[test]
    fn save_connection_preserves_comments_and_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "# mine\n[gui]\nfont_size = 15 # big\n[other]\nx = 1\n",
        );
        save_connection(dir.path(), &Target::Local).unwrap();
        let out = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(out.contains("# mine"), "{out}");
        assert!(out.contains("font_size = 15 # big"), "{out}");
        assert!(out.contains("[other]"), "{out}");
        assert!(out.contains("connection = \"local\""), "{out}");
        assert_eq!(
            load_config(dir.path()).unwrap().connection,
            Some(Target::Local)
        );
    }

    #[test]
    fn save_connection_preserves_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "[gui]\nfont_size = 15\n");
        let path = dir.path().join("config.toml");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        save_connection(dir.path(), &Target::Local).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn save_connection_keeps_inline_gui_table_keys() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "gui = { font_size = 15 }\n");
        save_connection(dir.path(), &Target::Local).unwrap();
        let c = load_config(dir.path()).unwrap();
        assert_eq!(c.font_size, 15.0);
        assert_eq!(c.connection, Some(Target::Local));
    }

    #[test]
    fn save_notifications_round_trips_and_keeps_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "[gui]\nfont_size = 15 # big\n");
        save_notifications(dir.path(), true).unwrap();
        let c = load_config(dir.path()).unwrap();
        assert!(c.notifications);
        assert_eq!(c.font_size, 15.0);
        save_notifications(dir.path(), false).unwrap();
        assert!(!load_config(dir.path()).unwrap().notifications);
        let out = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(out.contains("font_size = 15 # big"), "{out}");
    }
}
