# M2c Agent Lifecycle (Restart / Remove) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restart an exited/lost agent (new id, replaces the old one) and Remove/forget exited/lost agents, per agent and in bulk, through new node requests used by the CLI and the GUI.

**Architecture:** The node owns both operations (approach A): `RestartAgent` spawns from the stored record and forgets the old agent; `ForgetAgents` deletes rows, final screens, history and scrollback rings. An `AgentRemoved` event reaches every connection, and a durable old→new record (`AgentList.replaced`) lets clients that missed the event follow restarts. The GUI's logic lives in gpui-free `fleet-gui/src/core`.

**Tech Stack:** Rust, prost/protobuf (`crates/fleet-proto/proto/fleet.proto`), rusqlite (store), tokio (fleetd server), clap (CLI), gpui 0.2.2 (GUI app layer only).

**Spec:** `docs/superpowers/specs/2026-11-14-m2c-daily-drive-design.md` (§2–§5, §7, ADRs). The gpui restyle spike (§1) is **not** in this plan; it runs separately on `spike/gpui-restyle`.

## Global Constraints

- Only **exited or lost** agents can be restarted or forgotten; running agents are refused, never killed.
- Restart copies program, args, cwd and labels; `env` is **not** stored or restored (base env per 0024 + fresh `FLEET_AGENT_ID`).
- Restart atomicity: the old agent is forgotten only after the new one started; a failed spawn leaves the old agent untouched.
- Agent ids are **never reused** (store schema v3, `AUTOINCREMENT`).
- `PROTOCOL_VERSION` stays 2; all proto changes are additive. Old daemons answer new requests with `Error "empty request"`; clients show "daemon too old to restart/remove".
- `crates/fleet-gui/src/core` must not depend on gpui or AppKit.
- Never log label values, args or env (existing rule in `node.rs`).
- Perf rule (CLAUDE.md): before finishing, `fleet-perf run --suite wall` and `fleet-perf compare`; any regression → stop and ask the user. Never touch `bench/baselines.toml`, never add `Perf-Approved:`, never write `docs/perf-log.md`. Check `gh run list --workflow ci.yml --status in_progress` first.
- Commits: no `Co-Authored-By` trailer (user preference).

## Review Focus

1. **Restart of an agent whose cwd was deleted** → `Error` with the spawn error, old agent still listed and restartable. (Task 4 test `restart_failure_keeps_old_agent`.)
2. **Two clients restart the same agent at once** → exactly one new agent; the other gets `Error` ("unknown agent" or "agent is being restarted"). (Task 4 test `concurrent_restart_creates_one_agent`.)
3. **Forget the newest agent, then spawn** → the new agent gets a fresh id, not the forgotten one. (Task 1 test `ids_are_not_reused_after_delete`, Task 3 test `forgotten_id_is_not_reused`.)
4. **GUI closed while `fleet restart` runs; GUI reopens** → its pane shows the new agent. (Task 7 test `list_follows_replacement_into_layout`.)
5. **A restart chain (A→B, then B→C) seen only via the list** → a pane on A lands on C. (Task 1 test `replacement_chains_collapse`.)

---

### Task 1: Store schema v3 — ids never reused, replacement record

**Files:**
- Modify: `crates/fleet-node/src/store.rs` (migration, new methods, unit tests in its `mod tests`)

**Interfaces:**
- Produces:
  - `const SCHEMA_VERSION: i64 = 3;`
  - `Store::agent(&self, id: u64) -> rusqlite::Result<Option<AgentRecord>>` (with labels)
  - `Store::forget_agent(&self, id: u64) -> rusqlite::Result<bool>` — deletes row (labels cascade) and any `agent_replacements` rows whose `new_id = id`; true if a row was deleted. Refuses (returns `Ok(false)`, deletes nothing) if the agent's state is `running`.
  - `Store::replace_agent(&self, old: u64, new: u64) -> rusqlite::Result<()>` — one transaction: delete `old`'s row; `UPDATE agent_replacements SET new_id = new WHERE new_id = old`; `INSERT OR REPLACE INTO agent_replacements (old_id, new_id) VALUES (old, new)`.
  - `Store::replacements(&self) -> rusqlite::Result<HashMap<u64, u64>>`

