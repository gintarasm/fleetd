<p align="center">
  <img src="assets/logo/fleetd-wordmark.svg" alt="fleetd" width="360">
</p>

# fleetd

A fast agent fleet manager written in Rust. It runs CLI coding agents (Claude Code, Codex, Gemini CLI, …) in terminals that survive client crashes, on your machine and, later, on remote hosts. The design is in [`docs/plan.md`](docs/plan.md) and the decisions behind it are in [`docs/decisions/`](docs/decisions/).

**Status:** M0 through M3 are done: the local node, the macOS GUI and its daily-drive polish, and Satellite (agents spawning agents, `fleet tree`, the graph view, reports, mailbox, leases and questions). M4 (workflows: git worktrees and a diff view) is next. Today you get a daemon (`fleetd`), a command-line client (`fleet`) and a macOS app (`fleetd.app`). Anything marked **TBD** below is planned but not implemented yet.

| Milestone | What | Status |
|---|---|---|
| M0 | Benchmark harness and CI perf gate | done |
| M1 | Local node: PTYs, terminal state, scrollback, SQLite, protocol, `fleet` CLI | done |
| M2a | Daemon side of the GUI: agent history, protocol v2, labels, flood fix, `fleetd --stdio`, login-shell env, agent skill | done |
| M2b | GUI (gpui): sidebar, terminal panes, splits, menu-bar app, connect sheet | done |
| M2c | Daily-drive polish: native-feeling GUI, restart and remove exited agents, `fleet daemon restart` | done |
| M2d | GUI polish: new terminal, command palette, workspace tabs, find, ended-pane summary, motion | done |
| M3 | Satellite: agents spawning agents through the `fleet` CLI, spawn quotas and approval, `fleet tree`, graph view, reports, mailbox, leases, questions | done |
| M4 | Projects, workspaces, worktrees, tasks; diff view | TBD |
| M5 | Plugins, Claude Code and Codex adapters, hook-based status detection, notifications | TBD |
| M6 | Remote nodes over SSH | TBD |
| M7 | Credentials | TBD |

## Install

Requires the Rust toolchain pinned in `rust-toolchain.toml`; `rustup` installs it automatically. Runs on macOS and Linux.

```sh
scripts/install.sh
```

It builds, installs, and moves a running `fleetd` onto the new build with its agents kept (`fleet daemon restart`). Updating is `git pull && scripts/install.sh`. Each step prints one line.

- **macOS:** builds the app bundle (needs the Metal toolchain, see below), copies it to `/Applications/fleetd.app`, and links `fleetd` and `fleet` in `~/.local/bin` to the binaries inside it. It stops rather than replace an existing `fleetd` or `fleet` file there that is not ours.
- **Linux:** builds and copies `fleetd` and `fleet` into `~/.local/bin`. There is no app.
- **Flags:** `--app-dir <dir>` (default `/Applications`), `--bin-dir <dir>` (default `~/.local/bin`), `--state-dir <dir>` (default `~/.fleet`, the daemon to restart), `--force-stop`. Relative directories are taken from where you run the script. A `CARGO_TARGET_DIR` you have set is where it builds and installs from.
- **A daemon too old to restart itself** (built before ADR 0032) is not touched: the script says so and exits 1. Run again with `--force-stop` to stop it (its running agents end). Needed once; see **Updating fleetd**.
- **No daemon running:** nothing to restart; the next client starts the new one. If the daemon's socket exists but does not answer, the script says so, leaves it and exits 1.

Or, with cargo, install both binaries into `~/.cargo/bin`, which rustup normally puts on your `PATH` (this does not restart a running daemon; run `fleet daemon restart --binary ~/.cargo/bin/fleetd` afterwards):

```sh
cargo install --locked --path crates/fleetd      # installs fleetd
cargo install --locked --path crates/fleet-cli   # installs fleet
which fleetd fleet                               # check they are on PATH
```

- **Not on `PATH`?** Add `export PATH="$HOME/.cargo/bin:$PATH"` to your shell profile.
- **Updating:** after pulling, run the same commands again. Add `--force` if cargo says that version is already installed.
- **Another location:** add `--root <prefix>` to install into `<prefix>/bin`.
- **Removing:** `cargo uninstall fleetd fleet-cli`.

Or build without installing: `cargo build --release -p fleetd -p fleet-cli` puts them in `target/release/`.

## Running the daemon

```sh
fleetd                      # state in ~/.fleet
fleetd --state-dir /path    # alternative state directory
fleetd --upgrade            # restart a running daemon of another build onto this binary
fleetd --force-stop         # stop a running daemon too old to restart itself, then start
fleetd --stdio              # bridge stdin/stdout to the daemon, starting one if needed
```

