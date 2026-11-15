# fleetd Restart With Surviving Agents — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `fleet daemon restart` and the GUI's **Restart fleetd** replace the running daemon with a new binary while every running agent keeps running.

**Architecture:** The daemon freezes its PTY readers at a clean boundary, writes a versioned handoff record, clears close-on-exec on the fds it names and `execve`s the new binary in the same process (same pid, so agents stay its children). The new binary sees `FLEETD_HANDOFF_FD`, adopts the listener, lock and agents (replaying each screen snapshot) and serves. Ownership of an agent's terminal moves behind an `AgentHost` trait so a later holder-process implementation (crash survival) slots in.

**Tech Stack:** Rust, tokio, portable-pty, libc (`poll`, `fcntl`, `execve`, `waitpid`), prost (protobuf), rusqlite, clap, gpui 0.2.2 (app layer only).

**Spec:** `docs/superpowers/specs/2026-11-15-daemon-restart-design.md` — read it with this plan.

## Global Constraints

- Perf gate (ADR 0014) unchanged; any regression stops the work and goes to the user. Hot-path metrics most at risk: `daemon_flood50_*`, `flood50_*`, `input_echo_flood50_ns`, `input_to_render_*`, `fleetd_idle_rss_bytes`, `fleetd_startup_instructions`, `frame_decode_output_64x64k_instructions`. Never edit `bench/baselines.toml`, add a `Perf-Approved:` trailer or write `docs/perf-log.md`.
- Before finishing any task that touches `fleet-node` or `fleetd`: `cargo run -p fleet-perf --release --bin fleet-perf -- run --suite wall --out /tmp/wall.json` then `... compare --baselines bench/baselines.toml --results /tmp/wall.json`; not while a `perf-wall` CI job runs (`gh run list --workflow ci.yml --status in_progress`).
- `Frame` stays at most 88 bytes (`frame_stays_small` in `crates/fleet-proto/src/frame.rs`); box any new oneof variant that would grow it.
- `PROTOCOL_VERSION` stays 2; new messages and fields are additive.
- GUI core (`crates/fleet-gui/src/core/`) stays toolkit-free (ADR 0029, `tests/core_is_toolkit_free.rs`).
- Values: pre-flight check timeout **2 s**; freeze deadline **3 s**; CLI reconnect wait **10 s**; `HANDOFF_VERSION = 1`; handoff fd env var **`FLEETD_HANDOFF_FD`**; record file **`<state-dir>/handoff.bin`**, mode 0600; check flag **`--handoff-check`**, prints exactly `handoff 1` and exits 0.
- Copy, verbatim:
  - CLI success: `fleetd restarted: N agents kept`
  - CLI partial: `fleetd restarted: N kept, M could not be restored and were ended (ids …)` (exit 1; ids comma-separated)
  - CLI failure: `fleetd restart failed: <reason>; the old fleetd is still running` (exit 1)
  - Too-old daemon (CLI and GUI): `this fleetd is too old to restart itself; stop it once (pkill fleetd) and the next client starts the new one`
  - Requests refused while freezing: `fleetd is restarting`
  - Restart refused while spawns, restarts or forgets are still in flight at the deadline: `agents are starting; try again`
  - Second concurrent restart: `a restart is already in progress`
- Logs never contain the handoff record's contents, agent args, env or output.
- No `Co-Authored-By` trailer on commits.

## Review Focus

- **A stale `handoff.bin`** left by a restart that died between writing it and `exec` (or by a crash of the new binary) must never be adopted on a normal start: its fds are meaningless. Normal start deletes it. → test in Task 5.
- **`FLEETD_HANDOFF_FD` leaking** into agents spawned after a restart or into the login-env capture: the new daemon removes it from its environment before anything else. → test in Task 6.
- **An agent in the alternate screen** (an editor) keeps both screens across the restart, as a reattach would. → test in Task 4.
- **Two restart requests at once** (CLI and GUI): the second gets `a restart is already in progress`, the first proceeds. → test in Task 6.
- **An agent that never reads its input with a client typing into it** during the restart: the stuck `Input` is answered with `fleetd is restarting` and the freeze still completes inside 3 s. → test in Task 6.

