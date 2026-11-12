# 0003 — One binary, node and controller roles

**Status:** accepted · 2026-11-13

**Decision:** `fleetd` can run as:
- **node** — owns agent processes, PTYs, terminal grids, scrollback, worktrees, SQLite, adapter plugins.
- **controller** — aggregates nodes into one fleet view, runs fleet-level plugins, holds secrets, bootstraps nodes.

The Mac runs both roles in one process. Remote machines run node only. Nodes are autonomous: agents continue when the controller/GUI disconnects or the laptop sleeps.

**Consequences:** The local and remote paths share code. The node API is the single source of truth for agent state.
