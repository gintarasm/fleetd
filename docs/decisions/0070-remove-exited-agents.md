# 0070 — Remove all exited agents at once

**Status:** accepted · 2026-11-16 (issue #89)

**Amends:** [0030](0030-agent-restart-and-forget.md) (the CLI's `rm --exited` output and filter flag) and the M2c spec's "Clear exited" (`docs/superpowers/specs/2026-11-14-m2c-daily-drive-design.md`: no confirmation).

## Context

Exited agents pile up and are removed one by one. The protocol already has the bulk request: `ForgetAgents { exited_only, selector }` (0030) picks every exited or lost agent matching a label selector under the node's agents lock. The CLI exposed it as `fleet rm --exited --selector k=v`, printing only a count; the GUI as a titlebar button and status-menu entry with no confirmation, and a palette entry with no menu-bar equivalent.

## Decision

- **One request, no CLI loop.** `fleet rm --exited` sends a single `ForgetAgents { exited_only: true, selector }`. fleetd chooses the agents under its agents lock, so an agent that is running, spawning or being restarted at that moment is never removed. Listing in the CLI and then forgetting ids one by one would add a race and N round trips for nothing.
- **CLI filter is `--label k=v`, repeatable,** as in `fleet ls` (every pair must match). `--selector` stays as a hidden alias so existing scripts keep working. `--label` conflicts with explicit ids (clap otherwise drops `requires = "exited"` when ids are given, and the filter was silently ignored).
- **CLI output is the removed ids, one per line** on stdout (nothing when none), instead of `removed N agents`, so scripts and agents can see exactly what went. `fleet rm <id>…` is unchanged: silent on success, refused ids on stderr, exit 1.
- **GUI: one action, `RemoveExited`, titled "Remove Exited Agents…",** offered in the ⌘P palette and the app menu (fleet ▸ Remove Exited Agents…). It asks first, naming how many will go ("Remove 3 exited agents?"), counted from the GUI's current list; with none ended it does nothing. On Remove it sends the same `ForgetAgents { exited_only }` with no selector. The palette entry is now a plain action (`command_action` is total).
- The titlebar trash button and the status-menu "Clear exited agents" keep their immediate behaviour for now: their tooltip/visibility already shows the count, and the sidebar and status code are being reworked in parallel PRs. Folding them into `RemoveExited` is a follow-up.

## Consequences

- The confirmed count can differ from what fleetd removes (an agent can exit or be removed elsewhere between the list and the request); fleetd's choice is authoritative and every client relists on `AgentRemoved`.
- Scripts parsing `removed N agents` must count lines instead.
