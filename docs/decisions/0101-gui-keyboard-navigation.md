# 0101 — GUI: keyboard navigation in sheets and Settings

**Status:** accepted · 2026-11-23 (issue #140)

Builds on the sheets ([0031](0031-native-look.md)) and the Settings window ([0044](0044-gui-settings-window.md)).

## Context

The sheets only took Return and Escape, and the Settings window only Return in a text field. Tab did nothing, and the switches, pills, steppers and buttons could not take the focus, so they could only be used with the mouse.

## Decision

- **Tab and Shift-Tab** move the focus to the next or previous stop, wrapping. This applies in the New agent, Kill, Rename, Answer and Edit labels sheets and in the Settings window. Tab with ⌘, ⌃, ⌥ or fn held is left to shortcuts. The order is the drawing order. The sheets use gpui 0.2.2's tab stops (`tab_index`, `FocusHandle::tab_stop`, `Window::focus_next/prev`).
  - Only sheets and Settings mark stops, so a sheet's Tab never reaches the panes or sidebar behind it.
  - The Send sheet keeps its own Tab, which switches between the message and the filter (0083).
- **Stops:** text fields, push buttons (including presets, Choose… and the answer choices), the Kill sheet's checkbox, the Edit labels sheet's remove buttons, and the Settings switches, segmented controls and steppers. Lists (recent directories and commands, presets, shortcuts) stay mouse-only for now. A disabled segmented control or stepper is not a stop.
- **The focus ring** on a control is the same accent halo as on a focused text field (`controls::focus_ring`): a 3 pt spread at 35 % accent, drawn as a shadow so the layout does not move.
- **Keys on a focused control:**
  - Space clicks it. A switch toggles, a button is pressed, and the Kill sheet's checkbox toggles. This is gpui's keyboard click.
  - A segmented control is one stop, as in AppKit. ← and → pick the previous or next choice without wrapping. This covers the Settings section tabs, Theme, Liquid glass, Motion, Cursor blink and Option as Meta.
  - A stepper is one stop. ← or ↓ steps down, → or ↑ steps up, and a step past the end of the range does nothing.
  - Return never clicks the focused control (`controls::return_is_not_a_click` stops gpui's Return click). In a sheet Return is the default button, as before, whatever has the focus. In Settings, Return applies the focused text field.
  - In the Kill sheet, Space toggles "Don't ask again" only while the sheet itself has the focus, which is how it opens. Once a button has the focus, Space presses that button.
- **macOS Keyboard navigation policy: always.** Tab reaches every control whatever System Settings › Keyboard › Keyboard navigation (`AppleKeyboardUIMode`, ⌃F7) says. With that setting off, which is the macOS default, AppKit's Tab only visits text fields and lists. Following it would leave the Behaviour section with no stop at all and the sheets' buttons unreachable, and a keyboard-first tool should not need a hidden system toggle for that. There is no fleet setting for it.
- The pure parts (which key moves the focus, which arrow steps which control, and stepping through a list of choices without wrapping) are in `core::keyboard_nav`, with unit tests. Focus movement and Space are covered by gpui tests.

## Consequences

- Clicking a sheet's button focuses it, so it shows the ring until the sheet closes or the focus moves. This is the same as clicking a text field. In Settings, a click anywhere outside a field returns the focus to the window, so the ring does not stay after a click.
- If a future view outside a sheet marks tab stops, it must not be drawn while a sheet is open, or the sheet's Tab will reach it.
