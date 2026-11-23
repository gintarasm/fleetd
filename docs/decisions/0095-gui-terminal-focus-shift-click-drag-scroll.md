# 0095 — Terminal input: focus reports, Shift-click extends, drag-select scrolls

**Status:** accepted · 2026-11-23 (issue #134)

Builds on [0082](0082-gui-terminal-keys.md) and [0063](0063-scroll-feedback.md).

## Context

fleet-term tracked DECSET 1004 (focus events) but the GUI never sent the reports, so vim/nvim `autoread`, tmux and other TUIs never heard of focus. A click always started a new selection, and a drag-select stopped at the grid's edge: no selection longer than a screen.

## Decision

- **Focus reports.** A pane is focused when its window is active and keyboard focus is in it (the pane, its find bar or a split's chooser). When that changes and the program has DECSET 1004 on, it gets `CSI I` (in) or `CSI O` (out). The change is noted while the mode is off too, so a program that turns the mode on hears only later changes; nothing is sent when it turns the mode on (as xterm). Reports do not return a scrolled-up view to the grid (they are not typing). The pane follows its window's activation and its own focus-in/out; the window that draws it can change (the app is resident), so it re-subscribes then, and checks once more after the frame that subscribed, since gpui starts focus listeners a frame late.
- **Shift-click extends.** With Shift held and a selection present, a single click moves the selection's moving end to the clicked cell (the anchor stays; a word or line selection snaps as when dragging), and a drag from there keeps extending. Shift still bypasses mouse reporting, so this works in mouse-mode programs too. Without a selection, Shift-click starts one.
- **Drag-select auto-scroll.** While a drag-select's pointer is above or below the grid, a repeating 50 ms timer scrolls the view: one line a tick just past the edge, one more per line height further, at most 10. Each tick extends the selection to the pointer's column on the row now at that edge, and asks for older history pages. The timer stops on the first tick that finds the pointer back over the grid or the drag released. No scrolling on the alternate screen (no history) or without a selection.
- **Core.** `encode_focus`, `drag_scroll_lines` (and its constants) in `core/mouse.rs` and `PaneModel::drag_scroll` are toolkit-free and unit tested; the pane view only tracks focus and pointer and runs the timer.

## Consequences

- One timer only while a drag is held past an edge; nothing on the render path.
- Dragging past the bottom of a view that follows the grid does not scroll (there is nothing below); the selection extends to the bottom row as before.
- Shift-click to extend applies to cell clicks; Shift-double-click still selects a word anew.
