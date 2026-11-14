# 0005 — Terminal state lives on the node

**Status:** accepted · 2026-11-13

**Amended by:** [0021](0021-agent-history.md) — the node also keeps the lines that scroll off the grid (in-memory tail plus on-disk segments) and serves them with the snapshot and in pages.

**Decision:** Each node keeps a headless VT grid per agent (`alacritty_terminal`) plus bounded on-disk scrollback. On (re)attach, the client receives a grid snapshot followed by the live byte stream — never a full log replay.

**Consequences:** Reattach is O(screen), not O(history). GUI/controller disconnects lose nothing. Node memory grows per agent and is covered by the perf budget.
