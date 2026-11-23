//! The command palette's glue to gpui: each [`Command`]'s action, and its
//! shortcut as the window's key bindings have it.
//!
//! Every `fleet::*` action has a command, unless [`NOT_IN_PALETTE`] says
//! why not; the tests below fail when one is added without either
//! (ADR 0080).

use gpui::{Action, Window};

use super::keymap::{ArchiveWorktree, RunAction, ShowChanges};
use super::keymap::{
    ClearScrollback, ClosePane, CloseTab, Copy, CopyLastOutput, DuplicatePane, EditLabelsPane,
    Find, FindNext, FindPrevious, FocusDown, FocusFilter, FocusLeft, FocusPane1, FocusPane2,
    FocusPane3, FocusPane4, FocusRight, FocusUp, InstallSkill, KillPane, KillSelected, NewAgent,
    NewTab, NewTerminal, NextAgent, NextNeedsYou, NextTab, OpenOutput, OpenSettings, Paste,
    PrevTab, PreviousAgent, Quit, RemoveExited, RemovePane, RemoveSelected, RenamePane,
    ReopenClosedPane, RestartPane, RestartSelected, SaveOutput, ScrollPageDown, ScrollPageUp,
    ScrollToBottom, ScrollToTop, SearchAllAgents, SelectAll, SelectTab, SendToAgents,
    ShowShortcuts, SplitDown, SplitRight, SwitchFleetd, ToggleGraph, ToggleMaximize, ZoomIn,
    ZoomOut, ZoomReset,
};
use crate::core::palette::{Command, Mods, shortcut_text};

/// The `fleet::*` actions the palette leaves out, and why (checked by
/// the tests below).
#[cfg(test)]
const NOT_IN_PALETTE: &[(&str, &str)] = &[
    ("fleet::CommandPalette", "opens the palette itself"),
    // The palette takes the focus from the text field these act on.
    ("fleet::Cut", "text fields only"),
    ("fleet::Undo", "text fields only"),
    ("fleet::Redo", "text fields only"),
    // Row menu actions naming an agent; the palette's Kill Agent…,
    // Restart Agent in Pane and Remove Exited Agent in Pane act on the
    // focused pane's agent instead.
    ("fleet::Kill", "names an agent (row menu)"),
    ("fleet::Restart", "names an agent (row menu)"),
    ("fleet::Remove", "names an agent (row menu)"),
    // The graph's keys (ADR 0105): they act on the graph's selected card
    // or its terminal panel, which the palette takes the focus from.
    ("fleet::OpenAgentPanel", "graph only"),
    ("fleet::OpenInMainView", "graph only"),
    ("fleet::BackToGraph", "graph panel only"),
    ("fleet::ToggleChildren", "graph only"),
    // The diff pane's own keys (ADR 0094); the palette takes the focus
    // from the pane they act on, and Show Changes refreshes an open one.
    ("diff_pane::RefreshDiff", "the focused diff pane's ⌘R"),
    ("diff_pane::CopyDiff", "the focused diff pane's ⌘C"),
    ("diff_pane::SelectAllDiff", "the focused diff pane's ⌘A"),
];

