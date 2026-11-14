use std::collections::BTreeMap;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use super::layout::SavedLayout;
use super::target::Target;

pub const MAX_RECENT_TARGETS: usize = 10;
pub const MAX_RECENT_DIRS: usize = 20;

/// `$HOME/.fleet`.
pub fn default_state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".fleet")
}

/// GUI-owned state in `<state-dir>/gui-state.toml`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiState {
    /// Entries that do not parse are dropped.
    #[serde(deserialize_with = "lenient_targets")]
    pub recent_targets: Vec<Target>,
    /// Keyed by target string.
    pub recent_dirs: BTreeMap<String, Vec<String>>,
    /// Keyed by target string. Entries that do not parse (old or unknown shape) are dropped.
    #[serde(deserialize_with = "lenient_layouts")]
    pub layouts: BTreeMap<String, SavedLayout>,
}

fn lenient_targets<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Target>, D::Error> {
    let raw = Vec::<toml::Value>::deserialize(d)?;
    Ok(raw.into_iter().filter_map(|v| v.try_into().ok()).collect())
}

fn lenient_layouts<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, SavedLayout>, D::Error> {
    let raw = BTreeMap::<String, toml::Value>::deserialize(d)?;
    Ok(raw
        .into_iter()
        .filter_map(|(k, v)| v.try_into().ok().map(|l| (k, l)))
        .collect())
}

fn state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("gui-state.toml")
}

impl GuiState {
    /// A missing or corrupt file gives the default (corruption is reported on stderr).
    pub fn load(state_dir: &Path) -> GuiState {
        let path = state_path(state_dir);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return GuiState::default(),
            Err(e) => {
                eprintln!("fleet-gui: cannot read {}: {e}", path.display());
                return GuiState::default();
            }
        };
        toml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("fleet-gui: ignoring corrupt {}: {e}", path.display());
            GuiState::default()
        })
    }

    /// Atomic (temp file + rename), mode 0600.
    pub fn save(&self, state_dir: &Path) -> io::Result<()> {
        let text = toml::to_string(self).map_err(io::Error::other)?;
        write_atomic(&state_path(state_dir), text.as_bytes(), 0o600)
    }
}

/// Moves `item` to the front, removing any earlier equal entry, and keeps at most `cap`.
pub fn push_recent<T: PartialEq>(list: &mut Vec<T>, item: T, cap: usize) {
    list.retain(|x| *x != item);
    list.insert(0, item);
    list.truncate(cap);
}

/// Writes `bytes` to a temp file next to `path` (created with `mode`), then renames it over `path`.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn state_roundtrip_is_0600_and_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = GuiState::default();
        s.recent_targets.push(Target::Ssh {
            dest: "me@box".into(),
        });
        s.recent_targets.push(Target::Local);
        s.recent_dirs
            .insert("local".into(), vec!["/a".into(), "/b".into()]);
        s.layouts.insert("local".into(), SavedLayout::default());
        s.save(dir.path()).unwrap();
        assert_eq!(GuiState::load(dir.path()), s);
        let mode = std::fs::metadata(dir.path().join("gui-state.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["gui-state.toml"]);
    }

    #[test]
    fn corrupt_state_loads_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gui-state.toml"), "recent_targets = [[[").unwrap();
        assert_eq!(GuiState::load(dir.path()), GuiState::default());
    }

    #[test]
    fn unknown_layout_entry_is_dropped_rest_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gui-state.toml"),
            "recent_targets = [\"local\"]\n[layouts]\nlocal = \"old\"\n",
        )
        .unwrap();
        let s = GuiState::load(dir.path());
        assert_eq!(s.recent_targets, [Target::Local]);
        assert!(s.layouts.is_empty());
    }

    #[test]
    fn push_recent_moves_to_front_dedupes_and_caps() {
        let mut v = vec![1, 2, 3];
        push_recent(&mut v, 3, 3);
        assert_eq!(v, [3, 1, 2]);
        push_recent(&mut v, 9, 3);
        assert_eq!(v, [9, 3, 1]);
    }

    #[test]
    fn bad_recent_target_entry_is_dropped_rest_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gui-state.toml"),
            "recent_targets = [\"ftp://x\", \"local\", 5]\n[recent_dirs]\nlocal = [\"/a\"]\n",
        )
        .unwrap();
        let s = GuiState::load(dir.path());
        assert_eq!(s.recent_targets, [Target::Local]);
        assert_eq!(s.recent_dirs["local"], ["/a"]);
    }
}
