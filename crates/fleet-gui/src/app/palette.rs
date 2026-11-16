//! The command palette's glue to gpui: each [`Command`]'s action, and its
//! shortcut as the window's key bindings have it.

use gpui::{Action, Window};

use super::keymap::{
    ClosePane, FocusDown, FocusLeft, FocusPane1, FocusPane2, FocusPane3, FocusPane4, FocusRight,
    FocusUp, InstallSkill, NewAgent, NewTerminal, Quit, RemoveExited, RestartPane, SplitDown,
    SplitRight, SwitchFleetd, ToggleMaximize,
};
use crate::core::palette::{Command, Mods, shortcut_text};

/// The action a command dispatches.
pub fn command_action(c: Command) -> Box<dyn Action> {
    match c {
        Command::NewAgent => Box::new(NewAgent),
        Command::NewTerminal => Box::new(NewTerminal),
        Command::SplitRight => Box::new(SplitRight),
        Command::SplitDown => Box::new(SplitDown),
        Command::ClosePane => Box::new(ClosePane),
        Command::ToggleMaximize => Box::new(ToggleMaximize),
        Command::RestartPane => Box::new(RestartPane),
        Command::FocusLeft => Box::new(FocusLeft),
        Command::FocusRight => Box::new(FocusRight),
        Command::FocusUp => Box::new(FocusUp),
        Command::FocusDown => Box::new(FocusDown),
        Command::FocusPane1 => Box::new(FocusPane1),
        Command::FocusPane2 => Box::new(FocusPane2),
        Command::FocusPane3 => Box::new(FocusPane3),
        Command::FocusPane4 => Box::new(FocusPane4),
        Command::SwitchFleetd => Box::new(SwitchFleetd),
        Command::InstallSkill => Box::new(InstallSkill),
        Command::Quit => Box::new(Quit),
        Command::RemoveExited => Box::new(RemoveExited),
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
