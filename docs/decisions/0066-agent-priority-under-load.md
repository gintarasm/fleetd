# 0066 — Agents at a lower CPU priority, so typing stays fast under a flood

**Status:** accepted · 2026-11-16 (issue #60; the setting is off by default until the latency and throughput are measured, see Performance)

**Builds on:** [0002](0002-daemon-and-gpui-client.md) and [0003](0003-node-controller-roles.md) (the node owns the agent processes, so it is the one place that can set their priority), [0032](0032-daemon-restart-handoff.md) (what a restart keeps), [0033](0033-satellite-milestone.md) (fleets of up to 30 agents per node, agents spawning agents). Evidence: #59.

## Context

With 50 agents flooding output, the keystroke echo of an attached agent stalls 0.5 to 25 ms in bursts (#59). Alone, the echo path is fast: p90 94 µs with 4 flood agents. With 50, the machine is 99% busy: fleetd uses about 4.6 cores forwarding output and the 50 agents about 5 cores, half of it in the kernel doing PTY I/O. An echo crosses the agent (`cat` in the benchmark), a PTY reader thread, a tokio worker and the client, and each hop can wait a scheduler timeslice. Every agent and every fleetd thread runs at the same priority, so the scheduler has no reason to prefer the path the user is waiting on.

This is not only a benchmark: a burst of builds or log output from a few agents in a fleet of 30 is normal.

## Options

### 1. Lower the agents' priority at spawn

The node raises each agent's nice value right after it starts. fleetd, its PTY readers, its tokio workers and every client stay at the normal priority, so under saturation the forwarding path and the clients win the CPU, and the floods get what is left.

- **Mechanism (macOS and Linux):** a lowered agent is started as `nice -n <N> -- <program> <args>` (`/usr/bin/nice`, else `/bin/nice`). `nice` lowers itself and `exec`s the agent in the same process, so the agent is lowered before its first instruction, and everything it forks inherits the value: builds and tools an agent starts are lowered too. Raising one's own nice value needs no privilege. The pid, the PTY, `argv[0]` and the environment are the agent's as before; `ps` shows the agent, not `nice`.
- **Why not `setpriority` from fleetd after the spawn** (the first design): portable-pty builds the `Command` itself and offers no `pre_exec` hook, so the value can only be set from outside. Tried: `setpriority(PRIO_PGRP, pid, n)` right after `spawn_command` returns succeeds, reads back as `n`, and is back at 0 a few milliseconds later, every time, for `/bin/cat` as for `/bin/sh` (macOS 26). The likely cause: portable-pty's fork-and-exec returns as soon as the child's `execve` has closed the close-on-exec error pipe, before the `execve` completes, and macOS drops a value set in that window. (`std::process::Command` without `pre_exec`, which uses `posix_spawn`, and Python's fork-and-exec both kept the value in the same test.) Setting it 50 ms later sticks, but any fixed delay is a race under exactly the load this is for. `nice(1)` has no such window.
- **A missing program:** `nice` would report it only by exiting 127, after the spawn succeeded. So for a lowered agent the node first looks the program up the way portable-pty does (a path with `/` relative to the agent's cwd, else the agent's `PATH`), and a missing one fails the spawn as before. An unlowered agent's spawn is unchanged.
- **Without a `nice` binary** the agent runs unlowered (no error); both macOS and every mainstream Linux ship one.
- **Linux:** the same `nice` wrapper. With `CONFIG_SCHED_AUTOGROUP` enabled (common on desktop distributions), CFS shares CPU between *autogroups* (one per session) before nice applies inside one, and every agent is its own session; a per-process nice value then does nothing against other agents or fleetd. So on Linux the node also writes the nice value to `/proc/<pid>/autogroup`, best effort: the file is absent without autogroup, and an error is ignored. `SCHED_BATCH` and a cgroup were considered: `SCHED_BATCH` alone does not lower the CPU share, and a cgroup needs a delegated cgroup tree (systemd), which a `fleetd` started from a shell does not have. Both can come later behind the same setting.
- **macOS QoS instead of nice:** `PRIO_DARWIN_BG` / `taskpolicy -b` (background QoS) also throttles disk and network I/O and confines the process to the efficiency cores. A `cargo build` in an agent would crawl even on an idle machine. Rejected as the default; nice changes nothing until the CPU is contended. The setting leaves room to add a `background` level later.
- **Cost:** nothing per byte. A lowered spawn costs a `getpriority`, a `PATH` lookup and one extra `exec` (of `nice`), plus a `/proc` write on Linux; an unlowered spawn, including every spawn while the setting is off, is unchanged.

### 2. Raise the interactive path

Give the threads that carry client input and attached agents' output a higher QoS (`pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE)` on macOS), or move attached-client I/O to a small dedicated runtime.

- Input and output for every client share the tokio workers with the forwarding of all 50 agents. Raising them means either raising all workers (which then also prefers the floods' forwarding, so nothing changes between agents) or a second runtime for attached connections plus a hand-off between runtimes on the output path, which is gated (0014) and costs a copy or a wake-up per chunk.
- The PTY reader of the attached agent is one thread per agent; it could be raised while attached. Its echo still waits for the agent itself, which is at the same priority as the floods.
- Raising thread priority above the default on Linux needs `CAP_SYS_NICE` (or `RLIMIT_NICE`); macOS QoS works unprivileged.

Not chosen now: more code, a change on the output path, and it does not help the agent process itself. It is the follow-up if the measurement shows option 1 is not enough.

### 3. Boost the focused agent

While a client types into an agent, raise that agent (and its reader), and drop it back after a quiet period.

- **Nice cannot go back down without privilege.** Lowering a nice value needs root on macOS and `CAP_SYS_NICE` or `RLIMIT_NICE` on Linux. So a boost has to be "lower everything else and leave the focused agent alone", which with option 1 means never lowering it in the first place. A reversible boost needs macOS `PRIO_DARWIN_BG` (rejected above) or Linux autogroup nice, which has the same rule.
- It needs a definition of focus across clients (below), a timer per agent and a race against the agent's own children.

Rejected. Its useful part, keeping the agent the user works in at full speed, is the per-agent opt-out of option 1.

## Decision

**Option 1**, behind a node setting that is **off by default**, with a per-agent override.

### The setting

- **`FLEETD_AGENT_NICE`** in the daemon's environment, following `FLEETD_LOGIN_ENV` (0024). Unset, empty, `off` or `0`: agents keep fleetd's priority (today's behaviour). `1` to `19`: each agent runs at fleetd's own nice value plus this (capped at 19), like `nice -n`. Anything else is logged as a warning at start and treated as off. It is read once at start; a restart (0032) execs with the same environment, so it carries over.
- The environment is the only place the daemon takes settings from today. When `~/.fleet/config.toml` lands (the quotas of 0033), the setting moves there as `[agents] nice = N`, the variable kept as an override.

### Per agent

`SpawnAgent` gains `AgentPriority priority = 9`:

- `AGENT_PRIORITY_DEFAULT` (0, what every existing client sends): the node's setting.
- `AGENT_PRIORITY_NORMAL`: never lowered, whatever the setting. The opt-out for an agent that needs full speed, e.g. the user's own long build.
- `AGENT_PRIORITY_LOW`: lowered even when the setting is off, by the setting's value or 10 when it is off. Lets one flood agent be lowered while the default stays off, and is what the benchmark needs to compare the two.

The node stores the choice with the agent (store schema v4, a `priority` column, default 0), so restarting an exited agent (0030) keeps it. `fleet spawn --priority normal|low` sets it. `AgentInfo` does not carry it yet; the GUI keeps sending the default.

### The questions in #60

- **What "interactive" means with several clients attached.** Under option 1 nothing has to know: fleetd and every client stay at normal priority, and only agents are lowered. Under a later option 3 it would be the agent that last received input from any client (primary or not); observers (0022) do not count, since they cannot type.
- **Does it slow the user's own long builds?** Yes, when the CPU is saturated: a lowered build gets less of a contended CPU (on an idle machine nice changes nothing). That is why the default is off, why `normal` exists per agent, and why a bare `nice` and not background QoS.
- **Linux.** The same call on the process group, plus the autogroup nice when autogroup is on; without it Linux behaves as macOS does. Without a `nice` binary, or without autogroup, the respective part is a no-op.
- **The daemon restart handoff (0032).** Nice is a property of the agent processes, kept across the daemon's `exec` like everything else about them; the handoff record carries nothing new. An adopted agent keeps the priority it was started with, and the store keeps the per-agent choice for a later restart of the agent. A restart onto a binary with a different setting applies it to new agents only.
- **Remote nodes (M6).** Each node applies its own setting to the agents it spawns (for a node under `systemd --user`, set in the unit's `Environment=`). The per-agent priority travels in `SpawnAgent`, so a controller's choice reaches the node unchanged. The node's `nice` lookup is local to it.
- **Agents spawning agents (0033).** A child agent spawned through fleetd gets its own priority from its spawn, not its parent's; one started directly by an agent's shell inherits the parent's nice.

## Performance

Perf CI is off for this batch (0041), so nothing here was measured. **Before the setting defaults to on, the owner, with `PERF_CI` cleared, measures:**

- echo latency of an attached agent during the 50-agent flood, with the flood agents lowered, using the stable statistic from #59. Target: p90 under 1 ms and max under 5 ms on the owner's Mac. Both with the echo agent at `normal` and at the default, since with the setting on the agent being typed into is lowered too;
- `daemon_flood50_wall_ns` and `flood50_wall_ns`: lowering the floods may lengthen the flood's wall time when fleetd and the agents compete for the CPU. Any regression needs the owner's approval (CLAUDE.md, 0014);
- that the idle and spawn metrics do not move with the setting off (nothing changes then), and how much a lowered spawn costs (one extra `exec`).

If the target is missed, option 2 is next. The benchmark for this (a flood with lowered agents) is added with that measurement.

## Consequences

- The default changes nothing for anyone until the owner turns it on.
- A store migration (v3 to v4, `ALTER TABLE agents ADD COLUMN`); an older fleetd refuses the newer database, as for every schema change.
- A lowered agent cannot be raised again by fleetd; changing an agent's priority means restarting it.
