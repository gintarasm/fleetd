# 0043 — Command palette (⌘P) on a reusable fuzzy picker

**Status:** accepted · 2026-11-16 (issue #29, part of #26)

Builds on [0029](0029-gui-core-toolkit-free.md) (core stays toolkit-free) and [0031](0031-native-look.md) (tokens, motion).

## Context

Every GUI action needed its shortcut or a menu; there was no keyboard-first way to find an action or an agent. The split chooser (#30) and the agent switcher (#42) want the same kind of list, so the list is built once.

## Decision

- **Fuzzy matching in the core** (`core::fuzzy`): the query's characters, in order, case-insensitive, spaces ignored. An optimal-alignment score (not greedy): +16 per character, +12 at the string's start, +10 at a word start (after space, `/`, `-`, `_`, `.`, `:`, `=`, or a lower-to-upper change), +8 for a run, −1 per skipped character between matches. Positions are byte offsets, for highlighting.
- **The picker's state in the core** (`core::picker::Picker`): items (key, title, detail, hidden keywords, shortcut), the query, the ranked matches and the selection. Order: an empty query lists recent picks first (most recent first), then the items' order; a query sorts by score, a recent pick getting a bonus of 12 for the latest, one less for each older one (so it wins near-ties, not clear losses), then recency, then the items' order. ↑/↓ wrap.
- **The shell's picker** (`app::picker_view::PickerView`): a query field over a floating list on a raised card; ↑/↓ or ⌃P/⌃N, Enter, Esc, hover selects, click picks. It emits `Pick(key)` or `Dismiss`; the owner decides what a key means. Its appearance uses the theme tokens (so Reduce Transparency's flattened tokens apply) and the sheets' `appear` motion (off with Reduce Motion).
- **⌘P** (`CommandPalette`, also under the fleet menu) opens the palette, or closes it if open; not over a sheet. It lists:
  - every window action, with its shortcut read from the live key bindings (so it never drifts from the keymap) and shown as a Mac menu does (`⇧⌘D`); a pick dispatches the same action the shortcut would, to the focused pane. Clear exited has no action and is called directly;
  - every agent, newest first: its `name` label if set (#48), else "program in folder", then folder and status; its labels are matched too. A pick shows it in the focused pane, like a sidebar click;
  - the New agent sheet's folders (recent folders for the target, then the agents' folders). A pick spawns `[gui] default_command` there, in the focused pane, like the sheet.
- **Recency is per window, in memory**: recent picks are not saved across launches. Saving them would add a field to the GUI state file for little gain; revisit if it is missed.

## Consequences

- The palette is not a hot path: it builds its rows when opened and matches on each edit (a few dozen rows). The per-frame cost of the closed palette is one `Option` check in the workspace render.
- The split chooser and the agent switcher can reuse `PickerView` with their own items and key meanings.
- A session switch closes the palette (its agent ids would be stale), as it closes the New agent sheet.
