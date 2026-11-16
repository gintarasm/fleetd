# 0084 — Command presets, recent commands, a first prompt and Duplicate Agent

**Status:** accepted · 2026-11-16 (issue #96, part of #26)

Builds on [0044](0044-gui-settings-window.md) (Settings), [0050](0050-gui-split-chooser.md) (the split chooser), [0052](0052-agent-names.md) (agent names), [0080](0080-palette-lists-every-action.md) (every action in the palette) and [0029](0029-gui-core-toolkit-free.md) (core stays toolkit-free).

## Context

Starting an agent meant retyping its command (`claude`, `claude --continue`, `omp`, `codex`) and labels every time. The New agent sheet remembered directories, not commands, and nothing could start a copy of a running agent (Restart needs it ended).

## Decision

### Presets

- A preset is a name, a command and labels: `[[gui.presets]]` in `config.toml`.

  ```toml
  [[gui.presets]]
  name = "Claude (continue)"
  command = ["claude", "--continue"]
  labels = { role = "dev" }
  ```

  `command` is an array, as `default_command`. A preset without a name or a program is skipped, as is a second one with the same name; a `name` label is dropped (every spawn gets its own name, 0052).
- **Defaults.** Without a `presets` key, the agents found on `PATH`: Claude (`claude`), Claude (continue) (`claude --continue`), Codex (`codex`), omp (`omp`), in that order. They are looked for in `PATH` and then in the folders agents are usually installed in (`~/.local/bin`, `~/.claude/local`, `~/.bun/bin`, `~/.cargo/bin`, `~/.npm-global/bin`, `/opt/homebrew/bin`, `/usr/local/bin`), because an app started from the Finder gets the system's short `PATH`. The lookup is a few `stat`s, done once per target the first time presets are needed (the sheet, the chooser, the palette or Settings opening), never at launch or per frame. For an ssh target the GUI cannot see the remote `PATH`, so every candidate is offered; a missing one fails at spawn with the daemon's error. `presets = []` turns them off.
- There is no Shell default: New Terminal (⌘T, the chooser's 3) already starts a login shell, and does it right on a remote host. A shell preset can be added by hand.
- **Editing is config-file based.** Settings › Agents lists the presets (name, command, labels) read-only, with a note saying whether they were found on `PATH` or come from `[[gui.presets]]`. A full editor (add, remove, reorder rows with a command and label fields each) is more UI than the feature needs now. Opening Settings reads the file again (0044), so a hand edit takes effect then, or at the next launch.
- **Where they show:**
  - **The New agent sheet**: one button per preset at the top. A click fills the Command and Labels fields and puts the caret in Prompt; Return spawns. It does not spawn at once, so the folder and prompt can still change.
  - **The split chooser**: a row per preset after New agent, Existing agent and Terminal, keys 4 to 9 (the first six presets). Picking one starts it straight away in the chooser's folder, like row 1, with a generated name.
  - **The palette**: "New <preset>…" per preset, after the commands and before the agents. It opens the sheet prefilled with the preset. These are entries (`Entry::Preset`), not `fleet::*` actions: they are the user's data, not the keymap's, so 0080's drift rule does not apply to them.

### Recent commands

- The sheet lists the target's recent commands under the Command field, most recent first, like the recent directories; a click fills the field. They are kept per target in `gui-state.toml` (`recent_commands`, at most 20), as the field was typed, without the prompt.
- A command becomes recent once its pane shows the new agent, as a directory does: a spawn that fails adds nothing.

### First prompt

- The sheet has an optional Prompt field. When it is not blank, its trimmed text is appended as the command's last argument (one argument, whatever it contains), which is how `claude "…"`, `codex "…"` and `omp "…"` take a first task.
- The generated name follows the prompt too: the name is drawn from the command with the prompt after it, so a prompt like "fix the SIGTERM race" names the agent `sigterm-<animal>` (0052's task word).

### Duplicate Agent

- **Duplicate…** in the sidebar row's menu and **Duplicate Agent…** in the pane's menu (both next to Rename) open the New agent sheet prefilled with the agent's command (program and arguments, shell-quoted), folder and labels. It works whatever the agent's state, running included.
- The `name` label is not copied: the sheet generates a name unique among live agents (0052), and the original is live, so the copy gets the next animal. `fleet.*` labels (the core's) are not copied either. A first prompt the original was started with is part of its arguments, so it is in the Command field; the Prompt field starts empty.
- The palette gets **Duplicate Agent…** (`fleet::DuplicatePane`, unbound), on the focused pane's agent, shown when the pane shows one, as Rename Agent… (0080).

### Where

- `core::presets` (toolkit-free, unit-tested): the preset type, defaults and detection, the usable-preset filter, the label text, the prompt, the name source, the sheet's prefill (`Prefill::preset`, `Prefill::duplicate`) and the chooser's spawn request.
- `core::split_chooser` gains `Choice::Preset(i)`; `core::palette` gains `Entry::Preset(i)` and `Command::DuplicatePane`; `core::menus` gains the Duplicate items.
- `AppState::presets` caches the detection per target; `AppState::spawn_from_sheet` records the command for the recent list.

## Consequences

- GUI cold start is unchanged: the config is read as before (the presets are part of it), and `PATH` is only looked at on first use.
- A preset index (chooser row, palette entry) is resolved again when picked; editing the config between opening the chooser and picking could start the preset now at that index. Settings is the only thing that re-reads the config while running, so this needs a hand edit and a Settings open in between.
- The palette's recents remember a preset by its index.
- Not measured (PERF_CI off, [0041](0041-perf-ci-switch.md)); nothing is added to the frame path.
