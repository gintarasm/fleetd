# fleetd restart into a new version, agents surviving — design

**Status:** approved in conversation 2026-11-15 · **Implements:** ADR 0013's "v2: agents survive upgrades" (in a different form, see Approaches) · **Supersedes:** ADR 0018 § Daemon shutdown, for a requested restart only.

## Why

The user rebuilds fleetd often. Today the running daemon keeps the old binary forever (`fleetd --stdio` starts it in the background and it never stops), and the only way onto a new build is `pkill fleetd`, which ends every agent (ADR 0018: no PTY handoff in v1). They want a one-step "restart into the new version" from the CLI and the GUI, with running agents carrying on as if nothing happened.

They also want agents, later, to survive a fleetd **crash** or kill. That is not built now, but this work must leave the code shaped for it.

## Goals

- `fleet daemon restart` and a **Restart fleetd** item in the GUI's status (menu bar) menu replace the running daemon with a new binary.
- Every running agent keeps running across the restart: same process, same id, same terminal, same screen, same history, its exit code still recorded when it later exits.
- No output byte is lost or duplicated across the restart: what the agent wrote before, during and after shows up exactly once in the grid, the history and attached clients' streams.
- Clients see a short disconnect (about a second) and reconnect on their own; the GUI's panes come back attached to the same agents.
- The GUI tells the user when the daemon is older than the binary it would start ("fleetd update available"), and offers the restart there.
- A broken new binary does not cost the agents: the restart is refused and the old daemon keeps running.
- The node's ownership of an agent's terminal sits behind one interface, so a later "holder process per agent" (crash survival) replaces its implementation without touching the server, protocol or clients.

## Non-goals

- Surviving a fleetd crash, `kill -9`, or a new binary that passes the check and then dies while restoring. In those cases agents are lost, as today.
- Remote nodes upgrading themselves or each other: uploading binaries, or noticing a remote update (M5, `docs/plan.md`). A remote daemon can still be restarted onto the binary already at its own path (see SSH targets).
- Downgrades across handoff-format versions (a newer format the old binary cannot read is simply refused by the check).
- Keeping client connections open across the restart. They drop and reconnect.

## Anti-goals (a failure even if it technically works)

- A restart that silently loses agents, or leaves them running untracked (the stale-session leak ADR 0013 exists to prevent).
- Any measurable cost on the output hot path. The perf gate (ADR 0014) applies unchanged: `daemon_flood50_*`, `input_echo_flood50_ns`, `input_to_render_*`, `terminal_feed_*`, `frame_decode_*`, `fleetd_idle_rss_bytes`, `fleetd_startup_instructions`. A regression stops the work and goes to the user.
- A restart that needs the user to do anything with their agents afterwards.

## Approaches considered

1. **Replace in place (chosen).** The daemon `exec`s the new binary in its own process. The process id stays, so agents stay its children and `waitpid` still yields exit codes; the PTY master fds, the listening socket and the instance lock pass through `exec` simply by not being close-on-exec. State that lives only in memory (each agent's screen, size, labels in flight) is written to a handoff record the new binary reads at start. Small runtime cost (none on the hot path), medium build. Weakness: once `exec` succeeds there is no way back, so a pre-flight check of the new binary is the only guard.
2. **Hand fds to a separate new process over a unix socket (SCM_RIGHTS)**, as ADR 0013 sketched. The old daemon could abort if the new one fails. Rejected: the new process is not the agents' parent, so it cannot learn their exit codes (neither macOS nor Linux report a non-child's exit status), and it is no safer than approach 1 with a pre-flight check.
3. **A holder process per agent** (tmux-style): the holder owns the PTY; fleetd talks to holders. Survives crashes too. Deferred (see Preparing for crash survival): much bigger, changes how agents start, and adds a hop to every output byte.

The user chose 1 now, with the code prepared for 3.

## How it works (decided)

### Requesting a restart

