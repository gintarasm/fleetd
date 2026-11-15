# 0034 — Agent skill install for omp

**Status:** accepted · 2026-11-15 (issue #20)

## Context

`fleet skill install` only wrote the Claude Code skill (`~/.claude/skills/fleet/SKILL.md`). The owner uses omp at work, which reads skills from `~/.omp/agent/skills/<name>/SKILL.md`. The GUI's "Install agent skill" can run on an ssh target, so it cannot check which agents are installed by looking at the local disk.

## Decision

- `fleet skill --agent claude|omp|detected` (default `claude`; `--target`, the old placeholder flag, stays as an alias). The same `SKILL.md`, written the same way (create dirs, overwrite), for each agent. `uninstall` takes the same flag.
- `detected` means every agent whose home folder (`~/.claude`, `~/.omp`) exists on the machine `fleet` runs on, one printed path per agent, Claude first. None found is an error, not a silent no-op or a guessed default.
- `--project` stays Claude-only. omp's project-level skill path is unknown, so `--agent omp|detected --project` exits 1 with an error rather than guessing one.
- The GUI runs `fleet skill install --agent detected` locally or over ssh, and names the agents in the alert title from the printed paths ("Agent skill installed for Claude Code and omp").

## Consequences

- An older remote `fleet` without `--agent` makes the GUI install fail with clap's error in the alert; upgrading `fleet` there fixes it.
- Adding another agent means one more `Agent` variant (home folder, skill dir) and one more path suffix in the GUI's `installed_for`.
