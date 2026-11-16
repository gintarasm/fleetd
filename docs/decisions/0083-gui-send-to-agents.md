# 0083 — GUI: send text to one or many agents

**Status:** accepted · 2026-11-16 (issue #95, part of #26)

Builds on [0043](0043-gui-command-palette.md) (the fuzzy picker), [0042](0042-gui-agent-status.md) (agent status), [0060](0060-gui-sidebar-filter.md) (the sidebar), [0080](0080-palette-lists-every-action.md) (every action in the palette) and [0029](0029-gui-core-toolkit-free.md) (core stays toolkit-free). Overlaps [0034](0034-agent-communication.md): this is plain typed input, so it works with any agent today.

## Context

`fleet send` types into an agent without attaching. The GUI could only type into the focused pane. With several agents on related work you often want to say the same thing to all of them ("rebase on main", "stop and summarise"), or answer one without bringing it into a pane.

## Decision

- **Send to Agents… (`SendToAgents`, ⇧⌘M).** It is also in the palette, which shows it only while an agent is running (`PaletteState::any_running`), and in the fleet menu. ⇧⌘M was free: ⌘M is the system's Minimize, and the shifted key has no binding. The command opens a sheet with:
  - a multi-line **message** box, in which Return types a line break;
  - the **running agents**, newest first, as a checkbox list. It is the fuzzy picker in a new `PickerView::multi` mode. The filter matches the name, folder, id and labels (`role=reviewer`). Each row shows `#id · folder · status`. Enter or a click checks or unchecks the highlighted row;
  - **Press Return after**, on by default.

  ⌘Return or Send sends. Escape cancels. Tab moves between the message and the filter.
- **The same input as `fleet send`.** The text goes on the control connection as `SendInput` (`Session::send_input`, a tracked `send` so a refusal shows as "send failed: …"). Trailing line breaks are dropped, because Return is the checkbox's job. The Return (`\r`) goes as its own write 150 ms later (`RETURN_DELAY_MS`, the CLI's `--enter` delay). TUIs read text and Return that arrive together as a paste and leave the text unsent. With no text, the Return goes at once.
- **Bracketed paste only where it is known.** A multi-line text goes as one bracketed paste (`encode_paste`, as the pane's ⌘V) when a pane showing that agent says it turned bracketed paste on. Otherwise it goes as typed, like `fleet send`. The daemon does not report terminal modes, and attaching to every target only to learn them would cost more than it saves.
- **Warn before typing into a working agent.** If a target's ADR 0042 status is Working, Send shows "*name* is working: the text lands mid-turn. Send anyway?" and turns into **Send Anyway**. The next Send sends. Changing which agents are checked withdraws the warning. A target that starts working after the warning gets a new one. Targets that ended while the sheet was open are dropped before sending.
- **Sidebar multi-select.** ⌘-click toggles a row. The first ⌘-click also adds the focused pane's row, as Finder does with the selected row. ⇧-click adds the run of rows from the last row clicked, in the order drawn. A plain click clears the selection and shows the agent, as before. Escape in the sidebar clears the selection. Picked rows get an accent tint. A row that is gone leaves the selection.
  - A running row's menu has **Send to Agent…**, which checks that agent. When the row is part of a selection of several, it has **Send to Selected…** instead, which checks every picked row that is running.
  - ⇧⌘M checks the selection too.
- **Logic in the core** (`core::broadcast`): the picker rows (`send_items`), the sheet's state and its Send outcome (`SendForm::submit` → `Nothing` / `Warn` / `Send(SendPlan)`), the warning text, the bytes (`encode_text`) and the sidebar selection (`RowSelection`). Each has unit tests. `core::text_edit` gains a multi-line mode (`TextEdit::multiline`, plus `LineStart`/`LineEnd`; ⌘⌫ and ⌃K stop at the line's ends). `TextField::multiline` draws it wrapped at the field's width, with ↑/↓ by row and vertical scrolling.

## Consequences

- No protocol change: the sheet sends the same `SendInput` requests as `fleet send`.
- A multi-line message to an agent that no pane shows goes as typed. An agent TUI that treats a bare `\n` as Return submits at the first line break. Show the agent in a pane once, or send one line at a time.
- The closed sheet costs one `Option` check per frame. An empty sidebar selection costs a `retain` on an empty list. The sidebar's per-row cost is one `contains` on the selection.
- The palette keeps its rule from ADR 0080: `SendToAgents` has a `Command` and is bound.