- New request `RestartDaemon { string binary = 1; }`. Empty `binary` means "the binary the daemon itself runs from" (the path in its `HelloOk.binary`), resolved by the daemon at restart time, so a file rebuilt at that path is picked up; if that path no longer exists the check refuses it, with no `PATH` fallback *(user's decision, 2026-11-15; this replaced "the `fleetd` on the daemon's `PATH`")*. *(Call, from the builder check:)* it is answered only once the pre-flight check **and** the freeze have succeeded, immediately before `exec`, with `RestartingDaemon { uint32 agents = 1; }` (the number of agents in the handoff record). Any failure before that point is answered with an `Error`, and the daemon resumes (see Failure before exec).
- **Every daemon start has an instance token**, a random `u64`, sent in `HelloOk` *(call)*. A daemon started from a handoff also keeps a **restore summary** until the next restart: the old token, and the agent ids kept, failed and (on fallback) ended. It is reported in `HelloOk`, or in a new response to a status request; the builder decides which. A client tells the outcomes apart like this:
  - **Same token after reconnecting:** the `exec` never happened, so the restart failed. The summary names the reason.
  - **New token with a full summary:** success.
  - **New token with a fallback summary:** the agents were ended.
- `fleet daemon restart [--binary <path>]`: sends the request, waits for the reconnect with the normal client retry (bounded to 10 s), then prints one of these:
  - `fleetd restarted: N agents kept`
  - `fleetd restarted: N kept, M could not be restored and were ended (ids …)`. Exit code 1.
  - `fleetd restart failed: <reason>; the old fleetd is still running`. Exit code 1.
- GUI: **Restart fleetd** in the status menu (core `status_menu`), and on the update notice (below). The GUI passes the `fleetd` it would itself launch (`core::launch::find_tool("fleetd")`, the same rule it uses to start one). While the daemon is restarting the GUI shows its usual reconnecting state; once reconnected, panes re-attach as after any reconnect.
- **SSH targets** *(user's decision)*: the GUI offers **Restart fleetd** for an SSH target too, sending an empty `binary`, so the remote daemon restarts onto its own binary's path, as now on disk (for example, after a `cargo install` on that machine replaced it). The "update available" check runs for the local target only; for an SSH target, the GUI never compares binaries and never shows the notice.
- **A daemon too old to know the request** answers "empty request"; the CLI and GUI say "this fleetd is too old to restart itself; stop it once (`fleetd --force-stop`, or `scripts/install.sh --force-stop`) and the next client starts the new one". (The running daemon on the user's Mac is such a daemon: the first move onto this feature costs one last agent-ending restart.)

### Pre-flight check (before anything irreversible)

1. Resolve `binary` to an absolute path; it must be an executable file.
2. Run `<binary> --handoff-check` with a short timeout (2 s). It prints the handoff-format versions it can read, one line, and exits 0. Anything else — not executable, non-zero, timeout, no common version — refuses the restart with an `Error` naming the reason. Nothing has changed; the daemon carries on.
3. Refuse if a restart is already in progress, or if an agent spawn or agent restart (M2c) is mid-flight (wait for those to settle, bounded, then refuse).

### Freezing the node at a clean boundary

*(Call, from the builder check:)* the freeze runs under a 3 s deadline. Every step below has to be undoable. If the deadline passes or a step fails, the daemon thaws: readers resume, requests are accepted again, and the restarting client gets an `Error`. Requests stuck on an agent, such as an `Input` blocked on a full input queue, don't hold the freeze. They are answered with an `Error` ("fleetd is restarting"), and their unsent bytes are dropped, as a lagged write is today.

- Stop accepting connections and requests. Requests already being handled finish; new ones get an `Error` ("fleetd is restarting").
- Stop every PTY reader thread **at a point where it holds no unprocessed bytes**: a reader may not be stopped between `read()` returning and its bytes reaching the sink (grid, history ring, subscribers). Whatever the agent writes after that stays in the kernel's PTY buffer and is read by the new daemon. This is how "no byte lost or duplicated" is met. *(Builder's choice of mechanism; one way: the reader waits in `poll` on the PTY and a per-node stop pipe instead of a blocking `read`. Whatever is chosen must not regress the flood benchmarks.)*
- An agent that exits during the freeze is recorded through the normal exit path if its reader already saw EOF; otherwise it is still a zombie child at `exec` time and the new daemon reaps it (below).
- Flush every history ring and scrollback file; finish any in-flight store write.

### The handoff record

- One record, versioned (`handoff_version`, starting at 1), written to a file in the state directory (mode 0600) whose fd is passed through `exec`; the new daemon reads and deletes it. *(Format is the builder's call — protobuf in `fleet-proto` fits the project; it must be versioned and forward-checked by `--handoff-check`.)*
- Per running agent: id, pid (and process group), how to reach its terminal (**an fd number now; later a holder socket path** — see Preparing), terminal size, and the screen as the existing reattach snapshot bytes (ADR 0018 § Snapshot as ANSI). History and labels already live in the store and ring files and are reopened, not copied.
- Daemon-wide: the listening socket's fd, the instance lock's fd, the retained exited snapshots (last 64 exits, ADR 0018) — or drop those and let attach fall back to the empty-snapshot path; builder's call, noted in the ADR.
- Every fd named in the record has close-on-exec cleared just before `exec`; every other fd keeps it.

### Failure at or after exec

If `execve` itself fails (for example, the binary was replaced after the check), the daemon thaws and resumes, exactly as when the freeze fails. It also records the failure, so a reconnecting client sees the same token and the reason. The client already got `RestartingDaemon`, so it learns of the failure from the token, as described above.

### The new daemon starting from a handoff

- `fleetd` started with the handoff fd skips the normal "mark running as lost" step for the agents in the record (and only those: anything else still `running` in the store is lost as today).
- For each agent: build a fresh grid of the recorded size, feed it the snapshot bytes, reopen its history ring for append, start its reader on the inherited PTY fd, and reap its exit with `waitpid(pid)` (no portable-pty `Child` exists for an adopted agent).
- Adopt the listening socket and lock; start serving. Clients reconnecting now see the agents as running, attach, and get a snapshot as usual.
- If the record is unreadable or restoring fails: log it, and fall back to the normal start (agents in the record are signalled like a shutdown, then marked as exited/lost). A half-restored daemon is worse than a clean one.

### Screen fidelity

The replay carries what a reattach carries (ADR 0018 § Snapshot as ANSI) and loses what a reattach loses: scroll region, character sets, tab stops, hyperlinks, soft-wrap markers, kitty flag stacks below the top. Accepted: these are already the limits clients live with. A program that relies on its scroll region redraws it on its next full repaint.

### "Update available"

- The daemon reports its binary's identity in `HelloOk.server` (or a new `HelloOk` field — builder's call): the executable's path and file identity at start (device, inode, size, mtime). Clients compare it with the binary they would launch. Different → "fleetd update available". No build-time stamping, so every rebuild is detected, and nothing is computed on the daemon's start path beyond one `stat`.
- GUI: a small notice in the sidebar footer or status menu with a **Restart** action. The CLI's `fleet status`-style output (if any) may show it; not required.

### Logging

Log the restart request, the check result, the number of agents handed over, the new daemon's restore result per agent (id, ok / failed), and the time from freeze to serving. Never log the handoff record's contents.

## Preparing for crash survival

- **One interface for "an agent's terminal".** Today `fleet-node::pty::PtyHost` spawns, reads, writes, resizes, signals and reaps. Split what the node needs into one interface (spawn, adopt-from-handoff, write input, resize, signal, exit notification, describe-for-handoff), implemented now by the in-process PTY. A later holder-process implementation replaces it; `Node`, the server, the protocol and clients do not change. The output path stays as it is now (reader → `OutputSink`); the interface must not add dynamic dispatch or copies on it.
- **The handoff record already carries "how to reach the terminal"** as a tagged choice (fd now; holder socket later), so the holder version reuses the record and the restart path unchanged.
- **Not built now:** the holder process, crash survival, marking agents as "survived" after a crash.

## Constraints

- macOS (the user's daily machine) and Linux (the CI and future remote nodes) both work. `exec` with other threads running is fine on both; the record must be complete before it.
- The fd budget (ADR 0018) is unchanged: an adopted agent holds the same fds as a spawned one.
- Protocol: adding `RestartDaemon` is backwards compatible within protocol v2 (an old daemon answers "empty request"). No `PROTOCOL_VERSION` bump.
- The GUI core stays toolkit-free (ADR 0029).
- `Frame` stays at 88 bytes (`frame_stays_small`, PR #14): box the new request if it is large (it holds one string, so it should not be).
- A new ADR (0032) records the decision and amends 0013 and 0018.

## Tests that must exist

- Integration (fleetd crate): spawn agents (an interactive shell; one that writes continuously; one that exits mid-restart), restart the daemon onto the same binary, then: same ids running, input works, the screen matches the pre-restart snapshot, the exiting agent's exit code is recorded, a new instance token and a full restore summary are reported. **No gap or duplicate** *(call, from the builder check)* is checked on the node's side, where it is exact: the continuous writer prints numbered lines, and after the restart its scrollback ring and history pages hold every number once, in order. A re-attach after the restart gets a snapshot, history tail and stream that line up, as any re-attach does. The protocol gets no resume offset: clients re-attach after a reconnect as they always have.
- Freeze and exec failures: an input stuck on an agent that doesn't read, and an exec of a binary replaced after the check. Each one thaws and keeps serving, with the same token and the reason in the summary.
- Pre-flight refusals: missing binary, non-executable, `--handoff-check` failing or timing out, unknown handoff version, restart already running. Each leaves the old daemon serving with agents untouched.
- Restore failure fallback: a corrupt record → normal start, agents in it ended and recorded, nothing left running untracked.
- CLI: `fleet daemon restart` against a too-old daemon prints the "stop it once" message.
- GUI core: the status-menu entry (local and SSH targets), the update-available state from mismatched identities (local only, never for SSH), reconnect-and-reattach after a restart (fake server).

## Left to the builder

- The freeze mechanism for readers, the record's encoding, how the handoff fd is named to the new process (argument or environment), whether retained exited snapshots are carried, where the update notice sits in the GUI, and the exact CLI wording beyond the messages quoted above.
