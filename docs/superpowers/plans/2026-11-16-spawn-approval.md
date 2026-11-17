# M3-I: spawn approval + new-child highlight — plan (#110, ADR 0079)

Spec §1/§6, ADR 0033 ("Approval"). Proto block 75–79. No store migration (pending spawns are in memory only).

## Steps

1. **Config** (`fleet_node::scope`): `[node] spawn_approval = true|false` (default false) and `spawn_approval_timeout = <seconds>` (default 300) next to the quotas. Unit test in the config reader.
2. **Proto** (75–79): `ListPendingSpawns`/`PendingSpawns`, `DecideSpawn` (→ `Ack`), events `SpawnPending` (a `PendingSpawn`) and `SpawnDecided { id, decision }`.
3. **Registry** `fleet_node::approval::Approvals` (in memory): `ask` → `Ticket` (await the decision with a timeout; dropping it withdraws), `decide`, `list`, `refuse_all`, a broadcast of `SpawnPending`/`SpawnDecided`. Unit tests: approve, deny, expiry, withdraw on drop, unknown id, refuse_all.
4. **Node**: `Node::precheck_spawn(caller, &SpawnAgent)` runs the quotas (announcing `SpawnRefused` as today) and returns the narrowed labels to show; `Node::approvals()`.
5. **Server**: an agent's `Spawn` with approval on: precheck → ask → a task per pending spawn (aborted with the connection) → on approval the request goes back through the connection loop and spawns as before (quotas checked again under the gate); otherwise `forbidden: spawn not approved …`. A daemon restart's request freeze refuses every pending spawn. Scope table rows: `ListPendingSpawns`/`DecideSpawn` refused for agents. Events forwarded to the user's connections only.
6. **CLI** `crates/fleet-cli/src/approval.rs`: `fleet spawns [--json]` (pending), `fleet approve <id>`, `fleet deny <id>`. Exit 0 / 1 (no such pending spawn) / 2 / 3.
7. **Integration test** (`fleetd/tests/approval.rs`): agent spawn waits, user sees `SpawnPending`, approve → spawned; deny → forbidden; timeout → forbidden; the user's spawn never waits; the agent sees no approval events; quota refusal comes first.
8. **GUI**: session keeps pending spawns (list on connect, events), `Session::decide_spawn`; a "Spawn requests" block at the top of the sidebar (asker, program + args, cwd, labels, Approve / Deny). Core `NewChildren` tracker (first-seen time of agents with a parent, after the first list) → highlight strength for sidebar rows and graph cards; reduce motion = a steady highlight without fade.
9. ADR 0079, fmt, clippy, full tests, PR.
