# 0027 — Daemon logging: a rotated file and `fleet logs`

**Status:** accepted · 2026-11-13 (M2a, task 15)

Builds on [0013](0013-daemon-lifecycle.md), [0018](0018-m1-protocol-and-node.md) (connection writer and drop policies), [0022](0022-protocol-v2.md) (protocol v2) and [0024](0024-agent-skill-stdio-env.md) (`--stdio`, login env). Serves remote daemons per [0019](0019-control-plane-direction.md) and [0026](0026-node-providers-and-transports.md).

## Context

fleetd reported problems with about 26 `eprintln!`s. A daemon started by `fleetd --stdio`, and later by the GUI or a service manager, has stderr on `/dev/null`, so nothing was visible. The user wants a log file on the daemon's host and a way to read the log over the protocol, so it also works for a remote daemon. Logging must not slow down agents or the output path, which have hard performance budgets ([0014](0014-performance-gate.md)).

## Decision

### Library and format

- **`tracing` + `tracing-subscriber`** with only the `fmt`, `env-filter` and `std` features: no ANSI colours, no `log` bridge, no regex-heavy extras. `fleet-node` depends on `tracing` alone and emits events; only the `fleetd` binary installs a subscriber, so library users and tests without one pay only a disabled-callsite check.
- **Level:** `FLEETD_LOG` holds an `EnvFilter` directive (for example `debug` or `fleetd=debug,fleet_node=info`). The default is `info`. An invalid directive falls back to `info` and is logged as a warning.
- **Format:** one line per event: an RFC 3339 UTC timestamp, the level, the target, the message and `key=value` fields. Strings that come from clients (the client name in `Hello`, programs and paths) are logged with `Debug` quoting.
- **No line injection (backstop):** every formatted event goes through `escape_controls` before it is queued. Any `\n`, `\r` or other control character other than tab or the event's final newline becomes `\n`, `\r` or `\xNN`. So no message or field, including the `Display` text of an error, can start a line of its own. Every line in the file begins with a timestamp.

  ```
  2026-11-13T21:42:45.974000Z  INFO fleet_node::node: agent spawned agent=1 program="/bin/sh" cwd="/home/u/src" labels=["team"]
  ```

### File and rotation

- **Location:** `<state-dir>/logs/fleetd.log`. The `logs` directory has mode 0700 and the files 0600; both modes are also enforced on existing files.
- **Size rotation:** before a write would take the file past 10 MiB, `fleetd.log.1` is renamed to `fleetd.log.2`, `fleetd.log` to `fleetd.log.1`, and a new `fleetd.log` is opened. At most 30 MiB is kept.
- **Several writers.** Each `fleetd --stdio` bridge logs to its own file, `logs/fleetd-stdio.log`, with the same rotation, so a bridge never writes into the daemon's file. Several processes can still append to one file: concurrent bridges, or a daemon that lost the instance lock and logs that before it exits. Files are opened with `O_APPEND`, and a line is written with one `write`, so lines from different processes do not tear. A process rotates only while it holds an exclusive `flock` on `logs/<name>.lock`, and only if the path still names the file it has open (same inode). If another process has already rotated the file, it just reopens the path. So two writers never rotate the same file twice, and no generation is overwritten by mistake. A process that has not noticed a rotation yet keeps appending to `<name>.1` until its own byte count reaches the limit. This is tested in `logging::tests::two_writers_rotating_one_file_lose_no_line_shape`.
- **`fleetd --stdio` never logs to stdout**, which carries the protocol; nor to stderr, apart from its fatal error, which goes to the remote client as before.
- **Fatal errors always go to stderr too.** A fatal startup or serve error, and `fleetd already running` (exit 2), are logged *and* always printed to stderr, whether or not stderr is a terminal. The log file may be unavailable, and a script or service manager may be capturing stderr. A daemon auto-started by `--stdio` has stderr on `/dev/null`, so it is unaffected.

### Never block

- Each event is formatted on the thread that emits it and handed to a **bounded queue of 4096 lines** with `try_send`. A dedicated `log-writer` thread writes the queued lines in batches. When the queue is full the line is dropped and counted. The next line queued successfully is preceded by `WARN fleetd::logging: dropped N lines (log queue full)`, at the place where the lines were lost. Neither a PTY reader nor a tokio worker ever waits for the disk.
- **No events on the output hot path:** nothing in `on_output`, `Terminal::feed`, `Queue::offer`, or the forwarders' per-chunk path. Events happen once per connection, request, agent or failure. The history store's disk-failure warning fires only when the store goes from healthy to broken (a failed write, or a clear that cannot rewrite its files), not on every later clear while it stays broken; a clear that succeeds makes it healthy again, so a later failure warns again.
- At exit, a guard waits for the queue to drain, for at most 1 s.
- **Bounded lines and replies.** A formatted line is cut to 4 KiB (escapes included, at a character boundary) and ends in ` …[truncated]`. A `LogLines` reply holds at most 8 MiB of lines (lines read from the files are cut the same way); the oldest are dropped first, so a large tail never exceeds the 16 MiB frame limit.

