//! App actions, their cmd-key bindings, and the conversion of gpui
//! keystrokes into [`Key`]s for the PTY encoder.

use gpui::{Action, App, KeyBinding, Keystroke, actions};

use crate::core::keys::{Key, KeyCode, KeyMods};
use crate::core::shortcuts::{self, Binding, Section};

actions!(
    fleet,
    [
        SplitRight,
        SplitDown,
        ClosePane,
        FocusLeft,
        FocusRight,
        FocusUp,
        FocusDown,
        FocusPane1,
        FocusPane2,
        FocusPane3,
        FocusPane4,
        NewAgent,
        NewTerminal,
        Copy,
        Paste,
        Quit,
        ToggleMaximize,
        SwitchFleetd,
        InstallSkill,
        RestartPane,
        KillPane,
        SelectAll,
        Cut,
        Undo,
        Redo,
        ReopenClosedPane,
        ShowShortcuts,
        Find,
        FindNext,
        FindPrevious,
        CommandPalette,
        NextAgent,
        PreviousAgent,
    ]
);

/// Kill an agent, after the user confirms.
#[derive(Clone, Debug, PartialEq, Action)]
#[action(namespace = fleet, no_json)]
pub struct Kill {
    pub agent_id: u64,
}

/// Restart an exited or lost agent from the sidebar (no confirmation).
#[derive(Clone, Debug, PartialEq, Action)]
#[action(namespace = fleet, no_json)]
pub struct Restart {
    pub agent_id: u64,
}

/// Forget an exited or lost agent (no confirmation).
#[derive(Clone, Debug, PartialEq, Action)]
#[action(namespace = fleet, no_json)]
pub struct Remove {
    pub agent_id: u64,
}

/// Key context of the workspace window.
pub const WORKSPACE_CONTEXT: &str = "Workspace";
/// Key context of a terminal pane.
pub const TERMINAL_CONTEXT: &str = "Terminal";
/// Key context of a text field.
pub const TEXT_FIELD_CONTEXT: &str = "TextField";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-d", SplitRight, None),
        KeyBinding::new("cmd-shift-d", SplitDown, None),
        KeyBinding::new("cmd-w", ClosePane, None),
        KeyBinding::new("cmd-alt-left", FocusLeft, None),
        KeyBinding::new("cmd-alt-right", FocusRight, None),
        KeyBinding::new("cmd-alt-up", FocusUp, None),
        KeyBinding::new("cmd-alt-down", FocusDown, None),
        KeyBinding::new("cmd-1", FocusPane1, None),
        KeyBinding::new("cmd-2", FocusPane2, None),
        KeyBinding::new("cmd-3", FocusPane3, None),
        KeyBinding::new("cmd-4", FocusPane4, None),
        KeyBinding::new("cmd-n", NewAgent, None),
        KeyBinding::new("cmd-t", NewTerminal, None),
        KeyBinding::new("cmd-p", CommandPalette, None),
        KeyBinding::new("ctrl-tab", NextAgent, None),
        KeyBinding::new("ctrl-shift-tab", PreviousAgent, None),
        KeyBinding::new("cmd-shift-enter", ToggleMaximize, None),
        KeyBinding::new("cmd-r", RestartPane, Some(WORKSPACE_CONTEXT)),
        // Kills the focused pane's agent (asking first); ⌘W only detaches.
        KeyBinding::new("cmd-shift-w", KillPane, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd-c", Copy, Some(TERMINAL_CONTEXT)),
        KeyBinding::new("cmd-v", Paste, Some(TERMINAL_CONTEXT)),
        // Also from the pane's find field, inside the terminal's context.
        KeyBinding::new("cmd-f", Find, Some(TERMINAL_CONTEXT)),
        KeyBinding::new("cmd-g", FindNext, Some(TERMINAL_CONTEXT)),
        KeyBinding::new("cmd-shift-g", FindPrevious, Some(TERMINAL_CONTEXT)),
        KeyBinding::new("cmd-v", Paste, Some(TEXT_FIELD_CONTEXT)),
        KeyBinding::new("cmd-c", Copy, Some(TEXT_FIELD_CONTEXT)),
        KeyBinding::new("cmd-x", Cut, Some(TEXT_FIELD_CONTEXT)),
        KeyBinding::new("cmd-a", SelectAll, Some(TEXT_FIELD_CONTEXT)),
        KeyBinding::new("cmd-z", Undo, Some(TEXT_FIELD_CONTEXT)),
        KeyBinding::new("cmd-shift-z", Redo, Some(TEXT_FIELD_CONTEXT)),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-shift-t", ReopenClosedPane, None),
        KeyBinding::new("cmd-/", ShowShortcuts, None),
    ]);
}