---

### Task 1: Protocol messages for restart, handoff and identity

**Files:**
- Modify: `crates/fleet-proto/proto/fleet.proto`
- Create: `crates/fleet-proto/proto/handoff.proto` (package `fleet.handoff.v1`, compiled by the same `build.rs`)
- Modify: `crates/fleet-proto/build.rs`, `crates/fleet-proto/src/lib.rs`, `crates/fleet-client/src/lib.rs`
- Test: `crates/fleet-proto/src/frame.rs` (tests), `crates/fleet-proto/tests/roundtrip.rs`

**Interfaces:**
- Produces (proto, `fleet.v1`):
  - `Request.kind`: `RestartDaemon restart_daemon = 15;` with `message RestartDaemon { string binary = 1; }`
  - `Response.kind`: `RestartingDaemon restarting_daemon = 12;` with `message RestartingDaemon { uint32 agents = 1; }`
  - `HelloOk`: add `uint64 instance = 3; BinaryIdentity binary = 4; RestoreSummary restore = 5;`
  - `message BinaryIdentity { string path = 1; uint64 dev = 2; uint64 ino = 3; uint64 size = 4; int64 mtime_ns = 5; }`
  - `message RestoreSummary { uint64 previous_instance = 1; repeated uint64 kept = 2; repeated uint64 failed = 3; bool fell_back = 4; string restart_error = 5; }` — `restart_error` set (and `previous_instance` 0) when a restart was attempted and the daemon thawed.
- Produces (proto, `fleet.handoff.v1`): `message HandoffRecord { uint32 version = 1; int32 listener_fd = 2; int32 lock_fd = 3; uint64 previous_instance = 4; repeated HandoffAgent agents = 5; }`, `message HandoffAgent { uint64 id = 1; int32 pid = 2; uint32 cols = 3; uint32 rows = 4; bytes snapshot = 5; oneof terminal { int32 fd = 6; string holder_socket = 7; } }`
- Produces (Rust): `pub const HANDOFF_VERSION: u32 = 1;` and `pub mod handoff` re-exporting the generated types in `fleet-proto`; `Connection::hello(&self) -> &HelloOk` in `fleet-client` (store the `HelloOk` received in the handshake).

- [ ] **Step 1: Write failing tests** — in `roundtrip.rs`, `restart_daemon_frames_round_trip` encodes/decodes `Request{kind: RestartDaemon{binary:"/x/fleetd"}}`, `Response{kind: RestartingDaemon{agents:3}}` and a `HelloOk` with all new fields set, asserting equality; a `handoff_record_round_trips` test encodes a `HandoffRecord` with one agent using `terminal = fd(7)` and one using `holder_socket("/s")` via `prost::Message::encode_to_vec`/`decode`. `frame_stays_small` must still pass.
- [ ] **Step 2: Run** `cargo test -p fleet-proto` — expect compile failure (types missing).
- [ ] **Step 3: Add the messages and fields above; compile `handoff.proto` in `build.rs`; expose `HANDOFF_VERSION`, `handoff` module and `Connection::hello`.**
- [ ] **Step 4: Run** `cargo test -p fleet-proto -p fleet-client` — PASS, including `frame_stays_small`.
- [ ] **Step 5: Commit** `feat(proto): RestartDaemon, handoff record, instance and binary identity in HelloOk`

### Task 2: `AgentHost` trait and freezable PTY readers

**Files:**
- Create: `crates/fleet-node/src/host.rs` (the trait and its types)
- Modify: `crates/fleet-node/src/pty.rs`, `crates/fleet-node/src/lib.rs`, `crates/fleet-node/src/node.rs` (hold `Arc<dyn AgentHost>`)
- Test: `crates/fleet-node/src/pty.rs` (tests)

