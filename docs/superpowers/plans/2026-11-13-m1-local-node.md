# M1 — Local Node Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A local `fleetd` that runs agents in PTYs, keeps a headless terminal grid and on-disk scrollback per agent, persists agent records in SQLite, and serves a versioned protobuf protocol on its unix socket; `fleet` (CLI) can spawn, list, attach (snapshot + live stream, interactive), send input, resize and kill agents.

**Architecture:** `fleet-proto` owns messages (prost, compiled with `protox` — no `protoc` needed) and a framing codec shared by both ends. `fleet-node` grows from a drain-only `PtyHost` into a `Node`: per-agent output pipeline (reader thread → VT grid → scrollback ring file → bounded broadcast to attached clients) plus a SQLite store. `fleetd` maps protocol requests onto `Node`. `fleet-cli` is a thin client. Existing perf gate stays authoritative; new metrics are added for the new hot paths.

**Tech Stack:** Rust 1.99.1, tokio, prost + protox + prost-build, alacritty_terminal, rusqlite (bundled), portable-pty, crossterm (CLI raw mode), clap.

**Spec:** `docs/plan.md` (Architecture, Storage, Milestones: "M1 Local node: PTYs, terminal grid, scrollback, SQLite, protocol, fleet-cli"), `docs/decisions/0002`, `0003`, `0005`, `0006`, `0010`, `0013`, `0014`, `0016`.

## Global Constraints

