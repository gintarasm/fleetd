# 0035 — Agent roles: scoped, supervised agents handing work to each other

**Status:** phased · 2026-11-23. Templates and spawn/manage scoping are decided in [0130](0130-roles-v1.md) (M4b, M4c); §3 supervision is M5b; the rest of §2 and §4 queues are M7b. Proposed 2026-11-15.

**Builds on:** [0025](0025-core-knows-agents-workflows-own-hierarchy.md) (labels, workflows), [0012](0012-credentials.md) and [0019](0019-control-plane-direction.md) (grants by label selector), [0033](0033-satellite-milestone.md) (agent tree, quotas), [0034](0034-agent-communication.md) (the CLI agent channel: `fleet wait`, mailbox, leases, questions).

## Context

The user wants agents with a **role**: a defined job and a matching, limited set of powers. The first example is a pair:

- a **watcher** that runs for a long time, watches logs for problems and writes them down;
- a **fixer** that takes those problems and fixes them.

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
  grants = ["logs-read"]              # credentials (0012), by name
  produces = "fleetd-problems"        # the queue it adds items to (§4)
  supervise = { restart = "on-failure", rotate = "6h", heartbeat = "30m" }

  [roles.fixer]
  program = ["claude"]
  instructions = "roles/fixer.md"
  worktree = true                     # a fresh worktree per agent (git-worktree workflow, 0025)
  grants = ["git-push-branch", "pr-create"]
  consumes = "fleetd-problems"        # one fixer per ready item (§4)
  max_concurrent = 3

  [queues.fleetd-problems]
  backend = "…"                       # any installed queue backend (§4); none is built in or assumed
  # backend-specific settings follow
  ```

- `fleet spawn --role watcher` (and the GUI's New agent dialog) stamps everything on: the program, instructions, cwd, labels (`role=watcher` set by fleetd, so it can't be forged), quotas and grants.
- An agent **cannot widen its role**: it can't change its `role` label (0025), and a child it spawns gets the same role or a role the parent's role lists under `may_spawn`, never an arbitrary one.

### 2. Scoping: what a role may do

Three layers, from strongest to weakest:

1. **Credentials (fleetd enforces).** Grants are given by role, so the watcher's credentials can read logs but not push code, and the fixer's can push branches and open PRs but not merge. Queue access needs no credential in the agent at all: agents reach the queue through fleetd (§4), and the backend holds its own. This is the real boundary, since an agent can run anything its shell allows.
2. **fleetd scopes (fleetd enforces).** Per role: which `fleet` commands are allowed (a watcher may `fleet message` and `fleet report` but not `fleet spawn`), and which other roles it may message.
3. **The agent program's own permissions (best effort).** fleetd writes the role's tool rules into the agent program's settings at start (Claude Code `permissions`, omp's equivalent), through the M5 adapters. It narrows what the agent tries; it is not a security boundary.

The token boundary open in 0034 (an agent dropping its token to act as the user) must be closed before layer 2 means anything.

### 3. Supervision for long-running roles

- **Restart policy:** `never`, `on-failure` or `always`, with backoff and a cap (e.g. 5 restarts an hour; past that the agent is marked as needing the user).
- **Context rotation:** a long-running agent's context fills up. At `rotate` (or when the agent asks with `fleet report rotate`), fleetd asks it for a handoff note (`fleet report handoff "…"`), stops it, and starts a fresh agent of the same role with the note appended to its instructions. The agent's `name` (issue #48) stays the same; its id changes.
- **Heartbeat:** a role can require a `fleet report` every `heartbeat`. A missed heartbeat marks it `waiting (stale)` for the user and the parent (0034 §2).

### 4. Handing work between roles: a pluggable queue

Roles hand work over through a **queue**, and fleetd defines only the queue's *interface*, never its storage. Where items live is the user's choice, made per queue in config.

**Backends are plugins.** A queue backend is provided by a plugin (a workflow plugin, 0025, or a dedicated plugin kind; decided with the plugin host in M5). Examples of backends someone could write: an issue tracker (GitHub, Linear, Jira), a shared file or database, or a board kept in fleetd's own state. None is built in or assumed by this ADR, and roles never name one: they name a queue (`produces`, `consumes`), and the queue names its backend.

**The interface** every backend implements:
- **Items** have an id, a title, a body, `evidence` (free-form attachments such as log lines), and a **state**: `triage` → `ready` → `in-progress` → `done`, plus `blocked` and `rejected`. Backends map these to their own model (labels, columns, a status field).
- **Operations:** `add`, `get`, `list {state}`, `update {state, note}`, `claim` (atomically `ready` → `in-progress` for one consumer, so two fixers never take the same item), and `find_similar {title, body}` for duplicate checks.
- **Events:** item added and state changed, so consumers don't poll.
- **Links:** an item can record the agents that worked on it (name and id) and results (a PR URL, a commit), shown however the backend shows them.

**Agents use it through the CLI** (0034), not through the backend's own tools: `fleet queue add <queue> --title … --evidence …`, `fleet queue similar <queue> …`, `fleet queue update <id> --state blocked --note …`. fleetd checks the caller's role (a watcher may `add` to the queues it `produces`; a fixer may `update` items it claimed) and passes the call to the backend, which holds its own credentials. Changing backends never changes an agent's instructions.

**The flow:**
- **The watcher** adds items in `triage` with the log evidence (lines, timestamps, counts). Before adding, it must call `find_similar` and **add to an existing item instead of filing a duplicate** (on 2026-11-15, #17 and #18 were exact duplicates of #19 and #20). It records what it has already reported, so a restart doesn't add again.
- **The gate:** the user, or a rule the user writes (e.g. "low-severity items go straight to ready"), moves an item from `triage` to `ready`.
- **The dispatcher** is part of roles, not of any backend: on each `ready` item in a queue a role `consumes`, it `claim`s the item and spawns an agent of that role, up to the role's `max_concurrent`, taking turns fairly, and links the agent to the item.
- **The fixer** works in its own worktree, uses leases for shared resources (0034 §4, e.g. `perf-wall`), opens a PR, and reports `done` with the result; the dispatcher moves the item to `done`. If the fixer gets stuck it reports `waiting` with a question (0034 §6), and the item goes to `blocked`.

### 5. Human gates

- The watcher only **reports** problems to the queue; it never fixes them.
- Fixers **open** PRs; they never merge.
- Perf regressions still need the user's explicit approval (CLAUDE.md, 0014); a fixer asks through the questions channel.
- The user can pause a role (`fleet role pause fixer`): no new spawns; running agents finish.

### 6. Seeing it

- The graph view (0033) groups agents by role, shows each queue's depth per state (`triage`, `ready`, `in-progress`, `blocked`) whatever its backend, and links an agent to its item.
- The sidebar and status dots (issue #32) show role agents like any other, with the role as a chip (issue #48).

## Consequences

- **Fits the roadmap as "Later":** it needs M3 (agent channel), M4/M5 (workflows, adapters) and M7 (grants). It replaces the *coordinator mode* item in `docs/plan.md`'s Later list, and touches *scheduled automations* (a role that starts on a schedule is a natural extension).
- **Constraints on earlier milestones:** M3's token scopes should be expressible per role (layer 2), and M5's adapters should be able to write an agent program's permission settings at start (layer 3). Neither needs to ship roles.
- **Cost:** a long-running watcher is a long-running agent session; rotation keeps context bounded but not usage. Usage tracking (Later) matters more once roles exist.
- **Plugin host (M5)** must be able to host queue backends: the interface in §4 becomes a plugin contract (WIT world or similar, 0007), with its own tests that any backend must pass.
- **Open:**
  - whether queue backends are workflow plugins or their own plugin kind;
  - how the watcher reads logs: tailing files, a command (`journalctl -f`, `kubectl logs`), or a log source plugin (likely pluggable the same way);
  - the gate's rules language (state and severity only, or more);
  - whether roles can be shared per project (a `.fleet/roles/` in the repo, which agents could edit) or stay user-only in `~/.fleet/`;
  - running roles on remote nodes (M6).
