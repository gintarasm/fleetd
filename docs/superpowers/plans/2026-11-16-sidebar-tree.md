# M3-C: sidebar tree and `fleet tree` — plan

Spec §6, §8; ADR 0074.

1. `fleet-client::tree` (test-first): parent from label, forest with order, loops/self/missing parent → roots, ancestors/descendants, pre-order walk.
2. `fleet tree` in `fleet-cli/src/tree.rs`: selection (ended rule, `--label` keeps ancestors), box-drawing text, JSON; unit tests plus one integration test against fleetd.
3. GUI core `sidebar_tree.rs`: `arrange` (nesting, disclosure, filter keeps ancestors), `removable_ended`; wire into `sidebar_groups`.
4. GUI app: disclosure state, chevron and indent in the sidebar row, Clear exited / Remove Exited / titlebar count use `removable_ended`.
5. ADR 0074; fmt, clippy, full tests. Perf not measured (PERF_CI off).
