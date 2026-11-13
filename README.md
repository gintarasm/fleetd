# fleetd

A fast agent fleet manager written in Rust. It runs CLI coding agents (Claude Code, Codex, Gemini CLI, …) in terminals that survive client crashes, on your machine and, later, on remote hosts. The design is in [`docs/plan.md`](docs/plan.md) and the decisions behind it are in [`docs/decisions/`](docs/decisions/).

**Status:** M1 (local node) is done. Today you get a single-machine daemon (`fleetd`) and a command-line client (`fleet`). Anything marked **TBD** below is planned but not implemented yet.

| Milestone | What | Status |
|---|---|---|
| M0 | Benchmark harness and CI perf gate | done |
| M1 | Local node: PTYs, terminal state, scrollback, SQLite, protocol, `fleet` CLI | done |
| M2 | GUI (gpui): sidebar, terminal pane, splits, menu-bar app | TBD |
| M3 | Projects, workspaces, worktrees, tasks; diff view | TBD |
| M4 | Plugins, Claude Code and Codex adapters, status detection, notifications, agent MCP channel | TBD |
| M5 | Remote nodes over SSH | TBD |
| M6 | Credentials | TBD |

## Install

Requires the Rust toolchain pinned in `rust-toolchain.toml`; `rustup` installs it automatically. Runs on macOS and Linux.

```sh
cargo build --release -p fleetd -p fleet-cli
# binaries: target/release/fleetd and target/release/fleet
```

Put both on your `PATH`, or call them by path.

## Running the daemon

```sh
fleetd                      # state in ~/.fleet
fleetd --state-dir /path    # alternative state directory
```

- **Foreground process.** `fleetd` runs in the foreground and serves clients on `<state-dir>/fleetd.sock`. To keep it running, start it from a terminal multiplexer or a service manager (a launchd/systemd setup is TBD).
- **One instance per state directory.** A second `fleetd` on the same directory exits with `fleetd already running` (exit code 2).
- **Stopping.** SIGTERM, SIGINT (Ctrl-C) or SIGHUP stops every agent: each gets SIGTERM, then 2 s later SIGKILL. Their exits are recorded, then the socket is removed.
- **Crashes.** If `fleetd` is killed hard, the agents it was running are shown as `lost` on the next start.

The state directory (mode 0700) holds:

| Path | What |
|---|---|
| `fleetd.sock` | client socket |
| `fleetd.lock` | single-instance lock |
| `state.db` | agent records (SQLite) |
| `scrollback/` | per-agent output history (8 MiB ring file each) |

## CLI: `fleet`

Every command talks to a running `fleetd`. `--state-dir <path>` selects a daemon other than the one in `~/.fleet`. `fleet --help` and `fleet <command> --help` show all options.

### Start an agent

```sh
fleet spawn claude
fleet spawn -- codex --model o4-mini
fleet spawn --cols 120 --rows 40 bash -lc 'npm test'
```

`fleet spawn` prints the new agent's ID.
- **Working directory:** the agent starts in your current directory.
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

### Attach to an agent

```sh
fleet attach 1
```

Opens the agent's terminal in your terminal.
- **Screen:** you immediately see its current screen (a snapshot), then live output. Input goes to the agent, and your terminal size is passed to it.
- **Detach:** press **Ctrl-\\**. The agent keeps running. Your terminal is restored on every way out: detach, agent exit, a signal sent to the `fleet` process (SIGINT/SIGTERM/SIGHUP), or an error. While attached, Ctrl-C goes to the agent, not to `fleet`.
- **Many attachments:** several clients can attach to the same agent at once.
- **Falling behind:** if your client can't keep up with the agent's output, `fleetd` drops the stream rather than slowing the agent. `fleet attach` then exits with code 2 and `output lagged behind; re-attach`. Run it again to get a fresh snapshot.
- **Kitty keyboard limit:** when an agent turns on the kitty keyboard protocol, Ctrl-\\ may not detach. A fix is in progress.
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

### Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | error (for example `fleetd is not running`, or an unknown agent) |
| 2 | `fleet attach` fell behind the agent's output |
| 128 + n | `fleet attach` was ended by signal n sent to the `fleet` process |

### Not yet in the CLI (TBD)

- Projects, workspaces, worktrees and tasks (`fleet project …`, `fleet task …`): **TBD** (M3)
- Remote nodes (`fleet node add user@host`): **TBD** (M5)
- Plugins and agent adapters: **TBD** (M4)
- Credentials and secret grants: **TBD** (M6)
- Agent-to-agent messages (`fleet msg`) and status reporting (`fleet status`): **TBD** (M4, [0019](docs/decisions/0019-control-plane-direction.md))
- Setting environment variables for an agent from the CLI: **TBD** (the protocol supports it)

## GUI

**TBD** (M2). Planned: a native gpui app with a menu-bar icon, a fleet sidebar grouped by node and workspace with status badges, terminal panes with up to 4 splits, a command palette and a read-only diff view. See [`docs/plan.md`](docs/plan.md).

## Remote agents

**TBD** (M5). Planned: `fleet node add user@host` installs `fleetd` on a Linux or macOS host over SSH and runs it as a user service. Agents run there and you attach from your machine. See [0011](docs/decisions/0011-remote-nodes.md).

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Performance budgets are enforced in CI, and regressions need explicit approval. Read [`bench/README.md`](bench/README.md) and [`CLAUDE.md`](CLAUDE.md) before changing anything on a hot path.