- [ ] **Step 1: Write failing tests** in `store.rs` `mod tests`:
  - `ids_are_not_reused_after_delete`: insert agents 1, 2; `forget_agent(2)` after `set_exited(2, 0)`; next `insert_agent` returns 3.
  - `v2_database_migrates_to_v3_keeping_rows_and_labels`: build a v2 DB by hand (the v1+v2 DDL as in `migrate`, `PRAGMA user_version = 2`, one agent id 7 with label `project=x`), `Store::open` it; `agent(7)` has the label; `user_version` is 3; next insert returns 8; after forgetting 8, the next insert returns 9.
  - `forget_refuses_running`: running agent → `forget_agent` returns `false`, `agent(id)` still `Some`.
  - `replacement_chains_collapse`: agents 1,2,3; exit 1; `replace_agent(1, 2)`; exit 2; `replace_agent(2, 3)` → `replacements() == {1: 3, 2: 3}`; `agent(1)` and `agent(2)` are `None`.
  - `forgetting_a_successor_drops_its_replacements`: `replace_agent(1, 2)`, exit 2, `forget_agent(2)` → `replacements()` empty.
- [ ] **Step 2:** `cargo test -p fleet-node --lib store` — new tests FAIL (missing methods / reused id).
- [ ] **Step 3: Implement.** Migration `version < 3`: run with `PRAGMA foreign_keys = OFF` (it cannot change inside a transaction; set it before `BEGIN`, restore `ON` after `COMMIT`), then in one transaction: `CREATE TABLE agents_v3 (... id INTEGER PRIMARY KEY AUTOINCREMENT ...)` with the same columns/check, `INSERT INTO agents_v3 SELECT * FROM agents`, `DROP TABLE agents`, `ALTER TABLE agents_v3 RENAME TO agents`, `CREATE TABLE agent_replacements (old_id INTEGER PRIMARY KEY, new_id INTEGER NOT NULL)`, `PRAGMA user_version = 3`. Run `PRAGMA foreign_key_check` before commit and fail the migration if it returns rows. The v1 `CREATE TABLE agents` for fresh DBs stays as is (v3 then rebuilds it — fresh DBs are empty, so it is cheap and keeps one path).
- [ ] **Step 4:** `cargo test -p fleet-node --lib store` — PASS.
- [ ] **Step 5:** Commit `feat(node): store v3, agent ids are never reused, replacement record`.

### Task 2: Protocol messages

**Files:**
- Modify: `crates/fleet-proto/proto/fleet.proto`
- Modify: every construction site of `AgentList { agents }` (`crates/fleetd/src/server.rs`, tests) — add `..Default::default()` or `replaced`.
- Test: `crates/fleet-proto/src/frame.rs` tests (round-trip)

**Interfaces:**
- Produces (exactly as spec §2): `Request.kind`: `RestartAgent restart = 13; ForgetAgents forget = 14;` `Response.kind`: `Restarted restarted = 10; Forgotten forgotten = 11;` `Event.kind`: `AgentRemoved agent_removed = 7;` messages `RestartAgent { uint64 agent_id = 1; uint32 cols = 2; uint32 rows = 3; bool attach = 4; }`, `Restarted { uint64 old_id = 1; AgentInfo agent = 2; optional AttachOk attached = 3; }`, `ForgetAgents { repeated uint64 ids = 1; bool exited_only = 2; map<string, string> selector = 3; }`, `Forgotten { repeated uint64 ids = 1; repeated uint64 refused = 2; }`, `AgentRemoved { uint64 agent_id = 1; optional uint64 replaced_by = 2; }`, `AgentList.replaced = 2` (`map<uint64, uint64>`). Copy the spec's comments onto each message.

