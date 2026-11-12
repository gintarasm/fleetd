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
| `fleetd_startup_instructions` | instr | Callgrind instruction count of `fleetd::startup` (x86_64, ubuntu-24.04) |

Tolerances: `instr` 2%, `wall` 5%.

`flood50_cpu_ns_per_mib` is `report_only` (see below) until the self-hosted runner is characterised.

### Report-only metrics

A metric with `report_only = true` is still measured, printed (its status is marked `(report-only)`) and tightened, but a regression or budget breach on it does not fail the gate. Missing, unknown, invalid (non-finite or <= 0) and unit-mismatched values still fail: the measurement must exist and be sane. Turning `report_only` on is a loosening and needs user approval; turning it off is free.

## Baselines

- `fleet-perf tighten --results <file>` records unrecorded baselines (`baseline = 0`) and locks in improvements **beyond tolerance** (status `improved`); within-tolerance noise leaves the baseline alone, so baselines do not ratchet down on lucky runs. It never raises. Agents may run it.
- `fleet-perf baseline-add` adds a baseline for a new metric. Agents may run it.
- Raising a baseline or accepting a regression is user-only: a commit with the trailer `Perf-Approved: gintarasm`, the updated baseline, and a row in `docs/perf-log.md`. `fleet-perf check-approval --base <ref>` enforces this in CI. Agents never write the trailer or the log.
- Per-metric tolerance: a metric may set `tolerance = <fraction>` to override its suite's tolerance. Setting one above the effective tolerance at the merge-base, or raising it, is a loosening and needs the same user approval. No metric sets one today.
- Gate code: changing or deleting `crates/fleet-perf/src/{baseline,approval,wall,instr}.rs`, `crates/fleet-perf/benches/instr.rs` or anything under `.github/workflows/` (when it existed at the merge-base) needs the same approval — trailer plus `docs/perf-log.md` entry. Newly created files are exempt.
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
