//! The command palette (⌘P): every window action, every agent and the
//! recent folders, as [`PickerItem`]s. Picking an action performs it,
//! an agent is shown in the focused pane, a folder gets a new agent
//! running the default command.

use std::path::Path;

use fleet_proto::AgentState;

use super::picker::PickerItem;
use super::session::AgentRow;
use super::sidebar::{program_name, tilde};

/// A window action the palette offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    NewAgent,
    NewTerminal,
    SplitRight,
    SplitDown,
    ClosePane,
    ToggleMaximize,
    RestartPane,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    FocusPane1,
    FocusPane2,
    FocusPane3,
    FocusPane4,
    RemoveExited,
    ToggleGraph,
    SwitchFleetd,
    InstallSkill,
    Quit,
}

impl Command {
    /// Every command, in the palette's order.
    pub const ALL: [Command; 20] = [
        Command::NewAgent,
        Command::NewTerminal,
        Command::SplitRight,
        Command::SplitDown,
        Command::ClosePane,
        Command::ToggleMaximize,
        Command::RestartPane,
        Command::FocusLeft,
        Command::FocusRight,
        Command::FocusUp,
        Command::FocusDown,
        Command::FocusPane1,
        Command::FocusPane2,
        Command::FocusPane3,
        Command::FocusPane4,
        Command::RemoveExited,
        Command::ToggleGraph,
        Command::SwitchFleetd,
        Command::InstallSkill,
        Command::Quit,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Command::NewAgent => "New Agent…",
            Command::NewTerminal => "New Terminal",
            Command::SplitRight => "Split Right",
            Command::SplitDown => "Split Down",
            Command::ClosePane => "Close Pane",
            Command::ToggleMaximize => "Maximize or Restore Pane",
            Command::RestartPane => "Restart Agent in Pane",
            Command::FocusLeft => "Focus Pane on the Left",
            Command::FocusRight => "Focus Pane on the Right",
            Command::FocusUp => "Focus Pane Above",
            Command::FocusDown => "Focus Pane Below",
            Command::FocusPane1 => "Focus Pane 1",
            Command::FocusPane2 => "Focus Pane 2",
            Command::FocusPane3 => "Focus Pane 3",
            Command::FocusPane4 => "Focus Pane 4",
            Command::RemoveExited => "Remove Exited Agents…",
            Command::ToggleGraph => "Show or Hide the Agent Graph",
            Command::SwitchFleetd => "Switch fleetd…",
            Command::InstallSkill => "Install Agent Skill",
            Command::Quit => "Quit fleet",
        }
    }

    /// Other words it is found by.
    fn keywords(self) -> &'static str {
        match self {
            Command::NewAgent => "spawn",
            Command::NewTerminal => "shell",
            Command::SplitRight | Command::SplitDown => "pane",
            Command::ClosePane => "detach",
            Command::ToggleMaximize => "zoom",
            Command::RemoveExited => "clear forget rm lost",
            Command::SwitchFleetd => "connect target",
            Command::ToggleGraph => "tree spawned children satellite",
            _ => "",
        }
    }
}

/// What a palette row stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Command(Command),
    Agent(u64),
    /// Spawn the default command here.
    Folder(String),
}

impl Entry {
    /// The row's [`PickerItem::key`].
    pub fn key(&self) -> String {
        match self {
            Entry::Command(c) => format!("command:{c:?}"),
            Entry::Agent(id) => format!("agent:{id}"),
            Entry::Folder(dir) => format!("folder:{dir}"),
        }
    }
}

/// An agent's state as the palette shows it.
pub fn status_text(row: &AgentRow) -> String {
    match row.info.state() {
        AgentState::Running => "running".to_owned(),
        AgentState::Exited => match row.info.exit_code {
            Some(code) => format!("exited {code}"),
            None => "exited".to_owned(),
        },
        AgentState::Lost | AgentState::Unspecified => "lost".to_owned(),
    }
}

