# fleetd

Agent fleet manager in Rust. Read `docs/plan.md` and `docs/decisions/` before making design changes; record new decisions as new numbered files in `docs/decisions/`.

## Performance rule (non-negotiable)

- Benchmarks in CI are required and budgets are hard (see `docs/decisions/0014-performance-gate.md`).
- If a change regresses any benchmark: **stop, show the before/after numbers, and ask the user for approval.**
- Never edit `bench/baselines.toml` to accept a regression, never add a `Perf-Approved:` trailer, and never write `docs/perf-log.md` entries on your own. Only the user approves regressions.
