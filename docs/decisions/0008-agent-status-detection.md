# 0008 — Agent status detection: hooks first, heuristics fallback

**Status:** accepted · 2026-11-13

**Context:** Orca has idle-detection false positives from relying on heuristics.

**Decision:** Statuses: working / waiting for input / done / error / auth needed. Primary source is the agent CLI's own hooks (Claude Code hooks, Codex `notify`, …) — the adapter installs a hook that calls back into `fleetd`. Fallback is PTY output and silence heuristics, defined per adapter. Every status records its source (hook | heuristic) and a confidence.
