# M2 — gpui client (design)

Status: draft for approval · 2026-11-13

## Intent

The user will **daily-drive** fleetd: their Claude Code sessions run under fleetd, and the gpui client is the window they work in. The bar is "replaces the terminal tab I use today for Claude", not a demo. M2 delivers `docs/plan.md`'s milestone: sidebar, terminal pane, splits, menu-bar app. It must also make long sessions usable, which pulls in node-side history.

Today `claude` is the command the user runs, but this must be easy to change. The GUI has no Claude-specific code. The command is a config default and can be edited on each spawn. Agent-aware behaviour comes with adapters in M4.

## Goals

1. **Spawn, view, type into and kill agents from the GUI.** The default command is `claude`, from config.
2. **Terminal fidelity for Claude Code:**
   - Shift+Enter and the kitty keyboard protocol, Option-as-Meta, bracketed paste, IME;
   - answering terminal queries (the node leaves them to the attached client, per 0018);
   - mouse selection and copy, and scrolling.
3. **The whole conversation is scrollable,** including after a GUI restart or a re-attach. Recent history appears instantly; older history loads from disk.
4. **Sidebar** of all agents, plus **up to 4 split panes**.
5. **The app stays resident in the menu bar** when the window closes. Attachments stay alive, so reopening is instant.
6. **The 4 visible panes keep streaming during a 50-agent flood.** Panes recover by themselves after a lag.
7. **Benchmarks** for the plan's GUI budgets: cold start < 200 ms and input-to-render < 16 ms.
8. **A fleet skill for agents, installed with one command.** An agent (Claude Code first) learns from it how to work with the fleet through the `fleet` CLI: list agents, read another agent's screen or history, send it input, spawn one. `fleet skill install` puts it in place (§6a).

## Non-goals (later milestones)

- project/workspace/worktree hierarchy and the diff view (M3);
- status badges, notifications, adapters, the MCP channel (M4);
- remote nodes under a controller, bootstrap/upload of `fleetd`, viewing several daemons at once (M5). Connecting the GUI to *one* remote fleetd over SSH **is** in M2 (user request);
- credentials (M6);
- the command palette;
- themes beyond one dark and one light;
- tabs inside a pane;
- reflowing old history on resize.

## Anti-goals

- **A GUI that lags or drops keystrokes while agents flood.** That is the Orca failure this project exists to beat.
- **Any assumption that the daemon is local** (0019). All GUI↔daemon traffic goes through the protocol over a byte stream. The GUI never reads the node's files or SQLite directly.
- **Any GUI feature that makes the daemon depend on the GUI.** Agents keep running when the GUI crashes or quits.
- **Copying Zed's `terminal_view`/`terminal` code.** Those crates are GPL-3.0. gpui itself is Apache-2.0. Study them, write our own.

## Constraints

- Perf rules in `CLAUDE.md` and ADR 0014 apply. The history work touches the output hot path, so `terminal_feed_64k_instructions` and the `daemon_flood50_*`/`flood50_*` metrics may move. **Any regression is shown to the user with before/after numbers and needs their approval.** New metrics get baselines recorded from CI, as in M1.
- Pinned toolchain `1.99.1`. `gpui = "0.2.2"` (crates.io). Xcode's `metal` is available on the dev Mac and the `perf-wall` runner.
- CI's Linux jobs (`fmt`, `clippy`, `test`, `instr`) must stay green. `fleet-gui` is macOS-only, and its gpui deps are behind `cfg(target_os = "macos")` or the crate is excluded from Linux jobs.
- Node memory: the idle-RSS budget is unchanged (50 MB). History must not hold per-agent cell grids in memory.
- Crate layout changes and protocol changes are recorded as ADRs (CLAUDE.md).

## Approaches considered (history)

