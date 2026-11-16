# 0045 — Resizable splits: a ratio per split, saved with the layout

**Status:** accepted · 2026-11-16 (issue #35, M2d)

## Context

Every split in the GUI's pane tree was fixed at 50/50 (`core/layout.rs`). The owner wants to give one agent more room than its neighbour, and have that survive a restart, like the layout itself already does per target.

## Decision

- **Model.** Each split node in `core::layout` carries a `ratio`: the first side's share of the split's extent along its axis. A new split, and a reset one, is even (0.5). Panes are still laid out from the tree alone; there are no pixel sizes in the model.
- **Dividers.** `Layout::dividers(w, h)` lists each split's line in tree order (`SplitId` is that index; it is only held while a drag lasts). `move_divider` puts a divider at a position along its axis; `reset_divider` evens it. Both are toolkit-free ([0029](0029-gui-core-toolkit-free.md)).
- **Minimum pane size.** A move is clamped so every pane on each side keeps `chrome::PANE_MIN` (120 px, gap included) along the axis. A side's need counts its own panes: two panes side by side behind the divider need twice the minimum, two stacked ones need it once, and nested ratios are kept, not squeezed. A split too small to honour the minimum on both sides does not move. Shrinking the window can still make panes smaller than the minimum; the ratios are kept.
- **Shell.** Each divider gets a hit strip `PANE_GAP + 4` px wide, centred on the gap between cards, with a left-right or up-down resize cursor. Dragging moves the divider by the pointer's travel (no jump to where it was grabbed); a double-click resets it. No dividers are drawn while a pane is maximized. There is no animation, so Reduce Motion and Reduce Transparency have nothing to change.
- **Persistence.** `SavedNode::Split` gains `ratio`, saved through the existing per-target `layouts` in the GUI state with the usual debounce. Layouts saved before this read as even (`#[serde(default)]`); a saved ratio that is not finite or not strictly inside `0.01..=0.99` reads as even. Older builds ignore the field.

## Consequences

- Reading order (pane numbers, ⌘1–⌘4) and arrow-key focus follow where panes sit, so they can change as dividers move; that is intended.
- Every drag step resizes the panes' terminals, as a window resize does.
