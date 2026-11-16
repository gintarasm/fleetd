# 0075 — `fleet report` and the shared inferred status

**Status:** accepted · 2026-11-17 (issue #105, M3 workstream D)

**Builds on:** [0033](0033-satellite-milestone.md), [0034](0034-agent-communication.md), [0042](0042-agent-status.md) (the heuristic), [0072](0072-agent-identity-by-session.md) (identity, scope), the M3 spec §2.

## Context

Agents should say how they are doing instead of the GUI guessing from their last output line. The guess (#32) stays as the fallback, and the node needs it too (M3-E's `fleet wait --state waiting`, exit reports), so it must live outside `fleet-gui`.

## Decision

**Reporting.**
- `fleet report <working|waiting|done|error> ["message"]`, in `fleet-cli/src/report.rs`. Proto: `Request.report = 50` (`ReportStatus { state, message }`, answered by `Ack`), `AgentInfo.reported = 50` (`Reported { state, message, at_unix_ms }`, unset until the first report), `Event.reported = 50` (`AgentReported { agent_id, reported }`). The request names no agent: the node takes the caller's identity (0072), so "self only" holds by construction. The scope table's row for it is an explicit `None`.
- **The user is refused:** a connection that resolves to `Caller::User` (including a root shell in a GUI terminal, 0072 amendment) gets `forbidden: fleet report needs an agent identity …`, exit 3. It has no agent to report for; a usage error would hide that it is a permission matter.
- **A `kind=shell` agent cannot report** (an agent-spawned shell resolves to that agent as caller, which has a parent and can run `fleet`, but the shell is status-less, 0071): a plain error, exit 2.
- **Message:** at most 200 characters (counted as chars, not bytes), no control characters (it ends up in one-line views). The CLI checks the length first (exit 2); the node checks again. Empty is fine.
- **Exit codes:** 0 ok; 2 usage (clap), over-long message, or a node error; 3 forbidden.
- **Stored on the agent** (`agents.reported_state`, `reported_message`, `reported_unix_ms`, schema **v11**; NULL = never reported), so it survives a daemon restart and a handoff. A later report replaces the earlier one. Only a running agent can report.
- **Staleness.** A report holds until the agent reports again; nothing expires it. A reported status counts only **while the agent runs** (`fleet_proto::status::live_report`): an ended agent speaks through its exit (done/failed), not through what it last said. `fleet restart` makes a new agent, so it starts unreported.
- **Events are scoped** (0072 amendment): `AgentReported` goes to the user's connections, and to an agent's only for `D(a)` (`scope::forward_reported`, through `Scope::sees`). A lagging receiver skips reports; the next list carries the latest.

**Shared inferred status** lives in **`fleet_proto::status`**, not `fleet-client`: `fleet-node` depends on `fleet-proto` and not on `fleet-client`, and the node is the second user. Moved: `AgentStatus`, `looks_like_prompt`, `QUIET_MS`, `is_shell` and the shell label constants, plus `inferred_status(info, last_line, last_output_ms, rang, now, quiet_ms)`, a pure function. `fleet-gui::core::status` keeps its `AgentRow` wrapper (`agent_status`) and re-exports the rest, so GUI callers did not change. The existing tests moved with the code.
- **Two quiet periods.** The GUI keeps 3 s (`QUIET_MS`, behaviour unchanged). The spec's 30 s for the node is `WAITING_QUIET_MS`, passed as `quiet_ms` by the node's callers (M3-E). One heuristic, two thresholds.
- `ReportedState::name()/from_name()` and `live_report()` are shared with the CLI.

**GUI.** `agent_status` returns the reported status when there is a live report (working → Working, waiting → NeedsYou, done → Done, error → Failed), else the inferred one. So the badge, filters and notifications see reports too: a reported `waiting` counts as "needs you", a reported `done`/`error` raises the finished/failed notification (its body is the report's message). Dots: **solid** when reported **or** when the agent has exited (an exit is a fact, not an inference), **hollow** (a ring) for a running agent's inferred status. This replaces 0042's "only idle is a ring". Sidebar, pane header and graph card all use `dot_fill` (`core::graph::card_dot` is the one seam M3-B left, now filled). `AgentReported` updates the row at once.

**CLI.** `fleet ls` gains a `REPORTED` column (`-` when none; the message is in `--json`, as `reported`), `fleet tree` shows the reported state in place of `running` followed by the message in quotes (`reported` in `--json`, `status` carries the state). The `ls` column sits before `PROGRAM`, so scripts that split the row by position need `--json`; the three in-repo tests were updated.

**Left out:** `--question` / `--choice` (M3-G); using `WAITING_QUIET_MS` in the node (M3-E).

## Consequences

- Protocol stays additive (`PROTOCOL_VERSION` 2); an old GUI ignores the event and the field.
- Schema **v11** is provisional: M3-E (9) and M3-F (10) are in flight; the overseer renumbers at merge. The migration only adds three nullable columns.
- Perf: nothing is on the PTY output path; a report is one SQLite UPDATE and one broadcast. Not measured (PERF_CI off).
- A report that was true an hour ago still shows. If that proves misleading, expiring a `working` report after silence is a change in `live_report` alone.
