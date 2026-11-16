# 0059 — Drag and drop of agents and panes

**Status:** accepted · 2026-11-16 (issue #34, M2d)

Builds on [0045](0045-gui-resizable-splits.md) (split ratios) and [0029](0029-gui-core-toolkit-free.md) (toolkit-free core).

## Context

Putting an agent in a pane, or rearranging panes, takes the row menu or the keyboard. The owner wants to drag: a sidebar row onto a pane, or a pane by its header onto another pane.

## Decision

- **Core.** `core::drag_drop` holds a press (`Drag`), what is dragged (`Payload::Agent` or `Payload::Pane`), the drop target under the pointer (`DropTarget`: a pane and a `Zone`), what a drop does (`DropAction`) and the area to highlight. The shell feeds it pointer positions in the pane area's pixels and the rects of the panes drawn; it names no toolkit type.
- **Starting.** A left press on a sidebar row or on a pane's header (the top `PANE_HEADER_HEIGHT` of the card) becomes a drag once the pointer has travelled `DRAG_THRESHOLD` (4 px); a shorter one stays a click. The header's buttons stop the press, so they still click. A drag that ends on its own row is not a click.
- **Zones.** Within `EDGE_SHARE` (a quarter) of a pane's width or height from an edge is that edge's band; the nearest edge, as a share of the extent, wins in a corner. The rest is the middle.
- **Drops.** A row on a pane's middle shows the agent there; on an edge band, it splits that pane evenly with the new pane on that side showing the agent. With `MAX_PANES` panes an agent's edge bands count as the middle. A pane on another's middle swaps the two; on an edge band, it leaves its place (its split collapses into the other side) and goes beside the target in an even split. A pane is never its own target. The pane the drop lands in gets the focus and the keyboard. While a pane is maximized only it is drawn, so only an agent can be dropped, on it or (splitting, which restores the split view) beside it.
- **Ratios.** Swapping keeps every split's ratio; moving keeps every split but the one the pane left (gone) and the one it joins (even). Layouts are saved as usual.
- **Feedback.** While dragging, the cursor is a closed hand and the target zone is outlined in the accent colour: the whole pane for the middle, the half a new or moved pane would take for an edge. The outline has a light accent tint, dropped under Reduce Transparency. There is no animation, so Reduce Motion has nothing to change.
- **Cancel.** Esc ends a drag with nothing done, before the focused pane sees the key; so does a release outside every pane.

## Consequences

- Pane ids stay with their panes through a swap or a move, so each pane keeps its connection, scrollback and agent; only its place changes, and its terminal resizes like after a divider drag.
- gpui's own `on_drag`/`on_drop` is not used: drop zones need the pointer's position within the pane, and the logic stays in the core and testable without a window.
- In shell tests, `debug_bounds` keeps selectors from earlier frames, so tests read `Workspace::drop_target` to see that no drag is under way.
