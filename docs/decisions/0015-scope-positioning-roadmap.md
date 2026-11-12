# 0015 — Scope, positioning, platforms, licensing

**Status:** accepted · 2026-11-13

**Decision:**
- **Agents:** v1 wraps existing CLI agents. End goal: a built-in agent runtime, added later as a native adapter speaking the adapter interface in-process — no architectural change.
- **Positioning vs Orca:** small, opinionated core done extremely well, everything else as plugins; weighted toward server/headless-first fleet management.
- **Users:** single developer; don't preclude multi-user.
- **v1 in:** CLI agents in PTYs, hierarchy, live terminals + sidebar, status, notifications, read-only diff, local + remote nodes, plugins, credentials.
- **v1 out:** chat UI, diff comments, coordinator mode, browser/computer use, mobile, automations, usage tracking, integrations.
- **Platforms:** controller + GUI on macOS; nodes on Linux (and macOS). Windows deferred.
- **Licensing:** private for now; revisit before the plugin API stabilises.
- **Milestones:** M0 bench harness + CI gate → M1 local node → M2 GUI → M3 hierarchy + diff → M4 plugins/adapters/status/notifications → M5 remote nodes → M6 credentials.
