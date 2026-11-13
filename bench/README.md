# Performance gate

Baselines live in `bench/baselines.toml`; all metrics are lower-is-better. Design: `docs/decisions/0014-performance-gate.md`, enforcement: `0017`.

## Running

```sh
cargo build --release --workspace
target/release/fleet-perf run --suite wall --out wall.json      # add --quick for a smoke run
target/release/fleet-perf compare --baselines bench/baselines.toml --results wall.json
```

The `instr` suite needs Linux + valgrind (`cargo install gungraun-runner --version 0.20.0 --locked`):

```sh
cargo run -p fleet-perf --release --bin fleet-perf -- run --suite instr --out instr.json
```

Locally on macOS use Docker (`rust:1`, install `valgrind`) with `--security-opt seccomp=unconfined` (valgrind fails otherwise) and `-e CARGO_TARGET_DIR=/w/target-docker`.

## Metrics

| Metric | Suite | Measures |
|---|---|---|
| `fleetd_cold_start_ns` | wall | spawn `fleetd` until its socket accepts a connection (minimum of 20; spawn latency has a heavy right tail); budget 200 ms |
| `fleetd_idle_rss_bytes` | wall | daemon RSS 1 s after ready (median of 5); budget 50 MB |
| `flood50_wall_ns` | wall | wall time for 50 fake agents flooding PTYs (median of 5) |
| `flood50_cpu_ns_per_mib` | wall | CPU time of the PTY host per MiB read |
| `flood50_peak_rss_bytes` | wall | peak RSS of the PTY host |
| `daemon_flood50_wall_ns` | wall | `fleetd` spawns 50 `fake-agent --ansi --bytes 8388608` via the protocol, one client attached to every agent on one connection (snapshot + stream consumed); first spawn request to last `AgentExited` (median of 3) |
| `daemon_flood50_cpu_ns_per_mib` | wall | CPU time (user + sys) of the `fleetd` process alone (`proc_pid_rusage` / `/proc/<pid>/stat`, agents excluded) over that run, per MiB of agent output |
| `daemon_flood50_peak_rss_bytes` | wall | peak `fleetd` RSS during that run, sampled every 50 ms |
| `daemon_flood50_lagged_subscriptions` | wall | subscriptions `fleetd` dropped as lagged during that run (unit `count`) |
| `flood50_attached4_lagged_subscriptions` | wall | `fleetd` with 50 `fake-agent --ansi --bytes 8388608` spawned on one connection that is attached to agents 0–3 only (a GUI showing four panes) and consumes everything until all exit: subscriptions dropped as lagged (`Event::Lagged`; worst of 3 runs; unit `count`) |
| `input_echo_flood50_ns` | wall | that flood plus a 51st agent `/bin/cat` attached on its own connection: once every flood agent is spawned, `SendInput` of one byte written until the `Output` holding its PTY echo is read, one at a time 5 ms apart (median of 100, all taken while the flood runs) |
| `reattach_snapshot_ns` | wall | `Attach` sent to `AttachOk` received for an idle agent that printed 8 MiB (median of 50, detaching in between) |
| `fleetd_startup_instructions` | instr | Callgrind instruction count of `fleetd::startup` (x86_64, ubuntu-24.04) |
| `frame_decode_output_64x64k_instructions` | instr | decode 64 back-to-back 64 KiB `Output` frames with `FrameCodec` |
| `terminal_feed_64k_instructions` | instr | feed 64 KiB of deterministic agent-like ANSI (`FakeOutput`, `\n` as `\r\n`) into a blank 120x40 `Terminal` |
| `snapshot_120x40_instructions` | instr | `Terminal::snapshot` of a 120x40 screen filled with that output |

Tolerances: `instr` 2%, `wall` 5%.

`flood50_cpu_ns_per_mib` and `daemon_flood50_cpu_ns_per_mib` are `report_only` (see below) until the self-hosted runner is characterised; the other `daemon_flood50_*` metrics and `reattach_snapshot_ns` until their CI variance is characterised. `fleetd_cold_start_ns` is `report_only` against its baseline because a ~2 ms process start varies about ±15% run to run on the runner; its 200 ms budget still gates. `flood50_wall_ns` is `report_only` because it varied +6% to +23% on unchanged code on the shared runner. `daemon_flood50_lagged_subscriptions` is report-only because the M1 node cannot stream 50 flooding agents to one attached client (ADR 0018); a run with 0 lagged subscriptions is an invalid value for `compare` (values must be > 0), so once lagging is fixed the metric must be retired.

