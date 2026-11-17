# 0088 — Inferred `waiting`: a periodic sweep

**Status:** accepted · 2026-11-17 (issue #126, M3)

**Builds on:** [0075](0075-fleet-report-and-inferred-status.md) (the heuristic, `WAITING_QUIET_MS`), [0076](0076-mailbox-exit-reports-wait.md) ("Merged with M3-D": the open item), [0014](0014-performance-gate.md) (idle budgets).

## Decision

- **One sweep thread in the node** (`fleet-node/src/node/waiting.rs`), not a task in fleetd: it needs the live pipelines and the store, which the node owns. It starts with the first agent that runs (a spawn or an adopted agent kicks it) and **parks while no agent runs**; otherwise it wakes every **5 s** (`SWEEP_PERIOD`). So an idle node with no agents has no thread and no wakeups, and detection is at most 5 s later than the 30 s quiet period.
- **Cost per round:** one clone of the live map, then per running agent one pipeline lock to read `last_output_unix_ms`. An agent that printed within 30 s (or never) is skipped there. For a quiet one the last line is read once and **cached by its output time**, so a parked agent's screen is read once, not every round. Only an agent whose last line looks like a prompt costs a store read (its record, for kind and report). Nothing is added to the PTY output path: `feed` is unchanged, the sweep only reads what it already records.
- **The rule is one function** (`is_waiting`): `fleet_proto::status::inferred_status(…, WAITING_QUIET_MS)` equal to `NeedsYou`, for a running, non-shell agent with no live report. The sweep and `Node::wait_check` (so `fleet wait --state waiting` with named targets) both call it with the same two numbers from the pipeline (`AgentPipeline::activity()`, `Node::last_activity`). The bell (`rang`) is not used in the node.
- **Precedence: a live reported status wins.** An agent that has said `working`/`waiting`/`done`/`error` (ADR 0075, `live_report`) is never inferred about: the sweep posts nothing and a wait does not match inferred `waiting` for it. Its report already posted its own message. Reports are not expired by silence (0075 stands).
- **Transitions.** Into inferred `waiting`: `status_changed(agent, Waiting, "", inferred = true)` (body `waiting (inferred)`). Out of it, while the agent still runs, has no live report and its output resumed (or the line stopped looking like a prompt): one `working (inferred)`. The one-per-change check in `status_changed` stops repeats. An agent that exits or reports while flagged leaves the set silently (its exit report or its own report speaks).
- **Sweep memory is not persisted.** After a daemon restart or handoff an agent already quiet at a prompt is reported once, 30 s after the node sees it quiet again (the pipeline's last output time restarts), unless the latest status message already says so.
- **Locks.** `wait_check` reads the live agents' activity *before* it takes the store lock, so the agents lock is never taken inside the store lock (restart takes agents then store).
- **Testing seam:** `Node::sweep_waiting_at(now)` and `Node::wait_check_at(…, now)` take the clock; the thread passes the real one.

## Consequences

- `Node::inferred_waiting`'s stub is gone; `fleet wait --state waiting` matches reported and inferred waiting alike.
- A prompt-like last line on a *busy-looking but silent* agent (a long compile that ends in `:`) is reported as waiting after 30 s; that is the heuristic's known weakness (0042), now visible to parents. A report overrides it.
- Perf: nothing on the output path; idle cost is one 256 KiB-stack thread waking every 5 s only while agents run. Not measured (PERF_CI off).
