# 0063 — Scroll feedback: overlay scrollbar and "New output" chip

**Status:** accepted · 2026-11-16 (issue #39)

Builds on [0029](0029-gui-core-toolkit-free.md) and [0031](0031-native-look.md).

## Context

Scrolled up in a busy pane, nothing showed where the view was in the history, and the only sign of new output was a "↓ N new lines" pill whose count kept changing. The owner wants a macOS-style overlay scrollbar and a calm chip.

## Decision

- **Overlay scrollbar.** A 6 pt rounded thumb along the pane's right edge, drawn over the text (no gutter, the grid keeps its width). Its length is the visible share of the loaded history plus the grid (at least 24 pt); its position is the view's place in it. Only loaded history counts: older pages are fetched as the view nears the top, as with the wheel, and the thumb then shrinks.
- **When it shows.** Only while scrolling: a scroll of the history (wheel or drag) fades it in (100 ms), it stays 800 ms after the last one and fades out (300 ms), easing out (`style::motion`). Output alone never shows it. The pointer over it keeps it; leaving it starts a new hold.
- **Dragging.** While it shows, a 14 pt strip on the right edge takes left presses: on the thumb it is dragged from where it was grabbed, elsewhere on the strip the thumb jumps under the pointer and drags from there. A hidden scrollbar takes nothing: the strip is the selection's again.
- **Reduce Motion.** It appears and disappears without the fades (shown for the hold, then gone).
- **"New output" chip.** "↓ New output" at the bottom middle of the pane while the view is scrolled up and output has scrolled lines into history below it since (`PaneModel::new_output`). A redraw in place (a spinner, a status line) adds no lines and shows no chip. Clicking it, or pressing End (unmodified) while scrolled up, returns to the live grid; End at the bottom still goes to the agent. Typing anything else returns to the bottom as before.
- **Core.** Thumb geometry, dragging back to an offset, the fade's timing and when End jumps live in `src/core/scroll_feedback.rs`; the clock is a parameter. The pane view owns the fade state, the element only paints.

## Consequences

- No per-frame work while idle: a pane that is not scrolling skips the scrollbar entirely (one `bool`). While it shows, the view redraws frame by frame only during the two fades; the hold is one timer. With Reduce Motion there are no animation frames at all.
- The count of new lines is gone from the chip; `PaneModel::unseen_lines` still has it.
- Output changing grid rows hidden below the view without scrolling a line into history (a TUI redrawing, a pane whose grid is not yet full) shows no chip.
- The thumb covers the last column's right edge while it shows; it fades within a second.
