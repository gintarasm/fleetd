# 0103 — GUI: Peek at an agent's live screen

**Status:** accepted · 2026-11-23 (issue #142, umbrella #149)

Builds on [0022](0022-protocol-v2.md) (the `observe` attach), [0028](0028-gui-client.md) (the GUI client), [0083](0083-gui-send-to-agents.md) (the sidebar's row selection) and [0089](0089-graph-message-lines.md) (the graph view). Core logic stays toolkit-free ([0029](0029-gui-core-toolkit-free.md)).

## Context

To check on or unblock an agent you had to show it in a pane, which replaces what the focused pane showed. The sidebar row's preview is one line. Quick Look in Finder solves this for files: Space shows the file over the window, and Escape puts it away.

## Decision

- **Peek** is a popover over the window with the agent's live screen. Nothing in the layout changes. Only one peek is open at a time: opening another one replaces it.
- **How it opens.**
  - **Sidebar:** Space while the sidebar's rows have the keyboard peeks at the selected row. That is the last row clicked (`RowSelection::anchor`, so a ⌘-click selects a row and keeps the keyboard in the sidebar), or else the last row picked. With no row selected, Space still starts the filter as before.
  - **Graph:** the pointer resting on a card for 700 ms (`HOVER_HOLD`) with no button held. Moving to another card restarts the wait. Leaving the card, a press on it, or a pan cancels it. Space in the graph still collapses a subtree, so the graph has no key for it.
- **Keys in the peek.** Escape closes it. A click outside also closes it. ⌘Return shows the agent in the focused pane and leaves the graph if the graph was shown. Every other key goes to the agent through the pane's usual encoding, so you can answer a prompt. While a peek is open, the workspace counts as modal: the workspace's own shortcuts (split, close, palette…) wait until it closes. Because Escape closes the peek, a peek cannot send Escape to the agent. ⌘Return in a pane can.
- **Connection.** The peek has its own pane connection, `PEEK_PANE = PaneId(u32::MAX)`. Layout ids count up from 1, so it never collides. It attaches with `Attach.observe = true` (ADR 0022): it never takes or changes the primary role, and its writer drops resizes (`PaneLink::open_as(.., observe)`, `Session::open_peek`). Input still goes out as `SendInput`, which does not promote an observer. Terminal query replies are already discarded for a non-primary model. Closing the peek detaches and never kills the agent. Replacing the session closes the peek with the session's other panes.
- **Size.** An observer draws at the agent's size, so the popover is sized from the agent's grid. It uses the panes' font (zoom included). The font shrinks (to at least 8 pt) when the grid would exceed 85 % of the window (`core::peek::fit`). The view is a `PaneView` with `headless` set: no header and no question strip. The peek's title strip shows the status dot, the name, `#id` and the two key hints.
- **Where the code lives.** `core::peek` (the target row, the key mapping, the hover-hold timer, the fit, each with unit tests) and `app::peek_view` (the popover). The workspace, the graph, the app state and the session get small hooks only.

## Consequences

- No protocol change. A peek costs one more connection to fleetd while it is open.
- The agent keeps the size its primary client gave it. On a small window the peek's text can be small, down to 8 pt. Past that the grid is clipped at the right and bottom.
- A closed peek costs one `Option` check per frame in the workspace. Each graph card carries one hover listener. The hold timer runs only after the pointer enters a card.
- An agent that exits while you peek shows its final screen until you close the peek.
