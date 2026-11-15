# 0034 — Agent communication through the CLI: wait, exit reports, mailbox, leases, broadcast, questions

**Status:** accepted · 2026-11-15 (user decisions, after the first multi-agent session; the detailed design is written when M3 starts)

**Amends:**
- [0033](0033-satellite-milestone.md): **the agent channel is the `fleet` CLI, not an MCP server.** `fleetd mcp`, its tools and `fleet mcp install` leave M3; each tool becomes a `fleet` command with the same scope rules. M3 also gains the commands below, and agent-to-agent messaging moves from M5 into M3. macOS notifications stay in M5.
- [0019](0019-control-plane-direction.md): the agent channel gains a mailbox, leases and events for the parent, and is reached through the CLI.

## Context

On 2026-11-15 one coordinating Claude Code agent spawned five agents through the `fleet` skill, each fixing a GitHub issue in its own worktree. The only channel between them was `fleet send`, which types keystrokes into the other agent's terminal. The session ran into these problems:

1. **Keystrokes are fragile.** `fleet send --enter` wrote the text and its Return in one write; Claude Code read it as a paste and left five messages typed but unsent. Fixed by sending the Return as a separate write, but the channel stays keystrokes: a message lands in whatever the agent is doing.
2. **No trusted sender.** A report arrives as if the user had typed it. The receiver can only treat it as untrusted text and cannot tell an agent's report from the user's instruction.
3. **The parent has to poll.** The coordinator watched screens with a polling script, then asked children to report back with `fleet send`. The report only arrives if the child remembers to send it.
4. **A dead child is silent.** A child that crashes, hangs, or waits on a permission prompt cannot report.
5. **No shared resources.** The children had to take turns running the wall benchmark (0014). A `mkdir` lock serialised the runs but left a stale lock when its holder was stopped, and it could not keep the other children's builds off the machine (load average ~210 made a wall run report a false `gui_cold_start_ns` regression).
6. **No broadcast.** The same rule was sent five times, one agent at a time.
7. **Questions to the user get stuck.** A child that needed the user's approval asked in its own terminal; the coordinator relayed it.

0033 planned an MCP server (`fleetd mcp`) as the agent channel. But every coding agent fleetd runs has a shell, and the `fleet` CLI already reaches fleetd, sends the agent's token (0033) and prints `--json`. An MCP server adds no capability or security over that: an agent can always run `fleet` from its shell (0025 makes the same argument for node primitives). It does add a process per agent (against the idle-memory budgets), an install step per agent program, and a second interface to keep in step with the CLI.

## Decision

### The channel is the CLI

- Agents talk to fleetd with **`fleet` commands**, authenticated by `FLEET_AGENT_TOKEN` (0033). With a token, fleetd applies the agent's scope; without one, the caller is the user.
- Every agent-facing command has **`--json`** output and **documented exit codes**, so agents parse results instead of reading prose.
- The **agent skill** (`fleet skill install`) is how agents learn the commands; it works for Claude Code and omp alike (issue #20).
- **0033's tools map to commands:** `spawn_agent` → `fleet spawn` (labels may only narrow the parent's, plus `--name`, issue #48), `list_agents` / `agent_status` → `fleet ls` / `fleet status <id>`, `read_screen` / `read_history` → `fleet screen` / `fleet history`, `send_input` → `fleet send`, `kill_agent` → `fleet kill`, `report_status` → `fleet report`. With a token, all of them act only on the caller's descendants (0033), except where noted below.
- **MCP is deferred,** not rejected: if an agent can't run `fleet` (a sandbox that blocks fleetd's socket, or a tool-only agent with no shell), `fleetd mcp` may come back later as a thin wrapper over the same `fleet-client` library, with the same commands, scopes and results. It is not part of M3.

### 1. Waiting for children: `fleet wait`