- Toolchain `1.99.1` (rust-toolchain.toml); `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean; deps via `cargo add`, no git deps.
- No `Co-Authored-By` or `Perf-Approved` trailers. Agents never change existing baseline values, tolerances or report-only flags; new metrics are added with `fleet-perf baseline-add` / `tighten` only.
- Perf rule (CLAUDE.md): if an existing gated metric regresses, stop and report the numbers — the user approves, not the agent.
- Gate code (`crates/fleet-perf/src/{baseline,approval,wall,instr,lib,results}.rs`, `src/bin/fleet-perf.rs`, `benches/instr.rs`, `crates/fleet-perf/Cargo.toml`, `rust-toolchain.toml`, `.github/workflows/`) changes need user approval after merge — touch them only in Task 8, and keep changes additive (new scenarios/metrics), never altering existing metrics' measurement.
- Budgets: daemon idle RSS < 50 MB; ≥ 50 concurrent agents; reattach is O(screen) (decision 0005).
- Protocol version: `PROTOCOL_VERSION = 1`; mismatch ⇒ server sends `HandshakeReject` and closes.
- State dir `~/.fleet` created with mode 0700 (deferred M0 must-fix).
- Work on branch `m1-local-node`; never push to main.

## Review Focus

1. **Slow or stuck attached client** must never stall an agent or other clients → Task 4 test `slow_subscriber_is_dropped_not_blocking`.
2. **Client disconnects mid-attach / daemon gets EOF** must not leak subscriptions or panic → Task 6 test `client_disconnect_unsubscribes`.
3. **Snapshot fidelity** — feeding a snapshot into a fresh grid reproduces the visible screen (text, cursor) → Task 2 test `snapshot_roundtrip_reproduces_screen`.
4. **Daemon restart** — agents from a previous daemon run are recorded as `lost` (no PTY to reattach), not shown as running → Task 5 test `startup_marks_running_agents_lost`.
5. **Oversized / malformed frame** from a client → connection closed with an error, daemon keeps serving others → Task 1 test `oversized_frame_rejected`, Task 6 test `malformed_frame_closes_only_that_connection`.

---

## File Structure

```
crates/fleet-proto/{Cargo.toml, build.rs, proto/fleet.proto, src/lib.rs, src/frame.rs}
crates/fleet-node/src/lib.rs          pub mod pty; pub mod term; pub mod scrollback; pub mod store; pub mod node;
crates/fleet-node/src/term.rs         Terminal (alacritty_terminal wrapper), snapshot
crates/fleet-node/src/scrollback.rs   on-disk ring buffer
crates/fleet-node/src/pty.rs          PtyHost: input, resize, output sink, killpg, reaping
crates/fleet-node/src/store.rs        SQLite store
crates/fleet-node/src/node.rs         Node: ties pty + term + scrollback + store + subscribers
crates/fleetd/src/{lib.rs, server.rs}  protocol server
crates/fleet-cli/{Cargo.toml, src/main.rs, src/client.rs}
crates/fleet-perf/...                  Task 8 only (new scenarios)
docs/decisions/0018-m1-protocol-and-node.md
```

---

### Task 1: fleet-proto — schema, codec, handshake

**Files:** Create `crates/fleet-proto/**`. Test: unit tests in `frame.rs`, `tests/roundtrip.rs`.

**Interfaces (Produces):**
- `proto/fleet.proto` (package `fleet.v1`): `Hello{uint32 protocol_version; string client}`, `HelloOk{uint32 protocol_version; string server}`, `HandshakeReject{string reason}`; `Request{uint64 id; oneof kind{SpawnAgent spawn; ListAgents list; Attach attach; Detach detach; SendInput input; Resize resize; KillAgent kill}}`; `Response{uint64 id; oneof kind{AgentInfo spawned; AgentList agents; AttachOk attached; Ack ack; Error error}}`; `Event{oneof kind{AgentExited exited}}`; `SpawnAgent{string program; repeated string args; string cwd; map<string,string> env; uint32 cols; uint32 rows}`; `AgentInfo{uint64 id; string program; repeated string args; string cwd; AgentState state; optional int32 exit_code; int64 created_unix_ms}`; `enum AgentState{AGENT_STATE_UNSPECIFIED=0; RUNNING=1; EXITED=2; LOST=3}`; `Attach{uint64 agent_id}`; `AttachOk{uint64 agent_id; bytes snapshot; uint32 cols; uint32 rows}`; `SendInput{uint64 agent_id; bytes data}`; `Resize{uint64 agent_id; uint32 cols; uint32 rows}`; `KillAgent{uint64 agent_id}`; `Detach{uint64 agent_id}`; `AgentExited{uint64 agent_id; int32 exit_code}`; `Error{string message}`; `Ack{}`.
- `pub const PROTOCOL_VERSION: u32 = 1; pub const MAX_FRAME: usize = 16 * 1024 * 1024;`
- Wire: `[u32 BE length of rest][u8 kind][payload]`; kinds `1=Hello 2=HelloOk 3=HandshakeReject 4=Request 5=Response 6=Event 7=Output`. Output payload = `[u64 BE agent_id][raw bytes]` (not protobuf — decision 0006 "dedicated binary frames").
- `pub enum Frame { Hello(Hello), HelloOk(HelloOk), Reject(HandshakeReject), Request(Request), Response(Response), Event(Event), Output{agent_id: u64, data: bytes::Bytes} }`
- `pub struct FrameCodec;` implementing `tokio_util::codec::{Encoder<Frame>, Decoder<Item=Frame, Error=FrameError>}`; `FrameError::{TooLarge(usize), UnknownKind(u8), Decode(prost::DecodeError), Io(io::Error)}`.

- [ ] **Step 1:** Write failing tests: `roundtrip_every_kind` (each Frame variant encodes then decodes equal), `output_frame_is_raw` (bytes on wire = 4+1+8+len), `oversized_frame_rejected` (length > MAX_FRAME ⇒ `TooLarge` before allocating), `unknown_kind_rejected`, `partial_frames_buffer` (feeding a frame byte-by-byte yields exactly one Frame at the end).
- [ ] **Step 2:** Run `cargo test -p fleet-proto` → FAIL.
- [ ] **Step 3:** Implement; `build.rs` uses `protox::compile` + `prost_build::Config::compile_fds`.
- [ ] **Step 4:** Tests PASS; fmt/clippy clean. Commit `feat(proto): protocol schema and frame codec`.

---

### Task 2: Terminal grid + snapshot

**Files:** Create `crates/fleet-node/src/term.rs`. Test: unit tests.

**Interfaces (Produces):**
- `pub struct Terminal` — `new(cols: u16, rows: u16) -> Self`, `feed(&mut self, bytes: &[u8])`, `resize(&mut self, cols: u16, rows: u16)`, `snapshot(&self) -> Vec<u8>`, `visible_text(&self) -> Vec<String>` (one String per row, trailing spaces trimmed), `cursor(&self) -> (u16, u16)` (row, col).
- `snapshot()` returns an ANSI byte sequence that, fed into a fresh `Terminal` of the same size, reproduces visible text, SGR attributes (fg/bg/bold/italic/underline/inverse) and cursor position: begins with `\x1b[0m\x1b[H\x1b[2J`, emits rows with minimal SGR changes, ends with cursor position `\x1b[{r};{c}H`. Size O(screen).
- Uses `alacritty_terminal::Term` with a no-op `EventListener` and `vte::ansi::Processor`.

- [ ] **Step 1:** Failing tests: `plain_text_lands_on_grid`, `cursor_movement_and_overwrite` (`"abc\x1b[2Dx"` → `"axc"`), `snapshot_roundtrip_reproduces_screen` (feed `FakeOutput` 256 KiB with ANSI into A; feed `A.snapshot()` into fresh B; `visible_text` and `cursor` equal), `snapshot_preserves_colors` (red text cell attrs equal after roundtrip — compare via a `cell_attrs(row,col)` test helper), `snapshot_size_bounded` (after 8 MiB of output, snapshot < 64 KiB at 120×40), `resize_keeps_content`.
- [ ] **Step 2:** FAIL. **Step 3:** Implement (dev-dep on fleet-perf for `FakeOutput` is acceptable — or copy a small ANSI fixture; prefer dev-dep). **Step 4:** PASS, fmt/clippy. Commit `feat(node): headless terminal grid with snapshot`.

---

### Task 3: Scrollback ring file

**Files:** Create `crates/fleet-node/src/scrollback.rs`. Test: unit tests.

**Interfaces (Produces):**
- `pub struct Scrollback` — `create(path: &Path, capacity: u64) -> io::Result<Self>` (truncates), `append(&mut self, bytes: &[u8]) -> io::Result<()>`, `read_tail(&self, max: usize) -> io::Result<Vec<u8>>` (most recent ≤ max bytes, oldest-first), `len(&self) -> u64` (bytes retained, ≤ capacity).
- File layout: 16-byte header `[u64 LE write_pos][u64 LE total_written]` + `capacity` data bytes. Default capacity `8 * 1024 * 1024`. Writes buffered (`BufWriter`-style in-memory pending buffer ≤ 64 KiB, flushed on `flush()`/drop/when full) — no fsync per chunk.

- [ ] **Step 1:** Failing tests: `tail_before_wrap`, `tail_after_wrap_is_most_recent` (capacity 1000, append 2500 bytes of a counting pattern ⇒ tail(1000) = last 1000), `chunk_larger_than_capacity_keeps_suffix`, `reopen_reads_same_tail` (add `open(path) -> io::Result<Self>`), `len_never_exceeds_capacity`.
- [ ] **Step 2–4:** FAIL → implement → PASS. Commit `feat(node): on-disk scrollback ring buffer`.

---

### Task 4: PtyHost v2 — output pipeline, input, resize, broadcast

**Files:** Modify `crates/fleet-node/src/pty.rs`; create `crates/fleet-node/src/node.rs` (the per-agent pipeline types live here). Test: `crates/fleet-node/tests/pty_host.rs` (keep existing tests green), `tests/node_pipeline.rs`.

**Interfaces:**
- `PtyHost::spawn(&self, spec: CommandSpec, sink: Box<dyn OutputSink>) -> io::Result<AgentId>`; `pub trait OutputSink: Send + 'static { fn on_output(&mut self, bytes: &[u8]); fn on_exit(&mut self, code: i32); }`. The M0 counting behaviour becomes `CountingSink` used by flood-host (keeps the M0 flood metrics comparable).
- `PtyHost::write_input(&self, id, bytes: &[u8]) -> io::Result<()>`, `resize(&self, id, cols, rows) -> io::Result<()>`, `kill(&self, id)` sends SIGTERM to the agent's process group (`killpg`), `remove(&self, id)` drops a finished agent.
- `bytes_read` counters become `AtomicU64` (deferred M0 minor); keep `stats()` and `wait_all` semantics.
- `node::AgentPipeline` (an `OutputSink`): feeds `Terminal`, appends `Scrollback`, then broadcasts to subscribers. `pub fn subscribe(&self) -> (Vec<u8> /*snapshot*/, Subscription)` taken atomically under the pipeline lock so no bytes are lost or duplicated between snapshot and stream. `Subscription` wraps a bounded channel (capacity 256 chunks); when full, that subscriber is dropped and receives `Lagged` (client must re-attach) — the reader thread never blocks on a subscriber.

- [ ] **Step 1:** Failing tests: existing pty tests adapted to pass a `CountingSink`; `input_reaches_agent` (`cat` echoes typed bytes back via output); `resize_reaches_agent` (`sh -c 'sleep 0.2; stty size'` after resize to 50×100 prints `100 50`… use rows cols order of stty: `"50 100"`); `kill_terminates_process_group` (`sh -c 'sleep 60 & wait'` killed ⇒ exits, `wait_all(5s)` ok); `subscribe_gets_snapshot_then_stream_without_gap` (agent prints `A`, subscribe, agent prints `B`; snapshot shows `A`, stream contains `B`, concatenated grid equals agent grid); `slow_subscriber_is_dropped_not_blocking` (never-read subscriber + 4 MiB output ⇒ agent still completes, subscriber gets `Lagged`).
- [ ] **Step 2–4:** FAIL → implement → PASS (run pty tests 3× to check flakiness). Commit(s) `feat(node): PtyHost input/resize/killpg and output sinks`, `feat(node): agent pipeline with grid, scrollback and subscribers`.

---

### Task 5: SQLite store

**Files:** Create `crates/fleet-node/src/store.rs`. Test: unit tests.

**Interfaces (Produces):**
- `pub struct Store` — `open(path: &Path) -> rusqlite::Result<Self>` (WAL, `foreign_keys=ON`, migrations via `PRAGMA user_version`), `insert_agent(&self, rec: &NewAgent) -> Result<u64>`, `set_exited(&self, id: u64, code: i32) -> Result<()>`, `list_agents(&self) -> Result<Vec<AgentRecord>>`, `mark_running_as_lost(&self) -> Result<usize>`.
- Schema v1: `agents(id INTEGER PRIMARY KEY, program TEXT NOT NULL, args_json TEXT NOT NULL, cwd TEXT NOT NULL, state TEXT NOT NULL CHECK(state IN ('running','exited','lost')), exit_code INTEGER, created_unix_ms INTEGER NOT NULL, scrollback_path TEXT NOT NULL)`.
- `AgentRecord` mirrors the columns with `state: AgentState { Running, Exited, Lost }`.

- [ ] **Step 1:** Failing tests: `insert_then_list`, `exit_updates_state_and_code`, `startup_marks_running_agents_lost`, `reopen_preserves_rows`, `migration_is_idempotent` (open twice ⇒ user_version 1, no error).
- [ ] **Step 2–4:** FAIL → implement (rusqlite `bundled`) → PASS. Commit `feat(node): SQLite agent store`.

---

### Task 6: fleetd protocol server

**Files:** Modify `crates/fleetd/src/lib.rs`; create `crates/fleetd/src/server.rs`, `crates/fleet-node/src/node.rs` (`Node` facade). Test: `crates/fleetd/tests/protocol.rs`.

**Interfaces:**
- `fleet_node::node::Node` — `open(state_dir: &Path) -> io::Result<Node>` (store at `state_dir/state.db`, scrollback under `state_dir/scrollback/<id>.ring`, marks previous running agents lost), `spawn(SpawnAgent) -> AgentInfo`, `list() -> Vec<AgentInfo>`, `attach(id) -> (AttachOk, Subscription)`, `input`, `resize`, `kill`, exit events via a `tokio::sync::broadcast::Receiver<AgentExited>`.
- `startup()` additionally sets state dir mode 0700 (create or chmod). `serve()` per connection: expect `Hello` within 5 s; version ≠ 1 ⇒ `Reject` + close; then requests handled concurrently with output/event forwarding through one writer task per connection. On `Lagged` the server sends `Response{error:"lagged"}`-style `Event`? — Use `Error{message:"lagged: re-attach"}` as an unsolicited `Response` with id 0 and stops forwarding that agent.
- Accept loop behaviour from M0 unchanged (transient errors logged, not fatal).

- [ ] **Step 1:** Failing tests (spawn real `fleetd` binary on a TempDir, talk via `FrameCodec`): `handshake_ok`, `version_mismatch_rejected`, `spawn_list_kill`, `attach_receives_snapshot_then_output`, `input_round_trip` (`cat`), `exit_event_delivered`, `client_disconnect_unsubscribes` (attach then drop connection; daemon still serves a new client; agent unaffected), `malformed_frame_closes_only_that_connection`, `state_dir_is_0700`, `restart_marks_agents_lost`.
- [ ] **Step 2–4:** FAIL → implement → PASS. Commit `feat(fleetd): protobuf protocol server over unix socket`.

---

### Task 7: fleet-cli

**Files:** Create `crates/fleet-cli/**` (binary name `fleet`). Test: `crates/fleet-cli/tests/cli.rs`.

**Interfaces:**
- `fleet [--state-dir P] spawn [--cols C --rows R] -- <program> [args...]` prints the agent id; `fleet ls` prints a table `ID STATE EXIT PROGRAM` (one row per agent); `fleet kill <id>`; `fleet send <id> <text>` (appends `\r` only with `--enter`); `fleet attach <id>` puts the local tty in raw mode (crossterm), writes the snapshot then streams output, forwards stdin as input, forwards window resizes (SIGWINCH), detaches on `Ctrl-\` (0x1c) and restores the terminal on every exit path; exits 0 on agent exit, 2 on `lagged`.
- `client.rs`: `Client::connect(state_dir) -> Result<Client>` doing the handshake; request/response helper `call(kind) -> Response`.

- [ ] **Step 1:** Failing tests (non-tty paths only): `spawn_then_ls_shows_running`, `kill_then_ls_shows_exited`, `send_then_attach_snapshot_contains_text` (spawn `cat`, `send --enter hello`, then `attach` with stdin closed in a non-tty mode `--no-raw` that prints the snapshot and exits after 200 ms — add hidden flag `--snapshot-only`), `unknown_agent_errors_exit_1`, `daemon_not_running_message` (`fleet ls` with no daemon ⇒ exit 1, message `fleetd is not running`).
- [ ] **Step 2–4:** FAIL → implement → PASS. Manual check documented in report: attach interactively to `bash` in a real terminal works, Ctrl-\ detaches, terminal restored. Commit `feat(cli): fleet command-line client`.

---

### Task 8: Benchmarks for the new hot paths, baselines, docs

**Files:** Modify `crates/fleet-perf/src/wall.rs`, `crates/fleet-perf/src/instr.rs`, `crates/fleet-perf/benches/instr.rs`, `crates/fleet-perf/src/bin/fleet-perf.rs` (only to wire new scenarios), `bench/baselines.toml` (new metrics only), `bench/README.md`, `docs/decisions/0018-m1-protocol-and-node.md`, `docs/plan.md` (M1 status line).

**Interfaces / new metrics (all lower-is-better):**
- wall `daemon_flood50_cpu_ns_per_mib` (report-only, same reason as flood50 CPU), `daemon_flood50_peak_rss_bytes`, `daemon_flood50_wall_ns`: fleetd spawns 50 `fake-agent --ansi --bytes 8388608` via the protocol with one client attached to every agent (snapshot + full stream consumed), measured as daemon-process rusage (`RUSAGE_SELF` reported by fleetd? — no: read fleetd's RSS via `ps`/statm at peak sampling every 50 ms, and CPU via `proc_pid_rusage` on macOS / `/proc/<pid>/stat` on Linux).
- wall `reattach_snapshot_ns`: median of 50 attaches to an agent that has printed 8 MiB, from `Attach` sent to `AttachOk` received.
- instr `frame_decode_output_64x64k_instructions` (decode 64 back-to-back 64 KiB Output frames), `terminal_feed_64k_instructions` (feed 64 KiB of deterministic ANSI into a 120×40 `Terminal`), `snapshot_120x40_instructions`.
- Existing metrics' scenarios are NOT changed; flood-host keeps `CountingSink`.

- [ ] **Step 1:** Implement scenarios + metric names constants; update the shipped-baselines test.
- [ ] **Step 2:** Record new metrics: run wall suite locally (only when no `perf-wall` job is running) and instr suite in Docker for determinism; add new metrics via `baseline-add` with value 0? — No: add with `baseline-add` then the PR's CI artifacts become the authoritative values (same procedure as M0); document in report.
- [ ] **Step 3:** Run full wall `compare` against existing baselines: any existing gated metric regressed ⇒ STOP, report numbers (user approval needed).
- [ ] **Step 4:** Docs: ADR 0018 (protocol wire format, snapshot-as-ANSI, subscriber drop policy, `lost` state, protox), README metric table, plan.md M1 status. Commit `feat(perf): M1 benchmarks`, `docs: M1 decisions`.
