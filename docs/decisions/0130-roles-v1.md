# 0130 — Roles v1: reusable agent templates, spawn scoping and the overseer

**Status:** accepted · 2026-11-23 (the user's decisions, from a design grilling; scheduled as M4b and M4c)

**Builds on:** [0035](0035-agent-roles.md) (agent roles, proposed), [0025](0025-core-knows-agents-workflows-own-hierarchy.md) (labels, reserved `fleet.*` prefix), [0033](0033-satellite-milestone.md) / [0034](0034-agent-communication.md) (agents spawn agents, descendant scope, quotas), [0072](0072-agent-identity-by-session.md) (caller identity by session), [0079](0079-spawn-approval.md) (spawn approval), [0052](0052-agent-names.md) (agent names), [0090](0090-m4-workflows.md) (M4: `ReadFile`, the git-worktree workflow).

**Supersedes:** the presets part of [0084](0084-gui-command-presets.md) (`[[gui.presets]]`). Recent commands, the first prompt and Duplicate Agent stay.

**Phases:** [0035](0035-agent-roles.md). Its §1 (templates) and part of §2 (who may spawn and manage) land here; §3 (supervision) is M5b; the rest of §2 (grants, command scopes, `may_spawn`, tool rules) and §4 (queues and the dispatcher) are M7b.

## Context

The user wants to define roles for agents once and reuse them, for example an **overseer** that the user starts to manage other agents, and that no agent may start. 0035 sketched a full roles system, but most of it needs the plugin host (M5) and secrets (M7). The reusable-template part, and the scoping the overseer needs, need only M3 (agent identity and scope) and M4 (`ReadFile`, worktrees). GUI presets (0084) already give a small, GUI-only version of "a reusable spawn": a name, a command and labels.

## Decision

### 1. A role is a file

- One TOML file per role; the file name is the role name.
  - **User roles:** `~/.fleet/roles/<name>.toml`. Agents are not meant to edit these.
  - **Project roles:** `<repo>/.fleet/roles/<name>.toml`, shared through the repository and editable by agents.
  - When a user role and a project role have the same name, the **user role wins**.
- Fields:

  ```toml
  # ~/.fleet/roles/reviewer.toml
  description  = "Reviews a PR branch, never pushes"
  program      = ["claude"]
  instructions = "reviewer.md"          # a file next to the role file; or inline `instructions_text`
  cwd          = "~/Developer/fleetd"   # optional; the spawn's cwd overrides it
  worktree     = false                  # true: a fresh worktree from the git-worktree workflow (0090)
  labels       = { project = "fleetd" }
  env          = { }                    # extra environment variables; never secrets
  name         = "reviewer-{n}"         # default agent-name pattern (0052)
  vars         = ["pr"]                 # spawn-time variables, used as {pr} in instructions and name
  spawnable_by = "any"                  # §4
  manages      = "descendants"          # §4
  ```

- **No inheritance** (`extends`): a role is self-contained. Copying a few lines costs less than a resolution order.
- **Variables:** a role lists its `vars`. They are given at spawn (`--var pr=42`, or fields in the GUI sheet), substituted as `{name}` in `instructions`, `instructions_text` and `name`, and all must be supplied. An unknown or missing variable fails the spawn. Nothing else is templated.
- **Built-in roles:** only the program defaults found on `PATH`, as 0084's preset defaults: Claude (`claude`), Claude (continue) (`claude --continue`), Codex (`codex`), omp (`omp`), with no instructions. fleetd ships no opinionated roles (reviewer, coder, …); those belong in the user's or the project's files.

### 2. Presets become roles

- `[[gui.presets]]` entries are read as user roles (name, command, labels, `spawnable_by = "any"`), with a deprecation note in Settings. A real role file with the same name wins.
- Everywhere the GUI showed presets now lists roles: the New agent sheet's buttons, the split chooser's rows 4–9, and the palette's "New <role>…" entries.

### 3. Spawning a role

- **From everywhere:** `fleet spawn --role <name> [--var k=v]… [--cwd …] [--name …] [prompt]` from the CLI, from an agent (the M3 channel, subject to quotas and spawn approval), and from the GUI.
- **The daemon resolves roles**, never the client. It reads user roles from its own `~/.fleet/roles/` and project roles from the spawn cwd's repository (through M4's `ReadFile` when the repo is on another node), so the CLI, the GUI and agents always get the same answer.
- **The role label:** fleetd sets `fleet.role=<name>` on the agent. It is under the reserved prefix, so neither users nor agents can set or change it. A plain `role=…` label stays an ordinary user label (existing presets keep theirs).
- **Instructions** reach the agent per program: `claude --append-system-prompt <text>` and the codex/omp equivalents, from a small built-in table that the M5 adapters later take over. For an unknown program they are the first prompt. A task prompt given at spawn comes after the instructions, never instead of them.
- **Snapshots:** the agent keeps the role definition it was spawned with (`fleet role show --agent <id>`). Restart and Duplicate use the role's **current** definition, so an edit applies to everything started afterwards; if the role was deleted, they fall back to the snapshot.
- `fleet role list` and `fleet role show <name>` show roles with their source (user, project, built-in, preset).

