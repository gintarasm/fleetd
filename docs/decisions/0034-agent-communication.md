# 0034 — Agent communication: wait, exit reports, mailbox, leases, broadcast, questions

**Status:** accepted · 2026-11-15 (user decision, after the first multi-agent session; the detailed design is written when M3 starts)

**Amends:**
- [0033](0033-satellite-milestone.md): adds these tools to M3's MCP channel. Agent-to-agent messaging moves from M5 into M3; macOS notifications stay in M5.
- [0019](0019-control-plane-direction.md): the agent channel gains a mailbox, leases and events for the parent.

## Context

On 2026-11-15 one coordinating Claude Code agent spawned five agents through the `fleet` skill, each fixing a GitHub issue in its own worktree. The only channel between them was `fleet send`, which types keystrokes into the other agent's terminal. The session ran into these problems:

1. **Keystrokes are fragile.** `fleet send --enter` wrote the text and its Return in one write; Claude Code read it as a paste and left five messages typed but unsent. Fixed by sending the Return as a separate write, but the channel stays keystrokes: a message lands in whatever the agent is doing.
2. **No trusted sender.** A report arrives as if the user had typed it. The receiver can only treat it as untrusted text and cannot tell an agent's report from the user's instruction.
3. **The parent has to poll.** The coordinator watched screens with a polling script, then asked children to report back with `fleet send`. The report only arrives if the child remembers to send it.
4. **A dead child is silent.** A child that crashes, hangs, or waits on a permission prompt cannot report.
5. **No shared resources.** The children had to take turns running the wall benchmark (0014). A `mkdir` lock serialised the runs but left a stale lock when its holder was stopped, and it could not keep the other children's builds off the machine (load average ~210 made a wall run report a false `gui_cold_start_ns` regression).
6. **No broadcast.** The same rule was sent five times, one agent at a time.
7. **Questions to the user get stuck.** A child that needed the user's approval asked in its own terminal; the coordinator relayed it.

M3's `report_status` (0033) answers part of 3 and 7, but not the rest.

## Decision

All of the following lives in M3's agent channel. It is authenticated with the per-agent token (0033), so fleetd knows each caller and every message's sender is verified, not claimed. The same scope rule as 0033 applies: an agent acts only on its own descendants, plus the replies and leases described below.

### 1. Waiting for children: `wait_for`

- `wait_for {agents, states, timeout}`: blocks until any listed agent (default: all of the caller's descendants) reaches one of `states` (`waiting`, `done`, `error`, `exited`, `lost`; default all of them). It returns the agents that matched and their status and message.
- It replaces polling and works with any MCP client, since it is an ordinary tool call. `timeout` defaults to 10 minutes and is capped (the MCP client's own limit applies too); on timeout it returns the current statuses.
- CLI: `fleet wait <ids…> [--state …] [--timeout …]`, which prints the same result and exits 0 on a match, 1 on timeout.

### 2. Automatic exit reports

- When a child exits or is lost, fleetd records it as a status event for the parent, even if the child never called `report_status`: `exited` with the exit code, or `lost`.
- An inferred status (from output activity, 0033's hollow dot) is part of the event too, so a child quiet for a long time while showing a prompt can be reported as `waiting (inferred)`.
- These events are what `wait_for` and the mailbox (3) deliver; nothing has to be typed into the parent's terminal.

### 3. A mailbox instead of keystrokes

- Every agent has a **mailbox** in fleetd. `send_message {to, text}` puts a message in it, with the verified sender's id and name (`name` label, issue #48) and a timestamp. `text` is at most 4 KiB.
- **Allowed recipients:** the agent's own descendants, its parent, and the user. Not siblings or the user's other agents.
- `read_messages {since}` returns new messages; `wait_for` also returns when a message arrives (state `message`). Status events from (2) arrive in the parent's mailbox too.
- Messages are **data, not instructions**: the agent skill and the MCP tool descriptions say that a message from another agent is a report or a request to weigh, never the user's command.
- **Keystrokes stay** for agents without the MCP channel (`fleet send`, which keeps its separate Return). They are the fallback, not the main channel.
- The graph view (0033) draws message lines between agents from the mailbox.

### 4. Leases for shared resources

- `lease_acquire {name, wait, ttl}` and `lease_release {name}`, plus `fleet lease <name> -- <command>` (hold the lease while the command runs).
- A lease is held by an agent, not a file: fleetd **releases it automatically when the holder exits or is lost**, and at `ttl` (default 1 hour) if one is set. A stale lock like 2026-11-15's cannot happen.
- **Conditions:** a lease can require that the machine be quiet before it is granted, for example `--max-load 10` (1-minute load average), for benchmarks. While a `quiet` lease is held or waited on, fleetd may hold back new `quiet`-sensitive leases, but it never pauses other agents.
- **Scope:** names are per node. Any agent may use any lease name; leases grant nothing (no credentials, no quota), so they need no label rules.
- The wall benchmark uses a lease named `perf-wall`, and the `perf-wall` CI job takes it too, so local runs and CI never overlap.

### 5. Broadcast

- `send_message` accepts `to: {children: true}` (direct children), `{descendants: true}`, or a label selector over the caller's descendants, for example `{labels: {role: "reviewer"}}`.
- The user can broadcast from the GUI and CLI to any agents (`fleet message --label project=fleetd "…"`), since the user is not limited to descendants.

### 6. Questions to the user, answered in the GUI

- `report_status {state: "waiting", message, question}` can carry a **question**: its text, and optional choices (for example "Approve the regression" / "Re-run on a quiet machine").
- The GUI shows it on the agent's card, sidebar row and pane (the "needs you" state, issue #32), with the choices as buttons and a free-text answer. The CLI has `fleet answer <id> "…"`.
- The answer goes to the asking agent's mailbox as a message from the user, the only message an agent can treat as the user's own words. Its parent gets a copy, so a coordinator knows the question was answered.
- Perf-regression approval (CLAUDE.md, 0014) still needs the user's explicit decision; an answer through this channel is that decision only when the user gives it.

## Consequences

- **M3 grows.** Messaging, leases and questions move from M5 into M3. The rest of M5 (adapters, hook-based status, macOS notifications) stays.
- **The agent skill changes** when M3 lands: the main way to coordinate becomes `wait_for`, `send_message` and leases. `fleet send` is described as the fallback for agents without MCP.
- **Storage:** mailboxes and leases live in fleetd's state and survive a daemon restart (0032). A mailbox is removed with its agent (`fleet rm`).
- **Perf (0014):** mailbox and event delivery stay off the PTY output path, as 0019 requires. A GUI benchmark covers drawing message lines with the 50-agent graph.
- **Open for the milestone's design:**
  - how `wait_for` copes with MCP clients' tool-call timeouts (re-call versus a long poll);
  - the exact inferred-`waiting` heuristic, shared with issue #32;
  - mailbox retention limits;
  - whether `quiet` leases should also delay *new spawns* on a busy node.
