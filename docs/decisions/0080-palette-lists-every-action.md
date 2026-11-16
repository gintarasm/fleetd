# 0080 — Every action in the command palette

**Status:** accepted · 2026-11-16 (issue #92, part of #26)

Builds on [0043](0043-gui-command-palette.md) (the palette) and [0029](0029-gui-core-toolkit-free.md) (core stays toolkit-free).

## Context

The palette listed 19 commands while the keymap had about 40 `fleet::*` actions. Kill, Rename, Find, zoom, tabs, Settings, the cheat sheet and others could only be found by shortcut or menu, and nothing stopped the two lists drifting further apart.

## Decision

- **One command per action.** `core::palette::Command` has a variant for every `fleet::*` action, named after it; `app::palette::command_action` maps each to its action (the match is exhaustive). Parameterised actions get a variant per value the keymap binds: `SelectTab(1..=9)` are "Go to Tab N".
- **Left out on purpose**, listed with a reason in `app::palette::NOT_IN_PALETTE`: `CommandPalette` (itself), `Cut`/`SelectAll`/`Undo`/`Redo` (text fields only; the palette takes their focus) and the row menus' `Kill`/`Restart`/`Remove { agent_id }`, which name an agent. The palette acts on the focused pane's agent instead.
- **Two new unbound actions** so every palette command is an action: `RenamePane` (Rename Agent…) and `RemovePane` (Remove Exited Agent in Pane, as the pane's summary bar). They act on the focused pane's agent.
- **Drift tests** (`app::palette`): every registered `fleet::*` action (bound or not) has a command or a `NOT_IN_PALETTE` reason, the list only names real actions without commands, and every key binding's exact action (by `partial_eq`, so ⌥⌘3 must be `SelectTab(3)`) is a command. A branch adding an action adds its `Command` variant, its `ALL` entry, its title and its `command_action` arm.
- **Shown only when it can act.** `PaletteState` (panes, tabs, and the focused pane's agent: shown, running, ended, selection) decides `Command::shown`; the rest are hidden, not greyed (the picker has no disabled rows, and a hidden row keeps the list short):
  - Rename, Find, Find Next/Previous: the focused pane shows an agent; Kill and Paste: a running one; Restart and Remove: one that exited or is lost; Copy: a selection.
  - Focus moves, Focus Pane N (N ≤ panes) and Maximize: more than one pane; splits: below the pane limit.
  - Close/Next/Previous Tab and Go to Tab N (N ≤ tabs): more than one tab; New Tab: below the tab limit.
  The state mirrors the pane menu's (`PaneMenuState`). The actions still check for themselves, as from their shortcuts.
- **Shortcut glyphs stay live** (ADR 0043): read from the window's key bindings. Where two spellings are bound (⌘} and ⌘⇧] for Show Next Tab), the shifted one is bound last so the palette shows `⇧⌘]`, as the Window menu does.

## Consequences

- The palette lists about 50 commands at most; it still builds its rows once when opened.
- A new keymap action fails `cargo test -p fleet-gui` until it is in the palette or explicitly left out.
- Next/Previous Agent from the palette open the switcher without ⌃ held; it stays open until Enter or Esc.
