//! The context menus: a sidebar row's, a sidebar group header's and a
//! pane's. What they offer and what they are called; the shell shows them
//! as native menus.

use super::sidebar::GroupBy;

use super::status::Subject;

/// A menu line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEntry<T> {
    Item(T),
    Separator,
}

/// A command-key shortcut shown beside an item (the window's own key
/// bindings perform it; the menu only displays it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shortcut {
    /// The key: a lowercase character, or `"enter"`.
    pub key: &'static str,
    pub shift: bool,
}

const fn cmd(key: &'static str) -> Option<Shortcut> {
    Some(Shortcut { key, shift: false })
}

const fn cmd_shift(key: &'static str) -> Option<Shortcut> {
    Some(Shortcut { key, shift: true })
}

/// An item of a sidebar row's menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowMenuItem {
    OpenSplit,
    Rename,
    EditLabels,
    SaveOutput,
    OpenOutput,
    Kill,
    Restart,
    Remove,
}

impl RowMenuItem {
    pub fn title(self) -> &'static str {
        match self {
            RowMenuItem::OpenSplit => "Open in New Split",
            RowMenuItem::Rename => "Rename…",
            RowMenuItem::EditLabels => "Edit Labels…",
            RowMenuItem::SaveOutput => "Save Output…",
            RowMenuItem::OpenOutput => "Open Output in Editor",
            // It asks first.
            RowMenuItem::Kill => "Kill…",
            RowMenuItem::Restart => "Restart",
            RowMenuItem::Remove => "Remove",
        }
    }
}

/// A row's menu: Open in New Split (while a pane can be added), Rename…
/// and Edit Labels…, Save Output… and Open Output in Editor (ADR 0086),
/// then Kill for a running agent, or Restart and Remove for one that
/// exited or is lost.
pub fn row_menu(can_split: bool, ended: bool) -> Vec<MenuEntry<RowMenuItem>> {
    let mut out = Vec::new();
    if can_split {
        out.push(MenuEntry::Item(RowMenuItem::OpenSplit));
        out.push(MenuEntry::Separator);
    }
    out.push(MenuEntry::Item(RowMenuItem::Rename));
    out.push(MenuEntry::Item(RowMenuItem::EditLabels));
    out.push(MenuEntry::Separator);
    out.push(MenuEntry::Item(RowMenuItem::SaveOutput));
    out.push(MenuEntry::Item(RowMenuItem::OpenOutput));
    out.push(MenuEntry::Separator);
    if ended {
        out.push(MenuEntry::Item(RowMenuItem::Restart));
        out.push(MenuEntry::Item(RowMenuItem::Remove));
    } else {
        out.push(MenuEntry::Item(RowMenuItem::Kill));
    }
    out
}

/// A sidebar group header's menu (ADR 0085): what the sidebar groups
/// by, the current choice checked.
pub fn group_menu(current: GroupBy) -> Vec<(GroupBy, bool)> {
    GroupBy::ALL
        .into_iter()
        .map(|by| (by, by == current))
        .collect()
}

/// An item of a pane's menu (right-click on its header, or on the grid
/// while the agent does not take the mouse).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneMenuItem {
    Copy,
    Paste,
    SplitRight,
    SplitDown,
    Maximize,
    Restore,
    Rename,
    EditLabels,
    CopyLastOutput,
    SaveOutput,
    OpenOutput,
    Restart,
    Kill,
    Close,
}

impl PaneMenuItem {
    /// The item's title; `subject` names what the pane shows ("Kill
    /// Shell…" for a shell, ADR 0071).
    pub fn title(self, subject: Subject) -> String {
        let what = subject.title();
        match self {
            PaneMenuItem::Copy => "Copy".to_owned(),
            PaneMenuItem::Paste => "Paste".to_owned(),
            PaneMenuItem::SplitRight => "Split Right".to_owned(),
            PaneMenuItem::SplitDown => "Split Down".to_owned(),
            PaneMenuItem::Maximize => "Maximize Pane".to_owned(),
            PaneMenuItem::Restore => "Restore Pane".to_owned(),
            PaneMenuItem::Rename => format!("Rename {what}…"),
            PaneMenuItem::EditLabels => "Edit Labels…".to_owned(),
            PaneMenuItem::CopyLastOutput => "Copy Last Output".to_owned(),
            PaneMenuItem::SaveOutput => "Save Output…".to_owned(),
            PaneMenuItem::OpenOutput => "Open Output in Editor".to_owned(),
            PaneMenuItem::Restart => format!("Restart {what}"),
            // It asks first (unless told not to).
            PaneMenuItem::Kill => format!("Kill {what}…"),
            PaneMenuItem::Close => "Close Pane".to_owned(),
        }
    }

