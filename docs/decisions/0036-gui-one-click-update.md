# 0036 — GUI: one-click Update fleetd in the titlebar

**Status:** accepted · 2026-11-15 (M2d)

**Amends:** [0032](0032-daemon-restart-handoff.md) § Clients (the sidebar's "fleetd update available" notice). Issue #22.

## Decision

- While `update_available` is true (0032: the local daemon's `BinaryIdentity` differs from the `fleetd` the GUI would launch), the titlebar shows an **Update fleetd** button, left of Clear exited. The sidebar footer notice with its "Restart" link is removed, so the offer appears once.
- A click runs the same restart as the status menu's Restart fleetd (`Session::restart_daemon`): for a local target it names the `fleetd` the update check compared against, so success is what makes the check pass and the button go away; for ssh it sends an empty `binary` (the remote daemon's own path). No rebuild or install from the GUI: `scripts/install.sh` stays the way to install a build.
- While the restart is in flight (`Session::restart_pending`) the button reads **Updating fleetd…**, is disabled, and a further click (or the menu item) does nothing; the session also refuses a second request. One click sends one `RestartDaemon`.
- Outcomes are the shared `fleet_client::restart_outcome` wording, shown as today's toasts (success neutral, partial and failure as errors; a too-old daemon gets `TOO_OLD` with its `--force-stop` instructions). After any outcome the update check runs again: success clears the button; a failure leaves it.
- The decision of what the button shows is `core::titlebar::update_button`, toolkit-free and unit-tested; the session's part is covered in `crates/fleet-gui/tests/session.rs` with a scripted fleetd and a test hook for the on-disk `fleetd` path (`set_fleetd_for_test`, `test-hooks` only).
