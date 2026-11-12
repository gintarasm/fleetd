# M0 — Workspace, Benchmark Harness & Perf Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Cargo workspace whose CI fails any change that regresses daemon cold start, idle memory, PTY flood handling (50 fake agents) or daemon startup instruction count, unless the user approved it.

**Architecture:** A minimal `fleetd` (single-instance lock + unix socket, nothing else) and a minimal `fleet-node::PtyHost` (spawn commands in PTYs, drain output) give the harness real code to measure. `fleet-perf` provides a deterministic `fake-agent`, two benchmark suites (`wall` on a self-hosted Mac, `instr` via callgrind on Linux) that emit a common results JSON, a comparator against `bench/baselines.toml`, and an approval check on git history.

**Tech Stack:** Rust 1.99.1 (pinned), tokio, portable-pty, fs4 (flock), serde/serde_json/toml, clap, tempfile, iai-callgrind, GitHub Actions.

**Spec:** `docs/plan.md`, `docs/decisions/0013-daemon-lifecycle.md`, `docs/decisions/0014-performance-gate.md`, `docs/decisions/0016-crate-layout.md`

## Global Constraints

- Toolchain pinned via `rust-toolchain.toml` to `1.99.1`; `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` must pass.
- Add dependencies with `cargo add` (latest versions); no git dependencies.
- No `Co-Authored-By` trailers in commits.
- Budgets: daemon idle RSS < 50 MB (52_428_800 bytes); cold start < 200 ms (daemon part, measured to socket-ready); ≥ 50 concurrent agents.
- Gate tolerances: `instr` metrics 2% (0.02); `wall` metrics 5% (0.05).
- All metrics are lower-is-better.
- Only the user approves regressions: trailer `Perf-Approved: gintarasm` + updated baseline + `docs/perf-log.md` entry. Agents never write that trailer.
- One `fleetd` per user per machine (decision 0013).
- Work on branch `m0-perf-harness`, not `main`.

## Review Focus

1. **Stale socket after a crash** — a leftover `fleetd.sock` with no live daemon must not block startup → Task 3 test `stale_socket_is_replaced`.
2. **Second daemon instance** — must exit with code 2 and must not delete the running daemon's socket → Task 3 test `second_instance_exits_2_and_keeps_socket`.
3. **Agent dies early / PTY closes** — `PtyHost` must report exit and never hang `wait_all` → Task 2 test `killed_agent_reports_exit`.
4. **Metric missing from results or baseline** — gate must fail, not silently pass → Task 4 tests `missing_metric_fails`, `unknown_metric_fails`.
5. **Socket path over macOS limit (104 bytes)** — clear error, not a panic → Task 3 test `overlong_socket_path_errors`.

---

## File Structure

```
Cargo.toml                         workspace (members: crates/*), shared lints
rust-toolchain.toml                channel = "1.99.1", components rustfmt, clippy
.github/workflows/ci.yml           check, perf-instr (ubuntu), perf-wall (self-hosted mac)
bench/baselines.toml               metrics: suite, unit, baseline, budget + gate config
bench/README.md                    how to run suites, tighten, request approval
crates/fleet-node/src/lib.rs       pub mod pty
crates/fleet-node/src/pty.rs       PtyHost
crates/fleetd/src/lib.rs           Config, Daemon, startup(), run()
crates/fleetd/src/main.rs          CLI wrapper
crates/fleet-perf/src/bin/fake-agent.rs
crates/fleet-perf/src/bin/fleet-perf.rs      subcommands: run, compare, tighten, baseline-add, check-approval, flood-host
crates/fleet-perf/src/fake.rs      deterministic output generator
crates/fleet-perf/src/results.rs   Results JSON model
crates/fleet-perf/src/baseline.rs  baselines.toml model + compare + tighten
crates/fleet-perf/src/approval.rs  git-history approval check
crates/fleet-perf/src/wall.rs      wall suite scenarios
crates/fleet-perf/benches/instr.rs iai-callgrind bench of fleetd::startup
```

