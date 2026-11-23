# 0096 — Underline styles and colour, and the blinking terminal cursor

**Status:** accepted · 2026-11-23 (issue #135)

**Builds on:** [0020](0020-stay-on-alacritty.md) (alacritty's grid and parser), [0028](0028-gui-client.md) (the terminal element), [0031](0031-native-look.md) (the text field caret's 530 ms blink), [0044](0044-gui-settings-window.md) (Settings), [0065](0065-gui-motion-and-feedback.md) (Reduce Motion), [0014](0014-performance-gate.md) (frame budget).

## Decision

### Underlines keep their kind and colour (`fleet-term`)

- `CellStyle.underline` is now an `Underline` enum (`None`, `Single`, `Double`, `Curly`, `Dotted`, `Dashed`) and `CellStyle.underline_color` carries SGR 58 (`Color::Default`, after SGR 59 or none, means the text colour). Both paths fill them: grid rows from alacritty's cell flags and `underline_color()`, history lines from the SGR parser (`parse.rs`). History lines already wrote `4:2`..`4:5` and `58` (`history.rs`), so the two paths agree; a test feeds lines through both and compares.
- SGR 21 stays what alacritty's parser makes of it (not a double underline); `4:2` is the double underline apps send.

### How each kind is drawn (`core/underline.rs`, gpui-free, unit-tested)

- **Single** and **curly** go on the text run: gpui's straight underline, and its wavy one for the undercurl (`UnderlineStyle.wavy`), in the underline colour.
- **Double**, **dotted** and **dashed** cannot be a gpui run decoration, so the element fills rectangles after the text: two 1 pt lines 2 pt apart (the second above the first when the row has no room below); 1 pt dots a dot apart; one dash per cell covering 60 % of it. Spans of one kind and colour are merged before the marks are computed, at gpui's own underline height (baseline + 0.618 of the descent).
- **Cost:** nothing changes for cells without these kinds (one enum check per cell). A dotted span costs about four quads per cell, a dashed one one per cell; both are rare (LSP diagnostics use the undercurl).
- The underline colour is resolved like the foreground (palette, 256-colour, RGB); a hidden cell's underline takes the background, so it stays hidden.

### The cursor blinks when the app asks (`core/cursor_blink.rs`)

- `CursorView.blinking` is DECSCUSR's blink bit (1, 3, 5, and `?12h`), read from alacritty's cursor style; DECSCUSR 0 returns to a steady cursor.
- **`[gui] cursor_blink`**: `app` (default: blink when the app asks), `on` (always), `off` (never). In Settings → Behaviour, "Cursor blink", next to Motion.
- **Never** in an unfocused pane (or an inactive window), **never** under reduced motion (`ReducedMotion`, i.e. System Settings or `[gui] reduce_motion`), and not where no cursor is drawn (empty, ended).
- **Phase:** 530 ms shown, 530 ms hidden (AppKit's period, as the text fields). Each keypress (anything the pane sends) restarts it shown; focusing a pane starts it shown.
- **Timer:** one task per pane, alive only while that pane's cursor blinks, which in practice is the focused pane. It sleeps until the next toggle (no polling) and notifies only that `PaneView`; the render decides from the clock whether the cursor is in its shown half and drops the task when blinking stops. gpui 0.2.2 redraws the window on a view notify (the panes are not cached views), so a blink costs one ordinary frame every 530 ms while the focused pane's app wants a blinking cursor; other panes do no extra work and no timer runs when nothing blinks. Making panes cached views would confine the redraw further but changes how every pane refreshes; left for a separate change if the wall suite shows it matters.

## Left out

- SGR 5/6 (blinking text) stays ignored.
- No "stop blinking after N seconds idle" (macOS Terminal does not either).