/// The ⌘/ cheat sheet: every key binding registered now, whoever bound it.
pub fn cheat_sheet(cx: &App) -> Vec<Section> {
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    let bindings: Vec<(String, &'static str, Option<String>)> = keymap
        .bindings()
        .filter(|b| !gpui::is_no_action(b.action()))
        .map(|b| {
            let strokes: Vec<String> = b.keystrokes().iter().map(|k| k.unparse()).collect();
            (
                strokes.join(" "),
                b.action().name(),
                b.predicate().map(|p| p.to_string()),
            )
        })
        .collect();
    shortcuts::cheat_sheet(bindings.iter().map(|(keys, action, context)| Binding {
        keys,
        action,
        context: context.as_deref(),
    }))
}

/// The key a keystroke encodes; `None` for cmd-keys (app shortcuts) and
/// keys the encoder has no name for.
///
/// With Option held, `text` is the shift-only character (gpui's `key_char`
/// then holds the Option-composed glyph, which Option-as-Meta must not
/// send). Without Option it is gpui's `key_char`.
pub fn to_key(ks: &Keystroke) -> Option<Key> {
    let m = &ks.modifiers;
    if m.platform {
        return None;
    }
    let code = match ks.key.as_str() {
        "enter" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "escape" => KeyCode::Escape,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "insert" => KeyCode::Insert,
        "delete" => KeyCode::Delete,
        "space" => KeyCode::Char(' '),
        name => {
            if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                if !(1..=12).contains(&n) {
                    return None;
                }
                KeyCode::F(n)
            } else {
                let mut chars = name.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => KeyCode::Char(c),
                    _ => return None,
                }
            }
        }
    };
    let text = match code {
        KeyCode::Char(c) if m.alt => {
            // gpui reports shifted non-letters as the shifted character
            // with shift off, so only letters need upper-casing here.
            let shifted = if m.shift {
                c.to_uppercase().next()
            } else {
                None
            };
            Some(shifted.unwrap_or(c).to_string())
        }
        _ => ks.key_char.clone(),
    };
    Some(Key {
        code,
        mods: KeyMods {
            shift: m.shift,
            alt: m.alt,
            ctrl: m.control,
        },
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::to_key;
    use crate::core::keys::{Key, KeyCode, KeyMods};
    use gpui::Keystroke;

    fn parse(s: &str) -> Keystroke {
        Keystroke::parse(s).unwrap()
    }

    fn key(code: KeyCode, shift: bool, alt: bool, ctrl: bool, text: Option<&str>) -> Key {
        Key {
            code,
            mods: KeyMods { shift, alt, ctrl },
            text: text.map(str::to_owned),
        }
    }

    #[test]
    fn to_key_maps_names_and_mods() {
        let enter = Keystroke {
            key_char: Some("\n".into()),
            ..parse("enter")
        };
        assert_eq!(
            to_key(&enter),
            Some(key(KeyCode::Enter, false, false, false, Some("\n")))
        );
        assert_eq!(
            to_key(&parse("shift-tab")),
            Some(key(KeyCode::Tab, true, false, false, None))
        );
        assert_eq!(
            to_key(&parse("alt-b")),
            Some(key(KeyCode::Char('b'), false, true, false, Some("b")))
        );
        assert_eq!(
            to_key(&parse("ctrl-c")),
            Some(key(KeyCode::Char('c'), false, false, true, None))
        );
        assert_eq!(
            to_key(&parse("f5")),
            Some(key(KeyCode::F(5), false, false, false, None))
        );
        assert_eq!(
            to_key(&parse("up")),
            Some(key(KeyCode::Up, false, false, false, None))
        );
        assert_eq!(
            to_key(&parse("space")),
            Some(key(KeyCode::Char(' '), false, false, false, None))
        );
        assert_eq!(to_key(&parse("cmd-c")), None);
        assert_eq!(to_key(&parse("f13")), None);

        // Option+Shift: the composed glyph in key_char is ignored.
        let alt_shift_b = Keystroke {
            key_char: Some("ı".into()),
            ..parse("alt-shift-b")
        };
        assert_eq!(
            to_key(&alt_shift_b),
            Some(key(KeyCode::Char('b'), true, true, false, Some("B")))
        );
        // A shifted non-letter arrives as the shifted character, shift off.
        let alt_bang = Keystroke {
            key_char: Some("⁄".into()),
            ..parse("alt-!")
        };
        assert_eq!(
            to_key(&alt_bang),
            Some(key(KeyCode::Char('!'), false, true, false, Some("!")))
        );
    }
}