Other crates from decision 0016 (`fleet-proto`, `fleet-controller`, `fleet-plugin-host`, `fleet-gui`, `fleet-cli`) are created by the milestone that first needs them.

---

### Task 1: Workspace, toolchain, fake-agent

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `.gitignore` (`/target`), `.github/workflows/ci.yml` (job `check` only)
- Create: `crates/fleet-perf/Cargo.toml`, `crates/fleet-perf/src/lib.rs`, `crates/fleet-perf/src/fake.rs`, `crates/fleet-perf/src/bin/fake-agent.rs`
- Test: `crates/fleet-perf/src/fake.rs` (unit), `crates/fleet-perf/tests/fake_agent.rs`

**Interfaces:**
- Produces: `fleet_perf::fake::FakeOutput::new(seed: u64, line_len: usize, ansi: bool) -> FakeOutput` with `fn fill(&mut self, buf: &mut [u8])` (writes exactly `buf.len()` bytes, deterministic for a seed).
- Produces: binary `fake-agent --bytes <N> [--line-len 120] [--ansi] [--seed 0]` — writes exactly N bytes to stdout then exits 0.

- [ ] **Step 1:** `git checkout -b m0-perf-harness`; create workspace `Cargo.toml` (`[workspace] members = ["crates/*"]`, `resolver = "3"`, `[workspace.lints.clippy] all = "warn"`), `rust-toolchain.toml`, `.gitignore`.
- [ ] **Step 2: Write failing tests**
  - `same_seed_same_bytes`: two `FakeOutput::new(7, 80, true)` filling 64 KiB produce identical buffers.
  - `lines_have_requested_length`: with `ansi=false`, splitting on `\n` every full line has length 80.
  - `ansi_mode_emits_escapes`: with `ansi=true`, output contains `\x1b[`.
  - `fake_agent_writes_exact_bytes` (integration, `env!("CARGO_BIN_EXE_fake-agent")`): `--bytes 1000000` → stdout length is exactly 1_000_000, exit status 0.
- [ ] **Step 3:** Run `cargo test -p fleet-perf` — expect compile failure.
- [ ] **Step 4:** Implement `FakeOutput` (xorshift PRNG seeded by `seed`; printable ASCII words; in ANSI mode sprinkle SGR color codes and occasional `\x1b[2K\r` line rewrites, as agent TUIs do) and `fake-agent` (clap; write in 64 KiB chunks).
- [ ] **Step 5:** `cargo test -p fleet-perf` → PASS; `cargo fmt --check && cargo clippy --all-targets -- -D warnings` → clean.
- [ ] **Step 6:** Add `.github/workflows/ci.yml` job `check` (ubuntu-latest; fmt, clippy, test). Commit: `build: workspace skeleton and fake-agent`.

---

### Task 2: fleet-node PtyHost

**Files:**
- Create: `crates/fleet-node/Cargo.toml`, `crates/fleet-node/src/lib.rs`, `crates/fleet-node/src/pty.rs`
- Test: `crates/fleet-node/tests/pty_host.rs` (uses `/bin/sh` commands only; `fake-agent` is exercised by the fleet-perf suite in Task 6)

**Interfaces:**
- Produces:
  ```rust
  pub struct CommandSpec { pub program: PathBuf, pub args: Vec<String>, pub cwd: Option<PathBuf>, pub env: Vec<(String,String)>, pub cols: u16, pub rows: u16 }
  pub struct AgentId(pub u64);
  pub struct AgentStats { pub bytes_read: u64, pub exit_code: Option<i32> }
  impl PtyHost {
      pub fn new() -> Self;
      pub fn spawn(&self, spec: CommandSpec) -> std::io::Result<AgentId>;
      pub fn stats(&self, id: AgentId) -> Option<AgentStats>;
      pub fn kill(&self, id: AgentId) -> std::io::Result<()>;
      pub fn wait_all(&self, timeout: Duration) -> Result<(), WaitTimeout>;
  }
  ```
  Approach for M0: one reader thread per PTY (portable-pty), 64 KiB read buffer, output counted and discarded (M1 adds the terminal grid). Stats behind a `Mutex`/atomics.