/// An agent's row: its `name` label (else program and folder), then the
/// folder and status; its labels are matched too.
pub fn agent_item(row: &AgentRow, home: &Path) -> PickerItem {
    let info = &row.info;
    let cwd = tilde(&info.cwd, home);
    let title = match info.labels.get("name") {
        Some(name) if !name.is_empty() => name.clone(),
        _ => format!("{} in {cwd}", program_name(&info.program)),
    };
    let mut labels: Vec<String> = info
        .labels
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    labels.sort();
    PickerItem {
        key: Entry::Agent(info.id).key(),
        title,
        detail: format!("{cwd} · {}", status_text(row)),
        keywords: format!("agent {} {}", program_name(&info.program), labels.join(" ")),
        shortcut: None,
        preview: String::new(),
    }
}

/// The palette's rows: the commands (each with the shortcut `shortcut`
/// gives it), the agents, newest first, then the recent folders.
pub fn palette_items(
    shortcut: impl Fn(Command) -> Option<String>,
    agents: &[AgentRow],
    folders: &[String],
    home: &Path,
) -> Vec<(Entry, PickerItem)> {
    let mut out = Vec::new();
    for c in Command::ALL {
        let entry = Entry::Command(c);
        out.push((
            entry.clone(),
            PickerItem {
                key: entry.key(),
                title: c.title().to_owned(),
                detail: String::new(),
                keywords: c.keywords().to_owned(),
                shortcut: shortcut(c),
                preview: String::new(),
            },
        ));
    }
    let mut rows: Vec<&AgentRow> = agents.iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.info.created_unix_ms));
    for r in rows {
        out.push((Entry::Agent(r.info.id), agent_item(r, home)));
    }
    for dir in folders.iter().filter(|d| !d.is_empty()) {
        let entry = Entry::Folder(dir.clone());
        out.push((
            entry.clone(),
            PickerItem {
                key: entry.key(),
                title: tilde(dir, home),
                detail: "new agent here".to_owned(),
                keywords: "folder directory recent".to_owned(),
                shortcut: None,
                preview: String::new(),
            },
        ));
    }
    out
}

/// Modifier keys of a shortcut.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub cmd: bool,
}

/// A shortcut as a Mac menu shows it: `⌃⌥⇧⌘` then the key (`⇧⌘D`,
/// `⌥⌘←`, `⇧⌘↩`). `key` is a key name as the keymap writes it.
pub fn shortcut_text(mods: Mods, key: &str) -> String {
    let mut s = String::new();
    for (on, sym) in [
        (mods.ctrl, '⌃'),
        (mods.alt, '⌥'),
        (mods.shift, '⇧'),
        (mods.cmd, '⌘'),
    ] {
        if on {
            s.push(sym);
        }
    }
    match key {
        "enter" => s.push('↩'),
        "left" => s.push('←'),
        "right" => s.push('→'),
        "up" => s.push('↑'),
        "down" => s.push('↓'),
        "tab" => s.push('⇥'),
        "escape" => s.push('⎋'),
        "backspace" => s.push('⌫'),
        "delete" => s.push('⌦'),
        "space" => s.push_str("Space"),
        k => s.push_str(&k.to_uppercase()),
    }
    s
}