    /// The window's key binding for it (see `app::keymap`).
    pub fn shortcut(self) -> Option<Shortcut> {
        match self {
            PaneMenuItem::Copy => cmd("c"),
            PaneMenuItem::Paste => cmd("v"),
            PaneMenuItem::SplitRight => cmd("d"),
            PaneMenuItem::SplitDown => cmd_shift("d"),
            PaneMenuItem::Maximize | PaneMenuItem::Restore => cmd_shift("enter"),
            PaneMenuItem::Restart => cmd("r"),
            PaneMenuItem::Kill => cmd_shift("w"),
            PaneMenuItem::Close => cmd("w"),
            PaneMenuItem::SaveOutput => cmd_shift("s"),
            PaneMenuItem::Rename
            | PaneMenuItem::EditLabels
            | PaneMenuItem::CopyLastOutput
            | PaneMenuItem::OpenOutput => None,
        }
    }
}

/// What a pane's menu depends on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaneMenuState {
    /// Copy and Paste are offered (a right-click on the grid).
    pub on_grid: bool,
    /// Some text is selected (Copy is enabled).
    pub has_selection: bool,
    /// Another pane can be added.
    pub can_split: bool,
    pub maximized: bool,
    /// The pane's agent exited or is gone.
    pub restartable: bool,
    /// The pane shows an agent (Rename, Edit Labels and the output items
    /// are offered).
    pub has_agent: bool,
    /// The pane shows a running agent (Kill Agent is offered).
    pub killable: bool,
}

/// A pane's menu, with each item's enabled state.
pub fn pane_menu(s: PaneMenuState) -> Vec<MenuEntry<(PaneMenuItem, bool)>> {
    let mut out = Vec::new();
    if s.on_grid {
        out.push(MenuEntry::Item((PaneMenuItem::Copy, s.has_selection)));
        out.push(MenuEntry::Item((PaneMenuItem::Paste, true)));
        out.push(MenuEntry::Separator);
    }
    if !s.maximized {
        out.push(MenuEntry::Item((PaneMenuItem::SplitRight, s.can_split)));
        out.push(MenuEntry::Item((PaneMenuItem::SplitDown, s.can_split)));
    }
    let size = if s.maximized {
        PaneMenuItem::Restore
    } else {
        PaneMenuItem::Maximize
    };
    out.push(MenuEntry::Item((size, true)));
    // Its output (ADR 0086).
    if s.has_agent {
        out.push(MenuEntry::Separator);
        for item in [
            PaneMenuItem::CopyLastOutput,
            PaneMenuItem::SaveOutput,
            PaneMenuItem::OpenOutput,
        ] {
            out.push(MenuEntry::Item((item, true)));
        }
    }
    // The agent's own items, in one group: Rename and Edit Labels, then
    // Restart (ended) or Kill (running).
    let mut agent = Vec::new();
    if s.has_agent {
        agent.push(PaneMenuItem::Rename);
        agent.push(PaneMenuItem::EditLabels);
    }
    if s.restartable {
        agent.push(PaneMenuItem::Restart);
    } else if s.killable {
        agent.push(PaneMenuItem::Kill);
    }
    if !agent.is_empty() {
        out.push(MenuEntry::Separator);
        out.extend(agent.into_iter().map(|i| MenuEntry::Item((i, true))));
    }
    out.push(MenuEntry::Separator);
    out.push(MenuEntry::Item((PaneMenuItem::Close, true)));
    out
}

#[cfg(test)]
mod tests {
    use super::{
        GroupBy, MenuEntry, PaneMenuItem as P, PaneMenuState, RowMenuItem as R, group_menu,
        pane_menu, row_menu,
    };

    #[test]
    fn the_group_menu_checks_the_current_grouping() {
        assert_eq!(
            group_menu(GroupBy::Repo),
            [(GroupBy::Folder, false), (GroupBy::Repo, true)]
        );
        assert_eq!(
            GroupBy::ALL.map(GroupBy::title),
            ["Group by Folder", "Group by Repository"]
        );
    }

    fn items<T: Copy>(m: &[MenuEntry<T>]) -> Vec<T> {
        m.iter()
            .filter_map(|e| match e {
                MenuEntry::Item(i) => Some(*i),
                MenuEntry::Separator => None,
            })
            .collect()
    }

