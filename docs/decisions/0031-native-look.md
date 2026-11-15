# 0031 — A native look for fleet-gui, in gpui

**Status:** proposed · 2026-11-15 (M2c)

**Amends:** [0028](0028-gui-client.md) (`[gui] font_family` defaults to `"SF Mono"`, Menlo when not installed; native context menus replace the drawn row menu). Builds on [0029](0029-gui-core-toolkit-free.md). Spec: `docs/superpowers/specs/2026-11-14-m2c-daily-drive-design.md` §1. Branch: `spike/gpui-restyle`.

## Context

After M2b the GUI works but does not feel like a Mac app. The spec's look-and-feel track asks whether gpui 0.2.2 can get close enough, by restyling all six areas (chrome, material, type and spacing, controls, motion, icons and terminal), before the user decides between finishing the polish in gpui or rewriting the shell natively (Swift/SwiftTerm). The spike was to be throwaway, but may be kept if it is good, so it was built to merge quality: core logic toolkit-free and unit-tested, the gpui shell only drawing it, every existing behaviour and test kept.

## Decision

Restyle in gpui, filling gpui's gaps with small objc2 calls (`src/app/native.rs`, `src/app/context_menu.rs`; objc2 / objc2-app-kit / objc2-foundation were already dependencies). No crate was added.

### Per area

