# 0098 — Search every agent's output

**Status:** accepted · 2026-11-23 (issue #137)

**Builds on:** [0021](0021-agent-history.md) (the node's history), [0053](0053-gui-find-in-scrollback.md) (find in a pane), [0029](0029-gui-core-toolkit-free.md) (toolkit-free GUI core), [0072](0072-agent-identity-by-session.md) (scope), [0080](0080-palette-lists-every-action.md) (the palette lists every action).

**Changes:** [0060](0060-gui-sidebar-filter.md)'s ⌘⇧F for the sidebar filter, which moves to ⌥⌘F.

## Context

Find (⌘F) searches one pane. "Which agent hit that error, or touched that file?" meant opening every pane in turn. The node has every agent's history on disk (segment files, not SQLite as the issue says) and every running agent's screen; it is the only place that can search them all without shipping them to the client.

## Decision

### The request: `SearchOutput` → `SearchHits` (proto fields 90, the block 90–94)

- `SearchOutput { text, case_sensitive, limit, per_agent }`. The text is matched as **plain text**, one line at a time, case folded unless `case_sensitive`: the same `Needle` the GUI's find uses, moved from `fleet-gui/core/find.rs` to `fleet-term::find` so the node and the GUI fold case the same way. No regex.
- `SearchHits { hits, truncated }`; a hit is `{agent_id, line, text}`. `line` is numbered as the GUI's find numbers lines: a history line by its absolute number, screen row `r` as the history's end plus `r`. So a hit can be found again in a pane without translating anything. `text` is the line without styling, cut to 1 KiB.
- An empty text is an error. An old daemon answers "empty request"; the CLI shows that, the GUI says to restart fleetd.
- No `SCHEMA_VERSION` change and no `PROTOCOL_VERSION` bump: it is a new request, and no stored data changes.

### What is searched, in what order

- **Agents:** every agent the store lists, newest (highest id) first.
- **Per agent:** its screen bottom up (a running agent's grid; an exited agent's final screen, which the node keeps for the last 64 exits), then its history from the newest line back. Each agent's newest matches come first; that is what "which agent hit that error" wants.
- **One hit per line**, the line's first match. The GUI's find then shows every match on it.

### Cost on the node

- **Caps:** `limit` hits in all (0 means 200, at most 1000) and `per_agent` per agent (0 means 20), so one noisy agent cannot fill the list and push the others out. Reaching `limit` stops the search and sets `truncated`.
- **Byte budget:** a request reads at most 256 MiB of history and screen text (four agents' full 64 MiB histories). A rare text over a big fleet stops there with `truncated`, instead of reading gigabytes. Newest agents are read first, so what is left out is the oldest.
- **No lock held across the scan.** History is read one page at a time through `Node::read_history` (the pipeline lock for a running agent, the history handle cache for others), as `fleet history` reads it. Output keeps flowing while a search runs. It runs on the blocking pool.
- **Stripping, not parsing.** History lines are ANSI with SGR only (`fleet-term`'s `render_line`); `history_line_text` drops CSI sequences in one pass instead of running the VT parser over every line. An ASCII line folds bytewise.
- No index. An index (trigram or similar) would cost on every line written for a search that runs rarely; the caps keep a search bounded without it. Revisit if searches get slow on big fleets.

### Scope: the user only

- Agents get `forbidden:` (`fleetd/src/scope.rs`): the search reads every agent's output, and an agent may only read its descendants' (ADR 0072). Filtering hits by scope would be possible later; nothing needs it now. The agent skill does not mention `fleet search`.

### `fleet search <text>`

- Prints `ID:LINE:TEXT`, grep's shape, for scripting and tests; `--json` prints `{hits, truncated}`. `--case-sensitive`, `--limit N`, `--per-agent N`. Exit code 1 when nothing matched (as grep); a note on stderr when the result was truncated.

### GUI: Search All Agents… (⇧⌘F)

- **Opening it.** ⇧⌘F, the palette ("Search All Agents…", ADR 0080), and Edit ▸ Search All Agents…. The sidebar filter moves from ⌘⇧F to **⌥⌘F** ("Filter Agents" in the palette); ⇧⌘F is search across files in most editors, and the filter is still reached by typing on the sidebar.
- **The picker.** The palette's picker in a new mode (`PickerView::search`): the owner lists the rows for each query instead of the picker ranking them fuzzily. Rows are hit lines (the match shown, with a `…` lead when it lies far into the line, the matched characters bold), under a heading per agent (its name or program and folder, its folder, git branch and state, as the palette's agent rows show them). A line under the list says "Searching…", "No matches", or "First 200 matches; type more to narrow" when truncated.
- **Cost on the node, from the GUI.** Typing is **debounced 250 ms**, and **at most one search runs at a time**: a query typed while one runs is sent when it answers, and only the latest. No search for an empty query. The GUI asks for 200 hits, 20 per agent. Each search uses a connection of its own, as Save Output does (ADR 0086) — over ssh that costs a connect per search, which the debounce keeps rare.
- **Smart case.** The GUI's query is case sensitive only if it has an upper-case letter (`error` finds `Error`; `Error` does not find `error`). The CLI has `--case-sensitive` instead.
- **A pick.** Enter (or a click) closes the picker, shows the agent in the focused pane (as a sidebar click does) and opens that pane's find bar on the query, on the hit's line (`PaneModel::find_open_at`): the search picks the match on that line, or the next one below it, once it has scanned down to it, paging older history in as find already does (ADR 0053). A hit in history older than the pane's 100,000-line cap lands on the oldest match loaded.
- **Core.** Debounce and request sequencing (`Requests`), rows and headings (`hit_rows`), snippets and smart case are in `core/search_all.rs`; the target line is in `core/find.rs`. All gpui-free and unit-tested. `app/search_all_view.rs` only wires them to the picker.

## Consequences

- A search reads up to 256 MiB from disk; on a fleet with little history it reads far less and answers in milliseconds. Its reads are page-sized, so it shares the disk and the pipeline locks with running agents in small pieces.
- Nothing changes on any hot path: no index, nothing per line written, nothing while no one searches. The GUI's frame costs are untouched when the picker is closed; perf not measured (`PERF_CI` off).
- A hit can be stale by the time it is picked (the line scrolled further, the history was cleared): the pane's find then lands on the nearest match, or none.
- Follow-ups: a case toggle in the picker, regex, filtering hits by label, a scope-filtered search for agents.