### 4. Scoping: who may spawn a role, and what it may manage

- **`spawnable_by`** on the role being spawned: `"any"` (the default), or a list of `"user"` and role names.

  ```toml
  spawnable_by = ["user"]               # overseer: only the user
  spawnable_by = ["user", "overseer"]   # coder: the user, or an overseer
  ```

  fleetd checks it at spawn against the caller's identity (0072): the user, or an agent with a `fleet.role`. An agent without a role counts only under `"any"`. As 0072 says, this holds against an agent that forgets or drops its token, not against a determined same-uid process.
- **`manages`**: `"descendants"` (the default, the M3 scope) or `"fleet"`. With `"fleet"`, the agent may list, read the screens and history of, message, wait on, send input to and stop **every** agent on the daemon, not only its descendants. Limits:
  - `manages = "fleet"` is allowed only in a **user** role whose `spawnable_by` is exactly `["user"]`; a role file that breaks this fails to load, with the reason shown in `fleet role list` and Settings.
  - Even then it cannot act on the user, change another agent's `fleet.role` or other `fleet.*` labels, or spawn a role whose `spawnable_by` does not name it.
- **Project roles can't grant powers:** they cannot set `manages = "fleet"` and cannot shadow a user role (§1). They may restrict `spawnable_by`.
- A parent-side `may_spawn` (which roles an agent may start), command scopes and grants are M7b (0035 §2).

### 5. The GUI

- **M4b:** Settings › Roles lists roles read-only, grouped as User, Project and Built-in, with load errors. **Save as role…**, on a running agent or in the New agent sheet, writes a user role from its command, labels, cwd and prompt; the prompt becomes the instructions.
- **M4c, the role editor:** New, Duplicate and Delete. A form for every §1 field plus `spawnable_by`, `manages` and `vars`, and a multi-line editor for the instructions (saved to the `.md` file). A choice of where to save (User, or this project), checked against the §4 rules. Built-in roles are read-only; Duplicate makes an editable user copy. The editor reads the file again when it opens and warns if the file changed underneath it; hand-edited files and the editor are equal.
- The daemon writes role files through new **`SaveRole`** / **`DeleteRole`** requests, so the editor also works for a remote target (M4 gives nodes only read primitives).

## Consequences

- **Milestones:** M4b (§1–§4, §5's list and Save as role) after M4; M4c (§5's editor) after M4b. 0035's remaining phases become M5b (supervision) and M7b (role scoping and queues). `docs/plan.md` lists all four.
- The overseer works from role files and the CLI at the end of M4b, before the editor exists.
- `fleet.role` joins the core-written `fleet.*` labels (as `fleet.parent`, 0033), so `SetLabels` must reject it from users and agents alike.
- Agents with `manages = "fleet"` widen the M3 scope rules (0033/0034); the scope checks gain one branch, and the mailbox's allowed recipients grow to every agent for such a caller.
- **Open, for the M4b spec:** the codex/omp flags for instructions; whether `manages = "fleet"` reaches agents on other nodes before M6; how the GUI shows a role's variables in the split chooser, which spawns without a sheet.
