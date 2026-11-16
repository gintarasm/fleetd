# 0056 — GUI: the shortcut cheat sheet comes from the keymap; reopen closed pane; first-run welcome

**Status:** accepted · 2026-11-16 (issue #46)

## Context

Shortcuts could only be found in menus. Closing a pane could not be undone. A first launch showed a blank pane. Several open branches add key bindings at the same time (⌘P, ⌘,, ⌘⇧W, ⌃Tab, ⌘F, ⌘+/−), so a hand-written list of shortcuts would be out of date as soon as they merged.

## Decision

- **Cheat sheet (⌘/, Help ▸ Keyboard Shortcuts).** The sheet reads gpui's registered keymap (`cx.key_bindings()`) each time it opens. Nothing lists the shortcuts by hand.
  - Each binding's keystrokes are drawn as macOS glyphs, with modifiers in menu order (⌃⌥⇧⌘).
  - The title is the action's name split into words (`fleet::ReopenClosedPane` becomes "Reopen Closed Pane"). An action bound later shows up without any change here.
  - The sheet has one section per key context: Window (no context, or `Workspace`), Terminal, Text Fields, then any other context. A later binding of the same keys in the same section replaces the earlier one, as it does in gpui.
  - The formatting and grouping live in `core::shortcuts` and are tested there (ADR 0029). Escape, ⌘/ or a click outside closes the sheet. While it is open, the pane shortcuts do nothing.
- **Reopen closed pane (⌘⇧T).** Closing a pane that shows an agent pushes the agent onto a stack of the 10 most recently closed. ⌘⇧T takes the newest one that the agent list still has, and drops any above it that are gone.
  - The agent opens in the focused pane if that pane is empty, otherwise in a new split. If no split fits, the agent stays on the stack.
  - The stack is not saved, and switching fleetd clears it.
- **Welcome.** When the agent list has arrived and is empty, an empty pane shows a welcome with New Agent, Terminal, Install Agent Skill and a link to the shortcuts. Before the list arrives, the pane shows the usual one-line hint, so the welcome never flashes.

## Consequences

- Action names are now user-visible text, so a new action needs a readable CamelCase name.
- A binding that should not appear on the sheet would need a filter in `keymap::cheat_sheet`. Today only gpui's `NoAction` is filtered out.
