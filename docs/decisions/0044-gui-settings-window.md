# 0044 — The GUI Settings window

**Status:** accepted · 2026-11-16 (M2d, issue #28)

**Amends:** [0028](0028-gui-client.md) (the GUI now writes `[gui]` keys besides `connection`; new keys below). Builds on [0029](0029-gui-core-toolkit-free.md) and [0031](0031-native-look.md).

## Context

Every GUI setting (`font_family`, `font_size`, `option_as_meta`, `default_command`, `connection`) could only be set by editing `config.toml` and restarting. Issue #28 asks for a Settings window on ⌘, and in the app menu that writes the same file and applies changes live, with sections Appearance, Behaviour, Agents and Keys. Later issues (liquid glass #31, notifications #32, sounds) add their own toggles to it.

## Decision

- **A separate window**, not a sheet: ⌘, and fleet › Settings… open it, or bring the open one to the front. It is titled "Settings", fixed size, opaque; ⌘W closes it. A sheet would block the panes and could not stay open beside them while trying a font or an opacity.
- **The model is in the core** (`core::settings`, toolkit-free per 0029): the sections, the choices, one `Change` per control (clamped and trimmed when applied, reporting its key only when something changed), writing the changed keys back, the command-line parse, and the shortcut list. `src/app/settings_window.rs` only draws it.
- **Writes are per change and per key.** Each control writes only its own key to `[gui]` with `toml_edit` (`config::save_gui_values`, which `save_connection` now uses too): comments, unknown keys and a key's trailing comment survive, the file and the table are created when missing, the file's mode is kept. If the write fails, the change is not applied and the window shows why. Text fields (font family, default command, default folder) apply on Return or when they lose focus, not per keystroke; a default command that does not parse is shown, not applied.
- **Opening Settings reads the file again**, so edits made by hand meanwhile take effect (a malformed file is left for the next launch to report).
- **Live apply.** `AppState::change_settings` updates the config and every open pane (font family, size and Option as Meta; the next layout resizes the grid). The workspace observes the config and re-applies its look when the theme, opacity, blur or motion changes. New panes and sheets read the config as before.
- **Theme** is applied by setting `NSApplication.appearance` (Aqua, Dark Aqua, or nil for System). AppKit then reports the new effective appearance to every window, so the existing appearance path recomputes the theme, and the blur material, native menus and alerts follow too. The theme is also resolved from the setting directly (`ThemeChoice::resolve`), so it holds where AppKit is absent (tests).
- **Reduce Transparency** still wins: the window opens opaque and the tokens are flattened, so blur and opacity have no effect; their steppers are disabled with a note. **Reduce Motion** can be overridden either way (`reduce_motion`: `system` follows Accessibility, `on` always reduces, `off` always animates): the user's explicit choice in fleet's own settings beats the system default.
- **Window opacity** is the window tint's alpha, from 0.76 to 1.0 (default 0.84, the theme's). Below 0.76 secondary text on the tint over a white desktop falls under 3:1; the theme's contrast tests cover the whole range.
- **Fonts.** Choosing SF Mono after a launch that did not preload it reads Terminal.app's four faces then, once (about 425 KB, on the main thread, only on that change).
- **Keys** is a read-only list (`core::settings::SHORTCUTS`); a test checks every listed binding is bound.

New `[gui]` keys:

| key | type | default |
|---|---|---|
| `theme` | `"system"`, `"light"` or `"dark"` | `"system"` |
| `blur` | number, points, 0–80 | `40` |
| `window_opacity` | number, 0.76–1.0 | `0.84` |
| `reduce_motion` | `"system"`, `"on"` or `"off"` | `"system"` |
| `confirm_kill` | bool (Kill asks first) | `true` |
| `notifications` | bool | `true` |
| `default_dir` | string (New agent's folder; empty: the most recent) | `""` |

## Consequences

- `notifications` is stored and shown, but nothing posts notifications yet: #32 reads it when it adds them.
- Font family, size and Option as Meta, theme, opacity, blur, motion, kill confirmation, default command and folder all take effect without a restart.
- Values outside a control's range in a hand-edited file are clamped where they are used (opacity, blur) or at the next step (font size).
- The status menu does not list Settings; the app menu and ⌘, do.
- Performance: nothing is added to the per-frame path; a settings change re-resolves the font and redraws once. Not measured (PERF_CI off, [0041](0041-perf-ci-switch.md)).
