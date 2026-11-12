# 0017 — Enforcing perf approval

**Status:** accepted · 2026-11-13

**Decision:**
- For now Claude opens PRs with the user's credentials, and the user reviews and merges manually.
- **Nothing blocks a merge on GitHub today.** This is a private repo on the Free plan, and GitHub refuses rulesets and branch protection for it (API 403: "Upgrade to GitHub Pro or make this repository public"). The only gate is the user's manual check, before merging, that `check`, `perf-instr`, `perf-wall` and `perf-approval` are all green. Upgrade path: GitHub Pro, then branch protection on `main` requiring a PR and those four status checks with **0 required approvals** (GitHub does not allow self-approval, so requiring one would block every PR).
- **Gate code needs the same approval.** `fleet-perf check-approval` also requires the `Perf-Approved` trailer (from a merge-base approver) and a `docs/perf-log.md` change when the branch modifies or deletes gate code that existed at the merge-base: `crates/fleet-perf/src/{baseline,approval,wall,instr}.rs`, `crates/fleet-perf/benches/instr.rs`, `.github/workflows/`. Otherwise the gate could be weakened instead of the baselines. Files created on the branch are exempt.
- `.github/CODEOWNERS` assigns `bench/baselines.toml`, `docs/perf-log.md`, `.github/workflows/`, `crates/fleet-perf/` and itself to `@gintarasm`. It is inert until a separate bot identity opens the PRs; then "require review from code owners" is enabled and the approval gate (0014) is enforced by GitHub instead of by manual review.

**Consequences:** Until then the `Perf-Approved` trailer check (`fleet-perf check-approval`), the user's manual all-green check before merging, and the user's own review are the only enforcement; agents must still never write the trailer (`CLAUDE.md`).
