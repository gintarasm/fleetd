# 0019 — Control-plane direction: controller anywhere, agent channel (MCP), pluggable secrets

**Status:** proposed · 2026-11-13 (not yet accepted; open questions below)

**Amends when accepted:** [0003](0003-node-controller-roles.md) (where the controller runs), [0012](0012-credentials.md) (secret storage, OAuth browser host). Builds on [0007](0007-plugin-system.md), [0008](0008-agent-status-detection.md), [0011](0011-remote-nodes.md).

## Context

The long-term direction is that the controller acts as a **control plane**: it orchestrates agents, receives data from agents and forwards it to others, and schedules and starts agents on the local node or on remote hosts.

The node/controller split ([0003](0003-node-controller-roles.md)) already has the right shape. Starting an agent anywhere is the controller sending `SpawnAgent` to a node, and nodes keep running when the controller goes away. Four gaps remain:

1. **Where the controller runs.** The plan places the controller on the Mac with secrets in the macOS Keychain. An orchestrator must stay up, and a laptop sleeps.
2. **No structured agent ↔ fleetd channel.** Agents are byte streams in PTYs. Forwarding information between agents by scraping terminal output is fragile. M1 also showed that raw output streams do not scale: under the 50-agent flood, 45 of 50 attached streams lagged ([0018](0018-m1-protocol-and-node.md)).
3. **No durable controller state.** Only nodes have SQLite. Schedules, queued messages and multi-step workflows would be lost on a restart.
4. **No scheduling input.** Nodes do not report capacity, so the controller cannot choose a host.

## Proposal

### 1. The controller runs on any host

- The controller role can run on any machine that can run fleetd: the Mac (the default, both roles in one process, as today) or an always-on Linux host.
- The GUI and CLI reach the controller over the existing protocol. Locally that is the unix socket; remotely it is `ssh host fleetd --stdio`, the same transport the controller uses for nodes ([0004](0004-ssh-transport.md)). Nothing in the GUI may assume the controller is local, from M2 onward.
- One active controller per fleet. There is no high availability; if the controller is down, nodes keep running agents and orchestration pauses.
- **Human-facing actions follow the human, not the controller.** Opening an OAuth URL or showing a notification goes to the connected GUI/CLI client. The reverse port-forward chain becomes node → controller → client host. With the controller on the Mac this collapses to today's [0012](0012-credentials.md) behaviour.

### 2. Agent channel: an MCP server per agent

fleetd exposes a Model Context Protocol (MCP) server to each agent it spawns. Claude Code, Codex and Gemini CLI already speak MCP, so agents can call fleet tools directly.

- **Transport:** the node serves MCP for its agents. The adapter ([0007](0007-plugin-system.md)) registers it in the agent's MCP config (for example Claude Code's `--mcp-config`, Codex's `config.toml`) as a stdio command: `fleetd mcp`. That command connects to the node's socket.
- **Identity:** at spawn each agent gets a random per-agent token (env `FLEET_AGENT_TOKEN`, never logged and masked like a secret). The node maps the token to the agent, so an agent cannot act as another agent. The token is revoked on exit.
- **Initial tools:**
  - `report_status`, which unifies [0008](0008-agent-status-detection.md) hook callbacks;
  - `send_message` and `read_messages` (an inbox);
  - `list_tasks` and `task_status`;
  - `spawn_task`, gated (see below);
  - `request_secret`, gated by grants (see §3).
- **Routing:** messages travel node → controller → node, and the controller stores them durably (§4). Delivery is at-least-once, and the recipient pulls via `read_messages`. Typing a message into the recipient's PTY is an adapter-specific, opt-in fallback only, because it races with the agent's own input.
- **Permissions:** tools are capabilities granted per task, like plugin capabilities (for example `fleet:message:<project>`, `fleet:spawn`). Spawning has quotas: maximum children, maximum depth, and maximum concurrent agents per node. These quotas stop runaway self-replication.
- **Fallback for agents without MCP:** `fleet msg …` / `fleet status …` CLI subcommands, authenticated by the same token.
- **Performance:** these are small structured events, never PTY bytes. They must not add work to the PTY output path. New benchmarks: message round-trip latency, and a message flood across 50 agents.

