# 0057 — GUI sidebar row preview: last line, run time, exit code

**Status:** accepted · 2026-11-16 (M2d, issue #41)

Builds on [0029](0029-gui-core-toolkit-free.md) (toolkit-free core) and [0042](0042-gui-agent-status.md) (status dots).

## Context

A sidebar row showed the status dot, the name and the age of the last output, with the last line under it cut by gpui's `truncate()`. The issue asks for the last line dimmed and truncated with `core/truncate.rs`, the run time of running agents and the exit code of ended ones on the right, and a redraw of at most once a second per row. `AgentActivity` arrives up to every 250 ms per printing agent, and each one redrew the whole window.

## Decision

- **Second line:** `core::row_preview::preview_line`: the last non-blank line, control characters dropped, whitespace runs shown as one space; drawn dimmed and cut at its end by `truncate::head_end` (head + `…`, no space before it), through `head_text`, the head-keeping twin of `tail_text`.
- **Right side** (`row_meta`): the run time while running (`42s`, `12m`, `1h 5m`, `2d 3h`, from `created_unix_ms`; fleetd sends no exit time, so ended agents show no duration), `exit N` once exited (danger colour if non-zero), `exited` with no code, `lost`. It replaces the last-output age, which the status dot (working or quiet) now conveys, and the `✕code` next to the failed dot, so the code is shown once.
- **Redraw:** when an agent list differs from the shown one only in activity (`only_activity_changed`: same agents, same order, same info), the app state takes the new rows (statuses read them) but does not notify; the sidebar's existing one-second tick draws them. A change of state, labels or status still redraws at once.

## Consequences

- A row's text can lag its agent's output by up to a second; the dot's working/quiet change is not delayed.
- Fewer window redraws while agents print, which can only help `input_to_render_ns` (not measured in this batch: PERF_CI off).
