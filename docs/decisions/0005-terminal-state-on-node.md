# 0005 — Terminal state lives on the node

**Status:** accepted · 2026-11-13

**Decision:** Each node keeps a headless VT grid per agent (`alacritty_terminal`) plus bounded on-disk scrollback. On (re)attach, the client receives a grid snapshot followed by the live byte stream — never a full log replay.

**Consequences:** Reattach is O(screen), not O(history). GUI/controller disconnects lose nothing. Node memory grows per agent and is covered by the perf budget.