- [ ] **Step 1: Write failing tests**
  - `counts_all_output`: spawn `sh -c 'head -c 2000000 /dev/zero | tr "\0" a'`; `wait_all(10s)` ok; `bytes_read >= 2_000_000` (PTY may translate newlines; no newlines here so equality expected) and `exit_code == Some(0)`.
  - `fifty_concurrent_agents`: spawn 50 of `head -c 200000` commands; `wait_all(30s)` ok; every agent `exit_code == Some(0)` and `bytes_read == 200_000`.
  - `killed_agent_reports_exit`: spawn `sleep 60`; `kill`; `wait_all(5s)` ok; `exit_code.is_some()`.
  - `nonzero_exit_is_reported`: `sh -c 'exit 3'` → `exit_code == Some(3)`.
- [ ] **Step 2:** `cargo test -p fleet-node` → FAIL (missing types).
- [ ] **Step 3:** Implement `PtyHost` per Interfaces.
- [ ] **Step 4:** `cargo test -p fleet-node` → PASS; fmt/clippy clean.
- [ ] **Step 5:** Commit: `feat(node): PtyHost spawns and drains agents in PTYs`.

---

### Task 3: fleetd single-instance daemon stub

**Files:**
- Create: `crates/fleetd/Cargo.toml`, `crates/fleetd/src/lib.rs`, `crates/fleetd/src/main.rs`
- Test: `crates/fleetd/tests/lifecycle.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct Config { pub state_dir: PathBuf }           // default ~/.fleet
  impl Config { pub fn socket_path(&self) -> PathBuf;    // state_dir/fleetd.sock
                pub fn lock_path(&self) -> PathBuf; }    // state_dir/fleetd.lock
  pub enum StartupError { AlreadyRunning, SocketPathTooLong(PathBuf), Io(std::io::Error) }
  pub struct Daemon { /* holds lock file + listener */ }
  pub fn startup(cfg: &Config) -> Result<Daemon, StartupError>;   // create dir, flock lock (non-blocking), remove stale socket, bind
  impl Daemon { pub async fn serve(self) -> std::io::Result<()>; } // accept and immediately close connections (protocol arrives in M1)
  ```
  Binary: `fleetd [--state-dir <path>]`; exit code 2 on `AlreadyRunning` with message `fleetd already running`. Runtime: tokio multi-thread (so idle RSS baseline is realistic).

- [ ] **Step 1: Write failing tests** (each uses a `tempfile::TempDir` as state dir; spawn the binary via `env!("CARGO_BIN_EXE_fleetd")`):
  - `becomes_ready`: after spawn, connecting to the socket succeeds within 2 s.
  - `second_instance_exits_2_and_keeps_socket`: start one, then a second exits with code 2; the first still accepts connections.
  - `stale_socket_is_replaced`: create a plain file at `fleetd.sock` (no daemon); daemon starts and accepts connections.
  - `overlong_socket_path_errors`: `startup` with a state dir making the socket path > 103 bytes returns `Err(SocketPathTooLong(_))`.
  - `lock_released_on_exit`: kill the first daemon (SIGTERM), start a new one → ready.
- [ ] **Step 2:** `cargo test -p fleetd` → FAIL.
- [ ] **Step 3:** Implement per Interfaces (fs4 for flock; SIGTERM/SIGINT → graceful exit removing the socket).
- [ ] **Step 4:** `cargo test -p fleetd` → PASS; fmt/clippy clean.
- [ ] **Step 5:** Commit: `feat(fleetd): single-instance daemon stub with socket`.

---

### Task 4: Results model, baselines and the comparator

**Files:**
- Create: `crates/fleet-perf/src/results.rs`, `crates/fleet-perf/src/baseline.rs`, `bench/baselines.toml`
- Modify: `crates/fleet-perf/src/bin/fleet-perf.rs` (create; subcommands `compare`, `tighten`, `baseline-add`)
- Test: unit tests in `baseline.rs`

