# 0099 — GUI: Kill / Restart / Remove on the sidebar selection

**Status:** accepted · 2026-11-23 (issue #138, UI Improvements 2 #149)

Builds on [0083](0083-gui-send-to-agents.md) (the ⌘/⇧-click selection, `RowSelection`), [0080](0080-palette-lists-every-action.md) (every action in the palette) and [0029](0029-gui-core-toolkit-free.md) (core stays toolkit-free).

## Context

The selection only fed Send to Agents…. Killing, restarting or removing several agents meant one row at a time.

## Decision

- With **two or more rows selected**, the row menu of a selected row gains **Kill N Agents…**, **Restart N Exited** and **Remove N Exited**. N counts only the selected agents each action fits: Kill the running ones, Restart and Remove the exited or lost ones (`core::bulk`: `BulkTargets`, `BulkAction`).
- An action with no target stays in the menu **disabled, with the reason in its title** ("Restart Exited (none of the selected agents has exited)").
- The palette has **Kill Selected Agents…**, **Restart Selected Exited Agents** and **Remove Selected Exited Agents** (`KillSelected`, `RestartSelected`, `RemoveSelected`, unbound). It lists each only while the selection holds something it fits (`PaletteState::selected_running` / `selected_ended`), as it does for the other selection-dependent commands.
- **Kill asks once.** The kill sheet (`KillSheet::many`) asks "Kill 3 agents?" and lists the names (eight, then "and N more"). Its "Don't ask again" works as for one agent, and `[gui] confirm_kill = false` kills at once. **Restart and Remove do not ask**, as their single-agent forms do not. Remove sends one `ForgetAgents` with every id.
- The picks are checked again when they run: the targets are recomputed from the agent list then, so an agent that changed state since the menu opened is skipped.

## Consequences

- No protocol change. `KillSheet` keeps `agent_id` and gains `agent_ids` and `names`.
- Unlike Remove Exited Agents… (ADR 0086), Remove Selected does not keep an ended parent of a running agent: the user picked those rows.
