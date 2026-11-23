# 0092 — The workflow host, `ListActions` and `RunAction`

**Status:** accepted · 2026-11-23 (M4 workstream B, issue #162; design in [the M4 spec](../superpowers/specs/2026-11-23-m4-workflows-design.md) §2)

**Builds on:** [0025](0025-core-knows-agents-workflows-own-hierarchy.md) (workflows own the hierarchy), [0007](0007-plugin-system.md) (plugins, capabilities, declarative views), [0072](0072-agent-identity-by-session.md) (callers and scope), [0079](0079-spawn-approval.md) (spawn approval), [0090](0090-m4-workflows.md) (M4).

**Amends:** [0025](0025-core-knows-agents-workflows-own-hierarchy.md)'s "an agent spawned by another agent … may only narrow [labels]": a workflow spawning for an agent replaces the keys of the label prefix it owns (below). Nothing else about narrowing changes.

## Context

0090 decided the shape: workflows are a Rust trait in a new crate, hosted by `fleetd` behind a `Host` trait shaped like the M5 WIT imports, called by every client through `ListActions` and `RunAction`. The spec left to this workstream the exact protocol messages, how cancellation works, the `Storage` API, and the code of the one exception to M3's label narrowing.

## Decision

### Crate and layering

- **`fleet-workflow`** depends on `fleet-proto` only. It holds the `Workflow` and `Host` traits, the `Storage` trait, `Manifest` and capabilities, param validation, and the `Registry`. It has no node, no tokio, no git.
- **`fleetd/src/server/action.rs`** owns the daemon's registry, the per-call `Host` that reaches the node (`NodeHost`), and the task model. The built-in list is static (empty until git-worktree, 0093, joins it) and the registry is built on the first `ListActions` or `RunAction`, never at startup.
- Async trait methods return boxed futures, so `dyn Host` and `dyn Workflow` work with no extra crate; the M5 WASM host is another `impl Workflow`.
- **The node primitives** (`exec`, `read_file`, `list_dir`) are workstream A's (#161). Until A merges, `NodeHost` answers them "unavailable" (`TODO(#161)`); everything here is tested with fake workflows and a fake host.

### Protocol (`Request`/`Response` 95–96; 97–104 spare for M6's progress events and cancel)

- `ListActions {}` → `ActionList { actions }`: the actions the caller may call (an agent sees only `USER_AND_AGENTS` ones).
- `RunAction { name, params, cwd }` → `ActionResult { done | spawned | view | refused }`, or an `Error` (`forbidden:` for scope, exit 3 in the CLI). `cwd` is the caller's working directory, for actions that default to it.
- `ActionSpec { name, title, description, params, callers, destructive, target_param }`. `callers` defaults to `USER` (0), so an action is the user's unless it says otherwise. `target_param` names the `agent` param an action is about (clients offer it in an agent's menu with that agent filled in), instead of a bool: the client then knows which param to fill.
- `ParamSpec { key, title, kind, required, default, choices, help, placeholder, user_only }`; `ParamValue` is a oneof of string, bool, int, `StringList`, `StringMap`. `duration` is an int of milliseconds; an `agent` param is sent as an id or a name and reaches the workflow as the id.
- `View` is a oneof of `DiffView` (exactly as the spec shows), `TableView` (rows may link an agent id or a path) and `TextView`. `ActionRefused { reason, retry }`: resend with `retry` merged into the params.

### What the host enforces

Every call passes two layers before and while the workflow runs, both in `fleet-workflow` so the M5 host gets them unchanged:

1. **Validation** (`params::validate`), before the workflow sees the call: an agent calling a `USER` action, or setting a `user_only` param (even to `false`), is `forbidden:`; unknown params, missing required ones, wrong kinds, a `choice` outside its choices, a `path` that is not absolute or has `..`, an empty `argv`, a `labels` key in `fleet.` are invalid; an `agent` param must name an agent in the caller's scope (D(a) for an agent caller), else `forbidden:` (an unknown agent reads the same to an agent, so scope does not leak); an agent may not put keys of the workflow's owned prefix in a `labels` param (`forbidden:`). Defaults are filled in.
2. **Capabilities** (`checked::Checked`, wrapping the real host): `exec:<bin>` matches `argv[0]` exactly (a path is not the bin; `exec:*` runs anything); `fs:read:<prefix>` matches absolute, `..`-free paths component-wise; `spawn` covers `spawn`, `check_spawn` and `forget`; `labels:<prefix>` (at most one per manifest) covers `set_labels`, every key in the prefix; `storage:own` covers storage. `list_agents` needs none: it is the caller's scope. A malformed manifest or action schema fails when the registry is built (a test checks the built-ins).

