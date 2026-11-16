//! The command palette (⌘P): every window action, every agent and the
//! recent folders, as [`PickerItem`]s. Picking an action performs it,
//! an agent is shown in the focused pane, a folder gets a new agent
//! running the default command.
//!
//! Every action the keymap binds has a [`Command`] (ADR 0080; a test in
//! `app::palette` fails when one has none). A command that needs
//! something the window lacks, such as an agent in the focused pane, is
//! left out ([`Command::shown`]).

use std::path::Path;

use fleet_proto::AgentState;

use super::git;
use super::layout::MAX_PANES;
use super::picker::PickerItem;
use super::presets::Preset;
use super::session::AgentRow;
use super::sidebar::{program_name, tilde};
use super::tabs::MAX_TABS;

/// A window action the palette offers. Named after the action it
/// dispatches (`app::palette::command_action`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    NewAgent,
    NewTerminal,
    SplitRight,
    SplitDown,
    ClosePane,
    ReopenClosedPane,
    ToggleMaximize,
    RestartPane,
    KillPane,
    RenamePane,
    EditLabelsPane,
    DuplicatePane,
    RemovePane,
    Find,
    FindNext,
    FindPrevious,
    Copy,
    Paste,
    CopyLastOutput,
    SaveOutput,
    OpenOutput,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    FocusPane1,
    FocusPane2,
    FocusPane3,
    FocusPane4,
    NextAgent,
    PreviousAgent,
    FocusFilter,
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    /// Show workspace tab `n` (1-based).
    SelectTab(usize),
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ScrollPageUp,
    ScrollPageDown,
    ScrollToTop,
    ScrollToBottom,
    SelectAll,
    ClearScrollback,
    RemoveExited,
    ToggleGraph,
    /// Send to Agents… (ADR 0083).
    SendToAgents,
    ShowShortcuts,
    OpenSettings,
    SwitchFleetd,
    InstallSkill,
    Quit,
}

/// What decides which commands the palette offers now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaletteState {
    /// Panes in the shown tab.
    pub panes: usize,
    /// Workspace tabs.
    pub tabs: usize,
    /// The focused pane shows an agent (Rename, Find).
    pub has_agent: bool,
    /// The focused pane shows a running agent (Kill, Paste).
    pub running: bool,
    /// The focused pane's agent exited or is gone (Restart, Remove).
    pub restartable: bool,
    /// Text is selected in the focused pane (Copy).
    pub has_selection: bool,
    /// Some agent is running (Send to Agents…).
    pub any_running: bool,
}

impl Command {
    /// Every command, in the palette's order.
    pub const ALL: &[Command] = &[
        Command::NewAgent,
        Command::NewTerminal,
        Command::SplitRight,
        Command::SplitDown,
        Command::ClosePane,
        Command::ReopenClosedPane,
        Command::ToggleMaximize,
        Command::RestartPane,
        Command::KillPane,
        Command::RenamePane,
        Command::EditLabelsPane,
        Command::DuplicatePane,
        Command::RemovePane,
        Command::Find,
        Command::FindNext,
        Command::FindPrevious,
        Command::Copy,
        Command::Paste,
        Command::CopyLastOutput,
        Command::SaveOutput,
        Command::OpenOutput,
        Command::FocusLeft,
        Command::FocusRight,
        Command::FocusUp,
        Command::FocusDown,
        Command::FocusPane1,
        Command::FocusPane2,
        Command::FocusPane3,
        Command::FocusPane4,
        Command::NextAgent,
        Command::PreviousAgent,
        Command::FocusFilter,
        Command::NewTab,
        Command::CloseTab,
        Command::NextTab,
        Command::PrevTab,
        Command::SelectTab(1),
        Command::SelectTab(2),
        Command::SelectTab(3),
        Command::SelectTab(4),
        Command::SelectTab(5),
        Command::SelectTab(6),
        Command::SelectTab(7),
        Command::SelectTab(8),
        Command::SelectTab(9),
        Command::ZoomIn,
        Command::ZoomOut,
        Command::ZoomReset,
        Command::ScrollPageUp,
        Command::ScrollPageDown,
        Command::ScrollToTop,
        Command::ScrollToBottom,
        Command::SelectAll,
        Command::ClearScrollback,
        Command::RemoveExited,
        Command::ToggleGraph,
        Command::SendToAgents,
        Command::ShowShortcuts,
        Command::OpenSettings,
        Command::SwitchFleetd,
        Command::InstallSkill,
        Command::Quit,
    ];