### What is logged

| Event | Level | Fields |
|---|---|---|
| daemon start / stopping / stopped | info | version, pid, state dir, protocol; the signal |
| open-file limit | info / warn | the soft limit in effect, or the error |
| another daemon holds the lock (race loser) | info | state dir (also `fleetd already running` on stderr) |
| fatal startup or serve error | error | message (always also printed to stderr) |
| connection opened / closed | info | conn id, client name, close reason |
| handshake rejected, too many connections | warn | conn id, reason |
| spawn / spawn failed | info / warn | agent id, program, cwd, label **keys**; for a failure only the `io::ErrorKind` |
| exit, kill | info | agent id, exit code / signal |
| attach, detach, role change | info | agent id, conn id, primary |
| subscription dropped as `Lagged` | warn | agent id, conn id |
| missed exit events, accept errors, write errors | warn / debug | count / error |
| history: disk write failure, retention deletions | warn / debug, info | dir; agent id or count |
| login-env capture | info / warn | result (`ok`, `timeout` or `failed`), duration, variable **count** |
| `--stdio` bridge start and close, auto-start, auto-started daemon lost the lock | info | pid, state dir, daemon pid |

**Errors whose text can carry env or client data are not logged as text.** portable-pty's spawn error names the `PATH` it searched, which comes from the agent's env. So a failed spawn logs only `error_kind`, and an unreadable agent row only a fixed message. The other logged errors are OS I/O errors, frame-codec errors, parameterised SQLite errors, or text fleetd formats itself. `spawn_failure_logs_no_env_and_cannot_inject_lines` covers this, using a sentinel `PATH` and a program name containing a newline and a forged event.

**Never logged:** environment values (captured or `SpawnAgent.env`), agent arguments, PTY output, input bytes, history lines and label values. Label *keys* are logged. `spawn_and_exit_are_logged_without_env_values` checks this at `FLEETD_LOG=trace`. It uses a sentinel in a fake login shell's environment, an env value, a label value, input bytes and the agent's output.

### Protocol (v2 additions, no version bump)

- `Request.kind`: `ReadLogs read_logs = 11` (`ReadLogs { uint32 tail = 1; bool follow = 2; }`), `StopLogs stop_logs = 12`.
- `Response.kind`: `LogLines log_lines = 9` (`LogLines { repeated string lines = 1; }`): the last `tail` lines (capped at 10 000) of `fleetd.log.2`, `.1` and `fleetd.log`, oldest first. The files are read backwards in 64 KiB chunks, so a short tail is cheap.
- With `follow`, each line written after the tail arrives as `Event.kind`: `LogLine log_line = 5` (`LogLine { string line = 1; }`), until the client sends `StopLogs` (answered by `Ack`) or disconnects. A second `ReadLogs` replaces the follow. The tail is read and the follower registered under the writer's lock, so no line is missed or sent twice between them.
- **Slow followers:** each follower has a bounded queue of 1024 lines that the writer fills with `try_send`. When the queue is full, lines are counted and dropped. The next line that fits is preceded by `… N lines dropped`. The follower is never disconnected for this, and it never blocks the writer or other connections. Its lines then go through the connection's outbox like any other event (0018).
- A daemon without a log file (one that could not open it) answers `ReadLogs` with `Error`.
- Old clients never send these requests, so nothing changes for them. These are additions to `fleet.v1`, and per [0022](0022-protocol-v2.md) additions do not bump the version.

### CLI

`fleet logs [-n N] [-f]` prints the last N lines (default 100); with `-f` it then prints new lines until Ctrl-C. It works over any transport, including `ssh host fleetd --stdio`.

## Consequences

- Failures of an auto-started or remote daemon can be diagnosed with `fleet logs`, or by reading `~/.fleet/logs/fleetd.log` on its host.
- `fleetd_idle_rss_bytes` rises because of the subscriber, the env filter and the writer thread (128 KiB stack). The user accepted this ("logging is worth the tradeoff"). The task 15 report has the numbers.
- A log line costs one formatted `String` and a channel send on the emitting thread. That is fine at the rates above (per connection, request and agent event), and is the reason nothing logs per output chunk.
- A burst of more than 4096 lines faster than the disk takes them loses lines, and the loss is noted. Lines are never delayed.

## Open follow-ups

- A launchd/systemd unit could log to the same file; when it arrives, its stderr should go to `/dev/null` to avoid duplicates.
- The writer holds the shared lock while a `ReadLogs` reads the tail, at most 10 000 lines. Meanwhile new events queue up and are not dropped unless 4096 accumulate.
- Usage errors (`fleetd --bogus`) still go only to stderr: they happen before the state dir, and so the log, is known.