1. **Rendered lines on disk, plus an in-memory tail (chosen).** Lines that scroll off the node's grid are rendered to ANSI and appended to a per-agent history file. The most recent ones are also kept in a bounded in-memory tail, which is sent with the attach snapshot; older lines are served in pages from disk.
   - Pros: node memory stays O(screen + small tail); history survives GUI restarts; attach stays instant.
   - Cons: a new file format; extra work on the output path when lines scroll off; old lines keep their original width.
   - The user picked this and added the tail, so some history is available instantly.
2. **Keep alacritty scrollback in node memory and ship it with the snapshot.** Rejected: about 24 bytes per cell (a 10k × 120 session is ~29 MB, times 50 agents); attach cost grows with history; history dies with the daemon.
3. **Replay the raw output ring from M1.** Rejected:
   - Claude Code's constant redraws mean 8 MiB may not cover a conversation;
   - replay at a different width garbles it;
   - it reverses 0005 ("never a full log replay").

## Design

### 1. Node: history

- **Capture.** The node's `Terminal` (today `scrolling_history: 0`) gets a scrollback large enough to absorb one feed chunk. After each `feed`, rows that entered alacritty's history are rendered to ANSI lines (the same SGR rendering the snapshot uses, one line per row, with trailing blanks trimmed) and then cleared from the grid. The grid never holds history between feeds.
  - The builder chooses the exact mechanism, which must use alacritty's public API only (0020: no fork or patch). It must not silently drop lines when a single chunk pushes more rows than the scrollback holds; for example, split the feed.
- **Line numbering.** Each agent has a monotonically increasing absolute line number. A *clear generation* counter is bumped when the agent clears scrollback (`CSI 3 J`).
- **Clear-scrollback is honoured (my call).** It behaves like a real terminal: lines from before the clear are no longer served. Claude Code clears and reprints the conversation on some redraws, so ignoring the clear would duplicate history. The file may keep the bytes until rotation, but `ReadHistory` serves only the current generation.
- **In-memory tail.** The most recent rendered lines, capped at **256 KiB per agent** (my call, about 1–2k lines).
- **On disk.**
  - Location: `<state-dir>/history/<agent-id>/`, mode 0600, append-only, segmented so the **64 MiB per-agent cap** (my call) is enforced by dropping whole oldest segments.
  - Reads: a page read by line number must cost O(page). The builder chooses the indexing scheme.
  - Durability: appends are buffered and not fsynced, like the M1 ring. History is best effort; the live grid is authoritative.
- **Retention (my call):**
  - running agents keep their history;
  - exited agents keep it like `RETAINED_EXITED`, so the most recent 64 exited agents' history stays readable, including agents that exited under a previous daemon;
  - older ones are deleted.
- **Unchanged:** the M1 raw ring stays as it is (out of scope).
- **Alt screen:** lines never scroll into history while the alt screen is active, which matches alacritty and real terminals.

### 2. Protocol (bump to `PROTOCOL_VERSION = 2`; `fleet-cli` updated in the same change)