- **Foreground process.** `fleetd` runs in the foreground and serves clients on `<state-dir>/fleetd.sock`. To keep it running, start it from a terminal multiplexer or a service manager (a launchd/systemd setup is TBD).
- **One instance per state directory.** A second `fleetd` on the same directory exits with `fleetd already running` (exit code 2). If the running daemon is a different build, `fleetd` offers to restart it onto this binary (agents keep running); see **Updating fleetd**.
- **Stopping.** SIGTERM, SIGINT (Ctrl-C) or SIGHUP stops every agent: each gets SIGTERM, then 2 s later SIGKILL. Their exits are recorded, then the socket is removed.
- **Crashes.** If `fleetd` is killed hard, the agents it was running are shown as `lost` on the next start.
- **`--stdio`.** `fleetd --stdio` connects its stdin and stdout to the daemon's socket, so any client that can run a command can talk to it. If no daemon is running it starts one in the background, which keeps running after the bridge exits; concurrent bridges still end up with exactly one daemon. This is how a remote daemon is reached: `ssh host fleetd --stdio` (use the full path, such as `~/.cargo/bin/fleetd`, if it is not on the `PATH` of non-interactive ssh sessions; remote nodes in the client are M6).
- **Agent environment.** At startup the daemon asks your login shell (`$SHELL -lic`) for its environment, so agents see the same `PATH` and exports as a terminal would, even when `fleetd` was started by `ssh`, launchd or systemd. The capture runs in the background and gives up after 5 s; if it fails, agents get the daemon's own environment. Set `FLEETD_LOGIN_ENV=off` in the daemon's environment to skip it. Every agent also gets `TERM=xterm-256color`, `FLEET_AGENT_ID=<its id>` and `FLEET_AGENT_TOKEN` (a secret that proves who it is to `fleetd`; see [Agents that spawn agents](#agents-that-spawn-agents)).

The state directory (mode 0700) holds:

| Path | What |
|---|---|
| `fleetd.sock` | client socket |
| `fleetd.lock` | single-instance lock |
| `state.db` | agent records (SQLite) |
| `scrollback/` | per-agent raw output (8 MiB ring file each) |
| `history/` | per-agent history: the lines that scrolled off the screen (up to 64 MiB each) |
| `logs/` | the daemon's log `fleetd.log` (rotated at 10 MiB into `fleetd.log.1` and `.2`, 30 MiB at most) and the `--stdio` bridges' `fleetd-stdio.log` |

### Updating fleetd

A running daemon keeps the binary it started from. Restarting it onto a new build keeps every agent running (same process, id, terminal, screen and history; no output lost; clients reconnect on their own within about a second). Design: [0032](docs/decisions/0032-daemon-restart-handoff.md).

- **`git pull && scripts/install.sh`** builds, installs and restarts the daemon in one step.
- **`fleet daemon restart [--binary <path>]`** restarts the running daemon onto `<path>` (a relative path is taken from where you run `fleet`). Without `--binary` it restarts onto the binary it runs from, as that file is now: after a rebuild or reinstall at the same path, the new build. It prints `fleetd restarted: N agents kept` and exits 0. If some agents could not be restored it prints `fleetd restarted: K kept, M could not be restored and were ended (ids …)` and exits 1. If the restart was refused it prints `fleetd restart failed: <reason>; the old fleetd is still running` and exits 1. A refusal costs nothing: the new binary is checked first (`fleetd --handoff-check`), and a daemon that cannot freeze its agents in 3 s, or whose `exec` fails, resumes as before.
- **GUI:** the status-menu item **Restart fleetd** (also for an SSH target, which restarts the remote daemon onto its own binary, as now on disk). When the local daemon runs a different binary from the one the GUI would start, the sidebar shows "fleetd update available" with a Restart action.
- **`fleetd --upgrade`** does the same from the new binary when another fleetd holds the lock and runs a different build; in a terminal without the flag, `fleetd` asks first. Without a terminal or flag it only says so and exits 2. It needs `fleet` next to `fleetd` or on `PATH` (true for the app bundle, `install.sh` and cargo's `target/` directories).
- **One-time stop of an old daemon.** A daemon built before ADR 0032 cannot restart itself (`fleet daemon restart` says so). Stopping it ends its agents, once: `scripts/install.sh --force-stop` or `fleetd --force-stop` (SIGTERM to the pid holding the lock, found with `lsof` or `fuser`; never SIGKILL: if it does not exit within 5 s `fleetd` names the pid and exits 1), after which the next client starts the new daemon. Do not use `pkill` from a sandboxed shell: it silently drops signals to processes outside the sandbox.
- **What a restart loses:** the screen is restored as a reattach restores it (not scroll regions, character sets, tab stops, hyperlinks), and the final screens of agents that already exited are not carried over. A crash or `kill -9` still ends agents.

### Logs

The daemon logs to `<state-dir>/logs/fleetd.log` (`~/.fleet/logs/fleetd.log` by default), never to stdout, so a daemon started by `--stdio` or a service manager can still be diagnosed. Each line has a UTC timestamp, the level, where it came from, a message and `key=value` fields. Logged: start and stop, connections, spawns, exits, kills, attaches, lag drops, history problems and the login-env capture result. Never logged: environment values, agent arguments, terminal output, input and label values.

- **`FLEETD_LOG`** sets the level (default `info`), for example `FLEETD_LOG=debug fleetd`. It takes [`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) directives, such as `fleetd=debug,fleet_node=info`.
- **`fleet logs`** prints the log over the protocol, so it works for a daemon on another host too (see below).
- Logging never slows down agents: when the disk cannot keep up, lines are dropped and the log says how many. Design: [0027](docs/decisions/0027-logging.md).

## CLI: `fleet`

Every command talks to a running `fleetd`. `--state-dir <path>` selects a daemon other than the one in `~/.fleet`. `fleet --help` and `fleet <command> --help` show all options.

### Start an agent

```sh
fleet spawn claude
fleet spawn -- codex --model o4-mini
fleet spawn --cols 120 --rows 40 bash -lc 'npm test'
fleet spawn --cwd ~/src/app --label project=app --label role=reviewer -- claude
```

`fleet spawn` prints the new agent's ID.
- **Working directory:** the agent starts in your current directory, or in `--cwd DIR` (a relative `DIR` is taken from your current directory).
- **Labels:** `--label k=v` (repeatable) attaches labels to the agent; see [Labels](#labels).
- **Name:** `--name NAME` sets the agent's `name` label (default: generated from the command). Other commands take an id or a name.
- **Priority:** `--priority normal|low` (`low` always lowers the CPU priority; default: the daemon's `FLEETD_AGENT_NICE`).
- **Terminal size:** defaults to your terminal's size, or use `--cols`/`--rows`.
- **`TERM`:** set to `xterm-256color` for the agent.
- **Size limits:** 1–1000 columns or rows each, and at most 250,000 cells in total.

### List agents

```sh
$ fleet ls
ID  NAME            STATE    EXIT  REPORTED  PROGRAM
1   a               running  -     -         claude
2   readmem3-stoat  exited   0     -         bash -lc npm test
```

`REPORTED` is the state the agent last gave with `fleet report` (`-` if none); see [Agents that spawn agents](#agents-that-spawn-agents).

States:
- `running`: the agent is running.
- `exited`: the agent finished, with its exit code shown.
- `lost`: the agent was running when `fleetd` last died.

`fleetd` keeps the 64 most recent exited agents in memory. Older ones still appear in `fleet ls`.

```sh
fleet ls --label project=app      # only agents with every given label
fleet ls --json                   # JSON array: id, state, exit_code, program, args, cwd, created_unix_ms, labels
```

### Labels

Labels are free-form `key=value` pairs that `fleetd` stores and filters on without interpreting them, for example `project=fleetd` or `role=reviewer`. Set them at spawn (`--label`) or later:

```sh
fleet label 1 project=fleetd role=reviewer   # set
fleet label 1 --rm role                      # remove
```

At most 64 labels per agent, keys up to 64 bytes, values up to 256 bytes. Keys starting with `fleet.` are reserved. Labels of exited agents can still be changed.

### Read an agent's screen and history

```sh
fleet screen 1                    # the visible text of its current screen
fleet history 1                   # everything that scrolled off, oldest first, then the current screen
fleet history 1 --lines 50        # only the last 50 lines
fleet history 1 --plain           # without colours and other styling
```

- **History** is kept per agent on disk (up to 64 MiB), for running agents and the 64 most recently created others, and survives `fleetd` restarts.
- **Clearing:** when an agent clears its scrollback (`CSI 3 J`, as `clear` does), the lines before the clear are no longer served.
- Full-screen apps (the alternate screen) never add to history, as in a normal terminal. Lines keep the width they had; they are not reflowed after a resize.

### Attach to an agent

```sh
fleet attach 1
```

Opens the agent's terminal in your terminal.
- **Screen:** you immediately see its current screen (a snapshot), then live output. Input goes to the agent, and your terminal size is passed to it.
- **Detach:** press **Ctrl-\\**. The agent keeps running. Your terminal is restored on every way out: detach, agent exit, a signal sent to the `fleet` process (SIGINT/SIGTERM/SIGHUP), or an error. While attached, Ctrl-C goes to the agent, not to `fleet`.
- **Many attachments:** several clients can attach to the same agent at once. The one that attached or typed most recently is *primary*: only its terminal size is applied to the agent.
- **Falling behind:** if your client can't keep up with the agent's output, `fleetd` drops the stream rather than slowing the agent. `fleet attach` then exits with code 2 and `output lagged behind; re-attach`. Run it again to get a fresh snapshot.
- **Detach key in kitty keyboard mode:** Ctrl-\\ also detaches when an agent has turned on the kitty keyboard protocol or xterm's modifyOtherKeys, which encode the key as an escape sequence. Side effect: a lone Esc keypress reaches the agent up to 20 ms late. On keyboard layouts where `\` isn't a base key, only plain Ctrl-\\ (byte 0x1c) works.
- **Full-screen apps:** if an agent is in a full-screen app (alternate screen), the snapshot also carries the screen behind it, so leaving the app shows the right content.
- **Old exited agents:** attaching to an exited agent shows its final screen. Agents older than the 64 kept in memory show an empty screen.

### Send input without attaching

```sh
fleet send 1 "run the tests"           # types the text
fleet send 1 "run the tests" --enter   # types it and presses Enter
```

In the GUI, **Send to Agents…** (`cmd-shift-m`, ⌘P or the fleet menu) types the same text into one or many running agents, with Return after it unless unchecked. It warns first if a target is working. ⌘-click or ⇧-click sidebar tabs to pick several, then right-click one for **Send to Selected…**.

### Stop an agent

```sh
fleet kill 1
```

Sends SIGHUP, then SIGTERM, to the agent's process group. If it is still running 2 s later it gets SIGKILL ([0071](docs/decisions/0071-kill-escalates-and-shells-are-not-agents.md)).

### Restart and remove agents

```sh
fleet restart 1             # same program, args, cwd and labels, new id; the old agent is removed
fleet restart 1 --attach    # and attach to the new one
fleet rm 1 2                # forget exited or lost agents: list entry, history, scrollback
fleet rm --exited           # forget every exited or lost agent; prints the removed ids
fleet rm --exited --label project=fleetd --label role=coder
```

- `fleet rm --exited` never touches a running agent; `--label k=v` (repeatable, as in `fleet ls`) keeps only agents with every pair. In the GUI, **Remove Exited Agents…** (⌘P or the fleet menu) does the same after asking, with the count.

- Only exited or lost agents can be restarted or removed; running ones are refused. `fleet rm` exits non-zero if any id was refused.
- The environment given at spawn is not stored, so a restarted agent gets the daemon's base environment, not the original `--env`.
- Ids are never reused.
- An older daemon answers "the daemon is too old to restart/remove agents".

### Let agents use the fleet

```sh
fleet skill install                  # writes ~/.claude/skills/fleet/SKILL.md (Claude Code)
fleet skill install --agent omp      # writes ~/.omp/agent/skills/fleet/SKILL.md (omp)
fleet skill install --agent detected # both, for whichever of ~/.claude and ~/.omp exist
fleet skill install --project        # writes ./.claude/skills/fleet/SKILL.md (or --project DIR)
fleet skill uninstall                # removes it (takes the same --agent and --project)
fleet skill print                    # prints the skill
```

The skill teaches an agent (Claude Code or omp) to use `fleet`: list agents (`ls --json`), read another agent's `screen` or `history`, `spawn` or `kill` agents when you ask it to, and coordinate with `report`, `wait`, `message`/`inbox` and `lease` (see [Agents that spawn agents](#agents-that-spawn-agents)); `send` is the fallback for an agent that does not read its inbox. Each agent knows its own id from `$FLEET_AGENT_ID`. The skill is built into the `fleet` binary, so re-running `install` after an upgrade updates it. It does not need a running daemon. `--agent` defaults to `claude`. `--project` is Claude Code only: omp has no known project skill folder, so `--agent omp --project` is an error. The GUI's "Install agent skill" runs `--agent detected` on its target and says which agents it installed for.

### Agents that spawn agents

An agent that runs `fleet` is known to `fleetd` by its session (the process tree under the agent), then by `FLEET_AGENT_TOKEN`. Anything else, such as your own terminal or a GUI terminal, is **you**. Design: [0072](docs/decisions/0072-agent-identity-by-session.md), [0033](docs/decisions/0033-satellite-milestone.md), [0034](docs/decisions/0034-agent-communication.md); the spec is [`docs/superpowers/specs/2026-11-16-m3-satellite-design.md`](docs/superpowers/specs/2026-11-16-m3-satellite-design.md).

- **`fleet.parent`:** an agent's spawn gets the reserved label `fleet.parent=<its id>`. That is how the tree is built. A child keeps running when its parent ends. A parent that is gone (or not visible to the caller) makes the agent a root. Agents started before M3 are roots.
- **Scope:** an agent may act only on its **descendants** (children, their children, … including ended ones). `ls` and `tree` list only those, with no error. `screen`, `history`, `attach`, `send`, `kill`, `restart` and `rm` on any other agent are refused. `label`, `rename`, `logs` and `daemon` are refused for agents. `spawn` is allowed: the child gets the parent's labels (not `name`) plus new keys; changing a key the parent has is refused. You are never limited.
- **Refusals** are errors starting `forbidden:` and exit code **3**.
- **Not a sandbox.** A process of the same OS user that deliberately detaches from the agent's session and drops its token counts as you. For real isolation use another user or a container. An agent in a sandbox needs access to `~/.fleet/fleetd.sock` and the `fleet` binary.

#### Quotas and spawn approval

`~/.fleet/config.toml`, table `[node]`, read when the daemon starts (changing it needs `fleet daemon restart`). They apply to **agents' spawns only**; your own spawns are never refused or held.

| Key | Default | |
|---|---|---|
| `max_depth` | `3` | how deep the tree may go (a root is 0, so an agent at depth 3 cannot spawn) |
| `max_children` | `8` | running children per agent |
| `max_agents` | `30` | running agents on the node, yours included |
| `spawn_approval` | `false` | an agent's `fleet spawn` waits until you approve it |
| `spawn_approval_timeout` | `300` | seconds before a waiting spawn is refused (at least 1) |

A spawn over a quota fails at once with `forbidden: quota …` (exit 3); the graph shows it on the parent's card for a few seconds. With `spawn_approval = true` the quotas are checked first, then the spawn waits, and the asking `fleet spawn` blocks and prints nothing until you answer. An agent may have 8 spawns waiting. A restart by an agent does not ask again. Pending spawns are kept in memory only: `fleet daemon restart` refuses them (the agent can ask again).

```sh
fleet pending            # who asks, the command, time left, cwd, labels; --json for a JSON array
fleet approve 3          # start the pending spawn 3
fleet deny 3             # refuse it: the agent's spawn fails with "forbidden: spawn not approved"
```

The id is the pending spawn's own number (from `fleet pending`), not an agent id. Exit codes: 0, 1 no such pending spawn, 3 run inside an agent (only you decide). The GUI shows them as cards at the top of the sidebar (see [Agent tree and graph](#agent-tree-and-graph)). With approval off, a new child is only highlighted for a moment. Design: [0079](docs/decisions/0079-spawn-approval.md).

#### See the tree

```sh
$ fleet tree
1 planner  claude  running  12m
├─ 4 impl-api  claude  working "writing handlers"  8m
└─ 5 impl-ui  claude  waiting "need a design call"  8m
```

| Flag | |
|---|---|
| `--all` | include ended agents (default: ended ones only while they have a running descendant) |
| `--label k=v` | only agents with this label (repeatable, every pair must match); their ancestors stay |
| `--json` | an array of root nodes, each with `children` |

Each line is `id name  program  status  run-time`. The status is what the agent reported (with its message in quotes) or `running`; ended agents show `done`, `failed` or `lost`, and a terminal from the GUI shows `shell`. Inside an agent, `tree` shows only its descendants. Design: [0074](docs/decisions/0074-sidebar-tree-and-fleet-tree.md).

#### Report your own status

Run inside an agent. The node learns who you are from the connection, so an agent can only report for itself; you (outside an agent) get exit 3.

```sh
fleet report working "writing the parser"
fleet report done "all tests pass"
fleet report waiting "need a design decision" --question "REST or gRPC?" --choice REST --choice gRPC
```

| Argument / flag | |
|---|---|
| `STATE` | `working`, `waiting`, `done` or `error` |
| `MESSAGE` | a short note, at most 200 characters |
| `--question TEXT` | ask you a question (only with `waiting`; 1–500 characters) |
| `--choice TEXT` | an answer offered as a button (repeatable, at most 6, each at most 60 characters; needs `--question`) |

- A report stands until the agent reports again; nothing expires it. It counts only while the agent runs: an ended agent shows how it ended. A restarted agent starts with no report. A report is stored, so it survives a daemon restart.
- Each change is also posted to the agent's parent as one `status` message (see the mailbox below), so a parent can `fleet wait` for it.
- **Without a report**, the GUI and `fleet wait` guess: an agent that has been silent for 30 s with a last line that looks like a prompt is `waiting (inferred)`. A report always wins over the guess. The daemon checks this every 5 s, only while agents run. Design: [0075](docs/decisions/0075-fleet-report-and-inferred-status.md), [0088](docs/decisions/0088-inferred-waiting-sweep.md).

#### Questions

`--question` makes the agent "needs you" until you answer, or until it reports again (or ends). You answer in the GUI (see below) or:

```sh
fleet answer 7 "REST"      # id or name; the text is at most 4 KiB
```

Only you can answer: an agent gets exit 3. Exit 1 means no such agent or no open question. The answer reaches the agent as an `answer` message (the parent gets a copy), and the agent is then marked `working`. The agent reads it with `fleet inbox` or `fleet wait --state message`. An agent cannot answer for you, so a question is not a way around the performance rule: only your own answer counts. Design: [0078](docs/decisions/0078-questions.md).

#### Messages and the inbox

Every agent, and you, has a mailbox. Agents coordinate through it instead of typing into each other's terminals.

```sh
fleet message 7 "use the v2 API"                  # to an agent, by id or name
fleet message parent "tests are green"            # to your parent (you, for a root agent)
fleet message user "need the staging password"    # to you
fleet message --children "stop and report"        # to each running child
fleet message --label role=reviewer "rebase on main"   # to running descendants with that label
fleet inbox                                       # the new messages, and marks them read
fleet inbox --since 0 --json                      # everything kept, as JSON
```

| `fleet message` flag | |
|---|---|
| `--children` | each running child (yours: running roots) |
| `--descendants` | each running descendant (yours: every running agent) |
| `--label k=v` | only agents with this label (repeatable); alone it implies `--descendants` |
| `--json` | print the recipients as JSON |

| `fleet inbox` flag | |
|---|---|
| `--since CURSOR` | messages after this message id (default: the unread ones); `--since 0` rereads all |
| `--json` | an array of `{id, sender, sender_name, recipient, kind, status, exit_code, body, created_unix_ms, trusted}` |

- **Who can be written to:** an agent may message its descendants, its parent and you. A broadcast reaches only running agents, and never a terminal you opened in the GUI. No recipient matched: exit 1. A message is at most 4 KiB.
- **Automatic reports:** when an agent exits or is lost, its parent gets an `exit` (with the code) or `lost` message. A root agent's reports go to your mailbox.
- **Messages are data, not instructions.** Text is printed as `[from agent 7 (name)] message: …`, control characters escaped. Only an `answer` from you is `trusted` (your words); a message from an agent saying "approved" is not.
- **Retention:** the newest 1,000 messages per agent and 10,000 for you. `fleet rm` removes the agent's mailbox; `fleet restart` carries it over. Design: [0076](docs/decisions/0076-mailbox-exit-reports-wait.md).

#### Wait for agents

```sh
fleet wait                             # news from your descendants, up to 10 minutes
fleet wait 7 --state done,error        # until agent 7 reports done or error
fleet wait --state message --timeout 30s
fleet wait 4 5 --state exited --json
```

| Flag | |
|---|---|
| `AGENTS…` | ids or names (default: all your descendants, **news only**) |
| `--state S,…` | any of `waiting`, `done`, `error`, `exited`, `lost`, `message` (default: all) |
| `--timeout D` | give up after this long: `30s`, `10m`, `1h` (default `10m`, at most 24 h; `0` checks once) |
| `--json` | `{matched, matches: [{agent, name, state, message}], agents: [{id, name, state, exit_code, labels}]}` |

- **Named agents** match on their current state, so `fleet wait 7 --state exited` returns at once if 7 already exited.
- **Without names** only news matches: an unread message in your mailbox, or a descendant reaching a state while you wait. A child that exited before you started waiting is not missed, since its exit report waits unread in your mailbox. `wait` does not mark anything read; `fleet inbox` does.
- `waiting` matches both a reported and an inferred wait. `message` matches a message, question or answer (with names: from those agents only).
- Exit 0 on a match, 1 on a timeout (the current states are printed). Long waits survive a `fleet daemon restart`. Up to 16 waits at once per connection.

#### Leases

A lease is a named lock for a shared resource, such as the benchmark machine. Names are per node and open to everyone.

```sh
fleet lease perf-wall -- fleet-perf run --suite wall       # hold it while the command runs
fleet lease --wait --max-load 2 --ttl 1h perf-wall -- cargo bench
fleet lease acquire perf-wall --ttl 30m                    # hold without a command
fleet lease release perf-wall
fleet lease ls [--json]                                    # NAME HOLDER HELD EXPIRES WAITING
```

| Flag | |
|---|---|
| `--wait` | queue behind the holder (first come, first served) instead of failing with exit 1 |
| `--max-load N` | grant only while the 1-minute load average is below N |
| `--ttl D` | release after this long (`90s`, `30m`, `1h30m`, `2d`), counted from the grant |

- The command's exit code is yours (128+n for a signal, 127 not found, 126 not executable). `fleet lease` passes SIGTERM and SIGHUP on and ignores Ctrl-C itself, so the lease never outlives the command.
- Released when: you release it, the holding agent exits or is lost, the connection closes (the `--` form), or the TTL runs out. Your own `acquire` needs `--ttl`; an agent's does not. Only the holder, or you, can release (an agent releasing someone else's gets exit 3).
- A lease named `acquire`, `release` or `ls` cannot use the `--` form; use `fleet lease acquire <name>`.
- Leases survive `fleet daemon restart`; waiters re-queue at the back. In `lease ls`, an agent sees a holder outside its subtree only as hidden. The `perf-wall` CI job and `fleet-perf run --suite wall` take the lease `perf-wall`.
- Exit codes: 0, 1 busy or not held, 2 usage or other error, 3 forbidden. Design: [0077](docs/decisions/0077-leases.md).

### Read the daemon's log

```sh
fleet logs                 # the last 100 lines
fleet logs -n 500          # the last 500 (at most 10000)
fleet logs -f              # then follow new lines until Ctrl-C
```

`fleet logs` reads the daemon's log over the protocol, including the rotated files. A follower that falls behind sees `… N lines dropped` and keeps following.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | error (for example `fleetd is not running`, or an unknown agent); for `wait`, `lease`, `answer` and `approve`: no match, timeout, busy or not found |
| 2 | `fleet attach` fell behind the agent's output; also usage errors and other errors from `report`, `wait`, `message`, `inbox` and `lease` |
| 3 | `forbidden:` an agent tried something outside its scope ([Agents that spawn agents](#agents-that-spawn-agents)) |
| 128 + n | `fleet attach` was ended by signal n sent to the `fleet` process |

### Not yet in the CLI (TBD)

- Workflows (git worktree per agent, diff view; `RunAction`): **TBD** (M4, [0025](docs/decisions/0025-core-knows-agents-workflows-own-hierarchy.md))
- Remote nodes (`fleet node add user@host`): **TBD** (M6)
- Plugins and agent adapters: **TBD** (M5)
- Credentials and secret grants: **TBD** (M7)
- Setting environment variables for an agent from the CLI: **TBD** (the protocol supports it)

## GUI

`fleetd.app` is a native macOS app (gpui) and a protocol client like `fleet`: it never reads node files, never starts a daemon unless you click for it, and leaves agents running when it quits or crashes. Design: [0028](docs/decisions/0028-gui-client.md).

### Build and install

Requires macOS 13+, Xcode and its Metal toolchain (gpui's shaders are compiled at build time):

```sh
xcodebuild -downloadComponent MetalToolchain   # once
scripts/bundle-macos.sh                        # builds target/release/fleetd.app
open target/release/fleetd.app
```

The bundle holds `fleet-gui`, plus `fleetd` and `fleet` as siblings, so "Start fleetd on this Mac" and "Install agent skill" work without anything on `PATH`. Copy `fleetd.app` to `/Applications` if you like.

The bundle is unsigned. A copy that was downloaded (quarantined) is blocked by Gatekeeper on first open: right-click it and choose Open, or run `xattr -d com.apple.quarantine fleetd.app`. A bundle you built yourself is not quarantined. `scripts/bundle-macos.sh --icon-only` regenerates `assets/logo/fleetd.icns` (macOS 13–15) from `assets/logo/fleetd-icon.svg`; on macOS 26 the icon comes from `assets/logo/fleetd.icon`, compiled with Xcode 26's `actool` while bundling ([ADR 0104](docs/decisions/0104-macos-app-icon.md)).

### Connecting

On launch the GUI connects to the saved target. If that fails, or nothing is saved and no local daemon answers, a connect sheet offers:
- **Start fleetd on this Mac**: starts a detached local daemon, only when you click.
- **Connect to a remote fleetd**: `user@host`, over `ssh` (your `~/.ssh/config`, keys and agent apply; connections share one ControlMaster). `fleetd` must be installed on the host. ssh runs with `BatchMode=yes` and `ConnectTimeout=10`, so the host must be reachable without a password or host-key prompt (keys or the agent, and a known host). Every reconnect runs `fleetd --stdio` on the host again, which starts the remote daemon if it is not running: a remote daemon you stop comes back within seconds while the GUI is connected to it.
- **Recent targets.**

Not yet verified by hand: connecting to a remote host over ssh, the "Start fleetd on this Mac" flow from a clean state, and the menu-bar item with Dock hiding (Show fleetd, closing the window). They are covered by tests against fakes but are still on the hand-off checklist.

### Layout and keys

- **Sidebar:** one vertical tab per agent (status dot, title, time since start, last line), grouped by project label or working directory. Right-click a tab for Kill or Open in new split. Hovering a tab shows Stop for a running agent, Restart and Remove for an exited or lost one.
- **Panes:** rounded cards, up to 4 splits, each with header buttons: Stop (the circled square; Restart and Remove once the agent ended; always on the focused pane, on hover on the others), split right, split down, maximize, close. Close (×) only detaches; only Kill kills. Kill asks first ("Kill agent 7 (claude, ~/fleetd)?"); its "Don't ask again" sets `confirm_kill = false`.

| Key | Action |
|---|---|
| `cmd-d` / `cmd-shift-d` | split right / down |
| `cmd-w` | close the pane (detach) |
| `cmd-shift-w` | kill the focused pane's agent (asks first) |
| `cmd-shift-enter` | maximize or restore the pane |
| `cmd-alt-arrows` | move focus between panes |
| `cmd-1` … `cmd-4` | focus pane 1 to 4 |
| `cmd-n` | new agent |
| `cmd-shift-m` | send text to agents |
| `cmd-c` / `cmd-v` | copy / paste |

### Agent tree and graph

Agents that spawn agents (see [Agents that spawn agents](#agents-that-spawn-agents)) show up as a tree in the GUI. Design: [0073](docs/decisions/0073-graph-view.md), [0074](docs/decisions/0074-sidebar-tree-and-fleet-tree.md), [0089](docs/decisions/0089-graph-message-lines.md).

**Sidebar tree.**
- Children are indented under their parent, within a sidebar group (a child whose parent is in another group starts a tree of its own there).
- A parent with more than 3 children starts collapsed and shows `▸ N`. Your open/closed choice is kept for the session only.
- An ended parent stays, greyed, while it has children.
- A filter keeps the ancestors of a match (dimmed if they do not match themselves) and opens every branch.
- **Remove Exited Agents** removes ended branches: every ended agent with no running agent below it.
- A new child (an agent with a parent that appears after you connected) has its row tinted for about 2.5 s, as a notice that an agent spawned an agent.

**Graph view.** ⌘⇧G, the titlebar's graph button or *Show or Hide the Agent Graph* in the palette replaces the sidebar and panes with a full-window graph; the same key goes back. (In a terminal, ⌘⇧G is Find Previous while its find bar is open.)
- **Cards:** one per agent, with a status dot, name, run time, `program · project`, the last output line and, when collapsed, `N children`. Edges run from parent to child. Cards sit in a box for the machine and one for each project (the `project` label, else the git root of the working directory, else the directory). Ended cards are dimmed. A card that needs you has the warning colour; the accent marks the selected card.
- **Status dots:** solid when the agent reported its status or has ended, hollow for a running agent whose status is a guess.
- **Navigation:** pan by dragging the background or scrolling; zoom 25–200 % with ⌘+, ⌘−, ⌘0, ⌘-scroll or a pinch (below 60 % a card shows only its dot, name and run time). `/` filters by name, program, folder, label, status or id (⏎ returns to the graph, Esc clears, a second Esc gives the keys back).

| Key | Action |
|---|---|
| ← / → | parent / first child |
| ↑ / ↓ | the card above / below in the same column |
| click | select the card (an open terminal panel follows) |
| Space or ⏎ | open the **terminal panel** for the selected card, or give it the keys |
| ⌘⏎ or double-click | open the agent (closes the graph and focuses its pane) |
| C | collapse or expand the subtree |
| Esc | close the terminal panel; with no panel, back to the panes without opening |
| ⌫ | **Stop subtree**: asks first (always), then kills each running agent, deepest first |
| ⇧⌫ | **Remove subtree**: only when everything in it has ended; asks first when it is more than one agent |

- **Terminal panel:** the selected agent's live terminal, docked to the right of the graph (over it in a narrow window). Type into it, scroll, find (⌘F) and copy as in a pane; it never resizes the agent or changes your panes. Esc in it gives the keys back to the graph, Esc again closes it. Drag its left edge to resize it (kept for the session; double-click resets). Its title shows the agent's state and has *Open in Pane*, *Actions…* (the row menu) and a close button. An agent that exits keeps its last screen, marked *Exited*.
- **Message lines:** while the graph is shown, a line with an arrowhead joins two agents when one messaged the other (`message`, `question` or `answer`; not status or exit reports, and not messages to or from you). Lines are accent-coloured, fade over 5 minutes, and show the newest 24 pairs. They appear within about 2 s. No message text is shown.
- **Spawn refused:** a spawn over a quota shows on the parent's card for a few seconds.

### Questions and spawn requests

- **Questions.** An agent that asked with `fleet report waiting --question …` is "needs you". Its sidebar row and the strip under its pane header show the question with one button per choice (a click answers at once) and **Reply…**, which opens an Answer sheet with the question, the choices and a text field. A graph card shows the question in place of the last output line, with **Reply…**. The same answer is `fleet answer`. If the agent replaced the question meanwhile, your answer to the old one is refused.
- **Spawn requests.** With `[node] spawn_approval = true`, each waiting spawn is a card at the top of the sidebar (at the top right in the graph): "*name* (*id*) wants to start an agent", its command, folder and labels, and **Deny** / **Approve**. The same as `fleet pending`, `fleet approve` and `fleet deny`. There is no notification or countdown; `fleet pending` shows the time left. A daemon from before approval shows nothing here.

### Menu bar

A status item is always present: Show fleetd, the running agents, New agent…, Switch fleetd…, Restart fleetd, Install agent skill and Quit. The app menu has Switch fleetd…, Install agent skill and Quit too. Closing the window keeps the app resident (the Dock icon hides); Show fleetd brings it back (not yet verified by hand, see above). **Quit only quits the GUI; agents keep running.**

### Configuration

`~/.fleet/config.toml`, table `[gui]` (all optional):

| Key | Default | |
|---|---|---|
| `connection` | absent | `"local"` or `"ssh://[user@]host"`; the GUI writes this one |
| `default_command` | `["claude"]` | what New agent runs |
| `font_family` | `"SF Mono"` | terminal font; SF Mono is loaded from Terminal.app, any other family must be installed; Menlo when it is not |
| `font_size` | `13` | |
| `option_as_meta` | `"both"` | which Option sends Esc-prefixed keys: `left`, `right`, `both` or `none` (a bool still reads: `true` is `both`); the other Option types `#` `@` `€` on non-US layouts |
| `agent_emoji` | `false` | each agent's emoji in place of its colour swatch (row) and terminal icon (pane header) |
| `confirm_kill` | `true` | ask before killing an agent; the question's "Don't ask again" writes `false` |
| `presets` | the agents found on `PATH` | one-click commands for New agent, the split chooser and the palette: `[[gui.presets]]` tables with `name`, `command` (an array) and optional `labels`; `[]` for none |

For example:

```toml
[[gui.presets]]
name = "Claude (continue)"
command = ["claude", "--continue"]
labels = { role = "dev" }
```

The GUI keeps its own state (recent targets, directories and commands, layouts) in `~/.fleet/gui-state.toml`. `--state-dir` and `--target` override both for one launch.

### Performance

Advisory figures from one local wall run on a dev Mac (local wall numbers are advisory; CI is authoritative and records the baselines): cold start (`gui_cold_start_ns`) about 152 ms against a 200 ms budget, input to render (`input_to_render_ns`) about 12.9 ms and under a 50-agent flood (`input_to_render_flood50_ns`) about 11.8 ms, against 16 ms. These metrics need an unlocked, awake GUI session on the perf-wall runner; see [`bench/README.md`](bench/README.md).

## Remote agents

**TBD** (M6). Today you can already reach a daemon on another host with `ssh host fleetd --stdio`. Planned: `fleet node add user@host` installs `fleetd` on a Linux or macOS host over SSH and runs it as a user service. Agents run there and you attach from your machine. See [0011](docs/decisions/0011-remote-nodes.md).

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Performance budgets are enforced in CI, and regressions need explicit approval. Read [`bench/README.md`](bench/README.md) and [`CLAUDE.md`](CLAUDE.md) before changing anything on a hot path.