**Interfaces:**
- Produces in `host.rs`:
  ```rust
  pub enum TerminalHandle { Fd(std::os::fd::RawFd) /* later: HolderSocket(PathBuf) */ }
  pub struct FrozenAgent { pub id: AgentId, pub pid: u32, pub terminal: TerminalHandle }
  pub struct AdoptSpec { pub pid: u32, pub terminal: TerminalHandle, pub cols: u16, pub rows: u16 }
  pub trait AgentHost: Send + Sync {
      fn spawn_with_base_env(&self, spec: CommandSpec, base: Option<&[(OsString, OsString)]>, sink: Box<dyn OutputSink>) -> io::Result<AgentId>;
      fn adopt(&self, spec: AdoptSpec, sink: Box<dyn OutputSink>) -> io::Result<AgentId>; // Task 3
      fn write_input(&self, id: AgentId, bytes: &[u8]) -> io::Result<()>;
      fn resize(&self, id: AgentId, cols: u16, rows: u16) -> io::Result<()>;
      fn kill(&self, id: AgentId) -> io::Result<()>;
      fn force_kill(&self, id: AgentId) -> io::Result<()>;
      fn remove(&self, id: AgentId) -> io::Result<()>;
      fn wait_all(&self, timeout: Duration) -> Result<(), WaitTimeout>;
      fn stats(&self, id: AgentId) -> Option<AgentStats>;
      fn freeze(&self, deadline: Instant) -> io::Result<Vec<FrozenAgent>>;
      fn thaw(&self);
  }
  ```
  `PtyHost` implements it (its inherent `spawn`/`spawn_with_sink` stay for tests). `Node` holds `Arc<dyn AgentHost>`; `NodeSink.host` likewise. The output path (reader thread → `OutputSink::on_output`) gets no dynamic dispatch or copy it did not have.
- Freeze semantics: after `freeze` returns `Ok`, no reader holds bytes it has read but not passed to its sink, and no reader reads again until `thaw`; bytes the agent writes meanwhile stay in the kernel PTY buffer. An agent whose reader already saw EOF is not returned. `freeze` past `deadline` thaws what it froze and returns `Err(io::ErrorKind::TimedOut)`. While frozen, `write_input` returns `Err` with message `fleetd is restarting` (also for a write already blocked when the freeze began).

- [ ] **Step 1: Write failing tests** in `pty.rs`:
  - `freeze_stops_reading_without_losing_bytes`: spawn `sh -c 'i=0; while :; do echo $i; i=$((i+1)); done'` with a collecting sink; after 50 ms `freeze`, record the sink's byte count, sleep 100 ms, assert unchanged; `thaw`, wait 100 ms; assert the collected text's numbers are consecutive from 0 with no gap or repeat.
  - `freeze_releases_a_blocked_input_write`: spawn `sleep 30` (never reads), fill its input from a thread with `write_input` of 1 MiB; `freeze` with a 3 s deadline returns `Ok` within 1 s and the writer thread gets `Err` whose message is `fleetd is restarting`.
  - `frozen_agents_name_their_fd_and_pid`: two agents frozen → two `FrozenAgent`s with distinct `TerminalHandle::Fd` ≥ 0 and their pids.
