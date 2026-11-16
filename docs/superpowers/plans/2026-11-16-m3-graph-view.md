# M3-B: graph view (⌘⇧G) — implementation plan

Issue #103 · spec §7 of `docs/superpowers/specs/2026-11-16-m3-satellite-design.md` (without message lines) · ADR 0073.

## Pieces

1. **`core/graph.rs` (gpui-free, unit-tested)**
   - `parent_of(info)`: the `fleet.parent` label as a local id (`node/id` keeps the id part).
   - `Forest::build(rows, project_of)`: parents, children sorted by id, roots; unknown or cyclic parents make roots.
   - `layout(forest, collapsed, filter-visible set) -> GraphLayout`: boxes (machine ⊃ projects, sorted by title), trees within a box sorted by root id, a band layout (a subtree takes `max(1, Σ children)` rows, a parent sits on its first child's row, column = depth). Stable: a node coming or going moves only what is below it.
   - Edge geometry (elbow polylines), collapse with hidden-descendant counts, viewport (pan, zoom about a point, clamp) and culling, keyboard navigation (←/→ parent/first child, ↑/↓ nearest card in the same column), subtree helpers (stop order deepest first, all-ended check, ids), the filter (name, status, `key=value`; ancestors of a match stay).
   - A `Signature` of what the layout depends on, so the view re-lays out only when it changes.
2. **`app/graph_view.rs`**: a gpui entity drawn instead of sidebar + panes. Cards are absolutely positioned divs, only those in the viewport; one `canvas` paints all edges and boxes. Keys per spec, pan (drag, scroll), zoom (pinch, ⌘± while shown). Hollow status dots now; `DotFill` is the seam for M3-D's reported (solid) status.
3. **Workspace**: `ToggleGraph` (⌘⇧G, titlebar button); ⏎/click returns to panes with the agent focused (its pane in any tab, else the focused pane shows it). In a terminal ⌘⇧G stays Find Previous only while the find bar is open.
4. **Benchmarks**: bench modes `graph-open` / `graph-flood` in `app/bench.rs`, measuring fns in `fleet-perf/src/wall/gui.rs`. `wall.rs` is a gate file: its hookup goes in the PR body for the user. No baselines.
5. ADR 0073; tests, clippy, fmt. Perf not measured (PERF_CI off).
