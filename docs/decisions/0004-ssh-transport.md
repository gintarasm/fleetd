# 0004 — Controller connects to nodes over SSH

**Status:** accepted · 2026-11-13

**Context:** A "node dials home" design fails because the controller is a laptop behind NAT that sleeps; relays are a major source of Orca bugs.

**Decision:** Controller initiates: `ssh host fleetd --stdio` (or a forwarded unix socket). Reuses SSH keys and config, no open ports, no relay.

**Alternatives:** node listens on TLS/QUIC (cert management, open port/Tailscale); relay (needed only for mobile later).

**Consequences:** Transport is a plain byte stream, so a TLS/QUIC transport can be added later without protocol changes.