- **`AttachOk` gains:**
  - `history_tail` (ANSI lines);
  - `tail_first_line` (absolute number of the tail's first line);
  - `history_first_line` (oldest line still on disk);
  - `history_generation`.

  The tail, snapshot and subscription are taken under the same lock, so tail + snapshot + stream are contiguous.
- **New request** `ReadHistory { agent_id, generation, before_line, max_lines }` → `HistoryPage { first_line, lines }`. A stale generation is answered with an `Error`. Pages are capped (the builder chooses the cap, around 256 KiB).
- **Lag notice names the agent.** The `Response { id: 0, Error "lagged" }` notice is replaced by an `Event::Lagged { agent_id }`, so a pane can re-attach on its own.
- **Sidebar feed.** New `Event::AgentActivity { agent_id, last_line, last_output_unix_ms }`, pushed to every connection and throttled per agent (my call: at most ~4 Hz). The sidebar can then show each agent's last line without attaching to all 50 agents. A client that does not want it can ignore it. The builder decides whether it needs a subscribe flag.
- **Kept from M1:** existing semantics (ordering, drop policy, sizes) are unchanged.

### 3. Node: the flood fix

M1's `daemon_flood50` dropped 45–48 of 50 subscriptions because agents' parsing threads starve the connection forwarders (0018). M2 must at least guarantee **goal 6**: a client attached to 4 agents while 50 flood does not lag out. The builder finds the cause and the fix: forwarder priority, batching, or moving forwarding off the contended path. A new wall metric covers this (see §7). Improving the full 50-attached case is a stretch target; `daemon_flood50_lagged_subscriptions` stays report-only unless it reaches 0.

### 4. Crates

- **`fleet-client` (new):** the protocol client, extracted from `fleet-cli`'s `client.rs`, used by both the CLI and the GUI. It runs over any byte stream (unix socket today, `ssh … --stdio` later, per 0019).
- **`fleet-term` (new):** the `Terminal` wrapper (alacritty + snapshot + history-line rendering + clear detection), moved out of `fleet-node`, so node and GUI parse and render history the same way.
- **`fleet-gui` (new, macOS):** the app.
- These are recorded in an ADR amending 0016.

### 5. GUI

**Connection (user decision: the GUI asks; it never silently starts a daemon).**
- The GUI connects to **one daemon at a time**. This is 0019's one-controller model; seeing several daemons at once is M5's controller work.
- The connection target is saved in GUI config (`[gui] connection = "local"` or `"ssh://user@host"`, my call on the format).
- **On startup it tries the saved target.** If that fails, or if no target is saved and no local daemon answers, a connect sheet offers:
  - **Start fleetd on this Mac.** The GUI launches `fleetd` detached (`setsid`, so it outlives the GUI), looking next to its own binary first, then `PATH`, then waits for the socket. The one-daemon lock (0013) makes races harmless.
  - **Connect to a remote fleetd.** You enter `user@host`. The GUI runs `ssh user@host fleetd --stdio` and talks the protocol over its stdin/stdout. It uses the system `ssh`, so `~/.ssh/config`, keys and the agent all apply.
  - Recent targets are listed.
- **The menu-bar menu shows the current target** and has "Switch fleetd…", which reopens the sheet.
- **`fleetd --stdio` (new)** bridges stdio to that host's daemon socket. If no daemon is running there, it starts one detached, then bridges. M2 assumes fleetd is already installed on the remote host (`cargo install`); uploading it and running it under `systemd --user` is M5 bootstrap. The GUI shows a clear error when `fleetd` is not found remotely.
- **With a remote daemon**, paths in the New-agent sheet are paths on that host, so the native folder picker is hidden and recent directories come from that daemon's agent list.
- Everything else is the same: the GUI already assumes nothing about locality (anti-goal).
- **Disconnects:** when the daemon or the SSH link goes away, panes show "disconnected", the GUI reconnects to the same target with backoff, and it re-attaches the visible panes.
- **Threading:** protocol I/O runs off the UI thread. The builder chooses the threading.

**Agent environment (my call).** Agents inherit the daemon's environment, which can be bare, for example when the daemon is started from a GUI app or a non-interactive SSH session. So at startup `fleetd` captures the user's login-shell environment (`$SHELL -lic env`, with a timeout, falling back to its own env) and uses it as the base for every agent. `SpawnAgent.env` still overrides it. Then `claude` and the user's API keys and PATH work however the daemon was started.

**Packaging (my call).** A script builds a minimal `fleetd.app` bundle: Info.plist, icon, the `fleet-gui` binary, with `fleetd` and `fleet` alongside. It can be launched from the Dock or Spotlight. `cargo install` of `fleet-gui` also works from a terminal. A login item is later.

**Window layout.**
```
┌──────────────┬─────────────────────────────┬─────────────────────────────┐
│ + New agent  │ claude · ~/dev/fleetd   (1)  │ claude · ~/dev/api      (2) │
│              │                             │                             │
│ ~/dev/fleetd │  > refactor the node …      │  ● Reading src/main.rs      │
│ ● claude  2m │                             │                             │
│   "Edited…"  │                             │                             │
│ ~/dev/api    ├─────────────────────────────┴─────────────────────────────┤
│ ● claude  5s │ claude · ~/dev/fleetd/docs                          (3)   │
│ ○ claude  ✕0 │                                                           │
└──────────────┴───────────────────────────────────────────────────────────┘
```

**Sidebar.**
- Agents are grouped by working directory (a stand-in until M3's hierarchy).
- Each row shows the program, state (running / exited with code / lost), the last line from `AgentActivity`, and the time since last output.
- Clicking opens the agent in the focused pane. Exited agents are dimmed.
- Kill asks for confirmation. Closing a pane only detaches.

**New agent.**
- A sheet asks for the directory (a text field with recent directories and a native folder picker as a convenience; the path is a node path) and the command.
- The command is prefilled from `~/.fleet/config.toml` `[gui] default_command = ["claude"]` (my call on the key name). Program and args can be edited before spawning.
- The pane's size is used as the spawn size.

**Splits.**
- A split tree with at most 4 panes. My call on keys:
  - `cmd-D` split right, `cmd-shift-D` split down;
  - `cmd-W` close pane (detach);
  - `cmd-opt-arrows` move focus;
  - `cmd-1…4` focus a pane by number.
- Each pane has a header showing the agent and pane number.
- The layout and which agent is in which pane persist across GUI restarts (my call).

**Terminal pane.**
- **Model:** a client-side alacritty grid fed with the snapshot and then the stream, plus a GUI-owned **history line list** drawn above the live grid as one scrollable view. Lines that scroll off the client grid move into the list, the same way they do on the node.
- **History on attach:**
  1. feed the tail into the list, then the snapshot into the grid, then the stream;
  2. then fetch older pages with `ReadHistory`, in the background and when scrolling nears the top, prepending them.
- **Clear-scrollback:** the GUI honours `CSI 3 J` the same way the node does.
- **Rendering:** our own gpui element. It draws cells with batched text runs, the cursor and selection, and redraws at most once per frame, however much output arrived.
- **Input:**
  - gpui keystrokes are encoded to bytes: xterm encoding by default, and kitty-protocol encoding when the agent has enabled it (the client grid tracks the mode from the stream);
  - Option-as-Meta, on by default (my call);
  - bracketed paste when the mode is on;
  - IME through gpui's input handler;
  - query replies from the client grid sent back as `SendInput`.
- **Mouse:** mouse reporting is forwarded when the agent enables it; otherwise the GUI handles selection (across history and grid), copy with `cmd-C`, and wheel/trackpad scrolling.
- **Resizing:** debounced, then a `Resize` to the node.
- **Lag:** `Event::Lagged` triggers an automatic re-attach of that pane.
- **Exit:** the pane shows the final screen with an "exited (code)" banner.

**Fonts and theme.** A monospace font from config (default: Menlo 13), one dark and one light theme following the system appearance.

**Menu-bar app.**
- An `NSStatusItem` is always present.
- Closing the last window keeps the process and its attachments alive, and the Dock icon hides (accessory activation policy).
- The menu has:
  - "Show fleetd";
  - running agents (click to show in the focused pane);
  - "New agent…";
  - "Quit" (quits the GUI only; agents keep running).
- If gpui 0.2.2 has no status-item API, the builder calls AppKit directly (`objc2`).
- Notifications are M4.

### 6. CLI

`fleet attach` keeps working. The CLI moves to `fleet-client` and protocol v2. A `fleet history <id>` subcommand that prints the agent's history is in scope (my call; it is cheap and useful for testing).

### 6a. Fleet skill for agents

**Why.** The user wants agents to work with the fleet. The structured agent channel (MCP, per-agent tokens, quotas; 0019) is M4. Until then the skill teaches agents to use the `fleet` CLI, and M4 updates the skill to the MCP tools without changing how it is installed.

**What the skill covers.** A `SKILL.md` (Claude Code skill format: frontmatter `name: fleet` and a `description` that triggers on "other agents", "the fleet", "spawn an agent" and the like) teaches:
- `fleet ls` (with a new `--json` flag, my call, so agents can parse it);
- `fleet attach <id> --snapshot` to read another agent's current screen;
- `fleet history <id> [--lines N]` to read its scrollback (§6);
- `fleet send <id> "text" --enter` to give it input;
- `fleet spawn --cwd DIR -- claude …` to start one;
- `fleet kill <id>`.

It also tells the agent who it is:
- `fleetd` sets `FLEET_AGENT_ID` in every spawned agent's environment (my call);
- the agent should not send input to itself;
- it should spawn or kill agents only when the user asked, since quotas arrive with M4 (my call).

**Install.**
- The skill text is embedded in the `fleet` binary, so it always matches the installed CLI.
- `fleet skill install` writes it to `~/.claude/skills/fleet/SKILL.md`.
- `--project [DIR]` writes it to `DIR/.claude/skills/fleet/` instead.
- Re-running it overwrites the file (upgrades), and the command prints the path.
- `fleet skill uninstall` removes it, and `fleet skill print` writes it to stdout for other agents.
- Other agent CLIs' formats (Codex `AGENTS.md`, Gemini) are left for M4 adapters; `--target` is reserved for that.
- The GUI's menu also offers "Install agent skill" (my call).

### 7. Benchmarks

New metrics, with baselines recorded from CI like M1:

- `gui_cold_start_ns` (wall, budget 200 ms): from process launch to the first frame showing the populated sidebar, through a bench hook in the GUI (for example, an env var that makes it report and exit).
- `input_to_render_ns` (wall, budget 16 ms): from a synthetic key event in the GUI to the frame that shows the echoed character (agent: `cat`).
- `flood50_attached4_lagged_subscriptions` (wall, gated at 0): 50 flooding agents, one client attached to 4.
- `history_*` instr metrics: append cost per line, and page read.
- Existing `terminal_feed_64k_instructions` and the flood metrics measure the capture overhead. Regressions go to the user.

**Risk:** the GUI wall metrics need a window server. The `perf-wall` runner must run in a logged-in GUI session; if it can't, the builder reports back before going further.

## Left to the builder

- the history capture mechanism and on-disk format (within the constraints above);
- the cause and the fix for flood starvation;
- GUI threading and the gpui element design;
- the kitty and xterm key-encoding details;
- page sizes;
- reconnect backoff;
- how the bench hooks work;
- the order of milestones within M2.

## Decided after the builder check (my calls)

- **Input-to-render holds under flood.** The 16 ms budget also holds while 50 agents flood. `input_to_render_ns` gets a flood variant (`input_to_render_flood50_ns`, budget 16 ms), and the §3 fix must give priority to input and to attached agents' output.
- **Kitty keyboard detection for GUI spawns.** `SpawnAgent` gains `attach: bool`. The GUI spawns and attaches atomically, so the GUI's terminal answers Claude Code's startup queries, kitty `CSI ? u` included. Agents spawned unattached (CLI, skill) still get no kitty reply. The builder checks empirically how Claude Code behaves then, and reports before changing 0018's policy (having the node claim kitty support).
- **Primary client.** When several clients attach to one agent (GUI plus `fleet attach`, or two GUIs), the client that most recently attached or sent keystroke input is primary. Only the primary's `Resize` applies and only it answers terminal queries. The node tells clients their role with `Event::AttachRole { agent_id, primary }`. Observers don't answer queries and render at the primary's size. This is part of protocol v2.

## ADRs to write

- **History:** rendered lines plus tail, line numbering, clear generations, retention. Amends 0005.
- **Protocol v2:** `ReadHistory`, the `AttachOk` fields, `Lagged`, `AgentActivity`. Amends 0018.
- **Crates:** `fleet-client`, `fleet-term`. Amends 0016.
- **GUI:** resident menu-bar behaviour, the connect sheet (local start or remote over SSH), `fleetd --stdio`, login-shell env capture, config keys, primary client.
- **Agent skill:** CLI-based until M4, embedded in the binary, how it is installed, `FLEET_AGENT_ID`.
