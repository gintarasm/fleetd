# 0026 — Node providers and pluggable node transports

**Status:** accepted · 2026-11-13 (user decisions, from a design review)

**Amends:**
- [0004](0004-ssh-transport.md): the transport is "SSH, or a command that gives stdio", with port forwards;
- [0007](0007-plugin-system.md): the remote-provider kind becomes the `node-provider` WIT world;
- [0011](0011-remote-nodes.md): bootstrap runs over any provider connection, with systemd optional;
- [0019](0019-control-plane-direction.md): adds an `auth-required` error routed to the client, and the controller archives history before teardown.

Builds on [0025](0025-core-knows-agents-workflows-own-hierarchy.md).

## Context

Nodes will not only be long-lived SSH hosts. Real cases:

- **A company devpod**, created through the company's CLI and reachable over SSH afterwards. Fleet should create it or adopt it, install `fleetd` on it, and run agents there.
- **A Kubernetes pod**, reached with `kubectl exec`.
- **A throwaway sandbox**, for example a Firecracker microVM, for one agent.

The core already fits these cases:
- a node is fleetd running anywhere;
- the controller reaches it over a byte stream ([0006](0006-wire-protocol.md));
- nodes are autonomous ([0003](0003-node-controller-roles.md)).

What is missing is a defined way to create and destroy machines, transports other than SSH, bootstrap without systemd, and a policy for history on throwaway nodes.

## Decision

### Providers are WASM plugins by default

**The world.** A **node provider** is a WASM component implementing the WIT world `node-provider`. Its exports:
- `config-schema()`: the parameters `create` accepts, for example `cpu`, `image` or `branch`; clients render them as forms;
- `create(spec) -> {name, connect}`;
- `destroy(name)`;
- `list() -> [node]` (optional), for discovery;
- `keepalive(name)` (optional);
- `forward-port(name, port)` (optional; see Transport).

**Capabilities.** Providers get a narrow host API, gated by manifest capabilities approved at install:
- `exec:<bin>`: run one allowlisted binary with captured output, for example `exec:devpodctl` to wrap a company CLI;
- `net:<host>`: HTTP to a declared host;
- `storage:own`: the plugin's own storage.

The 0007 subprocess escape hatch remains, but it is not the default.

**Where providers run.** Providers run **on the controller**, so the binaries they `exec` must be installed, and logged in, on the controller's host.

### Transport: what `create` returns

`connect` is a tagged union:

- **`ssh {destination, options}`.** The core runs the system `ssh`, so `~/.ssh/config`, keys and the agent all apply. This gives file upload for bootstrap, `-R` port forwards for OAuth callbacks (0012) and opt-in agent forwarding (0011).
- **`command {argv}`.** Any command that gives a shell's stdio, for example `kubectl exec -i <pod> -- sh` or a vsock bridge. Port forwards then go through the provider's `forward-port` export. Without it, flows that need a localhost callback are unavailable on that node.

The protocol still runs as `fleetd --stdio` over that connection (0004, and the M2a `--stdio` mode).

### Providers own the machine; the core owns fleetd

- **Bootstrap is the core's job** (0011), over the provider's connection:
  - detect OS and architecture, upload the static `fleetd`, start it, and check the version on every connect, upgrading on a mismatch;
  - a provider can declare `fleetd_preinstalled` (for example baked into an image or rootfs) to skip the upload.
- **Keeping fleetd running:**
  - `systemd --user` with lingering when it is available;
  - otherwise `fleetd` is started detached (`setsid`);
  - a provider may supply its own start command.
- **Restarts:** if the machine or container restarts, the controller notices on reconnect and starts `fleetd` again, and that node's previous agents show as `lost`.
- **The built-in plain-SSH-host provider** turns `fleet node add user@host` into the trivial provider: `create` is adoption, and `connect` is `ssh`.

### Getting nodes

- `fleet node create --provider <p> [params…]` calls `create`.
- `fleet node add --provider <p> <name>` adopts a machine created outside fleet, for example with the company CLI.
- Discovery: with `list`, the GUI can show the provider's machines and offer "add as node".

### Lifecycle

**`keepalive`.** If the provider exports it, the controller calls it every few minutes while the node has running agents, so a platform's idle reaper does not kill agents that are working without an SSH session.

**`on_idle`.** Each node has an `on_idle` policy for when its last agent exits: `keep`, `stop` or `destroy`. The provider sets the default. A devpod provider defaults to `keep`, since it is the user's dev environment; a sandbox provider defaults to `destroy`.

**History before teardown.** Before any teardown the controller initiates, it **archives the node's agent history**:
- it pulls the pages with `ReadHistory` into the controller store, capped per node, for example 64 MiB as on a node;
- it keeps showing them as exited agents of a node that no longer exists.

A teardown from outside (the platform reaping a pod) loses the history.

### Logins for provider tools

A provider can fail with a typed error, `auth-required {message, login-command}`. The controller routes it to the connected GUI or CLI, following 0019's rule that human-facing actions follow the human. The user sees, for example, "devpod: login required — run `devpodctl login`" (a button in the GUI), and fleet retries afterwards.

### Sizing and per-agent sandboxes

The provider decides what a machine is and how big. "A fresh sandbox per agent" is a **workflow** (0025), not a core concept. For example, a `sandbox.new-agent` action:
1. calls the provider's `create`;
2. spawns the agent on the new node;
3. labels it;
4. sets `on_idle = destroy`.

A node can hold any number of agents.

## Consequences

- **M4:** the WASM plugin host must support the `node-provider` world's capabilities (`exec`, `net`, `storage:own`), which the secret providers need too (0019).
- **M5:** ships:
  - the `node-provider` world;
  - `fleet node create/add --provider`;
  - the built-in SSH-host provider;
  - bootstrap with optional systemd;
  - `keepalive` and `on_idle`;
  - history archiving before teardown;
  - routing of `auth-required`;
  - a sample provider to copy (for example, wrapping a company devpod CLI).
- **Transport code** treats SSH as one connect kind among others. Port forwards are an optional capability of a connection, not something the core assumes.
- **Controller store:** the controller's SQLite store (0019 §4) gains the node registry (provider, name, connect, `on_idle`) and archived history.