    pub fn title(self) -> String {
        let title = match self {
            Command::NewAgent => "New Agent…",
            Command::NewTerminal => "New Terminal",
            Command::SplitRight => "Split Right",
            Command::SplitDown => "Split Down",
            Command::ClosePane => "Close Pane",
            Command::ReopenClosedPane => "Reopen Closed Pane",
            Command::ToggleMaximize => "Maximize or Restore Pane",
            Command::RestartPane => "Restart Agent in Pane",
            // It asks first (unless told not to).
            Command::KillPane => "Kill Agent…",
            Command::RenamePane => "Rename Agent…",
            Command::EditLabelsPane => "Edit Labels…",
            Command::DuplicatePane => "Duplicate Agent…",
            Command::RemovePane => "Remove Exited Agent in Pane",
            Command::Find => "Find in Scrollback…",
            Command::FindNext => "Find Next",
            Command::FindPrevious => "Find Previous",
            Command::Copy => "Copy",
            Command::Paste => "Paste",
            Command::CopyLastOutput => "Copy Last Output",
            Command::SaveOutput => "Save Output…",
            Command::OpenOutput => "Open Output in Editor",
            Command::FocusLeft => "Focus Pane on the Left",
            Command::FocusRight => "Focus Pane on the Right",
            Command::FocusUp => "Focus Pane Above",
            Command::FocusDown => "Focus Pane Below",
            Command::FocusPane1 => "Focus Pane 1",
            Command::FocusPane2 => "Focus Pane 2",
            Command::FocusPane3 => "Focus Pane 3",
            Command::FocusPane4 => "Focus Pane 4",
            Command::NextAgent => "Switch to Next Agent…",
            Command::PreviousAgent => "Switch to Previous Agent…",
            Command::FocusFilter => "Filter Agents",
            Command::NewTab => "New Tab",
            Command::CloseTab => "Close Tab",
            Command::NextTab => "Show Next Tab",
            Command::PrevTab => "Show Previous Tab",
            Command::SelectTab(n) => return format!("Go to Tab {n}"),
            Command::ZoomIn => "Zoom In",
            Command::ZoomOut => "Zoom Out",
            Command::ZoomReset => "Reset Zoom",
            Command::ScrollPageUp => "Scroll Up a Page",
            Command::ScrollPageDown => "Scroll Down a Page",
            Command::ScrollToTop => "Scroll to Top",
            Command::ScrollToBottom => "Scroll to Bottom",
            Command::SelectAll => "Select All in Pane",
            Command::ClearScrollback => "Clear Scrollback",
            Command::RemoveExited => "Remove Exited Agents…",
            Command::ToggleGraph => "Show or Hide the Agent Graph",
            Command::SendToAgents => "Send to Agents…",
            Command::ShowShortcuts => "Keyboard Shortcuts",
            Command::OpenSettings => "Settings…",
            Command::SwitchFleetd => "Switch fleetd…",
            Command::InstallSkill => "Install Agent Skill",
            Command::Quit => "Quit fleet",
        };
        title.to_owned()
    }

