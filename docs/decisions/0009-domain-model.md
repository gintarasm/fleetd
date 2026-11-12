# 0009 — Domain model: project → workspace → worktree → task

**Status:** accepted · 2026-11-13

**Decision:**
- **Project** groups workspaces. Its workspaces can be on different nodes.
- **Workspace** = a directory on a node (node + path). Any directory works.
- If the workspace is a git repo, **worktrees** can be created from it (on the same node). Tasks in git workspaces default to a new worktree; attaching a task to the main checkout is an explicit opt-in.
- If the workspace is not a git repo, tasks attach directly to it.
- **Task** = one agent session (prompt, agent type). Several tasks may share a worktree/workspace (e.g. coder + reviewer + test runner).
- Concurrent writers: no locking; the UI warns when more than one writing task is active in the same worktree.
- Comparing agents = several worktrees in one workspace, one task each.
- Diff view (read-only, v1): available for git worktrees/checkouts. Non-git workspaces show "not a git repo"; snapshot-based diffs later.

**Consequences:** The schema supports many tasks per worktree from day one — no migration later.
