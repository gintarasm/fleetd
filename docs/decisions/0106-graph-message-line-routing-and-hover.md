# 0106 — Message lines: routed around cards, and hover to explain one

**Status:** accepted · 2026-11-23 (issue #171)

**Amends:** [0089](0089-graph-message-lines.md) ("What is drawn": straight lines, and "no hover"). **Builds on:** [0073](0073-graph-view.md) (the band layout, the edges canvas).

## Context

ADR 0089 drew a message line straight from border to border. In a real tree it cut through other cards and the tree's elbows: a report from the bottom of six siblings to their parent ran diagonally under its siblings and ended on the parent's bottom border, and looked like a rendering glitch. And nothing said what a line was: 0089 left hover out on purpose, which made the lines hard to read.

## Decision

### Routing (`core/message_lines.rs`, gpui-free)

- **Orthogonal paths with square corners** (not rounded elbows): they read as a different thing from the tree's elbows only by colour, lane and arrowhead, and square corners keep the hit test and the "never crosses a card" test exact.
- **Through the layout's free channels only.** The band layout (0073) puts every column at the same x in every project box, so the **gap between two columns** is free of cards from top to bottom, and no card spans the **gap between two rows**, in any column. A line:
  - leaves its sender by the side facing the route (right side when the recipient is in a column to the right, left side when it is to the left, right side for the same column);
  - runs in a lane of that column gap; if the recipient's side opens on the same gap (parent↔child, siblings), it goes straight along it to the recipient;
  - otherwise turns into the row gap next to the recipient (above it when the sender is higher, else below), crosses to the gap beside the recipient, and turns into it;
  - enters the recipient by the side facing it, the last piece level, with the **arrowhead on the recipient's border**.
- **Kept off the tree edges.** Tree elbows run in the middle of a column gap and attach at a card side's middle. Message lanes are 6–15 units either side of the gap's middle, ports 14–36 units above or below a side's middle; row gaps hold no tree segment that runs along them.
- **Lanes and ports.** Lines in the same column gap get different lanes (8 per gap), lines in the same row gap different lanes (5), lines on the same card side different ports (8); they wrap beyond that. So the two directions of a pair run apart (this replaces 0089's fixed 3.5-unit pair offset). They are handed out in `(sender, recipient)` order, so a given set of pairs always routes the same way. The cap stays at the newest 24 pairs.
- **Cheap.** `RouteCache` routes again only when the layout (its generation number) or the set of drawn pairs changes; any other frame costs the pair list (≤ 500 id-only rows, as before) and one comparison. Lines are culled to the view by their bounds. Painting is still one path per fade step, plus at most one dimmed path and one hovered path.
- **Tests:** no routed segment passes through any card (its own two only at their borders) for siblings, parent↔child, grandchild, cousins, separate trees and cross-project pairs, and for every pair of a 16-card fleet at once; no segment runs along a tree edge; lines sharing a gap or a card side get distinct lanes and ports; routes do not depend on pair order; the cache routes again only on a change.

### Hover, click, legend (`app/graph_lines.rs`; small hooks in `graph_view.rs`)

- **Hit test** in world space: the nearest line within 5 screen pixels (5 / zoom world units, so the same at any zoom). A point inside a card is the card, never a line (a line's ends are on card borders), so line hover never triggers card hover or Peek (#142), and the reverse.
- **Hovering a line** draws it brighter and thicker on top, rings both its cards in the accent colour (painted in the canvas, so the card elements are untouched), and shows a tooltip by the pointer: `ui2-167 → fleetd-wolf · message · 2 min ago`, plus `· 3 in 5 min` when the pair sent several in the window. The kind (`message`, `question`, `answer`) comes from the existing `MessageEdge.kind`, now kept by the GUI.
- **No body preview.** The proto is unchanged: the tooltip says who, what kind and when; the pane shows the rest. A preview would mean sending bodies (0089 chose not to) for a hover; it can be added later as an optional, capped field.
- **Clicking a line** selects its sender (and pans to it), without starting a pan.
- **Hovering a card** dims the lines that do not touch it.
- **Legend:** the hint bar shows an accent swatch and "messages (last 5 min)".
- Hover is computed on mouse move from the cached routes and the layout, and redraws only when what is hovered changes (moving between cards redraws only while there are lines to dim).

## Consequences

- 0089's "straight, border to border" and "no hover" are replaced by the above; the rest of 0089 (polling, which messages, fading, the cap) stands.
- `gui_graph_open_ns` and the flood frame draw no messages, so routing does not reach them; perf not measured (`PERF_CI` off).
