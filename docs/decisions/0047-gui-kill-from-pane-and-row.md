# 0047 — Kill an agent from its pane header and sidebar row

**Status:** accepted · 2026-11-16 (issue #27, M2d)

Builds on [0029](0029-gui-core-toolkit-free.md), [0030](0030-agent-restart-and-forget.md) and [0031](0031-native-look.md).

## Context

The pane header's × closes the pane and leaves the agent running (detach). Killing took a trip to the sidebar row's context menu, which asked in a native alert. The owner wants a kill button next to the agent, a shortcut, and a question that can be turned off.

## Decision

- **Buttons.** A Stop button (Lucide `circle-stop`, vendored like the others; never the × of Close) in the pane header, left of the split buttons, and on the sidebar row, over its "since" time. On the focused pane it is always shown; on other panes and on rows only while hovered (gpui `visibility`, so a hidden button takes no clicks). An exited or lost agent shows Restart and Remove there instead; the header's Restart appears once the pane holds the final screen, as ⌘R. What each slot offers is `core::kill::agent_buttons`.
- **Shortcut.** ⌘⇧W kills the focused pane's running agent (asking first); the pane menu shows it as "Kill Agent…". macOS apps often use ⌘⇧W for Close Window; fleet-gui has one window, closed with its red button (the app stays resident), so the key is free, and it sits next to ⌘W (detach): same pane, stronger action.
- **Question.** "Kill agent 7 (claude, ~/fleetd)?" with Kill / Cancel and a "Don't ask again" checkbox, in a sheet drawn by gpui (`app::kill_sheet`), not the native alert used before: gpui's `window.prompt` has no suppression checkbox, and a drawn sheet is testable headless. Return kills, Escape cancels. It follows Reduce Motion like the other sheets.
- **Setting.** `[gui] confirm_kill` (default `true`). "Don't ask again" writes `false` with `toml_edit`, keeping comments and other keys; while it is `false` every kill path kills at once. Turning it back on is by editing the file until the settings window (#28) offers it.

## Consequences

- Every kill path (row menu, row button, header button, pane menu, ⌘⇧W) goes through `Workspace::kill`, so the question and the setting apply the same way everywhere.
- The two new icons (`circle-stop`, `check` for the checkbox) are rasterised on first draw; the header draws one more button per pane.
