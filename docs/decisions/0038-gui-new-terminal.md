# 0038 — New terminal: a plain shell labelled `kind=shell`

**Status:** accepted · 2026-11-15 (issue #21)

## Context

The owner sometimes needs a plain terminal next to the agents. `fleet spawn zsh` and the New agent sheet can start one, but it takes several steps. Later, the sidebar may want to show shells differently and status detection should skip them, so the GUI needs to mark them without the core interpreting anything ([0025](0025-core-knows-agents-workflows-own-hierarchy.md)).

## Decision

- **Action.** GUI "New terminal": ⌘T in the window (it was unbound) and an item under "New agent…" in the menu-bar menu.
- **Program.** Locally `$SHELL -l` (falling back to `/bin/zsh`). On an ssh target the GUI's `$SHELL` means nothing, so it runs `/bin/sh -c 'exec "${SHELL:-/bin/zsh}" -l'` and the remote shell is looked up there.
- **Folder.** The focused pane's agent's cwd, else the last-used folder for the target, else `$HOME` (for an ssh target, empty: the daemon's own directory).
- **Pane.** A new split when the focused pane shows an agent and one fits, else the focused pane; the pane is focused.
- **Label.** `kind=shell`. `kind` is a plain label, not a reserved `fleet.` key: the core stores it and never interprets it. Clients may read `kind` to style or skip agents; an absent `kind` means an ordinary agent.
- Otherwise a shell is a normal agent: restart, remove, history, and it survives a fleetd restart.

## Consequences

- Sidebar styling and status-detection skipping for `kind=shell` are follow-ups that read the label; nothing new in the protocol or the daemon.
