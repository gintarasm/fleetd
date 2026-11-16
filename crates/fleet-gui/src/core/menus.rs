//! The context menus: a sidebar row's and a pane's. What they offer and
//! what they are called; the shell shows them as native menus.

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
    Kill,
    Restart,
    Remove,
}

impl RowMenuItem {
    pub fn title(self) -> &'static str {
        match self {
            RowMenuItem::OpenSplit => "Open in New Split",
            RowMenuItem::Rename => "Rename…",
            // It asks first.
            RowMenuItem::Kill => "Kill…",
            RowMenuItem::Restart => "Restart",
            RowMenuItem::Remove => "Remove",
        }
    }
}

/// A row's menu: Open in New Split (while a pane can be added), Rename…,
/// then Kill for a running agent, or Restart and Remove for one that
/// exited or is lost.
pub fn row_menu(can_split: bool, ended: bool) -> Vec<MenuEntry<RowMenuItem>> {
    let mut out = Vec::new();
    if can_split {
        out.push(MenuEntry::Item(RowMenuItem::OpenSplit));
        out.push(MenuEntry::Separator);
    }
    out.push(MenuEntry::Item(RowMenuItem::Rename));
    out.push(MenuEntry::Separator);
    if ended {
        out.push(MenuEntry::Item(RowMenuItem::Restart));
        out.push(MenuEntry::Item(RowMenuItem::Remove));
    } else {
        out.push(MenuEntry::Item(RowMenuItem::Kill));
    }
    out
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
    Restart,
    Close,
}

impl PaneMenuItem {
    pub fn title(self) -> &'static str {
        match self {
            PaneMenuItem::Copy => "Copy",
            PaneMenuItem::Paste => "Paste",
            PaneMenuItem::SplitRight => "Split Right",
            PaneMenuItem::SplitDown => "Split Down",
            PaneMenuItem::Maximize => "Maximize Pane",
            PaneMenuItem::Restore => "Restore Pane",
            PaneMenuItem::Rename => "Rename Agent…",
            PaneMenuItem::Restart => "Restart Agent",
            PaneMenuItem::Close => "Close Pane",
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
            PaneMenuItem::Close => cmd("w"),
            PaneMenuItem::Rename => None,
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
    /// The pane shows an agent (Rename is offered).
    pub has_agent: bool,
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
    if s.has_agent || s.restartable {
        out.push(MenuEntry::Separator);
    }
    if s.has_agent {
        out.push(MenuEntry::Item((PaneMenuItem::Rename, true)));
    }
    if s.restartable {
        out.push(MenuEntry::Item((PaneMenuItem::Restart, true)));
    }
    out.push(MenuEntry::Separator);
    out.push(MenuEntry::Item((PaneMenuItem::Close, true)));
    out
}

#[cfg(test)]
mod tests {
    use super::{
        MenuEntry, PaneMenuItem as P, PaneMenuState, RowMenuItem as R, pane_menu, row_menu,
    };

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
            [R::OpenSplit, R::Rename, R::Kill]
        );
        assert_eq!(
            items(&row_menu(true, true)),
            [R::OpenSplit, R::Rename, R::Restart, R::Remove]
        );
        assert_eq!(items(&row_menu(false, false)), [R::Rename, R::Kill]);
        assert_eq!(row_menu(false, true)[0], MenuEntry::Item(R::Rename));
        assert_eq!(R::Kill.title(), "Kill…");
        assert_eq!(R::Rename.title(), "Rename…");
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
        };
        assert_eq!(
            items(&pane_menu(grid)),
            [
                (P::Copy, false),
                (P::Paste, true),
                (P::SplitRight, false),
                (P::SplitDown, false),
                (P::Maximize, true),
                (P::Rename, true),
                (P::Restart, true),
                (P::Close, true)
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
        let states = (0..64u8).map(|bits| PaneMenuState {
            on_grid: bits & 1 != 0,
            has_selection: bits & 2 != 0,
            can_split: bits & 4 != 0,
            maximized: bits & 8 != 0,
            restartable: bits & 16 != 0,
            has_agent: bits & 32 != 0,
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
    }
}
