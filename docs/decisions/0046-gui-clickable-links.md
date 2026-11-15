# 0046 — Clickable URLs and file paths in panes

**Status:** accepted · 2026-11-16 (issue #37)

Builds on [0029](0029-gui-core-toolkit-free.md) and [0031](0031-native-look.md).

## Context

Agents print PR URLs and `path:line` locations constantly. The owner wants to open them from the pane, the way iTerm and Ghostty do, without losing plain clicks to the selection or to an agent's mouse mode.

## Decision

- **Gesture.** Holding ⌘ over a pane underlines the link under the pointer (accent colour, pointing-hand cursor); ⌘-click opens it. Without ⌘ nothing changes. ⌘ wins over an agent's mouse reporting, so links work in full-screen agents too.
- **What is a link.** In this order, on the hovered cell only:
  1. An OSC 8 hyperlink on that cell (`fleet_term::Terminal::hyperlink_at`). `http(s):` and `mailto:` open as URLs, `file:` URIs as files; other schemes are ignored.
  2. An `http://` or `https://` URL in the text, minus trailing punctuation and unbalanced closing brackets.
  3. A path, `path`, `path:line` or `path:line:col`, containing `/` or `.`, relative to the agent's recorded cwd (or absolute, or `~/`), and naming an existing regular file. git's `a/` and `b/` prefixes are tried off too.
- **Local only for files.** File paths are linked only when the GUI is connected to the local fleetd: a remote agent's paths are not on this machine. URLs work everywhere.
- **Opening.** URLs go to the system (`open_url`, the default browser). Files open in the editor named by `[gui] editor` in `config.toml`, else `$VISUAL`, else `$EDITOR`. Known GUI editors get the line and column (`code -g`, `zed path:l:c`, `subl`, `mate -l`, JetBrains `--line`, `emacsclient +l:c`, `bbedit +l`); unknown ones get the file. With no editor, or a terminal editor (vim, nvim, hx, nano, …) that would have no terminal, the file opens in the default text editor (`open -t`).
- **Core.** Detection and the editor command line live in `src/core/links.rs` (toolkit-free); the pane view only tracks ⌘ and the pointer, draws the underline and runs the command.

## Consequences

- Detection costs one row scan and at most three `stat`s per pointer move with ⌘ held; nothing without ⌘, nothing while drawing.
- A link is one row: a URL soft-wrapped onto the next row is cut at the row's end (rows carry no WRAPLINE marker, [0005](0005-terminal-state-on-node.md)).
- OSC 8 links exist only on cells written live: a snapshot (reattach) and history lines carry no hyperlinks, so there only text URLs and paths are found.
- Relative paths resolve against the cwd the agent was spawned in; a shell that later `cd`s elsewhere resolves against the old directory.
- Opening a file in a terminal editor inside a new fleet pane is a possible follow-up.
