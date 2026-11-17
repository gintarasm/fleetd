# M3-H plan: skill rewrite and message lines (issue #109, ADR 0089)

1. **Proto + node** (test first, `fleet-node/tests/mailbox.rs`): `ListMessageEdges`/`MessageEdges` (85); `Store::message_edges`, `Node::message_edges`.
2. **fleetd**: handler in `server.rs`; scope row (agents forbidden); wire test `tests/message_edges.rs`.
3. **GUI core** (`core/message_lines.rs`, tests first): fade steps, pair selection and cap, border-to-border routes with lanes and arrowheads, culling against the layout.
4. **GUI wiring**: session polls every 2 s while the graph is shown (`watch_message_edges`), `UiEvent::MessageEdgesChanged`, `AppState.message_edges`, one more path set in the graph's edge `canvas`.
5. **Skill**: rewrite; `skill::tests` (fenced commands exist, contract covered); adapt `skill_examples_run` and the line limit.
6. **Finish**: ADR 0089, full tests, clippy, fmt, PR. Perf not measured (`PERF_CI` off).
