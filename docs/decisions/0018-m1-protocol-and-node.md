# 0018 — M1 protocol server and node

**Status:** accepted · 2026-11-13

## Protocol

_To be completed in M1 Task 8 (protocol and snapshot details)._

## Daemon shutdown

**Context:** v1 has no PTY handoff to a successor daemon (0013). Leaving agents behind when `fleetd` stops would orphan untracked processes, the stale-session leak 0013 exists to prevent.

**Decision:** Stopping `fleetd` (SIGTERM/SIGINT) terminates its agents: SIGTERM to each agent's process group, up to 2 s grace, then SIGKILL (and a short wait). Exits are recorded through the normal exit path, so their rows become `exited`. An agent that still has not reported, or every agent after a daemon crash, stays `running` in the store and is marked `lost` on the next start.

**Consequences:** Upgrading or restarting the daemon ends all agents (drain-and-restart, as 0013 v1 requires); shutdown takes at most ~2.5 s. Agents that escape their process group (new session) are not reached.
