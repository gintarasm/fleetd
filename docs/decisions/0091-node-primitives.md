# 0091 — Node primitives: `Exec`, `ReadFile`, `ListDir`

**Status:** accepted · 2026-11-23 (M4 workstream A, issue #161; spec §1 of [the M4 design](../superpowers/specs/2026-11-23-m4-workflows-design.md))

**Builds on:** [0090](0090-m4-workflows.md) (M4), [0025](0025-core-knows-agents-workflows-own-hierarchy.md) (the primitives; agents never call them), [0024](0024-agent-skill-stdio-env.md) (the login-shell env), [0071](0071-kill-escalates-and-shells-are-not-agents.md) (SIGTERM, then SIGKILL after 2 s), [0072](0072-agent-identity-by-session.md) (who is an agent).

**Changes:** [0090](0090-m4-workflows.md) planned proto fields 90–92. `main` already had `SearchOutput`/`SearchHits` at 90 (ADR 0098), so the primitives take **91–93** (`Request`: `exec = 91`, `read_file = 92`, `list_dir = 93`; `Response`: `exec_result = 91`, `file_data = 92`, `dir_listing = 93`). 94 stays free.

## Decision

Three requests the node answers on blocking threads, never on the PTY output path. The wire shapes are in `fleet.proto`; the behaviour is below. The code is `fleet_node::primitives`, plain Rust types and functions, so the workflow host (ADR 0092) calls the same code in process: `exec` (async), `read_file` and `list_dir` (blocking, run them on a blocking thread).

### `Exec`

- **Request:** `argv` (non-empty), `cwd` (absolute), `env`, `clear_env`, `stdin` (≤ 1 MiB), `timeout_ms` (0 = 10 minutes, over 1 hour is an error), `max_output` (0 or above 15 MiB = 15 MiB).
- **Program lookup:** `argv[0]` without a `/` is searched on the child's `PATH`: the one in the request's `env` if it sets it, else the login-shell env's (0024; the daemon's own when no capture succeeded). The node resolves it itself, so the answer does not depend on the platform's `exec` searching the parent's `PATH`. A program that is not found is an `Error`, not a result.
- **Environment:** the login-shell env (the one agents start from) with `env` over it; with `clear_env`, `env` alone. The first Exec waits, as the first spawn does, for a capture still running.
- **Process hygiene:** the child is started in its own session (`setsid`), so it leads its own process group and has no controlling terminal. Stdin is `/dev/null` unless given; given stdin is written without blocking the output reads (a child that reads none of it is not an error). Not a PTY: interactive programs get EOF.
- **Timeout:** SIGTERM to the group at the timeout, SIGKILL 2 s later (0071's escalation); the reply has `timed_out` and the `signal` that ended the child. When the child exits, whatever is left in its group is SIGKILLed, so nothing it started keeps running or holds the output pipes open.
- **Output cap:** stdout and stderr share one budget (15 MiB at most, so the reply fits `MAX_FRAME`). Past it, bytes are read and dropped, and the reply sets `stdout_truncated` and/or `stderr_truncated` for the stream that lost bytes. The child is not stopped.
- **Result:** `exit_code` (-1 when a signal ended it), `signal`, `timed_out`, `stdout`, `stderr`, the two truncation flags, `duration_ms`.
- **Concurrency:** at most 8 Execs run at once on a node; the rest wait in arrival order (a FIFO semaphore). A waiting Exec holds a task, not a thread. A connection may have at most 32 Execs running or queued.
- **Off the request loop:** an Exec runs on its own task, like `Wait`, so its connection keeps serving requests (a reply may arrive after later requests' replies). It is not counted as a request in flight for a restart's freeze, since it can run for an hour.
- **Lifetime:** closing the connection aborts the Exec's task, and dropping the call ends the child the way a timeout does: SIGTERM to its group, SIGKILL 2 s later. A daemon restart (0032) or stop SIGKILLs every running Exec's group first (the children are in their own sessions and would outlive the daemon's exec); the client sees its connection close.

### `ReadFile`

`path` (absolute), `offset`, `max_bytes` (0 or above 15 MiB = 15 MiB). Returns the bytes, the file's `size` and `mtime_unix_ms` when read, and `eof` (the range reached the end). Symlinks are followed. A directory or special file is an error: the file is opened `O_NONBLOCK` and checked with `fstat`, so a FIFO with no writer cannot hang the call. An offset past the end is an empty read with `eof`.

### `ListDir`

`path` (absolute), `max_entries` (0 or above 10,000 = 10,000). Returns each entry's `name` (lossy UTF-8), `kind` (file, dir, symlink, other), `size` and `mtime_unix_ms`, **of the entry itself** (a symlink is reported as a symlink with its own size), unsorted, with `truncated` when more existed. No recursion, no `.` or `..`.

### Who may call them

The user only (a connection with no agent identity, 0072). An agent's `Exec`, `ReadFile` or `ListDir` is refused with `forbidden:` before anything runs: it already has a shell. Workflows call them in process through the host (ADR 0092), which checks their capabilities.

### CLI

`fleet node exec [--cwd D] [--timeout 30s] [--env K=V]… -- argv…` prints stdout and stderr to the matching streams and exits with the child's code: **124** on a timeout (as `timeout(1)`), **128 + signal** when a signal ended it, 3 when refused, 1 on any other failure. `--cwd` defaults to the shell's directory and may be relative. `fleet node cat <path>` writes the file to stdout (reading in 15 MiB steps). `fleet node ls <dir> [--json]` prints `kind size name` per entry, sorted by name (directories end in `/`), or `{entries, truncated}` as JSON. They are the user's; they exist so the primitives can be tested and scripted against a remote node in M6.

### Cost

Nothing runs at startup or idle: no thread, timer or watcher. A call uses the blocking pool for its duration (an Exec polls its pipes with a 50 ms look at its cancel flag and clock only while it runs). `Request.exec` and `Response.exec_result` and `file_data` are boxed in the generated code so `Frame` does not grow. New wall metric `exec_true_roundtrip_ns` (`Exec /usr/bin/true`, median of 50 on an idle daemon, budget 20 ms); no baseline until the user records one.

## Consequences

- A big `git` fan-out from a workflow (`wt ls`) is bounded at 8 children, and waits without holding threads.
- Output over 15 MiB is cut, not an error; callers that need more (a big diff) must ask git for less or read the file with `ReadFile` in ranges.
- An `Exec` reply can be as large as a frame; one connection reading slowly holds that memory until it reads. The per-connection cap of 32 bounds it.
- Not done: progress events for long Execs (M6's clone adds them), stdin for `fleet node exec`, process-group kill of a daemon that is itself killed with SIGKILL (the children then run on; nothing can prevent that from inside).
