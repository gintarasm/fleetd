# 0090 — M4 "Workflows": node primitives, `RunAction`, the git-worktree workflow and the diff view

**Status:** proposed · 2026-11-23 (overseer's design, for the user's review; the detailed design and work breakdown are in [the M4 spec](../superpowers/specs/2026-11-23-m4-workflows-design.md))

**Builds on:** [0025](0025-core-knows-agents-workflows-own-hierarchy.md) (core knows nodes and agents; workflows own the hierarchy), [0007](0007-plugin-system.md) (plugins, declarative UI), [0010](0010-storage-and-layout.md) (worktree layout, archive rule), [0033](0033-satellite-milestone.md) / [0072](0072-agent-identity-by-session.md) (agents spawn agents; scope), [0085](0085-gui-git-branch.md) (`AgentInfo.git`).

**Amends:**
- [0025](0025-core-knows-agents-workflows-own-hierarchy.md): `Exec`'s output cap is 15 MiB (stdout and stderr together), so a reply fits `MAX_FRAME` (16 MiB); a workflow may override inherited labels in the prefix it owns when it spawns for an agent; agents may call actions marked for them.
- [0010](0010-storage-and-layout.md): the worktree root and branch prefix are configurable per repository (0010's layout stays the default unless the user decides otherwise, spec open question 1).

**Reserves:** ADRs 0091 (node primitives), 0092 (workflow host and `RunAction`), 0093 (git-worktree workflow), 0094 (GUI diff view and actions), 0095 (skill, if needed), 0096–0099 spare. Protocol field blocks 90–114 and `SCHEMA_VERSION` 13–14, per workstream, in the spec.

## Context

M4 is the milestone 0025 described (renumbered by 0033): node primitives, the `RunAction` interface, git-worktree as a native built-in workflow, and the diff view. Two things changed since 0025 was written:

1. **Agents spawn agents (M3).** Every spawn from an agent gets `fleet.parent`, narrowed labels, quotas and optional approval, and an agent sees only its descendants (0072). 0025 did not say whether agents may run workflow actions, or how a workflow's labels fit the narrowing rule.
2. **The fleet already runs on worktrees by hand.** The overseer starts each worker in `../fleetd-wt/<slug>` on its own branch, reviews with `git diff`, and deletes merged worktrees (whose `target/` dirs are 8–18 GB each and once filled the disk) with a script that checks they are clean and merged. Every step is shell work an agent re-derives each session.

## Decision

### Shape

- **Node primitives** `Exec`, `ReadFile`, `ListDir` are node requests for the user and for workflows; agents are refused (0025). They run off the PTY path with caps, a timeout and at most 8 concurrent Execs per node.
- **Workflows are a Rust trait in a new `fleet-workflow` crate**, hosted by `fleetd` behind a `Host` trait shaped like the M5 `workflow` WIT world's imports. M4 has native built-ins only; M5's WASM host is another implementation of the same trait. Built-ins declare and are held to capabilities (`exec:git`, `labels:git.`, …) like plugins will be.
- **`ListActions` / `RunAction`** with fleetd's own small parameter schema (kinds such as `string`, `bool`, `choice`, `path`, `agent`, `argv`), rendered as forms by every client. Results are `done`, `spawned`, a `view` (diff, table, text: the start of 0007's declarative views), or an overridable `refused` with the params to resend.
- **git-worktree** is the first workflow: `new-agent`, `diff`, `archive`, `ls`, `prune`, all through `git` via `Exec`. The core still has no git knowledge. `fleet wt` is its CLI. Clone and fetch-back stay M6.
- **The GUI** gets a read-only diff pane, a "New worktree" option in the New agent sheet, archive in context menus, and a generic "Run Action…" form built from the schema.

### Workflows and agent scope

- **An action runs as its caller.** A spawn by an action is the caller's spawn: an agent's becomes its child, under its quotas and spawn approval. Nothing an action does lets an agent reach outside its subtree.
- **The host enforces scope, not the workflow:** an action is marked user-only or callable by agents; a param can be user-only (e.g. `force`); an `agent` param must name one of the caller's descendants.
- **Agents may** start children in new worktrees (`new-agent`), read diffs and list worktrees, and archive a descendant's worktree without `force`. **Only the user may** `prune`, force an archive, or call the node primitives.
- **A workflow owns a label prefix** (`git.` for git-worktree: `git.repo`, `git.worktree`, `git.base`). Only that workflow and the user write it; agents still can't write labels. When the workflow spawns for an agent, its own keys replace the inherited ones; that is the only exception to M3's narrowing, and it is safe because the values come from the workflow, not the agent. The core stores the prefix rule without interpreting the keys.
- **Side effects roll back on refusal:** the workflow dry-runs the quota check before touching disk, and removes a worktree it just made if the spawn is then refused or denied.

### State

- Workflow state (`storage:own`) is a per-workflow key–value table in the node store (schema 13) until the controller store (0019 §4) exists. git-worktree records the worktrees it made there, so `prune` never touches hand-made ones.

## Consequences

- The overseer's loop becomes `fleet wt new … -- claude "…"`, `fleet wait`, `fleet wt diff --stat`, `fleet wt prune`, and the user sees the same worktrees, diffs and archive buttons in the GUI.
- M5's plugin host has a fixed target: the `Workflow` and `Host` traits and the action/view protocol. Changing them after M4 is a protocol and WIT change.
- M6 adds clone and fetch-back as git-worktree actions, and action progress events in the reserved block; the primitives already are node requests, so a controller can call them on a remote node unchanged.
- New benchmarks (no baselines until the user records them): `exec_true_roundtrip_ns`, `diff_view_build_10k_lines_instructions`, `gui_diff_open_ns`, `gui_diff_scroll_frame_ns`. Existing gates should not move: nothing runs on the PTY path or at idle.
- Open decisions for the user are listed at the end of the spec and in the PR.
