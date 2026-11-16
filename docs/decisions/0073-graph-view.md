# 0073 — The graph view

**Status:** accepted · 2026-11-16 (builder's decisions within spec §7 of the [M3 design](../superpowers/specs/2026-11-16-m3-satellite-design.md); issue #103)

**Builds on:** [0033](0033-satellite-milestone.md) (the view's purpose and keys), [0025](0025-core-knows-agents-workflows-own-hierarchy.md) (`fleet.parent`, `project` label), [0042](0042-agent-status.md) (inferred status), [0071](0071-kill-escalates-and-shells-are-not-agents.md) (kill escalation), [0014](0014-performance-gate.md) (benchmarks).

## Context

The spec settles what the graph shows and how it is drawn (gpui elements for cards, one `canvas` for edges, viewport culling, layout only on tree changes). It leaves spacing, the details of the layout, the keys' exact behaviour and colours to the builder. This records those choices.

## Decision

### Tree and layout (`fleet-gui/src/core/graph.rs`, `graph_layout.rs`, gpui-free)

- **Parent** is the `fleet.parent` label: a local id, or `node/id` (the id part is used until M6). A parent that is not listed (removed) makes the child a root. Labels are never trusted to form a tree: a cycle is broken at its smallest id, which becomes a root.
- **Layout is a band layout.** A subtree takes one row if it is a leaf or collapsed, else the sum of its children's rows; a parent sits on its first child's row, in the column of its depth (roots on the left). Siblings and roots are ordered by id, project boxes by title. Positions depend on the tree only, so an agent coming or going moves only what is below it (its later siblings' subtrees, later trees and boxes). A centred tidy tree was rejected: a new child would move its parent.
- **Boxes.** One machine box (titled *This Mac*, or the ssh destination) holds one box per project. A tree goes in its **root's** project box; a child working in another project stays with its parent and shows its own project on its card (edges never cross boxes).
- **Project** is the `project` label (as the sidebar groups, 0025), else the git root of the cwd (the nearest directory with a `.git` file or directory, looked up once per cwd and only for a local fleetd), else the cwd, with `~` for home.
- **Card size** 232 × 84, columns 48 apart (edges elbow in the middle), rows 12 apart.
- **Collapse** (Space) is per agent for the session, in memory; everything starts expanded. A collapsed card shows its **direct** child count ("3 children").
- **Filter** (`/`): each word must be in the card's name, program, cwd, a `key=value` label, its status (as named in the GUI: "Needs you", "Working"…) or be its id. Matches keep their ancestors shown. ⏎ in the field goes back to the graph, Esc clears it, a second Esc gives the keys back.
- **Layout is recomputed only when its signature changes**: the `(id, parent, project)` of every agent, the collapsed set and the filter's result. Activity (new output, status, run time) redraws only the cards in view.

### View (`fleet-gui/src/app/graph_view.rs`)

- A gpui entity kept for the session (collapse, selection, pan and zoom survive toggling) and recreated on a switch to another fleetd. While shown it replaces the sidebar and panes; the titlebar stays.
- Cards are absolutely positioned divs, built only when they overlap the viewport. One `canvas` paints the boxes and every edge (one path for live edges, one for edges to ended agents, which are dimmer). Box titles are small elements (a handful).
- **Zoom** is 25–200 %: ⌘+/⌘−/⌘0 (scale the graph, not the terminal font, while it is shown), ⌘- or ⌃-scroll, and a trackpad pinch. gpui 0.2.2 delivers no magnify events, so a local `NSEvent` monitor (`native::monitor_magnify`) forwards pinches to the graph while it is shown. Below 60 % a card shows only its dot, name and run time. Pan is a background drag or a scroll; content cannot be panned fully out of sight.
- **Card:** status dot, name (as the sidebar names it), run time; `program · project`; the last activity line; the child count when collapsed. Ended cards are at half opacity. A *waiting* (needs you) card gets the needs-you colour (the warning colour its dot and the sidebar use) as border and tint; the **accent** marks the selected card, so the two never look alike.
- **Status dots** are hollow: only the inferred status exists. `core::graph::card_dot` is the one seam where M3-D makes a reported status win and draws it solid (`DotFill::Solid`).

### Keys

- ⌘⇧G or the titlebar's graph button toggles the view; the command palette has *Show or Hide the Agent Graph*. In a terminal ⌘⇧G is Find Previous **only while its find bar is open** (the pane adds `finding` to its key context); otherwise it toggles the graph.
- ← parent, → first child shown, ↑/↓ the nearest card above/below **in the same column** (the layout reads as an outline turned sideways).
- ⏎ or a click opens: the graph closes and the agent's pane is focused if one shows it (the shown tab first, then other tabs); otherwise the focused pane shows it. Esc goes back without opening.
- ⌫ **Stop subtree** always asks (even with `confirm_kill` off: it may stop many agents), then sends a `KillAgent` per running agent of the subtree, deepest first; each escalates as 0071 says.
- ⇧⌫ **Remove subtree** acts only when everything in the subtree has ended. It sends one `ForgetAgents` with every id, asking first when that is more than one agent (a single one is removed without a question, like the row menu's Remove).
- Pane shortcuts (⌘D, ⌘W…) still act on the hidden panes while the graph is shown; the palette and ⌃Tab still work.

### Benchmarks

- `gui_graph_open_ns`: fleetd with 50 idle agents; the GUI (`FLEET_GUI_BENCH=graph-open`) toggles the graph on 20 times (off in between) and times each from the dispatched `ToggleGraph` to its first presented frame; median. Budget 100 ms.
- `gui_graph_frame_flood50_ns`: 50 `fake-agent --forever` flood; the GUI (`graph-flood`, `FLEET_GUI_BENCH_AGENTS=50`) shows the graph and times 100 window redraws (after 20 warm-up) from the start of the workspace's render to the end of the graph's paint; median. This is the frame's CPU cost, without the wait for the display. Budget 16 ms.
- fleetd refuses `fleet.*` labels from a client, so both bench modes draw a **synthetic tree** (fan-out 3, by id) over agents with no parent; the edges are drawn as in real use.
- The GUI hooks (`app/bench.rs`) and the measuring functions (`fleet-perf/src/wall/gui.rs`) exist; **`wall.rs` does not call them yet.** It is gate code (`approval.rs` `GATE_PATHS`), so the hookup, and the baseline entries without values, are left to a commit of the user's with `Perf-Approved` (the diff is in the PR). No baselines are recorded until the user records them from CI.

## Consequences

- M3-C builds a tree in `fleet-client` in parallel. If both exist at merge, `core::graph::Forest` can be replaced by it; the layout only needs roots, children by id and parents.
- M3-D changes `card_dot` (and nothing else in the view) to show reported status.
- Wave 3 adds message lines to the edges canvas (a second path set).
