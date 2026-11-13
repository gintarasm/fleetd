# 0022 — Protocol v2: history, spawn-attach, lag and activity events, primary client, labels

**Status:** accepted · 2026-11-13 (M2a)

**Amends:** [0018](0018-m1-protocol-and-node.md). Framing, the handshake, request ordering, terminal sizes, snapshots, exited-agent release and the scrollback ring are unchanged. Implements the label part of [0025](0025-core-knows-agents-workflows-own-hierarchy.md) and serves the history of [0021](0021-agent-history.md).

## Context

The M2 client needs history, a way to spawn an agent with the GUI's terminal answering its startup queries, per-agent lag notices, a cheap sidebar feed (last line per agent), a rule for who sizes an agent's terminal when several clients attach, and labels to group by. All are additions to `fleet.v1`; the removal of the id-0 lag notice is not, so the version is bumped.

## Decision

### Version

`PROTOCOL_VERSION = 2`. A v1 client gets `HandshakeReject` ("unsupported protocol version 1 (server speaks 2)"). `fleet-cli` moved to v2 in the same release.

### New and changed messages

- **`ReadHistory { agent_id, generation, before_line, max_lines }` → `HistoryPage { agent_id, first_line, lines }`.** Returns the lines just before `before_line`, oldest first (`lines[0]` is line `first_line`), at most 4096 lines (larger requests are capped) and 256 KiB. A `generation` other than the current one is answered with `Error "history cleared"`; the client then re-attaches and starts again. An unknown agent is an `Error`.
- **`AttachOk` fields 5–9:** `history_tail` (the in-memory tail, ANSI lines, oldest first; contiguous with the snapshot), `tail_first_line` (line number of `history_tail[0]`), `history_first_line` (oldest line still on disk), `history_generation`, and `primary` (whether this client's resizes apply; true for a live agent, false after exit). Tail, snapshot and subscription are taken under the pipeline lock, so tail, screen and stream join without a gap.
- **`SpawnAgent.attach = 7` → `Response::SpawnedAttached { agent, attached }`.** The subscription is created before the process starts, so the agent's first output and its startup terminal queries (DA, kitty `CSI ? u`, …) go to the attaching client, whose terminal answers them, and its exit is delivered once, after its output. The spawner is primary.
- **`Event::Lagged { agent_id }`** replaces the id-0 notice `Response { id: 0, Error "lagged: re-attach" }`, which is removed: the client now knows which subscription was dropped and can re-attach to that agent alone. `Response.id = 0` is no longer used.
- **`Event::AgentActivity { agent_id, last_line, last_output_unix_ms }`:** the agent's last non-blank screen row (at most 200 characters) and the time of its last output. Pushed to every connection.
  - At most one per agent per 250 ms (`ACTIVITY_TICK`). The output path only marks the agent dirty; a ticker thread, started lazily by the first output and parked while nothing is dirty, reads the grid. An idle daemon has no ticker thread.
  - Delivered **latest-value per connection**: a slow connection keeps only the newest pending value per agent, so it never loses the current state, only superseded ones. No activity follows an agent's `AgentExited`.
- **`Event::AttachRole { agent_id, primary }`:** sent in the subscription's own stream (ordered with its output) when its role changes.
- **Labels ([0025](0025-core-knows-agents-workflows-own-hierarchy.md)):** `SpawnAgent.labels = 8`, `AgentInfo.labels = 8`, `Request::SetLabels { agent_id, set, remove }` (applies `set`, then `remove`, atomically; answered by `Ack`), and `ListAgents.selector = 1` (agents whose labels contain every selector pair; empty means all).
  - Limits: at most 64 labels, keys 1–64 bytes, values up to 256 bytes. Keys starting with `fleet.` are reserved for the core and rejected in `set`, `remove` and at spawn. A rejected change changes nothing.
  - Exited and lost agents keep editable labels; an unknown agent is an `Error`.
  - Store: `SCHEMA_VERSION = 2` adds the `agent_labels` table; a v1 database migrates in place on open.

### Primary client

When several connections attach to one agent (GUI plus `fleet attach`, or two GUIs):

- The **primary** is the connection that most recently attached to the agent or sent it input. Input from a connection that is not attached changes no role.
- Only the primary's `Resize` applies. A resize from a non-primary is acknowledged and ignored (size validation errors are still returned); while no connection is attached, any resize applies.
- When the primary's subscription ends (detach, disconnect, lag), the remaining subscriber that most recently attached or sent input is promoted and told so.
- A connection holds one subscription per agent; re-attaching replaces it. Attaching makes the connection primary, so a primary that re-attaches keeps its role without a demote/promote flap.
- Terminal query replies are unchanged from 0018: the node's grid answers only while no client is attached; otherwise the attached clients' terminals answer.

### Agent environment

The node sets `FLEET_AGENT_ID=<id>` in every agent's environment, **last**: after the base environment ([0024](0024-agent-skill-stdio-env.md)), `SpawnAgent.env` and the `TERM` default. A `FLEET_AGENT_ID` in `SpawnAgent.env` is overridden. This deviates on purpose from the M2a plan's precedence list, which put `SpawnAgent.env` above it: an agent spawning another must not be able to give it a false identity (0025, no impersonation).

## Consequences

- Clients can scroll a whole session (tail at attach, older pages on demand) and re-attach to one lagged agent.
- A GUI can show a sidebar of 50 agents from activity events without attaching to any.
- Two clients on one agent no longer fight over its size.
- Known limits: a resize from a just-demoted primary can race a role change and still apply; a stale `Lagged` can arrive before the `AttachOk` of a re-attach; `SetLabels` re-checks the merged set against the reserved prefix, which must change once the core writes `fleet.*` keys itself.
