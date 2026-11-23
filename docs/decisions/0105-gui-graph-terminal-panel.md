# 0105 — GUI: an agent's terminal inside the graph view

**Status:** accepted · 2026-11-23 (issue #168, umbrella #149)

Builds on [0073](0073-graph-view.md) (the graph view, whose keys this changes), [0103](0103-gui-peek.md) (Peek: the observer connection and the headless pane), [0022](0022-protocol-v2.md) (the `observe` attach) and [0029](0029-gui-core-toolkit-free.md) (core stays toolkit-free).

## Context

In the graph you could not see what an agent was doing without leaving it: a click or ⏎ on a card closed the graph and showed the agent in a pane. With a big tree that meant going back and forth and losing your place. Peek (0103) shows a live screen, but only while the pointer rests on a card, as a popover that goes away.

## Decision

- **A terminal panel docked to the right of the graph.** It shows the selected card's agent, live and interactive: scrollback, find (⌘F), copy and paste, and typing goes to the agent (answer a prompt, ⌃C). The graph stays visible and keeps its pan, zoom and selection. The panel is closed whenever the graph opens, and closes when the graph is hidden (⌘⇧G, ⌘⏎, a double-click, Esc) or another fleetd is chosen.
- **It is the pane's terminal view, not a new one.** A `PaneView` with `headless` set (as the peek's), under its own pane id `GRAPH_PANEL_PANE = PaneId(u32::MAX - 1)`, just below the peek's. Layout ids count up from 1, so neither collides with a layout pane. The panel's title strip and its header facts are the peek's (`peek_view::chrome_of`).
- **Connection: a read-only `observe` attach** (`Session::open_observer`, shared with the peek). It never takes the primary role and never resizes the agent, so a pane showing the same agent keeps its size, and the main layout's panes and tabs are never touched. Input still goes out as `SendInput`. The panel draws the agent's grid at the panes' font and shrinks it (to at least 8 pt, `core::peek::fit_within`) to fit the panel; past that the grid is clipped. When the selection moves, the same connection attaches to the new agent (`SessionApi::attach`), which detaches the old one. Closing the panel detaches and never kills the agent.
- **An agent that exits** keeps its last screen in the panel, and the title strip says *Exited* (as a pane keeps it).
- **Size and place.** The panel starts at 45 % of the window. Dragging the divider on its left edge resizes it, between 320 pt and 80 % of the window. The width is kept for the session (also across closing and reopening), and a double-click on the divider resets it. While the graph would keep less than 420 pt beside it, the panel goes **over** the graph's right side instead of squeezing it (`core::graph_panel::placement`).
- **Keys and clicks.**
  - A click on a card selects it; the arrows move the selection as before. An open panel follows the selection, and the keys stay with the graph.
  - **Space or ⏎** opens the panel on the selected card, or gives the open panel the keys.
  - **Esc in the panel** gives the keys back to the graph; the panel stays. **Esc in the graph** closes the panel first, then clears the filter, then leaves the graph (`core::graph_panel::graph_escape`). While the terminal's find bar is open, its Esc closes the find bar.
  - **⌘⏎ or a double-click** is the old "open": the graph closes and the agent is revealed in a pane (0073). ⌘⏎ does this in the panel too, as in the peek. Because Esc and ⌘⏎ are taken, the panel cannot send Esc to the agent; a pane can.
  - **Collapse moves from Space to C**, since Space now opens the panel.
  - The title strip shows the status dot, the name, `#id`, *Exited* when it ended, **Open in Pane ⌘⏎**, **Actions…** (the agent's row menu, the same as the sidebar's right-click: rename, labels, send, save output, kill, restart, remove…) and a close button.
- **The keys are key bindings**, so the ⌘/ cheat sheet lists them under *Graph* and *Graph Panel*. The graph's are bound in `Graph && !TextField`, so the filter field still types Space, ⏎ and C. The panel's Esc is bound in `GraphPanel && !finding`. Bindings run before the terminal's own key handling, so these keys never reach the agent. The cheat sheet names a narrowed context by its first name.
- **Where the code lives.** `core::graph_panel` (the pane id, the width, the placement, the divider drag and the Esc order, with unit tests) and `app::graph_panel` (the panel view, the slot the graph keeps, and the graph's open, follow, close and layout methods). `graph_view.rs`, `workspace.rs` and `app_state.rs` get small hooks only. GUI tests are in `tests/graph_panel.rs`.

## Consequences

- No protocol change. An open panel costs one more connection to fleetd.
- A closed panel costs one `Option` check per graph frame, so `gui_graph_open_ns` should not move (not measured: `PERF_CI` is off).
- Space no longer collapses a subtree in the graph and a single click no longer leaves it. Both are muscle-memory changes from 0073.
- The panel shows the agent at the size its primary client gave it. In a narrow panel the text can be small, down to 8 pt.
