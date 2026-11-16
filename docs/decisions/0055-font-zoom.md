# 0055 — Font zoom

**Status:** accepted · 2026-11-16 (M2d, issue #38)

**Builds on:** [0044](0044-gui-settings-window.md) (the Settings window and live apply), [0029](0029-gui-core-toolkit-free.md).

## Context

Issue #38 asks for ⌘+ and ⌘− to change the terminal font size and ⌘0 to reset it to the configured size, with the grid reflowing and the agent told the new size. It leaves open whether zoom is per pane or window-wide.

## Decision

- **Window-wide.** One zoom applies to every pane (fleet has one workspace window). The cell size stays the same across panes, so splits line up, and the state is a single value.
- **Not saved.** Zoom is a temporary change, as in Terminal: `[gui] font_size` stays the configured size and `config.toml` is not written. ⌘0 goes back to it. A relaunch starts at the configured size.
- **A new configured size ends the zoom.** Choosing a size in Settings, or a hand edit picked up when Settings opens, shows that size. Zoom then steps from it. The model (`core::zoom::FontZoom`) remembers which configured size it zoomed from, so no reset hook is needed.
- **Steps** are one point (`FONT_SIZE_STEP`), on whole points, clamped to the Settings range (8–32). A hand-edited size outside the range shows as configured until the first step brings it into range.
- **Keys:** `cmd-=` and `cmd-+` zoom in (⌘+ is ⌘= without Shift on most layouts), `cmd--` zooms out, `cmd-0` resets. A View menu offers Bigger, Smaller and Actual Size, and the Settings Keys list shows the three bindings.
- **Reflow** uses the existing path: `AppState::zoom` hands each pane the zoomed size through `PaneView::apply_config`, and the next layout recomputes the grid and resizes the session (the agent gets SIGWINCH).

## Consequences

- Zoom has no animation, so Reduce Motion does not apply; it does not touch transparency.
- Performance: nothing is added to the per-frame path. A zoom re-shapes once, like a Settings font change. Not measured (PERF_CI off, [0041](0041-perf-ci-switch.md)).