    #[test]
    fn row_menu_offers_kill_or_restart_and_remove() {
        assert_eq!(
            items(&row_menu(true, false)),
            [
                R::OpenSplit,
                R::Rename,
                R::EditLabels,
                R::SaveOutput,
                R::OpenOutput,
                R::Kill
            ]
        );
        assert_eq!(
            items(&row_menu(true, true)),
            [
                R::OpenSplit,
                R::Rename,
                R::EditLabels,
                R::SaveOutput,
                R::OpenOutput,
                R::Restart,
                R::Remove
            ]
        );
        assert_eq!(
            items(&row_menu(false, false)),
            [
                R::Rename,
                R::EditLabels,
                R::SaveOutput,
                R::OpenOutput,
                R::Kill
            ]
        );
        assert_eq!(R::SaveOutput.title(), "Save Output…");
        assert_eq!(row_menu(false, true)[0], MenuEntry::Item(R::Rename));
        assert_eq!(R::Kill.title(), "Kill…");
        assert_eq!(R::Rename.title(), "Rename…");
        assert_eq!(R::EditLabels.title(), "Edit Labels…");
    }

    #[test]
    fn pane_menu_follows_the_pane() {
        let header = PaneMenuState {
            can_split: true,
            ..Default::default()
        };
        assert_eq!(
            items(&pane_menu(header)),
            [
                (P::SplitRight, true),
                (P::SplitDown, true),
                (P::Maximize, true),
                (P::Close, true)
            ]
        );
        let grid = PaneMenuState {
            on_grid: true,
            has_selection: false,
            can_split: false,
            maximized: false,
            restartable: true,
            has_agent: true,
            killable: false,
        };
        assert_eq!(
            items(&pane_menu(grid)),
            [
                (P::Copy, false),
                (P::Paste, true),
                (P::SplitRight, false),
                (P::SplitDown, false),
                (P::Maximize, true),
                (P::CopyLastOutput, true),
                (P::SaveOutput, true),
                (P::OpenOutput, true),
                (P::Rename, true),
                (P::EditLabels, true),
                (P::Restart, true),
                (P::Close, true)
            ]
        );
        let running = PaneMenuState {
            has_agent: true,
            killable: true,
            ..Default::default()
        };
        assert_eq!(
            pane_menu(running)[3..],
            [
                MenuEntry::Separator,
                MenuEntry::Item((P::CopyLastOutput, true)),
                MenuEntry::Item((P::SaveOutput, true)),
                MenuEntry::Item((P::OpenOutput, true)),
                MenuEntry::Separator,
                MenuEntry::Item((P::Rename, true)),
                MenuEntry::Item((P::EditLabels, true)),
                MenuEntry::Item((P::Kill, true)),
                MenuEntry::Separator,
                MenuEntry::Item((P::Close, true))
            ]
        );
        let maximized = PaneMenuState {
            maximized: true,
            can_split: true,
            ..Default::default()
        };
        assert_eq!(
            items(&pane_menu(maximized)),
            [(P::Restore, true), (P::Close, true)]
        );
    }

    #[test]
    fn menus_never_start_or_end_with_a_separator_or_double_one() {
        let states = (0..128u8).map(|bits| PaneMenuState {
            on_grid: bits & 1 != 0,
            has_selection: bits & 2 != 0,
            can_split: bits & 4 != 0,
            maximized: bits & 8 != 0,
            restartable: bits & 16 != 0,
            killable: bits & 32 != 0,
            has_agent: bits & 64 != 0,
        });
        let check = |seps: Vec<bool>| {
            assert!(!seps.first().unwrap() && !seps.last().unwrap());
            assert!(seps.windows(2).all(|w| !(w[0] && w[1])));
        };
        for s in states {
            check(
                pane_menu(s)
                    .iter()
                    .map(|e| *e == MenuEntry::Separator)
                    .collect(),
            );
        }
        for (split, ended) in [(true, true), (true, false), (false, true), (false, false)] {
            check(
                row_menu(split, ended)
                    .iter()
                    .map(|e| *e == MenuEntry::Separator)
                    .collect(),
            );
        }
    }

    #[test]
    fn shortcuts_match_the_key_bindings() {
        assert_eq!(P::SplitDown.shortcut().unwrap().key, "d");
        assert!(P::SplitDown.shortcut().unwrap().shift);
        assert_eq!(P::Restart.shortcut().unwrap().key, "r");
        assert_eq!(P::Restore.shortcut(), P::Maximize.shortcut());
        assert_eq!(P::SaveOutput.shortcut().unwrap().key, "s");
        assert!(P::SaveOutput.shortcut().unwrap().shift);
    }
}
