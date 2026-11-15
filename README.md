<p align="center">
  <img src="assets/logo/fleetd-wordmark.svg" alt="fleetd" width="360">
</p>

# fleetd

A fast agent fleet manager written in Rust. It runs CLI coding agents (Claude Code, Codex, Gemini CLI, …) in terminals that survive client crashes, on your machine and, later, on remote hosts. The design is in [`docs/plan.md`](docs/plan.md) and the decisions behind it are in [`docs/decisions/`](docs/decisions/).

**Status:** M1 (local node), M2a (the daemon side of the GUI: agent history, protocol v2, labels, `fleetd --stdio`, the agent skill) and M2b (the macOS GUI) are done. Today you get a daemon (`fleetd`), a command-line client (`fleet`) and a macOS app (`fleetd.app`). Anything marked **TBD** below is planned but not implemented yet.

| Milestone | What | Status |
|---|---|---|
| M0 | Benchmark harness and CI perf gate | done |
| M1 | Local node: PTYs, terminal state, scrollback, SQLite, protocol, `fleet` CLI | done |
| M2a | Daemon side of the GUI: agent history, protocol v2, labels, flood fix, `fleetd --stdio`, login-shell env, agent skill | done |
| M2b | GUI (gpui): sidebar, terminal panes, splits, menu-bar app, connect sheet | done |
| M2c | Daily-drive polish: native-feeling GUI, restart and remove exited agents | TBD |
| M3 | Projects, workspaces, worktrees, tasks; diff view | TBD |
| M4 | Plugins, Claude Code and Codex adapters, status detection, notifications, agent MCP channel | TBD |
| M5 | Remote nodes over SSH | TBD |
| M6 | Credentials | TBD |

## Install

Requires the Rust toolchain pinned in `rust-toolchain.toml`; `rustup` installs it automatically. Runs on macOS and Linux.

