# 0072 — Agent identity: the caller's session, then its token

**Status:** accepted · 2026-11-16 (user decision; closes the "token boundary" question left open by [0034](0034-agent-communication.md))

**Amends:** [0033](0033-satellite-milestone.md) and [0034](0034-agent-communication.md): how fleetd decides whether a `fleet` call comes from an agent (and which one) or from the user.

## Context

Agents run as the user's own OS user and reach the same socket (`~/.fleet/fleetd.sock`). 0033 and 0034 identify an agent by its `FLEET_AGENT_TOKEN`. On its own a token is only cooperative: an agent that unsets it, or runs `env -u FLEET_AGENT_TOKEN fleet …`, would be treated as the user and escape the descendants-only scope. 0034 left open how fleetd tells an agent's process from the user's.

Every agent is the leader of its own session: the PTY host calls `setsid` for each child, so the agent's pid is its session id, and everything the agent starts shares that session unless it calls `setsid` itself.

## Decision

- **Session first.** On each accepted connection fleetd reads the peer's pid (`LOCAL_PEERPID` on macOS, `SO_PEERCRED` on Linux; tokio's `UnixStream::peer_cred`) and its session id (`getsid(pid)`). If that session id is a running agent's pid, **the caller is that agent**, whatever token it sends or doesn't send.
- **Token second.** If the session belongs to no agent, a valid `FLEET_AGENT_TOKEN` in `Hello` identifies the agent. This covers an agent's helpers that left its session (a `tmux` server, `nohup setsid …`).
- **Conflicts are refused.** If the session says agent A but the token says agent B, the handshake fails (`HandshakeReject`).
- **Otherwise the caller is the user.**
- Identity is fixed for the connection's lifetime, decided once at the handshake.
- **No escape detection.** A process that deliberately calls `setsid` and drops its token is treated as the user. Walking the parent-process chain to catch that was considered and left out: it costs a process-table walk per connection for a case that takes deliberate evasion.
- **Agents from before tokens existed** (running across the upgrade or a daemon restart) have no token in their environment, but they are still identified by their session. They have no `fleet.parent`, so they are roots.

## Consequences

- The scope rules of 0033/0034 hold against an agent that merely forgets or unsets its token, which is the realistic failure. They are not a sandbox: a determined same-uid process can still act as the user. That calls for OS-level isolation (another uid, a container), which is out of scope.
- An adopted agent after a daemon restart (0032) keeps its pid and session, so its identity survives the handoff without re-reading tokens.
- Session ids are reused only after the leader exits and its pid is recycled. fleetd only matches sessions of agents it knows are running, so a reused pid maps to nobody.
- Linux and macOS both provide the peer pid; no extra dependency (tokio already exposes it).
