# 0010 — Storage and on-disk layout

**Status:** accepted · 2026-11-13

**Decision:**
- Worktrees: `~/.fleet/worktrees/<repo>/<slug>`, branch `fleet/<slug>`.
- State: SQLite in WAL mode, one database per node (projects/workspaces/worktrees/tasks/events).
- Scrollback: on-disk ring-buffer file per agent.
- Archiving a worktree deletes it, but refuses if there are uncommitted or unpushed changes unless forced.
