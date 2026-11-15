# 0035 — Agent roles: scoped, supervised agents handing work to each other

**Status:** proposed · 2026-11-15 (the user's direction; not scheduled. Decide the details when a milestone takes it up.)

**Builds on:** [0025](0025-core-knows-agents-workflows-own-hierarchy.md) (labels, workflows), [0012](0012-credentials.md) and [0019](0019-control-plane-direction.md) (grants by label selector), [0033](0033-satellite-milestone.md) (agent tree, quotas), [0034](0034-agent-communication.md) (the CLI agent channel: `fleet wait`, mailbox, leases, questions).

## Context

The user wants agents with a **role**: a defined job and a matching, limited set of powers. The first example is a pair:

- a **watcher** that runs for a long time, watches logs for problems and writes them down as issues;
- a **fixer** that takes those issues and fixes them.

Today any agent can do anything its shell allows, nothing defines what a "watcher" is, a long-running agent that crashes or fills its context just stops, and nothing hands work from one agent to the next. The pieces exist or are planned: `role=…` labels (0025 uses `role=reviewer` as its example), grants scoped by label selectors (0025, M7), workflows that react to events and spawn agents (0025, M4/M5), and the 0034 channel. What's missing is what joins them.

## Proposal

### 1. Roles are templates the user defines

- A role is a named template in `~/.fleet/config.toml` (which agents can't read or change, like quotas in 0033):

  ```toml
  [roles.watcher]
  program = ["claude"]
  instructions = "roles/watcher.md"   # standing prompt, given at every start
  cwd = "~/Developer/fleetd"
  labels = { project = "fleetd" }     # plus role=watcher, added by fleetd
  tools = "roles/watcher-tools.toml"  # what the agent program itself may run
  grants = ["github:issues-write"]    # credentials (0012), by name
  supervise = { restart = "on-failure", rotate = "6h", heartbeat = "30m" }

  [roles.fixer]
  program = ["claude"]
  instructions = "roles/fixer.md"
  worktree = true                     # a fresh worktree per agent (git-worktree workflow, 0025)
  grants = ["github:push-branch", "github:pr-create"]
  max_concurrent = 3
  ```

- `fleet spawn --role watcher` (and the GUI's New agent dialog) stamps everything on: the program, instructions, cwd, labels (`role=watcher` set by fleetd, so it can't be forged), quotas and grants.
- An agent **cannot widen its role**: it can't change its `role` label (0025), and a child it spawns gets the same role or a role the parent's role lists under `may_spawn`, never an arbitrary one.

### 2. Scoping: what a role may do

Three layers, from strongest to weakest:

1. **Credentials (fleetd enforces).** Grants are given by role, so the watcher's GitHub token can open issues but not push, and the fixer's can push branches and open PRs but not merge. This is the real boundary, since an agent can run anything its shell allows.
2. **fleetd scopes (fleetd enforces).** Per role: which `fleet` commands are allowed (a watcher may `fleet message` and `fleet report` but not `fleet spawn`), and which other roles it may message.
3. **The agent program's own permissions (best effort).** fleetd writes the role's tool rules into the agent program's settings at start (Claude Code `permissions`, omp's equivalent), through the M5 adapters. It narrows what the agent tries; it is not a security boundary.

The token boundary open in 0034 (an agent dropping its token to act as the user) must be closed before layer 2 means anything.

### 3. Supervision for long-running roles

- **Restart policy:** `never`, `on-failure` or `always`, with backoff and a cap (e.g. 5 restarts an hour; past that the agent is marked as needing the user).
- **Context rotation:** a long-running agent's context fills up. At `rotate` (or when the agent asks with `fleet report rotate`), fleetd asks it for a handoff note (`fleet report handoff "…"`), stops it, and starts a fresh agent of the same role with the note appended to its instructions. The agent's `name` (issue #48) stays the same; its id changes.
- **Heartbeat:** a role can require a `fleet report` every `heartbeat`. A missed heartbeat marks it `waiting (stale)` for the user and the parent (0034 §2).

### 4. Handing work between roles: GitHub issues as the queue

- **The queue is GitHub issues**, not a fleetd store: it is visible, the user can edit it, it already drives the user's work, and fleetd stays small (0015).
- **The watcher** files issues labelled `fleet:triage`, with the log evidence (lines, timestamps, counts). Before filing, it must **search open and recently closed issues and add to an existing one instead of filing a duplicate** (on 2026-11-15, #17 and #18 were exact duplicates of #19 and #20). It records what it has already reported, so a restart doesn't file again.
- **The gate:** the user, or a rule the user writes (e.g. "severity:low issues go straight to ready"), moves an issue from `fleet:triage` to `fleet:ready`.
- **A dispatcher** (a workflow plugin, 0025) watches for `fleet:ready` issues and spawns a fixer for each, up to the role's `max_concurrent`, taking turns fairly. It labels the issue `fleet:in-progress` and links the agent (its name and id) in a comment.
- **The fixer** works in its own worktree, uses leases for shared resources (0034 §4, e.g. `perf-wall`), opens a PR that says `Closes #N`, and reports `done` with the PR URL. If it gets stuck it reports `waiting` with a question (0034 §6); the dispatcher labels the issue `fleet:blocked`.
- A **fleetd-native board** (queue in fleetd's own state, shown in the GUI) may come later for work that doesn't belong in GitHub. It would use the same states.

### 5. Human gates

- The watcher only **files** issues; it never fixes them.
- Fixers **open** PRs; they never merge.
- Perf regressions still need the user's explicit approval (CLAUDE.md, 0014); a fixer asks through the questions channel.
- The user can pause a role (`fleet role pause fixer`): no new spawns; running agents finish.

### 6. Seeing it

- The graph view (0033) groups agents by role, shows each role's queue depth (`triage`, `ready`, `in-progress`, `blocked`) and links an agent to its issue.
- The sidebar and status dots (issue #32) show role agents like any other, with the role as a chip (issue #48).

## Consequences

- **Fits the roadmap as "Later":** it needs M3 (agent channel), M4/M5 (workflows, adapters) and M7 (grants). It replaces the *coordinator mode* item in `docs/plan.md`'s Later list, and touches *scheduled automations* (a role that starts on a schedule is a natural extension).
- **Constraints on earlier milestones:** M3's token scopes should be expressible per role (layer 2), and M5's adapters should be able to write an agent program's permission settings at start (layer 3). Neither needs to ship roles.
- **Cost:** a long-running watcher is a long-running agent session; rotation keeps context bounded but not usage. Usage tracking (Later) matters more once roles exist.
- **Open:**
  - how the watcher reads logs: tailing files, a command (`journalctl -f`, `kubectl logs`), or a log source plugin;
  - the dispatcher's rules language (labels and severity only, or more);
  - whether roles can be shared per project (a `.fleet/roles/` in the repo, which agents could edit) or stay user-only in `~/.fleet/`;
  - running roles on remote nodes (M6).
