# 0058 — Ended pane summary and the agent exit time

**Status:** accepted · 2026-11-16 (issue #45)

Amends [0010](0010-storage-and-layout.md) (store schema v5) and [0022](0022-protocol-v2.md) (one new field). Builds on [0030](0030-agent-restart-and-forget.md) (Restart and Remove).

## Context

An agent that exits leaves its final screen in the pane, with a small "exited (n)" banner and a Restart button in the corner. The owner wants a summary at the bottom of the pane: how the agent ended, how long it ran, and Restart, Copy last output and Remove. The run time needs the exit time, which the node did not keep: `AgentInfo` had only `created_unix_ms`. A time taken by the GUI when it sees `AgentExited` is lost for an agent that exited while no GUI was connected, the usual case for a long run.

## Decision

### The node records when an agent exited

- Store schema **v5** (v4 is the priority column, ADR 0066): `agents.exited_unix_ms INTEGER` (nullable), added with `ALTER TABLE` to a v4 database; a new database is created at v5 directly. `set_exited` writes it with the exit code. It stays `NULL` while running, for a lost agent (when it died is unknown), and for rows that exited before v5.
- Protocol: `AgentInfo.exited_unix_ms = 9` (`optional int64`). Additive: `PROTOCOL_VERSION` stays 2, and old clients skip the field.

### The GUI's summary bar

- A pane whose agent exited or is lost draws a bar under its grid (`app/ended_view.rs`; logic in `core/ended.rs`, toolkit-free per [0029](0029-gui-core-toolkit-free.md)). It replaces the corner banner for those states; "attaching…" and "disconnected" stay banners.
- Text: "Exited · code N" in the success colour for 0 and the danger colour otherwise, or "The agent is gone" (danger) for a lost agent; then "ran 3m 12s" (two largest units) when the run time is known.
- **Run time** = `exited_unix_ms − created_unix_ms` from the list. When the GUI sees `AgentExited` it fills in its own clock until the next list replaces the row, so an older node still shows a run time for exits the GUI saw. No run time for a lost agent, a missing exit time, or an exit before the start (clock skew).
- **Restart** is ⌘R ([0030](0030-agent-restart-and-forget.md)). **Remove** forgets the agent (`ForgetAgents`), no confirmation, like the row menu's Remove; the pane empties when the node announces `AgentRemoved`.
- **Copy last output** copies the last screenful: the grid's rows with trailing spaces and the blank rows above and below the output dropped, whatever the view is scrolled to. "Since the last prompt" would need prompt marks (OSC 133), which `fleet-term` does not track; that is a follow-up.
- The scrollback above the bar stays scrollable and selectable: the bar is a sibling under the grid, not an overlay. It has no motion and only opaque theme fills, so Reduce Motion and Reduce Transparency need nothing more.

## Consequences

- Exits recorded before v5, and on a node that predates it, show no run time unless the GUI saw them happen.
- The store migration is one `ALTER TABLE`; a v5 database cannot be opened by an older fleetd (it refuses newer schemas), as with every schema bump.
