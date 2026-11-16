# 0087 — GUI: edit an agent's labels after it started

**Status:** accepted · 2026-11-16 (M2d, issue #99)

Builds on [0025](0025-core-knows-agents-workflows-own-hierarchy.md) (labels, `SetLabels`, who may write them), [0052](0052-agent-names.md) (the `name` label and Rename), [0035](0035-agent-roles.md) (`role`) and [0060](0060-gui-sidebar-filter.md) (the sidebar filter).

## Context

The New agent sheet takes labels at spawn and Rename changes the name, but the GUI cannot change any other label afterwards. Labels drive the sidebar's groups and chips, its filter and the palette's search, so a typo or a missing `role` stays until the agent is restarted. Issue #99 asks for an Edit Labels sheet, and for a label chip to filter the sidebar.

## Decision

### Protocol: none new

- fleetd already has what is needed: `SetLabels { agent_id, set, remove }` (0025) sets, then removes, labels of any known agent, running or ended, checks the label limits and refuses `fleet.*` keys, and persists the result in the store (`replace_labels`), so the change survives a daemon restart and an agent restart. Rename (0052) is a `SetLabels` of `name` alone.
- **No proto change and no protocol version bump.** Every fleetd the GUI can talk to answers `SetLabels`.
- The GUI's session generalises its rename command into one `SetLabels` command (`Session::set_labels`, with `Session::rename` a call of it), so both go through the same tracked request: a refusal shows like a failed rename, and the list is fetched right after so the sidebar follows.

### The sheet

- **Edit Labels…** is in the sidebar row's menu (after Rename…), the pane's menu (after Rename Agent…, when the pane shows an agent), and the command palette (ADR 0080: the unbound `EditLabelsPane` action, shown like Rename Agent… when the focused pane has an agent), for running and ended agents alike.
- One `key=value` field per editable label, sorted by key, each with a remove (×) button, then **Add Label**. An agent with no editable labels opens with one empty row.
- Each row is checked like the New agent sheet's Labels field (`core::labels::parse_label`, which `parse_label_list` now uses too): `key=value`, key and value trimmed, the key not empty. Blank rows are skipped. A key given twice is an error.
- **Read-only:** the `name` (Rename changes it) and the core's `fleet.*` labels are listed under the rows, dimmed, and cannot be set or removed here. A row naming either is an error, so a save never touches them.
- **Save** (or Return) sends one `SetLabels` with only the difference against the labels the sheet opened with: `set` holds the added and changed labels, `remove` the editable keys no row keeps (`core::labels::label_edit`). Nothing changed sends nothing. Escape or Cancel closes it.
- The sheet edits a snapshot: a label changed elsewhere while it is open is overwritten only if the user's rows change or drop that key.

### Chips filter

- A click on a row's label chip puts its `key=value` into the sidebar filter, added to the words already typed; a second click on the same chip takes it out again (`core::labels::chip_filter`). The click does not also show the agent.
- The filter matches by substring (0060), so `role=review` also matches `role=reviewer2`; good enough for the chips' short values.

### Who may change labels

- **The user.** The GUI and the CLI act for the user; any label but the core's may be changed on any agent.
- **Not agents.** 0025 still holds: labels select grants and quotas, so an agent must not write them. Until the M4 agent channel enforces that (0025's M2a note), labels are unauthenticated, so the rule is kept by the fleet skill, which tells agents labels are set by the user and never to pass `--label` (`--name` is 0052's exception). This ADR does not change the skill.

### CLI

- No new command: `fleet label <id|name> k=v… --rm k…` already sends `SetLabels` (0052 made it take a name). Unlike the sheet, it does not stop a `name` label set this way, but it checks it as a name.

## Consequences

- The sheet is GUI-only work; fleetd, the protocol and the CLI are unchanged.
- Changing `project` regroups the agent and, as 0060 notes, forgets its old group's collapse.
