# 0012 — Credential forwarding

**Status:** accepted · 2026-11-13

**Amended by:** [0025](0025-core-knows-agents-workflows-own-hierarchy.md) — grants are scoped by label selectors instead of project/workspace/task.

**Context:** Nodes and agents sometimes need secrets (API keys, tokens) and remote MCP servers need OAuth.

**Decision:**
- **Store:** macOS Keychain on the controller. Secrets never rest on node disks.
- **Grants:** each secret is scoped to node / project / workspace / task.
- **Injection:** at agent spawn the controller sends granted secrets over the SSH channel; the node injects them as env vars or tmpfs files (`/dev/shm`, 0600), deleted when the task exits. Never logged; exact values masked in scrollback.
- **Browser/OAuth flows** (MCP OAuth, `claude login`, …): the adapter/task asks the controller to open the URL in the Mac's browser; the controller sets up an SSH reverse port-forward so the `localhost:<port>` callback reaches the CLI on the node. Tokens land in the CLI's own storage on the node; optionally captured into Keychain for reuse.
- **ssh-agent forwarding:** off by default, opt-in per node (risk: root on the node can use the agent while connected).
