# 0006 — Protobuf-framed protocol with custom light RPC

**Status:** accepted · 2026-11-13

**Decision:** One protocol for GUI↔daemon and controller↔node: protobuf messages (prost), length-prefixed over any byte stream (unix socket, SSH stdio, later QUIC). Terminal output uses dedicated binary frames. A version handshake happens on connect.

**Alternatives:** full gRPC (heavy, awkward over stdio); postcard (fastest but Rust-only, bad for future mobile clients); JSON-RPC (too slow for 50 streaming terminals).
