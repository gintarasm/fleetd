# 0025 — The core knows nodes and agents; workflows own the hierarchy

**Status:** accepted · 2026-11-13 (user decisions, from a design review of 0009)

**Supersedes:** [0009](0009-domain-model.md).

**Amended by:** [0052](0052-agent-names.md) — the `name` label is grant-safe: an agent's spawner (another agent included) may set it, and a child never inherits it.

**Amends:**
- [0007](0007-plugin-system.md): new plugin kind `workflow`;
- [0010](0010-storage-and-layout.md): worktree layout and archive cleanup move to the git-worktree workflow;
- [0011](0011-remote-nodes.md): workspace clone and fetch-back become workflow actions;
- [0012](0012-credentials.md) and [0019](0019-control-plane-direction.md): grant and quota scopes become label selectors; placement and `NodeInfo` lose workspaces; the MCP tools are renamed.

## Context

0009 made project → workspace → worktree → task the domain model of the daemon. These are not core concepts, though. They are *workflows* for coordinating agents, and different people want different ones: a worktree per task, a shared checkout, non-git directories, multi-repo projects.

The user expects several interfaces: the CLI, the native app, an iOS app and maybe a webview. They should all share the same ways of doing things, such as creating a worktree for a new agent, without each one re-implementing them.

This follows 0015's positioning: a small, opinionated core done well, with everything else at the edges.

## Decision

### Core: nodes and agents

The core vocabulary of fleetd (node and controller) is **node** and **agent**. An agent is a process on a node, with a cwd, an env, a terminal and its history. The core has no project, workspace, worktree or task entity, and no git knowledge.

### Labels

**Shape.** Each agent carries opaque string key/value **labels** (for example `project=fleetd`, `git.worktree=…`, `role=reviewer`).
- They are set at spawn, editable with `SetLabels`, and filterable in `ListAgents` and in events.
- The core stores and matches them but never interprets them.

**Reserved keys.** Keys starting with `fleet.` are reserved for the core, for example `fleet.parent` on an agent spawned by another agent.

**Limits.** At most 64 labels per agent, keys up to 64 bytes, values up to 256 bytes.

**Who can write labels.** Only user-authenticated clients and workflow plugins can set or change labels. Agents can't, even through the MCP channel. An agent spawned by another agent inherits its parent's labels and may only narrow them, never widen or rewrite them. This matters because labels select grants (below), so a label an agent could write would let it relabel itself into a grant.

A project is just a label. Interfaces group by labels; for example, the GUI sidebar groups by `project` and falls back to the cwd.

### Generic node primitives

**The primitives:**
- `Exec` runs a program in a directory on a node, with argv, env and a timeout, and returns stdout, stderr and the exit code. Output is capped at 16 MiB, and the timeout defaults to 10 minutes.
- `ReadFile` reads a file.
- `ListDir` lists a directory.

**Who can call them:**
- workflow plugins, through capabilities approved at install (`exec:<bin>`, `fs:read:<path>`);
- user-authenticated clients.
- Agents never can; they already have a shell.

### Workflows

**A new plugin kind, `workflow`**, runs on the controller. It exposes named **actions** with parameter schemas, for example `git-worktree.new-agent {repo, branch, program}`.

**Calling an action.** Any interface calls `RunAction {name, params}` over the protocol. Thin clients, such as iOS or a webview, render parameter forms from the schema (the declarative UI from 0007) and so get every workflow with no workflow code of their own.

**What a workflow can use:**
- node primitives, spawning and labels;
- its own storage (`storage:own`) for internal state;
- subscriptions to agent events (exits, label changes), filtered by its labels.

Facts that other interfaces should see go into labels.

**The built-in `git-worktree` workflow** replaces 0009–0011's core git behaviour:
- a new agent in a new worktree (layout `~/.fleet/worktrees/<repo>/<slug>`, branch `fleet/<slug>`, as in 0010);
- the read-only **diff**, returned as a generic diff view that clients render;
- **archive**, refused when dirty or unpushed unless forced;
- **fetch-back** of a branch from a remote node;
- workspace **clone** on a remote node.

Several agents in one worktree, and the warning about concurrent writers, become this workflow's business.

### Grants, quotas, placement, MCP tools

- **Grants and quotas.** Secret grants (0012/0019) and spawn-quota overrides (0019) are scoped by **label selectors** (for example `project=fleetd,role=coder`) and node, instead of by project, workspace or task.
- **Placement** is an explicit node, else an affinity hint supplied by the caller (usually a workflow that knows where a repo lives), else the least-loaded node.
- **`NodeInfo`** no longer reports workspaces.
- **The agent-channel MCP tools** (0019) are `list_agents`, `agent_status` and `spawn_agent` (gated), filtered by label, plus `report_status`, the messaging tools and `request_secret`, unchanged.

## Consequences

- **M2a:** protocol v2 gains:
  - `labels` on `SpawnAgent` and `AgentInfo`;
  - a `SetLabels` request;
  - a label filter on `ListAgents`.

  Adding them now avoids another protocol bump. In M2a labels are unauthenticated: any client of the daemon's socket, an agent running `fleet` included, can `SetLabels`. Until the M4 agent channel enforces "who can write labels" above, labels must not gate anything.
- **M2b:** the sidebar groups by the `project` label, falling back to the cwd.
- **M3** becomes:
  - labels in the store;
  - the node primitives;
  - the `RunAction` interface with action schemas;
  - git-worktree as a **native built-in** workflow behind that interface;
  - the diff view.
- **M4:** the WASM plugin host also runs third-party workflows through the same interface.
- The core has no git dependency. A non-git workflow is just another workflow.
- 0009's "concurrent writers" warning, worktree defaults and diff rules now live in the git-worktree workflow. The core neither forbids nor knows about any of them.