- [ ] **Step 1:** Test `restart_and_forget_frames_round_trip` in `frame.rs`: encode/decode a `Request` with `RestartAgent{agent_id: 5, cols: 80, rows: 24, attach: true}`, an `Event` with `AgentRemoved{agent_id: 5, replaced_by: Some(9)}`, and an `AgentList` with `replaced = {5: 9}`; decoded equals original.
- [ ] **Step 2:** `cargo test -p fleet-proto` — FAIL (types missing).
- [ ] **Step 3:** Add the messages; fix construction sites until `cargo build --workspace` passes. Add `_ => {}` handling where a `match` on `request::Kind` is now non-exhaustive in fleetd **only as a temporary `error("not implemented")` arm** (Task 5 replaces it).
- [ ] **Step 4:** `cargo test -p fleet-proto && cargo build --workspace --all-targets` — PASS.
- [ ] **Step 5:** Commit `feat(proto): RestartAgent, ForgetAgents, AgentRemoved, AgentList.replaced`.

### Task 3: Node — forget and the removal broadcast

**Files:**
- Modify: `crates/fleet-node/src/node.rs`
- Create: `crates/fleet-node/tests/node_lifecycle.rs` (copy `spawn_sh`-style helpers from `tests/node_history.rs`)

**Interfaces:**
- Consumes: Task 1 `Store::forget_agent`, `Store::replacements`.
- Produces:
  - `pub struct Removed { pub event: fleet_proto::AgentRemoved, pub origin: Option<ConnId> }` (Clone)
  - `Node::removed_events(&self) -> broadcast::Receiver<Removed>` (channel capacity like `exits`)
  - `pub struct ForgetOutcome { pub forgotten: Vec<u64>, pub refused: Vec<u64> }`
  - `Node::forget(&self, ids: &[u64], exited_only: bool, selector: &Labels, origin: Option<ConnId>) -> io::Result<ForgetOutcome>` — `exited_only` with non-empty `ids` is `Err(InvalidInput)`.
  - `Node::replacements(&self) -> io::Result<HashMap<u64, u64>>`
  - private `Node::drop_agent_files(&self, id: u64)` — removes from `Agents.exited`, deletes history via `Histories::delete` (rename its log line from "beyond retention" to take a reason), deletes `scrollback_dir/<id>.ring` (NotFound ignored), calls `ActivitySinks::forget(id)`.

- [ ] **Step 1: Failing tests** in `node_lifecycle.rs`:
  - `forget_removes_exited_agent_everywhere`: spawn `sh -c 'echo hi; exit 3'`, wait exited; `forget(&[id], false, &empty, None)` → `forgotten == [id]`; `list()` lacks it; `attach(id, ..)` is `Err`; `<state>/history/<id>` and `<state>/scrollback/<id>.ring` do not exist; `retained_exited_agents()` decreased by 1.
  - `forget_refuses_running_and_unknown`: running agent and id 9999 → both in `refused`, running agent still alive (send input, see echo).
  - `forget_exited_only_respects_selector`: three exited agents, two with `project=a` → `forget(&[], true, {project: a}, None)` forgets exactly those two.
  - `forget_announces_removed`: subscribe `removed_events()` first; forget → receive `Removed{event: AgentRemoved{agent_id: id, replaced_by: None}, origin: None}`.
  - `forgotten_id_is_not_reused`: forget the newest agent, spawn → new id > forgotten id.
- [ ] **Step 2:** `cargo test -p fleet-node --test node_lifecycle` — FAIL.
- [ ] **Step 3:** Implement. For `exited_only`, collect non-running ids from `list_matching(selector)`. Per id: `store.forget_agent(id)` → if false → refused (running or unknown); else `drop_agent_files(id)` and broadcast `Removed` (ignore send error when no receivers). Log `info!(agent = id, "agent forgotten")`.
- [ ] **Step 4:** `cargo test -p fleet-node` — PASS (all, including existing retention tests).
- [ ] **Step 5:** Commit `feat(node): forget exited agents with their history and scrollback`.

### Task 4: Node — restart

**Files:**
- Modify: `crates/fleet-node/src/node.rs`
- Test: `crates/fleet-node/tests/node_lifecycle.rs`

