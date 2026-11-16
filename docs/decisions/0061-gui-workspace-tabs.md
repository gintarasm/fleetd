# 0061 — Workspace tabs: a layout per tab, ⌘⌥ keys

**Status:** accepted · 2026-11-16 (issue #43, M2d; keys decided by the owner on the issue)

## Context

A layout holds at most four panes (`core/layout.rs` `MAX_PANES`). The owner wants more agents on screen over a working day, kept apart by project: tabs in the titlebar, each with its own split layout, saved per target like the layout is today. The obvious keys were taken: ⌘T is New terminal ([0038](0038-gui-new-terminal.md)), ⌘1…⌘4 focus panes, ⌘W closes a pane, and ⌘⇧T is planned for reopening a closed pane (#75).

## Decision

- **Model** (`core::tabs`, toolkit-free, [0029](0029-gui-core-toolkit-free.md)). `Tabs` is a non-empty list of tabs and the one shown. A tab is a `Layout` (with its own split ratios, [0045](0045-gui-resizable-splits.md)) and its focused pane, which it keeps while hidden and gets back when shown. At most `MAX_TABS` = 9 tabs, so each has a key.
- **Pane ids are unique across tabs.** A pane id names one terminal connection in the session, whichever tab holds it: a split or a new tab takes the lowest id no tab has handed out yet. Ids of a closed tab may be handed out again, as a closed last pane's already is.
- **Hidden tabs stay attached.** Every tab's panes stay open and follow their agents (restarts, removals, list updates), so switching is instant and shows current output. Only the shown tab's panes are drawn; a hidden pane keeps its last size.
- **Tab operations.** New tab: one empty pane, right of the shown tab, shown and focused. Close tab: detaches its agents (never kills them; ⌘⇧T reopens them one by one, the last pane first) and shows the tab to its right, else to its left; the last tab stays (Close tab does nothing). Next and previous wrap around. Switching tabs restores the split view (no pane stays maximized).
- **Keys** (the owner's choice, on #43):

  | Action | Key |
  |---|---|
  | New tab | ⌘⌥T |
  | Close tab | ⌘⌥W |
  | Next tab | ⌘⇧] |
  | Previous tab | ⌘⇧[ |
  | Go to tab N | ⌘⌥1…⌘⌥9 |

  Unchanged: ⌘T New terminal, ⌘1…⌘4 focus a pane *of the shown tab*, ⌘W close pane, ⌘⇧T reopen closed pane (#75). ⌘⇧] and ⌘⇧[ are also bound as `cmd-}` and `cmd-{`, the spelling macOS may report. A new Window menu lists New Tab, Close Tab, Show Next Tab and Show Previous Tab with their keys.
- **Strip.** With two or more tabs, the bar shows a chip per tab between the fleetd block and the bar's buttons: the tab's number and the program its focused pane shows, highlighted when shown, with a ✕ to close it; then a New tab button. With one tab the bar is as before. The strip neither animates nor is translucent. A switched-to tab's panes ease in like any pane that starts being drawn ([0065](0065-gui-motion-and-feedback.md)), so Reduce Motion turns that off.
- **Persistence.** `GuiState` gains `tabs`, keyed by target like `layouts`: the tabs' saved layouts and the shown tab's index, saved with the usual debounce. On load `tabs` wins; a target with only an old `layouts` entry opens as one tab with it. `layouts` keeps being written with the shown tab's layout, so older builds still open what was on screen. A saved tab reusing an earlier tab's pane id is dropped, at most nine are read, an unknown index shows the first tab, and each tab focuses its first pane (as the layout did).

## Consequences

- The sidebar marks agents in the shown tab only; an agent shown in a hidden tab looks unshown, and clicking it shows it in the focused pane as before.
- More tabs mean more attached terminals: each costs its connection and grid even while hidden.
- Pane-level features act on the shown tab: drag and drop ([0059](0059-gui-drag-and-drop.md)) splits and moves within it (a split taking a fresh unique id), the split chooser follows its pane across tab switches, ⌃Tab's "on screen" and the sidebar marks mean the shown tab, and a bell is seen when its pane is focused in the shown tab.
- Moving a pane between tabs is left to drag and drop (#34).
