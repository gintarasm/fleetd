# 0065 — GUI motion and feedback polish

**Status:** accepted · 2026-11-16 (M2d, issue #44)

**Amends:** [0031](0031-native-look.md) §5 (motion), [0044](0044-gui-settings-window.md) (two new `[gui]` keys). Builds on [0029](0029-gui-core-toolkit-free.md).

## Context

ADR 0031 left motion partial: sheets and the toast slide in, nothing else moves. Issue #44 asks for small touches that make the window feel alive: panes easing in, the focus ring cross-fading, a focus mode that dims the other panes, feedback on copy, a toast when an agent exits, and an optional celebration for a success. All of it must be off under Reduce Motion, and none of it may draw frames while the window is idle.

## Decision

- **The decisions are in the core** (`core::feedback`, toolkit-free and unit-tested): a `Tween` (a start time and a length, eased with `style::motion::ease_out`), which panes are new (`entering`; none on the first frame), the ring's strength in a cross-fade (`ring`), when focus mode dims (`dims`, `dim_alpha`), the copy flash and glow curves, which agents just exited (`newly_exited`: running in the previous list, exited in this one; an agent first seen exited, or lost, is not news), whether to celebrate (`celebrates`: the setting is on and the code is 0), and the exit-toast queue (`ExitToasts`: one per agent, at most three, gone after 8 s or once the agent is restarted or removed).
- **No idle frames.** A tween runs only while it is moving: the view asks for the next frame (`Window::request_animation_frame`) only while one is running, and stops asking when it settles. Things that go away after a while (the "Copied" label, exit toasts) use one timer each, not a tick. The celebration's burst uses gpui's `with_animation`, which also stops asking for frames once it is done.
- **Pieces:**
  - *Panes ease in* when opened or split (and when shown again on restore from maximize): 180 ms, fading in while they grow from a 6 px inset. Not at launch.
  - *Focus ring cross-fade:* 150 ms; the old pane's ring fades out while the new one's fades in (the border blends from the plain border to the accent ring).
  - *Focus mode* (`[gui] focus_mode`, off by default): a pane without focus gets a veil of the window tint at 35 % when another pane is drawn beside it. The veil draws only, so clicks and keys still reach the pane. It fades with the focus ring.
  - *Copy feedback:* ⌘C (or Copy in the context menu) with a selection shows a small "Copied" label at the top of the pane for 900 ms (it fades over the last third), and the selection glows in the accent colour for 400 ms.
  - *Exit toast:* an agent that exits while the window watches gets a toast at the bottom centre, "<program> exited with code N", with Restart (as in the sidebar, panes showing it follow) and ✕.
  - *Celebration* (`[gui] celebrate`, off by default): an agent exiting with code 0 plays the system sound "Pop" (`NSSound`) and its toast shows a check, with six dots bursting out once (700 ms).
- **Reduce Motion** (`[gui] reduce_motion` or the system setting, ADR 0044) turns every movement off: panes appear, the ring and the dim switch at once, the label shows without fading, there is no glow and no burst, and toasts appear without sliding. What is information, not motion, stays: the dim, the "Copied" label, the exit toast and the check, and the sound (which has its own setting). The workspace keeps the resolved value in a gpui global (`controls::ReducedMotion`) that the panes read.
- **Settings:** Behaviour gains "Focus mode" and "Celebrate success" switches; both are off by default.

| key | type | default |
|---|---|---|
| `focus_mode` | bool | `false` |
| `celebrate` | bool | `false` |

## Consequences

- `PaneChrome` gains `dims`; `TerminalElement` gains `selection_glow`; `controls::appear` takes any element id (the exit toasts each need their own).
- Exit toasts and the pane's own exit banner (with its Restart) can both show for an agent in a pane. The toast is for agents that are not on screen, and it goes on its own.
- Performance: nothing is added to the idle path. While something moves, the window redraws at the display rate for at most 180 ms (a pane), 150 ms (focus), 900 ms (copy) or 700 ms (burst). The dim veil adds one quad per dimmed pane. Not measured (PERF_CI off, [0041](0041-perf-ci-switch.md)).
