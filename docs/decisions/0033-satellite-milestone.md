# 0033 — Satellite: agents spawning agents, and a graph view (new M3)

**Status:** accepted · 2026-11-15 (user decisions from a grilling session; the detailed design is written when the milestone starts)

**Amends:**
- [0019](0019-control-plane-direction.md): part of the agent channel moves from M4 to the new M3. That part is the per-agent token, `fleetd mcp`, spawn and inspection tools, `report_status` and quotas.
- [0015](0015-scope-positioning-roadmap.md): "coordinator/sub-agents" moves out of *Later*, but only as spawning and seeing the tree, not a coordinator mode.

**Milestone renumbering:** the new milestone becomes M3, and the later ones each move down one: the old M3 (worktrees and diff) becomes M4, M4 becomes M5, M5 becomes M6, and M6 becomes M7. Older ADRs keep their original numbers; read them through this mapping.

## Context

Agents can already start other agents with `fleet spawn` (through the agent skill), but nothing records who started whom. The user wants agents to create agents, and a "satellite" view of everything running and how it relates.

## Decision

### Purpose

The view is for these jobs:
1. **Glance** at what is running and its state.
2. **Navigate:** click an agent to reach its terminal.
3. **Control whole branches:** stop a subtree, or remove one that has ended.

Because ended agents stay visible, the view also shows after the fact what spawned what.

### Relationships

- **Lines:** the backbone is the *spawned-by* tree.
- **Grouping:** machines and projects are drawn as boxes, not as lines.
- **Later:** message lines arrive with messaging.
- **Left out:** "waits-on" dependencies.

### Identity and parents

- **Tokens:** every agent gets a per-agent token (0019's `FLEET_AGENT_TOKEN`), including agents the user starts. Those are roots, at depth 0.
- **Parent label:** fleetd sets `fleet.parent` itself from the authenticated caller, so a parent can't be forged.
- **Parent id format:** a local agent id for now, written so it can become node-qualified (`node/id`) in M6 (remote nodes) without a migration. Cross-machine spawning stays in M6.
- **The CLI path:** the `fleet` CLI sends `FLEET_AGENT_TOKEN` when set. Spawns through the CLI or the skill, for example from omp agents without MCP, land in the same tree under the same quotas.

### The MCP channel (the slice of 0019 pulled forward)

- **Server and setup:** `fleetd mcp` is a stdio MCP server. Each agent CLI gets it through a one-time `fleet mcp install --agent claude|omp`. Automatic injection into known CLIs waits for the M5 adapters.
- **Tools:**
  - `spawn_agent {program, args, cwd, labels}`: labels may only narrow the parent's (0025). There is no separate prompt field; args and `send_input` cover it.
  - `list_agents`, `agent_status`, `read_screen`, `read_history`, `send_input` and `kill_agent` act only on the caller's own descendants, never on siblings or the user's other agents.
  - `report_status`: one of `working`, `waiting` (needs the user), `done` or `error`, plus a message of at most 200 characters.
- **Approval:** no approval by default; new agents are briefly highlighted. `spawn_approval = true` in config asks the user first.
- **Quotas** live in `~/.fleet/config.toml`, which agents can't read or raise:
  - max depth 3;
  - 8 children per agent;
  - 30 concurrent agents per node.

  A refused spawn shows in the graph.
- **When a parent ends:** its children keep running and are shown under the greyed-out parent. Stopping them is a user action on the subtree.

### The view

- **Opening it:** a full-window mode of the main window, toggled with ⌘⇧G or a titlebar button. Clicking an agent returns to the panes with that agent focused. A pop-out window may come later.
- **Layout:** a tidy, stable tree, roots on the left, with boxes per machine and project. It pans, zooms and collapses subtrees, and positions don't reshuffle when agents come and go.
- **Each agent's card:**
  - the program and the project or folder;
  - a status dot: solid when reported through `report_status`, hollow when inferred from output activity;
  - the last activity line;
  - how long it has run;
  - a child count when collapsed.

  A live hover preview is a stretch goal, subject to the perf gates.
- **Waiting agents:** an agent in `waiting` gets a highlight and a dock badge. macOS notifications stay in M5.
- **Ended agents** stay greyed out until removed. "Clear exited" also removes ended branches, meaning an exited parent with no running descendants.
- **Subtree actions:** Stop subtree (SIGTERM to the agent and its descendants) and Remove subtree (when everything in it has ended). Restart stays per agent; restarting a subtree is left out.
- **Keyboard first:** arrows move, ⏎ opens, ⌫ stops a subtree after a confirmation, and `/` filters by name, status or label.
- **Sidebar:** children are indented under their parent, collapsed by default when an agent has more than 3 children.
- **`fleet tree`** prints the same tree with status in a terminal.

### Performance (0014)

New GUI benchmarks, gated like the others:
- the graph redraws 50 agents within the 16 ms frame budget, under the 50-agent flood;
- opening the view takes under 100 ms.

MCP events stay off the PTY output path, as 0019 requires.

## Consequences

- **Roadmap:** the old M3 (worktrees and diff) waits one milestone. The new M3 needs only a thin slice of the old M4; messaging, adapters, hook-based status and notifications stay in M5.
- **Labels:** `fleet.parent` becomes the first reserved label the core writes.
- **Open for the milestone's design:**
  - the MCP transport details;
  - how the token reaches agents started before the upgrade (they have none; they show as roots);
  - the graph's rendering approach in gpui.
