# 0029 — The GUI core stays toolkit-free

**Status:** accepted · 2026-11-15 (M2c)

Builds on [0028](0028-gui-client.md) and [0016](0016-crate-layout.md). Spec: `docs/superpowers/specs/2026-11-14-m2c-daily-drive-design.md`.

## Context

`crates/fleet-gui/src/core` holds the GUI's state and logic: session, sidebar, layout, panes, scrollback, selection, keys. The gpui shell (`src/app`) only draws it. A native Swift or iOS client may replace or join the gpui shell later, and would want this logic through uniffi rather than a rewrite.

## Decision

- `src/core` has no dependency on gpui, AppKit or any other UI toolkit (`use gpui`, `gpui::`, `objc2`, `cocoa`). Comments may mention them.
- `crates/fleet-gui/tests/core_is_toolkit_free.rs` reads every `src/core/*.rs` and fails on such a line outside `//` comments.

## Consequences

- The core's source names no toolkit API, so its logic can be tested without a window (its unit tests build no gpui context) and could later move to its own crate and be exposed through uniffi. It does not yet build without gpui: `src/core` lives in the `fleet-gui` crate, which depends on gpui, so building or testing it still compiles gpui; the guard only keeps that move a matter of moving files.
- Anything that needs the toolkit goes in `src/app`, even when it is small; the core takes plain data in and returns plain data out.
- The test is textual: it does not catch a toolkit type reached through a re-export. Review still matters.
