# 0002 — Headless daemon with a native gpui desktop client

**Status:** accepted · 2026-11-13

**Context:** Orca (Electron) suffers renderer crashes orphaning terminals, event-loop floods, input lag.

**Decision:** All state and processes live in a headless daemon (`fleetd`). The GUI is a thin client built with gpui (Zed's GPU-rendered framework), talking to the daemon over a local unix socket. The GUI stays resident as a macOS menu-bar app when its window is closed, and posts native notifications (daemons cannot without an app bundle). A thin `fleet-cli` is also provided.

**Alternatives:** TUI-first (ratatui), Tauri (webview), egui/iced, Electron.

**Consequences:** GUI crashes never affect agents. gpui has sparse docs and a smaller widget set — accepted cost for performance.
