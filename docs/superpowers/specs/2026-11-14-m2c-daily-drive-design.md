# M2c — Daily-drive polish: design

**Status:** draft · 2026-11-14 · milestone M2c in `docs/plan.md`

## Intent

The user daily-drives fleetd for Claude Code sessions through fleet-gui. After M2b the GUI works but "does not feel very good": chrome, blur, typography, controls, motion and layout all feel off next to a native Mac app (the user's reference screenshot lives at `~/Developer/fleetd-ui-reference.png`, deliberately **outside** the repo). And exited agents pile up in the sidebar as noise, with no way to bring one back or get rid of it short of the 64-agent retention rule.

M2c has two tracks:

1. **Look and feel.** Find out whether gpui can feel native enough, by a time-boxed throwaway spike compared side by side with the reference, then do the full polish in whichever toolkit the user picks.
2. **Agent lifecycle.** Restart an exited agent and Remove (forget) exited or lost agents, per agent and in bulk, from the GUI and the CLI.

Success: the user prefers opening fleet-gui to a terminal tab, and the sidebar only shows agents they care about.

## Goals

- "Before" screenshots of the M2b GUI, then a ~2-day gpui restyle spike covering all six areas below, ending in side-by-side screenshots (before / spike / reference).
- A go/no-go decision by the user after about an hour of using the spike build.
- Restart: an exited or lost agent can be restarted with the same program, args, cwd and labels; the new agent (new id) **replaces** the old one, which is forgotten.
- Remove: an exited or lost agent can be forgotten (list entry and history gone), one at a time or all exited/lost at once ("Clear exited").
- Both in the GUI and the CLI; every connected client sees the same result.

## Non-goals

- Restarting or removing a **running** agent. Kill it first (existing Kill). No "kill and restart" shortcut in M2c.
- Restoring the original `SpawnAgent.env` on restart (see Decisions).
- Undo for Remove.
- An iOS app; it is a maybe and does not drive any decision here.
- The Swift/SwiftTerm spike, unless the go/no-go calls for it (below).

## Anti-goals

- Any regression of the perf budgets (cold start < 200 ms, input-to-render < 16 ms, 4 panes under the 50-agent flood) by the polish or by a rewrite. The CLAUDE.md performance rule applies unchanged.
- A restart that can leave **both** agents or **neither**: the old agent is forgotten only once the new one exists.
- A Remove that silently kills something: running agents are refused, never killed.
- Committing the reference screenshot or any user-shared image to the repo.

## Constraints

- `crates/fleet-gui/src/core` stays gpui-free, so a Swift (or iOS) client could reuse it through uniffi. Record this in **ADR 0029**.
- Protocol changes follow 0006/0022 conventions (protobuf in `crates/fleet-proto/proto/fleet.proto`, `fleet-cli` updated in the same change). New messages are additive; `PROTOCOL_VERSION` stays 2 (see Decisions).
- Benchmarks gate as usual (0014, 0017). Lifecycle work is off the hot path and must not move existing metrics.

## Approaches considered

### Lifecycle: where restart and remove live

- **A. In the node (picked by the user).** New `RestartAgent` and `ForgetAgents` requests; the node spawns from its stored record and forgets the old agent in one step, and pushes an `AgentRemoved` event to every connection so all clients swap or drop the tab in place. Atomic, one implementation shared by CLI and GUI. Costs new protocol messages and an ADR.
- **B. In the clients.** Restart = client sends `Spawn` with the old info, then `ForgetAgents`; only forget is added to the node. Less node work, but not atomic, other clients see a new tab appear and an old one vanish instead of a swap, and the replace logic is written twice (GUI, CLI). Rejected.

### Look and feel: toolkit

Decided earlier with the user: no up-front choice. Spike gpui first (cheapest, keeps all M2b work); if the remaining gap is gpui-impossible (native text fields, rubber-band scrolling, system accent colour, accessibility), spike a Swift/SwiftTerm window before deciding. The user makes the call after using it.

## Design

### Order of work

1. **Before screenshots** of the M2b GUI (the user grants Screen Recording to the terminal). Stored outside the repo, next to the reference.
2. In parallel:
   - **gpui restyle spike** (§1), on a throwaway branch `spike/gpui-restyle`.
   - **Agent lifecycle** (§2–§5), on `m2c`, through the normal plan/TDD/review flow.
3. **Go/no-go** by the user. Then a separate design for the full polish in the chosen toolkit (it reuses the spike's findings, not its code — keeping spike code is a new request).

### 1. gpui restyle spike (throwaway, ~2 days)

Covers all six areas, as agreed:

1. **Chrome:** transparent titlebar with inline traffic lights, the target name and actions in a unified translucent bar; window drag via objc2 `performWindowDragWithEvent:`. Side (vertical) agent tabs stay.
2. **Material:** blur (gpui `WindowBackgroundAppearance::Blurred`; an objc2 `NSVisualEffectView` material if that is not enough) with layered cards.
3. **Type and spacing:** SF Pro type scale, 4/8 px spacing grid.
4. **Controls:** native `NSMenu` context menus; real text fields (cursor, selection, ⌘A, undo).
5. **Motion:** hover states, transitions, momentum scrolling.
6. **Icons and terminal:** Lucide icons (MIT); SF Mono 13 as the terminal default, Menlo fallback. Dark first, light secondary.

No tests beyond what keeps it compiling; no perf gate on the spike branch, but cold start and input-to-render are measured once on it so the go/no-go knows the cost. Deliverables: side-by-side screenshots (before / spike / reference), a short findings note `docs/superpowers/notes/m2c-gpui-spike.md` listing what gpui could and could not do, and which gaps look gpui-impossible.

### 2. Protocol (additive, `PROTOCOL_VERSION` stays 2)

```proto
// Request.kind
RestartAgent restart = 13;
ForgetAgents forget = 14;

// Response.kind
Restarted restarted = 10;
Forgotten forgotten = 11;

// Event.kind
AgentRemoved agent_removed = 7;

// Restart an exited or lost agent: same program, args, cwd and labels, new
// id. The old agent is forgotten once the new one has started. `cols`,
// `rows` and `attach` mean what they mean in SpawnAgent.
message RestartAgent { uint64 agent_id = 1; uint32 cols = 2; uint32 rows = 3; bool attach = 4; }
// `attached` is set when `attach` was.
message Restarted { uint64 old_id = 1; AgentInfo agent = 2; optional AttachOk attached = 3; }

// Forget exited or lost agents: their list entries, final screens,
// history and scrollback rings are deleted. `exited_only` = every exited or
// lost agent whose labels contain every `selector` pair (empty selector:
// all of them); `ids` must then be empty. Running agents are never touched.
message ForgetAgents { repeated uint64 ids = 1; bool exited_only = 2; map<string, string> selector = 3; }
// `refused`: ids that are running or unknown, with nothing done to them.
message Forgotten { repeated uint64 ids = 1; repeated uint64 refused = 2; }

// Pushed to every connection when an agent is forgotten, after the
// request that did it is answered. `replaced_by` is set by a restart.
message AgentRemoved { uint64 agent_id = 1; optional uint64 replaced_by = 2; }

// AgentList gains the durable restart record, so a client that missed
// AgentRemoved can still follow a restart: old id -> its newest successor
// (chains are collapsed on write, so one lookup suffices).
message AgentList { repeated AgentInfo agents = 1; map<uint64, uint64> replaced = 2; }
```

- **Restart errors:** unknown id → `Error "unknown agent"`; running → `Error "agent is running"`; spawn failure (cwd gone, program missing) → `Error` with the spawn error, and **the old agent stays** untouched.
- **Forget is partial, not all-or-nothing:** each listed id is forgotten or refused on its own; `Forgotten` reports both lists. Forgetting an id twice (two clients racing) just refuses it the second time.
- **Ids are never reused** (see §3), so a stale id held by a script, an agent or a saved layout can only ever be refused, never land on an unrelated agent.
- Only exited or lost agents can be forgotten, and an attach to one already ends right after `AttachOk` and the exit event, so there is no live stream to cut; the server just clears the id from the connection's `attached` set.
- **Ordering:** the requesting connection queues `AgentRemoved` itself right after its response, and its broadcast forwarder skips events it caused (the trick `forward_exits` uses with `attached`). Other connections get it from the broadcast.
- **Lag:** a broadcast `Lagged` on the removal channel is not just logged (as `forward_exits` does): the forwarder sends nothing for the missed events, and clients recover through the next `ListAgents` (the GUI relists every 5 s), whose `replaced` map carries restarts.
- **Why no version bump:** checked in code: prost decodes an unknown `oneof` variant as `kind = None` and the server answers `Error "empty request"` without disconnecting. A new client treats that reply to `RestartAgent`/`ForgetAgents` as "daemon too old to restart/remove". Old clients ignore unknown event kinds (GUI `session.rs` `_ => {}`; `fleet-client::call()` skips unrelated frames) and an unknown `AgentList.replaced` field.

### 3. Node

- **Store schema v3** (in-place migration from v2, tested like the v1→v2 one):
  - **Ids never repeat.** Today `agents.id INTEGER PRIMARY KEY` hands out max(id)+1, so forgetting the newest agent would give its id to the next spawn. v3 rebuilds the table with `INTEGER PRIMARY KEY AUTOINCREMENT` (SQLite then never reuses an id, including after failed spawns), preserving existing rows and `agent_labels`.
  - **`agent_replacements(old_id PRIMARY KEY, new_id)`**: written by a restart in the same transaction that deletes the old row; any row whose `new_id` is the old id is repointed (chains collapse). Served as `AgentList.replaced`. Bounded: the rows whose `new_id` no longer exists are deleted when that agent is itself forgotten without replacement; the table never holds more rows than agents ever restarted and still reachable. The GUI only needs it for ids in its saved layouts.
- `Node::restart(id, cols, rows, attach)`:
  - Check the agent exists and is not running; refuse a second concurrent restart of the same id through a `restarting` guard set (like `Histories.spawning`). The spawn itself runs **outside** the agents lock: `Node::start` takes that lock itself and waits on the base environment and PTY spawn, which must not block `on_exit`.
  - Read program/args/cwd/labels from the store and spawn through the existing path (base environment per 0024, fresh `FLEET_AGENT_ID`). Labels are copied as stored, validated **without** the reserved-`fleet.` check that client-supplied spawn labels get (none are set today; this keeps a future core label).
  - Then in one store transaction: delete the old row and labels, write the replacement. Then delete the old final screen, history dir and scrollback ring, and announce `AgentRemoved{old, replaced_by: new}`.
  - A crash between spawn and the transaction leaves both rows (old exited, new running), never neither. Retention prunes history dirs and final screens, **not** store rows, so the old row stays until the user removes it; 0030 records this.
- `Node::forget(ids | exited_only + selector)`: for each non-running id: drop it from `Agents.exited`, `Store::delete_agent`, delete its history dir via `remove_history_dir` (closing an open handle as `HistoryHandles::delete` does), and delete its `<state>/scrollback/<id>.ring` (raw output, may contain secrets; today never deleted). Then announce `AgentRemoved`. Retention (0021) is unchanged; forgetting frees slots early.
- The daemon (`crates/fleetd/src/server.rs`) fans `AgentRemoved` out the way `forward_exits` fans out exits (a broadcast channel, every connection, ordered after the response to the requester).

### 4. CLI

- `fleet restart <id> [--attach]` — prints the new id; `--attach` attaches like `fleet spawn --attach`.
- `fleet rm <id>...` — forgets; prints refused ids with the reason ("running"/"unknown") and exits non-zero if any were refused.
- `fleet rm --exited [--selector k=v]...` — all exited and lost agents (optionally only those matching the labels); prints how many.
- Against an older daemon both commands print "daemon too old to restart/remove" (the `Error "empty request"` reply).
- The fleet skill text (0024) gains one line each for restart and rm.

### 5. GUI

- **Sidebar row menu** (right-click, today: Kill, Open in new split): for an exited or lost row add **Restart** and **Remove**; Kill stays for running rows only.
- **Exited pane:** the pane that shows an exited agent's final screen gets a small **Restart** affordance (button in the pane's exit banner) and ⌘R while that pane is focused. Restart from a pane uses that pane's size and `attach: true`, so the new agent starts attached in the same pane.
- **Clear exited:** a sidebar action (header button and the app/status menu) sending `ForgetAgents{exited_only}` with no selector: every exited or lost agent on that node, including ones spawned by scripts or other agents. No confirmation dialog — it only touches agents that already ended; the count is shown in the button tooltip.
- **Restart from a pane** goes over that pane's own connection (`gui-pane-<n>`), so `attach: true` lands in that pane; from the sidebar it goes over the control connection without attach, and panes follow via `AgentRemoved`.
- **On `AgentRemoved`:** the GUI holds one control connection plus one per pane, so it receives each event several times; it acts on the control connection's copy and ignores the rest (idempotent anyway). The sidebar drops the row. Every pane (in every saved layout of that target) showing the old id switches to `replaced_by` if set — attaching to it if this client did not start it — or becomes an empty pane if not. Saved layouts are rewritten the same way.
- **Missed events:** on every `ListAgents` reply (connect, reconnect, the 5 s relist) the GUI applies `AgentList.replaced` to its panes and saved layouts the same way, and empties panes whose id is neither listed nor replaced. So a `fleet restart` done while the GUI was closed is followed on the next open.
- All of this logic lives in `fleet-gui/src/core` (session + layout), gpui-free per ADR 0029; the app layer only renders and dispatches.

### 6. Deferred design point from M2b

The primary pane resizes its local grid before the node confirms (immediate drags vs a brief garble). Revisit during the full-polish design after the go/no-go, since a native rewrite would change the answer.

### 7. Benchmarks

No new hot-path code. Add one wall metric only if cheap: `restart_agent_ns` (request to `Restarted`), report-only. Run `fleet-perf run --suite wall` and `compare` before finishing, per CLAUDE.md.

## ADRs to write

- **0029 — fleet-gui core stays toolkit-free:** `fleet-gui/src/core` has no gpui dependency so a Swift/iOS client could reuse it via uniffi.
- **0030 — Agent restart and forget:** the node-side design above (requests, event, replacement record, ids never reused, atomicity order, refusals, env not restored, retention never prunes store rows), amending 0021's retention and adding to 0022's message list.

## Decisions (agreed with the user)

- Restart **replaces** the old agent: the new one takes its tab, the old one is forgotten with its history.
- Restart and forget are **node requests** (approach A).
- `env` is **not stored** and not restored on restart: the new agent gets the node's base environment plus labels. Storing it would put secrets in SQLite (0012). Documented in 0030 and in `fleet restart --help`.
- A GUI that missed a restart (closed, reconnecting, lagged) still **follows it** to the new agent: the node keeps a durable old→new record (served in `AgentList.replaced`).
- **Clear exited** removes every exited or lost agent on the node, not just one group; the CLI adds an optional label selector.
- All six spike areas, side tabs stay, go/no-go is the user's after an hour of use, perf budgets bind any rewrite.

## Decided after the builder check (my calls)

- Agent ids are never reused (schema v3, `AUTOINCREMENT`) — without it Remove makes id reuse routine and stale ids hit unrelated agents.
- Forget also deletes the agent's scrollback ring file.
- Restart's spawn runs outside the agents lock with a per-id `restarting` guard; restart copies stored labels without the reserved-key check.
- `AgentRemoved` ordering via the requester queuing it itself; lag recovery via relist rather than replaying events.
- No protocol version bump: verified the server answers unknown requests with `Error "empty request"` and stays connected.

## Left to the builder

- Exact names of Rust functions and CLI output wording.
- Whether `AgentRemoved` reuses the exit broadcast channel or gets its own.
- Placement and styling of the Restart / Clear exited controls (the spike may restyle them anyway).
