# Performance regression log

Every accepted regression against `bench/baselines.toml` must have an entry here, approved by the user (see [decisions/0014](decisions/0014-performance-gate.md)).

| Date | Commit | Metric | Before → After | Justification | Approved by |
|---|---|---|---|---|---|
| 2026-11-13 | m1-local-node (PR #2) | `fleetd_idle_rss_bytes` | 2179072 → 4227072 (+94%) | M1 opens the node state at startup (SQLite + Node). Measured: ~1.3 MB is SQLite code pages (clean, reclaimable), ~0.25 MB heap, ~0.17 MB larger binary. Budget 50 MB. Lazy-opening would only move the cost to the first client. | gintarasm |
| 2026-11-13 | m0-perf-harness 710c356 (via PR #2) | `fleetd_cold_start_ns` → report-only | baseline unchanged (2418374.5) | ~2 ms process start ranges 2.14–2.85 ms run to run on the self-hosted runner, against a 5% tolerance. The 200 ms budget still gates; report-only metrics now still fail when over budget. `fleetd_startup_instructions` catches code regressions. | gintarasm |
| 2026-11-13 | m1-local-node (PR #2) | gate code: `crates/fleet-perf/{Cargo.toml, benches/instr.rs, src/instr.rs, src/wall.rs, src/baseline.rs}` | n/a | M1 benchmarks (daemon flood, reattach snapshot, frame decode, terminal feed, snapshot) and the report-only budget fix. No existing metric loosened besides the rows above. | gintarasm |
