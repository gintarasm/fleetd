use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use super::layout::{Layout, SavedLayout};
use super::sidebar::GroupBy;
use super::tabs::{SavedTabs, Tabs};
use super::target::Target;

pub const MAX_RECENT_TARGETS: usize = 10;
pub const MAX_RECENT_DIRS: usize = 20;
pub const MAX_RECENT_COMMANDS: usize = 20;

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
    /// Keyed by target string: the New agent sheet's commands, as typed
    /// (without the first prompt), most recent first (ADR 0084).
    pub recent_commands: BTreeMap<String, Vec<String>>,
    /// Keyed by target string. Entries that do not parse (old or unknown shape) are dropped.
    #[serde(deserialize_with = "lenient_layouts")]
    pub layouts: BTreeMap<String, SavedLayout>,
    /// Keyed by target string: the workspace tabs (ADR 0061). Where
    /// present it wins over `layouts`, which keeps the shown tab's layout
    /// for older builds. Entries that do not parse are dropped.
    #[serde(deserialize_with = "lenient_tabs")]
    pub tabs: BTreeMap<String, SavedTabs>,
    /// Keyed by target string: the titles of the sidebar groups shown
    /// collapsed (issue #40).
    pub collapsed_groups: BTreeMap<String, BTreeSet<String>>,
    /// What the sidebar groups by (ADR 0085), for every target. A value
    /// this build does not know reads as the default.
    #[serde(deserialize_with = "lenient_group_by")]
    pub sidebar_group_by: GroupBy,
    /// Target strings whose New agent sheet last spawned into a new
    /// worktree: its "New worktree" checkbox starts checked (ADR 0094).
    pub new_worktree: BTreeSet<String>,
}

fn lenient_group_by<'de, D: Deserializer<'de>>(d: D) -> Result<GroupBy, D::Error> {
    let raw = toml::Value::deserialize(d)?;
    Ok(raw.try_into().unwrap_or_default())
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

fn lenient_tabs<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, SavedTabs>, D::Error> {
    let raw = BTreeMap::<String, toml::Value>::deserialize(d)?;
    Ok(raw
        .into_iter()
        .filter_map(|(k, v)| v.try_into().ok().map(|t| (k, t)))
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

    /// `target`'s saved tabs; else one tab with its layout saved before
    /// tabs; else one empty pane.
    pub fn saved_tabs(&self, target: &Target) -> Tabs {
        let key = target.to_string();
        match (self.tabs.get(&key), self.layouts.get(&key)) {
            (Some(t), _) => Tabs::from_saved(t),
            (None, Some(l)) => Tabs::from_layout(Layout::from_saved(l)),
            (None, None) => Tabs::single(),
        }
    }

    /// Records `tabs` for `target`, and the shown tab's layout as the
    /// target's single layout (what older builds read).
    pub fn set_tabs(&mut self, target: &Target, tabs: &Tabs) {
        let key = target.to_string();
        self.layouts.insert(key.clone(), tabs.layout().to_saved());
        self.tabs.insert(key, tabs.to_saved());
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
    use crate::core::layout::Layout;
    use crate::core::session::PaneId;

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
        s.recent_commands
            .insert("local".into(), vec!["claude --continue".into()]);
        s.layouts.insert("local".into(), SavedLayout::default());
        s.collapsed_groups
            .insert("local".into(), ["fleetd".to_owned()].into());
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
    fn state_without_collapsed_groups_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gui-state.toml"),
            "recent_targets = [\"local\"]\n",
        )
        .unwrap();
        let s = GuiState::load(dir.path());
        assert_eq!(s.recent_targets, [Target::Local]);
        assert!(s.collapsed_groups.is_empty());
        assert!(s.recent_commands.is_empty());
    }

    #[test]
    fn the_sidebar_grouping_roundtrips_and_an_unknown_one_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(GuiState::load(dir.path()).sidebar_group_by, GroupBy::Folder);
        let s = GuiState {
            sidebar_group_by: GroupBy::Repo,
            ..GuiState::default()
        };
        s.save(dir.path()).unwrap();
        let text = std::fs::read_to_string(dir.path().join("gui-state.toml")).unwrap();
        assert!(text.contains("sidebar_group_by = \"repo\""), "{text}");
        assert_eq!(GuiState::load(dir.path()), s);
        std::fs::write(
            dir.path().join("gui-state.toml"),
            "sidebar_group_by = \"label\"\nrecent_dirs = { local = [\"/w\"] }\n",
        )
        .unwrap();
        let loaded = GuiState::load(dir.path());
        assert_eq!(loaded.sidebar_group_by, GroupBy::Folder);
        assert_eq!(loaded.recent_dirs["local"], ["/w"]);
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
    fn tabs_roundtrip_and_a_bad_entry_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut tabs = Tabs::single();
        tabs.new_tab();
        let mut s = GuiState::default();
        s.tabs.insert("local".into(), tabs.to_saved());
        s.save(dir.path()).unwrap();
        assert_eq!(GuiState::load(dir.path()), s);

        let text = std::fs::read_to_string(dir.path().join("gui-state.toml")).unwrap();
        std::fs::write(
            dir.path().join("gui-state.toml"),
            format!("{text}\n[tabs.\"me@box\"]\ntabs = 3\n"),
        )
        .unwrap();
        let back = GuiState::load(dir.path());
        assert_eq!(back.tabs.len(), 1);
        assert_eq!(back.tabs["local"], tabs.to_saved());
    }

    #[test]
    fn saved_tabs_win_over_an_older_single_layout() {
        let mut s = GuiState::default();
        let mut old = Layout::single();
        old.set_agent(PaneId(1), Some(4));
        s.layouts.insert("local".into(), old.to_saved());
        // Only the old layout: one tab with it.
        let t = s.saved_tabs(&Target::Local);
        assert_eq!((t.len(), t.layout().agent(PaneId(1))), (1, Some(4)));
        // Tabs saved too: they are used.
        let mut tabs = Tabs::single();
        tabs.new_tab();
        s.tabs.insert("local".into(), tabs.to_saved());
        assert_eq!(s.saved_tabs(&Target::Local), tabs);
        // Recording keeps the shown tab as the single layout.
        tabs.select(0);
        s.set_tabs(&Target::Local, &tabs);
        assert_eq!(s.layouts["local"], tabs.layout().to_saved());
        assert_eq!(s.tabs["local"], tabs.to_saved());
        // Nothing saved: one empty pane.
        let ssh = Target::Ssh { dest: "b".into() };
        assert_eq!(s.saved_tabs(&ssh), Tabs::single());
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
