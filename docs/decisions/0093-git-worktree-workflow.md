# 0093 — The built-in `git-worktree` workflow and `fleet wt`

**Status:** accepted · 2026-11-23 (M4 workstream C, issue #163; design in [the M4 spec](../superpowers/specs/2026-11-23-m4-workflows-design.md) §3, with the user's answers in #166)

**Builds on:** [0090](0090-m4-workflows.md) (M4), [0091](0091-node-primitives.md) (`Exec`, `ReadFile`, `ListDir`), [0092](0092-workflow-host-and-run-action.md) (the workflow host, `RunAction`, owned label prefixes), [0010](0010-storage-and-layout.md) (worktree layout and archive rule), [0025](0025-core-knows-agents-workflows-own-hierarchy.md) (the core has no git knowledge), [0072](0072-agent-identity-by-session.md) (scope), [0085](0085-gui-git-branch.md) (the repository's name).

**Amends:** [0010](0010-storage-and-layout.md): the root and branch prefix are configurable per repository; archive's "unpushed" means commits on no remote branch *and* not merged into the base. [0092](0092-workflow-host-and-run-action.md): the daemon's `NodeHost` now calls the node primitives (its "unavailable" stubs, `TODO(#161)`, are gone), behind the same `Checked` capability checks.

## Context

The overseer runs each worker in its own worktree by hand: `git worktree add`, `fleet spawn --cwd`, `git diff` in its own shell to review, and a cleanup script that deletes merged worktrees (and their 8–18 GB `target/`). The spec put those steps behind five actions of one built-in workflow, called the same way from the CLI, the GUI (workstream D, ADR 0094) and agents. It left to this workstream the exact params, how the workflow finds an agent's worktree, the refusal rules in detail, the caps' behaviour, and the CLI's output.

## Decision

### Shape

- **Native Rust in `fleet-workflow::git_worktree`**, registered in `fleetd`'s static built-in list (built on first `ListActions`/`RunAction`, never at startup). Manifest: `exec:git`, `fs:read:*`, `spawn`, `labels:git.`, `storage:own`; the host holds it to these like any plugin.
- **All git is `git` through `host.exec`**, with `GIT_TERMINAL_PROMPT=0`, `LC_ALL=C` and `-c core.quotepath=off` always, and `--no-optional-locks` + `GIT_OPTIONAL_LOCKS=0` for reads, so a diff or `ls` never blocks an agent's own git. Diffs also pass `--no-ext-diff --no-color --no-relative --src-prefix=a/ --dst-prefix=b/`, so a user's `diff.*` config cannot change the output the parser reads. The core gains no git calls.
- **Output parsing is pure** (`parse.rs`): `worktree list --porcelain -z`, `diff --numstat -z`, NUL lists, and unified diffs (quoted paths, renames, copies, mode and type changes, binaries, `\ No newline`), unit-tested on fixtures captured from real git.
- **Config** is `[workflows.git-worktree]` in the node's `config.toml` (`<state_dir>/config.toml`, i.e. `~/.fleet/config.toml`), read each time an action runs, with `[workflows.git-worktree.repos.<name>]` overriding per repository: `root` (default `~/.fleet/worktrees/{repo}`; `~` and `{repo}` expanded; a relative root is relative to the main checkout), `branch_prefix` (default `fleet/`), `fetch_base` (default `true`). An unknown key or a wrong type is an error naming it. The daemon hands the workflow a settings source (the file's text and `$HOME`); the workflow never reads the file system except through the host.
- **The repository** is any checkout's `git rev-parse --path-format=absolute --git-common-dir` without `/.git` (a bare repository is refused); its name is that folder's. `repo` defaults to the caller's cwd (`RunAction.cwd`).
- **Labels** `git.repo`, `git.worktree`, `git.base` (main checkout, worktree path, base ref) are set on agents the workflow starts, through the owned-prefix spawn (0092), so they replace inherited ones for an agent caller. The branch is not a label.
- **Storage**: `wt/<path>` → JSON `{repo, branch, base, created_ms, created_by, agent}` in `workflow_kv`, so `prune` touches only worktrees the workflow made and archive knows the base.

### Actions (names and params are what clients call; they do not change)

| Action | Params | Callers | Destructive | Target |
|---|---|---|---|---|
| `git-worktree.new-agent` | `slug`* (string), `command`* (argv), `repo` (path), `base`, `branch`, `name` (strings), `labels`, `reuse` (bool), `shared` (bool) | user, agents | no | — |
| `git-worktree.diff` | `agent` or `path`, `against` (`base` default / `uncommitted` / `committed`), `file` (string), `context` (int, default 3) | user, agents | no | `agent` |
| `git-worktree.archive` | `agent` or `path`, `force` (bool, **user-only**), `keep_branch` (bool) | user, agents | yes | `agent` |
| `git-worktree.ls` | `repo` | user, agents | no | — |
| `git-worktree.prune` | `repo`, `dry_run` (bool) | **user** | yes | — |

`shared` is added to the spec's list because `new-agent --reuse` into a worktree with a running agent is refused with `retry: {reuse: true, shared: true}`, and a retry may only name params the action has.

**`new-agent`:** validate the slug (`[A-Za-z0-9._-]`, ≤ 64, not `.`/`..`) before anything runs; resolve the repository and `root/slug`; `check_spawn` (a quota refusal leaves nothing behind); if the path is already a worktree of the repository: `refused` with `retry {reuse}`, or with `reuse`, refused while an agent runs there unless `shared`, else spawn into it with its recorded base. Otherwise: the base is `base`, else `origin/HEAD`'s branch, else the main checkout's branch; with `fetch_base` and a base of the form `<remote>/<branch>`, `git fetch --quiet --no-tags <remote> <branch>` with a 30 s timeout, and on failure the local ref is used and the result's message says so. A branch that already exists is an error naming it and where it is checked out. `git worktree add --no-track -b <branch> <path> <base>` (no upstream, so a `git push` from the worktree never targets the base branch); git's own lock or an existing folder makes a concurrent second call fail as "slug in use". Record, then spawn (`cwd` = the worktree, `name`, the caller's labels, `git.*`). If the record or spawn fails (approval denied or expired, quota raced, cancelled), remove the worktree (`--force`, it holds no work) and delete the branch and record, then return the error.

**`diff`:** the worktree is the agent's `git.worktree` label, else its cwd's `--show-toplevel`, or `path`'s. The base is the agent's `git.base`, else the record's, else the default branch. `base`: working tree (untracked included) against `merge-base(base, HEAD)`; `uncommitted`: against `HEAD`; `committed`: merge base against `HEAD`. Untracked files (`ls-files --others --exclude-standard -z`) are read with `ReadFile` (≤ 8 MiB each) as added files; a NUL in the first 8 KiB makes one binary. Files are sorted by path. **Caps:** hunks for at most 2,000 files, 50,000 lines and 8 MiB of hunk text; from the first file over a cap on, every remaining file keeps its stats only (`hunks_omitted`, `truncated` on the view) — the rest, not just the ones that would not fit, so the client sees one clear cut. With `file`, the line cap does not apply (8 MiB does), so asking for an omitted file always returns its hunks if it fits a frame. If git's output passes the `Exec` cap (15 MiB), the cut file loses its hunks and `diff --numstat` fills in the files the patch did not reach. At most 20,000 files are listed. Title `<worktree folder> · <branch>`, from `"<base> (merge base <short>)"` or `"HEAD (<short>)"`, to `working tree` or `HEAD`.

**`archive`:** never the main checkout. **Always refused, even with `force`**, while an agent in the caller's view runs with its cwd inside the worktree or with its `git.worktree` label (`refused` with no retry: kill it first); an agent caller is also refused when its own cwd (`RunAction.cwd`) is inside. Otherwise refused with `retry {force: true}` (no retry for an agent caller, which may not set `force`) when `git status --porcelain` shows uncommitted changes or untracked files that are not ignored (ignored files such as `target/` go with the worktree), or when `rev-list HEAD --not --remotes <base>` has commits (on no remote branch and not merged into the base). Then `git worktree remove [--force]`; the branch is deleted with `branch -D` when `HEAD` is an ancestor of the base (our own check: `branch -d` would judge against the main checkout's `HEAD`, not the base) or with `force`, kept with `keep_branch` or when unmerged (a pushed, unmerged branch stays); ended agents of the worktree are forgotten; the record goes. An agent may archive through `agent` (held to D(a) by the host) or a `path` it made (`created_by` is it) or that one of D(a) was started for.

**`prune`** (user only): for each recorded worktree of the repository (records only: hand-made worktrees are listed by `ls`, never pruned), after fetching each distinct base once when `fetch_base`: dropped record if git no longer knows the worktree; kept if its folder is missing (`prunable`), an agent runs in it, it is dirty, or `HEAD` is not in the base; else removed as `archive` without `force`. `dry_run` changes nothing. The result is a table: path, branch, base, result (`remove`, `removed (…)`, `keep: <why>`, `gone`).

**`ls`:** a table of every non-bare worktree (main first, as git lists them): path, branch (or `detached <short>`), base (record, else an agent's `git.base`, else the default branch; empty for the main checkout), ahead/behind (`+a -b` from `rev-list --left-right --count base...HEAD`), dirty (`yes`/`no`/`missing`), merged, agents (`name#id`). Each agent belongs to the worktree it was started for, else the deepest one holding its cwd; a row links its running agent (else its first) and its path.

### CLI: `fleet wt` (alias `fleet worktree`)

`new <slug> [--repo D] [--base REF] [--branch B] [--name N] [--label k=v]… [--reuse [--shared]] [--json] -- <program> [args…]` prints the agent's id (the message on stderr) or JSON; `ls [--repo D] [--json]` prints aligned columns; `diff [<agent>|--path D] [--against …] [--stat] [--file F] [-U N] [--json]` prints a unified diff, coloured on a terminal (`NO_COLOR` respected; no pager), or `--stat` (git's shape: `path | n +++--`, `Bin`, and a summary); with no agent or path it is the current folder's worktree. `archive <agent>|--path D [--force] [--keep-branch]`, `prune [--repo D] [--dry-run]`. Paths are made absolute against the shell's cwd. A refusal prints the reason and the flags to add (`--force`, `--reuse --shared`). Exit codes as M3: 0 ok, 1 refused or not found (no such agent, not in a git checkout), 2 usage or other error, 3 forbidden. `force` is sent only when given, since an agent setting it at all is forbidden.

### Cost

Nothing at startup or idle: no thread, timer or watcher; the registry is built on first use. Every git call is an `Exec` (at most 8 at once per node, 0091), off the PTY path. `ls` runs about three git reads per worktree, one after another. New instr metric `diff_view_build_10k_lines_instructions`: parsing a fixed 10,000-line unified diff (50 files) into a capped `DiffView` and encoding it; no baseline until the user records one.

## Consequences

- The overseer's loop becomes `fleet wt new … -- claude …`, `fleet wt diff <agent> --stat`, `fleet wt prune --dry-run && fleet wt prune`.
- An agent caller sees only its descendants (0072), so archive's running-agent check cannot see a running agent outside D(a) in the same worktree (other than the caller itself, through its cwd). Such sharing needs `shared`, which only the user or the agent that owns the worktree would set; the user's own archive sees every agent.
- A root that resolves through a symlink yields paths that differ from git's real paths; the workflow compares paths as written. Keep roots free of symlinks (the default is).
- Not done: clone and fetch-back (M6), a per-repo setup hook (#166 answer 6), special treatment of lock or generated files in diffs (#166 answer 7), parallel git calls in `ls`.
