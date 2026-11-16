# 0041 — A switch to turn the perf CI jobs off

**Status:** accepted · 2026-11-16 (user decision)

**Amends:** [0014](0014-performance-gate.md): the perf gate can be paused, by the owner only, with a repository variable.

## Context

The owner is hitting the GitHub Actions minutes limit. Every pull-request push and every merge to `main` runs three GitHub-hosted jobs: `check` (format, clippy, tests; ~3–5 min), `perf-instr` (~5–6 min, the most expensive) and `perf-approval` (~2 min). `perf-wall` and `gui-macos` run on the self-hosted Mac and cost no minutes, but `perf-wall` also occupies that Mac for about a minute per run.

The next stretch of work is a batch of GUI-polish issues (#26) done by several agents in parallel, opening many PRs. On 2026-11-15 the wall numbers proved unreliable while agents shared the Mac anyway (load average up to ~200; `input_echo_flood50_ns` unstable by design, #59).

## Decision

- The three perf jobs (`perf-instr`, `perf-wall`, `perf-approval`) run only when the repository variable **`PERF_CI`** is not `off`:
  `if: vars.PERF_CI != 'off'` (and, for `perf-approval`, still only on pull requests).
- `check` and `gui-macos` always run: tests, clippy and format stay gated.
- **Only the owner sets or clears `PERF_CI`** (repository settings, or `gh variable set PERF_CI --body off` / `gh variable delete PERF_CI`). Agents never change it.
- While it is `off`:
  - agents don't run the wall or instr suites locally either; they run tests, clippy and format;
  - PRs say "perf not measured (PERF_CI off)";
  - nothing about the baselines changes: agents still never edit `bench/baselines.toml`, `docs/perf-log.md` or add `Perf-Approved` trailers (CLAUDE.md).

## Consequences

- **Regressions can land unmeasured.** When the batch is done and `PERF_CI` is cleared, the owner runs the wall suite on a quiet machine (and `perf-instr` runs on the next push to `main`). Any regression found then is treated like one found in a PR: shown to the owner with before/after numbers, and fixed or approved by the owner.
- `perf-approval` is paused too, so a baseline loosened while the switch is off would not be caught in CI. The rule against agents touching baselines covers it; the first run after clearing the switch would show any change.
- Turning perf back on needs no code change.