**Interfaces:**
- Results JSON (written by both suites):
  ```json
  {"suite":"wall","metrics":{"fleetd_idle_rss_bytes":{"value":12345.0,"unit":"bytes"}}}
  ```
  `pub struct Results { pub suite: Suite, pub metrics: BTreeMap<String, Measurement> }`, `pub enum Suite { Wall, Instr }`, `pub struct Measurement { pub value: f64, pub unit: String }`.
- `bench/baselines.toml`:
  ```toml
  [gate]
  approvers = ["gintarasm"]
  tolerance = { instr = 0.02, wall = 0.05 }

  [metrics.fleetd_idle_rss_bytes]
  suite = "wall"
  unit = "bytes"
  budget = 52428800      # hard ceiling from docs/plan.md
  baseline = 0           # 0 = not yet recorded
  ```
  `pub struct Baselines { pub gate: Gate, pub metrics: BTreeMap<String, MetricSpec> }`, `MetricSpec { suite, unit, budget: Option<f64>, baseline: f64 }`.
- `pub fn compare(b: &Baselines, r: &Results) -> Report` where `Report { pub rows: Vec<Row>, pub failed: bool }`, `Row { name, baseline, value, delta_pct, status: Status }`, `enum Status { Ok, Improved, Regressed, OverBudget, Missing, Unknown, Unrecorded }`.
  Rules (only metrics of `r.suite` are considered): `value > budget` → OverBudget (fail); `baseline == 0` → Unrecorded (fail, message: run `fleet-perf tighten`); `value > baseline*(1+tol)` → Regressed (fail); `value < baseline*(1-tol)` → Improved (pass); metric in baselines but not results → Missing (fail); in results but not baselines → Unknown (fail, message: run `fleet-perf baseline-add`).
- `pub fn tighten(b: &mut Baselines, r: &Results)` — sets baseline to `value` only where baseline is 0 or `value < baseline`. Never raises a baseline. Agents may run this.
- `fleet-perf compare --baselines bench/baselines.toml --results <file>` prints a table, exit 1 if `failed`. `fleet-perf tighten …` rewrites the file. `fleet-perf baseline-add --results <file>` adds Unknown metrics with `baseline = value`, `budget` unset.

- [ ] **Step 1: Write failing tests:** `within_tolerance_ok` (baseline 100, value 104.9, wall → Ok), `regression_fails` (105.1 → Regressed, failed), `instr_uses_2pct` (instr, 102.1 → Regressed), `over_budget_fails_even_if_baseline_higher`, `improvement_passes` (94 → Improved), `missing_metric_fails`, `unknown_metric_fails`, `unrecorded_baseline_fails`, `other_suite_ignored`, `tighten_never_raises` (baseline 100, value 120 → still 100), `tighten_records_unrecorded` (0 → value), `toml_roundtrip_preserves_comments` (use `toml_edit` for writes so comments survive).
- [ ] **Step 2:** `cargo test -p fleet-perf baseline` → FAIL.
- [ ] **Step 3:** Implement per Interfaces. Seed `bench/baselines.toml` with gate config and metrics `fleetd_cold_start_ns`, `fleetd_idle_rss_bytes` (budget 52428800), `flood50_cpu_ns_per_mib`, `flood50_peak_rss_bytes`, `flood50_wall_ns`, `fleetd_startup_instructions` (suite instr) — all `baseline = 0`. `fleetd_cold_start_ns` budget 200000000.
- [ ] **Step 4:** Tests PASS; fmt/clippy clean.
- [ ] **Step 5:** Commit: `feat(perf): results model, baselines and comparator`.

---

### Task 5: Approval check

**Files:**
- Create: `crates/fleet-perf/src/approval.rs`
- Modify: `crates/fleet-perf/src/bin/fleet-perf.rs` (subcommand `check-approval --base <git-ref>`)
- Test: `crates/fleet-perf/tests/approval.rs` (builds throwaway git repos in a TempDir via `git` CLI)

