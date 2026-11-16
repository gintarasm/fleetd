# 0086 — Save, open and copy an agent's output from the GUI

**Status:** accepted · 2026-11-16 (issue #98)

Builds on [0047](0047-gui-kill-from-pane-and-row.md) (pane and row menus), [0058](0058-ended-pane-summary.md) (Copy last output) and [0070](0070-remove-exited-agents.md) (Remove Exited Agents).

## Context

An agent's output was reachable only in its pane or with `fleet history` in a terminal. The pane holds the local scrollback only; fleetd keeps the whole history, also for exited agents until they are removed. The owner wants to save it, open it in an editor and copy the screen, and a way to keep it before Remove deletes it.

## Decision

- **One plain-text rendering.** `fleet history`'s reading (read-only attach, `ReadHistory` pages, one retry when the history is cleared mid-read) and its `--plain` rendering moved from `fleet-cli` to `fleet_client::history` (`read`, `Transcript::plain_lines`/`plain_text`). The CLI and the GUI both call it, so a saved file is exactly `fleet history --plain`: history lines without colours or escapes, trailing spaces trimmed, then the screen rows. `fleet-client` now depends on `fleet-term` for that.
- **Its own connection, off the UI thread.** `Session::read_output` returns a blocking job that connects to the session's target as `gui-output`, reads, and closes; the GUI runs it on gpui's background executor. Panes and the control connection are not involved, and nothing is held between saves.
- **Save Output…** (⇧⌘S for the focused pane, the pane menu, the row menu, the palette) opens the native save panel (`prompt_for_new_path`) at the folder of the last save, else `~/Downloads`, suggesting `<agent-name>-<YYYY-MM-DD>.txt` (local date; the name label, else the program; `/`, `:` and control characters become `-`). The read starts once a path is chosen; the outcome is a neutral toast ("Saved output to ~/Downloads/otter-2026-11-16.txt") or the error toast. Exited and lost agents work as long as fleetd still has them.
- **Open Output in Editor** (pane menu, row menu, palette; no shortcut) writes the same text to `$TMPDIR/fleet-output/<agent-name>-<date>.txt`, replacing an older copy, and runs `open` on it (the default app for `.txt`). gpui's `open_with_system` would do the same but has no test double; `open` sits behind a global the tests replace.
- **Copy Last Output** (pane menu, palette) copies the pane's screen, the same text the ended summary's "Copy last output" copies (`core::ended::last_output`), for running agents too.
- **Remove Exited Agents… gets "Save Output First…".** The question now offers Remove, Save Output First… and Cancel. Save Output First asks for a folder, saves each ended agent's output there (numbered `-2`, `-3`… when names collide, never overwriting), and then forgets exactly the agents whose output was saved (`ForgetAgents` by id). One that could not be saved stays, and the error toast says so.
- The single-agent Remove (row menu, header, summary bar) keeps having no confirmation (0047, 0058): Save Output… sits next to it in the same menus.

## Consequences

- Reading a long history renders every line on the background thread and holds the whole text in memory once; a multi-megabyte history is fine, a pathological one is as slow as `fleet history --plain`.
- For an ssh target the file is written on this Mac; the read goes over a second ssh connection.
- `fleet-cli` lost its direct `bytes` and `fleet-term` dependencies (`bytes` stays a dev-dependency).
