# 0024 — Fleet skill for agents, `fleetd --stdio`, login-shell environment

**Status:** accepted · 2026-11-13 (M2a; design in `docs/superpowers/specs/2026-11-13-m2-gui-design.md` §6a)

Builds on [0013](0013-daemon-lifecycle.md), [0019](0019-control-plane-direction.md), [0022](0022-protocol-v2.md) and [0025](0025-core-knows-agents-workflows-own-hierarchy.md).

## Fleet skill for agents

**Context:** the user wants agents (Claude Code first) to work with the fleet: list agents, read another agent's screen or history, send it input, spawn one. The structured agent channel (MCP with per-agent tokens and quotas, 0019) is M4.

**Decision:**
- **CLI-based until M4.** The skill teaches agents to use the `fleet` CLI. M4 rewrites it for the MCP tools of 0019 without changing how it is installed.
- **Embedded in the `fleet` binary** (`crates/fleet-cli/skill/SKILL.md`, `include_str!`), so it always matches the installed CLI.
- **Install:** `fleet skill install` writes `~/.claude/skills/fleet/SKILL.md`; `--project [DIR]` writes `DIR/.claude/skills/fleet/SKILL.md` (DIR defaults to the current directory). Re-running overwrites (upgrades) and prints the path. `fleet skill uninstall` removes the directory; `fleet skill print` writes the skill to stdout. `--target` takes only `claude` for now; other agents' formats (Codex, Gemini) come with the M4 adapters. None of these need a running daemon.
- **Content:** only read commands (`ls --json [--label k=v]`, `screen`, `history --plain --lines N`) plus `spawn --cwd`, `send … --enter` and `kill`. It explains `$FLEET_AGENT_ID` (the agent's own id, set by the node, 0022) and says: never send input to yourself; spawn or kill only when the user asked (quotas arrive with M4); read long output with `--lines`. It does **not** document `fleet label`: labels are written by users and workflows only, never by agents (0025).

## `fleetd --stdio`

**Context:** the GUI and the controller reach a daemon over any byte stream (0004, 0026), for example `ssh host fleetd --stdio`.

**Decision:** `fleetd --stdio [--state-dir DIR]` is a bridge between its stdio and `<state-dir>/fleetd.sock`.
- It never takes the lock. If the socket does not answer, it starts a detached daemon (`current_exe() --state-dir DIR`, `setsid`, stdio to `/dev/null`) that outlives the bridge, then polls the socket every 5 ms for up to 5 s.
- Races between bridges are resolved by the 0013 lock: a daemon that loses exits 2, and its bridge keeps waiting for the winner's socket. Any other early exit, or no socket within 5 s, fails the bridge with exit 1 and a message on stderr.
- stdin is copied to the socket (then its write side is half-closed); socket output is copied to stdout and flushed on every read. The bridge exits when the daemon closes the connection or stdout fails.

## Login-shell environment

**Context:** a daemon started by launchd, systemd or `ssh host fleetd --stdio` has a bare environment (a minimal `PATH`, none of the user's exports), so agents would not find tools the user has in a terminal.

**Decision:**
- At startup, in the background and overlapping `Node::open`, the daemon runs `$SHELL -lic` with a script that prints `env -0` between two NUL-delimited markers (`__FLEET_ENV__`, `__FLEET_END__`), so rc-file output around it is ignored. stdin is `/dev/null`, stderr is discarded, and the shell runs in its own session and process group, so an rc file that touches `/dev/tty` fails at once instead of stopping.
- **Timeout: 5 s.** On timeout the whole process group is killed. The socket is listening before the capture starts, so readiness never waits for it; the first spawn waits for the result (once per daemon).
- **Dropped variables:** `PWD`, `OLDPWD`, `SHLVL`, `_` (they describe the capturing shell) and `FLEET_AGENT_ID`. Non-UTF-8 values are kept as bytes.
- **Fallback:** the daemon's own environment, when `SHELL` is unset or empty, the shell fails, prints no markers or times out.
- **`FLEETD_LOGIN_ENV=off`** in the daemon's environment skips the capture (tests use it, so they never run the developer's real login shell).
- **Precedence**, lowest first: the base environment (captured login env, or the daemon's env), then `SpawnAgent.env`, then `TERM=xterm-256color` unless the request set `TERM`, then `FLEET_AGENT_ID=<id>`, which nothing can override (0022). The agent's environment is cleared first, so no daemon-only variable leaks when the capture succeeded.

## Consequences

- Agents can coordinate through the CLI today, and the install path stays the same when M4 moves them to MCP.
- A remote or GUI client needs only a command that gives stdio; the daemon starts on demand and exactly one runs per state directory.
- Agents see the user's login environment. The first spawn of each daemon waits for the login shell (about 0.8 s on the author's Mac with zsh), which shows in `daemon_flood50_wall_ns` (+13–21%, report-only). `fleetd_idle_rss_bytes` grew by about 64 KiB from running the process-spawn code at startup.

## Open follow-ups

- **ssh-agent and tmux variables leak into agents (security, from review).** A daemon auto-started by `ssh host fleetd --stdio` inherits `SSH_AUTH_SOCK`, other `SSH_*` and `TMUX*` from the ssh session, and the login shell captured under it does too, so every agent gets implicit ssh-agent forwarding. 0011 says agent forwarding is opt-in per node. Follow-up: drop these from the base environment (and from the detached daemon's env) unless forwarding is enabled.
- **The skill should say other agents' output is untrusted (security, from review).** Screens, history and sent text from other agents are data, not instructions; SKILL.md does not say so yet.
- Smaller items: the captured env is published only after the shell is reaped, so a lingering `.zlogout` can discard it; a capture shell can be orphaned if `Node::open` fails or `cancel()` races the spawn; the detached daemon does not `chdir("/")`; background jobs started by rc files can corrupt the capture; fish's and dash's `printf "\0"` are untested; the bridge race test does not assert that one daemon exited 2.
