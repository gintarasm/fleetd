# 0017 — Enforcing perf approval

**Status:** accepted · 2026-11-13

**Decision:**
- For now Claude opens PRs with the user's credentials, and the user reviews and merges manually. Branch protection on `main` requires a PR and the status checks `check`, `perf-instr`, `perf-wall`, `perf-approval` with **0 required approvals** (GitHub does not allow self-approval, so requiring one would block every PR).
- `.github/CODEOWNERS` assigns `bench/baselines.toml`, `docs/perf-log.md`, `.github/workflows/` and itself to `@gintarasm`. It is inert until a separate bot identity opens the PRs; then "require review from code owners" is enabled and the approval gate (0014) is enforced by GitHub instead of by manual review.

**Consequences:** Until then the `Perf-Approved` trailer check (`fleet-perf check-approval`) and the user's own review are the only enforcement; agents must still never write the trailer (`CLAUDE.md`).
