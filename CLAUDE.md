# fleetd

Agent fleet manager in Rust. Read `docs/plan.md` and `docs/decisions/` before making design changes; record new decisions as new numbered files in `docs/decisions/`.

## Performance rule (non-negotiable)

- Benchmarks in CI are required and budgets are hard (see `docs/decisions/0014-performance-gate.md`).
- If a change regresses any benchmark: **stop, show the before/after numbers, and ask the user for approval.**
- Never edit `bench/baselines.toml` to accept a regression, never add a `Perf-Approved:` trailer, and never write `docs/perf-log.md` entries on your own. Only the user approves regressions.
- Before finishing any change, run `fleet-perf run --suite wall` and `fleet-perf compare` (see `bench/README.md`); if anything regresses, stop and ask the user.
- This Mac is also the self-hosted `perf-wall` runner: do not run the wall suite locally while a `perf-wall` CI job is running (check `gh run list --workflow ci.yml --status in_progress`). Wall numbers from a local run are advisory; CI is authoritative.
