# 0081 — Drop files and paste images into an agent

**Status:** accepted · 2026-11-16 (issue #93, part of #26)

Builds on [0059](0059-gui-drag-and-drop.md) (dragging agents and panes), [0046](0046-gui-clickable-links.md) (only a local agent's paths mean anything here) and [0029](0029-gui-core-toolkit-free.md) (core stays toolkit-free).

## Context

Agents like Claude Code take file and image paths as input: screenshots, logs, specs. Getting one into a pane meant typing the path. Terminals let you drop a file from Finder to type its path, and Claude Code turns a pasted image path into an attachment.

## Decision

- **Drop on a pane.** Files dropped from Finder onto a pane are typed into its agent as their shell-quoted paths, space-separated, with no trailing space, through the same path as ⌘V (`encode_paste`), so they are bracketed when the agent turned bracketed paste on. The pane gets the keyboard. Nothing happens on a pane with no agent, or one whose agent exited or is lost.
- **Drop on a sidebar row.** The paths go to that row's agent. If a pane in the shown tab shows the agent (the focused pane first), they go through that pane, bracketed as the agent asked. Otherwise they go on the control connection through the path Send to Agents… uses ([0083](0083-gui-send-to-agents.md): `SessionApi::send_input`, tracked so a refusal shows an error), without brackets, because no pane holds the agent's modes. The layout does not change.
- **Quoting** (`core::drop_paste::shell_quote`). A path made only of `[A-Za-z0-9_@%+=:,./-]` stays bare. Anything else goes in single quotes, with `'` written as `'\''`. A path with a control character uses `$'…'` with escapes, so a newline in a file name never reaches the agent as Return.
- **Paste an image.** ⌘V uses the clipboard's text when it has any (gpui reads text first). With only an image, the image is written to a new file `$TMPDIR/fleet-paste-<unix ms>.<ext>`, created with `create_new`; if the name is taken, `-1`, `-2` and so on are added. The file's quoted path is then pasted. The write runs on the background executor, and the paste follows on the UI thread. Each format gpui reads (PNG, JPEG, WebP, GIF, SVG, BMP, TIFF) keeps its own extension. The files stay in `$TMPDIR` for macOS to clean up, because the agent may read one long after the paste.
- **Remote targets.** On an SSH target the paths would name files on this Mac, not on the agent's host, so nothing is typed. Image paste shows "Image paste needs a local fleetd" in the neutral toast. A drop on a pane or row shows "Dropping files needs a local fleetd". Uploading through the node was considered and left for later: it needs a new request, size limits and a place on the remote host. `PaneChrome::local` (the target is `Target::Local`) tells the pane which case applies. A failed image save shows "Could not save the pasted image".
- **Feedback.** While files are dragged over a pane, it shows the ADR 0059 highlight: an accent outline with a light accent tint, and the tint is left off under Reduce Transparency. A sidebar row is tinted with the accent. The pane's highlight is a child that exists only while gpui has an active drag, so a frame with no drag builds nothing more.
- **Separate payloads.** External drops use gpui's own drag (`ExternalPaths` with `on_drop` and `drag_over`), which macOS file drags feed. Agent and pane drags (ADR 0059) keep their own pointer tracking and never set gpui's active drag, so neither kind sees the other. A file drag arrives as pointer moves with no press, so it cannot start an agent or pane drag either.
- **Toasts.** Panes raise these messages as `PaneEvent::Notice`, and the workspace puts them in the neutral toast (`AppState::show_notice`).
- No new keymap action, so the palette (ADR 0080) is unchanged.

## Consequences

- Shell quoting suits shells and agents alike. Claude Code accepts a quoted path, including one with spaces.
- A row drop to an agent no pane shows is unbracketed. This is safe, because the quoted text has no control characters, but an agent that recognises attachments only inside a bracketed paste will see plain typed text there.
- Pasted images build up in `$TMPDIR` until the system clears it.
- gpui cannot build `ExternalPaths` outside its own crate. Tests drive a real drop with an empty path list, which checks the highlight and the focus, and call the typing step (`PaneView::type_paths`, `AppState::drop_paths_on_agent`) with paths directly.
