# 0085 — Git repository and branch per agent

**Status:** accepted · 2026-11-16 (M2d, issue #97)

Builds on [0022](0022-protocol-v2.md) (protocol v2, `AgentInfo`), [0025](0025-core-knows-agents-workflows-own-hierarchy.md) (sidebar groups by `project`, else cwd), [0029](0029-gui-core-toolkit-free.md) (toolkit-free GUI core), [0043](0043-gui-command-palette.md) / [0054](0054-gui-agent-switcher.md) (palette and switcher matching), [0057](0057-gui-row-preview.md) (row preview) and [0060](0060-gui-sidebar-filter.md) (sidebar filter and groups).

## Context

Many agents run in sibling git worktrees (`../fleetd-wt/issue-27`, `issue-28`, …). The sidebar and pane header show the cwd, cut to the same prefix, so agents are hard to tell apart and their branches are invisible. Issue #97 asks for the repository (or worktree) and branch in the pane header and as the row's subtitle, a group-by-repository option, branch names in the palette and switcher, a dirty marker if cheap, all read on the fleetd host (remote targets too) and never on the render path.

## Decision

### Where it is read: the node, from `.git` files

- The node reads it (`fleet_node::git`), so it is the cwd on the host the agent runs on, local or over ssh; the GUI never touches the file system for it.
- **Files, not `git`.** From the cwd, walk up (at most 64 levels) to the first `.git`:
  - a directory is a main checkout: the repository's name is the folder holding it;
  - a file is `gitdir: <path>` (relative to its folder): a linked worktree, whose git directory has `commondir` naming the shared one (the repository is the folder holding that `.git`, or a bare `name.git` without its suffix, and the worktree is the checkout's folder), or a submodule (no `commondir`: its own repository);
  - `HEAD` is `ref: refs/heads/<branch>` (other refs show without `refs/`), or a commit hash: detached, shown by its first 7 digits.
- That is a few `stat`s and two or three reads of at most 4 KiB. Spawning `git` costs a fork and exec per cwd (milliseconds, and a `git` on the node's `PATH`), for the same answer.
- **No dirty marker.** Telling whether a checkout is dirty means comparing the index with the working tree (a `stat` per tracked file, or `git status`): not cheap for big repositories, every few seconds per agent. Left out; a later change could add it on a slower timer.
- A cwd that is not absolute, is in no checkout, or whose `.git` cannot be read has none.

### When: cached per cwd, refreshed by listing

- `Node::list` (and spawn) fill `AgentInfo.git` from a cache keyed by cwd. An answer is reused for 3 s (`GIT_TTL`); after that the next list reads it again. Agents sharing a cwd share one read.
- The GUI already lists every 5 s (`RELIST`), so a branch switch shows within about 8 s. No timer or thread is added to the node: an idle daemon nobody lists reads nothing. The reads happen on the request's blocking thread, never on the PTY output path, the activity ticker, or the GUI's render path.
- Not "when the agent goes idle": the list's refresh is already slow and bounded, and tying reads to activity would put file reads next to the output path. The cache keeps at most 256 cwds before dropping stale ones.

### Protocol: one additive field

- `AgentInfo.git = 10`, a `GitInfo { repo, worktree, branch, detached }` message. `worktree` is empty for a main checkout.
- Additive, as `exited_unix_ms` was ([0058](0058-ended-pane-summary.md)): `PROTOCOL_VERSION` stays 2. An older GUI skips field 10; a newer GUI talking to an older node gets no `git` and shows the cwd as before. Not stored: it is read again after a daemon restart.

### GUI

- `core::git` turns `AgentInfo.git` into text; it reads no files.
- **Sidebar row:** the second line's left part is `worktree · branch` (or `repo · branch` in a main checkout; `detached 1a4a202` when detached) in place of `program · id`, cut with an ellipsis at 60 % of the sidebar so the last line keeps room. Rows with no checkout keep `program · id`. The pane header still shows `program · id`.
- **Pane header:** after `program · id`, `repo (worktree) · branch` (the worktree only when linked and named differently), then the path; the path gives way first.
- **Group by:** a right-click on a group header opens a native menu, *Group by Folder* (today: `project` label, else cwd) or *Group by Repository* (the repository's name, all its worktrees together; agents in no checkout group as by folder), the current one checked. Remembered for every target in `gui-state.toml` as `sidebar_group_by = "folder" | "repo"`; an unknown value reads as folder. Collapsed groups are remembered by title as before, so the two groupings keep separate collapse sets naturally.
- **Matching:** the sidebar filter's `query_matches` also searches repository, worktree and branch; the palette's and switcher's agent rows carry them in their keywords (so the fuzzy matcher finds `frzetimeout`), and the palette's agent detail shows the checkout.

## Consequences

- Agents in sibling worktrees are told apart by worktree and branch; grouping by repository puts them under one header.
- A branch switch shows up to ~8 s late; a renamed or deleted worktree reads as no checkout on the next list.
- An agent started under a home directory that is itself a git checkout (dotfiles) shows that checkout, as `git` would.
- Each list does a few small file reads per distinct cwd at most every 3 s; nothing is read while nobody lists. Not measured in this batch (PERF_CI off).