In `daemon_flood50` fleetd drops most subscriptions as lagged under full load (drop policy, `docs/decisions/0018-m1-protocol-and-node.md`); the client does not re-attach (the notice does not name the agent), so the scenario measures the daemon with clients attached until they lag out. The count is reported as `daemon_flood50_lagged_subscriptions` (and printed to stderr); wall time, CPU and RSS shift with it. Setup (fixtures, the screen to snapshot) runs outside the measured region of every `instr` benchmark.

### Report-only metrics

A metric with `report_only = true` is still measured, printed (its status is marked `(report-only)`) and tightened, but a regression against its baseline does not fail the gate. Its `budget`, if any, still does: the budget is the hard limit. Missing, unknown, invalid (non-finite or <= 0) and unit-mismatched values still fail: the measurement must exist and be sane. Turning `report_only` on is a loosening and needs user approval; turning it off is free.

## Baselines

- `fleet-perf tighten --results <file>` records unrecorded baselines (`baseline = 0`) and locks in improvements **beyond tolerance** (status `improved`); within-tolerance noise leaves the baseline alone, so baselines do not ratchet down on lucky runs. It never raises. Agents may run it.
- `fleet-perf baseline-add` adds a baseline for a new metric. Agents may run it.
- Raising a baseline or accepting a regression is user-only: a commit with the trailer `Perf-Approved: gintarasm`, the updated baseline, and a row in `docs/perf-log.md`. `fleet-perf check-approval --base <ref>` enforces this in CI. Agents never write the trailer or the log.
- Per-metric tolerance: a metric may set `tolerance = <fraction>` to override its suite's tolerance. Setting one above the effective tolerance at the merge-base, or raising it, is a loosening and needs the same user approval. No metric sets one today.
- Gate code: changing or deleting `crates/fleet-perf/src/{baseline,approval,wall,instr,lib,results}.rs`, `crates/fleet-perf/src/bin/fleet-perf.rs`, `crates/fleet-perf/Cargo.toml`, `rust-toolchain.toml`, `crates/fleet-perf/benches/instr.rs` or anything under `.github/workflows/` (when it existed at the merge-base) needs the same approval — trailer plus `docs/perf-log.md` entry. Newly created files are exempt.
- `compare` also fails on invalid values (non-finite or <= 0) and on a results unit that differs from the baseline's unit.

### Recording baselines

Both suites are recorded from CI, never from a local run:

- **wall:** download the `wall-results` artifact from the self-hosted runner's `perf-wall` job and run `fleet-perf tighten --results <file>` (same procedure as instr). Confirm the numbers by re-running `perf-wall` before committing.
- **instr:** re-run `perf-instr` at least 3 times and check the spread of `fleetd_startup_instructions` across the `instr-results` artifacts before recording one of them.

`fleetd_startup_instructions` starts at `baseline = 0`, so the first CI run of `perf-instr` on a branch is EXPECTED to fail as Unrecorded. Record it by downloading the `instr-results` artifact (x86_64; after the spread check above) and running `fleet-perf tighten --results <file>`, then commit the updated `bench/baselines.toml` to the same PR. Recording 0 -> value needs no approval.

## One-time user setup

1. Register this Mac as a self-hosted runner for the repo with labels `self-hosted, macOS, fleet-perf` (Settings > Actions > Runners). Put `~/.cargo/bin` on the runner's `PATH`, and keep the Mac awake (no sleep) while it is the runner.
2. Before merging any PR, check that `check`, `perf-instr`, `perf-wall` and `perf-approval` are all green: on the Free plan GitHub refuses branch protection for this private repo, so this manual check is the only gate (`docs/decisions/0017-perf-approval-enforcement.md`). Until the runner exists `perf-wall` stays queued.
3. With GitHub Pro: branch protection on `main` requiring a PR and those four status checks.