**Interfaces:**
- Consumes: Task 1 `Store::agent`, `Store::replace_agent`; Task 3 `Removed`, `drop_agent_files`.
- Produces:
  - `Node::restart(&self, id: u64, cols: u32, rows: u32, origin: Option<ConnId>) -> io::Result<AgentInfo>`
  - `Node::restart_attached(&self, id: u64, cols: u32, rows: u32, conn: ConnId, before_start: impl FnOnce(u64)) -> io::Result<(AgentInfo, AttachOk, Subscription)>`
  - Errors: unknown → `NotFound` "unknown agent {id}"; running → `InvalidInput` "agent {id} is running"; restart already in progress → `WouldBlock` "agent {id} is being restarted".

- [ ] **Step 1: Failing tests:**
  - `restart_replaces_exited_agent`: spawn `sh -c 'echo run-$FLEET_AGENT_ID; exit 0'` with labels `{project: x}` in a temp cwd; wait exited; `restart(old, 0, 0, None)` → new id ≠ old, same program/args/cwd/labels, state running; `list()` has new, lacks old; `replacements() == {old: new}`; the new agent's screen shows `run-<new>`; removed event `{old, replaced_by: Some(new)}`.
  - `restart_refuses_running_and_unknown`: running → `Err` kind `InvalidInput`; 9999 → `NotFound`; running agent untouched.
  - `restart_failure_keeps_old_agent`: spawn in a temp dir, wait exited, delete the dir, `restart` → `Err`; `list()` still has old; `replacements()` empty; no new row.
  - `concurrent_restart_creates_one_agent`: exited agent; two threads call `restart(old, ..)` behind a `Barrier` → exactly one `Ok`; `list()` has exactly one new running agent.
  - `restart_attached_gets_first_output`: like `spawn_attached` tests: subscription receives the new agent's first output.
  - `restart_does_not_restore_env`: spawn with `env {SECRET: s}` printing `$SECRET`, exit; restart; new screen does not contain `s`.
- [ ] **Step 2:** `cargo test -p fleet-node --test node_lifecycle restart` — FAIL.
- [ ] **Step 3:** Implement:
  - Move `check_labels(&req.labels)?` out of `start` into `spawn` and `spawn_attached` (stored labels were validated when first set; restart does not re-check them, so a future core `fleet.` label survives).
  - Add `restarting: HashSet<u64>` to `Agents`. Restart: under the agents lock, insert into `restarting` (present → `WouldBlock`), release. A guard struct removes the id on drop.
  - Read `store.agent(id)`: `None` → `NotFound`; `Running` → `InvalidInput`. Build `SpawnAgent { program, args, cwd, cols, rows, labels, env: empty, attach: false }` and call `start` (outside the agents lock).
  - On success: `store.replace_agent(old, new.id)`; `drop_agent_files(old)`; broadcast `Removed{AgentRemoved{old, Some(new)}, origin}`. If `replace_agent` fails, log a warning and still return the new agent (both rows remain — never neither).
- [ ] **Step 4:** `cargo test -p fleet-node` — PASS.
- [ ] **Step 5:** Commit `feat(node): restart an exited agent in place of the old one`.

### Task 5: fleetd — serve restart/forget, fan out AgentRemoved

**Files:**
- Modify: `crates/fleetd/src/server.rs`
- Create: `crates/fleetd/tests/agent_lifecycle.rs` (reuse `Daemon`/`Client` helpers pattern from `tests/protocol.rs`)

**Interfaces:**
- Consumes: Task 3/4 node API.
- Produces: wire behaviour of spec §2. `ListAgents` replies carry `replaced` from `Node::replacements()`.

- [ ] **Step 1: Failing tests** (two client connections A, B):
  - `restart_over_the_wire`: A spawns `sh -c 'exit 0'`, waits exited; A sends `RestartAgent{old}` → `Restarted{old_id: old, agent: new, attached: None}`; A then receives `AgentRemoved{old, Some(new)}` **after** the response; B receives the same event; `ListAgents` → `replaced == {old: new}`.
  - `restart_attached_over_the_wire`: `attach: true` → `Restarted.attached` is `Some` and output frames for the new id follow on that connection.
  - `forget_over_the_wire`: two exited agents and one running; `ForgetAgents{exited_only: true}` → `Forgotten{ids: [both], refused: []}`; running agent listed; B gets two `AgentRemoved`.
  - `forget_ids_reports_refused`: `ForgetAgents{ids: [running, 9999]}` → `refused == [running, 9999]`.
  - `restart_running_is_an_error`: → `Error` containing "is running".
