# 0094 — The GUI diff pane and workflow actions

**Status:** accepted · 2026-11-23 (M4 workstream D, issue #164; design in [the M4 spec](../superpowers/specs/2026-11-23-m4-workflows-design.md) §4)

**Builds on:** [0029](0029-gui-core-toolkit-free.md) (toolkit-free GUI core), [0043](0043-gui-command-palette.md) (palette), [0045](0045-gui-resizable-splits.md) / [0050](0050-gui-split-chooser.md) (splits and the split chooser), [0058](0058-ended-pane-summary.md) (ended summary), [0084](0084-gui-command-presets.md) (New agent sheet), [0085](0085-gui-git-branch.md) (`AgentInfo.git`), [0090](0090-m4-workflows.md) (M4), [0092](0092-workflow-host-and-run-action.md) (`ListActions` / `RunAction`, the action and view protocol), [0093](0093-git-worktree-workflow.md) (the git-worktree workflow's actions).

**Amends the M4 spec §4** (the overseer's calls, 2026-11-23): Show Changes is **⇧⌘C**, not ⇧⌘D, which has been Split Down since 0045 (and ⌥⌘D is taken by macOS for the Dock); and the diff pane's **⌘R** refreshes it, bound in the pane's own key context so it wins over the workspace's ⌘R (Restart Agent in Pane) only while the diff pane has the keyboard.

## Context

0090 put a read-only diff view, a "New worktree" spawn, an archive item and a generic action form in the GUI; the user chose a split beside the agent's terminal for the diff (#166, answer 5). Everything goes through 0092's `RunAction`: the GUI never runs git. Two GUI batches change `fleet-gui` at the same time, so the work lives in new modules with small hooks in the shared files.

## Decision

### Model, in `core` (gpui-free, unit-tested)

- **`core::diff`** turns a `DiffView` into display rows once per reply (`DiffDoc`): a file header, hunk headers (`@@ -a,b +c,d @@ header`), context / added / removed lines with old and new line numbers, `\ No newline` markers, and one-line notes (binary file, mode change). A line longer than 2,000 characters is cut with an ellipsis (no horizontal scroll in M4). Collapsing a file changes only the list of shown rows (`u32` indices), not the rows. A file over the reply's cap (`hunks_omitted`) starts collapsed; expanding it asks for that file alone (the `file` param) and its rows replace the empty ones. `copy_text` writes selected rows back as a unified diff. Tables and text views become lines (`table_lines`, `text_lines`), aligned as `fleet action run` prints them.
- **`core::action_form`** turns an `ActionSpec` into fields: string / int / duration / argv / labels are a line, `text` a text box, `bool` a checkbox, `choice` a pop-up of its choices, `agent` a pop-up of the agents, `path` a folder field. Defaults fill the fields, and an untouched field sends nothing (the workflow's default applies). Values are checked by kind before anything is sent (required, whole number, duration as the CLI reads it, absolute path or `~`, `k=v` words, balanced quotes, a choice in its list), each error on its field. `with_retry` merges a refusal's `retry` into the params for "… Anyway".
- **`core::worktree`** knows git-worktree's action names and params (`new-agent`, `diff`, `archive`), the slug rule, when an agent offers Show Changes (`AgentInfo.git` set and the daemon has the diff action) or Archive Worktree… (ended, with `git.worktree`, and the action), and when a shown diff refreshes. `Actions` is the daemon's answer to `ListActions`: unknown, unsupported, or the list.

### The diff pane

- **A layout leaf with no terminal behind it.** `AppState` keeps diff panes beside the terminal panes, keyed by pane id; no connection is opened for them. Showing an agent or spawning into a focused diff pane turns it back into an empty terminal pane first. Diff panes are **not saved**: the saved tabs leave them out (`core::diff::without_panes`), so a relaunch shows the terminals only.
- **Where it opens.** Show Changes focuses the agent's existing diff pane (in any tab, refreshing it), else splits the pane showing the agent to the right (0050's rule: the split pane's agent), else the focused pane. At four panes it says so in the error toast instead.
- **Opening it:** ⇧⌘C on the focused pane's agent; Show Changes in the sidebar row's, the graph card's (a right-click, which now opens the row's menu) and the pane's menus; the palette's "Show Changes" and "Show Changes: <agent>" rows; and the ended summary's Show Changes button. All only for agents in a git checkout, and only while the daemon has `git-worktree.diff`.
- **Layout.** A header (the view's title, `from → to`, files and `+n −m`, the `against` choice base / uncommitted / committed, refresh and close), the file list (status letter coloured from the theme, path, `+/−`; click or ↑/↓ to jump), and the unified diff (old and new line numbers, added and removed lines tinted with the theme's success and danger colours, hunk headers on the accent wash) with a sticky header for the file at the top. Space collapses or expands the selected file; a click on a file header does too.
- **Rendering.** Both lists are gpui `uniform_list`s of fixed-height rows, so only the rows in the viewport are built: a 50,000-line diff costs what a screenful costs. Rows use the terminal font at the zoomed size.
- **Selection.** Line-granular: a click selects a row, a drag or Shift-click extends it, ⌘A selects all, ⌘C copies the rows as a unified diff. Character-level selection inside a line is left for later.
- **Refresh:** on open, on ⌘R or the button, on a change of `against`, and once when the agent's status turns waiting, done or failed/ended (a transition of the status the sidebar shows) while the pane is in the shown tab. Never on a timer, never on output. A reply to an older request is dropped. A refresh keeps the files the user collapsed.
- **Errors and refusals** show in the pane ("Cannot show the changes" and the reason); an ended agent's diff works while its worktree exists.

### Actions in the GUI

- **`ListActions` once per connect**, on a connection of its own, off the UI thread (as Search All Agents). A daemon from before workflows answers the unknown-request error: every workflow item (Show Changes, Archive Worktree…, Run Action…, "New worktree") stays hidden.
- **Run Action…** (palette) opens the action sheet: the daemon's actions to pick from, then the form rendered from the schema; a destructive action asks once more. **Archive Worktree…** (row, graph card and pane menus, palette) opens the same sheet as a question naming the worktree. The workspace runs the action on its own connection and puts the sheet where the result says:
  - `done`: the sheet closes, the message (else "<action>: done") is the toast;
  - `spawned`: the new agent shows in the focused pane;
  - `view`: a view pane opens beside the focused pane: a diff in the diff pane, a table or text as lines in the same pane, which refreshes by running the call again;
  - `refused`: the reason and "<verb> Anyway", which resends with `retry` merged (Archive Anyway, Run Anyway);
  - an error stays in the sheet (or, from the New agent sheet, the error toast).
- **New agent sheet.** While the daemon has `git-worktree.new-agent` and the folder is in a git checkout (a local folder: a `.git` in it or a parent, checked when the folder changes; a remote one is assumed to be), a "New worktree" checkbox adds Slug (following the agent's name until edited, `[A-Za-z0-9._-]`, ≤ 64), Branch (placeholder `fleet/<slug>`) and Base (placeholder: the default branch). Spawn then runs `new-agent` with the folder as `repo`, the command line as `command`, and the sheet's name and labels; blank Branch and Base are left to the workflow. The checkbox is remembered per target (`gui-state.toml`, `new_worktree`). Presets fill the sheet as before.

### Where the code is

`core::diff`, `core::action_form`, `core::worktree`; `app::diff_pane` (the pane, its key context and actions), `app::action_sheet`, and `app::workspace::workflow_ui` (the workspace's side: menus, palette rows, the sheet, results, the worktree spawn). `workspace.rs`, `pane_view.rs` and `app_state.rs` get hooks only. `Session::list_actions` / `run_action` are blocking jobs like `search_output`.

## Consequences

- Every future workflow's actions appear in Run Action… with a form and result handling and no GUI code; the iOS/web clients can copy `core::action_form`.
- Nothing runs while idle: one `ListActions` per connect, diffs only when asked or once per status transition of a shown pane's agent.
- New benchmarks, no baselines until the user records them (listed as unrecorded in `fleet-perf`'s baseline test): `gui_diff_open_ns` (open to first frame of a 10,000-line fixture diff; budget 100 ms) and `gui_diff_scroll_frame_ns` (a frame while scrolling it during the 50-agent flood; budget 16 ms), driven by the GUI's `diff-open` and `diff-scroll` bench modes on a fixture `DiffView` (no git).
- Left for later: character selection inside a diff line, horizontal scroll, side-by-side diffs, syntax colours, comments to the agent, a folder picker for `path` fields in the action form.