    /// Other words it is found by.
    fn keywords(self) -> &'static str {
        match self {
            Command::NewAgent => "spawn",
            Command::NewTerminal => "shell",
            Command::SplitRight | Command::SplitDown => "pane",
            Command::ClosePane => "detach",
            Command::ReopenClosedPane => "undo restore",
            Command::ToggleMaximize => "zoom",
            Command::KillPane => "stop terminate",
            Command::RenamePane => "name label",
            Command::EditLabelsPane => "label role project tag",
            Command::DuplicatePane => "copy clone again spawn",
            Command::RemovePane => "forget rm ended lost",
            Command::Find | Command::FindNext | Command::FindPrevious => "search scrollback",
            Command::Copy | Command::Paste => "clipboard",
            Command::NextAgent | Command::PreviousAgent => "switcher recent",
            Command::FocusFilter => "sidebar search",
            Command::NewTab
            | Command::CloseTab
            | Command::NextTab
            | Command::PrevTab
            | Command::SelectTab(_) => "workspace",
            Command::ZoomIn => "bigger font larger",
            Command::ZoomOut => "smaller font",
            Command::ZoomReset => "actual size font",
            Command::CopyLastOutput => "screen clipboard",
            Command::SaveOutput => "history transcript export file",
            Command::OpenOutput => "history transcript text edit",
            Command::ScrollPageUp
            | Command::ScrollPageDown
            | Command::ScrollToTop
            | Command::ScrollToBottom => "scrollback history",
            Command::SelectAll => "copy text",
            Command::ClearScrollback => "erase history buffer",
            Command::RemoveExited => "clear forget rm lost",
            Command::SendToAgents => "broadcast message type input fleet send",
            Command::ShowShortcuts => "cheat sheet keys help",
            Command::OpenSettings => "preferences config",
            Command::SwitchFleetd => "connect target",
            Command::ToggleGraph => "tree spawned children satellite",
            _ => "",
        }
    }

    /// Whether the palette offers it in `s`: what acts on the focused
    /// pane's agent needs one (in the right state), pane and tab moves
    /// need somewhere to go, and adding stops at the limit.
    pub fn shown(self, s: &PaletteState) -> bool {
        match self {
            Command::SplitRight | Command::SplitDown => s.panes < MAX_PANES,
            Command::ToggleMaximize
            | Command::FocusLeft
            | Command::FocusRight
            | Command::FocusUp
            | Command::FocusDown => s.panes > 1,
            Command::FocusPane1 | Command::FocusPane2 => s.panes > 1,
            Command::FocusPane3 => s.panes >= 3,
            Command::FocusPane4 => s.panes >= 4,
            Command::RenamePane
            | Command::EditLabelsPane
            | Command::DuplicatePane
            | Command::CopyLastOutput
            | Command::SaveOutput
            | Command::OpenOutput
            | Command::Find
            | Command::FindNext
            | Command::FindPrevious
            | Command::ScrollPageUp
            | Command::ScrollPageDown
            | Command::ScrollToTop
            | Command::ScrollToBottom
            | Command::SelectAll
            | Command::ClearScrollback => s.has_agent,
            Command::KillPane | Command::Paste => s.running,
            Command::RestartPane | Command::RemovePane => s.restartable,
            Command::Copy => s.has_selection,
            Command::SendToAgents => s.any_running,
            Command::NewTab => s.tabs < MAX_TABS,
            Command::CloseTab | Command::NextTab | Command::PrevTab => s.tabs > 1,
            Command::SelectTab(n) => s.tabs > 1 && n <= s.tabs,
            _ => true,
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
    /// "New <preset>…": the New agent sheet with the preset at this index
    /// (ADR 0084).
    Preset(usize),
}

impl Entry {
    /// The row's [`PickerItem::key`].
    pub fn key(&self) -> String {
        match self {
            Entry::Command(c) => format!("command:{c:?}"),
            Entry::Agent(id) => format!("agent:{id}"),
            Entry::Folder(dir) => format!("folder:{dir}"),
            Entry::Preset(i) => format!("preset:{i}"),
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
/// folder, git checkout (ADR 0085) and status; its labels, repository,
/// worktree and branch are matched too.
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
        detail: match git::row_subtitle(info) {
            Some(g) => format!("{cwd} · {g} · {}", status_text(row)),
            None => format!("{cwd} · {}", status_text(row)),
        },
        keywords: format!(
            "agent {} {} {}",
            program_name(&info.program),
            labels.join(" "),
            git::search_terms(info).join(" ")
        ),
        shortcut: None,
        preview: String::new(),
    }
}

/// The palette's rows: the commands shown in `state` (each with the
/// shortcut `shortcut` gives it), "New <preset>…" for each preset, the
/// agents, newest first, then the recent folders.
pub fn palette_items(
    shortcut: impl Fn(Command) -> Option<String>,
    state: &PaletteState,
    presets: &[Preset],
    agents: &[AgentRow],
    folders: &[String],
    home: &Path,
) -> Vec<(Entry, PickerItem)> {
    let mut out = Vec::new();
    for &c in Command::ALL.iter().filter(|c| c.shown(state)) {
        let entry = Entry::Command(c);
        out.push((
            entry.clone(),
            PickerItem {
                key: entry.key(),
                title: c.title(),
                detail: String::new(),
                keywords: c.keywords().to_owned(),
                shortcut: shortcut(c),
                preview: String::new(),
            },
        ));
    }
    for (i, p) in presets.iter().enumerate() {
        let entry = Entry::Preset(i);
        out.push((
            entry.clone(),
            PickerItem {
                key: entry.key(),
                title: format!("New {}…", p.name),
                detail: p.command_line(),
                keywords: "preset new agent spawn".to_owned(),
                shortcut: None,
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
        "home" => s.push('↖'),
        "end" => s.push('↘'),
        "pageup" => s.push('⇞'),
        "pagedown" => s.push('⇟'),
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
        assert_eq!(
            shortcut_text(
                Mods {
                    shift: true,
                    ..Mods::default()
                },
                "pageup"
            ),
            "⇧⇞"
        );
    }

    /// Everything there is to act on: every command is shown.
    fn everything() -> PaletteState {
        PaletteState {
            panes: 3,
            tabs: 3,
            has_agent: true,
            running: true,
            restartable: true,
            has_selection: true,
            any_running: true,
        }
    }

    fn shown(s: &PaletteState) -> Vec<Command> {
        palette_items(|_| None, s, &[], &[], &[], home())
            .into_iter()
            .map(|(e, _)| match e {
                Entry::Command(c) => c,
                other => panic!("{other:?} is not a command"),
            })
            .collect()
    }

    #[test]
    fn every_command_is_listed_with_its_shortcut() {
        let mut state = everything();
        state.tabs = MAX_TABS;
        let items = palette_items(
            |c| (c == Command::SplitRight).then(|| "⌘D".to_owned()),
            &state,
            &[],
            &[],
            &[],
            home(),
        );
        // At the tab limit with three panes: all but New Tab and Focus
        // Pane 4.
        let listed: Vec<_> = items.iter().map(|(e, _)| e.clone()).collect();
        let expected: Vec<_> = Command::ALL
            .iter()
            .filter(|&&c| c != Command::NewTab && c != Command::FocusPane4)
            .map(|&c| Entry::Command(c))
            .collect();
        assert_eq!(listed, expected);
        let (entry, split) = &items[2];
        assert_eq!(*entry, Entry::Command(Command::SplitRight));
        assert_eq!(split.title, "Split Right");
        assert_eq!(split.shortcut.as_deref(), Some("⌘D"));
        assert_eq!(items[0].1.shortcut, None);
        // Keys and titles are distinct.
        let mut keys: Vec<_> = Command::ALL
            .iter()
            .map(|&c| Entry::Command(c).key())
            .collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), Command::ALL.len());
        let mut titles: Vec<_> = Command::ALL.iter().map(|c| c.title()).collect();
        titles.sort();
        titles.dedup();
        assert_eq!(titles.len(), Command::ALL.len());
        let tab = Command::SelectTab(3);
        assert_eq!(tab.title(), "Go to Tab 3");
        assert_eq!(Entry::Command(tab).key(), "command:SelectTab(3)");
    }

    #[test]
    fn what_acts_on_the_focused_agent_needs_one() {
        let agent_only = [
            Command::RenamePane,
            Command::EditLabelsPane,
            Command::DuplicatePane,
            Command::Find,
            Command::FindNext,
            Command::FindPrevious,
            Command::KillPane,
            Command::Paste,
            Command::RestartPane,
            Command::RemovePane,
            Command::Copy,
            Command::CopyLastOutput,
            Command::SaveOutput,
            Command::OpenOutput,
        ];
        // An empty pane: none of them.
        let empty = PaletteState {
            has_agent: false,
            running: false,
            restartable: false,
            has_selection: false,
            ..everything()
        };
        let listed = shown(&empty);
        for c in agent_only {
            assert!(!listed.contains(&c), "{c:?} shown for an empty pane");
        }
        // The rest stays.
        assert!(listed.contains(&Command::NewAgent));
        assert!(listed.contains(&Command::ZoomIn));
        assert!(listed.contains(&Command::RemoveExited));

        // A running agent: Kill, not Restart or Remove.
        let running = PaletteState {
            restartable: false,
            has_selection: false,
            ..everything()
        };
        let listed = shown(&running);
        for c in [
            Command::KillPane,
            Command::Paste,
            Command::RenamePane,
            Command::EditLabelsPane,
            Command::Find,
        ] {
            assert!(listed.contains(&c), "{c:?}");
        }
        for c in [Command::RestartPane, Command::RemovePane, Command::Copy] {
            assert!(!listed.contains(&c), "{c:?}");
        }

        // An ended one: Restart and Remove, not Kill.
        let ended = PaletteState {
            running: false,
            ..everything()
        };
        let listed = shown(&ended);
        assert!(listed.contains(&Command::RestartPane));
        assert!(listed.contains(&Command::RemovePane));
        assert!(!listed.contains(&Command::KillPane));
    }

    #[test]
    fn send_to_agents_needs_a_running_agent() {
        assert!(shown(&everything()).contains(&Command::SendToAgents));
        let none = PaletteState {
            any_running: false,
            ..everything()
        };
        assert!(!shown(&none).contains(&Command::SendToAgents));
        assert_eq!(Command::SendToAgents.title(), "Send to Agents…");
    }

    #[test]
    fn pane_and_tab_moves_need_somewhere_to_go() {
        let one = PaletteState {
            panes: 1,
            tabs: 1,
            ..everything()
        };
        let listed = shown(&one);
        for c in [
            Command::FocusLeft,
            Command::FocusPane1,
            Command::ToggleMaximize,
            Command::CloseTab,
            Command::NextTab,
            Command::PrevTab,
            Command::SelectTab(1),
        ] {
            assert!(!listed.contains(&c), "{c:?} shown with one pane and tab");
        }
        assert!(listed.contains(&Command::SplitRight));
        assert!(listed.contains(&Command::NewTab));

        let listed = shown(&everything());
        assert!(listed.contains(&Command::FocusPane3));
        assert!(!listed.contains(&Command::FocusPane4));
        assert!(listed.contains(&Command::SelectTab(3)));
        assert!(!listed.contains(&Command::SelectTab(4)));

        let full = PaletteState {
            panes: MAX_PANES,
            tabs: MAX_TABS,
            ..everything()
        };
        let listed = shown(&full);
        assert!(!listed.contains(&Command::SplitRight));
        assert!(!listed.contains(&Command::SplitDown));
        assert!(!listed.contains(&Command::NewTab));
        assert!(listed.contains(&Command::FocusPane4));
        assert!(listed.contains(&Command::SelectTab(MAX_TABS)));
    }

    #[test]
    fn terminal_commands_are_found_by_what_they_do() {
        let items: Vec<PickerItem> = palette_items(|_| None, &everything(), &[], &[], &[], home())
            .into_iter()
            .map(|(_, i)| i)
            .collect();
        let first = |q: &str| {
            let mut p = Picker::new(items.clone(), Recents::default());
            p.set_query(q);
            p.selected_item().map(|i| i.title.clone())
        };
        // They act on the focused pane's agent: none, none shown.
        let none = shown(&PaletteState::default());
        assert!(!none.contains(&Command::ScrollToTop));
        assert!(!none.contains(&Command::SelectAll));
        assert!(!none.contains(&Command::ClearScrollback));
        assert_eq!(
            first("clear scrollback").as_deref(),
            Some("Clear Scrollback")
        );
        assert_eq!(first("select all").as_deref(), Some("Select All in Pane"));
        assert_eq!(first("scroll to top").as_deref(), Some("Scroll to Top"));
    }

    #[test]
    fn agents_newest_first_then_folders() {
        let agents = [
            row(1, "/bin/sh", "/Users/me/a", 1, AgentState::Running),
            row(2, "claude", "/Users/me/b", 2, AgentState::Exited),
        ];
        let items = palette_items(
            |_| None,
            &everything(),
            &[],
            &agents,
            &["/Users/me/a".into(), String::new()],
            home(),
        );
        let rest: Vec<_> = items[shown(&everything()).len()..]
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

        let items: Vec<PickerItem> =
            palette_items(|_| None, &everything(), &[], &[a, b], &[], home())
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

    /// ADR 0085: the git checkout is shown and matched.
    #[test]
    fn an_agent_is_found_by_its_branch_and_worktree() {
        let mut a = row(7, "claude", "/Users/me/wt/issue-97", 1, AgentState::Running);
        a.info.git = Some(fleet_proto::GitInfo {
            repo: "fleetd".into(),
            worktree: "issue-97".into(),
            branch: "fix/freeze-timeout".into(),
            detached: false,
        });
        let b = row(8, "claude", "/Users/me/other", 2, AgentState::Running);
        let item = agent_item(&a, home());
        assert_eq!(
            item.detail,
            "~/wt/issue-97 · issue-97 · fix/freeze-timeout · running"
        );
        let items: Vec<PickerItem> =
            palette_items(|_| None, &everything(), &[], &[a, b], &[], home())
                .into_iter()
                .map(|(_, i)| i)
                .collect();
        let first = |q: &str| {
            let mut p = Picker::new(items.clone(), Recents::default());
            p.set_query(q);
            p.selected_item().map(|i| i.key.clone())
        };
        assert_eq!(first("frzetimeout").as_deref(), Some("agent:7"));
        assert_eq!(first("issue-97").as_deref(), Some("agent:7"));
    }

    #[test]
    fn presets_are_new_commands_before_the_agents() {
        let presets = [Preset {
            name: "Claude (continue)".into(),
            command: vec!["claude".into(), "--continue".into()],
            labels: Default::default(),
        }];
        let agents = [row(1, "sh", "/Users/me/a", 1, AgentState::Running)];
        let items = palette_items(|_| None, &everything(), &presets, &agents, &[], home());
        let n = shown(&everything()).len();
        assert_eq!(items[n].0, Entry::Preset(0));
        assert_eq!(items[n].1.title, "New Claude (continue)…");
        assert_eq!(items[n].1.detail, "claude --continue");
        assert_eq!(items[n].1.key, "preset:0");
        assert_eq!(items[n + 1].0, Entry::Agent(1));
        let mut p = Picker::new(
            items.into_iter().map(|(_, i)| i).collect(),
            Recents::default(),
        );
        p.set_query("continue");
        assert_eq!(p.selected_item().map(|i| i.key.as_str()), Some("preset:0"));
    }

    #[test]
    fn ended_states_read_plainly() {
        let mut r = row(1, "sh", "/", 1, AgentState::Exited);
        assert_eq!(status_text(&r), "exited");
        r.info.set_state(AgentState::Lost);
        assert_eq!(status_text(&r), "lost");
    }
}