- [ ] **Step 2:** `cargo test -p fleetd --test agent_lifecycle` — FAIL.
- [ ] **Step 3:** Implement:
  - `handle`: `Restart(r)` with `r.attach` → `restart_attached` mirroring `spawn_attached` (mark `attached` in `before_start`, `forward` after respond). Without attach → `blocking(.. n.restart(r.agent_id, r.cols, r.rows, Some(conn)))`. `Forget(f)` → `n.forget(&f.ids, f.exited_only, &f.selector, Some(conn))` → `Forgotten`.
  - After responding, the requester queues its own `AgentRemoved` frames (one per forgotten id / the old id), so they follow the response.
  - New task `forward_removed(rx, outbox, own: ConnId)` skips `origin == Some(own)`; on `Lagged(n)` logs `warn!` and continues (clients recover via relist). Subscribe in `connection` before the handshake, like `exits`.
  - Also clear forgotten/old ids from the connection's `attached` set and stop their forwarders.
- [ ] **Step 4:** `cargo test -p fleetd` — PASS.
- [ ] **Step 5:** Commit `feat(fleetd): serve RestartAgent and ForgetAgents, broadcast AgentRemoved`.

### Task 6: CLI — `fleet restart`, `fleet rm`, skill text

**Files:**
- Modify: `crates/fleet-cli/src/main.rs`, `crates/fleet-cli/skill/SKILL.md`
- Test: `crates/fleet-cli/tests/cli.rs`

**Interfaces:**
- Produces:
  - `Restart { id: u64, #[arg(long)] attach: bool }` — prints the new id (no attach); with `--attach`, sends `RestartAgent{attach: false}` then runs the existing `attach::run(client, new_id)` (simplest; the GUI uses the attached form).
  - `Rm { ids: Vec<u64>, #[arg(long, conflicts_with = "ids")] exited: bool, #[arg(long = "selector", value_name = "K=V", requires = "exited")] selector: Vec<String> }` — at least one of `ids`/`--exited` required.
  - Help text of `restart`: "Restart an exited agent (same command, cwd and labels; new id). Its environment is not restored."
  - Output: `rm` prints nothing per success; per refused id stderr `fleet: agent <id> not removed (running or unknown)` and exit code 1. `rm --exited` prints `removed <n> agents`.
  - An `Error "empty request"` reply → stderr `fleet: the daemon is too old to restart/remove agents`, exit 1.

- [ ] **Step 1: Failing tests** in `cli.rs`: `restart_prints_new_id_and_replaces` (spawn `sh -c 'exit 0'`, wait exited, `fleet restart <id>` prints a different id; `fleet ls --json` lacks old, has new); `rm_removes_exited_and_refuses_running` (exit code 1, stderr names the running id, exited one gone); `rm_exited_counts` (`removed 2 agents`).
- [ ] **Step 2:** `cargo test -p fleet-cli --test cli` — FAIL.
- [ ] **Step 3:** Implement; add to SKILL.md "Start and stop agents" block: `fleet restart <id>` and `fleet rm <id>` with one bullet each ("restart: same command/cwd/labels, new id, only after it exited; rm: forgets an exited agent and its history — only when asked").
- [ ] **Step 4:** `cargo test -p fleet-cli` — PASS.
- [ ] **Step 5:** Commit `feat(cli): fleet restart and fleet rm`.

### Task 7: GUI core — session and layout follow removals

**Files:**
- Modify: `crates/fleet-gui/src/core/layout.rs`, `crates/fleet-gui/src/core/session.rs`, `crates/fleet-gui/src/core/pane_link.rs`
- Tests: unit tests in those files

