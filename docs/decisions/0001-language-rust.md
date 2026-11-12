# 0001 — Implement in Rust

**Status:** accepted · 2026-11-13

**Context:** The app must be fast and performant. Zig was considered.

**Decision:** Rust.

**Alternatives:** Zig — slightly more control, but pre-1.0 with breaking changes and no ecosystem for what we need (async runtime, WASM host, SSH, PTY, VT parsing, GPU UI).

**Consequences:** We use tokio, wasmtime, russh/openssh, portable-pty, alacritty_terminal, prost, rusqlite, gpui. If a hot path ever needs it, Zig can be added via C FFI.
