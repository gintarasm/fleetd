# 0071 — Kill ends shells: SIGHUP, SIGTERM, then SIGKILL; a shell is called a shell

**Status:** accepted · 2026-11-16

## Context

A New terminal (issue #21) is an ordinary agent labelled `kind=shell`. Killing one did nothing: `Node::kill` sent only SIGTERM to the process group, and an interactive shell (zsh, bash, sh) ignores SIGTERM by design. Checked on macOS: `zsh -f -i` and `bash --norc -i` in a PTY as session leaders both survived `killpg(SIGTERM)` and exited on `killpg(SIGHUP)`. Kill had no fallback, so any agent that traps SIGTERM could not be killed either.

The GUI also called the shell an agent: "Kill agent 4 (zsh, ~/src)?", "Kill Agent…", "Rename agent".

## Decision

- **Kill sends SIGHUP, then SIGTERM, to the process group.** SIGHUP is what closing a terminal window sends, so shells (and the jobs they own) exit at once. SIGTERM is still sent for programs that only handle that one.
- **After `KILL_GRACE` (2 s) a still-running agent is SIGKILLed.** A short-lived thread per kill waits out the grace, checks the agent has not exited, and calls `force_kill`, which does nothing for an exited agent (its pid may have been reused). Kill still replies straight away; the exit arrives as usual.
- This lives in the node, so `fleet kill`, the GUI and the agent skill all get it.
- **The GUI names a `kind=shell` row a shell.** `core::status::Subject` (`Agent` | `Shell`, from the label) feeds the kill question ("Kill shell 4 (zsh, ~/src)?"), the pane menu ("Rename Shell…", "Restart Shell", "Kill Shell…") and the rename sheet ("Rename shell"). The node and protocol are unchanged: a shell is still an agent on the wire and keeps its label.

## Consequences

- An agent that cleans up on SIGTERM gets SIGHUP first. Most CLIs treat the two the same; one that ignores SIGHUP still gets SIGTERM.
- An agent that needs more than 2 s to shut down after SIGTERM is cut short. Raise `KILL_GRACE` if a real agent needs it.
- Daemon shutdown (`Node::shutdown`) also goes through `AgentHost::kill`, so it now sends SIGHUP too. It already escalated to SIGKILL after `SHUTDOWN_GRACE`.
- Not covered: the ended-pane bar of a lost shell still says "The agent is gone".
