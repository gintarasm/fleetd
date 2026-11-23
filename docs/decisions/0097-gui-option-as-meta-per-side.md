# 0097 — GUI: Option as Meta per side

**Status:** accepted · 2026-11-23 (issue #136)

Changes `option_as_meta` from [0028](0028-gui-client.md) (a bool, default `true`); the Settings window is [0044](0044-gui-settings-window.md).

## Context

With `option_as_meta = true` both Option keys send Esc-prefixed keys, so on UK, German and Lithuanian layouts `#`, `@`, `€`, `[` and `]` (Option+key) cannot be typed in a pane. Ghostty solves it with `macos-option-as-alt = left | right | true | false`: one Option stays Meta, the other composes text.

## Decision

- `[gui] option_as_meta = "left" | "right" | "both" | "none"`, default `"both"` (today's behaviour).
- The old bool still reads: `true` is `both`, `false` is `none`. A bool in the file is rewritten as a string the first time the Settings window saves the key. Any other string is a config error.
- **Telling the sides apart:** gpui 0.2.2's `Modifiers` has a single `alt`, so it cannot. The GUI reads the device-dependent bits of `NSEvent.modifierFlags` of the key event being handled (`NX_DEVICELALTKEYMASK` 0x20, `NX_DEVICERALTKEYMASK` 0x40) in `app/native.rs` (`option_sides`). The decision itself, `OptionAsMeta::applies(left_down, right_down)`, is pure and lives in `core/settings.rs`; `encode_key` is unchanged and still takes a bool, which the pane computes per key press.
- If both Options are held, either side's mode applies. If neither device bit is set (no current event), the side is unknown and only `both` counts as Meta.
- Settings › Behaviour shows a four-way picker (Left, Right, Both, None) in place of the switch.

## Consequences

- Changing the setting applies live, as before (the pane reads the config on change).
- The side check is AppKit-only; the pure part is covered by tests, the device bits by manual check.
