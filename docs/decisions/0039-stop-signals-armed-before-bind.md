# 0039 — Stop signals are armed before the socket is bound

**Status:** accepted · 2026-11-15 (issue #23)

**Amends:** [0032](0032-daemon-restart-handoff.md), known limits (a SIGTERM during a freeze).

## Context

A SIGTERM sent as soon as `fleetd.sock` appeared sometimes did not stop the daemon. The handlers were installed with tokio's `signal()` at the top of `Daemon::serve`, after `startup` had bound the socket. That left two windows:

1. **Bind until `serve`.** The default disposition applied, so the signal killed the process outright. No clean shutdown ran, and the socket file was left behind. The new test `crates/fleetd/tests/early_signal.rs` reproduced this in 72 of 200 starts.
2. **Inside tokio's `signal()`.** Tokio installs its process handler (`signal_enable`) and only then subscribes the stream (`register_listener`). A signal that the driver broadcast between the two reached no listener. The handler had already replaced the default action, so the process went on serving and the signal was lost. This is the hang the #16 A/B harness saw.

## Decision

- fleetd installs its own `sigaction` handlers for SIGTERM, SIGINT and SIGHUP (`crates/fleetd/src/stop_signal.rs`). They are armed in `main` before `startup` binds the socket, and no tokio runtime is needed for that. Tokio's `signal` is no longer used for them.
- The handler is async-signal-safe and keeps errno. It records the first stop signal in an atomic and writes one byte to a non-blocking, close-on-exec socketpair. The byte stays there until `serve` reads it, so a signal received at any point after arming stops the daemon once it serves, through the normal shutdown. The write end is never closed while the handlers are armed, so a late signal cannot write to a reused fd.
- Arming happens after the handoff is taken or abandoned, because abandoning closes every inherited fd.
- When another fleetd holds the lock, `main` disarms the handlers (back to `SIG_DFL`) before the upgrade prompt or the `fleet daemon upgrade` exec, so Ctrl-C still works there.
- Exec resets caught handlers to their defaults, so agents, the login-env capture and a restart's new binary all start with the default dispositions. Nothing changes for children.

### Restart (0032)

A restart's freeze runs while the serve loop waits for it. Before this change, a SIGTERM during the freeze sat in tokio's stream and was lost when the process exec'd. Now the restart checks the recorded signal twice: once with the agents frozen, before the record is written, and again just before `execve`. If a stop signal has arrived, the restart gives up with "fleetd is stopping (stop signal …)". Everything is undone, and the serve loop then stops the daemon as usual. Test: `stop_signal_during_freeze_stops_instead_of_exec` (it uses the `FLEETD_TEST_FROZEN_DELAY_MS` hook, debug builds only).

The re-exec'd daemon arms its handlers in `main` like any other start, before it serves the inherited listener.

## Known limits

- A signal can still arrive in the few instructions between the last check and `execve`, and that signal is lost.
- In the new image, a stop signal that arrives between `execve` and arming gets the default action. That kills the daemon, and with it the handed agents. Blocking the signals across the exec would close this window, but a restart onto an older fleetd, which never unblocks them, would then be unstoppable by SIGTERM. That trade was rejected.
- A signal sent before arming, early in `main`, still gets the default action. The process dies before a client could have seen its socket.
- `stop_signal::arm` runs in `main`, not in the benchmarked `startup`. It costs one socketpair and three `sigaction` calls.
