# 0023 — Crates `fleet-term` and `fleet-client`

**Status:** accepted · 2026-11-13 (M2a)

**Amends:** [0016](0016-crate-layout.md): two library crates join the workspace.

## Context

The GUI (M2b) must parse agent output and render history exactly as the node does, and talk to `fleetd` the same way the CLI does. In M1 the terminal wrapper lived in `fleet-node` and the protocol client in `fleet-cli`, so the GUI would have depended on the node or on a binary crate.

## Decision

- **`fleet-term`:** the `Terminal` wrapper around `alacritty_terminal` (feed, snapshot, history capture through the `Recorder` handler, ANSI row rendering, last non-blank line). Moved out of `fleet-node` unchanged in behaviour. Used by `fleet-node` (agent grids and history, re-exported as `fleet_node::term`/`fleet_node::Terminal`), `fleet-cli` (`fleet history --plain` renders lines through it) and the GUI. Dependencies: `alacritty_terminal` and `unicode-width` (pinned to the version alacritty uses, so wide-character decisions agree).
- **`fleet-client`:** a blocking, transport-generic protocol client, moved out of `fleet-cli`. `Connection::over(read, write)` runs the handshake over any byte stream; `Connection::unix(path)` connects to a daemon socket (a missing or refusing socket is `NotRunning`); `Connection::command(cmd)` spawns a process and speaks over its stdio (for `ssh host fleetd --stdio` or any provider command of [0026](0026-node-providers-and-transports.md)), killing it if the handshake fails; `call` sends one request and returns its response; `split` gives a frame reader and a sender for streaming use on two threads. Depends only on `fleet-proto`, `bytes` and `tokio-util`'s codec, no runtime.

## Consequences

Node, CLI and GUI share one terminal implementation and one client, so a rendering fix lands everywhere at once. The client is blocking; an async front end can be added later on top of the same frame codec. Updated crate list: `fleet-proto`, `fleet-term`, `fleet-node`, `fleet-client`, `fleet-controller`, `fleet-plugin-host`, `fleetd`, `fleet-gui`, `fleet-cli` (plus `fleet-perf` for benchmarks).
