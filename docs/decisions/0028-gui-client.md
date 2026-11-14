# 0028 — The GUI client (`fleet-gui`)

**Status:** accepted · 2026-11-14 (M2b)

Builds on [0002](0002-daemon-and-gpui-client.md), [0016](0016-crate-layout.md), [0019](0019-control-plane-direction.md), [0022](0022-protocol-v2.md), [0023](0023-crates-term-client.md) and [0026](0026-node-providers-and-transports.md). Spec: `docs/superpowers/specs/2026-11-13-m2-gui-design.md` §5.

## Context

M2b adds a native macOS GUI on gpui. It is a protocol client like the CLI: it never reads node files, never starts a daemon without asking, and leaves agents running when it quits or crashes. This ADR records the decisions the spec left to the builder.

## Decision

- **Core/shell split.** `fleet-gui` is a library (`fleet_gui`) plus the `fleet-gui` binary. `src/core` is platform-neutral (targets, config, saved state, sessions, pane models, key encoding, split tree) and uses no gpui or AppKit, so it is unit-tested on Linux CI. `src/app` is the gpui shell, compiled only on macOS. On other platforms the binary prints `fleet-gui is macOS-only` and exits 1. gpui, `objc2`, `objc2-app-kit` and `objc2-foundation` are `cfg(target_os = "macos")` dependencies; gpui is pinned `=0.2.2`.
- **Resident menu-bar app.** A status item is always present. Closing the last window keeps the process and its attachments alive and hides the Dock icon (accessory activation policy). The menu shows the current target, running agents, "New agent…", "Switch fleetd…" and "Quit". Quit only detaches; only Kill kills an agent.
- **Connect sheet.** On startup the GUI tries the saved target. If that fails, or none is saved and no local daemon answers, a sheet offers "Start fleetd on this Mac" (launched detached, only on the user's click), "Connect to a remote fleetd" (`user@host`) and the recent targets. A remote host without `fleetd` is reported as "fleetd is not installed on <host>", not as a protocol error.
- **Config and saved state.** `<state-dir>/config.toml` (default `~/.fleet`), table `[gui]`:

  | key | type | default |
  |---|---|---|
  | `connection` | `"local"` or `"ssh://[user@]host"` | absent |
  | `default_command` | array of strings | `["claude"]` |
  | `font_family` | string | `"Menlo"` |
  | `font_size` | number | `13` |
  | `option_as_meta` | bool | `true` |

  The GUI writes only `connection`, with `toml_edit`, so comments and other keys survive. A malformed config is an error naming the file and line. GUI-owned state lives in `<state-dir>/gui-state.toml` (mode 0600, atomic temp-file-and-rename writes, debounced 500 ms): `recent_targets` (at most 10), `recent_dirs` per target (at most 20) and `layouts` per target. A missing or corrupt file gives defaults, and layout data of an unknown shape is dropped rather than failing the load. Flags `--state-dir` and `--target` override the directory and the saved connection for one launch.
- **Connections.** One control connection (`gui`: list, activity, spawn, kill) plus one connection per visible pane (`gui-pane-<n>`), per [0022](0022-protocol-v2.md) and [0023](0023-crates-term-client.md). Handshake timeout is 10 s locally and 30 s over ssh. Disconnects reconnect to the same target with backoff (250 ms doubling to 5 s) and re-attach visible panes.
- **Threading.** Protocol I/O never runs on the UI thread. Each connection has a reader thread and a writer thread; they update shared pane models and notify the UI through an `async_channel`. The UI thread only enqueues input and redraws, at most once per frame however much output arrived, so a blocked socket cannot stall typing.
- **ssh transport.** `ssh -o ControlMaster=auto -o ControlPath=<state-dir>/ssh/%C -o ControlPersist=60 <dest> sh -lc 'exec fleetd --stdio'`, using the system `ssh` so `~/.ssh/config`, keys and the agent apply. The shared ControlMaster keeps all of a window's connections on one TCP link; `sh -lc` resolves `~/.cargo/bin` from a login PATH. The `ssh` directory is mode 0700.
- **Client-side query answering.** The client terminal answers the agent's startup queries itself and sends the replies as `SendInput` on the primary connection. Kitty keyboard-protocol replies (`CSI ? u`) are masked to the flags the GUI supports (`KITTY_SUPPORTED_FLAGS = 0b1`, disambiguate only). Colour queries (OSC 10, 11 and 4) are answered from the active light or dark palette, which follows the window appearance.
- **Shift+Enter.** Sends `\x1b[13;2u` when the agent enabled kitty disambiguation, otherwise `\x1b\r`, so a newline can be entered in Claude Code.
- **Precompiled shaders.** gpui is built with its default features, so Metal shaders are compiled at build time. Building `fleet-gui` therefore needs the Metal toolchain (`xcodebuild -downloadComponent MetalToolchain`). `runtime_shaders` is not used: compiling shaders at every launch would threaten the 200 ms cold-start budget ([0014](0014-performance-gate.md)).

## Consequences

Core logic is testable without a window server. A macOS build needs Xcode and its Metal toolchain; the perf-wall runner already has both. Terminal rendering is our own gpui element; no Zed terminal code is copied (GPL-3.0).