/// The question Remove Exited Agents asks, naming how many go; `None`
/// when none has ended (nothing is asked or removed).
pub fn remove_exited_prompt(ended: usize) -> Option<String> {
    match ended {
        0 => None,
        1 => Some("Remove 1 exited agent?".to_owned()),
        n => Some(format!("Remove {n} exited agents?")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_exited_prompt_names_the_count() {
        assert_eq!(remove_exited_prompt(0), None);
        assert_eq!(
            remove_exited_prompt(1).as_deref(),
            Some("Remove 1 exited agent?")
        );
        assert_eq!(
            remove_exited_prompt(3).as_deref(),
            Some("Remove 3 exited agents?")
        );
    }
    use crate::core::picker::{Picker, Recents};
    use fleet_proto::AgentInfo;

    fn row(id: u64, program: &str, cwd: &str, created: i64, state: AgentState) -> AgentRow {
        let mut info = AgentInfo {
            id,
            program: program.into(),
            cwd: cwd.into(),
            created_unix_ms: created,
            ..Default::default()
        };
        info.set_state(state);
        AgentRow {
            info,
            last_line: String::new(),
            last_output_unix_ms: None,
        }
    }

    fn home() -> &'static Path {
        Path::new("/Users/me")
    }

    #[test]
    fn shortcuts_read_like_a_mac_menu() {
        let cmd = Mods {
            cmd: true,
            ..Mods::default()
        };
        let all = Mods {
            ctrl: true,
            alt: true,
            shift: true,
            cmd: true,
        };
        assert_eq!(shortcut_text(cmd, "d"), "⌘D");
        assert_eq!(shortcut_text(Mods { shift: true, ..cmd }, "enter"), "⇧⌘↩");
        assert_eq!(shortcut_text(Mods { alt: true, ..cmd }, "left"), "⌥⌘←");
        assert_eq!(shortcut_text(all, "1"), "⌃⌥⇧⌘1");
        assert_eq!(shortcut_text(Mods::default(), "escape"), "⎋");
    }

    #[test]
    fn every_command_is_listed_with_its_shortcut() {
        let items = palette_items(
            |c| (c == Command::SplitRight).then(|| "⌘D".to_owned()),
            &[],
            &[],
            home(),
        );
        assert_eq!(items.len(), Command::ALL.len());
        let (entry, split) = &items[2];
        assert_eq!(*entry, Entry::Command(Command::SplitRight));
        assert_eq!(split.title, "Split Right");
        assert_eq!(split.shortcut.as_deref(), Some("⌘D"));
        assert_eq!(items[0].1.shortcut, None);
        // Keys are distinct.
        let mut keys: Vec<_> = items.iter().map(|(_, i)| i.key.clone()).collect();
        keys.dedup();
        assert_eq!(keys.len(), Command::ALL.len());
    }

    #[test]
    fn agents_newest_first_then_folders() {
        let agents = [
            row(1, "/bin/sh", "/Users/me/a", 1, AgentState::Running),
            row(2, "claude", "/Users/me/b", 2, AgentState::Exited),
        ];
        let items = palette_items(
            |_| None,
            &agents,
            &["/Users/me/a".into(), String::new()],
            home(),
        );
        let rest: Vec<_> = items[Command::ALL.len()..]
            .iter()
            .map(|(e, _)| e.clone())
            .collect();
        assert_eq!(
            rest,
            [
                Entry::Agent(2),
                Entry::Agent(1),
                Entry::Folder("/Users/me/a".into())
            ]
        );
        let folder = &items.last().unwrap().1;
        assert_eq!(folder.title, "~/a");
        assert_eq!(folder.key, "folder:/Users/me/a");
    }

    #[test]
    fn an_agent_is_found_by_name_folder_label_and_status() {
        let mut a = row(
            7,
            "/usr/bin/claude",
            "/Users/me/src/fleetd",
            1,
            AgentState::Running,
        );
        a.info.labels.insert("project".into(), "fleetd".into());
        let mut b = row(8, "zsh", "/tmp", 2, AgentState::Exited);
        b.info.exit_code = Some(2);
        b.info.labels.insert("name".into(), "sigterm-otter".into());

        let item = agent_item(&a, home());
        assert_eq!(item.title, "claude in ~/src/fleetd");
        assert_eq!(item.detail, "~/src/fleetd · running");
        assert_eq!(item.key, "agent:7");
        let named = agent_item(&b, home());
        assert_eq!(named.title, "sigterm-otter");
        assert_eq!(named.detail, "/tmp · exited 2");

        let items: Vec<PickerItem> = palette_items(|_| None, &[a, b], &[], home())
            .into_iter()
            .map(|(_, i)| i)
            .collect();
        let first = |q: &str| {
            let mut p = Picker::new(items.clone(), Recents::default());
            p.set_query(q);
            p.selected_item().map(|i| i.key.clone())
        };
        assert_eq!(first("otter").as_deref(), Some("agent:8"));
        assert_eq!(first("src/fleetd").as_deref(), Some("agent:7"));
        assert_eq!(first("project=fleetd").as_deref(), Some("agent:7"));
        assert_eq!(first("exited 2").as_deref(), Some("agent:8"));
    }

    #[test]
    fn ended_states_read_plainly() {
        let mut r = row(1, "sh", "/", 1, AgentState::Exited);
        assert_eq!(status_text(&r), "exited");
        r.info.set_state(AgentState::Lost);
        assert_eq!(status_text(&r), "lost");
    }
}
