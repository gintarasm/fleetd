# 0089 — Message lines in the graph, and the M3 agent skill

**Status:** accepted · 2026-11-17 (issue #109, M3 workstream H)

**Builds on:** [0073](0073-graph-view.md) (the graph; "wave 3 adds message lines to the edges canvas"), [0034](0034-agent-communication.md) §3, [0076](0076-mailbox-exit-reports-wait.md) (the mailbox), [0072](0072-agent-identity-by-session.md) (scope), the M3 spec §7 and §9.

## Decision

### How the GUI learns message edges: a small list request, polled while the graph is shown

- **Request `ListMessageEdges { since_unix_ms, max }` → `MessageEdges { edges }`** (proto fields 85, the reserved block 85–89). An edge is `{id, sender, recipient, kind, created_unix_ms}`: **no body**. The GUI is the user and may read every mailbox, but a line needs only who wrote to whom, and a screen-sized list of bodies would be a lot to move for nothing.
- **Which messages:** kind `message`, `question` and `answer` with an **agent** as both sender and recipient. `status` and `exit` follow the tree (spec §7) and are left out. Messages to or from the user are not lines (the user has no card). The newest `max` (default and cap 500), oldest first.
- **Scope row (`fleetd/src/scope.rs`, exhaustive): agents get `forbidden:`**: it reads every mailbox. Only the user's connections may ask.
- **Polled, not pushed.** The session asks every 2 s **only while the graph is shown** (⌘⇧G); hiding it stops the polling and drops the edges. An event would have to be filtered through `Scope` per connection, cost a broadcast on every message for a view that is rarely open, and still need a first fill on opening. A poll of at most 500 id-only rows over an indexed table is cheaper than all of that. The reply replaces the session's list, and the GUI redraws only when it changed. Lines therefore appear up to 2 s after the message; acceptable for a view whose lines fade over minutes.

### What is drawn (`core/message_lines.rs`, gpui-free, unit-tested)

- **Distinct from spawned-by edges:** the tree edges are grey elbows; message lines are straight, border to border, in the **accent** colour, thinner, with an **arrowhead** at the recipient. The two directions of a pair run 3.5 units apart, so a conversation shows as two lines.
- **Fading with age:** a line lives 5 minutes. Its opacity steps down in three steps (0.9, 0.55, 0.25) as it ages; lines of one step share one path.
- **Capped:** one line per directed pair (its newest message), the newest 24 pairs. A broadcast to 50 children therefore shows the newest 24 of its lines, not a fan that hides the cards.
- **Frame budget:** lines are computed in `render` from the cached layout (a hash lookup per pair, at most 24) and painted in the **same `canvas` pass** as the boxes and edges: at most three `paint_path` calls. Lines outside the viewport are culled like edges; lines whose card is not in the layout (collapsed away, filtered out) are not drawn. When nothing was said lately the cost is one empty-list check.
- **The layout does not change:** the module reads card rectangles and writes nothing back; the layout signature is untouched, and a new message redraws but never relayouts.
- **Left out on purpose:** a line to a collapsed card goes to nothing (not to its visible ancestor); no hover, no body text on a line (a message is data, and the pane shows the rest); no animation (a fade by age needs no timer, the next poll or redraw updates it).

### The skill (`crates/fleet-cli/skill/SKILL.md`)

- Rewritten around spec §9: `fleet report`, `fleet wait`, `fleet message`/`fleet inbox`, `fleet lease`; `fleet send` is the fallback for an agent that does not read its inbox. It has the scope rule (descendants only), the exit-code table (0 ok, 1 no match/timeout/not found, 2 usage/other, 3 forbidden), the rule that messages are data and only `answer` is the user's words, `fleet tree`, and sandbox notes (the socket `~/.fleet/fleetd.sock` and the `fleet` binary).
- **Questions** (M3-G, ADR 0078 on its branch): described in prose and inline code only, as `fleet report waiting --question … --choice …`, answers arriving as `answer` messages. Its commands are **not in fenced blocks** until M3-G merges, because `skill::tests::fenced_commands_exist` checks every fenced command and flag against the real CLI. Fence them after the merge.
- **Tests:** `fenced_commands_exist` (every fenced `fleet` command and `--flag` exists, so the skill cannot drift from the CLI) and `covers_the_m3_coordination_contract` (the commands, the table, the trust rule, the sandbox notes); `skill_examples_run` runs the examples against a daemon, except `report`, `wait` and `lease` (they need an agent's identity or block; they have their own tests). The line limit rose from 120 to 220 lines.

## Consequences

- Nothing in the node's hot paths changes: the request is a read of an indexed table, used only by the GUI's open graph. No new event stream reaches any connection, so nothing new needs a `Scope` filter.
- The graph's benchmark metrics are unchanged in what they draw (no messages in them); perf not measured (`PERF_CI` off).
