# 0011 — Remote nodes: bootstrap, workspaces, results

**Status:** accepted · 2026-11-13

**Amended by:** [0026](0026-node-providers-and-transports.md) — bootstrap runs over any provider connection; systemd is optional; nodes come from providers.

**Amended by:** [0025](0025-core-knows-agents-workflows-own-hierarchy.md) — workspace clone and fetch-back are git-worktree workflow actions.

**Decision:**
- **Bootstrap** (`fleet node add user@host`): over SSH detect OS/arch, upload a static (musl) `fleetd` to `~/.fleet/bin`, run as a `systemd --user` service with lingering. Version checked on every connect; controller upgrades nodes on mismatch. No root required.
- **Adding a workspace:** point at an existing directory on the node, or clone a repo onto it — from origin (via opt-in ssh-agent forwarding or node-local credentials), falling back to a git bundle pushed over SSH when the node cannot reach origin.
- **Getting results back:** the local repo fetches the task branch directly from the node over SSH (node as a git remote). WIP never has to be pushed to the forge.
- **Agent CLI auth:** detected by the adapter ("auth needed" status); login is done via OAuth forwarding ([0012](0012-credentials.md)).
