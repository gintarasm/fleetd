# 0013 — Daemon lifecycle and upgrades

**Status:** accepted · 2026-11-13

**Context:** Orca leaks stale daemons across updates, accumulating invisible sessions until OOM.

**Decision:** Exactly one `fleetd` per user per machine, enforced by lockfile + socket. v1 upgrades: warn that N agents are running and require drain-and-restart. v2: live handoff of PTY fds to the new daemon over a unix socket (SCM_RIGHTS) so agents survive upgrades.