`NodeHost` then acts as the caller: `list_agents`, `forget` and `set_labels` stay in the caller's scope (a `set_labels` target must be in D(a)); `spawn` is the caller's spawn (`fleet.parent`, narrowing, quotas, and spawn approval exactly as a `Spawn` request: precheck, the pending request, its timeout, withdrawn if the connection closes); `check_spawn` is the quota dry run.

### The owned-prefix exception (`fleet_node::scope::child_labels_owned`)

When a workflow that owns prefix `p` (its manifest's `labels:p`) spawns for agent `a`:

- the request's keys in `p` are the workflow's (validation keeps the agent's own `labels` param out of `p`);
- **every** inherited key in `p` is dropped and the workflow's keys are set: the prefix describes the new child, all of it (a child in a new worktree must not keep its parent's `git.base`);
- everything outside `p` is `child_labels` unchanged: inherited, added to only, `name` and `fleet.*` not inherited, `fleet.parent = a`;
- the request's own labels may not touch `p`, and the workflow's keys must all be in `p` (`forbidden:` otherwise);
- `p` itself is checked: ASCII letters, digits, `-`, `_`, `.`, ending in `.`, 2–32 bytes, no `..`, never `fleet.`.

It is reachable only through `Node::spawn_for_workflow` / `precheck_spawn_for_workflow`, which only the workflow host calls; a plain `Spawn` from an agent still inherits `p`'s keys and cannot change them. It is safe for the reason 0090 gives: the values come from the workflow, not the agent. The user is never narrowed, so for the user the labels are taken as given.

### Running

- Each `RunAction` runs on a task of its own (like `Wait`), so the connection keeps serving. At most 4 per connection and 16 per daemon run at once; more wait (per-connection slot first, so a queue on one connection holds no daemon slot).
- The task is not a request in flight: a long action (or one waiting for spawn approval) never holds off a daemon restart. Instead each host call that changes the node (`spawn`, `set_labels`, `forget`, storage writes) is admitted on its own and refused with "fleetd is restarting" while one freezes the node; a `RunAction` arriving then is refused the same way.
- **Cancellation:** closing the connection sets the call's `CancelFlag` (and withdraws a pending spawn approval). The workflow's next host call fails with `cancelled`; calls after that run again so it can roll back what it did (remove the worktree it just made), except `spawn` and `check_spawn`, which stay cancelled. The task is never aborted mid-call.

### Storage

`workflow_kv(workflow, key, value BLOB, updated_ms, PRIMARY KEY (workflow, key))`, schema **13**. 64 KiB per value, 10,000 keys per workflow (overwriting an existing key is always allowed), keys 1–1024 bytes. `Storage` is `get`, `put`, `delete` and `keys(prefix)` (sorted; keys only, so a list never loads 10,000 values).

### CLI

`fleet action ls [--json]` and `fleet action run <name> [key=value]… [--json] [-- argv…]`. The CLI reads the action's schema first and parses each value by kind: a bare key sets a bool; `labels=k=v` repeats; durations take `ms`/`s`/`m`/`h` (bare = seconds); relative paths are made absolute against the shell's cwd, with `.`/`..` resolved by name. Views print as text (a diff as a unified diff, a table as aligned columns). Exit codes: 0 ok, 1 refused (the reason and the `key=value`s to add go to stderr), 2 usage or other error, 3 forbidden.

## Consequences

- Workstream C (git-worktree) adds its workflow to `builtin_workflows()` and calls `host.exec` once A's primitives replace the stubs; D renders `ActionSpec`s and `View`s from this proto.
- No new benchmark for this workstream (the spec's metrics are A's, C's and D's). Nothing runs at startup or idle: the registry and its semaphores do no work until an action is listed or run.
- M5 keeps `Workflow`, `Host`, `Storage`, `Manifest` and the action/view messages; changing them is a protocol and WIT change.
- M6's progress events and an explicit cancel take the spare fields 97–104.