### 3. Credentials: a core broker plus secret-provider plugins

This splits [0012](0012-credentials.md) into a core part and a plugin part.

**Core (the credential broker):**
- grants (secret → node / project / workspace / task) and their enforcement;
- injection at spawn (env vars or tmpfs files, 0600) and deletion on exit;
- masking in scrollback; secrets never logged;
- `request_secret` over the agent channel (grant-checked and audited);
- the OAuth reverse-forward transport.

Delivery stays in one audited place, so no plugin can weaken it.

**New plugin kind, `secret-provider`:** a provider stores and fetches secret values.
- Examples: macOS Keychain, 1Password (`op`), HashiCorp Vault, Linux Secret Service, `pass`, an age-encrypted file.
- The manifest capability is `secrets:provide`. It is approved separately at install, with a stronger warning, because a provider sees every value it serves.
- A provider never sees other providers' secrets. Values go provider → broker → node → agent and never pass through other plugins.
- Most providers need the OS. Sandboxed WASM cannot reach the Keychain, so they use the subprocess escape hatch ([0007](0007-plugin-system.md)) or host functions supplied by the broker.

**Built-in default provider:** Keychain on macOS; Secret Service, or an encrypted file, on Linux. Credentials therefore work with no plugins installed, and a controller on Linux works out of the box.

**Login recipes as plugins:** per-service OAuth steps (which URL to open, which callback port to forward) can live in adapter or provider plugins. The forwarding mechanism stays in the core.

### 4. Durable controller state

- The controller gets its own SQLite (WAL) database, separate from the node database ([0010](0010-storage-and-layout.md)). It holds the message queue, schedules, workflow state, the node registry and grants.
- A restart resumes from this database. Messages and spawns that are in flight are idempotent, keyed by ID.

### 5. Node capacity and placement

- A new node RPC, `NodeInfo`, returns: CPUs, load, free memory, running agents, workspaces present (with repo identity), installed adapters and fleetd version.
- Placement starts as a built-in policy: an explicit node, else the node that already has the workspace, else the least-loaded node. A `scheduler` plugin kind can come later if a policy needs it.

## Consequences

- **M2 (GUI):** connects to "a controller" through the client protocol and never assumes it is in-process. This is a small cost now and avoids a rewrite later.
- **M4 (plugins, adapters, status):** adds the MCP agent channel, with `report_status` as its first tool. The hook-based status of [0008](0008-agent-status-detection.md) then goes through the same path.
- **M5 (remote nodes):** adds `NodeInfo`, and the client → controller-over-SSH path.
- **M6 (credentials):** builds the broker and the `secret-provider` kind, with the built-in provider. The Keychain is no longer hard-wired.
- **After M6:** coordination features ("Later" in the plan: coordinator/sub-agents, scheduled automations) build on §2 and §4 without core changes.
- The performance budgets are unchanged. The agent channel and controller store need their own benchmarks before they ship.

## Open questions (to settle before accepting)

1. **Durability vs. latency:** should messages be stored before acknowledging (at-least-once, a disk write per message) or kept in memory with best effort?
2. **Pull vs. push:** is pull-only delivery enough? Or should the adapter interrupt or notify an idle agent when a message arrives (for example by resuming it with a prompt)?
3. **Spawn quotas:** what default limits apply, and who can raise them (the user only, or a coordinator task)?
4. **Secrets in a sandbox:** do secret providers have to be subprocess plugins, or does the broker expose a narrow host API so WASM providers can work?
5. **Two controllers:** if a second controller connects to the same node (for example the Mac's and a Linux box's), does the node reject it, or does it accept both with one designated as owner?
6. **Remote controller:** when the controller runs remotely, does the Mac still need a local fleetd at all (for local agents), or is the Mac then just a client?