**Interfaces:**
- `pub fn loosened(old: &Baselines, new: &Baselines) -> Vec<String>` — names where baseline or budget increased, a budget was removed, a metric was removed, or a tolerance increased. Recording a previously-0 baseline is not loosening.
- `pub fn check(repo: &Path, base: &str) -> Result<ApprovalOutcome, Error>`; `enum ApprovalOutcome { NotNeeded, Approved, Rejected(String) }`. Reads `bench/baselines.toml` at `git merge-base base HEAD` and at `HEAD`; if `loosened` is non-empty, requires (a) some commit in `base..HEAD` with trailer `Perf-Approved: <name>` where name ∈ `gate.approvers` (parse with `git log --format=%(trailers:key=Perf-Approved,valueonly)`), and (b) `docs/perf-log.md` changed in `base..HEAD`. Exit 1 on Rejected with the reason and the loosened metric names.

- [ ] **Step 1: Write failing tests:** `no_change_not_needed`, `tighten_not_needed`, `first_recording_not_needed` (0 → 500), `raised_baseline_without_trailer_rejected`, `trailer_without_perf_log_rejected`, `trailer_from_non_approver_rejected` (`Perf-Approved: claude`), `trailer_and_log_approved`, `removed_metric_needs_approval`, `raised_tolerance_needs_approval`.
- [ ] **Step 2:** `cargo test -p fleet-perf --test approval` → FAIL.
- [ ] **Step 3:** Implement per Interfaces (shell out to `git`).
- [ ] **Step 4:** Tests PASS; fmt/clippy clean.
- [ ] **Step 5:** Commit: `feat(perf): approval check for loosened baselines`.

---

### Task 6: Wall suite

**Files:**
- Create: `crates/fleet-perf/src/wall.rs`
- Modify: `crates/fleet-perf/src/bin/fleet-perf.rs` (subcommands `run --suite wall --out <file> [--quick]`, hidden `flood-host --agents N --bytes B --fake-agent <path>`)
- Test: `crates/fleet-perf/tests/wall_smoke.rs`

**Interfaces:**
- Consumes: `fleetd` binary (Task 3), `fleet_node::pty::PtyHost` (Task 2), `fake-agent` (Task 1), `Results` (Task 4).
- Binary paths: `fleet-perf run` locates `fleetd` and `fake-agent` next to its own executable (`std::env::current_exe()?.parent()`), so CI runs `cargo build --release --workspace` first.
- Metrics produced (lower is better, release build):
  - `fleetd_cold_start_ns` — median of 20 runs: spawn `fleetd --state-dir <fresh tempdir>` → first successful socket connect; poll every 1 ms; kill after.
  - `fleetd_idle_rss_bytes` — start fleetd, wait until ready + 1 s, read RSS (`proc_pidinfo`/`ps -o rss=` on macOS, `/proc/<pid>/statm` on Linux); median of 5.
  - `flood50_wall_ns`, `flood50_cpu_ns_per_mib`, `flood50_peak_rss_bytes` — run `fleet-perf flood-host --agents 50 --bytes 8388608` as a child (it uses `PtyHost` to run 50 `fake-agent --ansi` and `wait_all(120s)`, asserting every agent read exactly 8 MiB and exited 0, else exit 1). From the parent: wall time, child rusage (`wait4`) `ru_utime+ru_stime` divided by total MiB (400), and `ru_maxrss` (bytes on macOS, KiB on Linux — normalise). Median of 5.
  - `--quick` reduces runs to 1 and bytes to 256 KiB (for the smoke test only).

- [ ] **Step 1: Write failing test** `wall_suite_quick_emits_all_metrics`: run `fleet-perf run --suite wall --quick --out <tmp>`; parse `Results`; suite is Wall; contains exactly the five wall metric names; every value > 0.
- [ ] **Step 2:** `cargo test -p fleet-perf --test wall_smoke` → FAIL.
- [ ] **Step 3:** Implement per Interfaces.
- [ ] **Step 4:** Test PASS. Then `cargo build --release --workspace && target/release/fleet-perf run --suite wall --out /tmp/wall.json` locally; confirm `fleetd_idle_rss_bytes` < 52428800 and `fleetd_cold_start_ns` < 200000000 (if not, stop and report — a budget miss at M0 is a design problem).
- [ ] **Step 5:** Commit: `feat(perf): wall suite (cold start, idle RSS, 50-agent flood)`.

