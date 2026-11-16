# 0051 — A lagging subscriber's drop is logged when it happens

**Status:** accepted · 2026-11-16 (issue #67)

**Amends:** [0027](0027-logging.md), "What is logged" (subscription dropped as `Lagged`).

## Context

A subscriber whose queue is full is dropped by the agent's reader thread (`fleet-node`): it sets the subscription's `lagged` flag and removes it. Until now the only log line was the connection forwarder's `subscription dropped: the client fell behind (Lagged)`. The forwarder writes it when it next reads from the subscription. A client that has stopped reading keeps the forwarder blocked on the connection's outbox, so that line comes only after the client reads its way through the backlog. A client that stays stalled never gets it.

That also left `stalled_terminal_gets_lagged_exit_2` (`fleet-cli`) without a condition to wait on. It paused the terminal for a fixed 3 s and hoped the backlog had reached the daemon by then. On a loaded machine it had not. Once the terminal read again, `fleet attach` kept up and no lag ever came. The test then spun in `until` (whose deadline did not hold while output kept arriving) for hours.

## Decision

- When output drops subscribers because their queues are full, the node logs `subscriber dropped: its queue is full` at **warn**, with `agent` and `conn` (absent for an anonymous subscriber). It is logged at the moment of the drop, from the reader thread. Logging goes through the non-blocking queue of ADR 0027, so the reader thread still never waits.
- The forwarder's `Lagged` warning stays. It records when the client is told about the lag.
- Only drops caused by output are logged this way. A drop while offering a role change or a resize needs a queue that output had not already overflowed, which is rare, and the forwarder's line still covers it.
- `stalled_terminal_gets_lagged_exit_2` keeps the terminal stalled until this line appears for its agent, bounded at 20 s. Then it lets the terminal drain and expects the restore sequence and exit 2.

The 0027 table gains a row:

| Event | Level | Fields |
|---|---|---|
| subscriber dropped: queue full (at the drop) | warn | agent id, conn id |

## Consequences

- One lag now gives two warnings: the drop, and later (if the client reads on) the notice to the client.
- No output, input or other client data is logged. The fields are ids only.
- Cost is nil on the normal path. `feed` already compared the subscriber count before and after offering. It now also collects the dropped subscribers' connection ids, in a `Vec` that allocates only when something was dropped.