- [ ] **Step 2: Run** `cargo test -p fleet-node freeze_` — FAIL (no `freeze`).
- [ ] **Step 3: Implement.** The decided mechanism (plan's call): the PTY master is set `O_NONBLOCK` once at spawn; the reader loops `read()` until `EAGAIN`, then `poll`s on the master and a per-host stop pipe (so a flood costs no extra syscall per read; idle readers sleep in `poll`); it checks a per-host `frozen` flag before each `read`, and on freeze acknowledges and parks on a condvar until thaw. `AgentInput::write_all` loops on `EAGAIN` with `poll(POLLOUT)` plus the stop pipe and the `frozen` flag; `try_write_reply` no longer toggles `O_NONBLOCK` (the fd already is). `freeze` sets the flag, writes the stop pipe, and waits until every live reader has acknowledged or seen EOF.
- [ ] **Step 4: Run** `cargo test -p fleet-node` and `cargo test -p fleetd` — PASS (all existing tests unchanged).
- [ ] **Step 5: Perf** — run the wall suite and compare (Global Constraints). Report the `flood50_*`, `daemon_flood50_*`, `input_echo_flood50_ns` and `fleetd_idle_rss_bytes` rows in the task report. A regression stops the task (report, do not tune baselines).
- [ ] **Step 6: Commit** `refactor(node): AgentHost trait; PTY readers freeze at a clean boundary`

### Task 3: Adopting an inherited agent

**Files:**
- Modify: `crates/fleet-node/src/pty.rs`
- Test: `crates/fleet-node/src/pty.rs` (tests)

**Interfaces:**
- Consumes: `AgentHost`, `AdoptSpec`, `TerminalHandle` (Task 2).
- Produces: `PtyHost::adopt` — takes ownership of `TerminalHandle::Fd` (sets `O_NONBLOCK` and close-on-exec), starts the same reader loop, reaps with `libc::waitpid(pid, …, 0)` after EOF (no portable-pty `Child`), exit code = `WEXITSTATUS`, or `128 + WTERMSIG` for a signal (the same encoding `portable_pty::ExitStatus::exit_code` yields today — assert it in a test); `kill`/`force_kill` signal the process group `-pid`; `resize` uses `TIOCSWINSZ` on the fd. `TerminalHandle::HolderSocket` is not a variant yet (comment marks where it goes).

- [ ] **Step 1: Write failing tests:**
  - `adopted_agent_reads_writes_and_reports_exit`: spawn `cat` normally, `freeze`, take its `FrozenAgent`, build a second `PtyHost`, `adopt` it with a collecting sink (dup the fd first so the first host's drop does not close it: the test owns the dup), `write_input(b"hi\n")` → sink sees `hi`; `kill` → sink's `on_exit` gets the code a SIGTERM yields (`128 + 15`), matching what a normally spawned `cat` reports when killed (assert both in the test).
  - `adopted_agent_that_already_exited_is_reaped`: open a PTY with `libc::openpty`, `fork` a child that execs `sh -c 'exit 7'` on the slave, close the slave in the parent, and wait 100 ms so the child is a zombie. Then `adopt` the master with the child's pid. Expect `on_exit(7)`.
- [ ] **Step 2: Run** `cargo test -p fleet-node adopted_` — FAIL.
- [ ] **Step 3: Implement `adopt`.**
- [ ] **Step 4: Run** `cargo test -p fleet-node` — PASS.
- [ ] **Step 5: Commit** `feat(node): adopt an agent from an inherited PTY fd`

### Task 4: Node handoff — prepare, thaw, open from a record

**Files:**
- Create: `crates/fleet-node/src/handoff.rs`
- Modify: `crates/fleet-node/src/node.rs`, `crates/fleet-node/src/store.rs` (mark-lost with exceptions), `crates/fleet-node/src/lib.rs`
- Test: `crates/fleet-node/tests/handoff.rs`

**Interfaces:**
- Consumes: `AgentHost::{freeze, thaw, adopt}` (Tasks 2–3); `fleet_proto::handoff::{HandoffRecord, HandoffAgent}` (Task 1).
- Produces:
  - `Node::prepare_handoff(&self, deadline: Instant) -> io::Result<Vec<HandoffAgent>>`: refuses (`Err`, message `a restart is already in progress` if one is, or `agents are starting; try again` after waiting until `deadline` for in-flight spawns/M2c restarts/forgets) ; sets a `handing_off` state that makes `start`, `restart_with` and `forget` return `Err("fleetd is restarting")`; calls `host.freeze`; for each frozen agent takes `snapshot_sized()` from its pipeline; flushes each scrollback and history; returns one `HandoffAgent` per agent (`terminal = fd`). Retained exited snapshots are **not** carried (plan's call; recorded in the ADR): after a restart, attaching an agent that exited earlier takes the existing empty-snapshot path.
  - `Node::thaw(&self)`: undoes `prepare_handoff` (host thaw, state cleared). Idempotent.
  - `Node::open_with_handoff(state_dir: &Path, agents: Vec<HandoffAgent>) -> io::Result<(Node, Vec<u64> /*kept*/, Vec<u64> /*failed*/)>`: like `open`, but `Store::mark_running_as_lost_except(&[u64])` spares the handed-over ids; for each agent: `Scrollback::open`, `History::open`, `AgentPipeline::with_history(cols, rows, …)`, `pipeline.restore_screen(&snapshot)`, then `host.adopt`. An agent that fails any step: SIGTERM its group, then SIGKILL after `SHUTDOWN_GRACE`, `set_exited(id, -1)` *(plan's call: -1, as for a failed wait today)*, listed in `failed`.
  - `AgentPipeline::restore_screen(&self, bytes: &[u8])`: feeds the terminal grid only — nothing to the scrollback ring, history, subscribers, activity or the agent's input (query replies generated by the replay are discarded).

- [ ] **Step 1: Write failing tests** in `tests/handoff.rs` (one `Node` process-internal; the "new daemon" is a second `Node` opened on the same state dir after dropping the first's handles without killing agents — add `#[cfg(feature = "test-hooks")] Node::forget_host_for_test(self)` if needed to drop without shutdown):
  - `handed_over_agent_keeps_running_with_its_screen`: spawn `sh` that prints `ready`, then `prepare_handoff`, `open_with_handoff` with the returned agents → `kept == [id]`, `list()` shows it `running`, `visible_text()` equals the pre-handoff screen, `input(id, …, b"echo again\n")` shows `again`.
  - `numbered_output_is_exact_across_handoff`: a writer printing numbered lines; after handoff and 300 ms, read the scrollback ring tail and history pages; every number appears once, in order, no gap.
  - `alt_screen_agent_keeps_both_screens`: an agent that prints `primary`, then `\x1b[?1049h` and `alt`; after handoff, `visible_text()` shows `alt`; feeding `\x1b[?1049l` through `restore_screen`'s pipeline (via the agent: `printf '\033[?1049l'`) shows `primary`.
  - `restore_failure_ends_the_agent`: hand over a record whose agent has fd `-1` → that id in `failed`, its row `exited`, its process gone.
  - `thaw_resumes_after_prepare`: `prepare_handoff` then `thaw` → the writer's output continues with no gap; `spawn` works again.
  - `spawn_during_handoff_is_refused`: after `prepare_handoff`, `spawn` errs with `fleetd is restarting`.
  - `other_running_rows_are_still_marked_lost`: a row `running` in the store but not in the record → `lost` after `open_with_handoff`.
- [ ] **Step 2: Run** `cargo test -p fleet-node --test handoff` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p fleet-node` — PASS.
- [ ] **Step 5: Commit** `feat(node): prepare a handoff, thaw, and open from one`

### Task 5: fleetd identity, `--handoff-check` and the handoff file

**Files:**
- Create: `crates/fleetd/src/handoff.rs`
- Modify: `crates/fleetd/src/main.rs`, `crates/fleetd/src/lib.rs`, `crates/fleetd/src/server.rs` (HelloOk fields)
- Test: `crates/fleetd/tests/handoff_file.rs`, unit tests in `handoff.rs`

**Interfaces:**
- Consumes: Task 1 types.
- Produces:
  - `fleetd --handoff-check` → stdout `handoff 1\n`, exit 0, touching nothing (no state dir, no log, no lock).
  - `pub struct Identity { pub instance: u64, pub binary: BinaryIdentity }`, `Identity::current() -> Identity` (instance from `/dev/urandom`, 8 bytes; binary from one `stat` of `std::env::current_exe()`); sent in every `HelloOk` with the `RestoreSummary` the daemon started with (default when none).
  - `handoff::write_record(state_dir: &Path, rec: &HandoffRecord) -> io::Result<OwnedFd>` (writes `handoff.bin` mode 0600, returns an fd opened on it); `handoff::take_record(fd: RawFd, state_dir: &Path) -> io::Result<HandoffRecord>` (reads, closes, deletes the file, checks `version == HANDOFF_VERSION`); `handoff::remove_stale(state_dir: &Path)` (called on every normal start).
  - `pub fn run_check(binary: &Path, timeout: Duration) -> Result<(), String>`: runs `<binary> --handoff-check`, kills it at `timeout`, `Err` reason strings: `not an executable file`, `check timed out`, `check failed (exit N)`, `no common handoff version`.

- [ ] **Step 1: Write failing tests:**
  - `handoff_check_prints_version` (integration, runs the built `fleetd` via `env!("CARGO_BIN_EXE_fleetd")`): stdout exactly `handoff 1\n`, exit 0, and the given `--state-dir` is not created.
  - `record_round_trips_and_file_is_removed` and `record_of_unknown_version_is_refused` (unit).
  - `stale_record_is_removed_on_normal_start` (integration): put a `handoff.bin` in a state dir, start `fleetd`, connect → file gone, no agents adopted. **(Review Focus)**
  - `check_refusals`: a missing path, a non-executable file, a script that sleeps 5 s (→ `check timed out` within ~2 s), a script that prints `handoff 9` (→ `no common handoff version`), a script exiting 3 (→ `check failed (exit 3)`).
  - `hello_carries_instance_and_binary`: two daemon starts → different `instance`; `binary.path` is the `fleetd` executable and `ino`/`size` match its `stat`.
- [ ] **Step 2: Run** `cargo test -p fleetd` — FAIL.
- [ ] **Step 3: Implement** (the `--handoff-check` branch is the first thing `main` does, before logging).
- [ ] **Step 4: Run** `cargo test -p fleetd` — PASS. Perf: wall suite (`fleetd_startup_instructions` and `fleetd_cold_start_ns` are the ones to watch: identity costs one `stat` and one 8-byte read).
- [ ] **Step 5: Commit** `feat(fleetd): instance token, binary identity, handoff file and --handoff-check`

### Task 6: `RestartDaemon` end to end — freeze, exec, start from handoff

**Files:**
- Modify: `crates/fleetd/src/lib.rs` (serve loop), `crates/fleetd/src/server.rs` (request), `crates/fleetd/src/main.rs` (start from `FLEETD_HANDOFF_FD`), `crates/fleetd/src/handoff.rs` (exec)
- Test: `crates/fleetd/tests/daemon_restart.rs`

**Interfaces:**
- Consumes: Tasks 1, 4, 5.
- Produces:
  - Server: `RestartDaemon` is passed to the serve loop over a channel (one in flight; a second gets `Error("a restart is already in progress")`). The serve loop: resolves `binary` (empty → `fleetd` on the daemon's `PATH`), `run_check(binary, 2 s)`, stops accepting, `node.prepare_handoff(now + 3 s)`, writes the record, answers the requester `RestartingDaemon{agents}`, waits for that frame to be flushed (bounded 200 ms), then `handoff::exec`. Any failure before `execve` returns: `node.thaw()`, accept again, answer `Error(reason)` if not answered yet, and keep `restart_error = reason` in the `RestoreSummary` served from then on.
  - `handoff::exec(binary: &Path, state_dir: &Path, record_fd: OwnedFd, keep: &[RawFd]) -> io::Error` (returns only on failure): clears close-on-exec on `keep` and the record fd, sets `FLEETD_HANDOFF_FD=<record fd>`, `execve(binary, [binary, "--state-dir", state_dir])` with the daemon's own environment; on failure restores close-on-exec and returns the error.
  - `main`: if `FLEETD_HANDOFF_FD` is set, remove it from the environment first (before logging, before the login-env capture), `take_record`, adopt `listener_fd`/`lock_fd` (`from_raw_fd`, set close-on-exec), `Node::open_with_handoff`, serve with `RestoreSummary{previous_instance, kept, failed, fell_back:false}`. If the record cannot be read: log it, `remove_stale`, and start normally with `fell_back = true`. The agents are still this process's children, holding inherited PTY fds, and must not be left running untracked. So `exec` also sets **`FLEETD_HANDOFF_PIDS`** to a comma-separated list of the agents' pids, and the new daemon always removes it from its environment along with `FLEETD_HANDOFF_FD`. On fallback, the new daemon:
    1. Sends SIGTERM to each of those process groups, then SIGKILL after `SHUTDOWN_GRACE`.
    2. Reaps the agents with `waitpid`.
    3. Closes every fd above 2 that isn't its own: it walks `/dev/fd` and keeps only the listener, lock and log fds.

    The normal start marks their rows `lost`.
  - Input requests stuck on an agent while freezing are answered `Error("fleetd is restarting")` (from Task 2's `write_input`).

- [ ] **Step 1: Write failing integration tests** (start the real `fleetd` binary in a temp state dir; restart onto the same binary path):
  - `restart_keeps_agents_running`: spawn an interactive `sh` and a numbered writer; record the shell's screen; send `RestartDaemon{binary: CARGO_BIN_EXE_fleetd}` → `RestartingDaemon{agents: 2}`; reconnect (retry ≤ 10 s) → `HelloOk.instance` differs, `restore.previous_instance` = old instance, `restore.kept` = both ids, the daemon's pid is unchanged (the test started it and holds its `Child`, which is still running); shell screen equals the recorded one; input `echo after\n` echoes; the writer's history has every number once, in order.
  - `agent_exiting_during_restart_is_recorded`: an agent `sh -c 'sleep 0.3; exit 5'` alive at the request → after the restart its row is `exited` with code 5.
  - `failed_check_refuses_and_keeps_serving`: `binary` = a script exiting 3 → `Error` containing `check failed (exit 3)`; same instance on the same connection's next request; agents untouched.
  - `exec_failure_thaws` : `binary` = a script that passes the check, then is replaced by a non-executable file between check and exec (test hook `#[cfg(feature = "test-hooks")]` delay between check and exec) → reconnect sees the same instance and `restore.restart_error` non-empty; agents still running, writer has no gap.
  - `second_restart_is_refused` **(Review Focus)**: two connections send `RestartDaemon` back to back → exactly one `RestartingDaemon`, the other `Error("a restart is already in progress")`.
  - `stuck_input_does_not_block_restart` **(Review Focus)**: an agent `sleep 30`; a second connection sends 4 MiB of `SendInput` to it; the restart still completes; the input request gets `Error("fleetd is restarting")` or its connection drops.
  - `handoff_fd_does_not_leak_to_agents` **(Review Focus)**: after a restart, spawn `sh -c 'env'` → output has neither `FLEETD_HANDOFF_FD` nor `FLEETD_HANDOFF_PIDS`.
  - `corrupt_record_falls_back`: test hook writes garbage into the record before exec → reconnect: new instance, `restore.fell_back == true`, previous agents `lost`, their pids no longer exist (`kill(pid, 0)` fails with ESRCH).
- [ ] **Step 2: Run** `cargo test -p fleetd --test daemon_restart` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace` — PASS. Perf: wall suite and compare; report.
- [ ] **Step 5: Commit** `feat(fleetd): restart into a new binary with running agents handed over`

### Task 7: `fleet daemon restart`

**Files:**
- Modify: `crates/fleet-cli/src/main.rs`, `crates/fleet-cli/skill/SKILL.md` (one line: `fleet daemon restart` restarts fleetd; agents keep running)
- Test: `crates/fleet-cli/tests/` (new `daemon_restart.rs`)

**Interfaces:**
- Consumes: `Connection::hello()` (Task 1); `RestartDaemon`/`RestartingDaemon` (Task 6).
- Produces: `fleet daemon restart [--binary <path>]` (clap: `Daemon { #[command(subcommand)] cmd: DaemonCmd }`, `DaemonCmd::Restart { binary: Option<PathBuf> }`). Default binary: empty string (the daemon's `PATH`) *(plan's call — matches the GUI's SSH behaviour; `--binary` overrides)*. Flow: remember `hello().instance`; send; on `RestartingDaemon{agents}` reconnect with retries for up to 10 s; compare instance and read `restore`; print exactly one Global Constraints message. "empty request" → the too-old message, exit 1.

- [ ] **Step 1: Write failing tests** against a real `fleetd` in a temp state dir: success prints `fleetd restarted: 1 agents kept` for one agent *(the copy is fixed; do not pluralise)*; a failing `--binary` prints `fleetd restart failed: check failed (exit 3); the old fleetd is still running` and exits 1; a fake server answering "empty request" prints the too-old message.
- [ ] **Step 2: Run** `cargo test -p fleet-cli --test daemon_restart` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p fleet-cli` — PASS.
- [ ] **Step 5: Commit** `feat(cli): fleet daemon restart`

### Task 8: GUI — Restart fleetd, update notice, outcome

**Files:**
- Modify: `crates/fleet-gui/src/core/status_menu.rs`, `crates/fleet-gui/src/core/session.rs`, `crates/fleet-gui/src/core/target.rs` (if the target kind is needed), `crates/fleet-gui/src/app/status_item.rs`, `crates/fleet-gui/src/app/app_state.rs`, `crates/fleet-gui/src/app/sidebar_view.rs`
- Test: `crates/fleet-gui/src/core/status_menu.rs` (tests), `crates/fleet-gui/tests/session.rs`

**Interfaces:**
- Consumes: `HelloOk` fields (Task 1), `RestartDaemon` (Task 6), `core::launch::find_tool("fleetd")`.
- Produces:
  - `StatusEntry::RestartFleetd` titled `Restart fleetd`, listed for local and SSH targets, after `SwitchFleetd`.
  - `Session::restart_daemon(&self)`: sends `RestartDaemon{binary}` with `binary = find_tool("fleetd")` for `Target::Local` and empty for SSH targets.
  - `UiEvent::DaemonRestarted { kept: usize, failed: Vec<u64>, fell_back: bool }` and `UiEvent::DaemonRestartFailed(String)`, raised after the reconnect by comparing the instance seen before the request with the new `HelloOk` (same instance → `DaemonRestartFailed(restore.restart_error)`); "empty request" → `DaemonRestartFailed` with the too-old copy.
  - `pub fn update_available(target: &Target, running: &BinaryIdentity, local: Option<&Path>) -> bool` in core: `false` for SSH targets; for local, `true` when `stat(local)`'s dev/ino/size/mtime differ from `running`.
  - App: the status menu item calls `restart_daemon`; a sidebar-footer notice `fleetd update available` with a `Restart` button while `update_available`; a toast for each outcome event (`fleetd restarted: N agents kept`, or the partial/failed copy from Global Constraints).

- [ ] **Step 1: Write failing tests:** `status_menu_offers_restart_for_local_and_ssh`; `update_available_never_for_ssh`, `update_available_when_binary_changes` (temp file, then rewrite it); in `tests/session.rs` with the fake server: `restart_reports_kept_agents_after_reconnect` (fake server answers `RestartingDaemon`, drops, comes back with a new instance and `kept=[1,2]` → `DaemonRestarted{kept:2,…}`) and `restart_with_same_instance_reports_failure`.
- [ ] **Step 2: Run** `cargo test -p fleet-gui` — FAIL.
- [ ] **Step 3: Implement core, then app wiring.**
- [ ] **Step 4: Run** `cargo test -p fleet-gui` and `cargo clippy --workspace --all-targets -- -D warnings` — PASS. Perf: wall suite (GUI rows).
- [ ] **Step 5: Commit** `feat(gui): Restart fleetd, update notice and restart outcome`

### Task 9: ADR 0032 and docs

**Files:**
- Create: `docs/decisions/0032-daemon-restart-handoff.md`
- Modify: `docs/decisions/0013-daemon-lifecycle.md` and `0018-m1-protocol-and-node.md` (an "Amended by: 0032" line each), `README.md` (fleetd section: restarting into a new build), `docs/plan.md` (M2c lists the restart; "live PTY handoff on upgrade" moves from Later to done, crash survival stays in Later)

**Interfaces:** none.

- [ ] **Step 1: Write ADR 0032**: context, decision (exec in place, record format, freeze mechanism, refusals, fallback, identity, SSH behaviour), what a restart loses (reattach limits; retained exited snapshots not carried), the `AgentHost` seam and the `holder_socket` slot for crash survival, consequences (first upgrade from a pre-0032 daemon ends agents once).
- [ ] **Step 2: Update the other docs** as listed; README gives `fleet daemon restart` and the GUI menu item.
- [ ] **Step 3: Commit** `docs: ADR 0032 daemon restart with agents surviving`
