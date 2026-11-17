# 0079 — Spawn approval and the new-child highlight

**Status:** accepted · 2026-11-17 (M3-I, issue #110; builds on [0033](0033-satellite-milestone.md) "Approval", the M3 spec §1 and §6, and [0072](0072-agent-identity-by-session.md))

## Context

0033 settled that agents spawn without asking by default, that new agents are highlighted briefly, and that `spawn_approval = true` in the config makes an agent's spawn ask the user first. The spec adds that quotas live in `[node]` of `~/.fleet/config.toml` and apply to agents' spawns only. This ADR records what they left to the builder.

## Decision

**Config.** `[node] spawn_approval = true|false` (default `false`) and `spawn_approval_timeout = <seconds>` (default **300**, at least 1), read at daemon start by the same small `[node]` reader as the quotas (`fleet_node::scope::Quotas`, which gains the two fields). Like the quotas, changing them needs a daemon restart; agents can read the file but not change the policy through fleetd. A bad value is logged and the default kept.

**Who waits.** Only an **agent's** `Spawn` (a `Caller::Agent` connection, 0072). The user's spawns never wait, from any client. An agent's **restart** of a descendant does not ask again: it re-runs a command the user already let run (or that predates approval). It still counts against the quotas.

**Order.** The quotas are checked first (`Node::precheck_spawn`, the same check and `SpawnRefused` event as before): a spawn over a quota is refused at once and never shown to the user. The precheck also narrows the labels, so the user sees the labels the child would get. After approval the spawn runs the normal path, so the quotas are checked **again** under the spawn gate (a slot may have gone while the request waited). An agent may have at most **8** spawns waiting at once over all its connections; the ninth is `forbidden: 8 spawns of agent N already wait for approval`.

**Where it waits.** `fleet_node::approval::Approvals` is an in-memory registry owned by `Node`: `ask` → a `Ticket`, `decide`, `list`, `refuse_all`, and a broadcast of changes. It sets no timer. fleetd (`fleetd/src/approval.rs`) waits on the ticket on a task of its own with the timeout, so the asking connection keeps serving requests, is not "in flight" (it never holds off a daemon restart), and is aborted with the connection, which **withdraws** the request (dropping the ticket). On approval the request goes back through the connection's request loop and spawns like any other (attached or not); otherwise it is answered `Error`.

**Refusals** (all `forbidden:`, CLI exit 3):
- denied: `forbidden: spawn not approved`
- timed out: `forbidden: spawn not approved (no answer within 5m)`
- daemon restart: `forbidden: spawn not approved (fleetd is restarting)`

**Pending spawns survive nothing.** They are memory only. `fleet daemon restart` (0032) refuses every pending spawn as soon as it stops requests, before the freeze, even if the restart then fails and the daemon thaws: the agent asks again if it still wants to. The answer is written best-effort before the `exec`; an agent whose connection ends first sees a lost connection instead, and either way nothing was spawned. A plain stop or crash ends the connections, which withdraws everything. Nothing is persisted, so no schema change.

**Protocol** (field block 75–79, additive, `PROTOCOL_VERSION` stays 2): requests `ListPendingSpawns = 75` (→ `PendingSpawns = 75`) and `DecideSpawn { id, approve } = 76` (→ `Ack`, or `no pending spawn N` once decided, expired or withdrawn); events `SpawnPending = 75` (a `PendingSpawn`: id, parent, program, args, cwd, narrowed labels, asked and expiry times) and `SpawnDecided { id, decision } = 76` (approved, denied, expired, withdrawn, restarting). Pending ids count from 1 per daemon process and are not agent ids.

**Only the user approves.** Both requests are explicit rows in the scope table (`fleetd/src/scope.rs`) that refuse agents (`forbidden: only the user approves spawns`). The events go through the connection's `Scope` (`Scope::sees_spawn_approvals`): the user's connections get them, agents' never do, not even the asking agent's (it learns the outcome from its own reply).

**CLI** (`crates/fleet-cli/src/approval.rs`): `fleet pending [--json]` lists the waiting spawns (who asks, by id and name; the command; time left; cwd; labels without `fleet.*`), `fleet approve <id>`, `fleet deny <id>`. Exit codes: 0 ok, 1 no such pending spawn, 2 other error, 3 forbidden (run inside an agent). The asking agent's `fleet spawn` simply blocks until answered; it prints nothing while it waits (it does not know whether approval is on).

**GUI.**
- The session lists pending spawns on every (re)connect and follows the two events; a daemon from before approval refuses the list, which is not shown. Changes wake the UI as `AgentsChanged`.
- **Spawn requests** sit at the top of the sidebar, above the agents, so a request is never scrolled away: one card each with "*name* (*id*) wants to start an agent", the command line, its folder and labels, and **Deny** / **Approve**. In the graph view, where the sidebar is hidden, the same block floats at the top right. No countdown is drawn (it would redraw every second); `fleet pending` shows the time left.
- No macOS notification or dock badge for a pending spawn: notifications are M5.

**New-child highlight** (`core::new_children`). An agent with `fleet.parent` that first appears in a list after the GUI's first list (of this target) is new; what was there when the GUI connected is not news, and a restart's successor is not a new child. Its sidebar row gets an accent tint and its graph card an accent tint and border, at full strength for **1.6 s**, then fading over **0.9 s** (ease-out). With reduced motion it holds for the same 1.6 s and ends at once, without the fade. The view redraws every frame only during the fade and sets one timer for the end of the hold, so there is no idle cost. The highlight is every new child's, approved or not: with approval off it is the only notice of an agent's spawn.

## Consequences

- With approval on, a long unattended run stalls on the first spawn until the timeout: that is the point of the setting, and why it is off by default.
- An agent cannot distinguish "waiting for approval" from a slow spawn while it blocks. A dedicated "pending" reply was considered and left out: it would need a second reply per request, and the CLI's `call` model is one reply.
- The asking agent's parent gets no mailbox message about a refusal; the refusal is the spawn's own reply.
