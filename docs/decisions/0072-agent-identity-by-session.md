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

## Amendment — 2026-11-16, while building it (M3-A, #102)

Building it showed two things the decision above did not say, and left the rest to the builder.

**Changed or added:**

- **A root shell is the user.** The GUI's terminals are agents too (`kind=shell`, [0071](0071-kill-escalates-and-shells-are-not-agents.md)), so "session first" would have scoped the user's own `fleet` typed in a GUI terminal to that shell's (empty) subtree. A `kind=shell` agent **without** `fleet.parent` therefore resolves to the user, by its session and by its token. A shell an agent spawned has a parent and stays that agent. Agents cannot make a root shell: every spawn of theirs gets `fleet.parent`, and they cannot relabel.
- **An unknown token is no token.** A token that matches no running agent (another daemon's, for `fleet --state-dir …` from inside an agent, or a revoked one) is ignored, and the session decides. Refusing it would break every `fleet` call from an agent or a GUI terminal to a second daemon (and the test suites). Only a valid token naming a different agent than the session is a conflict.

**Builder's decisions:**

- **Storage.** A new table `agent_tokens(agent_id → agents ON DELETE CASCADE, digest BLOB UNIQUE)` (schema v8), not a column. A token is stored before the process starts; the lookup only matches agents in state `running`; exit and lost-marking delete the row. Handoff keeps the rows of agents it keeps, and those keep their pids, so both paths survive `fleet daemon restart`.
- **Proto.** `Hello.agent_token = 40` (empty: none), `Event.spawn_refused = 40`, `SpawnRefused { parent, reason }`, `parent` being the agent that asked. No new request kinds.
- **Where things live.** The tree, narrowing, quotas and the `[node]` reader are in `fleet_node::scope`; tokens in `fleet_node::token`; the per-request table in `fleetd/src/scope.rs`. `server.rs` only calls into them, and the user's requests skip the check entirely (no await on the user's path).
- **D(a) is strict:** an agent is not its own descendant, so it cannot `kill`/`screen` itself through fleetd. A restarted parent keeps its children: `fleet.parent` is resolved through the replacement records.
- **A connection caches** the agents it has found in its caller's subtree; an agent never leaves a subtree (`fleet.parent` cannot change), so only a miss reads the store.
- **Narrowing.** The child gets the parent's labels except `name` and `fleet.*`, plus the requested ones; a requested key the parent has with another value is `forbidden:`. `fleet.*` in a request stays the existing reserved-key error.
- **Quotas.** `max_depth` bounds the child's depth (root 0), so with 3 an agent at depth 3 cannot spawn; `max_children` counts running children; `max_agents` counts all running agents, the user's included. A spawn gate serialises agents' quota checks with their row inserts, so two spawns cannot take the last slot. An agent's **restart** of a descendant counts like a spawn under the restarted agent's parent. The config reader is a small `key = integer` reader of the `[node]` table, not a TOML parser (fleetd needs three numbers; the rest of the file is the GUI's), and logs lines it cannot use. There are no selectors, so 0052's "reject a `name` selector" has nothing to apply to yet.
- **Refusals.** `forbidden: …` messages for: a target outside D(a) (also for an unknown id, so ids outside the subtree reveal nothing), `SetLabels` (`label`, `rename`), `ReadLogs`/`StopLogs`, `RestartDaemon` (`daemon restart`, `upgrade`). `Detach` is allowed (it only touches the connection's own subscription). `ForgetAgents` with ids checks each; with `exited_only` it forgets only the caller's exited descendants.
- **CLI.** Sends `$FLEET_AGENT_TOKEN`; exits **3** for `forbidden:` (the restart path returns it as an error, not a failed restart). Other errors keep exit 1 for now; the 1/2 split of the spec's exit-code table is left to the commands wave 2 adds. `fleet skill` is not refused: it only writes files an agent could write itself, and the CLI cannot tell an agent from a root shell without asking fleetd.
