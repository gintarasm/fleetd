# 0021 — Agent history: rendered lines, an in-memory tail and segments on disk

**Status:** accepted · 2026-11-13 (M2a; design in `docs/superpowers/specs/2026-11-13-m2-gui-design.md` §1)

**Amends:** [0005](0005-terminal-state-on-node.md). The node still keeps one headless grid per agent and reattach still costs one screen. It now also keeps, and serves, the lines that scrolled off that grid. The raw-output scrollback ring of [0018](0018-m1-protocol-and-node.md) is unchanged.

**Amended by [0030](0030-agent-restart-and-forget.md):** forgetting an agent deletes its history early; retention never prunes store rows.

## Context

M2's client must show a whole long session (a Claude Code conversation), also after a GUI restart or a re-attach. 0005 gave clients only the visible screen. Keeping alacritty's scrollback in node memory and shipping it with the snapshot was rejected: about 24 bytes per cell (a 10k x 120 session is ~29 MB, times 50 agents), attach cost that grows with history, and history that dies with the daemon.

## Decision

### Capture: render rows as they scroll off

- **Mechanism.** `fleet-term`'s `Terminal` parses through `Recorder`, a vte `Handler` wrapper around alacritty's `Term`, which delegates every call unchanged. alacritty runs with `scrolling_history: 0`, so the grid never holds history. Right before an operation would scroll rows into history (linefeed/IND/NEL on the bottom margin, autowrap of a glyph or tab on the bottom margin, `CSI n S`, `CSI n M` on row 0, `CSI 2 J` on the primary screen), the recorder renders those rows to ANSI lines. It uses the snapshot's SGR rendering, one line per row, with trailing blanks trimmed. Only alacritty's public API is used (0020): no patch, fork or vendoring.
- **Conditions.** Rows are captured only when the scroll region starts at row 0 and the primary screen is active, exactly when alacritty itself would keep scrollback. alacritty keeps the scroll region private, so the recorder mirrors it: DECSTBM, resize, RIS, and DECCOLM (`?3h`/`?3l`, which resets the region) all update the mirror.
- **Alt screen.** The alt screen never produces history, as in alacritty and real terminals.
- **Clear scrollback.** `CSI 3 J` reaches the handler as `clear_screen(ClearMode::Saved)` on the primary screen. It produces a `Cleared` event, and the store starts a new **generation**: lines from before the clear are no longer served. Claude Code clears and reprints the conversation on some redraws, so ignoring the clear would duplicate history. vte reads only the first parameter, so `CSI 3;1 J` and `CSI 03 J` also clear.
- **Resize.** Only `Terminal::resize` turns alacritty's scrollback on, briefly (10 000 rows), then renders the rows the shrink pushed off (oldest first) and empties and truncates the scrollback again. Lines keep the width they had when captured: history is never reflowed. On the alt screen the primary is settled at once through the existing `with_primary` (`swap_alt`) round trip, which sets the primary's saved cursor to its cursor: the same snapshot limit 0018 already records.
- The node drains these events after each feed under the pipeline lock and appends them to the agent's store.

### Store: one per agent (`crates/fleet-node/src/history.rs`)

- **Location:** `<state-dir>/history/<agent-id>/` (0700). Lines are numbered from 0 for the agent's life; a clear bumps the generation and keeps numbering.
- **In-memory tail:** the most recent lines, at most 256 KiB **and** 4096 lines, held in one contiguous byte ring. It is sent with the attach snapshot, so recent history appears instantly.
- **Segments:** `<first_line:020>.seg` files of `[u32 LE len][bytes]` records, rolled over at 4 MiB. The per-agent cap is 64 MiB, enforced by deleting whole oldest segments (`first_line` advances).
- **Index:** sparse, one byte offset per 64 lines per segment; a page read seeks to the nearest indexed record and skips at most 63.
- **Meta:** `[u64 LE generation][u64 LE first_line]`, written by tmp file plus rename.
- **Writes** go through a 64 KiB buffer, never fsync'd. Lines still in the buffer are served from memory. History is best effort; the live grid stays authoritative.
- **Reopen** scans the segments, truncates a torn trailing record, and rebuilds the tail from the last lines on disk.
- **Disk errors** degrade the store to tail-only: it rolls back to what is durable and stops writing, and a page read that reaches past the durable end is an error. It never serves mislabelled lines.
- **fds:** segment files are opened per flush and per read, not held, so a running agent keeps the three fds of 0018's budget.
- **Pages:** at most 4096 lines and 256 KiB per `ReadHistory` (protocol: [0022](0022-protocol-v2.md)).

### Retention

Running agents keep their history. Of the others, the 64 most recently created keep it (the `RETAINED_EXITED` rule of 0018), including agents that exited under a previous daemon; `Node::open` deletes the history directories of all other agents.

**Deviation from the spec** (which said "the 64 most recently exited"): the two passes use different orders. At startup `Node::open` keeps the 64 most recently *created* non-running agents (the store has no exit time to order by). At runtime, an agent that exits in this process joins the retained-exited list, and the oldest *exit of this process* is evicted (with its history) once the list holds 64. The startup survivors are not on that list, so they are only pruned at the next start: up to 64 + 64 exited histories can exist at once, and an agent that exited long ago but was created recently can outlive one that exited later. The disk use stays bounded (at most 128 exited histories of up to 64 MiB each, plus the running agents'); a single exit-ordered rule needs an exit timestamp in the store.

## Consequences

- Node memory per agent grows by at most the tail (256 KiB); attach sends tail plus screen, still independent of total history; history survives daemon and GUI restarts.
- Capture is on the output hot path. User-approved (2026-11-13): `terminal_feed_64k_instructions` 6.89 M → 11.04 M (+60%), most of it rendering the 400-odd rows per 64 KiB that scroll off; `snapshot_120x40_instructions` fell 48% from the shared rendering's fast paths. `reattach_snapshot_ns` and `daemon_flood50_peak_rss_bytes` grew with the tail (user-approved, report-only). New metrics: `terminal_feed_scroll_64k_instructions`, `history_push_1k_lines_instructions`, `history_read_page_instructions`.
- Known limits: no reflow of old lines on resize; a single line larger than a segment can exceed the disk cap or a page's byte cap; a crash can leave the next line number below one already served in the same generation; clear deletes segments before rewriting meta, so a crash between them can briefly mislabel `first_line`.
