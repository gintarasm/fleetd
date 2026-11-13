# 0014 — Performance budgets and the regression gate

**Status:** accepted · 2026-11-13

**Decision:**
- **Budgets:** cold start < 200 ms; ≥ 50 concurrent agents per node without UI lag; input-to-render < 16 ms; daemon idle RSS < 50 MB.
- **Built first (M0):** a benchmark harness that spawns 50 fake agents flooding their PTYs, measuring daemon RSS/CPU, reattach snapshot latency, protocol throughput and GUI frame time. These benchmarks are required in CI.
- **Measurement:** deterministic instruction counts (iai-callgrind, Linux) for daemon/protocol code, gated at ~2%. Wall-clock budgets for GUI and end-to-end on a dedicated self-hosted Mac runner, gated at ~5%.
- **Baselines:** `bench/baselines.toml`. Improvements tighten baselines.
- **Regressions:** CI fails. A regression is accepted only with explicit approval **by the user** — a `Perf-Approved: <user>` commit trailer together with the updated baseline and a `docs/perf-log.md` entry with the justification. AI agents must never add the trailer or edit baselines themselves; they stop and ask (see `CLAUDE.md`). This will be automated later.

**Consequences:** Performance cannot drift silently — the failure mode of Orca. Enforcement details: `0017-perf-approval-enforcement.md`.
- `flood50_cpu_ns_per_mib` is report-only (measured and printed, not gating) pending calibration of the self-hosted runner, which showed ~35% cross-session drift (user decision 2026-11-13).
- `fleetd_cold_start_ns` is report-only against its baseline (user decision 2026-11-13): on the self-hosted runner a ~2 ms process start ranged 2.14–2.85 ms across runs, far beyond the 5% tolerance. Its 200 ms budget still fails the gate, and `fleetd_startup_instructions` catches startup code regressions deterministically. Report-only metrics are exempt from baseline regressions only, never from their budget.
