# M2c gpui restyle spike: how to compare it

Branch `spike/gpui-restyle` (`m2c-followups` merged in). What was done and what gpui could not do: [ADR 0031](../../decisions/0031-native-look.md). Spec: `docs/superpowers/specs/2026-11-14-m2c-daily-drive-design.md` §1.

The reference screenshot stays at `~/Developer/fleetd-ui-reference.png`; keep every screenshot taken below outside the repo too (next to it).

## Build both

```sh
cd ~/Developer/fleetd
gh run list --workflow ci.yml --status in_progress   # wait if a perf-wall job runs on this Mac
git switch spike/gpui-restyle
cargo build --release -p fleet-gui -p fleetd -p fleet-cli
# The "before" build, in a worktree so both can run at once:
git worktree add ../fleetd-before m2c-followups
(cd ../fleetd-before && cargo build --release -p fleet-gui -p fleetd -p fleet-cli)
```

`scripts/bundle-macos.sh` builds `target/release/fleetd.app` if you want to launch the spike from the Dock.

## Launch side by side

Both GUIs are clients of the same local daemon (its socket is `~/.fleet/fleetd.sock`, so they share the default state dir; `--state-dir` would point each at a different daemon). They also share `~/.fleet/gui-state.toml`, so the layout saved is the one of whichever quits last.

```sh
target/release/fleet-gui --target local &
../fleetd-before/target/release/fleet-gui --target local &
open ~/Developer/fleetd-ui-reference.png
```

If no local daemon runs, the first GUI offers "Start fleetd on this Mac".

Start two or three agents (⌘N; one `claude`, one `sh -c 'git log | less'`), split (⌘D, ⇧⌘D), let one exit. Put the three windows next to each other at the same size. Screenshots: ⇧⌘4 then Space and click a window (saved to the Desktop, outside the repo).

Light mode: System Settings › Appearance › Light (or `osascript -e 'tell app "System Events" to tell appearance preferences to set dark mode to false'`). Accessibility: System Settings › Accessibility › Display › Reduce transparency / Reduce motion (reopen the window after changing Reduce transparency).

## What to look at

1. **Chrome.** Traffic lights centred in the 40 px bar; "This Mac" + agent counts under them (click: Switch fleetd); + and trash buttons at the right with tooltips. Drag the window from the bar's empty space (top and bottom halves), also with the ⌘N or connect sheet open; double-click it (zoom/minimise per your setting); pressing a button must not drag. Full screen: the bar keeps working without the light gap.
2. **Material.** The desktop shows through, blurred, behind the bar, the sidebar and the gaps; pane cards and sheets read as raised cards with a soft shadow. Try a bright wallpaper: is text still readable? Reduce transparency: opaque, same look.
3. **Type and spacing.** SF Pro everywhere outside the terminal; headers semibold, second lines smaller and greyer; consistent 4/8 px rhythm in the sidebar rows, pane headers, sheets.
4. **Controls.** Right-click a sidebar row (running: Open in New Split, Kill…; exited: Restart, Remove) and a pane header or grid (Copy/Paste, splits, Maximize, Restart Agent, Close Pane with shortcuts): real macOS menus, keyboard navigable, typing a letter jumps. In ⌘N's fields: click to place the caret, drag/double/triple-click to select, ⌥←/⌘←, ⌘A, ⌘Z/⇧⌘Z, ⌃A/⌃E/⌃K, a long path scrolls; Edit menu › Emoji & Symbols; a Japanese or Pinyin input method (candidate window at the text). Compare with a real field (e.g. Finder's ⇧⌘G).
5. **Motion.** Hover and press fills on rows and buttons; the ⌘N sheet slides in from the bar; the error toast rises in. Trackpad flicks in the sidebar and in terminal history (with output scrolled back) coast with momentum. Note: no bounce at the ends, hover changes are instant.
6. **Icons and terminal.** Lucide icons in the pane headers (maximize turns into restore), the bar, the Restart button of an exited pane. Terminal text in SF Mono 13, loaded from Terminal.app (bold and italic are real faces; Menlo if those files are missing); `[gui] font_family = "Menlo"` in `~/.fleet/config.toml` still wins.

Then use the spike for about an hour of real work before the go/no-go.

## Measure (screen unlocked, nothing else running)

```sh
cargo build --release --workspace
caffeinate -d target/release/fleet-perf run --suite wall --out wall.json
target/release/fleet-perf compare --baselines bench/baselines.toml --results wall.json
```

Watch `gui_cold_start_ns` (~155 ms baseline, 200 ms budget), `input_to_render_ns` (~13.5 ms) and `input_to_render_flood50_ns` (~11.5 ms) against the 5% tolerance. Local wall numbers are advisory; CI is authoritative. A regression needs the user's approval before merge (CLAUDE.md). The likely costs are listed in ADR 0031 › Consequences; to isolate the blur, temporarily turn on Reduce transparency (the window then opens opaque) and rerun.