**Interfaces:**
- Produces:
  - `Layout::replace_agent(&mut self, old: u64, new: Option<u64>) -> Vec<PaneId>` — every leaf showing `old` gets `new`; returns those panes.
  - `SavedLayout::replace_agent(&mut self, old: u64, new: Option<u64>) -> bool` (for saved layouts not currently loaded; true if changed).
  - `UiEvent::AgentRemoved { agent_id: u64, replaced_by: Option<u64> }` — sent from the control connection only (pane connections ignore the event).
  - `Session::replaced(&self) -> HashMap<u64, u64>` — from the latest `ListAgents` reply.
  - `Session::restart(&self, agent_id: u64)` — control connection, `attach: false`.
  - `Session::restart_in_pane(&self, pane: PaneId, agent_id: u64, cols: u16, rows: u16) -> Result<(), String>` — the pane's connection, `attach: true`; `Restarted{attached: Some}` is handled exactly like `SpawnedAttached` (the pane model then shows the new id).
  - `Session::forget(&self, ids: Vec<u64>)` and `Session::clear_exited(&self)` — control connection.
  - `ControlCmd::Restart(u64)`, `ControlCmd::Forget(Vec<u64>)`, `ControlCmd::ClearExited`; `pane_link::Outgoing::Restart(RestartAgent)`.
  - Sidebar applies `AgentRemoved` by dropping the row and its activity; `Error "empty request"` replies are mapped to `UiEvent::Error("fleetd is too old to restart or remove agents")`.

- [ ] **Step 1: Failing tests:**
  - layout: `replace_agent_rewrites_every_leaf` (two leaves on 5, one on 6 → `replace_agent(5, Some(9))` returns both panes; 6 untouched); `replace_with_none_empties`; `saved_layout_replace_agent`.
  - session `apply`: `removed_event_drops_row_and_notifies` (row 5 listed; apply `AgentRemoved{5, Some(9)}` → row gone, `UiEvent::AgentRemoved{5, Some(9)}` and `AgentsChanged` sent); `list_reply_keeps_replaced_map` (`AgentList{replaced: {1: 3}}` → `replaced() == {1: 3}`); `empty_request_error_says_daemon_too_old`.
  - pane_link: `restart_reply_switches_pane_to_new_agent` (writer handles `Outgoing::Restart`, server replies `Restarted{old_id: 1, agent: 2, attached: Some(ok)}` → model `agent_id() == Some(2)`), mirroring `spawn_then_late_reattach_does_not_resubscribe_the_old_agent` for the reattach guard.
- [ ] **Step 2:** `cargo test -p fleet-gui --lib core` — FAIL.
- [ ] **Step 3:** Implement following the existing `Spawn`/`Kill` paths (`control_writer`, `Writer::spawn`).
- [ ] **Step 4:** `cargo test -p fleet-gui --lib core` — PASS.
- [ ] **Step 5:** Commit `feat(gui): core follows restarted and removed agents`.

### Task 8: GUI app — menus, Restart in pane, Clear exited, layout rewrite

**Files:**
- Modify: `crates/fleet-gui/src/app/app_state.rs` (`SessionApi` + `apply_events`), `crates/fleet-gui/src/app/workspace.rs` (row menu, actions), `crates/fleet-gui/src/app/keymap.rs` (`RestartPane` action, `cmd-r` in the workspace context), `crates/fleet-gui/src/app/pane_view.rs` (Restart button in the exit banner), `crates/fleet-gui/src/app/sidebar_view.rs` (Clear exited header button), `crates/fleet-gui/src/core/status_menu.rs` + `crates/fleet-gui/src/app/status_item.rs` (`StatusEntry::ClearExited` "Clear exited agents", shown only when any exist)
- Tests: gpui tests in `workspace.rs` / `app_state.rs` with the recording fake session

**Interfaces:**
- Consumes: Task 7.
- Produces: `SessionApi::{restart, restart_in_pane, forget, clear_exited, replaced}` (fake records calls); `Remove` and `Restart { agent_id }` actions (like `Kill`).

