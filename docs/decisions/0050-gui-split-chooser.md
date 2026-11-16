# 0050 — Split chooser: the new pane asks what to start

**Status:** accepted · 2026-11-16 (issue #30, part of #26)

Builds on [0029](0029-gui-core-toolkit-free.md) (core stays toolkit-free), [0038](0038-gui-new-terminal.md) (New terminal) and [0043](0043-gui-command-palette.md) (the reusable picker).

## Context

⌘D opened an empty pane that said "Select an agent or press ⌘N": a split was always followed by a second step somewhere else (the sidebar, the sheet, ⌘T).

## Decision

- **Which panes.** A pane opened by a split (⌘D, ⇧⌘D, the header buttons, the pane menu, the palette's Split commands) shows the chooser inline, in place of the empty text. Not a modal: other panes, the sidebar and every shortcut still work. The first pane, and a last pane emptied by closing, keep the plain text: there is no split to cancel there. A chooser is not saved; after a relaunch the pane is plainly empty.
- **The choices.** 1 New agent: `[gui] default_command` (with an empty command, the New agent sheet instead). 2 Existing agent: the [0043](0043-gui-command-palette.md) `PickerView` inline in the pane over the agents no pane shows, newest first, as the palette lists them (name, folder, status); disabled when every agent is on screen. 3 Terminal: New terminal's login shell, in this pane (no second split).
- **Folder.** New agent and Terminal start in the split pane's agent's folder, else the last-used one, else home: New terminal's rule, applied to the pane split rather than the focused one (which is now the new pane).
- **Keys.** 1/2/3 pick; ↑/↓ and Enter, hover and click work too. Esc closes the pane, cancelling the split. In the agent list Esc goes back to the choices (the list's own query owns the digits), so a second Esc closes.
- **Lifetime.** The chooser goes when picked, cancelled, when its pane closes, when the pane gets an agent another way (a sidebar click, the palette, the sheet) or a spawn is on its way into it, and on a session switch. A click into the pane leaves the keys with the chooser; when it goes while it has the keyboard, the pane gets it back.
- **Where.** The state (rows, stage, selection, keys) is `core::split_chooser`; the view is `app::split_chooser::SplitChooserView`, drawn by `PaneView` through a slot the workspace fills. Theme tokens only, so Reduce Transparency's flattened tokens apply; no motion of its own.

## Consequences

- The closed chooser costs one empty-map check per workspace frame; while one is open, an entity-id comparison per pane.
- The agent switcher (#42) can reuse `PickerView` the same way.
