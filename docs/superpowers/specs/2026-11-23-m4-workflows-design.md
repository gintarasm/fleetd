# M4 "Workflows" — design

**Date:** 2026-11-23 · **Status:** proposed (for the user's review before agents start)

**Sources:** [0025](../../decisions/0025-core-knows-agents-workflows-own-hierarchy.md) (the core knows nodes and agents; workflows own the hierarchy), [0007](../../decisions/0007-plugin-system.md) (plugins, declarative UI), [0010](../../decisions/0010-storage-and-layout.md) (worktree layout, archive rule), [0019](../../decisions/0019-control-plane-direction.md) and [0026](../../decisions/0026-node-providers-and-transports.md) (controller anywhere, providers), [0033](../../decisions/0033-satellite-milestone.md) / [0034](../../decisions/0034-agent-communication.md) / [0072](../../decisions/0072-agent-identity-by-session.md) (agents spawn agents, scope), [0085](../../decisions/0085-gui-git-branch.md) (`AgentInfo.git`), and [0090](../../decisions/0090-m4-workflows.md) (this PR). This spec does not re-open their decisions. It fills in what they left to the milestone and splits the work for parallel agents.

## Intent

The way the fleet already works by hand becomes a first-class workflow. Today a coordinating agent runs `git worktree add ../fleetd-wt/issue-N -b feat/x main`, then `fleet spawn --cwd ../fleetd-wt/issue-N claude …`, later reads the child's work with `git diff` in its own shell, and finally deletes the worktree (and its 8–18 GB `target/`) with a cleanup script that checks the branch is merged and clean. M4 puts each of those steps behind one action that every interface (CLI, GUI, an agent through `fleet`) calls the same way, and shows the user what an agent changed without leaving the app.

## Goals

1. **Node primitives:** `Exec`, `ReadFile` and `ListDir` requests on the node, for the user and for workflows, never for agents (0025).
2. **Workflows behind one interface:** `ListActions` and `RunAction`, with parameter schemas that clients turn into forms, and a workflow host that the M5 WASM plugin host can sit behind without changing callers.
3. **The built-in `git-worktree` workflow:** a new agent in a new worktree, the read-only diff, archive, list, and prune of merged worktrees.
4. **Scope:** agents can use the worktree workflow to start their children in worktrees, inside the M3 rules (parent, quotas, approval, descendants only).
5. **The diff view in the GUI:** what an agent changed, opened from its row, card or pane, read-only and fast on big diffs.
6. **The skill** teaches agents `fleet wt` instead of hand-rolled `git worktree` + `fleet spawn`.

## Non-goals

- **The WASM plugin host and third-party workflows** (M5). M4's workflow host is a Rust trait; only built-in, native workflows exist.
- **Remote nodes** (M6): workspace **clone** and **fetch-back** stay M6 actions of this workflow. M4 is designed so they slot in (primitives are node requests), but does not build them.
- Diff line comments to the agent (Later), side-by-side diff, syntax highlighting, editing in the diff view, staging or committing from fleetd.
- Non-git snapshot diffs (Later). The `DiffView` message is generic so a later workflow can fill it.
- The full declarative view tree of 0007 (lists, buttons, forms, key-value panels). M4 defines the three views it needs (diff, table, text) inside a `View` oneof that M5 extends.
- A per-repo setup hook (copy `.env`, prewarm a build) — see open question 6.

## Constraints

- **Protocol:** additive only; `PROTOCOL_VERSION` stays 2. New request kinds and messages in the field-number blocks reserved below. An old client skips them; a new client talking to an M3 daemon gets the usual unknown-request error and hides workflow UI.
- **Store:** `SCHEMA_VERSION` is 12 on `main`. Workstreams that add tables take the numbers reserved below; the later one to merge renumbers.
- **The core still has no git knowledge** (0025). `fleet-node` and `fleetd/src/server.rs` gain no git calls; everything git lives in the workflow crate and runs through `Exec`. (`fleet_node::git`, 0085, reads `.git` files for display only and stays as is.)
- **Perf (0014):** primitives and actions run off the PTY output path, on blocking threads or child processes, with bounded concurrency. Nothing new runs while idle: no timers, no watchers, no new threads at startup. While `PERF_CI=off`, agents run tests, clippy and fmt only and write "perf not measured (PERF_CI off)". They never touch baselines, `docs/perf-log.md` or `Perf-Approved`.
- **Frames:** `MAX_FRAME` is 16 MiB, so every reply below has a cap that fits one frame.
- **`fleet-gui/src/core` stays gpui-free:** diff layout, the action-form model and their tests live there.

## Design

### 1. Node primitives (workstream A, ADR 0091)

Three requests, answered on a blocking thread (like `ReadHistory`), never on the output path.

**`Exec`** runs a program, waits, and returns its output.

- `argv` (non-empty; `argv[0]` resolved on the node's login-shell `PATH`, 0024), `cwd` (absolute), `env` (added to the node's login-shell env; `clear_env` starts from an empty env instead), optional `stdin` bytes (≤ 1 MiB), `timeout_ms` (0 = 10 minutes, the 0025 default; at most 1 hour).
- **Output cap:** stdout and stderr together at most `max_output` bytes (0 or above the limit = 15 MiB, so the reply fits a 16 MiB frame; this narrows 0025's "16 MiB"). Past the cap the stream is read and dropped, and the reply says which stream was cut.
- **Process hygiene:** the child runs in its own process group with no controlling terminal and stdin closed unless given. Timeout: SIGTERM to the group, SIGKILL 2 s later (the 0071 escalation), and the reply has `timed_out`. Children of a closed connection are killed the same way. A daemon restart (0032) kills in-flight Execs; the client sees the connection close.
- **Concurrency:** at most 8 Execs run at once per node; more wait in a FIFO. This bounds a burst of `git` calls from a big `wt ls` and keeps them from starving agents.
- Not a PTY: interactive programs get EOF on stdin. `git` is always run with `GIT_TERMINAL_PROMPT=0` by the workflow, not by `Exec`.

**`ReadFile`** returns up to `max_bytes` (0 or above 15 MiB = 15 MiB) from `offset`, with the file's size and mtime and `eof`. Absolute paths only; symlinks are followed. A directory or special file is an error.

**`ListDir`** returns entries (name, kind: file / dir / symlink / other, size, mtime) of an absolute directory, unsorted, at most `max_entries` (0 or above 10,000 = 10,000) with `truncated`. No recursion.

**Who may call them:**

| Caller | `Exec` / `ReadFile` / `ListDir` |
|---|---|
| The user (no agent identity, 0072) | yes |
| An agent | refused, `forbidden:` (0025: it has a shell) |
| A workflow | in-process through the workflow host (§2), checked against the workflow's capabilities |

**CLI:** `fleet node exec [--cwd D] [--timeout 30s] [--env K=V]… -- <argv…>` (prints stdout and stderr to the matching streams, exits with the child's code, 124 on timeout like `timeout(1)`), `fleet node cat <path>`, `fleet node ls <dir> [--json]`. User-only; they exist so the primitives can be tested and scripted against a remote node in M6.

### 2. Workflows and `RunAction` (workstream B, ADR 0092)

**A new crate, `fleet-workflow`,** holds the workflow interface, the host, and the built-in workflows' shared code. `fleetd` owns the registry and wires the host to the node. Layering: `fleet-workflow` depends on `fleet-proto` (for `View`, `ActionSpec`) but not on `fleet-node`; the host adapter that calls the node lives in `fleetd`. This is the line the M5 WASM host uses: a WASM workflow is another `impl Workflow`, and the `Host` trait becomes its WIT imports.

```rust
trait Workflow: Send + Sync {
    fn id(&self) -> &str;                         // "git-worktree"
    fn manifest(&self) -> Manifest;               // capabilities, owned label prefix
    fn actions(&self) -> Vec<ActionSpec>;         // static; ListActions caches them
    async fn run(&self, host: &dyn Host, call: ActionCall) -> Result<ActionOutcome, ActionError>;
}

trait Host: Send + Sync {          // = the future `workflow` WIT world's imports
    fn caller(&self) -> &Caller;                  // who invoked the action (0072)
    async fn exec(&self, req: Exec) -> Result<ExecResult>;      // §1, capability-checked
    async fn read_file(&self, req: ReadFile) -> Result<FileData>;
    async fn list_dir(&self, req: ListDir) -> Result<DirListing>;
    async fn spawn(&self, req: SpawnAgent) -> Result<AgentInfo>; // as the caller: scope, quotas, approval
    async fn check_spawn(&self) -> Result<()>;    // quota dry run, before side effects
    async fn list_agents(&self, selector: Labels) -> Result<Vec<AgentInfo>>; // caller's scope
    async fn set_labels(&self, agent: u64, set: Labels, remove: Vec<String>) -> Result<()>; // owned prefix only
    async fn forget(&self, ids: Vec<u64>) -> Result<Forgotten>; // caller's scope
    fn storage(&self) -> &dyn Storage;            // storage:own, per workflow
}
```

**Manifest and capabilities.** A workflow declares what it uses, as plugins will (0007): `exec:<bin>`, `fs:read:<path-prefix>`, `spawn`, `labels:<prefix>`, `storage:own`. The host enforces them for built-ins too, so the code path is the one M5 relies on. git-worktree declares `exec:git`, `fs:read:*` (it reads files in repositories the caller names), `spawn`, `labels:git.`, `storage:own`.

**Action schemas.** An `ActionSpec` is `name` (`<workflow>.<action>`, e.g. `git-worktree.new-agent`), `title`, `description`, `params`, `callers` (`USER` or `USER_AND_AGENTS`), `destructive` (clients confirm first), and `target` (whether the action is about an agent, so clients can list it in an agent's context menu with that agent filled in). A param is `key`, `title`, `kind`, `required`, `default`, `choices`, `help`, `placeholder` and `user_only` (an agent caller may not set it, e.g. `force`). Kinds: `string`, `text` (multi-line), `bool`, `int`, `duration`, `choice`, `path` (a directory on the node), `agent` (an agent id or name), `argv` (a command line, an array of strings), `labels` (k=v pairs). Values travel as `ParamValue` (a oneof of string, bool, int, string list, string map). Our own small schema, not JSON Schema: it is what native forms can render, and nothing else consumes it.

**The host validates every call before the workflow sees it:** unknown or missing params, wrong kinds, `choice` outside its choices, and — the scope rule — an `agent` param must name an agent the caller may see (D(a) for an agent caller, 0072), else `forbidden:`. An action whose `callers` is `USER`, or a `user_only` param set by an agent, is `forbidden:`. Workflows therefore never re-implement scope.

**`RunAction` runs as the caller.** Spawns go through the same path as `SpawnAgent` from that caller: `fleet.parent`, narrowed labels, quotas and spawn approval (0079) all apply. The one difference is labels in the workflow's own prefix (§3, "Labels").

**Outcome.** `ActionResult` is one of:
- `done { message }`;
- `spawned { AgentInfo, message }`;
- `view { View }` — `View` is a oneof of `DiffView`, `TableView` (columns + rows of strings, each row optionally linked to an agent id or a path) and `TextView` (pre-formatted text);
- `refused { reason, retry }` — a refusal the user can override, with the params to resend (e.g. `force = true` for a dirty archive). Clients show the reason and an "… anyway" button; the CLI prints the reason and the flag to add, exit 1.

Errors that are not overridable are an ordinary `Error` (`forbidden:` → CLI exit 3, as in M3).

**Running.** The server runs each `RunAction` on its own task, so a slow action does not block the connection (like `Wait`). At most 4 actions run per connection and 16 per daemon; more wait. Closing the connection cancels the action at its next host call; a workflow that has made side effects rolls them back where it can (§3). No progress events in M4: every M4 action is seconds at most. M6's clone adds them in this block.

**Storage (`storage:own`).** A key–value table per workflow, `workflow_kv(workflow TEXT, key TEXT, value BLOB, updated_ms INTEGER, PRIMARY KEY (workflow, key))`, in the node's SQLite (schema **13**). It stands in for the controller store (0019 §4) until there is one; the `Storage` trait hides where it lives. Limits: 64 KiB per value, 10,000 keys per workflow.

**`ListActions`** returns every action the caller may call (agents see only `USER_AND_AGENTS` ones). The list is static per daemon build, so clients cache it per connection.

**CLI:** `fleet action ls [--json]`, `fleet action run <name> [key=value]… [--json]` (values parsed by the param's kind; `argv` takes the rest after `--`). `fleet action run` renders views as text: a diff as a unified diff, a table as aligned columns.

**Event subscriptions** (0025's "subscriptions to agent events") are part of the `workflow` WIT world in M5. git-worktree does not need them in M4, so the `Host` trait does not get them yet.

### 3. The built-in `git-worktree` workflow (workstream C, ADR 0093)

Native Rust in `fleet-workflow::git_worktree`. All git work is `git` through `host.exec`, with `GIT_TERMINAL_PROMPT=0`, `GIT_OPTIONAL_LOCKS=0` for reads, and `-c core.quotepath=off`. Output parsing (porcelain formats, unified diff) is pure and unit-tested on fixtures.

**Repositories.** A `repo` param is a path to any checkout of a repository (the main one or a linked worktree); the workflow resolves the main checkout with `git rev-parse --path-format=absolute --git-common-dir`. The repository's name is the main checkout's folder name, as in 0085. When `repo` is omitted, it is the caller's cwd: for an agent its own cwd, for the CLI the shell's cwd (the CLI sends it), for the GUI the New-agent sheet's folder.

**Layout and branches** (0010, made configurable). In `~/.fleet/config.toml`, read when an action runs:

```toml
[workflows.git-worktree]
root = "~/.fleet/worktrees/{repo}"   # where new worktrees go; {repo} = the repository's name
branch_prefix = "fleet/"              # new branch = prefix + slug, unless `branch` is given
fetch_base = true                     # fetch the base from its remote first (falls back to local, with a note)

[workflows.git-worktree.repos.fleetd]  # per repository, by name; any key above
root = "~/Developer/fleetd-wt"
branch_prefix = ""
```

A relative `root` is relative to the main checkout, so `root = "../{repo}-wt"` gives today's `../fleetd-wt/<slug>`. The default stays 0010's (open question 1).

**Labels.** The workflow owns the `git.` prefix (its manifest's `labels:git.`):
- `git.repo` = the main checkout's absolute path;
- `git.worktree` = the worktree's absolute path;
- `git.base` = the base ref the worktree was made from (e.g. `origin/main`).

The branch is not a label: it can change under the agent, and `AgentInfo.git` (0085) already shows it live. These labels mean "this agent was started for this worktree"; the diff, archive and the GUI use them to find an agent's worktree without guessing from the cwd.

**Who writes `git.*`.** Only the workflow (and the user, who may write any label). An agent can't (0025). When the workflow spawns for an agent, its `git.*` keys **replace** the inherited ones: that is the exception to M3's narrowing (where a child may not change a parent's key), and it is safe because the values come from the workflow, not the agent. A plain `fleet spawn` child inherits `git.*` like other labels, which is right for the common case (a helper in the parent's worktree) and is what 0025 says. The exception is generic: a workflow's owned prefix, declared in its manifest, is the only thing it may override; the core still does not interpret the keys.

**Actions:**

| Action | Params | Callers | What it does |
|---|---|---|---|
| `git-worktree.new-agent` | `slug` (req.), `command` (argv, req.), `repo`, `base`, `branch`, `name`, `labels`, `reuse` | user, agents | Makes a worktree and starts an agent in it (below) |
| `git-worktree.diff` | `agent` or `path` (one req.), `against` (`base` / `uncommitted` / `committed`), `file`, `context` | user, agents | Returns a `DiffView` (below) |
| `git-worktree.archive` | `agent` or `path`, `force` (user-only), `keep_branch` | user, agents | Removes a worktree (below) |
| `git-worktree.ls` | `repo` | user, agents | `TableView`: path, branch, base, ahead/behind, dirty, merged, agents in it |
| `git-worktree.prune` | `repo`, `dry_run` | user | Archives every worktree this workflow made that is clean, merged and has no running agent |

**`new-agent`:**
1. Validate `slug` (`[A-Za-z0-9._-]`, ≤ 64, not `.`/`..`) and resolve the repository and the target path `root/slug`.
2. `host.check_spawn()` first, so a quota refusal leaves nothing behind.
3. Base: `base` if given, else the remote's default branch (`refs/remotes/origin/HEAD`, else the main checkout's `HEAD`). With `fetch_base`, `git fetch <remote> <branch>` (30 s timeout); on failure, use the local ref and add a note to the message.
4. `git worktree add -b <branch> <path> <base>` (branch = `branch` or prefix + slug). If the path is already a worktree of this repository: with `reuse`, skip creation and use it (how the overseer resumes an agent with `claude --continue`); without it, `refused` with `retry: {reuse: true}`. A branch that exists elsewhere is an error naming it.
5. Record `wt/<path>` → `{repo, branch, base, created_ms, created_by}` in storage, so `prune` only touches worktrees this workflow made.
6. `host.spawn` with `cwd = path`, `command`, `name`, the caller's `labels` and the workflow's `git.*`. If the spawn is refused (approval denied or expired, quota raced), remove the worktree and branch just made (they are empty), and return the refusal.

**`diff`** is about one worktree, found from `agent` (its `git.worktree` label, else its cwd's checkout) or from `path`.
- `against = base` (default): everything the agent did, committed or not — the working tree (untracked files included) against `merge-base(git.base or the default branch, HEAD)`.
- `uncommitted`: the working tree against `HEAD`, untracked included.
- `committed`: `merge-base…HEAD`, commits only.
- Built from `git diff --no-color --no-ext-diff -M -U<context> <from>` plus `git ls-files --others --exclude-standard` (each untracked file read with `ReadFile`, as an added file; binary if it has a NUL in the first 8 KiB). Run with `--no-optional-locks` so it never blocks the agent's own git.
- **Caps:** at most 2,000 files and 50,000 lines or 8 MiB of hunk text per reply. Past the cap, the remaining files come with stats only (`hunks_omitted`), and the client asks for one file with `file = <path>`. Generated and lock files are not special-cased (open question 7).

**`archive`:**
- Refused (`refused`, `retry: {force: true}`) when the worktree has uncommitted changes, untracked files that are not ignored, or commits on its branch that are on no remote branch and not merged into the base (0010's rule).
- **Always refused, even with `force`,** when a running agent's cwd is inside it: kill it first. Exited agents whose cwd is inside it are forgotten with the worktree.
- `git worktree remove [--force] <path>`, then the branch is deleted when it is merged into the base (`git branch -d`); with `force` it is deleted anyway (`-D`) unless `keep_branch`. The storage record goes too.
- Agents may archive a worktree only through an `agent` param in D(a) (host-checked) or a `path` the workflow made for one of D(a), and never with `force`.

**`prune`** replaces the overseer's cleanup script: for every worktree this workflow made in the repository (from storage), archive it if it is clean, its branch is merged into the base (after a fetch when `fetch_base`), and no running agent is in it. `dry_run` lists what it would remove and why the rest stay. Worktrees made by hand are listed by `ls` but never pruned.

**Concurrency.** Two `new-agent` calls with one slug: git's own lock makes the second fail, reported as "slug in use". Two agents in one worktree are allowed (0025: this workflow's business); `ls` shows both, and `new-agent --reuse` into a worktree with a running agent returns `refused` ("another agent is working here") with `retry: {reuse: true, shared: true}`.

**CLI: `fleet wt`** (`fleet worktree` alias), a thin front over the actions:

```sh
fleet wt new <slug> [--repo D] [--base REF] [--branch B] [--name N] [--label k=v]… [--reuse] -- <program> [args…]
fleet wt ls [--repo D] [--json]
fleet wt diff [<agent>|--path D] [--against base|uncommitted|committed] [--stat] [--file F] [--json]
fleet wt archive <agent>|--path D [--force] [--keep-branch]
fleet wt prune [--repo D] [--dry-run]
```

`fleet wt new` prints the new agent like `fleet spawn` (id, or JSON). `fleet wt diff` prints a unified diff (coloured on a terminal) or `--stat`. Exit codes follow M3: 0 ok, 1 refused / not found, 2 usage or other error, 3 forbidden.

**How the overseer's day looks with it:**

```sh
fleet wt new issue-140 --branch feat/m4-primitives --name m4-a --label m4=a -- claude --model sonnet "…brief…"
fleet wait --state done,error,exited          # unchanged from M3
fleet wt diff m4-a --stat                      # review without cd-ing into the worktree
fleet wt prune --dry-run && fleet wt prune     # after the PRs merge; target/ goes with the worktree
```

### 4. GUI: the diff view and actions (workstream D, ADR 0094)

**Diff view.** A new pane kind next to terminal panes, read-only.
- **Opening it:** "Show Changes" (⌘⇧D) on the focused agent; in the agent's sidebar and graph context menus; in the palette ("Show changes: <agent>"). It opens in a new split beside the agent's terminal (the split chooser's rules, 0050), or focuses an existing diff pane of that agent. Shown only for agents in a git checkout (`AgentInfo.git` set); it calls `git-worktree.diff` with the `agent` param.
- **Layout:** a header (worktree · branch, against what, files changed, +/−, a refresh button and the `against` choice), a file list on the left (status letter, path, +/−; ↑/↓ or click jumps), and the unified diff on the right with old/new line numbers, coloured +/− lines from the theme, and a sticky file header. Files collapse and expand (space). Binary files and omitted hunks show a one-line note; an omitted file loads on expand (`file` param).
- **Refresh:** on open, on ⌘R / the button, and once when the agent's status goes to `waiting`, `done` or `exited` while the pane is visible. Never on a timer, never on output activity.
- **Rendering (core, unit-tested):** `core::diff` turns a `DiffView` into display rows (file headers, hunk headers, lines with numbers) once per refresh. The pane draws only the rows in the viewport (a uniform-height list), so a 50,000-line diff costs what a screenful costs. Long lines are cut at 2,000 characters with an ellipsis (horizontal scroll is not in M4). Text is selectable and copyable like terminal text.
- **Ended agents:** the diff still works while the worktree exists (it is read from disk, not from the agent). The ended-pane summary (0058) gains "Show Changes".

**Actions in the GUI.**
- **New agent sheet:** when the chosen folder is in a git checkout, a "New worktree" checkbox (remembered per target), with Slug (pre-filled from the agent name), Branch (placeholder `fleet/<slug>`) and Base (placeholder: the default branch). Spawn then calls `git-worktree.new-agent` instead of `SpawnAgent`. Presets (0084) work the same.
- **Context menus** (sidebar row, graph card, pane header) gain "Archive Worktree…" for an ended agent with `git.worktree` (a confirmation sheet; a `refused` result shows its reason and "Archive Anyway").
- **Generic action form:** the palette's "Run Action…" lists every action from `ListActions`, and picking one opens a sheet whose fields are rendered from the schema (`core::action_form` builds the field model; the sheet draws text fields, checkboxes, choice pop-ups, an agent picker and a folder field). Results show as a toast (`done`), focus the new agent (`spawned`), open a pane (`view`: diff, table or text) or the refusal sheet. This is the path every future workflow gets with no GUI code, and it is what the iOS/web clients will copy (0025).

**Benchmarks (0014, new metrics, no baselines until the user records them):**
- `gui_diff_open_ns`: from "Show Changes" to the first frame of a 10,000-line fixture diff (no git; the fixture is a `DiffView`). Budget 100 ms.
- `gui_diff_scroll_frame_ns`: a frame while scrolling that diff during the 50-agent flood. Budget 16 ms.

### 5. Benchmarks on the daemon side (workstreams A and C)

New metrics, no baselines until the user records them:
- `exec_true_roundtrip_ns` (wall, A): `Exec /usr/bin/true` from the CLI client to reply, idle daemon. Budget 20 ms.
- `diff_view_build_10k_lines_instructions` (instr, C): parsing a fixed 10,000-line unified diff into a `DiffView` and encoding it. Gated at the usual ~2% once recorded.

The existing gates must not move: nothing in M4 runs on the PTY path or at idle, so `fleetd_idle_rss_bytes`, the flood metrics and `fleetd_startup_instructions` should be unchanged (the registry is a static list built on first `ListActions`, not at startup).

### 6. The agent skill (workstream E)

The skill's worktree line ("Agents working in parallel on one repo each need their own git worktree") becomes a section: start children with `fleet wt new`, review with `fleet wt diff`, clean up with `fleet wt archive` / `prune`; `--reuse` to restart an agent in its old worktree; what `refused` means; that agents can't `--force`. `fleet node …` is not mentioned (agents are refused). The README and `docs/plan.md` (M4 done) are updated at the end.

## Protocol blocks and schema versions

Field numbers in `Request.kind`, `Response.kind` and `Event.kind` (M3 used 40–89; `main` has nothing above 85):

| Workstream | Block | Planned use |
|---|---|---|
| A. Primitives | 90–94 | `Request`: `exec = 90`, `read_file = 91`, `list_dir = 92`. `Response`: `exec_result = 90`, `file_data = 91`, `dir_listing = 92` |
| B. Workflows | 95–104 | `Request`: `list_actions = 95`, `run_action = 96`. `Response`: `action_list = 95`, `action_result = 96`. 97–104 spare (M6 progress events, cancel) |
| C. git-worktree | 105–109 | none planned: it goes through `RunAction`. Reserved in case it needs an event |
| D. GUI | 110–114 | none planned |

New top-level messages (`Exec`, `ExecResult`, `ReadFile`, `FileData`, `ListDir`, `DirEntry`, `DirListing`, `ActionSpec`, `ParamSpec`, `ParamValue`, `RunAction`, `ActionResult`, `View`, `DiffView`, `FileDiff`, `Hunk`, `TableView`, `TextView`) have their own field numbering. **B owns all action and view messages, `DiffView` included,** so C and D build on B's merged proto. The `DiffView` shape (B writes it, C fills it, D draws it):

```proto
message DiffView {
  string title = 1;          // "issue-140 · feat/m4-primitives"
  string from_label = 2;     // "origin/main (merge base 1a2b3c4)"
  string to_label = 3;       // "working tree"
  repeated FileDiff files = 4;
  uint32 files_changed = 5;
  uint64 additions = 6;
  uint64 deletions = 7;
  bool truncated = 8;        // some files have hunks_omitted
}
message FileDiff {
  string old_path = 1;       // empty when added
  string new_path = 2;       // empty when deleted
  FileStatus status = 3;     // ADDED MODIFIED DELETED RENAMED COPIED TYPE_CHANGED
  bool binary = 4;
  uint32 additions = 5;
  uint32 deletions = 6;
  repeated Hunk hunks = 7;
  bool hunks_omitted = 8;    // over the cap: ask again with `file`
  string old_mode = 9;
  string new_mode = 10;
}
message Hunk {
  uint32 old_start = 1; uint32 old_lines = 2;
  uint32 new_start = 3; uint32 new_lines = 4;
  string header = 5;         // the text after @@ … @@
  // The hunk's lines as in a unified diff: each starts with ' ', '+', '-'
  // or '\', separated by '\n'. One bytes field, not a message per line:
  // a 50,000-line diff stays one allocation per hunk on both sides.
  bytes body = 6;
}
```

**`SCHEMA_VERSION`:** B takes **13** (`workflow_kv`). C takes **14** only if it needs a table of its own (it should not: storage is `workflow_kv`). A, D and E add none. The overseer renumbers at merge time if the order differs.

**ADR numbers** (M4 owns 0090–0094 and 0120–0129; 0095–0119 belong to the parallel "UI Improvements 2" GUI batch, #149): 0090 milestone (this PR), 0091 A, 0092 B, 0093 C, 0094 D, 0120 E if the skill changes need one, 0121–0129 spare for amendments.

## Waves and agents

Each workstream is one agent in its own worktree under `../fleetd-wt/`, branched from `main`, with its ADR number and proto block pre-assigned, opening one PR with `Closes #N`. The overseer (Claude) reviews, chains conflicting branches and merges in order; the user merges unless they say otherwise. (From wave 2 on, the overseer can start agents with `fleet wt new` itself, which is the milestone's first real test.)

| Wave | Workstream | Depends on | Model | ADR |
|---|---|---|---|---|
| 1 | **A. Node primitives** (§1): proto 90–94, node impl (process group, caps, timeout, concurrency), scope refusal for agents, `fleet node exec/cat/ls`, `exec_true_roundtrip_ns` | — | Sonnet | 0091 |
| 1 | **B. Workflow host + `RunAction`** (§2): `fleet-workflow` crate (traits, schema validation, capability checks, scope on `agent` params, owned-prefix label override in `fleet_node::scope`), all action/view proto, `workflow_kv` (schema 13), server task model and limits, `fleet action ls/run`. Tested with a fake workflow and a fake `Host`; the real `Host` adapter calls A's primitives once A merges (stub until then) | — | Opus | 0092 |
| 2 | **C. git-worktree workflow** (§3): actions, config, labels, rollback, porcelain and diff parsers on fixtures, integration tests against real temp repos, `fleet wt`, `diff_view_build_10k_lines_instructions` | A, B | Opus | 0093 |
| 2 | **D. GUI diff pane + actions** (§4): `core::diff`, `core::action_form`, diff pane, New-agent "New worktree", context menus, Run Action… sheet, the two GUI benchmarks. Built against B's proto with fixture `DiffView`s; wired to C's actions when C merges | B | Opus | 0094 |
| 3 | **E. Skill, README, plan** (§6), plus any follow-ups from waves 1–2 | C, D | Sonnet | 0120 if needed |

- **Why these models:** B sets the interface M5's plugin host must keep, and carries a security rule (agents through actions); C does process orchestration with rollback and parsing of git's formats; D is a new rendering surface under perf gates. Those get Opus. A is a well-bounded node request in the existing pattern, and E is docs, so Sonnet.
- **Shared-file conflicts.** A and B both touch `fleet.proto`, `fleetd/src/server.rs`, `fleetd/src/scope.rs` and `fleet-cli/src/main.rs`. Each stays in its field block and puts its CLI in its own module (`fleet-cli/src/node.rs`, `action.rs`, later `wt.rs`), so merges are mostly additive. B's change to `fleet_node::scope` (the owned-prefix exception to narrowing) is the one place that touches M3's security code; the overseer reviews it on its own.
- **Wave 1** starts when this spec is merged. **Wave 2** starts when A and B are merged. D may start on B's branch before B merges if the overseer judges the proto stable.

## Decided here (overseer's calls, within 0025 / 0033 / 0072)

- Workflows are a Rust trait in a new `fleet-workflow` crate, hosted by `fleetd`, behind a `Host` trait shaped like the future WIT imports; capabilities are enforced for built-ins too.
- The parameter schema is fleetd's own small one, not JSON Schema.
- `RunAction` runs as the caller: agents get the M3 parent, scope, quotas and approval; the host checks `agent` params against D(a).
- A workflow owns a label prefix (`git.` for git-worktree) and only there may override inherited labels when spawning for an agent.
- Agents may call `new-agent`, `diff`, `ls` and non-forced `archive` on their own subtree; `prune`, `force` and the node primitives are the user's.
- `Exec` output cap is 15 MiB (fits `MAX_FRAME`), 8 concurrent per node; no streaming in M4.
- Diffs default to "everything since the merge base, untracked files included"; the diff pane refreshes on open, on demand, and once per transition to waiting/done/exited.
- Workflow state lives in a per-workflow key–value table in the node store until the controller store exists.
- `prune` touches only worktrees the workflow made; hand-made worktrees are listed, never pruned.

## Left to the builders

- Exact proto messages within the reserved blocks, except `DiffView` above.
- How the host cancels a running action at its next call, and the `Storage` trait's API.
- The diff pane's colours, spacing, and keyboard details beyond the ones above.
- Whether `fleet wt diff` pages through `$PAGER` on a terminal.

## Open questions for the user

These are in the PR body too; the defaults above are the overseer's proposals.

1. **Worktree root default.** Keep 0010's `~/.fleet/worktrees/<repo>/<slug>` with per-repo config (and set `fleetd`'s to `../fleetd-wt` in your config), or make the sibling `../<repo>-wt/<slug>` the default for every repo?
2. **Branch names.** Keep `fleet/<slug>` as the default, or default to no prefix (the branch is the slug) since you pass `--branch feat/…` anyway?
3. **Agents and actions.** OK for agents to call `new-agent`, `diff`, `ls` and non-forced `archive` on their own subtree, with `prune` and `--force` user-only?
4. **Fresh base.** Fetch the base from `origin` before every `new-agent` (default on, ~1 s, falls back to local), or branch from the local ref unless asked?
5. **Diff pane placement.** A split beside the agent's terminal (proposed), a full-window mode like the graph (⌘⇧G), or a separate window?
6. **Per-repo setup hook.** Leave it out of M4 (proposed), or add `setup = ["…"]` per repo, run in the new worktree before the agent starts (copy `.env`, set `CARGO_TARGET_DIR`, …)?
7. **Noise in diffs.** Show lock files and generated files like any other file (proposed), or collapse a configurable list (`Cargo.lock`, `*.min.js`, …) by default?
8. **Benchmark budgets.** `exec_true_roundtrip_ns` 20 ms, `gui_diff_open_ns` 100 ms, `gui_diff_scroll_frame_ns` 16 ms: acceptable?
