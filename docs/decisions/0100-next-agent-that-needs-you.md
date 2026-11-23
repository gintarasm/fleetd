# 0100 — Next agent that needs you (⌘J)

**Status:** accepted · 2026-11-23 (issue #139)

**Builds on:** [0042](0042-gui-agent-status.md) (statuses, the Dock and menu-bar badge), [0075](0075-fleet-report-and-inferred-status.md) (reported status), [0080](0080-palette-lists-every-action.md) (the palette lists every action), [0029](0029-gui-core-toolkit-free.md) (toolkit-free GUI core).

## Context

The most common move with many agents is "go to the one that needs me". The badge counts them, but nothing navigated to one. ⌃Tab (`NextAgent`) walks agents by recency, which says nothing about who is waiting.

## Decision

- **Action `NextNeedsYou`**, bound to **⌘J** (checked against `app/keymap.rs`: no ⌘J binding existed; the cheat sheet (⌘/) is built from the bindings and lists it). Also the palette's "Next Agent That Needs You" and the fleet menu's item of that name.
- **Order** (`core/needs_you.rs`): agents whose status is `NeedsYou`, the one that has waited longest first; then `Failed` agents, the one failed longest ago first. Ties go by id. Each press shows the agent after the focused pane's agent in that list, wrapping; when the focused agent is not in the list, the first. Answering an agent (or it ending) removes it from the list, so ⌘J from a handled agent lands on the next oldest.
- **Waiting since:** the node does not say when a status began, so the GUI records the time it first saw each agent in its current status (`Since`, updated in `AppState::refresh_status`, reset on a target switch). Waits begun before the GUI connected count from the connect, in id order.
- **Shown** in the focused pane, as a sidebar click does (`Workspace::show_agent`). Not over a sheet or the palette.
- **None:** no change, and the neutral toast "No agent needs you".
- **The badge:** a Dock click (reopen) shows the window and, when agents need you, goes to the first of them. The menu-bar status menu gets "Next agent that needs you" under "Show fleetd", listed only while one needs you (the status item's button opens the menu, so the entry is the click target).

## Consequences

- No hot-path change: a hash-map update once a second with the status refresh; perf not measured (`PERF_CI` off).
- Failed agents stay in the cycle until removed; clear exited agents to drop them.
