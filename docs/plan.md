# fleetd — Plan

Status: agreed design, pre-implementation (2026-11-13). Decisions are recorded in [`decisions/`](decisions/).

## Vision

A fast, reliable agent fleet manager — in the same space as [Orca](https://github.com/stablyai/orca) — written in Rust.

- **Start:** orchestrate existing CLI coding agents (Claude Code, Codex, Gemini CLI, …) running locally and on remote machines.
- **End goal:** also host a built-in agent runtime, plugged in as a native adapter ([0015](decisions/0015-scope-positioning-roadmap.md)).

**Positioning:** a small, opinionated core done extremely well, extensible through plugins, leaning server/headless-first. We win on exactly where Orca hurts: daemon that survives UI crashes, no lag with many agents, solid remote connectivity, no leaked daemons.

**User:** a single developer (the author). Data model and protocol must not preclude multi-user later.

## Performance budget (hard, CI-enforced)

| Metric | Budget |
|---|---|
| Cold start (GUI to usable) | < 200 ms |
| Concurrent agents per node without UI lag | ≥ 50 |
| Input-to-render latency | < 16 ms |
| Daemon idle RSS | < 50 MB |

Any regression fails CI and requires explicit approval by the user. See [0014](decisions/0014-performance-gate.md) and [`perf-log.md`](perf-log.md).

## Architecture

```
            macOS laptop                                   Linux box(es)
 ┌──────────────────────────────────────┐          ┌──────────────────────────┐
 │ fleet-gui (gpui, menu-bar resident)  │          │ fleetd (role: node)      │
 │        │ unix socket (protobuf)      │          │  PTYs · terminal grids   │
 │ fleetd (roles: controller + node)    │──SSH────▶│  worktrees · SQLite      │
 │  controller: fleet view, plugins,    │  stdio   │  adapter plugins (wasm)  │
 │   secrets (Keychain), node bootstrap │          │  systemd --user, lingering│
 │  node: local PTYs, worktrees, SQLite │          └──────────────────────────┘
 └──────────────────────────────────────┘
```

- **One binary, two roles.** `fleetd` runs as `node` (owns agent processes, PTYs, terminal state, SQLite) and/or `controller` (aggregates nodes, runs fleet-level plugins, holds secrets). The Mac runs both in one process; remote boxes run node only. Nodes are autonomous: agents keep running if the laptop sleeps or the GUI crashes. ([0002](decisions/0002-daemon-and-gpui-client.md), [0003](decisions/0003-node-controller-roles.md))
- **Transport:** controller → node over SSH (`ssh host fleetd --stdio` / forwarded socket). No open ports, no relay. TLS/QUIC listener later. ([0004](decisions/0004-ssh-transport.md))
- **Protocol:** protobuf (prost) messages, length-framed over any byte stream, custom light RPC, dedicated binary frames for terminal output, version handshake. Same protocol GUI↔daemon and controller↔node. ([0006](decisions/0006-wire-protocol.md))
- **Terminal state:** each node keeps a headless `alacritty_terminal` grid + bounded on-disk scrollback per agent. Reattach = instant snapshot + live stream. ([0005](decisions/0005-terminal-state-on-node.md))
- **Plugins:** WASM components (wasmtime + WIT), subprocess-over-stdio escape hatch, no native dylibs. UI plugins return a declarative view tree rendered natively. ([0007](decisions/0007-plugin-system.md))
- **Status detection:** agent CLI hooks first, PTY heuristics fallback; status carries source + confidence. ([0008](decisions/0008-agent-status-detection.md))
- **Control-plane direction:** the controller as a control plane that runs on any host, an MCP channel per agent for messaging and spawning, and credentials split into a core broker plus secret-provider plugins ([0019](decisions/0019-control-plane-direction.md)).

## Domain model ([0025](decisions/0025-core-knows-agents-workflows-own-hierarchy.md), supersedes 0009)

- **The core knows nodes and agents only.** An agent is a process on a node with a cwd, env, terminal and history.
- **Labels:** agents carry opaque key/value labels (`project=fleetd`, `git.worktree=…`). The core stores and filters them without interpreting them. Only user clients and workflows write them; agents cannot.
- **Workflows:** projects, workspaces, worktrees and tasks are higher-level concepts owned by **workflows**, a controller plugin kind.
  - Each workflow exposes named actions that any interface (CLI, GUI, iOS, web) calls with `RunAction`.
  - They are built on generic node primitives (`Exec`, `ReadFile`, `ListDir`) and labels.
- **The built-in git-worktree workflow** owns worktree-per-agent, the read-only diff, archive, clone and fetch-back.

## Storage ([0010](decisions/0010-storage-and-layout.md))

- Worktrees: `~/.fleet/worktrees/<repo>/<slug>`, branch `fleet/<slug>`.
- State: SQLite (WAL) per node. Scrollback: on-disk ring buffers per agent. History: rendered lines, an in-memory tail plus on-disk segments per agent ([0021](decisions/0021-agent-history.md)).
- Archive cleanup refuses when dirty/unpushed unless forced.

## Remote nodes ([0011](decisions/0011-remote-nodes.md))

- `fleet node add user@host`: detect OS/arch, upload static (musl) `fleetd` to `~/.fleet/bin`, run as `systemd --user` with lingering, auto-upgrade on version mismatch. No root.
- Workspaces on a node: point at an existing directory, or clone (from origin via ssh-agent forwarding; git bundle over SSH as fallback).
- Results: local repo fetches the task branch from the node over SSH — no need to push WIP to the forge.

## Credentials ([0012](decisions/0012-credentials.md))

- Secrets live in macOS Keychain on the controller, granted per node / project / workspace / task.
- Injected at agent spawn as env vars or tmpfs files (0600), deleted on exit; never logged; masked in scrollback.
- Browser/OAuth flows on nodes (MCP OAuth, `claude login`): controller opens the URL on the Mac and sets up an SSH reverse port-forward for the localhost callback.
- ssh-agent forwarding: opt-in per node.

## Plugins ([0007](decisions/0007-plugin-system.md))

| Kind | Runs on | v1 |
|---|---|---|
| Agent adapter | node | yes |
| Node provider (`node-provider` world, [0026](decisions/0026-node-providers-and-transports.md)) | controller | yes |
| Lifecycle hooks | where the event fires | yes |
| UI (declarative) | controller → rendered by GUI | yes |
| Integrations (GitHub, Linear, Slack) | — | later |

Manifest declares kind + capabilities (`pty:read`, `pty:write`, `fs:worktree`, `net:<host>`, `exec:<bin>`, `notify`, `storage:own`); approved at install, re-approved on new capabilities. Controller distributes pinned `.wasm` to nodes.

## v1 scope

**In:** spawn/manage CLI agents in PTYs · agent labels + workflows (built-in git-worktree) · live terminal + fleet sidebar (grouped by label, status badge, last line) · up to 4 splits · command palette · status detection · native notifications via menu-bar app · read-only diff view · local + remote nodes · plugins (adapters, providers, hooks, declarative UI) · credential forwarding.

**Later:** chat UI · diff line comments → agent · coordinator/sub-agents · built-in browser / computer use · mobile apps · scheduled automations · usage/account tracking · integrations · TLS/QUIC transport · agents surviving a fleetd crash (holder process per agent, [0032](decisions/0032-daemon-restart-handoff.md)) · non-git snapshot diffs · built-in agent runtime · multi-user.

**Platforms:** controller + GUI on macOS; nodes on Linux (and macOS). Windows deferred.

## Crate layout ([0016](decisions/0016-crate-layout.md))

| Crate | Purpose |
|---|---|
| `fleet-proto` | protobuf messages + framing |
| `fleet-term` | terminal wrapper (alacritty grid, snapshot, history capture and rendering) shared by node, CLI and GUI ([0023](decisions/0023-crates-term-client.md)) |
| `fleet-node` | PTYs, terminal grid, history, worktrees, SQLite |
| `fleet-client` | blocking, transport-generic protocol client (unix socket, any stdio command) ([0023](decisions/0023-crates-term-client.md)) |
| `fleet-controller` | fleet view, SSH transport, bootstrap, secrets |
| `fleet-plugin-host` | wasmtime host, WIT worlds |
| `fleetd` | daemon binary wiring roles |
| `fleet-gui` | gpui client (macOS shell over a platform-neutral core) ([0028](decisions/0028-gui-client.md)) |
| `fleet-cli` | thin scripting client |

## Milestones

| | Milestone |
|---|---|
| **M0** | Workspace skeleton, benchmark harness (50 fake agents flooding PTYs), CI perf gate, baselines |
| **M1** | Local node: PTYs, terminal grid, scrollback, SQLite, protocol, `fleet-cli` — done 2026-11-13 ([0018](decisions/0018-m1-protocol-and-node.md)); new benchmarks' baselines are recorded from CI |
| **M2a** | Daemon side of the GUI: agent history (tail + pages), protocol v2 (history, spawn-attach, lag and activity events, primary client, labels), the 50-agent flood fix, `fleet-term` and `fleet-client`, CLI `history`/`screen`/`label`, `fleetd --stdio`, login-shell env, the fleet agent skill — done 2026-11-13 ([0021](decisions/0021-agent-history.md), [0022](decisions/0022-protocol-v2.md), [0023](decisions/0023-crates-term-client.md), [0024](decisions/0024-agent-skill-stdio-env.md)); new benchmarks' baselines are recorded from CI |
| **M2b** | gpui client: sidebar, terminal pane with history, splits, menu-bar app, connect sheet, GUI benchmarks — done 2026-11-14 ([0028](decisions/0028-gui-client.md)); `fleetd.app` via `scripts/bundle-macos.sh`; GUI baselines are recorded from CI. Follow-ups: command palette, diff view (M3), light-theme polish, Intel/Linux GUI |
| **M2c** | Daily-drive polish: native-feeling UI polish of fleet-gui (unified translucent titlebar with inline traffic lights, blur, SF typography and spacing, native context menus, scrolling and motion; starts with a time-boxed gpui restyle spike compared side by side with the user's reference before deciding gpui vs a native app), plus agent lifecycle: Restart an exited agent (same program/args/cwd/labels, new id) and Remove/forget exited or lost agents (a node request that drops the list entry and its history), per tab and bulk "Clear exited", GUI and CLI; restart fleetd into a new build with agents kept running (`fleet daemon restart`, GUI Restart fleetd, `scripts/install.sh`; [0032](decisions/0032-daemon-restart-handoff.md)) |
| **M3** | Node primitives (`Exec`/`ReadFile`/`ListDir`), `RunAction` interface, built-in git-worktree workflow, diff view ([0025](decisions/0025-core-knows-agents-workflows-own-hierarchy.md); core labels landed in M2a) |
| **M4** | Plugin host (incl. third-party workflows), Claude Code + Codex adapters, hook-based status, notifications |
| **M5** | Remote nodes via node providers ([0026](decisions/0026-node-providers-and-transports.md)): `node-provider` world, built-in SSH-host provider, SSH/command transports, bootstrap (systemd optional), keepalive and `on_idle`, history archive before teardown; workspace clone and fetch-back as git-worktree actions |
| **M6** | Secrets, OAuth reverse-forwarding, ssh-agent forwarding |

## Licensing

Private for now; revisit before the plugin API stabilises.