/// The action a command dispatches.
pub fn command_action(c: Command) -> Box<dyn Action> {
    match c {
        Command::NewAgent => Box::new(NewAgent),
        Command::NewTerminal => Box::new(NewTerminal),
        Command::SplitRight => Box::new(SplitRight),
        Command::SplitDown => Box::new(SplitDown),
        Command::ClosePane => Box::new(ClosePane),
        Command::ReopenClosedPane => Box::new(ReopenClosedPane),
        Command::ToggleMaximize => Box::new(ToggleMaximize),
        Command::RestartPane => Box::new(RestartPane),
        Command::KillPane => Box::new(KillPane),
        Command::RenamePane => Box::new(RenamePane),
        Command::EditLabelsPane => Box::new(EditLabelsPane),
        Command::DuplicatePane => Box::new(DuplicatePane),
        Command::RemovePane => Box::new(RemovePane),
        Command::Find => Box::new(Find),
        Command::SearchAllAgents => Box::new(SearchAllAgents),
        Command::FindNext => Box::new(FindNext),
        Command::FindPrevious => Box::new(FindPrevious),
        Command::Copy => Box::new(Copy),
        Command::Paste => Box::new(Paste),
        Command::CopyLastOutput => Box::new(CopyLastOutput),
        Command::SaveOutput => Box::new(SaveOutput),
        Command::OpenOutput => Box::new(OpenOutput),
        Command::FocusLeft => Box::new(FocusLeft),
        Command::FocusRight => Box::new(FocusRight),
        Command::FocusUp => Box::new(FocusUp),
        Command::FocusDown => Box::new(FocusDown),
        Command::FocusPane1 => Box::new(FocusPane1),
        Command::FocusPane2 => Box::new(FocusPane2),
        Command::FocusPane3 => Box::new(FocusPane3),
        Command::FocusPane4 => Box::new(FocusPane4),
        Command::NextAgent => Box::new(NextAgent),
        Command::NextNeedsYou => Box::new(NextNeedsYou),
        Command::PreviousAgent => Box::new(PreviousAgent),
        Command::FocusFilter => Box::new(FocusFilter),
        Command::NewTab => Box::new(NewTab),
        Command::CloseTab => Box::new(CloseTab),
        Command::NextTab => Box::new(NextTab),
        Command::PrevTab => Box::new(PrevTab),
        Command::SelectTab(number) => Box::new(SelectTab { number }),
        Command::ZoomIn => Box::new(ZoomIn),
        Command::ZoomOut => Box::new(ZoomOut),
        Command::ZoomReset => Box::new(ZoomReset),
        Command::RemoveExited => Box::new(RemoveExited),
        Command::KillSelected => Box::new(KillSelected),
        Command::RestartSelected => Box::new(RestartSelected),
        Command::RemoveSelected => Box::new(RemoveSelected),
        Command::ToggleGraph => Box::new(ToggleGraph),
        Command::SendToAgents => Box::new(SendToAgents),
        Command::ShowShortcuts => Box::new(ShowShortcuts),
        Command::OpenSettings => Box::new(OpenSettings),
        Command::ScrollPageUp => Box::new(ScrollPageUp),
        Command::ScrollPageDown => Box::new(ScrollPageDown),
        Command::ScrollToTop => Box::new(ScrollToTop),
        Command::ScrollToBottom => Box::new(ScrollToBottom),
        Command::SelectAll => Box::new(SelectAll),
        Command::ClearScrollback => Box::new(ClearScrollback),
        Command::SwitchFleetd => Box::new(SwitchFleetd),
        Command::InstallSkill => Box::new(InstallSkill),
        Command::Quit => Box::new(Quit),
        Command::ShowChanges => Box::new(ShowChanges),
        Command::RunAction => Box::new(RunAction),
        Command::ArchiveWorktree => Box::new(ArchiveWorktree),
    }
}

/// The command's shortcut where the focus is now (`⇧⌘D`), if bound.
pub fn command_shortcut(c: Command, window: &Window) -> Option<String> {
    let action = command_action(c);
    let binding = window.highest_precedence_binding_for_action(action.as_ref())?;
    let texts: Vec<String> = binding
        .keystrokes()
        .iter()
        .map(|ks| {
            let m = ks.modifiers();
            let mods = Mods {
                ctrl: m.control,
                alt: m.alt,
                shift: m.shift,
                cmd: m.platform,
            };
            shortcut_text(mods, ks.key())
        })
        .collect();
    Some(texts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::keymap::bind_keys;
    use gpui::TestAppContext;

    fn left_out(name: &str) -> bool {
        NOT_IN_PALETTE.iter().any(|(n, _)| *n == name)
    }

    /// Every `fleet::*` action, bound or not, has a palette command (or a
    /// reason in `NOT_IN_PALETTE`). A new action fails here until it gets
    /// a `Command` (core/palette.rs) and its `command_action`.
    #[gpui::test]
    fn every_fleet_action_has_a_palette_command(cx: &mut TestAppContext) {
        let names: Vec<&'static str> = cx.update(|cx| cx.all_action_names().to_vec());
        let commands: Vec<&'static str> = Command::ALL
            .iter()
            .map(|&c| command_action(c).name())
            .collect();
        let missing: Vec<_> = names
            .iter()
            .filter(|n| n.starts_with("fleet::"))
            .filter(|n| !left_out(n) && !commands.contains(n))
            .collect();
        assert!(
            missing.is_empty(),
            "no palette command for {missing:?}: add one to core::palette::Command \
             and app::palette::command_action, or list it in NOT_IN_PALETTE"
        );
        // The list stays honest: what it names exists and has no command.
        for (name, _) in NOT_IN_PALETTE {
            assert!(names.contains(name), "{name} is not an action");
            assert!(!commands.contains(name), "{name} has a command too");
        }
    }

    /// Every key binding, with its exact action (⌘⌥3 is Go to Tab 3), is
    /// a palette command.
    #[gpui::test]
    fn every_key_binding_is_a_palette_command(cx: &mut TestAppContext) {
        cx.update(|cx| {
            bind_keys(cx);
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            for b in keymap.bindings() {
                let action = b.action();
                if left_out(action.name()) {
                    continue;
                }
                assert!(
                    Command::ALL
                        .iter()
                        .any(|&c| command_action(c).partial_eq(action)),
                    "{action:?} ({:?}) is bound but not in the palette",
                    b.keystrokes()
                );
            }
        });
    }
}
