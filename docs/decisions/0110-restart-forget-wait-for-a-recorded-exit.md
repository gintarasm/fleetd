# 0110 — Restart and forget wait for an exit already recorded

**Status:** accepted · 2026-11-23 (issue #159)

## Context

An agent's exit is processed in its output sink (`NodeSink::on_exit`) in this order: the store row is set to exited (so anyone who sees the exit also lists it), the exit is reported to the parent's mailbox, and the `Notice::State { Exited }` that wakes `fleet wait --state exited` goes out. Then the pipeline is ended, and only after that is the agent taken out of `live` and retired.

Restart and forget refuse an agent that is still `live` ("agent N is running" / refused), whatever its row says. So between those steps a client that just saw the exit, through `fleet ls`, `fleet wait --state exited` or the GUI's list, and then restarts or removes the agent was refused as if the agent were running. On an idle machine the window is short. Under load (CI) it was long enough to fail `fleet restart` and `fleet restart --attach` in the CLI tests about half the time, and a tight `list` → `restart` loop in the node hit it on the first try. Overseer scripts that do `fleet wait --state exited X && fleet restart X` hit the same window.

## Decision

- `Node::restart_with` (restart, restart --attach, an agent's restart) and `Node::forget` (rm, rm --exited) first wait while the agent's **exit is in progress**: it is still `live` but its row already says exited. They poll every 1 ms, for at most **5 s** (`EXIT_SETTLE`), checking under the agents lock, then the store lock (the order restart already uses).
- After that the existing checks run unchanged: an agent that is running, or still finishing its exit after 5 s, is refused as before.
- The order in `on_exit` is unchanged: the store still records the exit first, and the notices and the exit event still go out when they did.

## Consequences

- An agent listed as exited can be restarted or removed straight away; nothing else changes.
- Restart and forget of an agent whose sink is stuck mid-exit now take up to 5 s before refusing it. Both already block (SQLite, process spawn) and run off the async executor.
- `restart_and_forget_right_after_the_exit_is_listed` (fleet-node `node_lifecycle`) restarts and forgets the moment `list` shows the exit, 40 times. It failed on its first iteration before this change.
- Perf not measured (`PERF_CI` off). The wait happens only in that window. Otherwise it is one extra agents-lock check (and a store read for a live agent) per restart or forget.
