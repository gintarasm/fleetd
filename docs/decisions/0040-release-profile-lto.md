# 0040 — Release profile: fat LTO, one codegen unit

**Status:** accepted · 2026-11-15 (issue #19)

## Context

The daemon-restart work ([0032](0032-daemon-restart-handoff.md), #16) raised `fleetd_idle_rss_bytes` about 4% (baseline raised to 5472256 with approval). The growth was resident `__TEXT` from code layout, not heap. Until now the workspace built releases with Cargo's defaults (thin local LTO, 16 codegen units).

## Decision

The workspace `Cargo.toml` sets `[profile.release]` to `lto = "fat"` and `codegen-units = 1`. The `bench` profile inherits it, so the `instr` suite measures the same code that ships.

`panic` stays `unwind`. The restart handoff (`crates/fleetd/src/restart.rs`) relies on `catch_unwind`, so `panic = "abort"` is ruled out. `crates/fleetd/tests/release_profile.rs` checks both settings.

## Consequences

- Smaller binaries and less resident text. On the M-series dev Mac, `fleetd` shrank from 6.86 to 5.19 MB, `fleet-gui` from 9.96 to 7.14 MB, and `fleet-perf` from 2.38 to 1.78 MB. The benchmark numbers are in the PR for #19.
- A clean `cargo build --release --workspace` takes longer in wall time: about 100 s before and 140 s after on that Mac (10 cores, load average under 6). It uses less CPU in total: 648 s of user time before, 529 s after. The single codegen unit serialises the last crates and the fat link. Incremental release rebuilds pay the whole-program link every time. Dev and test builds are unaffected.
- Instruction counts in the `instr` suite move, because inlining now crosses crate boundaries.