- `fleet wait [<id|name>…] [--state waiting,done,error,exited,lost,message] [--timeout 10m] [--json]`: blocks until any listed agent (default: all of the caller's descendants) reaches one of the states (default: all of them). It prints the agents that matched with their status and message; exit 0 on a match, 1 on timeout (printing the current statuses), 2 on error.
- It replaces polling. An agent runs it in the background where its client allows (Claude Code's background shell wakes the agent when the command exits) or in the foreground with a timeout shorter than its shell's limit, then calls it again.

### 2. Automatic exit reports

- When a child exits or is lost, fleetd records a status event for the parent, even if the child never ran `fleet report`: `exited` with the exit code, or `lost`.
- An inferred status (from output activity, 0033's hollow dot) is part of the event too, so a child quiet for a long time at a prompt can be reported as `waiting (inferred)`.
- These events are what `fleet wait` returns and what lands in the parent's mailbox (3); nothing is typed into the parent's terminal.

### 3. A mailbox instead of keystrokes

- Every agent has a **mailbox** in fleetd. `fleet message <id|name> "text"` puts a message in it, with the verified sender's id and name (`name` label, issue #48) and a timestamp. Text is at most 4 KiB.
- **Allowed recipients** for an agent: its own descendants, its parent, and the user. Not siblings or the user's other agents.
- `fleet inbox [--since <cursor>] [--json]` reads new messages; `fleet wait --state message` returns when one arrives. Status events from (2) arrive in the parent's mailbox too.
- Messages are **data, not instructions**: the skill and `fleet inbox` output say that a message from another agent is a report or a request to weigh, never the user's command.
- **Keystrokes stay** (`fleet send`, with its separate Return) for nudging an agent that doesn't read its inbox. They are the fallback, not the main channel.
- The graph view (0033) draws message lines between agents from the mailbox.

### 4. Leases for shared resources

- `fleet lease <name> [--max-load N] [--ttl 1h] [--wait] -- <command>` holds the lease while the command runs; `fleet lease acquire` / `fleet lease release` for longer holds.
- A lease is held by an agent (or the user), not a file: fleetd **releases it automatically when the holder exits or is lost**, and at `ttl` if one is set. A stale lock like 2026-11-15's cannot happen.
- **Quiet-machine condition:** `--max-load N` grants the lease only while the 1-minute load average is below N, for benchmarks. fleetd never pauses other agents to get there.
- **Scope:** lease names are per node, and any caller may use any name. Leases grant nothing (no credentials, no quota), so they need no label rules.
- The wall benchmark runs under a lease named `perf-wall`, and the `perf-wall` CI job takes it too, so local runs and CI never overlap.

### 5. Broadcast

- `fleet message --children "…"`, `--descendants`, or `--label k=v` (over the caller's descendants) sends one message to many agents.
- The user, without a token, can broadcast to any agents, from the CLI and from the GUI.

### 6. Questions to the user, answered in the GUI

- `fleet report waiting "message" --question "…" [--choice "…"]…` carries a **question** with optional choices (for example "Approve the regression" / "Re-run on a quiet machine").
- The GUI shows it on the agent's card, sidebar row and pane (the "needs you" state, issue #32), with the choices as buttons and a free-text answer. The CLI has `fleet answer <id|name> "…"`.
- The answer goes to the asking agent's mailbox as a message from the user, the only message an agent can treat as the user's own words. Its parent gets a copy, so a coordinator knows the question was answered.
- Perf-regression approval (CLAUDE.md, 0014) still needs the user's explicit decision; an answer through this channel is that decision only when the user gives it.

## Consequences

- **M3 changes shape:** no MCP server or `fleet mcp install`; instead the `fleet` commands above, all token-scoped. Messaging, leases and questions move from M5 into M3. The rest of M5 (adapters, hook-based status, macOS notifications) stays.
- **The agent skill changes** when M3 lands: coordination is `fleet wait`, `fleet message` / `fleet inbox`, `fleet report` and `fleet lease`; `fleet send` is described as the fallback.
- **Sandboxed agents** need access to fleetd's socket (`~/.fleet/fleetd.sock`) and the `fleet` binary; the skill's install notes say how to allow them in the agent's sandbox settings. If that proves impossible for some client, it is the trigger for the deferred MCP wrapper.
- **Storage:** mailboxes and leases live in fleetd's state and survive a daemon restart (0032). A mailbox is removed with its agent (`fleet rm`).
- **Perf (0014):** no extra process per agent. Mailbox and event delivery stay off the PTY output path, as 0019 requires. A GUI benchmark covers drawing message lines with the 50-agent graph.
- **Open for the milestone's design:**
  - **Token boundary.** An agent runs as the same OS user as the user and reaches the same socket, so it could run `fleet` without its token and act as the user. The scope rules hold only if fleetd can tell an agent's process from the user's (for example by the peer's process ancestry on the socket, or a user credential kept out of agents' reach). This applies to the CLI and MCP alike; 0033's scopes depend on the answer.
  - the exact inferred-`waiting` heuristic, shared with issue #32;
  - mailbox retention limits;
  - whether `--max-load` leases should also delay *new spawns* on a busy node.
