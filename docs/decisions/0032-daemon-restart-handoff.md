# 0032 — Daemon restart into a new binary, agents surviving

**Status:** accepted · 2026-11-15 (M2c)

**Amends:** [0013](0013-daemon-lifecycle.md) (its "v2: live handoff" is built, as exec in place instead of SCM_RIGHTS) and [0018](0018-m1-protocol-and-node.md) (§ Daemon shutdown no longer applies to a requested restart). Spec: `docs/superpowers/specs/2026-11-15-daemon-restart-design.md`.

## Context

The running daemon keeps its binary until it stops, and stopping it ends every agent (0018: no PTY handoff in v1). Rebuilding fleetd is routine, so the only way onto a new build, `pkill fleetd`, cost the user all their agents each time. Wanted: one step from the CLI, the GUI or the install script, with agents unaffected: same process, id, terminal, screen and history; no output byte lost or duplicated; a failed restart costs nothing.

## Decision

### Exec in place

The daemon `exec`s the new binary in its own process. The pid stays, so agents stay its children and `waitpid` still yields exit codes. PTY master fds, the listening socket and the instance lock pass through `exec` by having close-on-exec cleared just before it. Everything held only in memory goes into a handoff record the new binary reads at start.

Rejected: handing fds to a separate new process over a unix socket (0013's sketch). The new process would not be the agents' parent and could not learn their exit codes on macOS or Linux, and it is no safer than exec plus a pre-flight check. Also deferred: a holder process per agent (see Crash survival).

### Protocol (additive; `PROTOCOL_VERSION` stays 2)

- `Request::RestartDaemon { binary }`; empty `binary` means the `fleetd` on the daemon's `PATH`. Answered with `RestartingDaemon { agents }` only after the pre-flight check and the freeze succeeded, immediately before `exec`. Every earlier failure is an `Error` and the daemon resumes.
- `HelloOk` gains `instance` (a random `u64` per process start), `binary` (`BinaryIdentity`: path, dev, ino, size, mtime of the running executable, one `stat`) and `restore` (`RestoreSummary`: `previous_instance`, `kept`, `failed`, `fell_back`, `restart_error`).
- An old daemon answers "empty request"; clients print "this fleetd is too old to restart itself; stop it once (`pkill fleetd`) and the next client starts the new one". `Frame` stays at 88 bytes (`frame_stays_small`).

### Handoff record

`crates/fleet-proto/proto/handoff.proto`, version 1, written to `<state>/handoff.bin` (mode 0600), its fd passed through exec and named by `FLEETD_HANDOFF_FD` (plus `FLEETD_HANDOFF_PIDS`, the agent pids, so a fallback can end them). The new daemon removes both variables before anything else, so no agent or later restart sees them. A record holds the version, the listener and lock fds, `previous_instance`, `frozen_unix_ms` (to log freeze-to-serving time) and per agent: id, pid, size, the screen as the reattach snapshot, and a `terminal` oneof: `fd` now, `holder_socket` reserved. History and labels stay in the store and ring files and are reopened. The record's contents are never logged. A leftover file is deleted on every normal start, after the lock is taken.

The new daemon checks the record before trusting it: every claimed terminal fd must be a tty and above 2, and no fd or id may appear twice, so a bad record cannot close the listener, lock, log or stdio.

### Freezing

The freeze runs under a 3 s deadline; every step is undoable, so a miss or failure thaws and the requester gets an `Error`.

- Requests stop first, including resize and kill. Requests already running finish; new ones get "fleetd is restarting". A requester that needs a spawn or restart to settle waits, bounded, then is refused ("agents are starting; try again").
- **Readers stop by signal, not by a new read loop.** The first design (read until EAGAIN, then poll, to stop at a known boundary) cost +77% on `flood50_cpu_ns_per_mib` and broke the spec's "no cost on the output hot path". The built design leaves the blocking read loop unchanged. A no-op `SIGUSR2` handler (no `SA_RESTART`) is installed once per process; each reader checks an atomic `frozen` flag before every read; the freeze sets the flag and `pthread_kill`s each reader, re-sending every 5 ms until it acknowledges (closing the check-then-read race). An interrupted `read` returns `EINTR` having consumed nothing, so unread bytes stay in the kernel's PTY buffer for the new daemon. Blocked input writers are registered while inside `write_all` and interrupted the same way; their unsent bytes are dropped with "fleetd is restarting", as a lagged write is today. No `O_NONBLOCK` on the master. Host CPU per MiB: +1.1% (A/B). SIGUSR2 must stay unused by fleetd and tokio; children reset handlers on exec.
- An agent that exits during the freeze is recorded by the normal path if its reader saw EOF; otherwise it is an unreaped zombie and the new daemon reaps it. A live agent whose reader already ended but which is not yet retired is refused ("an agent is exiting; try again") rather than left out of the record.
- Rings and history are flushed and store writes finished before the record is written.

### Pre-flight, refusals and failure

Before anything irreversible: `binary` resolves to an absolute executable file; `<binary> --handoff-check` (2 s timeout, output read with a bounded wait so a grandchild holding stdout cannot hang it) prints `handoff N` for each version it reads, and one must match; no restart is running; no spawn or agent restart is mid-flight. Each refusal leaves the old daemon serving, agents untouched.

If `execve` fails after the answer (the binary changed after the check), the daemon thaws and records the failure in its restore summary (`restart_error`, same `instance`), so the client that already got `RestartingDaemon` learns the reason on reconnecting.

### New daemon and fallback

With a handoff fd the daemon skips "mark running as lost" for the recorded agents only, adopts the listener and lock, and for each agent builds a grid of the recorded size, feeds it the snapshot, reopens its history ring and starts a reader on the inherited fd. Exit is reaped with `waitpid(pid)` (no portable-pty `Child` exists for an adopted agent). An adopted agent that dies of a signal reports exit code 1, matching portable-pty for a spawned one.

If the record is unusable or restore fails, the daemon logs it, signals the recorded agents as a shutdown would, marks them exited and starts normally (`fell_back`). A half-restored daemon is worse than a clean one. Ending the agents of a failed restore can delay startup by up to 2.5 s.

### Telling the outcome

One decision, `restart_outcome` in `fleet-client`, is shared by the CLI and the GUI, so both print identical words and agree on success. Success needs positive proof: a new instance whose summary names the old instance as predecessor, nothing failed or fell back, and exactly the announced number of agents kept. Missing summary, another predecessor or too few kept is a partial result (`fleetd restarted: K kept, M could not be restored and were ended (ids …)`, exit 1; "(ids unknown)" when the summary lists none). The same instance with a `restart_error` is `fleetd restart failed: <reason>; the old fleetd is still running`; the same instance without one is still waiting. Wait is bounded at 10 s.

### Clients

- `fleet daemon restart [--binary <path>]`; `scripts/install.sh` runs it after building.
- GUI: **Restart fleetd** in the status menu, local and SSH targets alike (SSH sends an empty `binary`). The sidebar shows "fleetd update available" when the local daemon's `BinaryIdentity` differs from the `fleetd` the GUI would launch (dev/ino/size/mtime, so the same file reached by a symlink is the same build). The check runs on connect and window focus and never for SSH targets.
- `fleetd --upgrade` and `fleetd --force-stop` (and the prompt in a terminal) act when another fleetd holds the lock. To keep fleetd from linking `fleet-client` (binary 6.84 to 6.98 MB and idle RSS about +1%, a gated regression), fleetd only does the cheap same-build check and then `exec`s the sibling `fleet daemon upgrade` (hidden; same directory as `fleetd`, else `PATH`), which shares the restart loop. Without a `fleet` it prints the old message plus "install fleet next to fleetd to upgrade from here". `--force-stop` sends SIGTERM to the pid holding the lock (`lsof`, else `fuser`), never SIGKILL, waits 5 s and names the pid if it stays. Both flags are ignored with `--stdio`.
- `scripts/install.sh` builds, installs and restarts; a daemon too old to restart is left alone unless `--force-stop` is given.

## What a restart loses

- **Screen fidelity is a reattach's.** The replay carries the reattach snapshot (0018 § Snapshot as ANSI) and loses what it loses: scroll region, character sets, tab stops, hyperlinks, soft-wrap markers, kitty flag stacks below the top.
- **Retained exited snapshots are not carried** (the last 64 exits, 0018). Attaching an agent that exited before the restart takes the empty-snapshot path.
- Client connections drop and reconnect (about a second); panes re-attach as after any reconnect. The protocol gets no resume offset.
- A crash, `kill -9`, or a new binary that passes the check and then dies while restoring still ends the agents.

## Known limits

- Between clearing close-on-exec on the listener and lock and the `execve` (the login-env capture can spawn then), a forked child could inherit them. The window is short; closing it needs the capture moved out of that interval.
- A SIGTERM that arrives during a freeze which then execs successfully is lost; the new daemon does not see it.
- A panic in the restart coordinator after the freeze leaves the node frozen while the daemon serves on.
- A requester that disconnects before its answer does not cancel the restart.
- portable-pty reports any death by signal as exit code 1, so adopted agents do too.
- The Bash/Claude Code sandbox silently drops signals sent to processes outside it, so `pkill fleetd` from a sandboxed shell exits 0 and does nothing. Stop a daemon from an ordinary terminal.

## Crash survival: the seam

`fleet-node::host::AgentHost` is the one interface for an agent's terminal: spawn, `adopt`, write input, resize, kill, force-kill, remove, wait; output still flows reader to `OutputSink` with no extra dispatch or copy. `PtyHost` implements it. A holder-process-per-agent host (agents surviving a crash) replaces that implementation; the handoff record's `holder_socket` slot already names how to reach such a terminal, so `Node`, the server, the protocol and the clients do not change. Not built: the holder, crash survival, marking agents as survived. It stays in `docs/plan.md` "Later".

## Performance

The perf gate (0014) applies unchanged. The first freeze design failed it (above) and was replaced. Idle RSS (`fleetd_idle_rss_bytes`) is **+125 KiB (+2.6%)**, accepted by the user on 2026-11-15; it is resident `__TEXT` from the restart code, which is kept synchronous and lazy in its own modules. Going lower needs link-order control; `lto = "fat"` with `codegen-units = 1` idled about 290 KiB below the old baseline and shrank the binary from 6.83 to 5.17 MB but moves every benchmark and compile time, so it is deferred to its own PR. If CI `perf-wall` fails on this change, only the user adds the `Perf-Approved` trailer, baseline and perf-log entry.

Test hooks (forcing a freeze timeout or exec failure) exist only under `cfg(debug_assertions)` and an environment variable; a `test-hooks` feature cannot reach the binary `cargo test --workspace` builds. Release builds never carry them.

## Consequences

- The first move onto this feature from a pre-0032 daemon ends its agents once: it cannot restart itself. `scripts/install.sh --force-stop` or `fleetd --force-stop` does that stop; after it, restarts keep agents.
- Every restart is an `exec`: a new binary that passes the check but is broken fails after the point of no return. The check is the only guard, hence its version handshake.
- An old client against a new daemon is unaffected; a new client against an old daemon says it is too old.
