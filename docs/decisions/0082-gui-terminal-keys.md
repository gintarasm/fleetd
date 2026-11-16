# 0082 — Terminal keys: scroll the scrollback, ⌘A select all, ⌘K clear

**Status:** accepted · 2026-11-16 (issue #94)

Builds on [0021](0021-agent-history.md), [0053](0053-gui-find-in-scrollback.md) and [0063](0063-scroll-feedback.md).

## Context

A terminal pane could only be scrolled back with the trackpad (⇧PageUp went to the agent), had no Select All, and no way to clear a cluttered view. Every macOS terminal has all three.

## Decision

- **Keys, in the terminal's key context** (so the cheat sheet lists them, and the palette shows their shortcuts):
  - ⇧PageUp / ⇧PageDown (`ScrollPageUp`, `ScrollPageDown`) scroll the view by a page: the grid's rows less one, so a line of context stays.
  - ⌘↑ and ⌘Home (`ScrollToTop`) go to the oldest loaded line; ⌘↓ and ⌘End (`ScrollToBottom`) back to the live grid. ⌘⌥↑/↓ stay pane focus moves.
  - ⌘A (`SelectAll`, the Edit menu's action) selects the loaded history and the grid down to its last row with text, as a line selection, ready for ⌘C. A text field's own ⌘A wins inside it (the find field).
  - ⌘K (`ClearScrollback`) clears this pane's loaded history. ⌘K was unbound.
- **The alternate screen keeps its keys.** It has no history, so there the scroll actions propagate and ⇧PageUp/⇧PageDown reach the program as before (`CSI 5;2~`, `CSI 6;2~`). ⌘-keys never reach a program, so on the alternate screen ⌘↑/⌘↓ do nothing.
- **The find field keeps ⌘↑/⌘↓** (they move its caret); ⇧PageUp/⇧PageDown scroll the pane from it.
- **Clear is the view's only.** `Scrollback::clear_view` drops the loaded lines without bumping the history generation (that is the node's numbering; page requests must stay valid), marks the history fully loaded and sets a floor below which no page in flight adds lines. The grid is the program's and stays; ⌃L asks the program to redraw it. The agent and the node's history are untouched: `fleet history` and a re-attach (reconnect, daemon restart) bring the lines back.
- **Everywhere the issue asked.** The pane's grid context menu gains Select All, Scroll to Top, Scroll to Bottom and Clear Scrollback (each enabled when it would do something); the palette gains the six commands (shown while the focused pane has an agent, ADR 0080); the Edit menu gains Clear Scrollback. All dispatch the same actions as the keys.
- **Core.** `ViewScroll`, `page_lines`, `PaneModel::{scroll_view, can_scroll_view, select_all, clear_scrollback}` and `Scrollback::clear_view` are toolkit-free and unit tested; the pane view only maps actions onto them.

## Consequences

- Select All covers loaded history only (up to the cap); older lines are on the node, in `fleet history`.
- As with any selection that touches the grid, output from the agent drops it: in a pane whose agent is printing, ⌘A then ⌘C must come before the next output.
- No new work on the render path: everything runs on the key press.
- "Clear to here" from the context menu (clear history above a clicked line) is not done.
