# 0030 — Agent restart and forget

**Status:** accepted · 2026-11-15 (M2c)

**Amends:** [0021](0021-agent-history.md) (forget deletes history early; retention never prunes store rows) and [0022](0022-protocol-v2.md) (new messages). Spec: `docs/superpowers/specs/2026-11-14-m2c-daily-drive-design.md` §2–§3.

## Context

Daily use of the GUI leaves exited and lost agents in the list with no way to rerun or remove them. Restart and removal change node state (the store, history, scrollback rings) and every connected client must see the result, so they are node requests, not client-side compositions of spawn and delete.

## Decision

### Protocol (additive; `PROTOCOL_VERSION` stays 2)

- `Request::RestartAgent { agent_id, cols, rows, attach }` → `Response::Restarted { old_id, agent, attached? }`. Same program, args, cwd and labels, a new id; `cols`, `rows`, `attach` mean what they mean in `SpawnAgent`.
- `Request::ForgetAgents { ids, exited_only, selector }` → `Response::Forgotten { ids, refused }`. Partial, not all-or-nothing: each id is forgotten or refused on its own. `exited_only` with a label `selector` (empty: all) forgets every exited or lost agent matching it; `ids` must then be empty.
- `Event::AgentRemoved { agent_id, replaced_by? }`, pushed to every connection after the request that caused it is answered. The requester queues it itself and its broadcast forwarder skips events it caused. A broadcast `Lagged` sends nothing; clients recover by relisting.
- `AgentList.replaced: map<old id, newest successor>`: the durable restart record, so a client that missed `AgentRemoved` can still follow a restart. Chains are collapsed on write, so one lookup suffices.
- `AgentRemoved` may arrive before `AgentExited` for the same id in a narrow window; clients treat it as authoritative and relist.

### GUI following

- A pane restarting its agent with ⌘R gets the successor from its own `Restarted` reply, so the control connection's `AgentRemoved` moves the layout to the successor without attaching it. If the pane's connection drops before that reply, the pane link sends `UiEvent::LaunchDropped(pane)` (same for a spawn into the pane): the app drops what was on its way into the pane and attaches the agent the layout names, which is the successor if it was announced already, else the old agent until `AgentRemoved` moves it on.
- After each list, a pane whose agent is neither listed nor replaced is emptied unless a spawn, restart or attach to another agent is on its way into it: a lost pane at once, a pane showing its agent's final screen only when two lists in a row left the agent out (so an agent spawned after a list was taken, which exited before the next one, keeps its pane).

### Store: schema v3

- `agents.id` becomes `INTEGER PRIMARY KEY AUTOINCREMENT`: **ids are never reused**, including after failed spawns, so a stale id held by a script, an agent or a saved layout can only be refused, never hit an unrelated agent. A fresh state DB is created directly at v3; v1 and v2 databases migrate in place, keeping rows and `agent_labels`.
- `agent_replacements(old_id PRIMARY KEY, new_id)`: written by a restart in the same transaction that deletes the old row; rows pointing at the old id are repointed (chains collapse). Rows whose `new_id` no longer exists are deleted when that agent is forgotten without replacement.

### Restart

1. Refuse, checked under the agents lock in this order: an id being restarted (per-id `restarting` guard) → `WouldBlock` "agent {id} is being restarted"; an id with no store row → `NotFound` "unknown agent {id}"; an id whose row says running, or that is still in `agents.live` or `histories.spawning` (its row may already say exited) → `InvalidInput` "agent {id} is running". The store row alone is not trusted for "not running".
2. Read program, args, cwd and labels from the store. **Labels are not re-validated**: they were validated when set, so the spec's "validated without the reserved check" is superseded. The stored cwd must be a directory, else `NotFound`: portable-pty would silently fall back to `$HOME`. Plain spawn keeps that fallback.
3. Spawn outside the agents lock (it waits on the base environment and PTY spawn, and must not block `on_exit`). A failed spawn leaves the old agent untouched.
4. One store transaction: delete the old row and labels, write the replacement. Then delete the old final screen, history directory and scrollback ring, and announce `AgentRemoved { old, replaced_by }`. If the transaction fails, the restart still succeeds (the new agent runs) but nothing is announced: `Node::restart` reports `replaced: false`, the old agent stays listed and unreplaced, and the server answers `Restarted` without queueing `AgentRemoved` to the requester either.

A crash between steps 3 and 4 leaves both rows (old exited, new running), never neither.

### Forget

For each id, under the agents lock: refuse it if it is in `agents.live`, `histories.spawning` or being restarted, or if the store has no non-running row for it (unknown, or its row says running). A refused id goes into `Forgotten.refused`, untouched; there is no per-id reason, so `fleet rm` reports it as "not removed (running or unknown)" even when it was refused for being mid-restart. Otherwise: delete the store row, then drop it from `Agents.exited` and delete the history directory (closing an open handle) and `<state>/scrollback/<id>.ring` (raw output, may contain secrets; before this ADR no code path deleted rings), and announce `AgentRemoved`.

A store error stops the loop. The request is answered with an `Error` (not `Forgotten`: the request failed, and a partial success would hide that), but the ids forgotten before it were already announced to other connections, so the requester's `AgentRemoved` for each of them is still queued after the `Error`. `Node::forget` returns them in `ForgetError.partial`.

### Orphan rings

A ring can outlive its row: deleting it can fail after the row is gone, or the node can crash in between. `Node::open` therefore deletes every `<state>/scrollback/<id>.ring` whose id has no store row. It is best-effort: an unreadable directory entry or a failed delete is logged and skipped, never fails the open. Only canonical names (the decimal id, so `7.ring` but not `007.ring` or `+7.ring`) count as rings; anything else in the directory is left alone.

### Environment is not restored

`SpawnAgent.env` is not stored (it would put secrets in SQLite, [0012](0012-credentials.md)). A restarted agent gets the node's base environment ([0024](0024-agent-skill-stdio-env.md)), a fresh `FLEET_AGENT_ID` and its labels. `fleet restart --help` says so.

### Retention

Retention ([0021](0021-agent-history.md)) prunes history directories and final screens, **never store rows**. An old row stays until the user removes it.

### No version bump

prost decodes an unknown `oneof` variant as `kind = None`, and the server answers `Error "empty request"` without disconnecting. A new client reads that reply to `RestartAgent`/`ForgetAgents` as "daemon too old": the CLI prints "the daemon is too old to restart/remove agents", the GUI "fleetd is too old to restart or remove agents". Old clients skip unknown event kinds and the unknown `AgentList.replaced` field.

## Consequences

- Restart replaces the old agent: the new one takes its tab, and clients that missed it follow `replaced`.
- Forgetting frees history slots early; ids stay unique for the life of the state DB.
- Known limits: a restarted agent loses its env overrides; a crash mid-restart leaves the exited original until it is removed; `AgentRemoved` can precede `AgentExited` for one id.
