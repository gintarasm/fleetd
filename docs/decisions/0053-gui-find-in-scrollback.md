# 0053 — Find in a pane's scrollback

**Status:** accepted · 2026-11-16 (issue #36)

Builds on [0021](0021-agent-history.md), [0029](0029-gui-core-toolkit-free.md) and [0046](0046-gui-clickable-links.md).

## Context

Agents print long output and there was no way to search it. A pane keeps up to 100,000 history lines loaded as raw ANSI ([0021](0021-agent-history.md)); parsing all of them takes far longer than a frame, and the GUI's `input_to_render_*` gates must not move.

## Decision

- **Gesture.** ⌘F (also Edit ▸ Find…) opens a find bar at the top right of the focused pane, over the grid (the grid is not resized, so the agent's PTY is not either). Typing searches as you type. Enter and ⌘G go to the next match **up** (older), ⇧Enter and ⌘⇧G back down, wrapping at either end, as in iTerm: you search scrollback from the bottom. The bar shows "3 of 17" ("17+" while lines are left to scan, "No matches"), an **Aa** case toggle (off: case-insensitive), up/down and close buttons. Esc closes it and gives the keys back to the terminal. ⌘F again brings the last query back, selected.
- **What matches.** Plain text, within one row, on the cells as drawn (colours and other SGR between letters do not split a match); not across a soft wrap (rows carry no WRAPLINE marker, as with links). Matches do not overlap.
- **Where.** The loaded history and the grid. When the search reaches the oldest loaded line and the node has older lines, the pane pages them in (the same `ReadHistory` paging as scrolling up, up to the pane's history cap) and the search continues into them.
- **Off the render path.** The search is incremental state in the core (`src/core/find.rs`, kept in `PaneModel`). The pane view runs it in steps of 2,000 lines on gpui's background executor, each step under the model lock, and redraws when a step finds something. Drawing only looks the visible rows up in the matches found so far (a binary search per row), and nothing at all while find is closed. History is numbered as the node numbers it and grid row `r` as `end_line + r`, so a match found on the grid keeps its place when its row scrolls into history; new output marks the grid for a rescan and the lines it pushed into history are scanned on the next step.
- **Showing a match.** The first match picked is the newest at or above the view's bottom; it and every match stepped to are scrolled into view (to the middle, unless already visible). All matches get a translucent yellow fill, the current one orange (`find_match`, `find_current` tokens). No animation, so Reduce Motion has nothing to change; the bar is drawn on `raised`, opaque under Reduce Transparency.

## Consequences

- With find open, each step holds the pane's model lock for about a millisecond, delaying that pane's output by as much; with it closed, nothing changes.
- A clear (`CSI 3 J`) or a re-attach restarts the search from the newest line; lines dropped past the history cap drop their matches.
- Matches are kept for every loaded line: a one-letter query over a full history can hold a few million matches (16 bytes each) until the query changes or find closes.
- Regex and whole-word search are possible follow-ups.