Behaviour:
- Row menu: running row → Open in new split, Kill (unchanged). Exited/lost row → Open in new split, **Restart**, **Remove**. No confirmation for Restart/Remove/Clear exited.
- Exit banner: `exited (3)` gets a "Restart" button; `cmd-r` on a focused pane whose model state is `Exited(_)`/`Lost` calls `restart_in_pane` with the pane's grid size; otherwise `cmd-r` does nothing.
- Clear exited button in the sidebar header, enabled only if any row is exited/lost; tooltip `Remove <n> exited agents`.
- `UiEvent::AgentRemoved{old, new}` → `layout.replace_agent(old, new)`; for each returned pane, `attach(pane, new)` if `new` is set and the pane does not already show it, else `open_pane(pane, None)`; also `replace_agent` on every other target's `SavedLayout` is **not** needed (ids are per target) — only `gui_state.layouts[current]`; then `layout_changed` (saves).
- `UiEvent::AgentsChanged` → for each pane agent id `a` with `session.replaced()[a] = n`, treat as `AgentRemoved{a, Some(n)}`. Panes whose model state is `Lost` and whose id is neither listed nor replaced → empty pane.

- [ ] **Step 1: Failing tests:** `row_menu_offers_restart_and_remove_for_exited` (menu items by id `restart`/`remove` present for exited row, absent for running); `remove_calls_forget`; `cmd_r_restarts_exited_pane_in_place` (fake records `restart_in_pane(pane, id, cols, rows)`); `agent_removed_moves_pane_to_successor` (layout leaf 5 → `AgentRemoved{5, Some(9)}` → leaf 9, fake recorded `attach(pane, 9)`, saved state has 9); `list_follows_replacement_into_layout` (fake `replaced() == {5: 9}`, `AgentsChanged` → leaf 9); `clear_exited_disabled_without_exited_rows`.
- [ ] **Step 2:** `cargo test -p fleet-gui` — FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p fleet-gui` — PASS; `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] **Step 5:** Manual check via the `run` skill: launch fleet-gui against a local fleetd, spawn `sh -c 'exit 1'`, Restart from the banner, Remove from the menu, Clear exited; then `fleet restart <id>` with the GUI closed and reopen. Screenshot only if Screen Recording is granted.
- [ ] **Step 6:** Commit `feat(gui): restart, remove and clear exited agents`.

### Task 9: ADRs, docs, perf gate

**Files:**
- Create: `docs/decisions/0029-gui-core-toolkit-free.md`, `docs/decisions/0030-agent-restart-and-forget.md`
- Modify: `docs/decisions/0022-protocol-v2.md` (one "Amended by 0030" line listing the new messages), `docs/decisions/0021-agent-history.md` (one "Amended by 0030" line: forget deletes history early; retention never prunes store rows), `README.md` CLI usage (restart, rm)
- Test: `crates/fleet-gui/tests/core_is_toolkit_free.rs`

- [ ] **Step 1:** Test `core_has_no_gpui_or_appkit_imports`: read every `src/core/*.rs`, assert no line matching `use gpui`, `gpui::`, `objc2`, `cocoa` outside comments (`//`).
- [ ] **Step 2:** `cargo test -p fleet-gui --test core_is_toolkit_free` — PASS (core is already clean; the test guards it).
- [ ] **Step 3:** Write 0029 (decision: core stays gpui/AppKit-free for a possible Swift/iOS client via uniffi; guarded by the test) and 0030 (spec §2–§3 decisions: node-side requests, `AgentRemoved` + `AgentList.replaced`, ids never reused via schema v3, atomicity order and crash outcome, refusals, env not restored, retention never prunes rows, no version bump with the `Error "empty request"` behaviour). Follow the format of existing ADRs (Status / Context / Decision / Consequences).
- [ ] **Step 4:** Perf gate: `gh run list --workflow ci.yml --status in_progress` (must be empty), then `fleet-perf run --suite wall` and `fleet-perf compare` per `bench/README.md`. Any regression → **stop, show before/after numbers, ask the user.**
- [ ] **Step 5:** `cargo test --workspace` — PASS.
- [ ] **Step 6:** Commit `docs: ADR 0029 (toolkit-free GUI core), 0030 (agent restart and forget)`.
