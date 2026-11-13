# 0007 — Plugin system

**Status:** accepted · 2026-11-13

**Amended by:** [0025](0025-core-knows-agents-workflows-own-hierarchy.md) — adds the `workflow` plugin kind.

**Decision:**
- **Mechanism:** WebAssembly components (wasmtime + WIT), one versioned WIT world per plugin kind. Escape hatch: subprocess plugins speaking the same interface over stdio, for plugins that need a full OS (e.g. cloud SDKs). No native dylibs — plugins must never be able to crash the daemon.
- **Kinds in v1:** agent adapters, remote providers, lifecycle hooks, UI. Integrations (GitHub/Linear/Slack) later.
- **UI plugins:** return a declarative view tree (lists, buttons, text, forms, key-value panels) rendered by the GUI with native gpui widgets.
- **Placement:** adapters run on the node (near PTY and hook callbacks); providers, UI plugins and fleet-level hooks run on the controller. The controller distributes pinned `.wasm` artifacts to nodes.
- **Capabilities:** the manifest declares kind and capabilities (`pty:read`, `pty:write`, `fs:worktree`, `net:<host>`, `exec:<bin>`, `notify`, `storage:own`). Approved on install; re-approval when an update adds capabilities.

**Consequences:** Sandboxed, polyglot plugins. Arbitrary custom UI is not possible.
