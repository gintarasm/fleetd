# 0042 — GUI agent status: a client-side heuristic until hooks

**Status:** accepted · 2026-11-16 (M2d, issue #32)

Builds on [0008](0008-agent-status-detection.md) (hooks first, heuristics fallback) and [0029](0029-gui-core-toolkit-free.md) (the GUI core stays toolkit-free).

## Context

The GUI should say which agent needs you without reading every screen. Agent hooks, ADR 0008's primary source, come with the plugin host in M5. Until then the GUI only has what fleetd already sends: each agent's state and exit code, and `AgentActivity` (its last non-blank line and last-output time, at most every 250 ms while it prints).

## Decision

- The GUI derives the status itself, in one tested function: `core::status::agent_status`. There is no protocol change.
  - **Working:** output in the last 3 s. Agent CLIs redraw a spinner while they work, so a quiet agent has stopped.
  - **Needs you:** quiet, and the last line looks like a prompt (`looks_like_prompt`: ends in `?`, `>`, `❯`, `:`, `$` and the like, carries a TUI waiting hint such as "? for shortcuts", "(y/n)" or "esc to cancel", or is a box-drawing rule), or a pane showing it rang the bell.
  - **Idle:** running and quiet with no sign that it waits (or no output seen yet). Not one of the issue's four states, but calling every quiet agent "needs you" would make the count noise; idle draws as a grey ring and is not counted.
  - **Done:** exited 0. **Failed:** exited non-zero, exited without a code, or lost.
  - Plain shells (`kind=shell`, ADR 0038) get no status.
- **Bell:** the client terminal (`fleet_term::Terminal::take_bell`) reports BEL; the pane flashes, and the agent's row is marked (and counts as needs you) until a pane showing it is focused in the active window. Only agents shown in a pane can ring: fleetd does not forward the bell.
- **Alerts** (`core::status::Alerts`): a notification when an agent starts to need you or ends, never for what was already so at launch, and only while the window is not focused. They are opt-in (`[gui] notifications = true`, toggled from the menu-bar menu until the Settings window, #28, shows it) and use `UNUserNotificationCenter`, which works only from the app bundle; clicking one shows that agent.
- The dot sits on the sidebar row and the pane header: working in the accent colour, needs you in a new `warning` token (system orange), done green, failed red (with `✕code`). A working dot breathes by dimming every other second on the sidebar's existing one-second tick (`dot_dimmed`), not with a gpui animation, which would redraw the window every frame while any agent works; it is steady under Reduce Motion. The bell flash fades the pane's border out over 600 ms (shown, then removed, under Reduce Motion).
- The needs-you count is the Dock badge and the menu-bar item's title; the menu's agent lines say "— needs you".

## Consequences

- The heuristic misreads some programs (a long silent build that ends its last line with `:` reads as needs you). It stays in one function so hook-based status (M5) can replace it, with the hook's source and confidence that ADR 0008 asks for.
- Unbundled runs (`cargo run`) have no notifications; the toggle says so on stderr.
