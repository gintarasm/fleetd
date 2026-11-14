use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::state::write_atomic;
use super::target::Target;

/// `[gui]` table of `<state-dir>/config.toml`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct GuiConfig {
    pub connection: Option<Target>,
    pub default_command: Vec<String>,
    pub font_family: String,
    pub font_size: f32,
    pub option_as_meta: bool,
}

impl Default for GuiConfig {
    fn default() -> Self {
        GuiConfig {
            connection: None,
            default_command: vec!["claude".to_owned()],
            font_family: "Menlo".to_owned(),
            font_size: 13.0,
            option_as_meta: true,
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
    if !doc.contains_table("gui") {
        doc["gui"] = toml_edit::table();
    }
    doc["gui"]["connection"] = toml_edit::value(target.to_string());
    write_atomic(&path, doc.to_string().as_bytes(), 0o644)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, text: &str) {
        std::fs::write(dir.join("config.toml"), text).unwrap();
    }

    #[test]
    fn missing_config_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let c = load_config(dir.path()).unwrap();
        assert_eq!(c.default_command, ["claude"]);
        assert_eq!(c.font_family, "Menlo");
        assert_eq!(c.font_size, 13.0);
        assert!(c.option_as_meta);
        assert_eq!(c.connection, None);
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
}