Install both binaries into `~/.cargo/bin`, which rustup normally puts on your `PATH`:

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
fleetd --stdio              # bridge stdin/stdout to the daemon, starting one if needed
```

- **Foreground process.** `fleetd` runs in the foreground and serves clients on `<state-dir>/fleetd.sock`. To keep it running, start it from a terminal multiplexer or a service manager (a launchd/systemd setup is TBD).
- **One instance per state directory.** A second `fleetd` on the same directory exits with `fleetd already running` (exit code 2).
- **Stopping.** SIGTERM, SIGINT (Ctrl-C) or SIGHUP stops every agent: each gets SIGTERM, then 2 s later SIGKILL. Their exits are recorded, then the socket is removed.
- **Crashes.** If `fleetd` is killed hard, the agents it was running are shown as `lost` on the next start.
- **`--stdio`.** `fleetd --stdio` connects its stdin and stdout to the daemon's socket, so any client that can run a command can talk to it. If no daemon is running it starts one in the background, which keeps running after the bridge exits; concurrent bridges still end up with exactly one daemon. This is how a remote daemon is reached: `ssh host fleetd --stdio` (use the full path, such as `~/.cargo/bin/fleetd`, if it is not on the `PATH` of non-interactive ssh sessions; remote nodes in the client are M5).
- **Agent environment.** At startup the daemon asks your login shell (`$SHELL -lic`) for its environment, so agents see the same `PATH` and exports as a terminal would, even when `fleetd` was started by `ssh`, launchd or systemd. The capture runs in the background and gives up after 5 s; if it fails, agents get the daemon's own environment. Set `FLEETD_LOGIN_ENV=off` in the daemon's environment to skip it. Every agent also gets `TERM=xterm-256color` and `FLEET_AGENT_ID=<its id>`.

The state directory (mode 0700) holds:

| Path | What |
|---|---|
| `fleetd.sock` | client socket |
| `fleetd.lock` | single-instance lock |
| `state.db` | agent records (SQLite) |
| `scrollback/` | per-agent raw output (8 MiB ring file each) |
| `history/` | per-agent history: the lines that scrolled off the screen (up to 64 MiB each) |
| `logs/` | the daemon's log `fleetd.log` (rotated at 10 MiB into `fleetd.log.1` and `.2`, 30 MiB at most) and the `--stdio` bridges' `fleetd-stdio.log` |

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
- **Terminal size:** defaults to your terminal's size, or use `--cols`/`--rows`.
- **`TERM`:** set to `xterm-256color` for the agent.
- **Size limits:** 1–1000 columns or rows each, and at most 250,000 cells in total.

### List agents

```sh
$ fleet ls
ID  STATE    EXIT  PROGRAM
1   running  -     claude
2   exited   0     bash -lc npm test
```

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

### Stop an agent

```sh
fleet kill 1
```

Sends SIGTERM to the agent's process group. There is no SIGKILL follow-up yet, so an agent that ignores SIGTERM keeps running until `fleetd` stops.

### Restart and remove agents

```sh
fleet restart 1             # same program, args, cwd and labels, new id; the old agent is removed
fleet restart 1 --attach    # and attach to the new one
fleet rm 1 2                # forget exited or lost agents: list entry, history, scrollback
fleet rm --exited           # forget every exited or lost agent
fleet rm --exited --selector project=fleetd
```

- Only exited or lost agents can be restarted or removed; running ones are refused. `fleet rm` exits non-zero if any id was refused.
- The environment given at spawn is not stored, so a restarted agent gets the daemon's base environment, not the original `--env`.
- Ids are never reused.
- An older daemon answers "the daemon is too old to restart/remove agents".

### Let agents use the fleet

```sh
fleet skill install                  # writes ~/.claude/skills/fleet/SKILL.md
fleet skill install --project        # writes ./.claude/skills/fleet/SKILL.md (or --project DIR)
fleet skill uninstall                # removes it (add --project to remove the project copy)
fleet skill print                    # prints the skill
```

The skill teaches an agent (Claude Code for now) to use `fleet`: list agents (`ls --json`), read another agent's `screen` or `history`, `send` it input, and `spawn` or `kill` agents when you ask it to. Each agent knows its own id from `$FLEET_AGENT_ID`. The skill is built into the `fleet` binary, so re-running `install` after an upgrade updates it. It does not need a running daemon.

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
| 1 | error (for example `fleetd is not running`, or an unknown agent) |
| 2 | `fleet attach` fell behind the agent's output |
| 128 + n | `fleet attach` was ended by signal n sent to the `fleet` process |

### Not yet in the CLI (TBD)

- Workflows (git worktree per agent, diff view; `RunAction`): **TBD** (M3, [0025](docs/decisions/0025-core-knows-agents-workflows-own-hierarchy.md))
- Remote nodes (`fleet node add user@host`): **TBD** (M5)
- Plugins and agent adapters: **TBD** (M4)
- Credentials and secret grants: **TBD** (M6)
- Agent-to-agent messages (`fleet msg`) and status reporting (`fleet status`): **TBD** (M4, [0019](docs/decisions/0019-control-plane-direction.md))
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

The bundle is unsigned. A copy that was downloaded (quarantined) is blocked by Gatekeeper on first open: right-click it and choose Open, or run `xattr -d com.apple.quarantine fleetd.app`. A bundle you built yourself is not quarantined. `scripts/bundle-macos.sh --icon-only` regenerates `assets/logo/fleetd.icns` from `assets/logo/fleetd-icon.svg`.

### Connecting

On launch the GUI connects to the saved target. If that fails, or nothing is saved and no local daemon answers, a connect sheet offers:
- **Start fleetd on this Mac**: starts a detached local daemon, only when you click.
- **Connect to a remote fleetd**: `user@host`, over `ssh` (your `~/.ssh/config`, keys and agent apply; connections share one ControlMaster). `fleetd` must be installed on the host. ssh runs with `BatchMode=yes` and `ConnectTimeout=10`, so the host must be reachable without a password or host-key prompt (keys or the agent, and a known host). Every reconnect runs `fleetd --stdio` on the host again, which starts the remote daemon if it is not running: a remote daemon you stop comes back within seconds while the GUI is connected to it.
- **Recent targets.**

Not yet verified by hand: connecting to a remote host over ssh, the "Start fleetd on this Mac" flow from a clean state, and the menu-bar item with Dock hiding (Show fleetd, closing the window). They are covered by tests against fakes but are still on the hand-off checklist.

### Layout and keys

- **Sidebar:** one vertical tab per agent (status dot, title, time since start, last line), grouped by project label or working directory. Right-click a tab for Kill or Open in new split.
- **Panes:** rounded cards, up to 4 splits, each with header buttons: split right, split down, maximize, close. Close only detaches; only Kill kills.

| Key | Action |
|---|---|
| `cmd-d` / `cmd-shift-d` | split right / down |
| `cmd-w` | close the pane (detach) |
| `cmd-shift-enter` | maximize or restore the pane |
| `cmd-alt-arrows` | move focus between panes |
| `cmd-1` … `cmd-4` | focus pane 1 to 4 |
| `cmd-n` | new agent |
| `cmd-c` / `cmd-v` | copy / paste |

### Menu bar

A status item is always present: Show fleetd, the running agents, New agent…, Switch fleetd…, Install agent skill and Quit. The app menu has Switch fleetd…, Install agent skill and Quit too. Closing the window keeps the app resident (the Dock icon hides); Show fleetd brings it back (not yet verified by hand, see above). **Quit only quits the GUI; agents keep running.**

### Configuration

`~/.fleet/config.toml`, table `[gui]` (all optional):

| Key | Default | |
|---|---|---|
| `connection` | absent | `"local"` or `"ssh://[user@]host"`; the GUI writes this one |
| `default_command` | `["claude"]` | what New agent runs |
| `font_family` | `"Menlo"` | |
| `font_size` | `13` | |
| `option_as_meta` | `true` | Option sends Esc-prefixed keys |

The GUI keeps its own state (recent targets and directories, layouts) in `~/.fleet/gui-state.toml`. `--state-dir` and `--target` override both for one launch.

### Performance

Advisory figures from one local wall run on a dev Mac (local wall numbers are advisory; CI is authoritative and records the baselines): cold start (`gui_cold_start_ns`) about 152 ms against a 200 ms budget, input to render (`input_to_render_ns`) about 12.9 ms and under a 50-agent flood (`input_to_render_flood50_ns`) about 11.8 ms, against 16 ms. These metrics need an unlocked, awake GUI session on the perf-wall runner; see [`bench/README.md`](bench/README.md).

## Remote agents

**TBD** (M5). Today you can already reach a daemon on another host with `ssh host fleetd --stdio`. Planned: `fleet node add user@host` installs `fleetd` on a Linux or macOS host over SSH and runs it as a user service. Agents run there and you attach from your machine. See [0011](docs/decisions/0011-remote-nodes.md).

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Performance budgets are enforced in CI, and regressions need explicit approval. Read [`bench/README.md`](bench/README.md) and [`CLAUDE.md`](CLAUDE.md) before changing anything on a hot path.