---

### Task 7: Instruction-count suite

**Files:**
- Create: `crates/fleet-perf/benches/instr.rs`
- Modify: `crates/fleet-perf/Cargo.toml` (`[[bench]] name = "instr" harness = false`), `crates/fleet-perf/src/bin/fleet-perf.rs` (`run --suite instr --out <file>`)
- Modify: `.github/workflows/ci.yml` (job `perf-instr`)

**Interfaces:**
- Consumes: `fleetd::startup`, `fleetd::Config` (Task 3); `Results` (Task 4).
- Bench `fleetd_startup`: iai-callgrind library benchmark calling `fleetd::startup(&Config { state_dir: tempdir })` then dropping the `Daemon` (setup creates the tempdir outside the measured region). If the crate has been renamed (gungraun), use the successor with the same semantics.
- `fleet-perf run --suite instr` runs `cargo bench -p fleet-perf --bench instr` with JSON summary output and maps the benchmark's `Ir` to metric `fleetd_startup_instructions` (unit `instructions`). Linux-only; on macOS it exits 1 with `instr suite requires Linux + valgrind`.

- [ ] **Step 1:** Write the bench and the `run --suite instr` mapping.
- [ ] **Step 2:** Verify on Linux via Docker: `docker run --rm -v "$PWD":/w -w /w rust:1.99.1 sh -c 'apt-get update && apt-get install -y valgrind && cargo install iai-callgrind-runner --version <matching> && cargo run -p fleet-perf --release -- run --suite instr --out /w/target/instr.json'`; expect `target/instr.json` with `fleetd_startup_instructions` > 0. Run twice; values must differ by < 0.5% (if not, move nondeterministic work out of the measured region).
- [ ] **Step 3:** Add CI job `perf-instr` (ubuntu-latest, installs valgrind + runner, `run --suite instr`, `compare`, uploads results artifact).
- [ ] **Step 4:** Commit: `feat(perf): instruction-count suite for fleetd startup`.

---

### Task 8: Gate wiring, baselines, docs

**Files:**
- Modify: `.github/workflows/ci.yml` (job `perf-wall`; `check-approval` step), `bench/baselines.toml`, `CLAUDE.md`
- Create: `bench/README.md`

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1:** Add CI job `perf-wall` on `runs-on: [self-hosted, macOS, fleet-perf]`: `cargo build --release --workspace`, `fleet-perf run --suite wall --out wall.json`, `fleet-perf compare`, upload artifact. Add step `fleet-perf check-approval --base origin/main` to `check` (with `fetch-depth: 0`). All three jobs required for merge.
- [ ] **Step 2:** Record initial baselines: run wall suite (release) on the user's Mac and instr suite in Docker; `fleet-perf tighten` with each results file; commit `bench/baselines.toml`. Verify `fleet-perf compare` passes against a fresh run of each suite.
- [ ] **Step 3:** Write `bench/README.md`: how to run each suite, what each metric measures, `tighten` vs `baseline-add` (agents may run both), and the approval procedure (user-only `Perf-Approved: gintarasm` trailer + `docs/perf-log.md` row). Add to `CLAUDE.md`: "Before finishing any change, run `fleet-perf run --suite wall` and `compare`; if anything regresses, stop and ask the user."
- [ ] **Step 4:** Commit: `ci: perf gate jobs, initial baselines, docs`. Push branch.
- [ ] **Step 5 (user action):** Register the Mac as a self-hosted GitHub Actions runner for `gintarasm/fleetd` with labels `self-hosted, macOS, fleet-perf`; enable branch protection on `main` requiring `check`, `perf-instr`, `perf-wall`. Until the runner exists, `perf-wall` stays queued.
