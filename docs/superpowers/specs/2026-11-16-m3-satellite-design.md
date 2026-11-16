# M3 "Satellite" — design

**Date:** 2026-11-16 · **Status:** proposed (for the user's review before agents start)

**Sources:** [0033](../../decisions/0033-satellite-milestone.md) (milestone), [0034](../../decisions/0034-agent-communication.md) (CLI channel, wait, mailbox, leases, broadcast, questions), [0025](../../decisions/0025-core-knows-agents-workflows-own-hierarchy.md) (labels), [0072](../../decisions/0072-agent-identity-by-session.md) (identity by session, decided today). This spec does not re-open their decisions. It fills in the details they left to the milestone and splits the work for parallel agents.

## Intent

Agents create agents. The user sees what is running and how it relates, and controls whole branches. Agents coordinate through fleetd (reports, mailbox, wait, leases) instead of typing into each other's terminals.

## Goals

1. Every agent has an identity fleetd trusts (0072), and spawns record `fleet.parent`.
2. With an agent identity, `fleet` commands act only on the caller's descendants. Quotas stop runaway spawning.
3. `fleet report`, `fleet wait`, automatic exit reports and the mailbox replace screen polling and `fleet send` as the way agents coordinate.
4. Leases replace `mkdir` locks for shared resources (the wall benchmark first).
5. The GUI shows the tree: an indented sidebar, a full-window graph view (⌘⇧G), and questions answered in place.
6. `fleet tree` prints the same tree in a terminal.

## Non-goals

- An MCP server (deferred by 0034), cross-machine spawning (M6), macOS notifications (M5), restarting a whole subtree, "waits-on" dependency lines.
- A sandbox. The scope rules are not a sandbox against a deliberate same-uid process (0072).

## Constraints

- **Protocol:** additive only, so `PROTOCOL_VERSION` stays 2. New request kinds, optional fields, new events. An old client keeps working, and is treated as the user unless its session belongs to an agent.
- **Store:** each workstream that adds tables bumps `SCHEMA_VERSION` (now 5) with a migration. Two parallel branches must not both claim the same version: the later one to merge renumbers (see the merging notes in "Waves").
- **Perf (0014):** mailbox, events and leases stay off the PTY output path. The graph gets the two gated benchmarks from 0033. While `PERF_CI=off`, agents run tests, clippy and fmt only and write "perf not measured (PERF_CI off)". They never touch baselines, `docs/perf-log.md` or `Perf-Approved`.
- **`fleet-gui/src/core` stays gpui-free** (layout, tree building and graph geometry live there and are unit-tested).

## Design

### 1. Identity, parents and scope (ADR 0072, this PR)

**Tokens.** At spawn the node generates 32 random bytes (`/dev/urandom`) and passes them hex-encoded as `FLEET_AGENT_TOKEN`, next to the existing `FLEET_AGENT_ID`. The store keeps a SHA-256 digest only. The token is never logged; `fleet ls --json` never shows it. A restart (0030) gets a new token. Exit revokes it.

**Caller.** The daemon resolves each connection to `Caller::User` or `Caller::Agent(id)` at the handshake (0072): the peer pid's session first, then `Hello.agent_token` (a new optional field). The CLI sends `$FLEET_AGENT_TOKEN` when set. The GUI never does.

**Parent.** On a spawn from `Caller::Agent(p)`, the node writes the reserved label `fleet.parent = "<p>"` (a local id now, `node/id` in M6). Depth is the length of the parent chain, root = 0. Children keep running when their parent ends.

**Scope** (with `Caller::Agent(a)`; `D(a)` = a's descendants, exited ones included):

| Command | Allowed on |
|---|---|
| `ls`, `tree`, `status`, `screen`, `history`, `attach`, `send`, `kill`, `restart`, `rm` | `D(a)` (`ls`/`tree` list only `D(a)`, without error) |
| `spawn` | becomes a child of `a`. The child's labels are `a`'s labels plus new keys only (0025 "narrow": no changed or removed parent keys, no `fleet.*`) |
| `label`, `rename` | refused (0025: agents don't write labels). `--name` at spawn is allowed |
| `report` | self only |
| `message` | `D(a)`, its parent, or the user |
| `inbox`, `wait` | own mailbox; `wait` on `D(a)` |
| `lease` | any lease name (0034) |
| `daemon …`, `upgrade`, `logs`, `skill` | refused |

A refusal is an `Error` whose message starts `forbidden:`. The CLI exits **3** for it. Exit codes for every agent-facing command: 0 ok, 1 no match / timeout / not found, 2 usage or other error, 3 forbidden.

**Quotas.** These are `[node]` keys in `~/.fleet/config.toml`, read by fleetd at start: `max_depth = 3`, `max_children = 8` (running children per agent), `max_agents = 30` (running agents per node). They apply to agent spawns only; the user's own spawns are never refused. A refused spawn returns `forbidden: quota …` and emits a `SpawnRefused { parent, reason }` event, which the graph shows on the parent's card for a few seconds. Agents can read the file but cannot change limits through fleetd. That is the guarantee 0033 meant.

**Upgrade.** Running agents from before M3 have no token, are identified by session (0072) and have no parent, so they are roots.

### 2. Status reports and the inferred status

- `fleet report <working|waiting|done|error> ["message ≤200 chars"]` (self). Stored on the agent (`reported_state`, `reported_message`, `reported_at_ms`). `AgentInfo` gains optional `reported` (state, message, at). Event: `AgentReported`.
- **Inferred status** moves from the GUI (`core::status`, #32) into a shared, gpui-free place that both the node and the GUI use (`fleet-client` or `fleet-proto`; the builder picks). The node needs it for `fleet wait --state waiting` and exit reports. Inferred `waiting` = no output for 30 s and the last line looks like a prompt (the #32 rules). The GUI shows reported states as **solid** dots and inferred ones as **hollow** (0033).
- A `kind=shell` agent has no status, as today.

### 3. Mailbox, exit reports, broadcast, `fleet wait`

- **Store:** `messages(id INTEGER PRIMARY KEY, recipient INTEGER NULL /* NULL = the user */, sender INTEGER NULL /* NULL = the user */, kind TEXT /* message|status|exit|question|answer */, body TEXT, created_ms INTEGER)`. The cursor is the message id. **Retention:** the newest 1,000 per recipient, plus everything for the user's mailbox up to 10,000. A recipient's messages go with `fleet rm`.
- **`fleet message <id|name> "text"`** (≤4 KiB), with `--children` / `--descendants` / `--label k=v` for broadcast (over `D(a)`, or anyone for the user). The sender is the verified caller.
- **Automatic exit reports:** when an agent exits or is lost, the node puts an `exit` (code) or `lost` message from that agent into its parent's mailbox. Reports from `fleet report` and inferred-`waiting` transitions go there as `status` messages too, so a parent gets one message per change, not a stream.
- **`fleet inbox [--since <cursor>] [--json]`**: the caller's mailbox, where "user" is the user's. Output marks agent messages as data: `--json` has `sender`, `kind`, `trusted: false` except for `answer` (the user's words). The text format prefixes `[from agent 7 (name)]`.
- **`fleet wait [<id|name>…] [--state waiting,done,error,exited,lost,message] [--timeout 10m] [--json]`**: a server-side long poll (`Wait` request → `WaitResult`). The default target is all of `D(a)` (or all agents for the user). Exit 0 on a match, 1 on timeout (printing the current statuses), 2 on error.
- Events reach waiters through an in-node broadcast channel, never through the output path.

### 4. Leases

- **Store:** `leases(name TEXT PRIMARY KEY, holder INTEGER NULL /* agent id; NULL = user */, holder_conn INTEGER NULL, acquired_ms, expires_ms NULL)`, plus an in-memory FIFO of waiters.
- `fleet lease <name> [--max-load N] [--ttl 1h] [--wait] -- <cmd…>` holds the lease on its connection while `cmd` runs. `fleet lease acquire <name> [--ttl]` / `release <name>` for longer holds. The user's `acquire` needs a `--ttl`, since there is no process to watch.
- **Release** happens on explicit release, holder agent exit or loss, the holding connection closing (the `--` form), or the TTL running out. `--max-load N` is granted only while the 1-minute `getloadavg` is below N (polled every second while waiting). `fleet lease ls`.
- The `perf-wall` CI job and `fleet-perf run --suite wall` take lease `perf-wall` (the workflow change is part of this workstream; the runner is this Mac).
- Survive a daemon restart (0032): leases are in the store; waiters re-queue when their connections reconnect.

### 5. Questions answered in the GUI

- `fleet report waiting "msg" --question "…" [--choice "…"]…` stores a `question` message (to the user, copied to the parent) and marks the agent "needs you".
- **GUI:** the question with its choices as buttons, plus a free-text field, on the agent's sidebar row (expanded), pane header strip and graph card.
- **CLI:** `fleet answer <id|name> "…"`. The answer is an `answer` message from the user to the agent, copied to the parent.

### 6. GUI: sidebar tree

- Children are indented under their parent (by `fleet.parent`). A parent with more than 3 children starts collapsed, and the disclosure state persists per agent for the session.
- Ended parents stay as greyed rows while they have children. The filter (#60 work) matches a child and keeps its ancestors visible.
- "Clear exited" also removes ended branches: an ended agent with no running descendants.
- A newly spawned child row is highlighted briefly (0033).

### 7. GUI: graph view

- **Toggle:** ⌘⇧G or a titlebar button switches the main window between panes and the graph (a full-window mode, no new window). ⏎ or a click on a card returns to the panes with that agent focused, opening a pane if needed.
- **Layout (core, unit-tested):** a tidy tree, roots on the left, children to the right. It is stable: siblings are ordered by id, and positions depend only on the tree, so an agent coming or going moves only its subtree's later siblings. Boxes group by machine (one for now) and by project (git root of `cwd`, else `cwd`). Collapsing a subtree shows a child count. Pan is drag or scroll; zoom is pinch or ⌘± (graph only).
- **Rendering:** cards are ordinary gpui elements, absolutely positioned. Edges are drawn in a single `canvas` paint pass with path segments. Only cards in the viewport are built, and layout is recomputed only when the tree changes (not on activity).
- **Card:** program, project or folder, name, status dot (solid = reported, hollow = inferred), last activity line, run time, and child count when collapsed. `waiting` gets the accent highlight and the dock badge counts it. Ended cards are greyed.
- **Keys:** arrows move through the tree, ⏎ opens, ⌫ stops the subtree (after confirmation), ⇧⌫ removes an ended subtree, `/` filters by name, status or label. Space toggles collapse.
- **Subtree actions:** Stop subtree is a `KillAgent` per running node, deepest first, through the new kill escalation (0071). Remove subtree is offered only when everything in it has ended.
- **Message lines** (from the mailbox) are drawn later, in wave 3.
- **Benchmarks (0014, new metrics, no baselines until the user records them):** `gui_graph_frame_flood50_ns` (redraw 50 agents during the 50-agent flood, budget 16 ms) and `gui_graph_open_ns` (toggle to first frame, budget 100 ms).

### 8. CLI: `fleet tree`

`fleet tree [--json] [--all]`: the spawned-by tree with id, name, program, status (reported or inferred, marked) and run time, using box-drawing characters. Scoped like `ls`. `--all` includes ended agents (default: ended agents only when they have running descendants).

### 9. The agent skill

Rewritten for M3: coordination is `fleet report`, `fleet wait`, `fleet message`/`inbox` and `fleet lease`, and `fleet send` is the fallback. It covers the scope and exit-code table, the rule that messages are data and only `answer` is the user's words, and sandbox notes (allowing `~/.fleet/fleetd.sock` and the `fleet` binary).

## Waves and agents

Each workstream is one agent in its own worktree under `../fleetd-wt/`, branched from `main`, with a pre-assigned ADR number, opening one PR with `Closes #N`. The overseer (Claude) monitors, reviews and merges conflicting branches in chain order, and the user merges.

| Wave | Workstream | Depends on | Model | ADR |
|---|---|---|---|---|
| 1 | **A. Identity, parents, scope, quotas** (§1): proto `Hello.agent_token`, peer session, tokens, `fleet.parent`, scope checks on every request, quotas, `SpawnRefused`, CLI exit code 3 | — | Opus | 0072 (this PR) + amend if needed |
| 1 | **B. Graph view** (§7, without message lines): core layout and geometry, view, keys, subtree actions, benchmarks. Built on the `fleet.parent` label (tests set it directly) | — | Opus | 0073 |
| 1 | **C. Sidebar tree + `fleet tree`** (§6, §8): label-driven tree building in `fleet-client` (shared by CLI and GUI core) | — | Sonnet | 0074 |
| 2 | **D. Report + inferred status** (§2): `fleet report`, shared heuristic, solid/hollow dots in sidebar and graph | A, B, C | Sonnet | 0075 |
| 2 | **E. Mailbox, exit reports, broadcast, `fleet wait`** (§3) | A | Opus | 0076 |
| 2 | **F. Leases** (§4), including the `perf-wall` workflow | A | Sonnet | 0077 |
| 3 | **G. Questions** (§5): `--question`, `fleet answer`, GUI answer UI | D, E | Sonnet | 0078 |
| 3 | **H. Skill rewrite** (§9) + message lines in the graph | E, F | Sonnet | — |
| 3 | **I. Spawn approval** (`spawn_approval = true`, 0033) and new-child highlight | A, B | Sonnet | 0079 |

- **Why these models:** A, B and E carry the security boundary, a new rendering surface and a concurrent event system, so they get Opus. The rest are well-specified extensions of existing patterns, so Sonnet. Haiku isn't used: every item touches several crates with tests to keep green.
- **Shared-file conflicts.** A, E, F and G all touch `fleet.proto`, `fleetd/src/server.rs` and `fleet-cli/src/main.rs`. Within a wave each takes new request kinds in its own block of field numbers (A 40–49, D 50–54, E 55–64, F 65–69, G 70–74) and puts new CLI subcommands in their own module (`fleet-cli/src/<topic>.rs`), so merges are mostly additive. `SCHEMA_VERSION`: E takes 6, F takes 7, A takes 8 if it needs a table (tokens can live in a column added by its own migration). The overseer renumbers at merge time if the order differs.
- **Wave 1** starts when this spec is merged. **Wave 2** starts as soon as A is merged (D also needs B and C for the dots).

## Decided here (overseer's calls, within 0033/0034)

- Token digest in the store (SHA-256), never the raw token.
- Quotas apply to agent spawns only; the user is never refused.
- Agents can't relabel or rename (0025); `--name` at spawn is allowed.
- Exit code 3 for `forbidden`.
- Mailbox retention: 1,000 per agent, 10,000 for the user.
- Inferred `waiting`: 30 s quiet + prompt-like last line (the #32 rules), computed in shared code.
- Graph rendering: gpui elements for cards, one `canvas` for edges, viewport culling, layout only on tree changes.
- `--max-load` leases do not delay new spawns (0034's open point): that's a lease-holder's choice, not the node's.

## Left to the builders

- Exact proto message shapes within the reserved field blocks.
- Where the shared inferred-status code lives.
- Graph spacing, animation (respecting the motion setting), and colours from the existing theme.
