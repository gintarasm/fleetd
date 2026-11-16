# 0074 — Sidebar tree and `fleet tree`

**Status:** accepted · 2026-11-16 (issue #104, M3 workstream C)

**Builds on:** [0025](0025-core-knows-agents-workflows-own-hierarchy.md) (`fleet.parent` label), [0070](0070-remove-exited-agents.md) (Remove Exited), the M3 spec §6 and §8.

## Context

Agents spawn agents (M3). The node records the spawner as the reserved label `fleet.parent` (workstream A). The sidebar and the CLI should show that tree, from the label alone, without a new protocol message.

## Decision

- **One tree builder in `fleet-client` (`tree.rs`), no UI.** `Tree::new(id, parent)` pairs / `Tree::of(&[AgentInfo])` give roots, children, parent, ancestors, descendants and a pre-order `walk` with the "last sibling" flags box-drawing needs. Siblings keep the order the caller gave. The CLI and `fleet-gui::core` both use it.
- **Missing parent → root.** A `fleet.parent` that is not in the list (removed, unknown, outside the caller's `ls` scope, or `node/id` before M6) makes the agent a root. A self-parent or a parent loop cannot come from the node; every agent on a loop becomes a root, and what hangs off the loop still hangs off it. Nothing recurses unboundedly.
- **Sidebar.** Nesting is within a sidebar group: a child whose parent sits in another group (different `project` label or cwd) is a root of its own group, so a group never shows rows that do not belong to it. Roots keep the existing order (running first, newest first); children follow their parent by id (spawn order). `SidebarGroup` gains `layout: Vec<RowLayout>` parallel to `agents` (depth, disclosure, child count, context) so the view change is small.
- **Disclosure.** A parent with more than 3 children starts collapsed; the user's choice per agent id lives in `AppState` for the session, is not saved, and is dropped when the target changes. A collapsed row shows `▸ N`.
- **Ended parents stay** greyed (the existing ended style) while listed; Clear exited does not remove them while a descendant runs.
- **Filter keeps ancestors.** While any filter (text or status) is active, rows that pass it and all their ancestors are kept, every branch is open (the stored disclosure is not changed), and ancestors that do not pass are drawn dimmed. A group's count is the rows that pass.
- **Clear exited / Remove Exited Agents removes ended branches:** every ended agent with no running agent below it. An ended agent with a running descendant stays. The confirmation, the titlebar tooltip and the button's enabled state all count the same set (`removable_ended`). When every ended agent is removable, the GUI still sends the single `ForgetAgents { exited_only }` that fleetd answers under its lock (0070). Otherwise it sends `ForgetAgents { ids }` for the removable ones; fleetd refuses a live agent in an id list, so a race can only remove less.
- **`fleet tree [--json] [--all] [--label k=v]`,** in `fleet-cli/src/tree.rs`. Box-drawing text, one line per agent: `id name  program  status  run-time` (program only when a name is shown). Siblings by id. Ended agents show only with a running descendant, unless `--all`. `--label` (repeatable, every pair must match) keeps matching agents and their ancestors, like the sidebar filter. Scoping follows `ls` (the node's list). `--json` is an array of root nodes, each with `children`.
- **Statuses in `fleet tree`:** ended agents as the GUI infers them (exit 0 `done`, else `failed`; `lost` kept as `lost`, which the GUI draws as failed); a `kind=shell` agent is `shell`. A running agent is `running`: the GUI also needs the agent's last line and last output time to tell working, needs-you and idle, and a one-shot client has no activity feed. Workstream D (0075) moves the inferred status into shared code and adds reported statuses; `fleet tree` takes them from there. The run time formatter is duplicated from the GUI (`format_run_time`) for the same reason and should move with D.

## Consequences

- Until workstream A lands, the core refuses clients writing `fleet.*`, so tests of the nested tree set the label in unit fixtures; the CLI integration test has only roots.
- The sidebar's tree is per group; a cross-group child is a root. If that proves confusing, nesting across groups is a change in `sidebar_tree::arrange` alone.
- `sidebar_groups` takes the disclosure map as a new last parameter.
