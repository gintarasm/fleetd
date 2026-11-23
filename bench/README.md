# Performance gate

Baselines live in `bench/baselines.toml`; all metrics are lower-is-better. Design: `docs/decisions/0014-performance-gate.md`, enforcement: `0017`.

## Running

```sh
cargo build --release --workspace
caffeinate -d target/release/fleet-perf run --suite wall --out wall.json   # add --quick for a smoke run
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
| `daemon_flood50_wall_ns` | wall | `fleetd` spawns 50 `fake-agent --ansi --bytes 8388608` via the protocol, one client attached to every agent on one connection (snapshot + stream consumed); first spawn request to last `AgentExited` (median of 3); since M2a the first spawn waits for the daemon's login-shell env capture (ADR 0024), which falls inside the timed region |
| `daemon_flood50_cpu_ns_per_mib` | wall | CPU time (user + sys) of the `fleetd` process alone (`proc_pid_rusage` / `/proc/<pid>/stat`, agents excluded) over that run, per MiB of agent output |
| `daemon_flood50_peak_rss_bytes` | wall | peak `fleetd` RSS during that run, sampled every 50 ms |
| `daemon_flood50_lagged_subscriptions` | wall | subscriptions `fleetd` dropped as lagged during that run (unit `count`) |
| `flood50_attached4_lagged_subscriptions` | wall | `fleetd` with 50 `fake-agent --ansi --bytes 8388608` spawned on one connection that is attached to agents 0–3 only (a GUI showing four panes) and consumes everything until all exit: subscriptions dropped as lagged (`Event::Lagged`; worst of 3 runs; unit `count`) |
| `input_echo_flood50_ns` | wall | that flood plus a 51st agent `/bin/cat` attached on its own connection: once every flood agent is spawned, `SendInput` of one byte written until the `Output` holding its PTY echo is read, one at a time 5 ms apart (median of 100, all taken while the flood runs) |
| `reattach_snapshot_ns` | wall | `Attach` sent to `AttachOk` received for an idle agent that printed 8 MiB (median of 50, detaching in between); since M2a `AttachOk` also carries the history tail (up to 256 KiB / 4096 lines) |
| `exec_true_roundtrip_ns` | wall | `Exec /usr/bin/true` (ADR 0091) sent to its `ExecResult` received, on one connection to an idle daemon (median of 50 after 3 warm-up calls); budget 20 ms; no baseline yet (the user records it) |
| `gui_cold_start_ns` | wall | `fleetd` with 10 idle agents (`/bin/cat`); `fleet-gui` (`FLEET_GUI_BENCH=cold-start --target local`) from just before spawn until it prints `ready`, which it does once the frame that drew every agent of its first `ListAgents` reply in the sidebar is presented (minimum of 10); budget 200 ms |
| `input_to_render_ns` | wall | `fleet-gui` (`FLEET_GUI_BENCH=input`) showing a `/bin/cat` agent in pane 1 on an idle daemon: 50 keys `x`, 20 ms apart, each dispatched through the window's key path (`Window::dispatch_keystroke`, the pane's key and text input handlers) until the frame whose grid holds its PTY echo is presented (`on_next_frame`); median of the 50, printed by the GUI; budget 16 ms |
| `input_to_render_flood50_ns` | wall | the same while 50 `fake-agent --ansi --forever` flood (spawned on their own connection, which consumes everything until all exit), with 3 of them shown in further panes (`FLEET_GUI_BENCH_EXTRA_PANES`); the GUI is launched once all 50 are spawned, and they are killed (`KillAgent`) only after it printed its median, so every sample is taken during the flood of all 50; the run fails if any flood agent exited before that (a fixed 8 MiB flood lasted ~3 s and raced the GUI's ~1.4 s); budget 16 ms |
| `gui_diff_open_ns` | wall | an idle `fleetd`; `fleet-gui` (`FLEET_GUI_BENCH=diff-open`) opens a diff pane (ADR 0094) on a 10,000-line fixture `DiffView` (20 files, hunks of 50; no git, no daemon round trip) 20 times, each timed from the open (layout split and `core::diff` layout) to the pane's first presented frame, closing it in between; median; budget 100 ms; no baseline yet (the user records it) |
| `gui_diff_scroll_frame_ns` | wall | 50 `fake-agent --ansi --forever` flood as for `input_to_render_flood50_ns`; `fleet-gui` (`diff-scroll`, `FLEET_GUI_BENCH_AGENTS=50`) shows that fixture diff and scrolls it three rows a frame, timing 100 frames (after 20 warm-up) from the workspace's render to the frame's presentation; median; budget 16 ms; no baseline yet (the user records it) |
| `gui_graph_open_ns` | wall | **not emitted yet** (ADR 0073: its `wall.rs` hookup needs the user's approval): `fleetd` with 50 idle agents; `fleet-gui` (`FLEET_GUI_BENCH=graph-open`) toggles the graph view on 20 times (off in between), each timed from the dispatched `ToggleGraph` to its first presented frame; median; budget 100 ms |
| `gui_graph_frame_flood50_ns` | wall | **not emitted yet** (as above): 50 `fake-agent --ansi --forever` flood as for `input_to_render_flood50_ns`; `fleet-gui` (`graph-flood`, `FLEET_GUI_BENCH_AGENTS=50`) shows the graph and times 100 redraws (after 20 warm-up) from the workspace's render to the end of the graph's paint; median; budget 16 ms. Both graph modes draw a synthetic spawned-by tree (fan-out 3) since fleetd refuses `fleet.*` labels from a client |
| `fleetd_startup_instructions` | instr | Callgrind instruction count of `fleetd::startup` (x86_64, ubuntu-24.04) |
| `frame_decode_output_64x64k_instructions` | instr | decode 64 back-to-back 64 KiB `Output` frames with `FrameCodec` |
| `terminal_feed_64k_instructions` | instr | feed 64 KiB of deterministic agent-like ANSI (`FakeOutput`, `\n` as `\r\n`) into a blank 120x40 `Terminal` |
| `snapshot_120x40_instructions` | instr | `Terminal::snapshot` of a 120x40 screen filled with that output |
| `terminal_feed_scroll_64k_instructions` | instr | the `terminal_feed_64k` feed plus `drain_history` of the lines it scrolled off (history capture, ADR 0021) |
| `history_push_1k_lines_instructions` | instr | push 1000 lines of 100 bytes into a new per-agent `History` store (includes its buffered file writes) |
| `history_read_page_instructions` | instr | one `read_page` of 1000 lines from the middle of a flushed store of 100 000 lines of 100 bytes |

The three GUI metrics need `fleet-gui` next to `fleet-perf` and a logged-in, **unlocked** macOS GUI session with the display awake (the self-hosted runner's; `perf-wall` runs the suite under `caffeinate -d`). A locked screen, a sleeping display or an occluded window presents no frames (gpui stops an occluded window's display link); in bench mode the GUI floats its window above other apps' windows on every Space (`bench::window_opened`; it does not activate itself) so it is never covered. A locked screen or a sleeping display still stalls it: the run checks `CGSSessionScreenIsLocked` up front and before each GUI launch and fails with "the screen is locked"; a GUI that presents no frame in time (exit 4) fails too. The GUI's bench hooks (`crates/fleet-gui/src/app/bench.rs`) never show the connect sheet: a failed connect exits 3. If `fleet-gui` is missing or exits non-zero before its line, the wall run fails with a message saying so. They are skipped, with a message, only on Linux (`fleet-gui` is macOS-only) and when `FLEET_PERF_NO_GUI` is set; `compare` then reports them missing, so a gate run fails.

The quick smoke test (`crates/fleet-perf/tests/wall_smoke.rs`) sets `FLEET_PERF_NO_GUI`, so a plain `cargo test` opens no windows; run `FLEET_PERF_GUI_SMOKE=1 cargo test -p fleet-perf --test wall_smoke` (macOS, screen unlocked) to include the GUI scenarios.

The `gui-macos` CI job lints and unit-tests the macOS GUI shell (`cargo clippy -p fleet-gui --all-targets`, `cargo test -p fleet-gui --lib`; Linux CI compiles that shell out). It runs on the same self-hosted runner as `perf-wall`, which takes one job at a time, so the two never overlap. Its tests use gpui's headless test platform and need no unlocked screen.

Tolerances: `instr` 2%, `wall` 5%.

`flood50_cpu_ns_per_mib` and `daemon_flood50_cpu_ns_per_mib` are `report_only` (see below) until the self-hosted runner is characterised; the other `daemon_flood50_*` metrics and `reattach_snapshot_ns` until their CI variance is characterised. `fleetd_cold_start_ns` is `report_only` against its baseline because a ~2 ms process start varies about ±15% run to run on the runner; its 200 ms budget still gates. `flood50_wall_ns` is `report_only` because it varied +6% to +23% on unchanged code on the shared runner. `daemon_flood50_lagged_subscriptions` was made report-only because the M1 node could not stream 50 flooding agents to one attached client; since M2a's merging subscriber queue it reads 0 (median of 3 floods; a single flood occasionally drops a few), see ADR 0018. 0 is a valid value for `count` metrics. `flood50_attached4_lagged_subscriptions` is gated by `budget = 0` (any lagged subscription fails); its baseline stays 0 and `compare` gates a `count` metric that has a budget and `baseline = 0` by the budget alone (value <= budget is ok, above it is over budget), because neither `tighten` nor `baseline-add` records a count of 0. A `count` with `baseline = 0` and no budget is still unrecorded.

In `daemon_flood50` the client does not re-attach after an `Event::Lagged`, so a dropped subscription stays dropped for the rest of the run (drop policy, `docs/decisions/0018-m1-protocol-and-node.md`); the count is reported as `daemon_flood50_lagged_subscriptions` (and printed to stderr), and wall time, CPU and RSS shift with it. In M1 fleetd dropped 45–48 of the 50; since M2a all 50 normally stream to the end, which costs memory (each subscription merges queued output up to 16 KiB per item, at most about 256 x max(16 KiB, one read)), so `daemon_flood50_peak_rss_bytes` is higher than in M1. Setup (fixtures, the screen to snapshot) runs outside the measured region of every `instr` benchmark.

### Report-only metrics

A metric with `report_only = true` is still measured, printed (its status is marked `(report-only)`) and tightened, but a regression against its baseline does not fail the gate. Its `budget`, if any, still does: the budget is the hard limit. Missing, unknown, invalid (non-finite or <= 0, except 0 for a `count` metric) and unit-mismatched values still fail: the measurement must exist and be sane. Turning `report_only` on is a loosening and needs user approval; turning it off is free.

## Baselines

- `fleet-perf tighten --results <file>` records unrecorded baselines (`baseline = 0`) and locks in improvements **beyond tolerance** (status `improved`); within-tolerance noise leaves the baseline alone, so baselines do not ratchet down on lucky runs. It never raises. Agents may run it.
- `fleet-perf baseline-add` adds a baseline for a new metric. Agents may run it.
- Neither `tighten` nor `baseline-add` writes a baseline of 0 for a `count` metric (0 means unrecorded): `tighten` keeps the existing baseline and `baseline-add` skips the metric, each printing a note.
- Raising a baseline or accepting a regression is user-only: a commit with the trailer `Perf-Approved: gintarasm`, the updated baseline, and a row in `docs/perf-log.md`. `fleet-perf check-approval --base <ref>` enforces this in CI. Agents never write the trailer or the log.
- Per-metric tolerance: a metric may set `tolerance = <fraction>` to override its suite's tolerance. Setting one above the effective tolerance at the merge-base, or raising it, is a loosening and needs the same user approval. No metric sets one today.
- Gate code: changing or deleting `crates/fleet-perf/src/{baseline,approval,wall,instr,lib,results}.rs`, `crates/fleet-perf/src/bin/fleet-perf.rs`, `crates/fleet-perf/Cargo.toml`, `rust-toolchain.toml`, `crates/fleet-perf/benches/instr.rs` or anything under `.github/workflows/` (when it existed at the merge-base) needs the same approval — trailer plus `docs/perf-log.md` entry. Newly created files are exempt.
- `compare` also fails on invalid values (non-finite or <= 0; 0 is valid when both the results and the baseline unit are `count`) and on a results unit that differs from the baseline's unit.

### Recording baselines

Both suites are recorded from CI, never from a local run:

- **wall:** download the `wall-results` artifact from the self-hosted runner's `perf-wall` job and run `fleet-perf tighten --results <file>` (same procedure as instr). Confirm the numbers by re-running `perf-wall` before committing.
- **instr:** re-run `perf-instr` at least 3 times and check the spread of `fleetd_startup_instructions` across the `instr-results` artifacts before recording one of them.

`fleetd_startup_instructions` starts at `baseline = 0`, so the first CI run of `perf-instr` on a branch is EXPECTED to fail as Unrecorded. Record it by downloading the `instr-results` artifact (x86_64; after the spread check above) and running `fleet-perf tighten --results <file>`, then commit the updated `bench/baselines.toml` to the same PR. Recording 0 -> value needs no approval.

## One-time user setup

1. Register this Mac as a self-hosted runner for the repo with labels `self-hosted, macOS, fleet-perf` (Settings > Actions > Runners). Put `~/.cargo/bin` on the runner's `PATH`, and keep the Mac awake (no sleep) while it is the runner.
2. Before merging any PR, check that `check`, `perf-instr`, `perf-wall` and `perf-approval` are all green: on the Free plan GitHub refuses branch protection for this private repo, so this manual check is the only gate (`docs/decisions/0017-perf-approval-enforcement.md`). Until the runner exists `perf-wall` stays queued.
3. With GitHub Pro: branch protection on `main` requiring a PR and those four status checks.
