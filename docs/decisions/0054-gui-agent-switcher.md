# 0054 — Agent switcher (⌃Tab) on the shared picker

**Status:** accepted · 2026-11-16 (issue #42, part of #26)

Builds on [0043](0043-gui-command-palette.md) (the reusable picker), [0029](0029-gui-core-toolkit-free.md) (core stays toolkit-free) and [0031](0031-native-look.md) (tokens, motion).

## Context

Jumping between agents took the mouse or remembering pane numbers. ⌘Tab's gesture (hold a modifier, tap to cycle, release to choose) is the one Mac users already know.

## Decision

- **Most recently used, per window, in memory** (`core::switcher::AgentMru`): the agent the focused pane shows becomes the most recent, however it got there (sidebar click, palette pick, a focus change between panes, the switcher). The workspace observes it once per frame (a layout lookup and a compare; nothing allocated unless it changed). Not saved across launches, like the palette's recents.
- **The list** (`core::switcher::switcher_items`): every agent, as the palette's agent row (#48's `name` label if set, else "program in folder") with the detail `#id · folder · status` and, as a new dim second line (`PickerItem::preview`), the agent's last output line. The shared `Picker` orders it: the MRU agents first, most recent first, then the rest newest first (the MRU becomes the picker's `Recents`).
- **The keys**: ⌃Tab opens the switcher on the agent used before the current one (`start_row`; on the first row when the focused pane shows no agent), ⌃⇧Tab on the least recent; more presses while open move the selection, wrapping. Releasing ⌃ shows the selection in the focused pane, like a sidebar click. Esc or a click beside it cancels; Enter or a click on a row picks. A release only picks if ⌃ was seen down while the switcher was open (so a switcher opened some other way waits for Enter).
- **The view** is `PickerView::list`: the palette's card without the query field (the list takes the keyboard), centred in the window like ⌘Tab, with the theme tokens (Reduce Transparency) and the sheets' `appear` motion (off with Reduce Motion). It is shown at once, without ⌘Tab's short delay.
- Not over a sheet, the connect sheet or the palette; a session switch closes it. ⌃Tab and ⌃⇧Tab no longer reach the terminal.

## Consequences

- The per-frame cost with the switcher closed is the MRU observe and one `Option` check in the render.
- The picker grew a list-only mode and an optional preview line; the palette uses neither.
