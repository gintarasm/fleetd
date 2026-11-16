# 0077 — Leases: named locks held by an agent, a connection or a TTL

**Status:** accepted · 2026-11-17 (M3-F, issue #107; builds on [0034](0034-agent-communication.md) §4 and the spec's §4)

## Context

0034 and the M3 spec (§4) settled what leases are: per-node named locks, open to any caller, held by an agent or the user, released on explicit release, holder exit or loss, the holding connection closing, or a TTL; `--max-load` for quiet-machine benchmarks; surviving a daemon restart; the `perf-wall` job and `fleet-perf run --suite wall` take lease `perf-wall`. This ADR records what the spec left to the builder.

## Decision

**Where it lives.** `fleet_node::lease::Leases` is a synchronous state machine owned by `Node`: the `leases` table (schema **v10**; v9 is M3-E's mailbox, renumbered by the overseer at merge) plus in-memory FIFO queues. It reads no clock and sets no timer: every method takes `now_ms`, and the load average comes from a function given at construction, so the logic is unit-tested without sleeping. fleetd's driver (`fleetd/src/server/lease.rs`) sleeps until `next_wake` (a TTL, a reclaim deadline, or one second while a head waiter has `--max-load`) or until woken; with nothing time-dependent it never wakes (0014: no idle cost).

**Protocol** (field block 65–69): requests `AcquireLease = 65`, `ReleaseLease = 66`, `ListLeases = 67`; responses `LeaseGranted = 65`, `LeaseList = 66`. Additive; `PROTOCOL_VERSION` stays 2. In the scope table (`fleetd/src/scope.rs`) all three are explicit rows that **allow** any caller (0034: leases grant nothing, so no label or descendants rule).

**Holder and the `--` form.** The holder is the caller's agent id, or the user (`NULL`). `holder_conn` in the table is *not* the daemon's connection id (that dies with the process): it is a random **holder key** the CLI picks once per `fleet lease … --` run, set only for leases tied to a connection. The connection itself is tracked in memory.

**Waiting.** A waiting `AcquireLease` is answered from its own task, so the connection keeps reading and a closed connection drops its waiters. FIFO is strict per name: the head waits for its `--max-load`, and those behind it wait for the head, even if they would pass. Without `--wait`, a held lease (or a free one with waiters ahead) is `Error("busy: …")`, a load above the limit is `busy: load average …`. `getloadavg` failing counts as infinite load: a benchmark is never granted on a guess.

**Release.** Explicit `release`: the holder, or **the user for any lease** (the way to clear a stuck one). An agent releasing a lease held by someone else gets `forbidden:` (exit 3): lease *names* are open to everyone, but only the holder lets go. Agent exit: the driver listens to the node's exit events and releases the agent's leases and queued asks; it sweeps once at start and after a lagged receiver, and at load time leases of agents that no longer run are deleted. A connection closing releases the leases tied to it and leaves every queue it was in. TTL: counted from the grant, also for the `--` form (a ceiling on a stuck command, which keeps running).

**User's `acquire` needs `--ttl`** (spec): enforced by the daemon (`usage: …`, exit 2), since the CLI cannot tell a user's session from an agent's. A user's `--` form needs none: its connection is the watch. Agents' `acquire` has none required: the agent's exit releases it.

**Restart (0032).** Leases are rows; `fleet daemon restart` execs, so connections end without destructors and nothing is released. A lease tied to a connection loads as **orphaned** with a 30 s grace; the CLI, whose connection ended, reconnects (retrying 45 s) and presents the same key, which re-ties it (keeping `acquired`). A client that does not come back loses it after the grace. A waiter that reconnects asks again and joins the **back** of the queue (the order is not kept across a restart). A plain `fleetd` stop normally drops the connections (their destructors run as the runtime shuts down), which releases the leases tied to them; that is right, since nothing watches them any more.

**Scope of what `lease ls` shows.** Any caller lists every lease (names are open). For an agent caller, a holder that is neither itself nor in its descendants is shown as `holder_hidden` (`holder_kind: "other"`), not its id: 0072 keeps ids outside the subtree from being revealed.

**CLI.** `fleet lease <name> [--max-load N] [--ttl 1h] [--wait] -- <cmd…>` runs `cmd` as a child and exits with its code (128+n for a signal; 127 not found, 126 not executable). fleet survives SIGINT/SIGQUIT (the terminal also sent them to the command), and passes SIGTERM and SIGHUP on, so the lease never outlives the command by accident. `acquire`, `release`, `ls [--json]` are subcommands, so a lease cannot be *named* `acquire`, `release` or `ls` in the `--` form (use `acquire`). Exit codes: 0 ok, 1 busy / not held, 2 usage or other error, 3 forbidden. `--ttl` takes `90`, `90s`, `30m`, `1h`, `1h30m`, `2d`. `--wait` has no timeout of its own (wrap the command in `timeout` where it matters); it prints "waiting for lease" after 0.4 s.

**`perf-wall`.** The user applies the gate-file changes (see the PR); this ADR only makes them possible.

## Consequences

- Nothing is on the PTY output path; a connection closing costs one lock and a scan of the (tiny) lease tables.
- Two clients cannot take the same hold key by accident (63 random bits); a key is only a reclaim ticket for a lease whose holder is the same caller identity, and the daemon checks both.
- Not decided here: a lease's metadata (why it is held), notifying the GUI of lease changes, and a `fleet lease wait` that only waits. They can be added as new request kinds in a later block.
