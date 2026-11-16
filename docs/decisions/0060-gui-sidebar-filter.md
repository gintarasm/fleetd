# 0060 — GUI sidebar filter and collapsible groups

**Status:** accepted · 2026-11-16 (M2d, issue #40)

Builds on [0025](0025-core-knows-agents-workflows-own-hierarchy.md) (the sidebar groups by the `project` label, else the cwd), [0029](0029-gui-core-toolkit-free.md) (the GUI core stays toolkit-free) and [0042](0042-gui-agent-status.md) (agent status).

## Context

With many agents the sidebar gets long, and it can't be narrowed. Issue #40 asks for a filter field, status filters and groups that collapse.

## Decision

- **Where the logic lives.** Matching, status filters and collapsing are in `core::sidebar` (`query_matches`, `StatusFilter`, `sidebar_groups`, `toggle_collapsed`) and tested there. The shell only draws the result.
- **Filter field.** It sits at the top of the sidebar. ⌘⇧F focuses it, and so does typing while the sidebar's tabs have the keyboard (a click on the sidebar's empty space gives it to them).
  - The text is split into words, ignoring case. Each word must appear in the row's name as the row shows it (the OSC title, else the program), the program, the cwd (as stored or with `~`), or a label as `key=value`.
  - Escape clears the text; a second Escape gives the keyboard back to the focused pane. Enter shows the first agent listed.
  - The text is not saved: a filter left on after a relaunch would hide agents.
- **Status filters.** The options are All, Needs you, Running and Exited, built on ADR 0042's statuses:
  - Needs you is the `NeedsYou` status. Shells have no status, so they never match it.
  - Running is any running process, shells included, whatever its status.
  - Exited is exited or lost (done and failed).
  - The choice is not saved either.
- **Groups.**
  - A click on a group's header collapses or expands it. A collapsed group shows `▸` and how many of its agents pass the filter.
  - Collapsed groups are remembered per target in `gui-state.toml` (`collapsed_groups`), saved with the layout's debounce.
  - While filtering, groups with no match are hidden. While text is typed, collapsed groups open to show what matched, without changing what is remembered.
- No animation and no new translucent surface. Reduce Motion and Reduce Transparency need nothing new.

## Consequences

- The status filter is a row of chips, not a dropdown: four short labels fit the 240 pt sidebar.
- A group is remembered by its title, so renaming a `project` label (or moving a cwd) forgets its collapse.