1. **Chrome — done.** `TitlebarOptions { appears_transparent, traffic_light_position }` puts AppKit's traffic lights, centred, in our own 40 px unified bar (`core::style::chrome`). The bar shows the connected fleetd and its agent counts (`core::titlebar`; a click opens Switch fleetd) and the window's actions (New agent, Clear exited, moved out of the sidebar). A press on its empty space calls `-[NSWindow performWindowDragWithEvent:]` with the current mouse-down (gpui 0.2.2's `Window::start_window_move` is a no-op on macOS); a double click calls gpui's `titlebar_double_click`, which honours the user's "double-click a window's title bar to" setting. Buttons in the bar stop the mouse-down so they never drag. Full screen drops the room kept for the traffic lights. The window title (Window menu, Mission Control) is "fleet — <host>". Side (vertical) agent tabs stay.
2. **Material — done.** `WindowBackgroundAppearance::Blurred` (gpui adds a colourless `NSVisualEffectView` and hides AppKit's tint layer) under a translucent window tint from the theme; the bar and the sidebar sit on it, pane cards float over it with a 1 px border and a soft shadow, sheets and the toast are raised cards. No objc2 material change was needed: the blur is gpui's, the tint is ours, so dark and light look the same way. With **Reduce Transparency** on (read via `NSWorkspace`) the window opens opaque and the tokens are flattened (`Tokens::opaque`).
3. **Type and spacing — done.** `core::style` holds the 4/8 px grid, radii, the macOS text styles (Title 3 15 semibold, Headline 13 semibold, Body 13, Callout 12, Subheadline 11, Caption 10) and chrome metrics; `core::theme` holds every colour token for dark and light with WCAG contrast tests over a black and a white desktop. Text is in gpui's default `.SystemUIFont` (SF Pro). The views use only tokens.
4. **Controls — done, with gaps.**
   - **Context menus** are real `NSMenu`s: the sidebar row (Open in New Split, Kill…, or Restart and Remove) and a new pane menu (right-click on a pane's header, or on its grid while the agent does not take the mouse: Copy, Paste, Split Right/Down, Maximize/Restore Pane, Restart Agent, Close Pane, with their shortcuts shown). What they offer is `core::menus`. The menu is popped a run-loop turn after the click (`performSelector:withObject:afterDelay:0`), outside gpui's event dispatch: AppKit's menu tracking loop must not run while gpui is mid-update (gpui's frame callback would re-borrow the app). Picks return through a channel to a foreground task and are dropped if the session changed meanwhile.
   - **Text fields** in the connect and new-agent sheets are a gpui element over `core::text_edit`: caret anywhere (blinking at AppKit's 530 ms), mouse/Shift/double/triple-click selection, ⌥ word and ⌘ line moves, the Cocoa Emacs keys, ⌘A/C/X/V, ⌘Z/⇧⌘Z with typing coalesced by word, a focus ring, horizontal scroll, IME composition with the candidate window placed at the marked text (`bounds_for_range`, `character_index_for_point`). An **Edit menu** carries the shortcuts; AppKit adds Emoji & Symbols, AutoFill and Dictation to it.
   - **Accent colour** comes from `NSColor.controlAccentColor` (also tints the terminal selection).
5. **Motion — partial.** Hover and pressed fills on every row and button; sheets hang from the bar and fade in while sliding 8 px (160 ms ease-out), the toast rises in; Reduce Motion turns both off. Trackpad momentum already reaches the sidebar's and the terminal history's scrolling (gpui maps the momentum phase to `TouchPhase::Moved` and our code takes every phase). Hover transitions are instant (gpui styles switch, they do not animate); no rubber-band.
6. **Icons and terminal — done.** Eleven Lucide icons (ISC, Feather portions MIT), vendored unmodified with the license under `crates/fleet-gui/assets/icons`, compiled in and served as gpui assets (`core::icons::Icon`); gpui rasterises each lazily on first draw. The terminal defaults to SF Mono 13; a family that is not installed (SF Mono ships with Xcode/Terminal, not with every Mac) falls back to Menlo instead of gpui's proportional fallback. A configured `font_family` still wins. Dark is the reference look; light follows the system appearance with its own tokens.

### What gpui could not do (and might never)

- **Native text fields.** Ours edit like `NSTextField` for the common keys, but they are not one: no spell check, autocorrect, text substitutions, Services, Look Up, the standard context menu (Cut/Copy/Paste/Writing Tools), grapheme-cluster caret moves (it moves by Unicode scalar), right-to-left text, or VoiceOver. Embedding a real `NSTextField` would mean an AppKit subview over gpui's Metal view, with its own focus and key routing outside gpui's: possible in principle, not attempted.
- **Rubber-band (elastic) scrolling.** gpui scroll containers clamp; nothing bounces at the ends.
- **Animated hover/state transitions.** Only explicit `with_animation` on an element; style changes on hover are instant.
- **Accessibility.** gpui 0.2.2 exposes no accessibility tree: VoiceOver sees one opaque view. Not fixable from outside gpui.
- **Native materials and vibrancy.** We get one blur behind the whole window; per-region materials (a sidebar material, vibrant text) would need our own `NSVisualEffectView`s placed under gpui regions and kept in step with layout.
- **System colour changes while running.** The accent colour and Reduce Transparency are read at launch and on appearance changes, not on their own notifications (`NSSystemColorsDidChangeNotification`, accessibility display options): changing them needs a window reopen or an appearance change.
- **Context menus** needed objc2 and a run-loop deferral; **window drag** needed objc2; **accent colour** needed objc2. They work, but each is AppKit glue gpui does not own.

## Consequences

- `src/core` gained `style`, `theme`, `titlebar`, `menus`, `icons` and `text_edit`, all toolkit-free and unit-tested (ADR 0029); `src/app` gained `controls`, `titlebar_view`, `text_field`, `context_menu`, `icons` and `native`.
- The gpui-drawn row menu is gone; its tests now check `core::menus` and the pick handlers (`Workspace::row_menu_pick`, `pane_menu_pick`), since a native menu cannot be driven headless.
- **Performance is unmeasured** (the Mac's screen was locked while building). Risks: the translucent, blurred window makes the Metal layer non-opaque, which the window server composites every frame (may move `input_to_render_ns` and the flood variant); SF Mono instead of Menlo changes glyph shaping and rasterisation, and on a Mac without SF Mono costs one failed font lookup at the first pane; the icon SVGs rasterise on first draw of the first frame (five in a pane header, three in the bar); the bar, the shadows and the extra element nesting add a little layout per frame. Nothing compiles shaders at runtime or decodes large assets at launch. `fleet-perf run --suite wall` and `compare` must be run with the screen unlocked before this is merged; a regression needs the user's approval (CLAUDE.md).
- To merge, the branch needs a rebase onto `m2c-followups`, which moved on (GUI restart/remove settling) after the spike branched.
- If the user picks a native rewrite, the core modules here (tokens, menus, editing model, titlebar text) carry over through uniffi; the gpui shell code does not.
