# M3: inferred `waiting` sweep — plan

Issue #126, ADR 0088 (builds on 0075, 0076). No proto change.

1. `AgentPipeline` gains `last_output_unix_ms()` and `last_line()` (read under its own lock, no change to the feed path).
2. `mailbox.rs`: the body of `Node::status_changed` becomes a free `post_status(store, notices, …)` so the sweeper thread can call it.
3. `node/waiting.rs`: `Sweeper` (one lazily started thread, parked while no agent runs, 5 s period), `is_waiting` shared by the sweep and `Node::inferred_waiting`.
4. `Node::wait_check` reads the live agents' activity before it takes the store lock (lock order: agents → store) and answers `waiting` from the same `is_waiting`.
5. Tests first (`crates/fleet-node/tests/waiting.rs`) with an injectable clock (`sweep_waiting_at`, `wait_check_at`).
6. ADR 0088, clippy, fmt, PR.
