# 0076 — Mailbox, automatic exit reports, broadcast and `fleet wait`

**Status:** accepted · 2026-11-17 (issue #106, M3 workstream E)

**Builds on:** [0034](0034-agent-communication.md) §1–3 and §5, [0072](0072-agent-identity-by-session.md) (callers and scope), [0032](0032-daemon-restart-handoff.md) (daemon restart), the M3 spec §3. The spec's decisions (table shape, retention 1,000 / 10,000, recipients D(a) + parent + user, `trusted` only for `answer`, exit codes) are not repeated here; this records what the spec left to the builder.

## Decision

### Store (schema v9)

- `messages(id, recipient, sender, kind, body, status, exit_code, created_ms)`, as the spec's table plus two nullable columns: `status` (the state of a `status` message: working, waiting, done, error) and `exit_code` (of an `exit` message), so `--json` and `fleet wait` read them without parsing the body. `id` is `AUTOINCREMENT`, so a cursor never points at a newer message after deletes. Index on `(recipient, id)`.
- `recipient` references `agents(id) ON DELETE CASCADE`: `fleet rm` takes the mailbox with it. `sender` is not a reference: what an agent sent outlives it (its name is then shown as unknown).
- **A restart keeps the mailbox.** `replace_agent` moves the old agent's messages and read mark to the new id in the same transaction, as `fleet.parent` already follows replacements.
- **Retention** is applied on every insert (delete beyond the newest N of that mailbox, in the insert's transaction).
- **Kinds:** the spec's five plus `lost`, so `fleet wait --state lost` and the inbox can tell a loss from an exit without a magic code.

### Read marks and the inbox

- Each mailbox has a **read mark** (`inbox_marks`, the user's under key 0). `fleet inbox` without `--since` returns what is after the mark; whatever an inbox read returns (with or without `--since`) moves the mark forward to it, never back. `--since 0` rereads everything kept. One page is at most 1,000 messages.
- `fleet inbox` text: `[from agent 7 (name)] <kind>: <body>` (`status waiting: …`, `exit 3: …`, `[from the user] …`). Control characters are escaped and every line of a body after the first is indented, so a body cannot pass for another message or for the user's words. `--json` is an array of `{id, sender, sender_name, recipient, kind, status, exit_code, body, created_unix_ms, trusted}`; `trusted` is true only for an `answer` from the user. The `answer` and `question` kinds are reserved: nothing writes them until M3-G.

### Sending and broadcast

- `fleet message <to> "text"`: `<to>` is an id, a name, or the keywords **`parent`** and **`user`** (a name of either word is reachable by id). `parent` of a root agent is the user, the same rule as reports.
- Broadcasts go to **running** agents only, and never to a root shell (`kind=shell` without a parent is the user's own terminal, 0072 amendment). `--children` is the caller's running children (the user's: running roots), `--descendants` all running descendants (the user's: every running agent), `--label k=v` narrows either and alone implies `--descendants`. No recipient matched: error, exit 1.
- A direct message to a root shell is refused (its `fleet inbox` reads the user's mailbox, so the message could never be read). A direct message to an exited agent is allowed: it keeps its mailbox until removed, and a restart carries it over.
- An agent naming an id outside D(a) + its parent gets `forbidden:` (exit 3), also for an unknown id, as 0072 does for every other request. The user naming an unknown id gets "not found" (exit 1).
- The node checks recipients, since it needs the tree; `fleetd/src/scope.rs` lets `SendMessage` and `ReadInbox` through (own mailbox) and checks `Wait`'s named agents against D(a).

### Automatic reports

- An exit (`NodeSink::on_exit`, after the exit is stored) posts `exit` with the code; agents marked lost when fleetd starts (`mark_running_as_lost_except`, in its transaction) post `lost`. Both go to the parent, followed through restarts.
- **Where reports go when there is no parent:** a root agent's reports go to the **user's mailbox**; so do an orphan's, whose parent was removed. A root shell reports nothing (it is the user). A shell an agent spawned reports like any child.
- **`status` messages:** `Node::status_changed(agent, status, message, inferred)` is the hook. It posts only when the state or the text differs from the agent's last `status` message (one message per change), and wakes waiters. **M3-D's `fleet report` calls it after storing the report; the shared inferred-`waiting` heuristic calls it with `inferred = true` on its transitions** (the body then ends in "(inferred)"). It is tested by calling it directly.
- Until D stores the reported state on the agent, the **current** reported state is read from the agent's newest `status` message (`Store::latest_status`). That message lives in the parent's mailbox, so it goes when the parent is removed or retention drops it. D replaces `latest_status` with its column; nothing else changes.

### `fleet wait`

- Request `Wait { agent_ids, states, timeout_ms }` → `WaitResult { matches, agents }`, `matches` empty on a timeout, `agents` the targets as they are now. Default timeout 10 m, at most 24 h per request; `--timeout 0` checks once.
- **What matches.** Named agents match on their **current** state (exited, lost, the latest reported done/error/waiting): `fleet wait 7 --state exited` returns at once if 7 has already exited. Without names (all of D(a); all agents for the user) only **news** matches, or a default wait would return forever once any descendant had ended: an **unread** report or message in the caller's mailbox whose kind maps to a requested state (`exit`→exited, `lost`→lost, `status`→its state, the rest→message), or a descendant reaching a requested state while the wait runs. A child's exit is therefore never missed, even when it exits before the parent starts waiting: its report waits unread in the parent's mailbox. `fleet wait` does not mark anything read; `fleet inbox` does. A grandchild's change reaches only its own parent's mailbox, so a wait sees it only as it happens.
- `--state message` with named agents matches unread messages from those agents only.
- **`waiting` means reported `waiting` for now.** `Node::inferred_waiting` is the seam for D's shared heuristic and answers no until then; D also feeds inferred transitions through `status_changed`, which wakes waiters.
- **Delivery.** The node has a `broadcast` channel of `Notice`s (a message stored in a mailbox; an agent reaching a state), sent from the store-writing paths, never from the PTY output path. A waiter re-checks the store on each notice and on lag. Waits read only their own mailbox and their targets, so they need no `Scope` filter of their own; no new event stream reaches client connections.
- **On the connection.** A wait runs as a task of its connection and answers through the outbox, so the connection keeps reading: a client that goes away aborts its waits. Its answer may come after the answers to later requests (responses carry the request id). At most **16 waits run at once per connection** (overseer review of #123): each re-reads the store on every notice, so a looping client must not pile them up; the 17th is refused with a plain error (exit 2), and a finished or aborted wait frees its slot. A wait is not counted as a request in flight, so it never holds off a daemon restart; the restart closes the connection, and `fleet wait` reconnects (for up to 15 s of failures) and waits on with the time left. Mailboxes are in the store, so they survive the restart.
- **Output.** A match prints one line per match (`agent 7 (name): exited: [from agent 7 (name)] exit 3: …`); a timeout prints "timed out after …" to stderr and each target's process state to stdout (`agent 7 (name): running`), exit 1. `--json`: `{matched, matches: [{agent, name, state, message}], agents: [{id, name, state, exit_code, labels}]}`.

### Exit codes and CLI layout

- The three commands live in `fleet-cli/src/mailbox.rs`, are dispatched before the shared connection setup, and keep the spec's codes themselves: 0 ok; 1 not found, no match, timeout; 2 usage or other errors, including fleetd not running; 3 forbidden.

### Protocol

Additive, in the reserved block: requests `send_message = 55`, `read_inbox = 56`, `wait = 57`; responses `messages_sent = 55`, `inbox = 56`, `wait_result = 57`. New messages `Message`, `SendMessage`, `MessagesSent`, `ReadInbox`, `Inbox`, `Wait`, `WaitResult`, `WaitMatch`, enums `MessageKind`, `Broadcast`, `WaitState`. The large ones are boxed so `Frame` stays small. No new `Event`: message lines in the graph (wave 3) may add one, filtered through `Scope` like the others.

## Consequences

- Coordinators can stop polling screens: `fleet wait` in the background (or in the foreground with a timeout under the shell's limit), then `fleet inbox`. The skill (wave 3) documents this loop.
- The user's mailbox collects every root agent's exit (up to 10,000). Nothing shows it in the GUI yet.
- `latest_status` is a stopgap and the inferred `waiting` seam answers no until M3-D merges; the PR for D replaces both.
- Every exit and loss costs one SQLite insert (and a retention delete) on the exit path, next to the existing `set_exited`; none touch the output path.

## Merged with M3-D (2026-11-17)

- `Node::report` (`fleet report`, ADR 0075) now calls `status_changed(…, inferred = false)` after storing the report, so the parent gets one `status` message per change and waiters wake. The store lock is released first; `status_changed` takes it itself.
- `fleet wait` reads the current reported state from the agent's stored report (ADR 0075's `reported_*` columns) instead of `Store::latest_status`, which is now used only by tests and the one-per-change check.
- **Still open:** inferred `waiting`. `Node::inferred_waiting` still answers no, and nothing posts inferred transitions yet. That needs a periodic sweep over running agents (the heuristic is `fleet_proto::status::inferred_status` with `WAITING_QUIET_MS`), because going quiet produces no event to wake a waiter. It is tracked as its own issue.
